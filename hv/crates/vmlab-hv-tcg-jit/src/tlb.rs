//! The fast TLB of compiled code (3e DESIGN.md H7): what the inlined load
//! and store sequences of `emit` read, and the operations by which the
//! owner of a CPU fills and flushes it. The owner decides what goes in
//! (H7a) and when it goes (H7b): this file knows no guest architecture.
//!
//! The fast TLB is not a second translation: every entry is a copy of what
//! the architectural TLB of the CPU holds, filled where the Rust load and
//! store paths reach RAM through the host address of that entry (card S3),
//! and dropped with it. It has four tables, one for reads and one for
//! writes of each of two levels, each of `ENTRIES` entries direct-mapped by
//! the 4 KiB page of the virtual address. An entry holds:
//!
//! - `tag`: the virtual address of the page, its low 12 bits zero. An empty
//!   entry holds `EMPTY`, which no masked address equals.
//! - `addend`: the host address of a byte is its virtual address plus this.
//! - `watch`: the address of the watch word of the physical page (card S3).
//!   A store reads it after its write and tells the owner when it is odd.
//!   Only a write entry needs it.
//! - `phys`: the physical address of the page, which compiled code does not
//!   read: the owner drops the write entries of a page that gets
//!   translated code by it (H11a).
//!
//! The inlined lookup masks the address with the page bits and the bits
//! below the size of the access, and compares that with the tag. So an
//! access that is not aligned to its size misses, and such an access is
//! the only one that can cross its page.
//!
//! A flush, and the search of the write entries of a page that gets code,
//! visit the places filled since the last flush while there are at most
//! `TRACKED`, and sweep the tables only past that: their cost follows what
//! the TLB holds and not its size. A boot flushes about a million times
//! (card J12: a sweep of the whole tables each time took 2.9 s of a boot at
//! 4,096 entries and 55 s at 65,536).

/// The entries of one table: 4 MiB of guest pages. From the curve of card
/// J12 (3e DESIGN.md J12a): at 1,024 a boot misses 2.6 % of its inlined
/// accesses and 256 entries 3.1 %, and 4,096 to 65,536 entries take a
/// tenth of what is left at most. The four tables are 128 KiB.
pub const ENTRIES: usize = 1024;
/// Log2 of the bytes of one entry.
pub const ENTRY_BITS: u32 = 5;
/// The tag of an empty entry. A masked address has at most its three low
/// bits set below the page bits, so it never equals this.
pub const EMPTY: u64 = u64::MAX;

/// The most mappings larger than a page that the TLB keeps apart. With
/// more, every invalidation by address drops everything.
const LARGE: usize = 64;

/// The most places that the TLB remembers as filled since the last flush
/// of everything. Up to that many, a flush and the search for the write
/// entries of a page visit those places and not the tables, so that their
/// cost follows what the TLB holds and not its size (3e DESIGN.md J12).
const TRACKED: usize = ENTRIES / 4;

/// One entry, as compiled code reads it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub tag: u64,
    pub addend: u64,
    pub watch: u64,
    pub phys: u64,
}

const _: () = assert!(std::mem::size_of::<Entry>() == 1 << ENTRY_BITS);

const NOTHING: Entry = Entry {
    tag: EMPTY,
    addend: 0,
    watch: 0,
    phys: 0,
};

/// A table that holds nothing: compiled code reads it in place of a real
/// one where nothing may hit, for example the writes while an exclusive
/// mark is set. Nothing writes it.
pub static EMPTY_TABLE: [Entry; ENTRIES] = [NOTHING; ENTRIES];

/// The offsets in an entry, for the emitter.
pub(crate) const TAG: i32 = 0;
pub(crate) const ADDEND: i32 = 8;
pub(crate) const WATCH: i32 = 16;

/// Two entries, so that an entry never crosses a cache line of the host.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Line([Entry; 2]);

const EMPTY_LINE: Line = Line([NOTHING; 2]);

/// The four tables, one after the other: table `2 * level + write`. Level
/// 0 serves a block of the lower level of the guest (EL0 of aarch64), level
/// 1 the upper one. On the heap: at a larger `ENTRIES` the tables are more
/// than a stack holds.
pub struct FastTlb {
    lines: Box<[Line]>,
    /// Entries that hold something, so that a flush of an empty TLB costs
    /// nothing.
    used: usize,
    /// The mappings larger than a page that entries were filled from since
    /// the last flush of everything, as a page of the mapping and the bits
    /// of a page number inside it. `None`: more than `LARGE`.
    large: Option<Vec<(u64, u32)>>,
    /// The places filled since the last flush of everything, as indices
    /// of the tables taken one after the other, while there are at most
    /// `TRACKED`. A place can be in it twice, or be empty again. `None`:
    /// more, and every entry may hold something.
    filled: Option<Vec<u32>>,
    /// Fills and the flushes of everything, for the counts of the owner.
    pub fills: u64,
    pub flushes: u64,
    /// The searches of the write tables for a page that gets code
    /// (`drop_writes` on a TLB that holds something), and the time that
    /// the flushes and those searches took, for the cost of a size.
    pub scans: u64,
    pub flush_ns: u64,
}

impl FastTlb {
    pub fn new() -> Box<Self> {
        Box::new(Self {
            lines: vec![EMPTY_LINE; 2 * ENTRIES].into_boxed_slice(),
            used: 0,
            large: Some(Vec::new()),
            filled: Some(Vec::with_capacity(TRACKED)),
            fills: 0,
            flushes: 0,
            scans: 0,
            flush_ns: 0,
        })
    }

    #[inline(always)]
    fn index(va: u64) -> usize {
        (va >> 12) as usize & (ENTRIES - 1)
    }

    /// The entry at `at` of the tables, table after table.
    #[inline(always)]
    fn entry(&self, at: usize) -> &Entry {
        &self.lines[at / 2].0[at % 2]
    }

    #[inline(always)]
    fn entry_mut(&mut self, at: usize) -> &mut Entry {
        &mut self.lines[at / 2].0[at % 2]
    }

    /// The place of the entry `index` of the table of `level` for reads or
    /// for writes.
    #[inline(always)]
    fn place(level: usize, write: bool, index: usize) -> usize {
        ((level & 1) * 2 + usize::from(write)) * ENTRIES + index
    }

    /// The table of `level` for reads or for writes, as compiled code takes
    /// it.
    pub fn table(&self, level: usize, write: bool) -> *const Entry {
        let lines = self.lines.as_ptr().cast::<Entry>();
        // In the allocation: a place is less than its length in entries.
        lines.wrapping_add(Self::place(level, write, 0))
    }

    /// The entry that an access of `bytes` bytes at `va` hits, as the
    /// inlined lookup finds it.
    pub fn lookup(&self, level: usize, write: bool, va: u64, bytes: u8) -> Option<&Entry> {
        let entry = self.entry(Self::place(level, write, Self::index(va)));
        (va & (!0xfff | (u64::from(bytes.max(1)) - 1)) == entry.tag).then_some(entry)
    }

    /// Keep the page of `va` for `level`: its host page at `host`, its
    /// watch word at `watch` (0 for a read entry), its physical address
    /// `phys`, from a mapping of `1 << span` bytes. The caller checked
    /// what H7a asks.
    #[allow(clippy::too_many_arguments)]
    pub fn fill(
        &mut self,
        level: usize,
        write: bool,
        va: u64,
        host: usize,
        watch: usize,
        phys: u64,
        span: u8,
    ) {
        let tag = va & !0xfff;
        let at = Self::place(level, write, Self::index(va));
        if self.entry(at).tag == EMPTY {
            self.used += 1;
            if let Some(filled) = &mut self.filled {
                if filled.len() < TRACKED {
                    filled.push(at as u32);
                } else {
                    self.filled = None;
                }
            }
        }
        let place = self.entry_mut(at);
        *place = Entry {
            tag,
            addend: (host as u64).wrapping_sub(tag),
            watch: watch as u64,
            phys: phys & !0xfff,
        };
        self.fills += 1;
        let within = u32::from(span).saturating_sub(12);
        if within != 0
            && let Some(large) = &mut self.large
            && !large
                .iter()
                .any(|&(page, bits)| bits == within && (page ^ (va >> 12)) >> within == 0)
        {
            if large.len() < LARGE {
                large.push((va >> 12, within));
            } else {
                self.large = None;
            }
        }
    }

    /// Drop every entry.
    pub fn flush(&mut self) {
        if self.used != 0 {
            let start = std::time::Instant::now();
            match &mut self.filled {
                Some(filled) => {
                    for &at in filled.iter() {
                        let at = at as usize;
                        self.lines[at / 2].0[at % 2] = NOTHING;
                    }
                    filled.clear();
                }
                None => {
                    self.lines.fill(EMPTY_LINE);
                    self.filled = Some(Vec::with_capacity(TRACKED));
                }
            }
            self.used = 0;
            self.flushes += 1;
            self.flush_ns += start.elapsed().as_nanos() as u64;
        }
        self.large = Some(Vec::new());
    }

    /// Drop what an invalidation of the virtual page number `page` names,
    /// compared in its `significant` bits: the entries at its place in
    /// every table, or everything when an entry may come from a larger
    /// mapping that holds the page.
    pub fn flush_page(&mut self, page: u64, significant: u64) {
        if self.used == 0 {
            return;
        }
        let in_large = self.large.as_ref().is_none_or(|large| {
            large
                .iter()
                .any(|&(other, within)| ((other ^ page) & significant) >> within == 0)
        });
        if in_large {
            self.flush();
            return;
        }
        let index = page as usize & (ENTRIES - 1);
        for table in 0..4 {
            let entry = self.entry_mut(table * ENTRIES + index);
            if entry.tag != EMPTY {
                *entry = NOTHING;
                self.used -= 1;
            }
        }
    }

    /// Drop the write entries of the physical page that holds `phys`: it
    /// holds translated code from now on (H11a).
    pub fn drop_writes(&mut self, phys: u64) {
        if self.used == 0 {
            return;
        }
        let start = std::time::Instant::now();
        let page = phys & !0xfff;
        let mut dropped = 0;
        let mut clear = |entry: &mut Entry| {
            if entry.tag != EMPTY && entry.phys == page {
                *entry = NOTHING;
                dropped += 1;
            }
        };
        match &self.filled {
            // A write table is an odd one.
            Some(filled) => {
                for &at in filled.iter().filter(|&&at| (at as usize / ENTRIES) & 1 == 1) {
                    let at = at as usize;
                    clear(&mut self.lines[at / 2].0[at % 2]);
                }
            }
            None => {
                for level in 0..2 {
                    let from = Self::place(level, true, 0) / 2;
                    for line in &mut self.lines[from..from + ENTRIES / 2] {
                        line.0.iter_mut().for_each(&mut clear);
                    }
                }
            }
        }
        self.used -= dropped;
        self.scans += 1;
        self.flush_ns += start.elapsed().as_nanos() as u64;
    }

    /// Whether the TLB remembers the places that it filled.
    #[cfg(test)]
    fn tracking(&self) -> bool {
        self.filled.is_some()
    }

    /// The entries that hold something.
    pub fn len(&self) -> usize {
        self.used
    }

    pub fn is_empty(&self) -> bool {
        self.used == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hit_needs_the_page_and_the_alignment_of_the_access() {
        let mut tlb = FastTlb::new();
        tlb.fill(1, false, 0x1234_5678, 0x7000_0000, 0, 0x8000_5000, 12);
        let entry = *tlb.lookup(1, false, 0x1234_5000, 8).expect("a hit");
        assert_eq!(entry.tag, 0x1234_5000);
        assert_eq!(0x1234_5678u64.wrapping_add(entry.addend), 0x7000_0678);
        assert_eq!(entry.phys, 0x8000_5000);
        // Aligned accesses of every size hit, up to the last of the page.
        for bytes in [1u8, 2, 4, 8] {
            assert!(
                tlb.lookup(1, false, 0x1234_6000 - u64::from(bytes), bytes)
                    .is_some()
            );
        }
        // One that is not aligned to its size misses, and so does every one
        // that crosses into the next page.
        assert!(tlb.lookup(1, false, 0x1234_5001, 2).is_none());
        assert!(tlb.lookup(1, false, 0x1234_5ffc, 8).is_none());
        assert!(tlb.lookup(1, false, 0x1234_5fff, 1).is_some());
        // The other level, the other kind, and another page with its place.
        assert!(tlb.lookup(0, false, 0x1234_5000, 8).is_none());
        assert!(tlb.lookup(1, true, 0x1234_5000, 8).is_none());
        let rival = 0x1234_5000 + ((ENTRIES as u64) << 12);
        assert!(tlb.lookup(1, false, rival, 8).is_none());
        assert_eq!(tlb.len(), 1);
    }

    #[test]
    fn the_tables_follow_each_other_and_no_entry_crosses_a_cache_line() {
        let tlb = FastTlb::new();
        let first = tlb.table(0, false) as usize;
        assert_eq!(first % 64, 0);
        let size = ENTRIES * std::mem::size_of::<Entry>();
        assert_eq!(tlb.table(0, true) as usize, first + size);
        assert_eq!(tlb.table(1, false) as usize, first + 2 * size);
        assert_eq!(tlb.table(1, true) as usize, first + 3 * size);
        // The last entry of the last table is in the TLB.
        let mut tlb = FastTlb::new();
        let va = ((ENTRIES as u64 - 1) << 12) | 0x10;
        tlb.fill(1, true, va, 0x7000_0000, 0x55, 0x9000, 12);
        let entry = tlb.lookup(1, true, va, 8).expect("a hit") as *const Entry as usize;
        assert_eq!(entry, tlb.table(1, true) as usize + size - 32);
    }

    #[test]
    fn flushes_and_searches_find_every_entry_with_few_or_many_filled() {
        // Few: the places are remembered. Many: past `TRACKED`, the tables
        // are swept. Each with a place filled twice and one dropped between.
        for pages in [3u64, TRACKED as u64 + 5] {
            let mut tlb = FastTlb::new();
            for page in 0..pages {
                let write = page % 2 == 1;
                tlb.fill(1, write, page << 12, 0x1000_0000, 0x55, 0x9000, 12);
            }
            tlb.fill(0, true, 0x1000, 0x2000_0000, 0x56, 0xa000, 12);
            tlb.flush_page(0, u64::MAX);
            tlb.fill(1, false, 0, 0x1000_0000, 0, 0x9000, 12);
            assert_eq!(tlb.tracking(), pages < TRACKED as u64);
            // Every write entry of the page 0x9000 goes, and only those.
            tlb.drop_writes(0x9000);
            let writes = pages / 2;
            assert_eq!(tlb.len() as u64, pages - writes + 1);
            assert!(tlb.lookup(0, true, 0x1000, 8).is_some());
            assert!((0..pages).all(|page| tlb.lookup(1, true, page << 12, 1).is_none()));
            tlb.flush();
            assert!(tlb.is_empty());
            for (table, write) in [(0, false), (0, true), (1, false), (1, true)] {
                let first = tlb.table(table, write);
                // SAFETY: the table has `ENTRIES` entries from `first`.
                let entries = unsafe { std::slice::from_raw_parts(first, ENTRIES) };
                assert!(entries.iter().all(|entry| *entry == NOTHING));
            }
            assert!(tlb.tracking());
        }
    }

    #[test]
    fn an_empty_entry_matches_no_address() {
        let tlb = FastTlb::new();
        for va in [0, u64::MAX, !0xfff, u64::MAX - 7] {
            for bytes in [1u8, 2, 4, 8] {
                assert!(tlb.lookup(0, false, va, bytes).is_none());
                assert!(tlb.lookup(1, true, va, bytes).is_none());
            }
        }
        assert!(EMPTY_TABLE.iter().all(|entry| entry.tag == EMPTY));
    }

    #[test]
    fn an_invalidation_by_address_drops_its_place_or_everything() {
        let mut tlb = FastTlb::new();
        for level in 0..2 {
            for write in [false, true] {
                tlb.fill(level, write, 0x5000, 0x10_0000, 0, 0x9000, 12);
                tlb.fill(level, write, 0x6000, 0x20_0000, 0, 0xa000, 12);
            }
        }
        assert_eq!(tlb.len(), 8);
        // The place of the page in every table, and nothing else.
        tlb.flush_page(5, u64::MAX);
        assert_eq!(tlb.len(), 4);
        assert!(tlb.lookup(1, true, 0x6000, 1).is_some());
        // A page of a 2 MiB mapping: the invalidation of another page of
        // that mapping drops everything.
        tlb.fill(1, false, 0x20_0000, 0x40_0000, 0, 0x20_0000, 21);
        tlb.flush_page(0x201, u64::MAX);
        assert!(tlb.is_empty());
        // The large mappings are forgotten with a flush of everything.
        tlb.fill(1, false, 0x6000, 0x20_0000, 0, 0xa000, 12);
        tlb.flush_page(0x201, u64::MAX);
        assert_eq!(tlb.len(), 1);
        // More larger mappings than it keeps apart: every invalidation
        // drops everything.
        for mapping in 0..(LARGE as u64 + 1) {
            tlb.fill(0, false, 0x4000_0000 + (mapping << 21), 0x1000, 0, 0, 21);
        }
        tlb.flush_page(0x7_0000, u64::MAX);
        assert!(tlb.is_empty());
    }

    #[test]
    fn a_page_that_gets_code_loses_its_write_entries_only() {
        let mut tlb = FastTlb::new();
        tlb.fill(1, false, 0x5000, 0x10_0000, 0, 0x9000, 12);
        tlb.fill(1, true, 0x5000, 0x10_0000, 0x77, 0x9000, 12);
        tlb.fill(0, true, 0x8000, 0x10_0000, 0x77, 0x9000, 12);
        tlb.fill(0, true, 0x6000, 0x20_0000, 0x78, 0xa000, 12);
        tlb.drop_writes(0x9abc);
        assert!(tlb.lookup(1, false, 0x5000, 8).is_some());
        assert!(tlb.lookup(1, true, 0x5000, 8).is_none());
        assert!(tlb.lookup(0, true, 0x8000, 8).is_none());
        assert_eq!(
            tlb.lookup(0, true, 0x6000, 8).map(|entry| entry.watch),
            Some(0x78)
        );
        assert_eq!(tlb.len(), 2);
        tlb.flush();
        assert!(tlb.is_empty() && tlb.lookup(1, false, 0x5000, 8).is_none());
        assert_eq!(tlb.flushes, 1);
    }
}
