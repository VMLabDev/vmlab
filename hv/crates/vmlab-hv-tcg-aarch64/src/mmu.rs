//! Address translation (card C8, DESIGN.md R12, A4): the stage-1 walk of the
//! EL1&0 regime with its parameters as data, the permissions, the faults with
//! their status codes, and the softmmu TLB of one CPU.
//!
//! The first half of the file is pure: `walk` takes the control registers as
//! a value and reads descriptors through a function. The granule (4K, 16K or
//! 64K), the size of each half of the address space and the size of a
//! physical address are numbers in `Controls`, so nothing is written for one
//! granule (A4). A regime is a value too (G4): EL1&0 is the one that exists.
//!
//! The second half, `Mmu`, puts the generic TLB of `vmlab-hv-tcg` in front of
//! the walk. An entry is one 4K page of a mapping, tagged with its ASID
//! unless the mapping is global. A fault is never kept.
//!
//! The CPU of phase 3d is Armv8.0: no hardware update of the access flag and
//! no dirty state. A descriptor with the access flag clear takes an access
//! flag fault, and the guest sets the flag. QEMU's `cortex-a53` does the
//! same.
//!
//! Sources read (DESIGN.md "Sources"):
//! - S6: the lockstep against QEMU with the MMU on (`VERIFICATION.md`, card
//!   C8): ESR_EL1, FAR_EL1 and PAR_EL1 are compared after every instruction,
//!   for random translation tables. Where this file says "QEMU", the
//!   behaviour was seen there.
//! - The layout of a translation table descriptor, of TCR_EL1 and of the
//!   fault status codes is general knowledge of the architecture's public
//!   register and descriptor formats. No file of S2 to S5 was opened, and no
//!   text of the Arm manual. QEMU's source was not read (X1), and no Arm
//!   pseudocode (X2).

use crate::state::State;
use crate::sysreg::{self, reg};
use vmlab_hv_tcg::tlb::{Entry, PAGE_BITS, Tlb};
use vmlab_hv_tcg::{Error, Memory};

/// A translation regime (DESIGN.md G4). EL2 and EL2&0 come with a CPU that
/// has EL2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regime {
    /// EL1 and EL0, through TTBR0_EL1 and TTBR1_EL1.
    El10,
}

/// The regimes that a CPU keeps a TLB for.
pub const REGIMES: usize = 1;

impl Regime {
    pub fn index(self) -> usize {
        match self {
            Self::El10 => 0,
        }
    }
}

/// What an access does with its memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    Execute,
}

/// The permissions of a page, and its memory type, as the bits of
/// `Translation::flags` and of a TLB entry.
pub mod flag {
    /// EL1 may read, write, execute.
    pub const PR: u8 = 1;
    pub const PW: u8 = 2;
    pub const PX: u8 = 4;
    /// EL0 may read, write, execute.
    pub const UR: u8 = 8;
    pub const UW: u8 = 16;
    pub const UX: u8 = 32;
    /// Device memory: an access that is not aligned to its size faults.
    pub const DEVICE: u8 = 64;
}

/// The bit of `flags` that an access of `kind` needs. `user`: the access is
/// made as EL0.
#[inline(always)]
pub fn needed(kind: Kind, user: bool) -> u8 {
    let bit = match kind {
        Kind::Read => flag::PR,
        Kind::Write => flag::PW,
        Kind::Execute => flag::PX,
    };
    if user { bit << 3 } else { bit }
}

/// Why a translation or an access fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// An address is wider than the output size.
    AddressSize,
    /// No valid descriptor.
    Translation,
    /// The access flag of the descriptor is clear.
    AccessFlag,
    Permission,
    /// The memory holds nothing where a descriptor is.
    ExternalOnWalk,
    /// The memory holds nothing at the translated address.
    External,
    /// An address that is not a multiple of the size of the access, where
    /// the access asks for that.
    Alignment,
}

/// A fault with the level of the walk that found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub kind: FaultKind,
    pub level: u8,
}

impl Fault {
    pub const EXTERNAL: Self = Self {
        kind: FaultKind::External,
        level: 0,
    };
    pub const ALIGNMENT: Self = Self {
        kind: FaultKind::Alignment,
        level: 0,
    };

    /// The fault status code of ESR_EL1 and PAR_EL1.
    pub fn status(self) -> u8 {
        let level = self.level & 3;
        match self.kind {
            FaultKind::AddressSize => level,
            FaultKind::Translation => 0x04 | level,
            FaultKind::AccessFlag => 0x08 | level,
            FaultKind::Permission => 0x0c | level,
            FaultKind::External => 0x10,
            FaultKind::ExternalOnWalk => 0x14 | level,
            FaultKind::Alignment => 0x21,
        }
    }
}

/// The control registers of a regime as the walk reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controls {
    /// SCTLR.M: stage 1 is on.
    pub enabled: bool,
    /// SCTLR.WXN: what can be written cannot be executed.
    pub wxn: bool,
    /// SCTLR.A: every access that is not aligned to its size faults.
    pub strict_alignment: bool,
    pub tcr: u64,
    pub ttbr: [u64; 2],
    pub mair: u64,
    /// The width of a physical address of the CPU.
    pub pa_bits: u32,
    /// The granules that the CPU has, by their shift: bit 12, 14 and 16.
    pub granules: u32,
}

impl Controls {
    /// The controls of `regime` in `state`.
    pub fn of(state: &State, regime: Regime) -> Self {
        let Regime::El10 = regime;
        let regs = &state.system.regs;
        let features = regs[sysreg::index("ID_AA64MMFR0_EL1")];
        let mut granules = 0;
        // TGran4 and TGran64: 0 says that the CPU has the granule. TGran16:
        // 1 says so.
        if (features >> 28) & 15 == 0 {
            granules |= 1 << 12;
        }
        if (features >> 20) & 15 == 1 {
            granules |= 1 << 14;
        }
        if (features >> 24) & 15 == 0 {
            granules |= 1 << 16;
        }
        let sctlr = regs[reg::SCTLR_EL1];
        Self {
            enabled: sctlr & sysreg::sctlr::M != 0,
            wxn: sctlr & sysreg::sctlr::WXN != 0,
            strict_alignment: sctlr & sysreg::sctlr::A != 0,
            tcr: regs[reg::TCR_EL1],
            ttbr: [regs[reg::TTBR0_EL1], regs[reg::TTBR1_EL1]],
            mair: regs[reg::MAIR_EL1],
            pa_bits: state.physical_address_bits(),
            granules,
        }
    }

    /// TCR.TBI0 and TBI1 as the bits 0 and 1.
    pub fn tbi(&self) -> u8 {
        (self.tcr >> 37) as u8 & 3
    }

    /// The ASID that the non-global entries of now belong to: of TTBR1 when
    /// TCR.A1 is set, else of TTBR0, and eight bits unless TCR.AS is set.
    pub fn asid(&self) -> u16 {
        let ttbr = self.ttbr[usize::from(self.tcr & (1 << 22) != 0)];
        let asid = (ttbr >> 48) as u16;
        if self.tcr & (1 << 36) != 0 {
            asid
        } else {
            asid & 0xff
        }
    }

    /// The width of an output address: TCR.IPS, and at most what the CPU
    /// has.
    pub fn output_bits(&self) -> u32 {
        let ips = [32, 36, 40, 42, 44, 48, 52]
            .get(((self.tcr >> 32) & 7) as usize)
            .copied()
            .unwrap_or(52);
        ips.min(self.pa_bits)
    }

    /// `va` without the bits that the regime ignores: with TCR.TBI of its
    /// half set, the top byte is the bit 55 repeated.
    pub fn untagged(&self, va: u64) -> u64 {
        let upper = va & (1 << 55) != 0;
        if self.tbi() & (1 << u8::from(upper)) == 0 {
            va
        } else if upper {
            va | 0xff << 56
        } else {
            va & !(0xff << 56)
        }
    }

    /// The parameters of the half of the address space that `va` is in.
    fn half(&self, va: u64) -> Half {
        let upper = va & (1 << 55) != 0;
        let fields = if upper { self.tcr >> 16 } else { self.tcr };
        let granule = match ((fields >> 14) & 3, upper) {
            (0, false) | (2, true) => 12,
            (2, false) | (1, true) => 14,
            (1, false) | (3, true) => 16,
            // TG0 3 and TG1 0 are reserved.
            _ => 12,
        };
        Half {
            upper,
            // QEMU takes a size outside 16 to 39 as the nearest of the two.
            tsz: ((fields & 0x3f) as u32).clamp(16, 39),
            granule: self.supported(granule),
            disabled: fields & (1 << 7) != 0,
            table: self.ttbr[usize::from(upper)],
        }
    }

    /// The granule that the walk uses for one that TCR selects. A granule
    /// that the CPU does not have is replaced by one that it has.
    fn supported(&self, granule: u32) -> u32 {
        if self.granules & (1 << granule) != 0 {
            return granule;
        }
        [12, 16, 14]
            .into_iter()
            .find(|other| self.granules & (1 << other) != 0)
            .unwrap_or(12)
    }
}

struct Half {
    upper: bool,
    tsz: u32,
    /// The shift of the granule: 12, 14 or 16.
    granule: u32,
    /// TCR.EPD: no walk through this half.
    disabled: bool,
    table: u64,
}

/// What an address translates to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Translation {
    /// The physical address of the 4K page that holds the address.
    pub page: u64,
    /// `flag` bits.
    pub flags: u8,
    /// Log2 of the size of the mapping: of the page or the block.
    pub span: u8,
    /// The level of the descriptor, for a permission fault.
    pub level: u8,
    /// Not tagged with an ASID.
    pub global: bool,
    /// The byte of MAIR that the descriptor selects.
    pub attributes: u8,
    /// The shareability field of the descriptor.
    pub shareability: u8,
}

const fn fault(kind: FaultKind, level: u8) -> Fault {
    Fault { kind, level }
}

/// The bits 47 to 12 of a descriptor or of a table base.
const ADDRESS: u64 = 0x0000_ffff_ffff_f000;

/// The shape of a walk for a granule of `1 << granule` bytes and an input
/// address of `input` bits: the first level, the count of address bits that
/// the first level resolves, and the count that every later level resolves.
/// A table holds 8-byte descriptors, so a level resolves `granule - 3` bits,
/// and the table of the first level holds as many entries as the input size
/// leaves for it.
pub fn geometry(granule: u32, input: u32) -> (u32, u32, u32) {
    let stride = granule - 3;
    let levels = (input - granule).div_ceil(stride);
    (4 - levels, input - granule - stride * (levels - 1), stride)
}

/// Translate `va`, which is untagged already. `read` gives the eight bytes
/// of a descriptor at a physical address, or `None` where the memory holds
/// nothing. The permissions of the result are not checked against an access.
pub fn walk(
    controls: &Controls,
    va: u64,
    read: &mut dyn FnMut(u64) -> Option<u64>,
) -> Result<Translation, Fault> {
    if !controls.enabled {
        // Stage 1 is off: the address is the physical address, everything is
        // allowed, and data is Device memory. That is why QEMU faults every
        // access that is not aligned while the MMU is off (card C3).
        if va >> controls.pa_bits != 0 {
            return Err(fault(FaultKind::AddressSize, 0));
        }
        return Ok(Translation {
            page: va & !0xfff,
            flags: 0x3f | flag::DEVICE,
            span: PAGE_BITS as u8,
            level: 0,
            global: true,
            attributes: 0,
            shareability: 0,
        });
    }
    let half = controls.half(va);
    let input = 64 - half.tsz;
    // Every bit above the input size is the bit that selects the half.
    let above = va >> input;
    let expected = if half.upper { u64::MAX >> input } else { 0 };
    if above != expected || half.disabled {
        return Err(fault(FaultKind::Translation, 0));
    }
    let granule = half.granule;
    let (start, first_bits, stride) = geometry(granule, input);
    let output = controls.output_bits();
    // The bits of TTBR below the size of the first table are not address
    // bits.
    let mut table = half.table & ADDRESS & !((8u64 << first_bits) - 1);
    if table >> output != 0 {
        return Err(fault(FaultKind::AddressSize, 0));
    }
    // What the table descriptors on the way take away: no access from EL0,
    // no write, no execution at EL0, no execution at EL1.
    let (mut no_user, mut read_only, mut uxn, mut pxn) = (false, false, false, false);
    for level in start..=3 {
        let shift = granule + stride * (3 - level);
        let bits = if level == start { first_bits } else { stride };
        let index = (va >> shift) & ((1 << bits) - 1);
        let level = level as u8;
        let Some(descriptor) = read(table | index << 3) else {
            return Err(fault(FaultKind::ExternalOnWalk, level));
        };
        if descriptor & 1 == 0 {
            return Err(fault(FaultKind::Translation, level));
        }
        let is_table = descriptor & 2 != 0;
        if level < 3 && is_table {
            no_user |= descriptor & (1 << 61) != 0;
            read_only |= descriptor & (1 << 62) != 0;
            uxn |= descriptor & (1 << 60) != 0;
            pxn |= descriptor & (1 << 59) != 0;
            table = descriptor & ADDRESS & !((1u64 << granule) - 1);
            if table >> output != 0 {
                return Err(fault(FaultKind::AddressSize, level));
            }
            continue;
        }
        // A block exists at the levels 1 and 2 of the 4K granule and at the
        // level 2 of the others. At level 3 the bit 1 says "page".
        let block_level = level == 2 || (level == 1 && granule == 12);
        if (level == 3 && !is_table) || (level < 3 && !block_level) {
            return Err(fault(FaultKind::Translation, level));
        }
        let base = descriptor & ADDRESS & !((1u64 << shift) - 1);
        if base >> output != 0 {
            return Err(fault(FaultKind::AddressSize, level));
        }
        if descriptor & (1 << 10) == 0 {
            return Err(fault(FaultKind::AccessFlag, level));
        }
        // AP[2:1] of the descriptor: bit 6 opens EL0, bit 7 takes writes
        // away.
        let user = descriptor & (1 << 6) != 0 && !no_user;
        let writable = descriptor & (1 << 7) == 0 && !read_only;
        uxn |= descriptor & (1 << 54) != 0;
        pxn |= descriptor & (1 << 53) != 0;
        let user_write = user && writable;
        let mut flags = flag::PR;
        if writable {
            flags |= flag::PW;
        }
        if user {
            flags |= flag::UR;
        }
        if user_write {
            flags |= flag::UW;
        }
        // What EL0 can write, EL1 does not execute. With WXN, what a level
        // can write it does not execute.
        if !pxn && !user_write && !(controls.wxn && writable) {
            flags |= flag::PX;
        }
        if !uxn && !(controls.wxn && user_write) {
            flags |= flag::UX;
        }
        let attributes = (controls.mair >> (8 * ((descriptor >> 2) & 7))) as u8;
        if attributes & 0xf0 == 0 {
            flags |= flag::DEVICE;
        }
        let within = va & ((1u64 << shift) - 1);
        return Ok(Translation {
            page: (base | within) & !0xfff,
            flags,
            span: shift as u8,
            level,
            global: descriptor & (1 << 11) == 0,
            attributes,
            shareability: (descriptor >> 8) as u8 & 3,
        });
    }
    // The loop returns at level 3 at the latest.
    Err(fault(FaultKind::Translation, 3))
}

/// PAR_EL1 after an address translation instruction that succeeded.
pub fn par_of(translation: &Translation) -> u64 {
    // QEMU reports Device memory and memory that is not cached as outer
    // shareable.
    let attributes = translation.attributes;
    let shareability = if attributes & 0xf0 == 0 || attributes == 0x44 {
        2
    } else {
        u64::from(translation.shareability)
    };
    (translation.page & ADDRESS)
        | 1 << 11
        | shareability << 7
        | u64::from(attributes) << 56
}

/// PAR_EL1 after an address translation instruction that failed.
pub fn par_fault(fault: Fault) -> u64 {
    1 | u64::from(fault.status() & 0x3f) << 1 | 1 << 11
}

/// Why `Mmu::translate` gives no address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Miss {
    Fault(Fault),
    /// An error of the host, not of the guest.
    Host(String),
}

impl From<Fault> for Miss {
    fn from(fault: Fault) -> Self {
        Self::Fault(fault)
    }
}

/// Log2 of the entries of the TLB of a regime.
const TLB_BITS: u32 = 10;

/// The bits of a page number that an invalidation by address names: the
/// address bits 55 to 12.
const TLBI_PAGE_BITS: u64 = (1 << 44) - 1;

/// The translation state of one CPU: the TLB of each regime, and the
/// controls that its entries were made under.
pub struct Mmu {
    tlbs: [Tlb; REGIMES],
    controls: Controls,
    asid: u16,
}

impl Mmu {
    pub fn new(state: &State) -> Self {
        let controls = Controls::of(state, Regime::El10);
        Self {
            tlbs: [Tlb::new(TLB_BITS)],
            controls,
            asid: controls.asid(),
        }
    }

    pub fn controls(&self) -> &Controls {
        &self.controls
    }

    /// Take the controls of `state`. A change of anything but the tables
    /// drops every entry: the entries hold what SCTLR, TCR and MAIR said
    /// when they were made. A new table base or a new ASID drops nothing,
    /// as on a CPU: the entries are tagged, and the guest invalidates what
    /// it changes.
    #[inline]
    pub fn sync(&mut self, state: &State) {
        let regs = &state.system.regs;
        let old = &self.controls;
        if regs[reg::TTBR0_EL1] == old.ttbr[0]
            && regs[reg::TTBR1_EL1] == old.ttbr[1]
            && regs[reg::TCR_EL1] == old.tcr
            && regs[reg::MAIR_EL1] == old.mair
            && regs[reg::SCTLR_EL1] & sysreg::sctlr::TRANSLATION == self.sctlr_bits()
            && state.physical_address_bits() == old.pa_bits
        {
            return;
        }
        let new = Controls::of(state, Regime::El10);
        let tables_only = Controls {
            ttbr: new.ttbr,
            ..self.controls
        } == new;
        self.controls = new;
        self.asid = new.asid();
        if !tables_only {
            self.flush();
        }
    }

    fn sctlr_bits(&self) -> u64 {
        let controls = &self.controls;
        (if controls.enabled { sysreg::sctlr::M } else { 0 })
            | (if controls.wxn { sysreg::sctlr::WXN } else { 0 })
            | (if controls.strict_alignment {
                sysreg::sctlr::A
            } else {
                0
            })
    }

    /// The physical address of `va` when the TLB holds its page with one of
    /// the bits `need`, and the access stays inside the page.
    #[inline(always)]
    pub fn hit(&self, va: u64, bytes: u64, need: u8) -> Option<(u64, u8)> {
        let entry = self.tlbs[0].lookup(va >> PAGE_BITS, self.asid)?;
        let offset = va & 0xfff;
        (entry.flags as u8 & need != 0 && offset + bytes <= 0x1000)
            .then_some((entry.phys | offset, entry.flags as u8))
    }

    /// The translation of the page of `va`, from the TLB or from a walk,
    /// without a permission check. A walk that succeeds fills the TLB.
    pub fn page<M: Memory>(&mut self, memory: &mut M, va: u64) -> Result<Translation, Miss> {
        if let Some(entry) = self.tlbs[0].lookup(va >> PAGE_BITS, self.asid) {
            // The entry keeps what an access needs, not the attributes of an
            // address translation instruction: those walk again.
            return Ok(Translation {
                page: entry.phys,
                flags: entry.flags as u8,
                span: entry.span,
                level: (entry.flags >> 8) as u8,
                global: entry.global,
                attributes: 0,
                shareability: 0,
            });
        }
        let translation = self.walk(memory, va)?;
        self.tlbs[0].insert(Entry {
            page: va >> PAGE_BITS,
            phys: translation.page,
            asid: self.asid,
            span: translation.span,
            // The level of the descriptor rides above the permissions.
            flags: u16::from(translation.flags) | u16::from(translation.level) << 8,
            global: translation.global,
        });
        Ok(translation)
    }

    /// Walk the tables for `va` and keep nothing: the descriptors that the
    /// walk read, as physical address and value, and the physical address
    /// of `va` when it has one.
    pub fn trace<M: Memory>(&self, memory: &mut M, va: u64) -> (Vec<(u64, u64)>, Option<u64>) {
        let mut seen = Vec::new();
        let mut read = |address: u64| {
            let mut bytes = [0u8; 8];
            memory.read(address, &mut bytes).ok()?;
            let value = u64::from_le_bytes(bytes);
            seen.push((address, value));
            Some(value)
        };
        let page = walk(&self.controls, self.controls.untagged(va), &mut read);
        (seen, page.ok().map(|page| page.page | (va & 0xfff)))
    }

    /// Walk the tables for `va` and keep nothing.
    pub fn walk<M: Memory>(&self, memory: &mut M, va: u64) -> Result<Translation, Miss> {
        let mut host = None;
        let mut read = |address: u64| {
            let mut bytes = [0u8; 8];
            match memory.read(address, &mut bytes) {
                Ok(()) => Some(u64::from_le_bytes(bytes)),
                Err(Error::Memory(_)) => None,
                Err(error) => {
                    host = Some(error.to_string());
                    None
                }
            }
        };
        let result = walk(&self.controls, self.controls.untagged(va), &mut read);
        match host {
            Some(message) => Err(Miss::Host(message)),
            None => result.map_err(Miss::Fault),
        }
    }

    /// The physical address of `va` for an access of `kind`, made as EL0
    /// when `user`.
    pub fn translate<M: Memory>(
        &mut self,
        memory: &mut M,
        va: u64,
        kind: Kind,
        user: bool,
    ) -> Result<(u64, u8), Miss> {
        let translation = self.page(memory, va)?;
        if translation.flags & needed(kind, user) == 0 {
            return Err(Miss::Fault(fault(FaultKind::Permission, translation.level)));
        }
        Ok((translation.page | (va & 0xfff), translation.flags))
    }

    /// Drop every entry of every regime.
    pub fn flush(&mut self) {
        for tlb in &mut self.tlbs {
            tlb.flush();
        }
    }

    /// Drop the entries of the mapping that holds the address of a TLBI
    /// argument: the address bits 55 to 12 in its bits 43 to 0. `asid`: only
    /// the global entries and those of this ASID.
    pub fn flush_page(&mut self, regime: Regime, argument: u64, asid: Option<u16>) {
        let asid = asid.map(|asid| self.asid_of(asid));
        self.tlbs[regime.index()].flush_page(argument, TLBI_PAGE_BITS, asid);
    }

    /// Drop the entries of an ASID that are not global.
    pub fn flush_asid(&mut self, regime: Regime, asid: u16) {
        let asid = self.asid_of(asid);
        self.tlbs[regime.index()].flush_asid(asid);
    }

    /// An ASID of a TLBI argument as the entries hold it: eight bits unless
    /// TCR.AS is set.
    fn asid_of(&self, asid: u16) -> u16 {
        if self.controls.tcr & (1 << 36) != 0 {
            asid
        } else {
            asid & 0xff
        }
    }

    /// The count of entries that the TLBs hold.
    pub fn entries(&self) -> usize {
        self.tlbs.iter().map(Tlb::len).sum()
    }
}
