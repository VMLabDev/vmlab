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

    /// TBI0 and TBI1 as they hold for an instruction address: without the
    /// halves that TCR.TBID0 and TBID1 (the bits 51 and 52) take out. Those
    /// two bits belong to pointer authentication, which this CPU does not
    /// have, and QEMU's `cortex-a53` acts on them all the same: the lockstep
    /// found a branch that kept its tag. We follow QEMU (DESIGN.md P2, U1).
    pub fn tbi_of_instructions(&self) -> u8 {
        tbi_of_instructions(self.tcr)
    }

    /// `va` without the bits that the regime ignores: with TCR.TBI of its
    /// half set, the top byte is the bit 55 repeated. `execute`: `va` is an
    /// instruction address.
    pub fn untagged(&self, va: u64, execute: bool) -> u64 {
        let upper = va & (1 << 55) != 0;
        let tbi = if execute {
            self.tbi_of_instructions()
        } else {
            self.tbi()
        };
        if tbi & (1 << u8::from(upper)) == 0 {
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
            tsz: (fields & 0x3f) as u32,
            granule: self.supported(granule),
            disabled: fields & (1 << 7) != 0,
            // TCR.HPD0 and HPD1, the bits 41 and 42, belong to Armv8.1, and
            // QEMU's `cortex-a53` acts on them all the same (the lockstep
            // of card C8). We follow QEMU (DESIGN.md P2, U1).
            flat: self.tcr & (1 << (41 + u32::from(upper))) != 0,
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

/// TBI0 and TBI1 of a TCR as they hold for an instruction address.
pub fn tbi_of_instructions(tcr: u64) -> u8 {
    (tcr >> 37) as u8 & 3 & !((tcr >> 51) as u8 & 3)
}

struct Half {
    upper: bool,
    tsz: u32,
    /// The shift of the granule: 12, 14 or 16.
    granule: u32,
    /// TCR.EPD: no walk through this half.
    disabled: bool,
    /// TCR.HPD: a table descriptor takes no permission away.
    flat: bool,
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
    // A size of a half outside 16 to 39 names no address space of Armv8.0:
    // QEMU translates nothing through such a half.
    if !(16..=39).contains(&half.tsz) {
        return Err(fault(FaultKind::Translation, 0));
    }
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
    // bits. A first table that is smaller than 4K has address bits below
    // bit 12.
    let mut table = half.table & 0x0000_ffff_ffff_ffff & !((8u64 << first_bits) - 1);
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
            if !half.flat {
                no_user |= descriptor & (1 << 61) != 0;
                read_only |= descriptor & (1 << 62) != 0;
                uxn |= descriptor & (1 << 60) != 0;
                pxn |= descriptor & (1 << 59) != 0;
            }
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
    // shareable. The byte 0x40 is no memory type of Armv8.0, and QEMU
    // takes it as not cached too.
    let attributes = translation.attributes;
    let shareability = if attributes & 0xf0 == 0 || attributes == 0x44 || attributes == 0x40 {
        2
    } else {
        u64::from(translation.shareability)
    };
    // Bit 11 reads as one, and bit 9 says "not secure": this CPU has no
    // secure state, and QEMU sets it.
    (translation.page & ADDRESS)
        | 1 << 11
        | 1 << 9
        | shareability << 7
        | u64::from(attributes) << 56
}

/// PAR_EL1 after an address translation instruction that failed.
pub fn par_fault(fault: Fault) -> u64 {
    1 | u64::from(fault.status() & 0x3f) << 1 | 1 << 11
}

/// Whether the top byte of `va` is its bit 55 repeated.
#[inline(always)]
pub fn canonical(va: u64) -> bool {
    ((va as i64) << 8 >> 8) as u64 == va
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
    /// Changes of what a virtual address translates to: every flush and
    /// every new control. What keeps a translation beside the TLB (the jump
    /// cache of the JIT, 3e DESIGN.md H6) is dropped when this moves.
    changes: u64,
}

impl Mmu {
    pub fn new(state: &State) -> Self {
        let controls = Controls::of(state, Regime::El10);
        Self {
            tlbs: [Tlb::new(TLB_BITS)],
            controls,
            asid: controls.asid(),
            changes: 0,
        }
    }

    /// The count of changes of the translation (`changes`).
    pub fn changes(&self) -> u64 {
        self.changes
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
        self.changes += 1;
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
        (if controls.enabled {
            sysreg::sctlr::M
        } else {
            0
        }) | (if controls.wxn { sysreg::sctlr::WXN } else { 0 })
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
    /// `kind` says whether `va` is an instruction address.
    pub fn page<M: Memory>(
        &mut self,
        memory: &mut M,
        va: u64,
        kind: Kind,
    ) -> Result<Translation, Miss> {
        // An entry is kept under the address with its tag. An instruction
        // address may keep a tag that a data address loses: then the entry
        // of the data address is not its translation.
        let execute = kind == Kind::Execute;
        let tagged = execute && !canonical(va) && self.controls.untagged(va, true) == va;
        if !tagged && let Some(entry) = self.tlbs[0].lookup(va >> PAGE_BITS, self.asid) {
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
        let translation = self.walk(memory, va, execute)?;
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
        let page = walk(&self.controls, self.controls.untagged(va, false), &mut read);
        (seen, page.ok().map(|page| page.page | (va & 0xfff)))
    }

    /// Walk the tables for `va` and keep nothing. `execute`: `va` is an
    /// instruction address.
    pub fn walk<M: Memory>(
        &self,
        memory: &mut M,
        va: u64,
        execute: bool,
    ) -> Result<Translation, Miss> {
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
        let result = walk(
            &self.controls,
            self.controls.untagged(va, execute),
            &mut read,
        );
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
        let translation = self.page(memory, va, kind)?;
        if translation.flags & needed(kind, user) == 0 {
            return Err(Miss::Fault(fault(FaultKind::Permission, translation.level)));
        }
        Ok((translation.page | (va & 0xfff), translation.flags))
    }

    /// Drop every entry of every regime.
    pub fn flush(&mut self) {
        self.changes += 1;
        for tlb in &mut self.tlbs {
            tlb.flush();
        }
    }

    /// Drop the entries of the mapping that holds the address of a TLBI
    /// argument: the address bits 55 to 12 in its bits 43 to 0. `asid`: only
    /// the global entries and those of this ASID.
    pub fn flush_page(&mut self, regime: Regime, argument: u64, asid: Option<u16>) {
        let asid = asid.map(|asid| self.asid_of(asid));
        self.changes += 1;
        self.tlbs[regime.index()].flush_page(argument, TLBI_PAGE_BITS, asid);
    }

    /// Drop the entries of an ASID that are not global.
    pub fn flush_asid(&mut self, regime: Regime, asid: u16) {
        let asid = self.asid_of(asid);
        self.changes += 1;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use vmlab_hv_tcg::FlatMemory;

    /// Translation tables in a map from physical address to descriptor.
    struct Tables {
        words: HashMap<u64, u64>,
        next: u64,
    }

    /// The descriptor bits of the tests: valid, a page at level 3, the
    /// access flag.
    const VALID: u64 = 1;
    const TABLE: u64 = 3;
    const AF: u64 = 1 << 10;
    const USER: u64 = 1 << 6;
    const READ_ONLY: u64 = 1 << 7;
    const NOT_GLOBAL: u64 = 1 << 11;
    const PXN: u64 = 1 << 53;
    const UXN: u64 = 1 << 54;

    impl Tables {
        fn new() -> Self {
            Self {
                words: HashMap::new(),
                next: 0x4000_0000,
            }
        }

        fn table(&mut self) -> u64 {
            let at = self.next;
            self.next += 0x1_0000;
            at
        }

        /// Put `descriptor` where a walk from `root` for `va` reads at
        /// `leaf`, with the tables on the way, and give the addresses of
        /// the descriptors of the path.
        fn map(
            &mut self,
            root: u64,
            granule: u32,
            input: u32,
            va: u64,
            leaf: u32,
            descriptor: u64,
        ) -> Vec<u64> {
            let (start, first_bits, stride) = geometry(granule, input);
            let va = va & ((1u64 << input) - 1);
            let mut table = root;
            let mut path = Vec::new();
            for level in start..=leaf {
                let shift = granule + stride * (3 - level);
                let bits = if level == start { first_bits } else { stride };
                let entry = table + ((va >> shift) & ((1 << bits) - 1)) * 8;
                path.push(entry);
                if level == leaf {
                    self.words.insert(entry, descriptor);
                    break;
                }
                table = match self.words.get(&entry) {
                    Some(found) => found & ADDRESS & !((1u64 << granule) - 1),
                    None => {
                        let next = self.table();
                        self.words.insert(entry, next | TABLE);
                        next
                    }
                };
            }
            path
        }

        fn walk(&self, controls: &Controls, va: u64) -> Result<Translation, Fault> {
            walk(controls, va, &mut |address| {
                Some(self.words.get(&address).copied().unwrap_or(0))
            })
        }
    }

    /// Controls with both halves of `tsz` and of the granule `granule`.
    fn controls(granule: u32, tsz: u64, ttbr: [u64; 2]) -> Controls {
        let (tg0, tg1) = match granule {
            12 => (0, 2),
            14 => (2, 1),
            _ => (1, 3),
        };
        Controls {
            enabled: true,
            wxn: false,
            strict_alignment: false,
            // IPS 5: 48 bits.
            tcr: tsz | tg0 << 14 | tsz << 16 | tg1 << 30 | 5 << 32,
            ttbr,
            mair: 0x0000_0000_0000_04ff,
            pa_bits: 48,
            granules: 1 << 12 | 1 << 14 | 1 << 16,
        }
    }

    fn fault_of(kind: FaultKind, level: u8) -> Result<Translation, Fault> {
        Err(Fault { kind, level })
    }

    #[test]
    fn the_shape_of_a_walk_follows_from_the_granule_and_the_input_size() {
        // First level, bits of the first level, bits of the others.
        assert_eq!(geometry(12, 48), (0, 9, 9));
        assert_eq!(geometry(12, 39), (1, 9, 9));
        assert_eq!(geometry(12, 31), (1, 1, 9));
        assert_eq!(geometry(12, 25), (2, 4, 9));
        assert_eq!(geometry(14, 48), (0, 1, 11));
        assert_eq!(geometry(14, 47), (1, 11, 11));
        assert_eq!(geometry(14, 36), (2, 11, 11));
        assert_eq!(geometry(14, 25), (3, 11, 11));
        assert_eq!(geometry(16, 48), (1, 6, 13));
        assert_eq!(geometry(16, 42), (2, 13, 13));
        assert_eq!(geometry(16, 29), (3, 13, 13));
        assert_eq!(geometry(16, 25), (3, 9, 13));
    }

    #[test]
    fn a_page_and_a_block_of_each_granule_translate() {
        // Granule, the level of a block, the bytes of a block.
        for (granule, block_level, block) in [
            (12u32, 2u32, 1u64 << 21),
            (14, 2, 1 << 25),
            (16, 2, 1 << 29),
        ] {
            for tsz in [16u64, 20, 25, 30] {
                let input = 64 - tsz as u32;
                let mut tables = Tables::new();
                let roots = [tables.table(), tables.table()];
                let controls = controls(granule, tsz, roots);
                let page = 1u64 << granule;
                // A page in the lower half and one in the upper half.
                let low = 3 * block + 5 * page;
                let high = (u64::MAX << input) + 2 * block + 7 * page;
                tables.map(roots[0], granule, input, low, 3, 0x8000_0000 | TABLE | AF);
                tables.map(
                    roots[1],
                    granule,
                    input,
                    high,
                    3,
                    0x9000_0000 | TABLE | AF | NOT_GLOBAL,
                );
                // An address in the upper half of the page: the 4K page of
                // the result is the one that holds it.
                let offset = page / 2 + 0x234;
                let found = tables.walk(&controls, low + offset).unwrap();
                assert_eq!(
                    found.page,
                    0x8000_0000 + (offset & !0xfff),
                    "{granule} {tsz}"
                );
                assert_eq!(
                    (found.span, found.level, found.global),
                    (granule as u8, 3, true)
                );
                let found = tables.walk(&controls, high + page - 1).unwrap();
                assert_eq!(found.page, 0x9000_0000 + ((page - 1) & !0xfff));
                assert!(!found.global);
                // A block: the address bits below its size come from the
                // virtual address, whatever the descriptor holds there.
                let at = 5 * block;
                if at >> input == 0 {
                    tables.map(
                        roots[0],
                        granule,
                        input,
                        at,
                        block_level,
                        0x40_0000_0000 | 0x1f_f000 | VALID | AF,
                    );
                    let found = tables.walk(&controls, at + 0x12_3456).unwrap();
                    assert_eq!(found.page, 0x40_0000_0000 + 0x12_3000);
                    assert_eq!(
                        (u64::from(found.span), found.level),
                        (u64::from(block.trailing_zeros()), 2)
                    );
                }
                // An address between the two halves is in neither.
                assert_eq!(
                    tables.walk(&controls, 1 << input),
                    fault_of(FaultKind::Translation, 0)
                );
                assert_eq!(
                    tables.walk(&controls, !(1 << input)),
                    fault_of(FaultKind::Translation, 0)
                );
            }
        }
        // The 4K granule has a block of 1 GiB at level 1 too.
        let mut tables = Tables::new();
        let roots = [tables.table(), tables.table()];
        let controls = controls(12, 25, roots);
        tables.map(roots[0], 12, 39, 3 << 30, 1, 0x80_0000_0000 | VALID | AF);
        let found = tables.walk(&controls, (3 << 30) + 0x1234_5678).unwrap();
        assert_eq!(
            (found.page, found.span, found.level),
            (0x80_1234_5000, 30, 1)
        );
    }

    #[test]
    fn each_fault_names_its_level() {
        let mut tables = Tables::new();
        let roots = [tables.table(), tables.table()];
        let mut controls = controls(12, 16, roots);
        let va = 0x7f_1234_5000u64;
        let path = tables.map(roots[0], 12, 48, va, 3, 0x8000_0000 | TABLE | AF);
        assert_eq!(path.len(), 4);
        assert!(tables.walk(&controls, va).is_ok());
        let saved: Vec<u64> = path.iter().map(|entry| tables.words[entry]).collect();
        for (level, entry) in path.iter().enumerate() {
            let level = level as u8;
            // No valid descriptor.
            tables.words.insert(*entry, saved[level as usize] & !VALID);
            assert_eq!(
                tables.walk(&controls, va),
                fault_of(FaultKind::Translation, level)
            );
            // An address above the output size, in a table descriptor and
            // in the page descriptor.
            tables.words.insert(*entry, saved[level as usize] | 1 << 47);
            controls.tcr = controls.tcr & !(7 << 32) | 2 << 32;
            assert_eq!(
                tables.walk(&controls, va),
                fault_of(FaultKind::AddressSize, level)
            );
            controls.tcr = controls.tcr & !(7 << 32) | 5 << 32;
            tables.words.insert(*entry, saved[level as usize]);
        }
        // The descriptor of level 3 with the bit 1 clear is no page, and a
        // block at level 0 is no block.
        tables.words.insert(path[3], saved[3] & !2);
        assert_eq!(
            tables.walk(&controls, va),
            fault_of(FaultKind::Translation, 3)
        );
        tables.words.insert(path[3], saved[3] & !AF);
        assert_eq!(
            tables.walk(&controls, va),
            fault_of(FaultKind::AccessFlag, 3)
        );
        tables.words.insert(path[3], saved[3]);
        tables.words.insert(path[0], 0x8000_0000 | VALID | AF);
        assert_eq!(
            tables.walk(&controls, va),
            fault_of(FaultKind::Translation, 0)
        );
        tables.words.insert(path[0], saved[0]);
        // A table base above the output size is a fault of level 0.
        controls.ttbr[0] |= 1 << 40;
        controls.tcr = controls.tcr & !(7 << 32) | 2 << 32;
        assert_eq!(
            tables.walk(&controls, va),
            fault_of(FaultKind::AddressSize, 0)
        );
        controls.ttbr[0] = roots[0];
        // A half that TCR.EPD closes, and a size that Armv8.0 does not have.
        controls.tcr |= 1 << 7;
        assert_eq!(
            tables.walk(&controls, va),
            fault_of(FaultKind::Translation, 0)
        );
        controls.tcr &= !(1 << 7);
        for tsz in [0u64, 15, 40, 63] {
            let mut odd = controls;
            odd.tcr = odd.tcr & !0x3f | tsz;
            assert_eq!(
                tables.walk(&odd, 0x1000),
                fault_of(FaultKind::Translation, 0),
                "{tsz}"
            );
        }
        // Memory that holds no descriptor.
        let result = walk(&controls, va, &mut |address| {
            (address != path[2]).then(|| tables.words.get(&address).copied().unwrap_or(0))
        });
        assert_eq!(result, fault_of(FaultKind::ExternalOnWalk, 2));
    }

    #[test]
    fn the_status_codes_are_those_of_the_syndrome() {
        let status = |kind, level| Fault { kind, level }.status();
        assert_eq!(status(FaultKind::AddressSize, 0), 0x00);
        assert_eq!(status(FaultKind::AddressSize, 3), 0x03);
        assert_eq!(status(FaultKind::Translation, 2), 0x06);
        assert_eq!(status(FaultKind::AccessFlag, 3), 0x0b);
        assert_eq!(status(FaultKind::Permission, 1), 0x0d);
        assert_eq!(status(FaultKind::ExternalOnWalk, 3), 0x17);
        assert_eq!(Fault::EXTERNAL.status(), 0x10);
        assert_eq!(Fault::ALIGNMENT.status(), 0x21);
    }

    #[test]
    fn the_permissions_follow_the_descriptor_the_tables_and_wxn() {
        let mut tables = Tables::new();
        let roots = [tables.table(), tables.table()];
        let mut controls = controls(12, 16, roots);
        let va = 0x1234_5000u64;
        let path = tables.map(roots[0], 12, 48, va, 3, 0);
        let flags = |tables: &mut Tables, controls: &Controls, leaf: u64, table: u64| {
            tables
                .words
                .insert(path[3], 0x8000_0000 | TABLE | AF | leaf);
            let old = tables.words[&path[1]];
            tables
                .words
                .insert(path[1], old & 0x07ff_ffff_ffff_ffff | table);
            tables.walk(controls, va).unwrap().flags & 0x3f
        };
        use flag::{PR, PW, PX, UR, UW, UX};
        // AP: EL1 only, both, EL1 read-only, both read-only.
        assert_eq!(flags(&mut tables, &controls, 0, 0), PR | PW | PX | UX);
        // What EL0 can write, EL1 does not execute.
        assert_eq!(
            flags(&mut tables, &controls, USER, 0),
            PR | PW | UR | UW | UX
        );
        assert_eq!(flags(&mut tables, &controls, READ_ONLY, 0), PR | PX | UX);
        assert_eq!(
            flags(&mut tables, &controls, USER | READ_ONLY, 0),
            PR | PX | UR | UX
        );
        // UXN and PXN.
        assert_eq!(
            flags(&mut tables, &controls, USER | READ_ONLY | UXN, 0),
            PR | PX | UR
        );
        assert_eq!(
            flags(&mut tables, &controls, USER | READ_ONLY | PXN, 0),
            PR | UR | UX
        );
        // A table descriptor takes away: EL0 (bit 61), writes (62),
        // execution at EL0 (60) and at EL1 (59).
        assert_eq!(
            flags(&mut tables, &controls, USER, 1 << 61),
            PR | PW | PX | UX
        );
        assert_eq!(
            flags(&mut tables, &controls, USER, 1 << 62),
            PR | PX | UR | UX
        );
        assert_eq!(
            flags(&mut tables, &controls, USER | READ_ONLY, 1 << 60),
            PR | PX | UR
        );
        assert_eq!(
            flags(&mut tables, &controls, USER | READ_ONLY, 1 << 59),
            PR | UR | UX
        );
        // With TCR.HPD0 it takes nothing away, as on QEMU.
        controls.tcr |= 1 << 41;
        assert_eq!(
            flags(&mut tables, &controls, USER, 0xf << 59),
            PR | PW | UR | UW | UX
        );
        controls.tcr &= !(1 << 41);
        // WXN: what a level can write, it does not execute.
        controls.wxn = true;
        assert_eq!(flags(&mut tables, &controls, 0, 0), PR | PW | UX);
        assert_eq!(flags(&mut tables, &controls, USER, 0), PR | PW | UR | UW);
        assert_eq!(
            flags(&mut tables, &controls, USER | READ_ONLY, 0),
            PR | PX | UR | UX
        );
        controls.wxn = false;
        // The memory type comes from the byte of MAIR that the descriptor
        // selects: the index 1 is Device memory here.
        tables
            .words
            .insert(path[3], 0x8000_0000 | TABLE | AF | 1 << 2 | 3 << 8);
        let found = tables.walk(&controls, va).unwrap();
        assert_ne!(found.flags & flag::DEVICE, 0);
        assert_eq!((found.attributes, found.shareability), (0x04, 3));
        // What an access needs.
        assert_eq!(needed(Kind::Read, false), PR);
        assert_eq!(needed(Kind::Write, true), UW);
        assert_eq!(needed(Kind::Execute, true), UX);
    }

    #[test]
    fn the_controls_give_the_asid_the_tags_and_the_sizes() {
        let mut controls = controls(12, 16, [0xab12 << 48, 0xcd34 << 48]);
        // Eight bits of TTBR0 until TCR.AS and TCR.A1 say otherwise.
        assert_eq!(controls.asid(), 0x12);
        controls.tcr |= 1 << 36;
        assert_eq!(controls.asid(), 0xab12);
        controls.tcr |= 1 << 22;
        assert_eq!(controls.asid(), 0xcd34);
        // TBI0: the top byte of a lower address is ignored.
        let low = 0x5a00_1234_5678_9000u64;
        let high = 0x5aff_1234_5678_9000u64;
        assert_eq!(controls.untagged(low, false), low);
        controls.tcr |= 1 << 37;
        assert_eq!(controls.untagged(low, false), 0x0000_1234_5678_9000);
        assert_eq!(controls.untagged(high, false), high);
        controls.tcr |= 1 << 38;
        assert_eq!(controls.untagged(high, false), 0xffff_1234_5678_9000);
        assert_eq!((controls.tbi(), controls.tbi_of_instructions()), (3, 3));
        // TBID0, as QEMU has it: not for an instruction address.
        controls.tcr |= 1 << 51;
        assert_eq!(controls.tbi_of_instructions(), 2);
        assert_eq!(controls.untagged(low, true), low);
        assert_eq!(controls.untagged(low, false), 0x0000_1234_5678_9000);
        assert!(canonical(0xffff_8000_0000_0000) && canonical(0x7f_ffff_ffff_ffff));
        assert!(!canonical(low) && !canonical(0x00ff_0000_0000_0000));
        // The output size: IPS, and at most what the CPU has.
        for (ips, bits) in [
            (0u64, 32),
            (1, 36),
            (2, 40),
            (3, 42),
            (4, 44),
            (5, 48),
            (6, 48),
            (7, 48),
        ] {
            controls.tcr = controls.tcr & !(7 << 32) | ips << 32;
            assert_eq!(controls.output_bits(), bits);
        }
        // A granule that the CPU does not have is replaced.
        controls.granules = 1 << 12;
        assert_eq!(
            (
                controls.supported(12),
                controls.supported(16),
                controls.supported(14)
            ),
            (12, 12, 12)
        );
        controls.granules = 1 << 14 | 1 << 16;
        assert_eq!((controls.supported(12), controls.supported(14)), (16, 14));
    }

    #[test]
    fn without_stage_1_an_address_is_its_physical_address_in_device_memory() {
        let mut controls = controls(12, 16, [0, 0]);
        controls.enabled = false;
        controls.pa_bits = 40;
        let found = walk(&controls, 0x12_3456_7abc, &mut |_| None).unwrap();
        assert_eq!(found.page, 0x12_3456_7000);
        assert_eq!(found.flags, 0x3f | flag::DEVICE);
        assert!(found.global);
        assert_eq!(
            walk(&controls, 1 << 40, &mut |_| None),
            fault_of(FaultKind::AddressSize, 0)
        );
    }

    #[test]
    fn par_holds_the_address_and_the_attributes_or_the_fault() {
        let page = Translation {
            page: 0x40_1234_5000,
            flags: 0x3f,
            span: 12,
            level: 3,
            global: true,
            attributes: 0xff,
            shareability: 3,
        };
        assert_eq!(par_of(&page), 0xff00_0040_1234_5b80);
        // Device memory, and memory that is not cached, read as outer
        // shareable.
        assert_eq!(
            par_of(&Translation {
                attributes: 0x04,
                ..page
            }),
            0x0400_0040_1234_5b00
        );
        assert_eq!(
            par_of(&Translation {
                attributes: 0x44,
                ..page
            }),
            0x4400_0040_1234_5b00
        );
        assert_eq!(
            par_fault(Fault {
                kind: FaultKind::Translation,
                level: 2
            }),
            0x80d
        );
        assert_eq!(
            par_fault(Fault {
                kind: FaultKind::Permission,
                level: 3
            }),
            0x81f
        );
    }

    /// A memory with one page mapped at `va` through tables of the 4K
    /// granule, and the state that names them.
    fn mapped(va: u64, leaf: u64) -> (FlatMemory, State, Vec<u64>) {
        const BASE: u64 = 0x4000_0000;
        let mut tables = Tables::new();
        let roots = [tables.table(), tables.table()];
        let path = tables.map(roots[0], 12, 48, va, 3, leaf);
        let mut memory = FlatMemory::new(BASE, vec![0u8; 0x10_0000]);
        for (address, value) in &tables.words {
            memory.write(*address, &value.to_le_bytes()).unwrap();
        }
        let mut state = State::reset(BASE);
        state.system.regs[reg::TTBR0_EL1] = roots[0] | 7 << 48;
        state.system.regs[reg::TTBR1_EL1] = roots[1];
        state.system.regs[reg::TCR_EL1] = 16 | 16 << 16 | 2 << 30 | 2 << 32;
        state.system.regs[reg::MAIR_EL1] = 0xff;
        state.system.regs[reg::SCTLR_EL1] |= sysreg::sctlr::M;
        (memory, state, path)
    }

    #[test]
    fn the_tlb_keeps_a_translation_until_it_is_invalidated() {
        let va = 0x7f_0000_3000u64;
        let leaf = 0x4008_0000 | TABLE | AF | NOT_GLOBAL;
        let (mut memory, mut state, path) = mapped(va, leaf);
        let mut mmu = Mmu::new(&state);
        assert_eq!(mmu.entries(), 0);
        assert_eq!(mmu.hit(va + 8, 8, flag::PR), None);
        assert_eq!(
            mmu.translate(&mut memory, va + 8, Kind::Read, false),
            Ok((0x4008_0008, flag::PR | flag::PW | flag::PX | flag::UX))
        );
        assert_eq!(mmu.entries(), 1);
        assert!(mmu.hit(va + 8, 8, flag::PR).is_some());
        // An access that leaves the page, or lacks the permission, is no hit.
        assert_eq!(mmu.hit(va + 0xffc, 8, flag::PR), None);
        assert_eq!(mmu.hit(va, 8, flag::UR), None);
        assert_eq!(
            mmu.translate(&mut memory, va, Kind::Read, true),
            Err(Miss::Fault(Fault {
                kind: FaultKind::Permission,
                level: 3
            }))
        );
        // The descriptor changes, and the TLB still holds the old one.
        memory
            .write(
                path[3],
                &(0x4009_0000u64 | TABLE | AF | NOT_GLOBAL).to_le_bytes(),
            )
            .unwrap();
        assert_eq!(
            mmu.translate(&mut memory, va, Kind::Read, false).unwrap().0,
            0x4008_0000
        );
        // A TLBI of another ASID leaves an entry that is not global.
        mmu.flush_page(Regime::El10, va >> 12 | 9 << 48, Some(9));
        mmu.flush_asid(Regime::El10, 9);
        assert_eq!(mmu.entries(), 1);
        mmu.flush_page(Regime::El10, va >> 12 | 7 << 48, Some(7));
        assert_eq!(mmu.entries(), 0);
        assert_eq!(
            mmu.translate(&mut memory, va, Kind::Read, false).unwrap().0,
            0x4009_0000
        );
        mmu.flush_asid(Regime::El10, 7);
        assert_eq!(mmu.entries(), 0);
        // A new ASID finds no entry of the old one, and drops none.
        mmu.translate(&mut memory, va, Kind::Read, false).unwrap();
        state.system.regs[reg::TTBR0_EL1] =
            state.system.regs[reg::TTBR0_EL1] & 0xffff_ffff_ffff | 8 << 48;
        mmu.sync(&state);
        assert_eq!(mmu.entries(), 1);
        assert_eq!(mmu.hit(va, 8, flag::PR), None);
        // A fault is never kept.
        memory.write(path[3], &0u64.to_le_bytes()).unwrap();
        assert!(mmu.translate(&mut memory, va, Kind::Read, false).is_err());
        memory.write(path[3], &leaf.to_le_bytes()).unwrap();
        assert!(mmu.translate(&mut memory, va, Kind::Read, false).is_ok());
        // A change of a control register drops everything.
        assert!(mmu.entries() >= 1);
        state.system.regs[reg::MAIR_EL1] = 0x44;
        mmu.sync(&state);
        assert_eq!(mmu.entries(), 0);
        // A walk that the memory does not hold a descriptor for.
        state.system.regs[reg::TTBR0_EL1] = 0x7000_0000;
        mmu.sync(&state);
        assert_eq!(
            mmu.translate(&mut memory, va, Kind::Read, false),
            Err(Miss::Fault(Fault {
                kind: FaultKind::ExternalOnWalk,
                level: 0
            }))
        );
    }

    #[test]
    fn an_invalidation_by_address_drops_every_page_of_a_block() {
        let va = 0x7f_0020_0000u64;
        let (mut memory, state, path) = mapped(va, 0);
        // A block of 2 MiB at level 2, in place of the table of level 3.
        memory
            .write(path[2], &(0x4000_0000u64 | VALID | AF).to_le_bytes())
            .unwrap();
        let mut mmu = Mmu::new(&state);
        for page in [0u64, 5, 0x1ff] {
            let (phys, _) = mmu
                .translate(&mut memory, va + page * 0x1000, Kind::Read, false)
                .unwrap();
            assert_eq!(phys, 0x4000_0000 + page * 0x1000);
        }
        assert_eq!(mmu.entries(), 3);
        let (descriptors, phys) = mmu.trace(&mut memory, va + 0x5123);
        assert_eq!((descriptors.len(), phys), (3, Some(0x4000_5123)));
        // The address of the TLBI is in the middle of the block.
        mmu.flush_page(Regime::El10, (va + 0x10_0000) >> 12, None);
        assert_eq!(mmu.entries(), 0);
    }
}
