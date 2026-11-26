//! Guest memory as an emulated CPU sees it (DESIGN.md of phase 3d, F2): the
//! `Memory` trait of the emulator over the guest RAM of the VMM, with every
//! address that no RAM holds sent to the MMIO bus that the devices are on.
//!
//! The emulator keeps translated code, and exclusive loads keep a
//! reservation, both against the write generation of the pages they cover.
//! A CPU writes through this adapter. A device writes guest RAM through
//! vm-memory, straight into the mapping. Both come past the bitmap of the
//! region (`WriteDiscard::watch`), which is where the generations move: once
//! per write, for the pages of the write. A write that the kernel makes for
//! another process (vhost) does not come past there, so a machine with an
//! emulated CPU must not give its RAM to a vhost backend.
//!
//! With more than one CPU, a store, an exclusive load and an exclusive
//! store hold one lock, so that the check of a reservation and its store
//! are one step (DESIGN.md M6: sequentially consistent, as `SharedMemory`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::ReentrantMutex;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
use vmlab_hv_devices::GuestMemory;
use vmlab_hv_tcg::{CodeStamp, Error, Memory, Result};

use super::VcpuHost;

const PAGE: u64 = 4096;
/// The widest access of the MMIO bus.
const BUS_WIDTH: usize = 8;

/// The write generations of one region of RAM.
struct Stamps {
    /// The generation of the whole memory: the count of its writes.
    generation: Arc<AtomicU64>,
    /// For each page, the generation of its last write.
    pages: Vec<AtomicU64>,
}

impl Stamps {
    fn wrote(&self, offset: usize, len: usize) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if len == 0 {
            return;
        }
        let first = offset / PAGE as usize;
        let last = ((offset + len - 1) / PAGE as usize).min(self.pages.len() - 1);
        for page in &self.pages[first.min(last)..=last] {
            page.store(generation, Ordering::SeqCst);
        }
    }

    /// The newest generation of the pages of the range.
    fn newest(&self, offset: u64, len: usize) -> u64 {
        let first = (offset / PAGE) as usize;
        let last = ((offset + len.max(1) as u64 - 1) / PAGE) as usize;
        self.pages[first..=last]
            .iter()
            .map(|page| page.load(Ordering::SeqCst))
            .max()
            .unwrap_or(0)
    }
}

struct Region {
    base: u64,
    len: u64,
    stamps: Arc<Stamps>,
}

struct Shared {
    memory: GuestMemory,
    regions: Vec<Region>,
    generation: Arc<AtomicU64>,
    identity: u64,
    host: Arc<dyn VcpuHost>,
    transaction: Arc<ReentrantMutex<()>>,
    /// More than one CPU writes this memory.
    serialize: bool,
}

/// The memory of one emulated CPU. Every CPU of a machine has a clone.
#[derive(Clone)]
pub struct GuestBus(Arc<Shared>);

impl GuestBus {
    /// The RAM `memory` and the MMIO bus of `host`, for a machine of `cpus`
    /// emulated CPUs. One memory takes one `GuestBus`, and its clones.
    pub fn new(
        memory: GuestMemory,
        host: Arc<dyn VcpuHost>,
        cpus: usize,
    ) -> std::result::Result<Self, String> {
        let generation = Arc::new(AtomicU64::new(0));
        let mut regions = Vec::new();
        for region in memory.iter() {
            let stamps = Arc::new(Stamps {
                generation: generation.clone(),
                pages: (0..region.len().div_ceil(PAGE))
                    .map(|_| AtomicU64::new(0))
                    .collect(),
            });
            let watch = stamps.clone();
            if !region
                .get_mmap()
                .bitmap()
                .watch(Box::new(move |offset, len| watch.wrote(offset, len)))
            {
                return Err("the guest memory has an emulated CPU already".into());
            }
            regions.push(Region {
                base: region.start_addr().0,
                len: region.len(),
                stamps,
            });
        }
        Ok(Self(Arc::new(Shared {
            memory,
            regions,
            generation,
            identity: vmlab_hv_tcg::next_identity(),
            host,
            transaction: Arc::new(ReentrantMutex::new(())),
            serialize: cpus > 1,
        })))
    }

    /// The region of RAM that holds the whole range, and the offset in it.
    fn ram(&self, address: u64, len: usize) -> Option<(&Region, u64)> {
        self.0.regions.iter().find_map(|region| {
            let offset = address.checked_sub(region.base)?;
            (offset.checked_add(len as u64)? <= region.len).then_some((region, offset))
        })
    }

    /// Whether RAM holds a part of the range. Such an access reaches over
    /// the end of a region: no bus access is made for its other part.
    fn touches_ram(&self, address: u64, len: usize) -> bool {
        let end = address.saturating_add(len as u64);
        self.0
            .regions
            .iter()
            .any(|region| address < region.base + region.len && region.base < end)
    }

    fn lock(&self) -> Option<parking_lot::ReentrantMutexGuard<'_, ()>> {
        self.0.serialize.then(|| self.0.transaction.lock())
    }

    fn copy_out(&self, address: u64, bytes: &mut [u8]) -> Result<()> {
        self.0
            .memory
            .read_slice(bytes, GuestAddress(address))
            .map_err(|_| Error::Memory(address))
    }
}

impl Memory for GuestBus {
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
        if self.ram(address, bytes.len()).is_some() {
            return self.copy_out(address, bytes);
        }
        if self.touches_ram(address, bytes.len()) {
            return Err(Error::Memory(address));
        }
        let mut at = address;
        for part in bytes.chunks_mut(BUS_WIDTH) {
            if !self.0.host.mmio_read(at, part) {
                return Err(Error::Memory(at));
            }
            at += part.len() as u64;
        }
        Ok(())
    }

    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        if self.ram(address, bytes.len()).is_some() {
            let _atomic = self.lock();
            // The bitmap of the region moves the generations.
            return self
                .0
                .memory
                .write_slice(bytes, GuestAddress(address))
                .map_err(|_| Error::Memory(address));
        }
        if self.touches_ram(address, bytes.len()) {
            return Err(Error::Memory(address));
        }
        let mut at = address;
        for part in bytes.chunks(BUS_WIDTH) {
            if !self.0.host.mmio_write(at, part) {
                return Err(Error::Memory(at));
            }
            at += part.len() as u64;
        }
        // The CPU counts each of its writes as one generation.
        self.0.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::SeqCst)
    }

    fn code_stamp(&self, address: u64, len: usize) -> Option<CodeStamp> {
        if len == 0 {
            return None;
        }
        let (region, offset) = self.ram(address, len)?;
        Some(CodeStamp {
            identity: self.0.identity,
            mapping: region.base,
            first_page: offset / PAGE,
            last_page: (offset + len as u64 - 1) / PAGE,
            generation: region.stamps.newest(offset, len),
        })
    }

    fn code_bytes(&self, address: u64, bytes: &mut [u8]) -> bool {
        self.ram(address, bytes.len()).is_some() && self.copy_out(address, bytes).is_ok()
    }

    fn ram_bounds(&self, address: u64) -> Option<(u64, u64)> {
        let (region, _) = self.ram(address, 1)?;
        Some((region.base, region.base + (region.len - 1)))
    }

    fn reservation_generation(&self, address: u64, len: usize) -> u64 {
        match self.ram(address, len) {
            Some((region, offset)) => region.stamps.newest(offset, len),
            None => self.generation(),
        }
    }

    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64> {
        let _atomic = self.lock();
        // The generation first: a device write between the two reads is
        // then seen by the store.
        let generation = self.reservation_generation(address, bytes.len());
        self.read(address, bytes)?;
        Ok(generation)
    }

    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool> {
        let _atomic = self.lock();
        if self.reservation_generation(address, bytes.len()) != generation {
            return Ok(false);
        }
        self.write(address, bytes)?;
        Ok(true)
    }

    fn transaction_lock(&self) -> Option<Arc<ReentrantMutex<()>>> {
        self.0.serialize.then(|| self.0.transaction.clone())
    }
}
