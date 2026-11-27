//! A softmmu TLB (phase 3d, R12): virtual page to physical page, with the
//! address space identifier of the entry and what the architecture keeps
//! with a translation.
//!
//! The TLB knows no architecture. A page is 4096 bytes, which every guest
//! granule is a multiple of. One `Tlb` serves one translation regime. An
//! entry is tagged with an address space identifier unless it is global, so
//! a change of the identifier needs no flush. The owner gives the meaning of
//! the permission bits in `flags`.
//!
//! An entry also keeps the size of the guest mapping that it came from
//! (`span`): an invalidation by address names that whole mapping, and the
//! TLB holds one entry for each page of it that was used.
//!
//! An entry can keep the host address of its physical page (card S3): a
//! CPU then reads and writes RAM with one probe of the TLB, an add and the
//! access, and asks its memory nothing. The owner fills `host` and `watch`
//! from `Memory::host_page`, and drops every entry when the memory names
//! another mapping.
//!
//! The index of a page folds its higher bits onto its low ones, so pages a
//! multiple of the table size apart do not take the same place. An
//! invalidation by address looks at one place when no entry of a larger
//! mapping covers the address, and at every entry when one does.

/// The bits of an address inside a page.
pub const PAGE_BITS: u32 = 12;

/// One translation of one page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The virtual page number: the address without its low `PAGE_BITS`
    /// bits. `EMPTY` in an entry that holds nothing.
    pub page: u64,
    /// The physical address of the first byte of the page.
    pub phys: u64,
    /// The host address of the first byte of the physical page, or 0: an
    /// access goes through the memory.
    pub host: usize,
    /// The address of the watch word of the physical page
    /// (`HostPage::watch`), or 0: a write goes through the memory.
    pub watch: usize,
    /// The address space identifier that the entry belongs to. It is not
    /// looked at for a global entry.
    pub asid: u16,
    /// What the owner keeps with a translation: permissions, memory type.
    pub flags: u16,
    /// Log2 of the size of the guest mapping, at least `PAGE_BITS`.
    pub span: u8,
    /// The entry holds for every address space identifier.
    pub global: bool,
}

/// The page number of an entry that holds nothing. No address has it.
pub const EMPTY: u64 = u64::MAX;

const NOTHING: Entry = Entry {
    page: EMPTY,
    phys: 0,
    host: 0,
    watch: 0,
    asid: 0,
    flags: 0,
    span: PAGE_BITS as u8,
    global: false,
};

/// The most mappings larger than a page that the TLB keeps apart. With
/// more, an invalidation by address looks at every entry until the TLB is
/// empty again.
const LARGE: usize = 64;

/// A direct-mapped TLB.
#[derive(Clone, Debug)]
pub struct Tlb {
    entries: Box<[Entry]>,
    bits: u32,
    mask: usize,
    /// Entries that hold a translation. An empty TLB is flushed in no time.
    used: usize,
    /// The mappings larger than a page that entries came from, each as a
    /// page of it and the bits of a page number inside it. There can be
    /// some that no entry holds any more. `None`: more than `LARGE`.
    large: Option<Vec<(u64, u32)>>,
}

impl Tlb {
    /// A TLB of `1 << bits` entries. An invalidation by address compares at
    /// least the low `2 * bits` bits of a page number, which the index
    /// takes.
    pub fn new(bits: u32) -> Self {
        assert!((1..=16).contains(&bits), "a TLB of 2 to 65536 entries");
        let count = 1usize << bits;
        Self {
            entries: vec![NOTHING; count].into_boxed_slice(),
            bits,
            mask: count - 1,
            used: 0,
            large: Some(Vec::new()),
        }
    }

    /// The place of `page`: its low bits, folded with the next ones.
    #[inline(always)]
    fn index(&self, page: u64) -> usize {
        (page ^ (page >> self.bits)) as usize & self.mask
    }

    /// The entry of `page` for the address space `asid`.
    #[inline(always)]
    pub fn lookup(&self, page: u64, asid: u16) -> Option<&Entry> {
        // SAFETY: `index` is masked to the length of `entries`.
        let entry = unsafe { self.entries.get_unchecked(self.index(page)) };
        (entry.page == page && (entry.global || entry.asid == asid)).then_some(entry)
    }

    /// Keep `entry`. It replaces what its place held.
    pub fn insert(&mut self, entry: Entry) {
        debug_assert_ne!(entry.page, EMPTY);
        let index = self.index(entry.page);
        let place = &mut self.entries[index];
        if place.page == EMPTY {
            self.used += 1;
        }
        *place = entry;
        let within = u32::from(entry.span) - PAGE_BITS;
        if within != 0
            && let Some(large) = &mut self.large
            && !large
                .iter()
                .any(|&(page, bits)| bits == within && (page ^ entry.page) >> within == 0)
        {
            if large.len() < LARGE {
                large.push((entry.page, within));
            } else {
                self.large = None;
            }
        }
    }

    /// Drop every entry.
    pub fn flush(&mut self) {
        if self.used != 0 {
            self.entries.fill(NOTHING);
            self.used = 0;
        }
        self.large = Some(Vec::new());
    }

    fn drop_where(&mut self, mut gone: impl FnMut(&Entry) -> bool) {
        if self.used == 0 {
            return;
        }
        let mut large = Vec::new();
        for entry in self.entries.iter_mut() {
            if entry.page == EMPTY {
                continue;
            }
            if gone(entry) {
                *entry = NOTHING;
                self.used -= 1;
                continue;
            }
            let within = u32::from(entry.span) - PAGE_BITS;
            if within != 0
                && large.len() <= LARGE
                && !large
                    .iter()
                    .any(|&(page, bits)| bits == within && (page ^ entry.page) >> within == 0)
            {
                large.push((entry.page, within));
            }
        }
        // A look at every entry knows the larger mappings again.
        self.large = (large.len() <= LARGE).then_some(large);
    }

    /// Drop the entries of the mapping that holds `page`. `significant`: the
    /// bits of a page number that are compared, for an architecture whose
    /// invalidation names fewer address bits than a page number has. It
    /// holds at least the low `2 * bits` bits of a page number.
    /// `asid`: only global entries and those of this address space.
    pub fn flush_page(&mut self, page: u64, significant: u64, asid: Option<u16>) {
        if self.used == 0 {
            return;
        }
        let matches = |entry: &Entry| {
            let within = u32::from(entry.span) - PAGE_BITS;
            ((entry.page ^ page) & significant) >> within == 0
                && asid.is_none_or(|asid| entry.global || entry.asid == asid)
        };
        let in_large = self.large.as_ref().is_none_or(|large| {
            large
                .iter()
                .any(|&(other, within)| ((other ^ page) & significant) >> within == 0)
        });
        if in_large {
            self.drop_where(matches);
            return;
        }
        // Every entry of one page that the invalidation names has the low
        // bits of `page`, and so its place.
        let index = self.index(page);
        let entry = &mut self.entries[index];
        if entry.page != EMPTY && matches(entry) {
            *entry = NOTHING;
            self.used -= 1;
        }
    }

    /// Drop the entries of the address space `asid` that are not global.
    pub fn flush_asid(&mut self, asid: u16) {
        self.drop_where(|entry| !entry.global && entry.asid == asid);
    }

    /// The count of entries that hold a translation.
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

    fn entry(page: u64, asid: u16, global: bool, span: u8) -> Entry {
        Entry {
            page,
            phys: page << PAGE_BITS,
            host: 0,
            watch: 0,
            asid,
            flags: 3,
            span,
            global,
        }
    }

    #[test]
    fn an_entry_answers_for_its_address_space_and_a_global_one_for_all() {
        let mut tlb = Tlb::new(4);
        tlb.insert(entry(0x10, 7, false, 12));
        tlb.insert(entry(0x11, 7, true, 12));
        assert_eq!(tlb.lookup(0x10, 7).map(|entry| entry.phys), Some(0x10000));
        assert!(tlb.lookup(0x10, 8).is_none());
        assert!(tlb.lookup(0x11, 8).is_some());
        // Another page with the same place replaces the entry: 0x101 folds
        // onto the place of 0x10.
        tlb.insert(entry(0x101, 7, false, 12));
        assert!(tlb.lookup(0x10, 7).is_none());
        assert_eq!(tlb.len(), 2);
        tlb.flush();
        assert!(tlb.is_empty() && tlb.lookup(0x101, 7).is_none());
    }

    #[test]
    fn pages_a_multiple_of_the_table_apart_keep_their_entries() {
        // 4 MiB apart, which took one place of 1,024 before the fold.
        let mut tlb = Tlb::new(10);
        for page in [0x400, 0x800, 0xc00, 0x1000] {
            tlb.insert(entry(page, 1, false, 12));
        }
        assert_eq!(tlb.len(), 4);
        for page in [0x400, 0x800, 0xc00, 0x1000] {
            assert!(tlb.lookup(page, 1).is_some());
        }
    }

    #[test]
    fn an_invalidation_of_a_page_looks_at_its_place_or_at_all_of_them() {
        let mut tlb = Tlb::new(10);
        for page in 0..512 {
            tlb.insert(entry(page, 1, false, 12));
        }
        // No larger mapping: one place, and only the entry of the page.
        tlb.flush_page(5, u64::MAX, Some(1));
        assert_eq!(tlb.len(), 511);
        assert!(tlb.lookup(5, 1).is_none() && tlb.lookup(4, 1).is_some());
        // Another address space keeps the entry.
        tlb.flush_page(6, u64::MAX, Some(2));
        assert_eq!(tlb.len(), 511);
        // A page of a 2 MiB mapping: the invalidation of any page of it
        // drops all of its entries.
        for page in [0x10_0000, 0x10_0042, 0x10_01ff] {
            tlb.insert(entry(page, 1, false, 21));
        }
        tlb.flush_page(0x10_0100, u64::MAX, Some(1));
        for page in [0x10_0000, 0x10_0042, 0x10_01ff] {
            assert!(tlb.lookup(page, 1).is_none());
        }
        assert!(tlb.lookup(100, 1).is_some());
        // More larger mappings than the TLB keeps apart: every invalidation
        // looks at every entry, and is still right.
        for mapping in 0..(LARGE as u64 + 1) {
            tlb.insert(entry(0x20_0000 + (mapping << 9), 1, false, 21));
        }
        assert!(tlb.large.is_none());
        tlb.flush_page(0x20_0000 + (3 << 9) + 7, u64::MAX, None);
        assert!(tlb.lookup(0x20_0000 + (3 << 9), 1).is_none());
        assert!(tlb.lookup(0x20_0000 + (4 << 9), 1).is_some());
        assert!(tlb.lookup(100, 1).is_some());
        tlb.flush();
        assert_eq!(tlb.large, Some(Vec::new()));
    }

    #[test]
    fn an_invalidation_by_address_takes_the_whole_mapping() {
        let mut tlb = Tlb::new(12);
        // Three pages of one 2 MiB mapping, and a page beside it.
        for page in [0x200, 0x2ff, 0x3ff] {
            tlb.insert(entry(page, 1, false, 21));
        }
        tlb.insert(entry(0x400, 1, false, 12));
        tlb.insert(entry(0x1_0000_0000_0201, 1, false, 12));
        // Another address space: nothing goes.
        tlb.flush_page(0x280, u64::MAX, Some(2));
        assert_eq!(tlb.len(), 5);
        tlb.flush_page(0x280, u64::MAX, Some(1));
        assert_eq!(tlb.len(), 2);
        assert!(tlb.lookup(0x400, 1).is_some());
        // With fewer significant bits the page with the high bits goes too.
        tlb.flush_page(0x201, 0xffff_ffff, None);
        assert_eq!(tlb.len(), 1);
    }

    #[test]
    fn an_invalidation_by_address_space_keeps_the_global_entries() {
        let mut tlb = Tlb::new(4);
        tlb.insert(entry(1, 5, false, 12));
        tlb.insert(entry(2, 5, true, 12));
        tlb.insert(entry(3, 6, false, 12));
        tlb.flush_asid(5);
        assert!(tlb.lookup(1, 5).is_none());
        assert!(tlb.lookup(2, 5).is_some() && tlb.lookup(3, 6).is_some());
        // A global entry goes with its address, whatever the address space.
        tlb.flush_page(2, u64::MAX, Some(9));
        assert_eq!(tlb.len(), 1);
    }
}
