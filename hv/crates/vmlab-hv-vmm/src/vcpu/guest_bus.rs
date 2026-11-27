//! Guest memory as an emulated CPU sees it (DESIGN.md of phase 3d, F2): the
//! `Memory` trait of the emulator over the guest RAM of the VMM, with every
//! address that no RAM holds sent to the MMIO bus that the devices are on.
//!
//! The emulator keeps translated code, and exclusive loads keep a
//! reservation, both against the write generation of the pages they cover.
//! A CPU writes through this adapter, or straight into the mapping through
//! the host page that the adapter handed to its TLB (`Memory::host_page`).
//! A device writes guest RAM through vm-memory, straight into the mapping.
//! A stamped write comes past the bitmap of the region
//! (`WriteDiscard::watch`), which is where the generations move: once per
//! write, for the pages of the write. A device write is always stamped. A
//! write that the kernel makes for another process (vhost) does not come
//! past there, so a machine with an emulated CPU must not give its RAM to a
//! vhost backend.
//!
//! With more than one CPU, a stamped store, an exclusive load and an
//! exclusive store hold one lock, so that the check of a reservation and
//! its store are one step (DESIGN.md M6).
//!
//! # Stores without stamps (card S3)
//!
//! Only two things read the stamps: translated code and a reservation. A
//! CPU writes a page that holds no translated code straight, without a
//! stamp, a lock or a sequentially consistent operation. The page says so
//! with its watch word (`HostPage`): odd while some CPU holds code that it
//! translated there. The word only grows.
//!
//! - **Code.** A translator makes the word odd (`watch_code`), then takes
//!   the stamp, then reads the code, then checks that the word is still the
//!   same (`watch_holds`). A stamped write makes the word even, then moves
//!   the stamps. A CPU that writes straight reads the word before its write
//!   and again after it, and moves the stamps itself when the word is odd
//!   then (`Memory::wrote_straight`).
//!
//!   Each side stores, then loads what the other stored, which needs a
//!   full fence on both. The writer has none: after the translator makes
//!   the word odd it runs `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED)`,
//!   which puts a full fence into every other running thread of the
//!   process. A straight write whose second load found the word even came
//!   before that fence in its thread, so it is visible when the barrier
//!   returns, and the translator reads its bytes. A write that found the
//!   word odd moves a stamp after the write: if the translator's stamp is
//!   older, the block fails its stamp at the next check, and if it is that
//!   one, the translator also reads the bytes. A write that makes the word
//!   even between the translator's stamp and its check changes the word,
//!   so the block takes no stamp. Without that change a block with a stamp
//!   that holds has an odd word, and every write into its page is stamped.
//!   The translator makes the word odd and runs the barrier under one
//!   mutex, so a translator that finds the word odd knows that the barrier
//!   of whoever made it odd has returned. With one CPU no other thread
//!   writes straight, and there is no barrier.
//! - **Reservations.** An exclusive store needs that no other CPU wrote
//!   the reserved bytes since its load. A straight write moves no stamp, so
//!   the exclusive store is one compare and exchange of the host against
//!   the value that its load read (`compare_and_store`), under the lock. A
//!   store of another CPU after the load that changes the value fails the
//!   exchange. One that writes the same value back is taken as a store
//!   before the load, which is what QEMU's exclusive store, a compare and
//!   exchange, does too. A device write and a stamped write still move the
//!   stamps, so they fail a reservation as before.
//!
//! A host without the barrier or without a compare and exchange of 16 bytes
//! runs a machine of more than one CPU as before: every write is stamped.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering, compiler_fence};

use parking_lot::ReentrantMutex;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
use vmlab_hv_devices::GuestMemory;
use vmlab_hv_tcg::{CodeStamp, Error, HostPage, Memory, Result};

use super::VcpuHost;

const PAGE: u64 = 4096;
/// The widest access of the MMIO bus.
const BUS_WIDTH: usize = 8;

/// The write generations of one region of RAM.
struct Stamps {
    /// The generation of the whole memory: the count of its stamped writes.
    generation: Arc<AtomicU64>,
    /// For each page, the generation of its last stamped write.
    pages: Vec<AtomicU64>,
    /// For each page, its watch word: odd while a CPU holds code that it
    /// translated there.
    watch: Vec<AtomicU32>,
}

impl Stamps {
    /// A write reached the bytes. Their pages hold no code that is known to
    /// hold any more: the watch words go even before the stamps move.
    /// `true`: this write made one even (`Memory::ended_code`).
    fn wrote(&self, offset: usize, len: usize) -> bool {
        if len == 0 {
            self.generation.fetch_add(1, Ordering::SeqCst);
            return false;
        }
        let mut ended = false;
        let first = offset / PAGE as usize;
        let last = ((offset + len - 1) / PAGE as usize).min(self.pages.len() - 1);
        let pages = first.min(last)..=last;
        for word in &self.watch[pages.clone()] {
            let now = word.load(Ordering::SeqCst);
            if now & 1 != 0 {
                // Only a write makes an odd word even, so an exchange that
                // fails found it even already.
                ended |= word
                    .compare_exchange(now, now.wrapping_add(1), Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok();
            }
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        for page in &self.pages[pages] {
            page.store(generation, Ordering::SeqCst);
        }
        ended
    }

    /// Whether a page of the range holds code that a CPU translated.
    #[inline(always)]
    fn watched(&self, offset: u64, len: usize) -> bool {
        let first = (offset / PAGE) as usize;
        let last = ((offset + len.max(1) as u64 - 1) / PAGE) as usize;
        self.watch[first..=last]
            .iter()
            .any(|word| word.load(Ordering::Relaxed) & 1 != 0)
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
// SAFETY: as for `Send`: the only pointer is to a `Sync` value that lives
// as long as the region.
unsafe impl Sync for Region {}

/// Copy `bytes.len()` bytes of guest RAM at the host address `from`. An
/// access of 2, 4 or 8 bytes at its own alignment is one host access, so
/// that another CPU never sees half of it (single-copy atomicity).
///
/// # Safety
/// `from` and the bytes after it are inside a live mapping of guest RAM.
#[inline(always)]
unsafe fn copy_in(from: usize, bytes: &mut [u8]) {
    let aligned = from.is_multiple_of(bytes.len().max(1));
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
    let aligned = to.is_multiple_of(bytes.len().max(1));
    // SAFETY: the caller's promise, as in `copy_in`.
    unsafe {
        match (bytes.len(), aligned) {
            (1, _) => std::ptr::write_volatile(to as *mut u8, bytes[0]),
            (2, true) => {
                std::ptr::write_volatile(to as *mut u16, u16::from_le_bytes([bytes[0], bytes[1]]))
            }
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

const MEMBARRIER_CMD_PRIVATE_EXPEDITED: libc::c_int = 1 << 3;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: libc::c_int = 1 << 4;

/// Ask the kernel for the private expedited `membarrier` of this process.
/// `false`: the host has none.
fn register_barrier() -> bool {
    // SAFETY: the command takes no pointer.
    unsafe {
        libc::syscall(
            libc::SYS_membarrier,
            MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED,
            0,
            0,
        ) == 0
    }
}

/// A full memory fence in every running thread of the process.
fn barrier() {
    // SAFETY: the command takes no pointer, and `register_barrier` asked
    // for it.
    let done =
        unsafe { libc::syscall(libc::SYS_membarrier, MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, 0) };
    // A registered command fails only for a bug of the kernel, and then
    // nothing about the order of the CPUs holds.
    assert_eq!(
        done,
        0,
        "membarrier failed: {}",
        std::io::Error::last_os_error()
    );
}

/// Whether the host has a compare and exchange of 16 bytes.
fn has_cas16() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("cmpxchg16b")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Compare and exchange the 16 bytes at `at`: `lock cmpxchg16b`, written
/// out because the intrinsic leaves a call of `__atomic_compare_exchange_16`
/// in a build without optimisation, which no library serves.
///
/// # Safety
/// `at` is a live, writable, 16-byte aligned host address, and the host has
/// `cmpxchg16b` (`has_cas16`).
#[cfg(target_arch = "x86_64")]
unsafe fn cas16(at: usize, expected: u128, new: u128) -> bool {
    let swapped: u8;
    // SAFETY: the caller's promise. `rbx` belongs to the compiler, so the
    // low half of the new value goes through another register and `rbx` is
    // put back.
    unsafe {
        std::arch::asm!(
            "xchg {low}, rbx",
            "lock cmpxchg16b xmmword ptr [{at}]",
            "sete {swapped}",
            "mov rbx, {low}",
            at = in(reg) at,
            low = inout(reg) new as u64 => _,
            swapped = out(reg_byte) swapped,
            inout("rax") expected as u64 => _,
            inout("rdx") (expected >> 64) as u64 => _,
            in("rcx") (new >> 64) as u64,
            options(nostack),
        );
    }
    swapped != 0
}

/// Compare and exchange `len` bytes, 1 to 16, at `at`: `expected` and `new`
/// are little-endian in their low bytes. `None`: the host has no such
/// operation.
///
/// # Safety
/// `at` is a live, writable host address aligned to `len`.
unsafe fn compare_exchange(at: usize, len: usize, expected: u128, new: u128) -> Option<bool> {
    let ordering = Ordering::SeqCst;
    // SAFETY: the caller's promise, and every atomic type has the alignment
    // of its size.
    unsafe {
        Some(match len {
            1 => AtomicU8::from_ptr(at as *mut u8)
                .compare_exchange(expected as u8, new as u8, ordering, ordering)
                .is_ok(),
            2 => AtomicU16::from_ptr(at as *mut u16)
                .compare_exchange(
                    (expected as u16).to_le(),
                    (new as u16).to_le(),
                    ordering,
                    ordering,
                )
                .is_ok(),
            4 => AtomicU32::from_ptr(at as *mut u32)
                .compare_exchange(
                    (expected as u32).to_le(),
                    (new as u32).to_le(),
                    ordering,
                    ordering,
                )
                .is_ok(),
            8 => AtomicU64::from_ptr(at as *mut u64)
                .compare_exchange(
                    (expected as u64).to_le(),
                    (new as u64).to_le(),
                    ordering,
                    ordering,
                )
                .is_ok(),
            #[cfg(target_arch = "x86_64")]
            16 if has_cas16() => cas16(at, expected.to_le(), new.to_le()),
            _ => return None,
        })
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
    /// A CPU writes a page without a code watch straight, without a stamp
    /// (the module comment).
    direct: bool,
    /// Held while a translator makes a watch word odd and runs the barrier.
    watching: parking_lot::Mutex<()>,
    /// The ranges, as (base, end), that a CPU reads and does not write: a
    /// flash bank that the machine maps as memory.
    roms: Vec<(u64, u64)>,
}

/// The region of RAM of `shared` that holds the whole range, and the offset
/// in it.
#[inline(always)]
fn shared_ram(shared: &Shared, address: u64, len: usize) -> Option<(&Region, u64)> {
    shared.regions.iter().find_map(|region| {
        let offset = address.checked_sub(region.base)?;
        (offset.checked_add(len as u64)? <= region.len).then_some((region, offset))
    })
}

/// The memory of one emulated CPU. Every CPU of a machine has a clone.
#[derive(Clone)]
pub struct GuestBus {
    shared: Arc<Shared>,
    /// The times that the writes through this clone moved the generation.
    own: u64,
    /// The writes through this clone that ended translated code.
    ended: u64,
}

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
            let pages = region.len().div_ceil(PAGE);
            let stamps = Arc::new(Stamps {
                generation: generation.clone(),
                pages: (0..pages).map(|_| AtomicU64::new(0)).collect(),
                watch: (0..pages).map(|_| AtomicU32::new(0)).collect(),
            });
            let watch = stamps.clone();
            if !region
                .get_mmap()
                .bitmap()
                .watch(Box::new(move |offset, len| {
                    watch.wrote(offset, len);
                }))
            {
                return Err("the guest memory has an emulated CPU already".into());
            }
            regions.push(Region {
                base: region.start_addr().0,
                len: region.len(),
                stamps,
                host: region.as_ptr() as usize,
                bitmap: region.get_mmap().bitmap() as *const _,
            });
        }
        // A region whose bitmap discards what a write leaves takes every
        // write through the bitmap. A host page is inside one region.
        let direct = regions.iter().all(|region| {
            // SAFETY: the bitmap is inside the region, which `memory` holds.
            !unsafe { &*region.bitmap }.discards()
                && region.base.is_multiple_of(PAGE)
                && region.len.is_multiple_of(PAGE)
        }) && (cpus <= 1 || (has_cas16() && register_barrier()));
        Ok(Self {
            shared: Arc::new(Shared {
                memory,
                regions,
                generation,
                identity: vmlab_hv_tcg::next_identity(),
                host,
                transaction: Arc::new(ReentrantMutex::new(())),
                serialize: cpus > 1,
                direct,
                watching: parking_lot::Mutex::new(()),
                roms: roms.iter().map(|(base, len)| (*base, base + len)).collect(),
            }),
            own: 0,
            ended: 0,
        })
    }

    /// Whether a CPU writes a page without translated code straight,
    /// without a stamp (the module comment).
    pub fn writes_straight(&self) -> bool {
        self.shared.direct
    }

    /// The region of RAM that holds the whole range, and the offset in it.
    #[inline(always)]
    fn ram(&self, address: u64, len: usize) -> Option<(&Region, u64)> {
        shared_ram(&self.shared, address, len)
    }

    fn is_rom(&self, address: u64) -> bool {
        self.shared
            .roms
            .iter()
            .any(|(base, end)| *base <= address && address < *end)
    }

    /// Whether RAM holds a part of the range. Such an access reaches over
    /// the end of a region: no bus access is made for its other part.
    fn touches_ram(&self, address: u64, len: usize) -> bool {
        let end = address.saturating_add(len as u64);
        self.shared
            .regions
            .iter()
            .any(|region| address < region.base + region.len && region.base < end)
    }

    fn copy_out(&self, address: u64, bytes: &mut [u8]) -> Result<()> {
        self.shared
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
            if !self.shared.host.mmio_read(at, part) {
                return Err(Error::Memory(at));
            }
            at += part.len() as u64;
        }
        Ok(())
    }

    #[inline]
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        let shared = self.shared.clone();
        if let Some((region, offset)) = shared_ram(&shared, address, bytes.len()) {
            if self.is_rom(address) {
                return Ok(());
            }
            let host = region.host + offset as usize;
            if shared.direct && !region.stamps.watched(offset, bytes.len()) {
                // No code of a CPU there: no stamp (the module comment).
                // SAFETY: `ram` found the whole range inside the region,
                // whose mapping `Shared::memory` keeps alive, and RAM is
                // writable.
                unsafe { copy_out_to(host, bytes) };
                compiler_fence(Ordering::SeqCst);
                if region.stamps.watched(offset, bytes.len()) {
                    self.ended += u64::from(region.stamps.wrote(offset as usize, bytes.len()));
                    self.own += 1;
                }
                return Ok(());
            }
            // The bitmap makes the watch words even: one that is odd now
            // ends code. A translator that makes one odd after this look
            // finds its word changed (`watch_holds`).
            self.ended += u64::from(region.stamps.watched(offset, bytes.len()));
            let _atomic = shared.serialize.then(|| shared.transaction.lock());
            // SAFETY: as above.
            unsafe { copy_out_to(host, bytes) };
            // The bitmap of the region moves the generations, after the
            // write, as vm-memory reports its own writes.
            // SAFETY: the bitmap is inside the region, as `host` is.
            let bitmap = unsafe { &*region.bitmap };
            vm_memory::bitmap::Bitmap::mark_dirty(bitmap, offset as usize, bytes.len());
            self.own += 1;
            return Ok(());
        }
        if self.touches_ram(address, bytes.len()) {
            return Err(Error::Memory(address));
        }
        let mut at = address;
        for part in bytes.chunks(BUS_WIDTH) {
            if !self.shared.host.mmio_write(at, part) {
                return Err(Error::Memory(at));
            }
            at += part.len() as u64;
        }
        // The CPU counts each of its writes as one generation.
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        self.own += 1;
        Ok(())
    }

    fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }

    fn code_stamp(&self, address: u64, len: usize) -> Option<CodeStamp> {
        if len == 0 {
            return None;
        }
        let (region, offset) = self.ram(address, len)?;
        Some(CodeStamp {
            identity: self.shared.identity,
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
        let shared = self.shared.clone();
        let _atomic = shared.serialize.then(|| shared.transaction.lock());
        // The generation first: a device write between the two reads is
        // then seen by the store.
        let generation = self.reservation_generation(address, bytes.len());
        self.read(address, bytes)?;
        Ok(generation)
    }

    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool> {
        let shared = self.shared.clone();
        let _atomic = shared.serialize.then(|| shared.transaction.lock());
        if self.reservation_generation(address, bytes.len()) != generation {
            return Ok(false);
        }
        self.write(address, bytes)?;
        Ok(true)
    }

    fn transaction_lock(&self) -> Option<Arc<ReentrantMutex<()>>> {
        self.shared
            .serialize
            .then(|| self.shared.transaction.clone())
    }

    fn host_page(&mut self, page: u64) -> Option<HostPage> {
        if !self.shared.direct {
            return None;
        }
        let (region, offset) = self.ram(page, PAGE as usize)?;
        let watch = if self.is_rom(page) {
            0
        } else {
            &region.stamps.watch[(offset / PAGE) as usize] as *const AtomicU32 as usize
        };
        Some(HostPage {
            base: region.host + offset as usize,
            watch,
        })
    }

    fn host_mapping(&self) -> Option<(u64, u64)> {
        self.shared.direct.then_some((self.shared.identity, 0))
    }

    fn watch_code(&mut self, address: u64) -> Option<u32> {
        if !self.shared.direct {
            return None;
        }
        let shared = self.shared.clone();
        let (region, offset) = shared_ram(&shared, address, 1)?;
        let word = &region.stamps.watch[(offset / PAGE) as usize];
        // Under the mutex: a word that another translator made odd has had
        // its barrier.
        let _watching = shared.watching.lock();
        let now = word.load(Ordering::SeqCst);
        if now & 1 != 0 {
            return Some(now);
        }
        // Only a translator makes an even word odd, and it holds the mutex.
        let odd = now.wrapping_add(1);
        word.store(odd, Ordering::SeqCst);
        if shared.serialize {
            barrier();
        }
        Some(odd)
    }

    fn watch_holds(&self, address: u64, word: u32) -> bool {
        self.ram(address, 1).is_some_and(|(region, offset)| {
            region.stamps.watch[(offset / PAGE) as usize].load(Ordering::SeqCst) == word
        })
    }

    fn wrote_straight(&mut self, address: u64, len: usize) {
        if let Some((region, offset)) = self.ram(address, len) {
            self.ended += u64::from(region.stamps.wrote(offset as usize, len));
            self.own += 1;
        }
    }

    fn ended_code(&self) -> u64 {
        self.ended
    }

    fn own_generation(&self) -> Option<u64> {
        self.shared.direct.then_some(self.own)
    }

    fn compare_and_store(&mut self, address: u64, expected: &[u8], new: &[u8]) -> Result<bool> {
        let shared = self.shared.clone();
        let len = new.len();
        if shared.direct
            && expected.len() == len
            && !self.is_rom(address)
            && let Some((region, offset)) = shared_ram(&shared, address, len)
            && (region.host + offset as usize).is_multiple_of(len)
        {
            let word = |bytes: &[u8]| {
                let mut wide = [0u8; 16];
                wide[..len].copy_from_slice(bytes);
                u128::from_le_bytes(wide)
            };
            // SAFETY: the range is inside the region, whose mapping
            // `Shared::memory` keeps alive, RAM is writable, and the host
            // address is aligned to the size.
            let swapped = unsafe {
                compare_exchange(
                    region.host + offset as usize,
                    len,
                    word(expected),
                    word(new),
                )
            };
            if let Some(swapped) = swapped {
                if swapped && region.stamps.watched(offset, len) {
                    self.ended += u64::from(region.stamps.wrote(offset as usize, len));
                    self.own += 1;
                }
                return Ok(swapped);
            }
        }
        // A memory where every write takes the lock that the caller holds,
        // a ROM, the bus, or one CPU on a host without the operation.
        let mut now = [0u8; 16];
        let now = &mut now[..expected.len()];
        self.read(address, now)?;
        if now != expected {
            return Ok(false);
        }
        self.write(address, new)?;
        Ok(true)
    }
}
