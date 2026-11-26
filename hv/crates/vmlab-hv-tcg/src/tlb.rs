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
    asid: 0,
    flags: 0,
    span: PAGE_BITS as u8,
    global: false,
};

/// A direct-mapped TLB.
#[derive(Clone, Debug)]
pub struct Tlb {
    entries: Box<[Entry]>,
    mask: usize,
    /// Entries that hold a translation. An empty TLB is flushed in no time.
    used: usize,
}

impl Tlb {
    /// A TLB of `1 << bits` entries.
    pub fn new(bits: u32) -> Self {
        let count = 1usize << bits;
        Self {
            entries: vec![NOTHING; count].into_boxed_slice(),
            mask: count - 1,
            used: 0,
        }
    }

    /// The entry of `page` for the address space `asid`.
    #[inline(always)]
    pub fn lookup(&self, page: u64, asid: u16) -> Option<&Entry> {
        let entry = &self.entries[page as usize & self.mask];
        (entry.page == page && (entry.global || entry.asid == asid)).then_some(entry)
    }

    /// Keep `entry`. It replaces what its place held.
    pub fn insert(&mut self, entry: Entry) {
        debug_assert_ne!(entry.page, EMPTY);
        let place = &mut self.entries[entry.page as usize & self.mask];
        if place.page == EMPTY {
            self.used += 1;
        }
        *place = entry;
    }

    /// Drop every entry.
    pub fn flush(&mut self) {
        if self.used != 0 {
            self.entries.fill(NOTHING);
            self.used = 0;
        }
    }

    fn drop_where(&mut self, mut gone: impl FnMut(&Entry) -> bool) {
        if self.used == 0 {
            return;
        }
        for entry in self.entries.iter_mut() {
            if entry.page != EMPTY && gone(entry) {
                *entry = NOTHING;
                self.used -= 1;
            }
        }
    }

    /// Drop the entries of the mapping that holds `page`. `significant`: the
    /// bits of a page number that are compared, for an architecture whose
    /// invalidation names fewer address bits than a page number has.
    /// `asid`: only global entries and those of this address space.
    pub fn flush_page(&mut self, page: u64, significant: u64, asid: Option<u16>) {
        self.drop_where(|entry| {
            let within = u32::from(entry.span) - PAGE_BITS;
            ((entry.page ^ page) & significant) >> within == 0
                && asid.is_none_or(|asid| entry.global || entry.asid == asid)
        });
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
        // Another page with the same place replaces the entry.
        tlb.insert(entry(0x20, 7, false, 12));
        assert!(tlb.lookup(0x10, 7).is_none());
        assert_eq!(tlb.len(), 2);
        tlb.flush();
        assert!(tlb.is_empty() && tlb.lookup(0x20, 7).is_none());
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
