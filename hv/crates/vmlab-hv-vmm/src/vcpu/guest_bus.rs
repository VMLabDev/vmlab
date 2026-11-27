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
    /// The host address of the first byte of the region. The mapping lives
    /// as long as `Shared::memory`, which holds the region.
    host: usize,
    /// The bitmap of the region, inside the same region: a write through
    /// `host` reports itself there, as vm-memory's own writes do.
    bitmap: *const vmlab_hv_devices::WriteDiscard,
}

// SAFETY: `bitmap` points into a region that `Shared::memory` keeps alive,
// and `WriteDiscard` is `Sync`. `host` is a plain address.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

/// Copy `bytes.len()` bytes of guest RAM at the host address `from`. An
/// access of 2, 4 or 8 bytes at its own alignment is one host access, so
/// that another CPU never sees half of it (single-copy atomicity).
///
/// # Safety
/// `from` and the bytes after it are inside a live mapping of guest RAM.
#[inline(always)]
unsafe fn copy_in(from: usize, bytes: &mut [u8]) {
    let aligned = from % bytes.len().max(1) == 0;
    // SAFETY: the caller's promise. Other threads (devices, CPUs) can write
    // the same RAM, so the access is volatile, as vm-memory's are.
    unsafe {
        match (bytes.len(), aligned) {
            (1, _) => bytes[0] = std::ptr::read_volatile(from as *const u8),
            (2, true) => {
                let v = std::ptr::read_volatile(from as *const u16);
                bytes.copy_from_slice(&v.to_le_bytes());
            }
            (4, true) => {
                let v = std::ptr::read_volatile(from as *const u32);
                bytes.copy_from_slice(&v.to_le_bytes());
            }
            (8, true) => {
                let v = std::ptr::read_volatile(from as *const u64);
                bytes.copy_from_slice(&v.to_le_bytes());
            }
            _ => {
                for (index, byte) in bytes.iter_mut().enumerate() {
                    *byte = std::ptr::read_volatile((from + index) as *const u8);
                }
            }
        }
    }
}

/// Copy `bytes` into guest RAM at the host address `to`, as `copy_in` reads.
///
/// # Safety
/// `to` and the bytes after it are inside a live, writable mapping of guest
/// RAM.
#[inline(always)]
unsafe fn copy_out_to(to: usize, bytes: &[u8]) {
    let aligned = to % bytes.len().max(1) == 0;
    // SAFETY: the caller's promise, as in `copy_in`.
    unsafe {
        match (bytes.len(), aligned) {
            (1, _) => std::ptr::write_volatile(to as *mut u8, bytes[0]),
            (2, true) => std::ptr::write_volatile(
                to as *mut u16,
                u16::from_le_bytes([bytes[0], bytes[1]]),
            ),
            (4, true) => std::ptr::write_volatile(
                to as *mut u32,
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            ),
            (8, true) => {
                let mut word = [0u8; 8];
                word.copy_from_slice(bytes);
                std::ptr::write_volatile(to as *mut u64, u64::from_le_bytes(word));
            }
            _ => {
                for (index, byte) in bytes.iter().enumerate() {
                    std::ptr::write_volatile((to + index) as *mut u8, *byte);
                }
            }
        }
    }
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
    /// The ranges, as (base, end), that a CPU reads and does not write: a
    /// flash bank that the machine maps as memory.
    roms: Vec<(u64, u64)>,
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
        Self::with_roms(memory, host, cpus, &[])
    }

    /// As `new`. A store of a CPU into one of `roms`, each a base and a
    /// length inside `memory`, changes nothing.
    pub fn with_roms(
        memory: GuestMemory,
        host: Arc<dyn VcpuHost>,
        cpus: usize,
        roms: &[(u64, u64)],
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
                host: region.as_ptr() as usize,
                bitmap: region.bitmap() as *const _,
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
            roms: roms.iter().map(|(base, len)| (*base, base + len)).collect(),
        })))
    }

    /// The region of RAM that holds the whole range, and the offset in it.
    #[inline(always)]
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
    #[inline]
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
        // RAM is read straight from its mapping: vm-memory's read takes a
        // lookup and a slice per access, which was a third of the time of
        // a boot (card C13).
        if let Some((region, offset)) = self.ram(address, bytes.len()) {
            // SAFETY: `ram` found the whole range inside the region, whose
            // mapping `Shared::memory` keeps alive.
            unsafe { copy_in(region.host + offset as usize, bytes) };
            return Ok(());
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

    #[inline]
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        if let Some((region, offset)) = self.ram(address, bytes.len()) {
            if self
                .0
                .roms
                .iter()
                .any(|(base, end)| *base <= address && address < *end)
            {
                return Ok(());
            }
            let _atomic = self.lock();
            // SAFETY: `ram` found the whole range inside the region, whose
            // mapping `Shared::memory` keeps alive, and RAM is writable.
            unsafe { copy_out_to(region.host + offset as usize, bytes) };
            // The bitmap of the region moves the generations, after the
            // write, as vm-memory reports its own writes.
            // SAFETY: the bitmap is inside the region, as `host` is.
            let bitmap = unsafe { &*region.bitmap };
            vm_memory::bitmap::Bitmap::mark_dirty(bitmap, offset as usize, bytes.len());
            return Ok(());
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
        let shared = self.0.clone();
        let _atomic = shared.serialize.then(|| shared.transaction.lock());
        // The generation first: a device write between the two reads is
        // then seen by the store.
        let generation = self.reservation_generation(address, bytes.len());
        self.read(address, bytes)?;
        Ok(generation)
    }

    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool> {
        let shared = self.0.clone();
        let _atomic = shared.serialize.then(|| shared.transaction.lock());
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
