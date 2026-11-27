use crate::{Error, Result};

const PAGE_SIZE: usize = 4096;
/// The size of an LR/SC reservation granule in `FlatMemory`. A store by any
/// hart to a granule breaks the reservations that cover it and no others, so
/// unrelated stores elsewhere in RAM cannot starve an LR/SC loop. One page
/// lets the granules share the per-page write generations code stamps keep.
pub const RESERVATION_GRANULE: usize = PAGE_SIZE;
static NEXT_IDENTITY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A new value for `CodeStamp::identity`: one per memory instance, also for
/// an implementation of `Memory` outside this crate.
pub fn next_identity() -> u64 {
    let id = NEXT_IDENTITY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_ne!(id, 0, "memory identity exhausted");
    id
}

/// A page of RAM that a CPU reads and writes through the host, without a
/// call of the memory (DESIGN.md of phase 3d, R12; card S3).
///
/// A CPU writes such a page straight only while its watch word is even.
/// The word is odd while the page holds code that a CPU translated, and
/// every write that a CPU or a device makes into a watched page goes
/// through the memory, which moves the code stamps of the page, makes the
/// word even again and then moves the stamps. A CPU that wrote straight
/// reads the word again after its write, and when it is odd then, it tells
/// the memory with `Memory::wrote_straight`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostPage {
    /// The host address of the first byte of the page. The mapping lives
    /// while the memory names the same `Memory::host_mapping`.
    pub base: usize,
    /// The address of the `AtomicU32` watch word of the page, or 0 for a
    /// page that a CPU reads straight and writes only through the memory
    /// (a ROM).
    pub watch: usize,
}

impl HostPage {
    /// The watch word of the page. `None` for a page that is not written
    /// straight.
    #[inline(always)]
    pub fn watch(&self) -> Option<&std::sync::atomic::AtomicU32> {
        // SAFETY: a non-zero `watch` is the address of an `AtomicU32` that
        // lives as long as the mapping of `base` (the promise of
        // `Memory::host_page`).
        (self.watch != 0).then(|| unsafe { &*(self.watch as *const std::sync::atomic::AtomicU32) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeStamp {
    pub identity: u64,
    pub mapping: u64,
    pub first_page: u64,
    pub last_page: u64,
    pub generation: u64,
}

/// Physical accesses, including page table accesses, arrive here from Sail.
/// Implementations may dispatch MMIO, share RAM across harts, and track writes.
/// An access the implementation does not hold fails with `Error::Memory` and
/// must change nothing: the interpreter gives the guest the access fault for
/// it. Any other error stops the hart as a host error.
pub trait Memory {
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()>;
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()>;
    fn generation(&self) -> u64;
    /// Some promises side-effect-free RAM fetch, a unique memory-instance ID,
    /// and invalidation when any covered page or its mapping changes.
    /// Page identifiers belong to the returned mapping. Requests covering the
    /// same pages may share a guard when all stamp fields are equal.
    /// Shared implementations must serialize this query with their writes.
    fn code_stamp(&self, _address: u64, _len: usize) -> Option<CodeStamp> {
        None
    }
    /// Copies bytes that `code_stamp` covers, with no side effect, and says
    /// whether it did. A stamp moves on any write to its pages, also on a
    /// write that leaves the fetched bytes as they were. With this copy a
    /// trace compares its fetched bytes and stays valid across such a write.
    /// `false`, the default, means every moved stamp ends the trace.
    /// Shared implementations must serialize this copy with their writes.
    fn code_bytes(&self, _address: u64, _bytes: &mut [u8]) -> bool {
        false
    }
    /// Inclusive numeric bounds of the RAM region containing this address.
    /// Specialization must guard the resulting range and retain memory effects.
    fn ram_bounds(&self, _address: u64) -> Option<(u64, u64)> {
        None
    }
    /// The write generation of the reservation granules covering this range:
    /// a value that changes whenever a write lands in any of them. An
    /// implementation chooses its granule, and the whole of memory is a valid
    /// one, which is this default. It must agree with what `load_reserved`
    /// returns and `store_conditional` validates.
    fn reservation_generation(&self, _address: u64, _len: usize) -> u64 {
        self.generation()
    }
    /// Read and return `reservation_generation` for the range as one atomic
    /// host operation.
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64>;
    /// Validate that generation against the range's reservation granules and
    /// write, as one atomic host operation.
    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool>;
    /// Serializes a generated read-modify-write sequence with ordinary accesses.
    fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>>;

    /// The host page of the page of RAM that starts at the physical address
    /// `page` (a multiple of 4096), when a CPU may access it straight. The
    /// mapping and the watch word live as long as `host_mapping` names the
    /// same value. `None`, the default: every access is a call.
    fn host_page(&mut self, _page: u64) -> Option<HostPage> {
        None
    }
    /// A value that names the host pages that `host_page` hands out: a CPU
    /// drops the pages it keeps when the value is another one. `None` for a
    /// memory that hands out none.
    fn host_mapping(&self) -> Option<(u64, u64)> {
        None
    }
    /// The page of the physical address holds code that a CPU translates
    /// now: from here every write into it moves its code stamp. The answer
    /// is the watch word after the call, for `watch_holds`. Call it before
    /// `code_stamp`, which comes before the code is read.
    fn watch_code(&mut self, _address: u64) -> Option<u32> {
        None
    }
    /// Whether the watch word of the page is still `word`: no write made it
    /// even since `watch_code`. `false` means a write raced the
    /// translation, and the block takes no stamp.
    fn watch_holds(&self, _address: u64, _word: u32) -> bool {
        true
    }
    /// A CPU wrote these bytes straight into a host page whose watch it then
    /// found odd: move the stamps as `write` would have.
    fn wrote_straight(&mut self, _address: u64, _len: usize) {}
    /// The count of times that the writes through this handle moved
    /// `generation`, for a memory that does not move it on every write.
    /// `None`, the default: every write moves it once, and the CPU counts
    /// its own.
    fn own_generation(&self) -> Option<u64> {
        None
    }
    /// Write `new` at `address` when the memory holds `expected` there, as
    /// one host operation against the writes of every other CPU, also those
    /// that a CPU makes straight. Both are the same size and the range is
    /// aligned to it. The caller holds `transaction_lock`. The default reads,
    /// compares and writes: right for a memory whose every write takes that
    /// lock.
    fn compare_and_store(&mut self, address: u64, expected: &[u8], new: &[u8]) -> Result<bool> {
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

#[derive(Debug)]
pub struct FlatMemory {
    pub base: u64,
    data: Vec<u8>,
    generation: u64,
    identity: u64,
    page_generations: Vec<u64>,
    /// The watch word of each page (`HostPage`).
    watch: Box<[std::sync::atomic::AtomicU32]>,
}

impl FlatMemory {
    pub fn new(base: u64, data: Vec<u8>) -> Self {
        let pages = data.len().div_ceil(PAGE_SIZE);
        Self {
            base,
            data,
            generation: 0,
            identity: next_identity(),
            page_generations: vec![0; pages],
            watch: (0..pages)
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
        }
    }

    /// Make the watch words of the pages `first..=last` even: a write into
    /// a page ends what a CPU translated there.
    fn unwatch(&self, first: usize, last: usize) {
        for word in &self.watch[first..=last] {
            let now = word.load(std::sync::atomic::Ordering::SeqCst);
            if now & 1 != 0 {
                word.store(now.wrapping_add(1), std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Move the stamps of the pages `first..=last`, after a write.
    fn stamp(&mut self, first: usize, last: usize) {
        self.generation = self.generation.wrapping_add(1);
        self.unwatch(first, last);
        self.page_generations[first..=last].fill(self.generation);
    }
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }
    #[inline]
    fn range(&self, address: u64, len: usize) -> Result<std::ops::Range<usize>> {
        let start = address
            .checked_sub(self.base)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(Error::Memory(address))?;
        let end = start
            .checked_add(len)
            .filter(|n| *n <= self.data.len())
            .ok_or(Error::Memory(address))?;
        Ok(start..end)
    }
}

impl Memory for FlatMemory {
    fn ram_bounds(&self, address: u64) -> Option<(u64, u64)> {
        let end = self
            .base
            .checked_add(u64::try_from(self.data.len().checked_sub(1)?).ok()?)?;
        (address >= self.base && address <= end).then_some((self.base, end))
    }
    #[inline]
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
        bytes.copy_from_slice(&self.data[self.range(address, bytes.len())?]);
        Ok(())
    }
    #[inline]
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        let range = self.range(address, bytes.len())?;
        self.data[range.clone()].copy_from_slice(bytes);
        if range.is_empty() {
            self.generation = self.generation.wrapping_add(1);
        } else {
            self.stamp(range.start / PAGE_SIZE, (range.end - 1) / PAGE_SIZE);
        }
        Ok(())
    }
    fn host_page(&mut self, page: u64) -> Option<HostPage> {
        let range = self.range(page, PAGE_SIZE).ok()?;
        if !range.start.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        Some(HostPage {
            base: self.data[range.clone()].as_mut_ptr() as usize,
            watch: &self.watch[range.start / PAGE_SIZE] as *const _ as usize,
        })
    }
    fn host_mapping(&self) -> Option<(u64, u64)> {
        Some((self.identity, self.base))
    }
    fn watch_code(&mut self, address: u64) -> Option<u32> {
        let offset = self.range(address, 1).ok()?.start;
        let word = &self.watch[offset / PAGE_SIZE];
        let now = word.load(std::sync::atomic::Ordering::SeqCst);
        if now & 1 != 0 {
            return Some(now);
        }
        word.store(now.wrapping_add(1), std::sync::atomic::Ordering::SeqCst);
        Some(now.wrapping_add(1))
    }
    fn watch_holds(&self, address: u64, word: u32) -> bool {
        self.range(address, 1).is_ok_and(|range| {
            self.watch[range.start / PAGE_SIZE].load(std::sync::atomic::Ordering::SeqCst) == word
        })
    }
    fn wrote_straight(&mut self, address: u64, len: usize) {
        if let Ok(range) = self.range(address, len)
            && !range.is_empty()
        {
            self.stamp(range.start / PAGE_SIZE, (range.end - 1) / PAGE_SIZE);
        }
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn code_stamp(&self, address: u64, len: usize) -> Option<CodeStamp> {
        let range = self.range(address, len).ok()?;
        if range.is_empty() {
            return None;
        }
        let first = range.start / PAGE_SIZE;
        let last = (range.end - 1) / PAGE_SIZE;
        Some(CodeStamp {
            identity: self.identity,
            mapping: self.base,
            first_page: first as u64,
            last_page: last as u64,
            generation: *self.page_generations[first..=last].iter().max()?,
        })
    }
    fn code_bytes(&self, address: u64, bytes: &mut [u8]) -> bool {
        match self.range(address, bytes.len()) {
            Ok(range) => {
                bytes.copy_from_slice(&self.data[range]);
                true
            }
            Err(_) => false,
        }
    }
    fn reservation_generation(&self, address: u64, len: usize) -> u64 {
        // Every write stamps its pages with a fresh global generation, so the
        // newest stamp over the covered granules moves exactly when one of
        // them is written. A range outside RAM has no granule to track.
        match self.range(address, len) {
            Ok(range) if !range.is_empty() => {
                let first = range.start / RESERVATION_GRANULE;
                let last = (range.end - 1) / RESERVATION_GRANULE;
                self.page_generations[first..=last]
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(self.generation)
            }
            _ => self.generation,
        }
    }
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64> {
        self.read(address, bytes)?;
        Ok(self.reservation_generation(address, bytes.len()))
    }
    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool> {
        if self.reservation_generation(address, bytes.len()) != generation {
            return Ok(false);
        }
        self.write(address, bytes)?;
        Ok(true)
    }
    fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
        None
    }
}

/// Sequentially consistent shared RAM. Each host access is serialized, including
/// reservation validation and SC writes. Per-hart reservations remain in the CPU.
#[derive(Clone, Debug)]
pub struct SharedMemory {
    data: std::sync::Arc<std::sync::Mutex<FlatMemory>>,
    transaction: std::sync::Arc<parking_lot::ReentrantMutex<()>>,
}

impl SharedMemory {
    pub fn new(memory: FlatMemory) -> Self {
        Self {
            data: std::sync::Arc::new(std::sync::Mutex::new(memory)),
            transaction: std::sync::Arc::new(parking_lot::ReentrantMutex::new(())),
        }
    }
    fn with<T>(&self, f: impl FnOnce(&mut FlatMemory) -> T) -> T {
        let _transaction = self.transaction.lock();
        let mut memory = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut memory)
    }
}

impl Memory for SharedMemory {
    fn ram_bounds(&self, address: u64) -> Option<(u64, u64)> {
        self.with(|memory| memory.ram_bounds(address))
    }
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
        self.with(|memory| memory.read(address, bytes))
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        self.with(|memory| memory.write(address, bytes))
    }
    fn generation(&self) -> u64 {
        self.with(|memory| memory.generation())
    }
    fn code_stamp(&self, address: u64, len: usize) -> Option<CodeStamp> {
        self.with(|memory| memory.code_stamp(address, len))
    }
    fn code_bytes(&self, address: u64, bytes: &mut [u8]) -> bool {
        self.with(|memory| memory.code_bytes(address, bytes))
    }
    fn reservation_generation(&self, address: u64, len: usize) -> u64 {
        self.with(|memory| memory.reservation_generation(address, len))
    }
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64> {
        self.with(|memory| memory.load_reserved(address, bytes))
    }
    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool> {
        self.with(|memory| memory.store_conditional(address, bytes, generation))
    }
    fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
        Some(self.transaction.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_bounds_cover_the_region_without_artificial_chunk_boundaries() {
        let memory = FlatMemory::new(0x8000_0000, vec![0; 0x20000]);
        for address in [0x8000_0000, 0x8000_ffff, 0x8001_0000, 0x8001_ffff] {
            assert_eq!(memory.ram_bounds(address), Some((0x8000_0000, 0x8001_ffff)));
        }
        assert_eq!(memory.ram_bounds(0x7fff_ffff), None);
        assert_eq!(memory.ram_bounds(0x8002_0000), None);
        assert_eq!(FlatMemory::new(0, vec![]).ram_bounds(0), None);
        assert_eq!(
            FlatMemory::new(u64::MAX, vec![0]).ram_bounds(u64::MAX),
            Some((u64::MAX, u64::MAX))
        );
    }

    #[test]
    fn data_writes_preserve_code_but_cross_page_code_writes_invalidate() {
        let mut memory = FlatMemory::new(0x8000, vec![0; PAGE_SIZE * 3]);
        let first = memory.code_stamp(0x8000, 4).unwrap();
        let crossing = memory.code_stamp(0x8ffe, 4).unwrap();
        memory.write(0xa000, &[7]).unwrap();
        assert_eq!(memory.code_stamp(0x8000, 4), Some(first));
        assert_eq!(memory.code_stamp(0x8ffe, 4), Some(crossing));
        memory.write(0x9000, &[9]).unwrap();
        assert_eq!(memory.code_stamp(0x8000, 4), Some(first));
        assert_ne!(memory.code_stamp(0x8ffe, 4), Some(crossing));
        memory.write(0x8fff, &[1, 2]).unwrap();
        assert_ne!(memory.code_stamp(0x8000, 4), Some(first));
    }

    #[test]
    fn code_bytes_copy_ram_and_refuse_what_is_outside_it() {
        let mut memory = FlatMemory::new(0x8000, vec![0; PAGE_SIZE]);
        memory.write(0x8ffc, &[1, 2, 3, 4]).unwrap();
        let stamp = memory.code_stamp(0x8ffc, 4);
        let mut bytes = [0; 4];
        assert!(memory.code_bytes(0x8ffc, &mut bytes));
        assert_eq!(bytes, [1, 2, 3, 4]);
        assert_eq!(memory.code_stamp(0x8ffc, 4), stamp);
        assert!(!memory.code_bytes(0x8ffe, &mut bytes));
        assert!(!memory.code_bytes(0x7fff, &mut bytes));
        let shared = SharedMemory::new(memory);
        assert!(shared.code_bytes(0x8ffc, &mut bytes));
        assert_eq!(bytes, [1, 2, 3, 4]);
    }

    #[test]
    fn replacement_and_remapping_do_not_reuse_a_code_stamp() {
        let mut memory = FlatMemory::new(0x8000, vec![0; PAGE_SIZE * 2]);
        let old = memory.code_stamp(0x8004, 4).unwrap();
        let replacement = FlatMemory::new(0x8000, vec![0; PAGE_SIZE * 2]);
        assert_ne!(replacement.code_stamp(0x8004, 4), Some(old));
        memory.base = 0x7000;
        assert_ne!(memory.code_stamp(0x8004, 4), Some(old));
        assert!(memory.code_stamp(0x6000, 4).is_none());
    }
    #[test]
    fn bounded_access_and_code_generations() {
        let mut m = FlatMemory::new(0x8000, vec![0; 8]);
        assert!(m.write(0x7fff, &[1]).is_err());
        assert!(m.write(u64::MAX, &[1, 2]).is_err());
        assert!(m.write(0x8007, &[1, 2]).is_err());
        m.write(0x8000, &[3, 4]).unwrap();
        let mut bytes = [0; 2];
        m.read(0x8000, &mut bytes).unwrap();
        assert_eq!(bytes, [3, 4]);
        assert_eq!(m.generation(), 1);
    }

    #[test]
    fn a_store_breaks_only_the_reservations_of_its_own_granule() {
        let granule = RESERVATION_GRANULE as u64;
        let mut hart0 = SharedMemory::new(FlatMemory::new(0x8000, vec![0; PAGE_SIZE * 4]));
        let mut hart1 = hart0.clone();
        let mut bytes = [0; 8];
        // Another hart stores to other granules: the reservation survives.
        let reserved = hart0.load_reserved(0x8000, &mut bytes).unwrap();
        for address in [0x8000 + granule, 0x8000 + 3 * granule] {
            hart1.write(address, &7u64.to_le_bytes()).unwrap();
        }
        assert_eq!(hart0.reservation_generation(0x8000, 8), reserved);
        assert!(
            hart0
                .store_conditional(0x8000, &1u64.to_le_bytes(), reserved)
                .unwrap()
        );
        // A store to the reserved granule itself, even to other bytes of it,
        // breaks the reservation and the SC writes nothing.
        let reserved = hart0.load_reserved(0x8000, &mut bytes).unwrap();
        hart1.write(0x8000 + granule - 1, &[9]).unwrap();
        assert_ne!(hart0.reservation_generation(0x8000, 8), reserved);
        assert!(
            !hart0
                .store_conditional(0x8000, &2u64.to_le_bytes(), reserved)
                .unwrap()
        );
        hart0.read(0x8000, &mut bytes).unwrap();
        assert_eq!(u64::from_le_bytes(bytes), 1);
        // An access across a granule boundary is covered by both granules.
        let reserved = hart0
            .load_reserved(0x8000 + granule - 4, &mut bytes)
            .unwrap();
        hart1.write(0x8000 + granule + 100, &[1]).unwrap();
        assert!(
            !hart0
                .store_conditional(0x8000 + granule - 4, &3u64.to_le_bytes(), reserved)
                .unwrap()
        );
        // A stale reservation never validates again after the granule moved on.
        let stale = hart0.load_reserved(0x8000, &mut bytes).unwrap();
        hart1.write(0x8000, &[5]).unwrap();
        let fresh = hart0.load_reserved(0x8000, &mut bytes).unwrap();
        assert_ne!(stale, fresh);
        assert!(!hart0.store_conditional(0x8000, &[0; 8], stale).unwrap());
        assert!(hart0.store_conditional(0x8000, &[0; 8], fresh).unwrap());
    }

    #[test]
    fn other_hart_invalidates_reservation_and_sc_is_atomic() {
        let mut hart0 = SharedMemory::new(FlatMemory::new(0, vec![0; 8]));
        let mut hart1 = hart0.clone();
        let mut bytes = [0; 8];
        let generation = hart0.load_reserved(0, &mut bytes).unwrap();
        std::thread::spawn(move || hart1.write(0, &7u64.to_le_bytes()).unwrap())
            .join()
            .unwrap();
        assert!(
            !hart0
                .store_conditional(0, &9u64.to_le_bytes(), generation)
                .unwrap()
        );
        let generation = hart0.load_reserved(0, &mut bytes).unwrap();
        assert_eq!(u64::from_le_bytes(bytes), 7);
        assert!(
            hart0
                .store_conditional(0, &11u64.to_le_bytes(), generation)
                .unwrap()
        );
    }
}
