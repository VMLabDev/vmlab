//! The system registers as one table (DESIGN.md G3e): each row gives a
//! register its storage, its reset value, who may read and write it, and the
//! bits that a write changes. The key is the encoding of the LLVM tables
//! (op0:op1:CRn:CRm:op2), so an access without a row is known by name.
//!
//! Sources read (DESIGN.md "Sources"):
//! - S1: `generated/sysreg.rs`, for the names and the encodings.
//! - S6: the answers of a running QEMU `cortex-a53`. The probe
//!   `examples/a64_sysprobe.rs` read and wrote every one of the 32,768
//!   encodings at EL1 and at EL0, and its output is the committed file
//!   `tests/qemu-cortex-a53-sysregs.txt`. The reset values, the write masks
//!   and the syndromes of this file are those answers. The tests below
//!   compare the table with that file, and `QEMU_DIFFERENCES` is the
//!   allow-list of the rows that differ by decision (G2, V5).
//! - No file of S2 to S5 was opened, and no text of the Arm manual. What a
//!   register is for is general knowledge of the architecture's public
//!   register names. QEMU's source was not read (X1).

use crate::state::State;
use crate::timer::Timer;

/// The 16-bit encoding of a register: op0:op1:CRn:CRm:op2.
pub const fn encoding(op0: u16, op1: u16, crn: u16, crm: u16, op2: u16) -> u16 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

/// Which direction of access an exception level has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rw {
    None,
    Ro,
    Wo,
    Rw,
}

impl Rw {
    fn allows(self, write: bool) -> bool {
        match self {
            Self::None => false,
            Self::Ro => !write,
            Self::Wo => write,
            Self::Rw => true,
        }
    }
}

/// A control bit that opens an EL0 access. While it is closed the access
/// traps to EL1 as a trapped system instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// No control bit: the access is open.
    Open,
    /// SCTLR_EL1.UMA: DAIF.
    Uma,
    /// SCTLR_EL1.UCT: CTR_EL0.
    Uct,
    /// CNTKCTL_EL1.EL0PCTEN: the physical counter.
    PhysicalCounter,
    /// CNTKCTL_EL1.EL0VCTEN: the virtual counter.
    VirtualCounter,
    /// Either of the two: CNTFRQ_EL0.
    AnyCounter,
    /// CNTKCTL_EL1.EL0PTEN: the physical timer.
    PhysicalTimer,
    /// CNTKCTL_EL1.EL0VTEN: the virtual timer.
    VirtualTimer,
    /// MDSCR_EL1.TDCC clear: the debug communication channel.
    Dcc,
}

/// A register whose value is not a stored word of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Special {
    Nzcv,
    Daif,
    CurrentEl,
    SpSel,
    Fpcr,
    Fpsr,
    SpEl0,
    /// DCZID_EL0: the block size, and DZP for the level that reads.
    Dczid,
    /// CCSIDR_EL1: the cache that CSSELR_EL1 selects.
    Ccsidr,
    /// ISR_EL1: the interrupt lines.
    Isr,
    /// OSLAR_EL1: bit 0 goes to OSLSR_EL1.OSLK.
    Oslar,
    /// DBGCLAIMSET_EL1: reads as the eight claim tags that exist, and a
    /// write sets tags.
    ClaimSet,
    /// DBGCLAIMCLR_EL1: reads as the tags that are set, and a write clears
    /// tags. The tags are the word of this row.
    ClaimClear,
    /// CNTPCT_EL0 and CNTVCT_EL0.
    Counter,
    /// CNT[PV]_CTL_EL0. `true`: the virtual timer.
    TimerControl(bool),
    /// CNT[PV]_CVAL_EL0.
    TimerCompare(bool),
    /// CNT[PV]_TVAL_EL0.
    TimerValue(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A word of `System::regs`. A write changes the bits of `mask`.
    Stored,
    Special(Special),
    /// The register has a name and this CPU does not have it: an access is
    /// undefined. The text says why.
    Absent(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub name: &'static str,
    pub encoding: u16,
    pub kind: Kind,
    pub el1: Rw,
    pub el0: Rw,
    pub gate: Gate,
    pub reset: u64,
    /// The bits that a write changes.
    pub mask: u64,
    /// What a write stores for the value that the guest gives, before the
    /// mask.
    pub hook: Option<fn(u64) -> u64>,
}

const ALL: u64 = u64::MAX;
const PMU: &str = "this CPU has no PMU (DESIGN.md V5)";

const fn stored(name: &'static str, encoding: u16, reset: u64, mask: u64) -> Row {
    Row {
        name,
        encoding,
        kind: Kind::Stored,
        el1: Rw::Rw,
        el0: Rw::None,
        gate: Gate::Open,
        reset,
        mask,
        hook: None,
    }
}

/// A breakpoint control register stores the byte address select as two
/// pairs of equal bits: bit 5 also in bit 6, and bit 7 also in bit 8. The
/// lockstep of card C6 found it at a write of 0x...756a, which QEMU reads
/// back as 0x...746a (S6).
fn breakpoint_control(value: u64) -> u64 {
    let pairs = (value & (1 << 5 | 1 << 7)) << 1;
    (value & !(1 << 6 | 1 << 8)) | pairs
}

const fn bcr(name: &'static str, encoding: u16) -> Row {
    let mut row = stored(name, encoding, 0, ALL);
    row.hook = Some(breakpoint_control);
    row
}

/// A register that EL1 reads and nothing writes.
const fn id(name: &'static str, encoding: u16) -> Row {
    let mut row = stored(name, encoding, 0, 0);
    row.el1 = Rw::Ro;
    row
}

const fn special(name: &'static str, encoding: u16, what: Special, el1: Rw) -> Row {
    let mut row = stored(name, encoding, 0, ALL);
    row.kind = Kind::Special(what);
    row.el1 = el1;
    row
}

const fn absent(name: &'static str, encoding: u16, why: &'static str) -> Row {
    let mut row = stored(name, encoding, 0, 0);
    row.kind = Kind::Absent(why);
    row.el1 = Rw::None;
    row
}

impl Row {
    const fn reset(mut self, value: u64) -> Self {
        self.reset = value;
        self
    }

    const fn el0(mut self, access: Rw, gate: Gate) -> Self {
        self.el0 = access;
        self.gate = gate;
        self
    }
}

/// The name of the row that answers for the unallocated encodings of the ID
/// space (op0 3, op1 0, CRn 0, CRm 1 to 7): they read as zero.
pub const ID_RESERVED: &str = "ID_RESERVED";

/// Every register of the CPU. The order is the order of the storage.
pub const ROWS: &[Row] = &[
    // --- debug: stored, and no breakpoint or watchpoint acts (V3) ---------
    stored("OSDTRRX_EL1", 0x8002, 0, 0),
    stored("DBGBVR0_EL1", 0x8004, 0, ALL),
    bcr("DBGBCR0_EL1", 0x8005),
    stored("DBGWVR0_EL1", 0x8006, 0, !3),
    stored("DBGWCR0_EL1", 0x8007, 0, ALL),
    stored("DBGBVR1_EL1", 0x800c, 0, ALL),
    bcr("DBGBCR1_EL1", 0x800d),
    stored("DBGWVR1_EL1", 0x800e, 0, !3),
    stored("DBGWCR1_EL1", 0x800f, 0, ALL),
    stored("MDCCINT_EL1", 0x8010, 0, 0),
    stored("MDSCR_EL1", 0x8012, 0, ALL),
    stored("DBGBVR2_EL1", 0x8014, 0, ALL),
    bcr("DBGBCR2_EL1", 0x8015),
    stored("DBGWVR2_EL1", 0x8016, 0, !3),
    stored("DBGWCR2_EL1", 0x8017, 0, ALL),
    stored("OSDTRTX_EL1", 0x801a, 0, 0),
    stored("DBGBVR3_EL1", 0x801c, 0, ALL),
    bcr("DBGBCR3_EL1", 0x801d),
    stored("DBGWVR3_EL1", 0x801e, 0, !3),
    stored("DBGWCR3_EL1", 0x801f, 0, ALL),
    stored("DBGBVR4_EL1", 0x8024, 0, ALL),
    bcr("DBGBCR4_EL1", 0x8025),
    stored("DBGBVR5_EL1", 0x802c, 0, ALL),
    bcr("DBGBCR5_EL1", 0x802d),
    stored("OSECCR_EL1", 0x8032, 0, 0),
    id("MDRAR_EL1", 0x8080),
    special("OSLAR_EL1", 0x8084, Special::Oslar, Rw::Wo),
    // OSLM says that the OS lock is implemented, and OSLK that it is set.
    id("OSLSR_EL1", 0x808c).reset(0xa),
    stored("OSDLR_EL1", 0x809c, 0, 1),
    special("DBGCLAIMSET_EL1", 0x83c6, Special::ClaimSet, Rw::Rw),
    special("DBGCLAIMCLR_EL1", 0x83ce, Special::ClaimClear, Rw::Rw),
    id("MDCCSR_EL0", 0x9808).el0(Rw::Ro, Gate::Dcc),
    stored("DBGDTR_EL0", 0x9820, 0, 0).el0(Rw::Rw, Gate::Dcc),
    // The same encoding is DBGDTRTX_EL0 for a write.
    stored("DBGDTRRX_EL0", 0x9828, 0, 0).el0(Rw::Rw, Gate::Dcc),
    // --- identification (G2): the values of the 3d CPU -----------------------
    // Implementer 0 is reserved for software. The part number is ours.
    id("MIDR_EL1", 0xc000).reset(0x000f_3d00),
    id("MPIDR_EL1", 0xc005).reset(0x8000_0000),
    id("REVIDR_EL1", 0xc006),
    // No AArch32 at any level: its ID registers read as zero.
    id("ID_PFR0_EL1", 0xc008),
    id("ID_PFR1_EL1", 0xc009),
    id("ID_DFR0_EL1", 0xc00a),
    id("ID_AFR0_EL1", 0xc00b),
    id("ID_MMFR0_EL1", 0xc00c),
    id("ID_MMFR1_EL1", 0xc00d),
    id("ID_MMFR2_EL1", 0xc00e),
    id("ID_MMFR3_EL1", 0xc00f),
    id("ID_ISAR0_EL1", 0xc010),
    id("ID_ISAR1_EL1", 0xc011),
    id("ID_ISAR2_EL1", 0xc012),
    id("ID_ISAR3_EL1", 0xc013),
    id("ID_ISAR4_EL1", 0xc014),
    id("ID_ISAR5_EL1", 0xc015),
    id("ID_MMFR4_EL1", 0xc016),
    id("ID_ISAR6_EL1", 0xc017),
    id("MVFR0_EL1", 0xc018),
    id("MVFR1_EL1", 0xc019),
    id("MVFR2_EL1", 0xc01a),
    id("ID_PFR2_EL1", 0xc01c),
    id("ID_DFR1_EL1", 0xc01d),
    id("ID_MMFR5_EL1", 0xc01e),
    // EL0 and EL1 in AArch64 only, FP and AdvSIMD, no EL2 and no EL3.
    id("ID_AA64PFR0_EL1", 0xc020).reset(0x11),
    id("ID_AA64PFR1_EL1", 0xc021),
    id("ID_AA64PFR2_EL1", 0xc022),
    id("ID_AA64ZFR0_EL1", 0xc024),
    id("ID_AA64SMFR0_EL1", 0xc025),
    id("ID_AA64FPFR0_EL1", 0xc027),
    // Debug of Armv8.0, six breakpoints, four watchpoints, no PMU (V5).
    id("ID_AA64DFR0_EL1", 0xc028).reset(0x1030_5006),
    id("ID_AA64DFR1_EL1", 0xc029),
    id("ID_AA64DFR2_EL1", 0xc02a),
    id("ID_AA64AFR0_EL1", 0xc02c),
    id("ID_AA64AFR1_EL1", 0xc02d),
    // AES with PMULL, SHA1, SHA2 and CRC32.
    id("ID_AA64ISAR0_EL1", 0xc030).reset(0x0001_1120),
    id("ID_AA64ISAR1_EL1", 0xc031),
    id("ID_AA64ISAR2_EL1", 0xc032),
    id("ID_AA64ISAR3_EL1", 0xc033),
    // 40 bits of physical address, 16 bits of ASID, the 4K granule only,
    // and little-endian only.
    id("ID_AA64MMFR0_EL1", 0xc038).reset(0x0f00_0022),
    id("ID_AA64MMFR1_EL1", 0xc039),
    id("ID_AA64MMFR2_EL1", 0xc03a),
    id("ID_AA64MMFR3_EL1", 0xc03b),
    id("ID_AA64MMFR4_EL1", 0xc03c),
    id(ID_RESERVED, 0xc01b),
    // --- control -----------------------------------------------------------
    stored("SCTLR_EL1", 0xc080, 0x00c5_0838, 0xf3ff_f01f_ffff_ffff),
    stored("ACTLR_EL1", 0xc081, 0, 0),
    stored("CPACR_EL1", 0xc082, 0, ALL),
    stored("TTBR0_EL1", 0xc100, 0, ALL),
    stored("TTBR1_EL1", 0xc101, 0, ALL),
    stored("TCR_EL1", 0xc102, 0, ALL),
    stored("SPSR_EL1", 0xc200, 0, ALL),
    stored("ELR_EL1", 0xc201, 0, ALL),
    special("SP_EL0", 0xc208, Special::SpEl0, Rw::Rw),
    special("SPSel", 0xc210, Special::SpSel, Rw::Rw),
    special("CurrentEL", 0xc212, Special::CurrentEl, Rw::Ro),
    stored("AFSR0_EL1", 0xc288, 0, 0),
    stored("AFSR1_EL1", 0xc289, 0, 0),
    stored("ESR_EL1", 0xc290, 0, ALL),
    stored("FAR_EL1", 0xc300, 0, ALL),
    stored("PAR_EL1", 0xc3a0, 0, ALL),
    stored("MAIR_EL1", 0xc510, 0, ALL),
    stored("AMAIR_EL1", 0xc518, 0, 0),
    stored("VBAR_EL1", 0xc600, 0, !0x1f),
    id("RVBAR_EL1", 0xc601),
    special("ISR_EL1", 0xc608, Special::Isr, Rw::Ro),
    stored("CONTEXTIDR_EL1", 0xc681, 0, ALL),
    stored("TPIDR_EL1", 0xc684, 0, ALL),
    stored("CNTKCTL_EL1", 0xc708, 0, ALL),
    // --- caches ------------------------------------------------------------
    special("CCSIDR_EL1", 0xc800, Special::Ccsidr, Rw::Ro),
    id("CLIDR_EL1", 0xc801).reset(0x0a20_0023),
    id("AIDR_EL1", 0xc807),
    stored("CSSELR_EL1", 0xd000, 0, 0xf),
    id("CTR_EL0", 0xd801)
        .reset(0x8444_8004)
        .el0(Rw::Ro, Gate::Uct),
    special("DCZID_EL0", 0xd807, Special::Dczid, Rw::Ro).el0(Rw::Ro, Gate::Open),
    // --- PSTATE and FP -------------------------------------------------------
    special("NZCV", 0xda10, Special::Nzcv, Rw::Rw).el0(Rw::Rw, Gate::Open),
    special("DAIF", 0xda11, Special::Daif, Rw::Rw).el0(Rw::Rw, Gate::Uma),
    special("FPCR", 0xda20, Special::Fpcr, Rw::Rw).el0(Rw::Rw, Gate::Open),
    special("FPSR", 0xda21, Special::Fpsr, Rw::Rw).el0(Rw::Rw, Gate::Open),
    // --- thread registers ----------------------------------------------------
    stored("TPIDR_EL0", 0xde82, 0, ALL).el0(Rw::Rw, Gate::Open),
    stored("TPIDRRO_EL0", 0xde83, 0, ALL).el0(Rw::Ro, Gate::Open),
    // --- the generic timer ---------------------------------------------------
    stored("CNTFRQ_EL0", 0xdf00, 62_500_000, ALL).el0(Rw::Ro, Gate::AnyCounter),
    special("CNTPCT_EL0", 0xdf01, Special::Counter, Rw::Ro).el0(Rw::Ro, Gate::PhysicalCounter),
    special("CNTVCT_EL0", 0xdf02, Special::Counter, Rw::Ro).el0(Rw::Ro, Gate::VirtualCounter),
    special("CNTP_TVAL_EL0", 0xdf10, Special::TimerValue(false), Rw::Rw)
        .el0(Rw::Rw, Gate::PhysicalTimer),
    special("CNTP_CTL_EL0", 0xdf11, Special::TimerControl(false), Rw::Rw)
        .el0(Rw::Rw, Gate::PhysicalTimer),
    special(
        "CNTP_CVAL_EL0",
        0xdf12,
        Special::TimerCompare(false),
        Rw::Rw,
    )
    .el0(Rw::Rw, Gate::PhysicalTimer),
    special("CNTV_TVAL_EL0", 0xdf18, Special::TimerValue(true), Rw::Rw)
        .el0(Rw::Rw, Gate::VirtualTimer),
    special("CNTV_CTL_EL0", 0xdf19, Special::TimerControl(true), Rw::Rw)
        .el0(Rw::Rw, Gate::VirtualTimer),
    special("CNTV_CVAL_EL0", 0xdf1a, Special::TimerCompare(true), Rw::Rw)
        .el0(Rw::Rw, Gate::VirtualTimer),
    // --- the PMU: rows so that an access is known by name ---------------------
    absent("PMINTENSET_EL1", 0xc4f1, PMU),
    absent("PMINTENCLR_EL1", 0xc4f2, PMU),
    absent("PMCR_EL0", 0xdce0, PMU),
    absent("PMCNTENSET_EL0", 0xdce1, PMU),
    absent("PMCNTENCLR_EL0", 0xdce2, PMU),
    absent("PMOVSCLR_EL0", 0xdce3, PMU),
    absent("PMSWINC_EL0", 0xdce4, PMU),
    absent("PMSELR_EL0", 0xdce5, PMU),
    absent("PMCEID0_EL0", 0xdce6, PMU),
    absent("PMCEID1_EL0", 0xdce7, PMU),
    absent("PMCCNTR_EL0", 0xdce8, PMU),
    absent("PMXEVTYPER_EL0", 0xdce9, PMU),
    absent("PMXEVCNTR_EL0", 0xdcea, PMU),
    absent("PMUSERENR_EL0", 0xdcf0, PMU),
    absent("PMOVSSET_EL0", 0xdcf3, PMU),
    absent("PMEVCNTR0_EL0", 0xdf40, PMU),
    absent("PMEVCNTR1_EL0", 0xdf41, PMU),
    absent("PMEVCNTR2_EL0", 0xdf42, PMU),
    absent("PMEVCNTR3_EL0", 0xdf43, PMU),
    absent("PMEVCNTR4_EL0", 0xdf44, PMU),
    absent("PMEVCNTR5_EL0", 0xdf45, PMU),
    absent("PMEVTYPER0_EL0", 0xdf60, PMU),
    absent("PMEVTYPER1_EL0", 0xdf61, PMU),
    absent("PMEVTYPER2_EL0", 0xdf62, PMU),
    absent("PMEVTYPER3_EL0", 0xdf63, PMU),
    absent("PMEVTYPER4_EL0", 0xdf64, PMU),
    absent("PMEVTYPER5_EL0", 0xdf65, PMU),
    absent("PMCCFILTR_EL0", 0xdf7f, PMU),
];

/// The count of rows, which is the count of stored words.
pub const COUNT: usize = ROWS.len();

// `find` holds a row number in a byte.
const _: () = assert!(COUNT < 255);

const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut at = 0;
    while at < a.len() {
        if a[at] != b[at] {
            return false;
        }
        at += 1;
    }
    true
}

/// The index of the row `name`, or `COUNT` when the table has none.
pub const fn position(name: &str) -> usize {
    let mut at = 0;
    while at < ROWS.len() {
        if same(ROWS[at].name, name) {
            return at;
        }
        at += 1;
    }
    COUNT
}

/// The index of the row `name`. A name without a row fails the build where
/// this is a constant.
pub const fn index(name: &str) -> usize {
    let at = position(name);
    assert!(at < COUNT, "the table has no row of that name");
    at
}

/// The rows that the core itself reads and writes.
pub mod reg {
    use super::index;
    pub const SCTLR_EL1: usize = index("SCTLR_EL1");
    pub const TTBR0_EL1: usize = index("TTBR0_EL1");
    pub const TTBR1_EL1: usize = index("TTBR1_EL1");
    pub const TCR_EL1: usize = index("TCR_EL1");
    pub const MAIR_EL1: usize = index("MAIR_EL1");
    pub const PAR_EL1: usize = index("PAR_EL1");
    pub const CPACR_EL1: usize = index("CPACR_EL1");
    pub const SPSR_EL1: usize = index("SPSR_EL1");
    pub const ELR_EL1: usize = index("ELR_EL1");
    pub const ESR_EL1: usize = index("ESR_EL1");
    pub const FAR_EL1: usize = index("FAR_EL1");
    pub const VBAR_EL1: usize = index("VBAR_EL1");
    pub const CNTKCTL_EL1: usize = index("CNTKCTL_EL1");
    pub const MDSCR_EL1: usize = index("MDSCR_EL1");
    pub const OSLSR_EL1: usize = index("OSLSR_EL1");
    pub const CSSELR_EL1: usize = index("CSSELR_EL1");
    pub const MPIDR_EL1: usize = index("MPIDR_EL1");
    pub const DBGCLAIMCLR_EL1: usize = index("DBGCLAIMCLR_EL1");
    pub const ID_RESERVED: usize = index(super::ID_RESERVED);
}

/// Bits of SCTLR_EL1 that the core acts on.
pub mod sctlr {
    pub const M: u64 = 1;
    pub const A: u64 = 1 << 1;
    pub const WXN: u64 = 1 << 19;
    /// The bits that a translation depends on.
    pub const TRANSLATION: u64 = M | A | WXN;
    pub const UMA: u64 = 1 << 9;
    pub const DZE: u64 = 1 << 14;
    pub const UCT: u64 = 1 << 15;
    pub const UCI: u64 = 1 << 26;
    pub const NTWI: u64 = 1 << 16;
    pub const NTWE: u64 = 1 << 18;
}

/// The row of an encoding. `None`: the CPU has no such register.
pub fn find(encoding: u16) -> Option<usize> {
    // The row number plus one of each encoding with op0 of 2 or 3.
    static TABLE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = vec![0u8; 0x8000];
        // op0 3, op1 0, CRn 0, CRm 1 to 7: the ID space reads as zero.
        for encoding in 0xc008..=0xc03fusize {
            table[encoding - 0x8000] = reg::ID_RESERVED as u8 + 1;
        }
        for (at, row) in ROWS.iter().enumerate() {
            if row.name != ID_RESERVED {
                table[usize::from(row.encoding) - 0x8000] = at as u8 + 1;
            }
        }
        table
    });
    let at = *table.get(usize::from(encoding).checked_sub(0x8000)?)?;
    (at != 0).then(|| usize::from(at) - 1)
}

/// The name of an encoding for a log line: the names of the LLVM tables, or
/// the generic form `S<op0>_<op1>_C<n>_C<m>_<op2>`.
pub fn name(encoding: u16) -> String {
    let names: Vec<&str> = crate::generated::sysreg::SYSREGS
        .iter()
        .filter(|register| register.encoding == encoding)
        .map(|register| register.name)
        .collect();
    if names.is_empty() {
        format!(
            "S{}_{}_C{}_C{}_{}",
            encoding >> 14,
            (encoding >> 11) & 7,
            (encoding >> 7) & 15,
            (encoding >> 3) & 15,
            encoding & 7
        )
    } else {
        names.join("/")
    }
}

/// The identity of a CPU (DESIGN.md A8): what its ID registers say, and the
/// reset value of any other stored row that differs from the table. A
/// machine picks one, and the M1 will bring its own.
#[derive(Debug, PartialEq, Eq)]
pub struct CpuModel {
    pub name: &'static str,
    /// Row name and reset value, for the rows that differ from the table.
    pub resets: &'static [(&'static str, u64)],
    /// CCSIDR_EL1 by the value of CSSELR_EL1.
    pub ccsidr: [u64; 16],
}

/// The cache geometry that QEMU's `cortex-a53` reports (the `ccsidr` lines
/// of the probe file), which the 3d CPU keeps: a data and an instruction
/// cache at level 1, and a unified cache at level 2.
const CCSIDR: [u64; 16] = {
    let mut values = [0; 16];
    values[0] = 0x700f_e01a;
    values[1] = 0x203f_e002;
    values[2] = 0x707f_e07a;
    values
};

/// The CPU of phase 3d (G2): Armv8.0-A with CRC32, AES, SHA1 and SHA2, a 4K
/// granule, EL0 and EL1 in AArch64 only, and no PMU. Its values are the
/// reset values of the table.
pub static V8_0: CpuModel = CpuModel {
    name: "vmlab-v8.0",
    resets: &[],
    ccsidr: CCSIDR,
};

/// The system registers of one CPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct System {
    /// One word per row of `ROWS`. A row that is not `Stored` leaves its
    /// word unused.
    pub regs: [u64; COUNT],
    /// The physical timer and the virtual timer.
    pub timers: [Timer; 2],
    pub model: &'static CpuModel,
}

impl System {
    pub fn reset(model: &'static CpuModel) -> Self {
        let mut regs = [0; COUNT];
        for (word, row) in regs.iter_mut().zip(ROWS) {
            *word = row.reset;
        }
        for (name, value) in model.resets {
            let at = position(name);
            assert!(at < COUNT, "the CPU model names the unknown row {name}");
            regs[at] = *value;
        }
        Self {
            regs,
            timers: [Timer::default(); 2],
            model,
        }
    }
}

/// Why an access does not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    /// The Undefined Instruction exception.
    Undefined,
    /// A trapped system instruction: exception class 0x18 with the ISS of
    /// the access.
    Trapped,
    /// FP is disabled for this level: exception class 0x07.
    Fp,
}

/// What the registers read that is not in the state of the CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Environment {
    /// The count of the system counter.
    pub count: u64,
    pub irq: bool,
    pub fiq: bool,
}

fn gate_open(gate: Gate, state: &State) -> bool {
    let sctlr = state.system.regs[reg::SCTLR_EL1];
    let cntkctl = state.system.regs[reg::CNTKCTL_EL1];
    match gate {
        Gate::Open => true,
        Gate::Uma => sctlr & sctlr::UMA != 0,
        Gate::Uct => sctlr & sctlr::UCT != 0,
        Gate::PhysicalCounter => cntkctl & 1 != 0,
        Gate::VirtualCounter => cntkctl & 2 != 0,
        Gate::AnyCounter => cntkctl & 3 != 0,
        Gate::VirtualTimer => cntkctl & (1 << 8) != 0,
        Gate::PhysicalTimer => cntkctl & (1 << 9) != 0,
        Gate::Dcc => state.system.regs[reg::MDSCR_EL1] & (1 << 12) == 0,
    }
}

/// Whether the level of `state` may read or write the row.
pub fn check(row: usize, write: bool, state: &State) -> Result<(), Denied> {
    let row = &ROWS[row];
    if matches!(row.kind, Kind::Absent(_)) {
        return Err(Denied::Undefined);
    }
    let el0 = state.context.el == 0;
    let access = if el0 { row.el0 } else { row.el1 };
    if !access.allows(write) {
        return Err(Denied::Undefined);
    }
    if el0 && !gate_open(row.gate, state) {
        return Err(Denied::Trapped);
    }
    match row.kind {
        Kind::Special(Special::Fpcr | Special::Fpsr) if !state.context.fp => Err(Denied::Fp),
        // SP_EL0 is not a register of its own while it is the stack pointer.
        Kind::Special(Special::SpEl0) if !state.context.sp_sel => Err(Denied::Undefined),
        _ => Ok(()),
    }
}

/// The bits of FPCR and FPSR that a write changes.
pub const FPCR_MASK: u64 = 0x07f7_0000;
pub const FPSR_MASK: u64 = 0xf800_009f;

/// The value that `MRS` of the row gives. The access is checked already.
pub fn read(row: usize, state: &State, environment: &Environment) -> u64 {
    let system = &state.system;
    let Kind::Special(what) = ROWS[row].kind else {
        return system.regs[row];
    };
    match what {
        Special::Nzcv => u64::from(state.nzcv()) << 28,
        Special::Daif => u64::from(state.daif) << 6,
        Special::CurrentEl => u64::from(state.context.el) << 2,
        Special::SpSel => u64::from(state.context.sp_sel),
        Special::Fpcr => u64::from(state.fpcr()),
        Special::Fpsr => u64::from(state.fpsr()),
        Special::SpEl0 => state.slots[crate::state::slot::SP_EL0] as u64,
        // Blocks of 64 bytes. DZP: DC ZVA is not allowed at this level.
        Special::Dczid => {
            let prohibited = state.context.el == 0 && system.regs[reg::SCTLR_EL1] & sctlr::DZE == 0;
            4 | u64::from(prohibited) << 4
        }
        Special::Ccsidr => system.model.ccsidr[(system.regs[reg::CSSELR_EL1] & 15) as usize],
        Special::Isr => u64::from(environment.irq) << 7 | u64::from(environment.fiq) << 6,
        Special::Oslar => 0,
        Special::ClaimSet => 0xff,
        Special::ClaimClear => system.regs[reg::DBGCLAIMCLR_EL1],
        Special::Counter => environment.count,
        Special::TimerControl(which) => {
            system.timers[usize::from(which)].control(environment.count)
        }
        Special::TimerCompare(which) => system.timers[usize::from(which)].compare,
        Special::TimerValue(which) => system.timers[usize::from(which)].value(environment.count),
    }
}

/// `MSR` of the row. The access is checked already.
pub fn write(row: usize, state: &mut State, environment: &Environment, value: u64) {
    let info = &ROWS[row];
    let Kind::Special(what) = info.kind else {
        let old = state.system.regs[row];
        let value = info.hook.map_or(value, |hook| hook(value));
        state.system.regs[row] = (old & !info.mask) | (value & info.mask);
        return;
    };
    match what {
        Special::Nzcv => state.set_nzcv((value >> 28) as u8 & 15),
        Special::Daif => state.daif = (value >> 6) as u8 & 15,
        Special::SpSel => state.context.sp_sel = value & 1 != 0,
        Special::Fpcr => state.set_fpcr((value & FPCR_MASK) as u32),
        Special::Fpsr => state.set_fpsr((value & FPSR_MASK) as u32),
        Special::SpEl0 => state.slots[crate::state::slot::SP_EL0] = value.into(),
        Special::Oslar => {
            let locked = &mut state.system.regs[reg::OSLSR_EL1];
            *locked = (*locked & !2) | (value & 1) << 1;
        }
        Special::ClaimSet => state.system.regs[reg::DBGCLAIMCLR_EL1] |= value & 0xff,
        Special::ClaimClear => state.system.regs[reg::DBGCLAIMCLR_EL1] &= !value,
        Special::TimerControl(which) => state.system.timers[usize::from(which)].set_control(value),
        Special::TimerCompare(which) => state.system.timers[usize::from(which)].compare = value,
        Special::TimerValue(which) => {
            state.system.timers[usize::from(which)].set_value(environment.count, value);
        }
        Special::CurrentEl | Special::Dczid | Special::Ccsidr | Special::Isr | Special::Counter => {
        }
    }
}

/// Whether an `MRS` of the row at the level `el` is a read of the stored
/// word and nothing else: no control bit decides it, and nothing is
/// computed. A lifter then reads the word inside the block (`READ_STORED`).
pub fn reads_stored(row: usize, el: u8) -> bool {
    let info = &ROWS[row];
    let access = if el == 0 { info.el0 } else { info.el1 };
    info.kind == Kind::Stored && access.allows(false) && (el != 0 || info.gate == Gate::Open)
}

fn read_stored(context: &mut dyn std::any::Any, args: &[u128], results: &mut [u128]) {
    let state = context
        .downcast_mut::<State>()
        .expect("the context of an aarch64 helper is the state");
    results[0] = state.system.regs[args[0] as usize].into();
}

/// The ordered helper that reads the stored word of the row that its
/// argument names. A register that a guest reads often, as TPIDR_EL1 for
/// every per-CPU variable of a kernel, then does not end its block.
pub static READ_STORED: vmlab_hv_tcg_ir::native::Helper = vmlab_hv_tcg_ir::native::Helper {
    name: "sysreg_read_stored",
    args: 1,
    results: 1,
    effects: vmlab_hv_tcg_ir::native::Effects::READS_STATE,
    function: vmlab_hv_tcg_ir::native::HelperFn::Ordered(read_stored),
};

/// The word of a row as a snapshot, a debugger and the lockstep see it,
/// without the access rules and without what a read computes: the stored
/// word, the compare value of a timer, its two control bits, SP_EL0.
pub fn peek(state: &State, row: usize) -> u64 {
    match ROWS[row].kind {
        Kind::Special(Special::TimerCompare(which)) => {
            state.system.timers[usize::from(which)].compare
        }
        Kind::Special(Special::TimerControl(which)) => {
            u64::from(state.system.timers[usize::from(which)].control)
        }
        Kind::Special(Special::SpEl0) => state.slots[crate::state::slot::SP_EL0] as u64,
        _ => state.system.regs[row],
    }
}

/// Set the word that `peek` gives. The caller refreshes the context.
pub fn poke(state: &mut State, row: usize, value: u64) {
    match ROWS[row].kind {
        Kind::Special(Special::TimerCompare(which)) => {
            state.system.timers[usize::from(which)].compare = value;
        }
        Kind::Special(Special::TimerControl(which)) => {
            state.system.timers[usize::from(which)].set_control(value);
        }
        Kind::Special(Special::SpEl0) => state.slots[crate::state::slot::SP_EL0] = value.into(),
        _ => state.system.regs[row] = value,
    }
}

/// The ISS of a trapped `MRS` or `MSR` (exception class 0x18).
pub fn trap_iss(encoding: u16, rt: u32, write: bool) -> u32 {
    let encoding = u32::from(encoding);
    let (op0, op1, crn, crm, op2) = (
        encoding >> 14,
        (encoding >> 11) & 7,
        (encoding >> 7) & 15,
        (encoding >> 3) & 15,
        encoding & 7,
    );
    op0 << 20 | op2 << 17 | op1 << 14 | crn << 10 | (rt & 31) << 5 | crm << 1 | u32::from(!write)
}

/// A row where QEMU's `cortex-a53` and the 3d CPU differ by decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Difference {
    pub name: &'static str,
    /// What QEMU reads at EL1 after its reset. `None`: QEMU has the register
    /// and we do not, or the other way.
    pub qemu: Option<u64>,
    pub why: &'static str,
}

const NO_AARCH32: &str = "no AArch32 at any level (G2): its ID registers read as zero";
const A53: &str = "an IMPLEMENTATION DEFINED register of the Cortex-A53, which the 3d CPU is not";

/// The allow-list of the probe (DESIGN.md O4, G2, V5): every row that reads
/// another value on QEMU's `cortex-a53`, and every register that only one of
/// the two has. The test `the_table_is_what_qemu_answers_outside_the_allow_list`
/// fails on a difference that is not here, and on an entry that is no
/// difference.
pub const QEMU_DIFFERENCES: &[Difference] = &[
    Difference {
        name: "MIDR_EL1",
        qemu: Some(0x410f_d034),
        why: "the 3d CPU is not a Cortex-A53: implementer 0 (software), part 0x3d0",
    },
    Difference {
        name: "REVIDR_EL1",
        qemu: Some(0x100),
        why: "no revision of a Cortex-A53",
    },
    Difference {
        name: "ID_PFR0_EL1",
        qemu: Some(0x131),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_PFR1_EL1",
        qemu: Some(0x1_0001),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_DFR0_EL1",
        qemu: Some(0x0300_0006),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_MMFR0_EL1",
        qemu: Some(0x1010_1105),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_MMFR1_EL1",
        qemu: Some(0x4000_0000),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_MMFR2_EL1",
        qemu: Some(0x0126_0000),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_MMFR3_EL1",
        qemu: Some(0x0210_2211),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_ISAR0_EL1",
        qemu: Some(0x0210_1110),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_ISAR1_EL1",
        qemu: Some(0x1311_2111),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_ISAR2_EL1",
        qemu: Some(0x2123_2042),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_ISAR3_EL1",
        qemu: Some(0x0111_2131),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_ISAR4_EL1",
        qemu: Some(0x1_1142),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_ISAR5_EL1",
        qemu: Some(0x1_1121),
        why: NO_AARCH32,
    },
    Difference {
        name: "MVFR0_EL1",
        qemu: Some(0x1011_0222),
        why: NO_AARCH32,
    },
    Difference {
        name: "MVFR1_EL1",
        qemu: Some(0x1211_1111),
        why: NO_AARCH32,
    },
    Difference {
        name: "MVFR2_EL1",
        qemu: Some(0x43),
        why: NO_AARCH32,
    },
    Difference {
        name: "ID_AA64PFR0_EL1",
        qemu: Some(0x22),
        why: "EL0 and EL1 run AArch64 only (G2): 1 in both fields, where QEMU has 2",
    },
    Difference {
        name: "ID_AA64DFR0_EL1",
        qemu: Some(0x1030_5106),
        why: "no PMU (V5): PMUVer is 0, where QEMU has PMUv3",
    },
    Difference {
        name: "ID_AA64MMFR0_EL1",
        qemu: Some(0x1122),
        why: "a 4K granule only and little-endian only (G2): TGran64 is 0xf, BigEnd and SNSMem are 0",
    },
    Difference {
        name: "RMR_EL1",
        qemu: Some(1),
        why: "no warm reset by the guest: a reset is a request of the machine",
    },
    Difference {
        name: "PMINTENSET_EL1",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMINTENCLR_EL1",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMCR_EL0",
        qemu: Some(0x4103_3000),
        why: PMU,
    },
    Difference {
        name: "PMCNTENSET_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMCNTENCLR_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMOVSCLR_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMSWINC_EL0",
        qemu: None,
        why: PMU,
    },
    Difference {
        name: "PMSELR_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMCEID0_EL0",
        qemu: Some(0x2_0001),
        why: PMU,
    },
    Difference {
        name: "PMCEID1_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMCCNTR_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMXEVTYPER_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMXEVCNTR_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMUSERENR_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMOVSSET_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVCNTR0_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVCNTR1_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVCNTR2_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVCNTR3_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVCNTR4_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVCNTR5_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVTYPER0_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVTYPER1_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVTYPER2_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVTYPER3_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVTYPER4_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMEVTYPER5_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "PMCCFILTR_EL0",
        qemu: Some(0),
        why: PMU,
    },
    Difference {
        name: "S3_1_C11_C0_2",
        qemu: Some(0),
        why: A53,
    },
    Difference {
        name: "S3_1_C11_C0_3",
        qemu: Some(0),
        why: A53,
    },
    Difference {
        name: "S3_1_C15_C0_0",
        qemu: Some(0),
        why: A53,
    },
    Difference {
        name: "S3_1_C15_C2_0",
        qemu: Some(0),
        why: A53,
    },
    Difference {
        name: "S3_1_C15_C2_1",
        qemu: Some(0),
        why: A53,
    },
    Difference {
        name: "S3_1_C15_C2_2",
        qemu: Some(0),
        why: A53,
    },
    Difference {
        name: "S3_1_C15_C2_3",
        qemu: Some(0),
        why: A53,
    },
    Difference {
        name: "S3_1_C15_C3_0",
        qemu: Some(0x0800_0000),
        why: A53,
    },
];

/// QEMU's `cortex-a53` as a CPU model: the 3d CPU with the ID values of the
/// allow-list put back. The lockstep runs our core as this CPU, so that an
/// `MRS` of an ID register agrees with the leader.
pub static QEMU_CORTEX_A53: CpuModel = CpuModel {
    name: "qemu-cortex-a53",
    resets: &[
        ("MIDR_EL1", 0x410f_d034),
        ("REVIDR_EL1", 0x100),
        ("ID_PFR0_EL1", 0x131),
        ("ID_PFR1_EL1", 0x1_0001),
        ("ID_DFR0_EL1", 0x0300_0006),
        ("ID_MMFR0_EL1", 0x1010_1105),
        ("ID_MMFR1_EL1", 0x4000_0000),
        ("ID_MMFR2_EL1", 0x0126_0000),
        ("ID_MMFR3_EL1", 0x0210_2211),
        ("ID_ISAR0_EL1", 0x0210_1110),
        ("ID_ISAR1_EL1", 0x1311_2111),
        ("ID_ISAR2_EL1", 0x2123_2042),
        ("ID_ISAR3_EL1", 0x0111_2131),
        ("ID_ISAR4_EL1", 0x1_1142),
        ("ID_ISAR5_EL1", 0x1_1121),
        ("MVFR0_EL1", 0x1011_0222),
        ("MVFR1_EL1", 0x1211_1111),
        ("MVFR2_EL1", 0x43),
        ("ID_AA64PFR0_EL1", 0x22),
        ("ID_AA64DFR0_EL1", 0x1030_5106),
        ("ID_AA64MMFR0_EL1", 0x1122),
    ],
    ccsidr: CCSIDR,
};

/// The CPU of the walk test against QEMU's `-cpu max` (card C8): QEMU's
/// `cortex-a53` with the 16K granule and 48 bits of physical address. It
/// is no CPU of a machine: the streams of that test stay inside what an
/// Armv8.0 walk and the walk of `max` agree on.
pub static QEMU_MAX_WALK: CpuModel = CpuModel {
    name: "qemu-max-walk",
    resets: &[
        ("MIDR_EL1", 0x410f_d034),
        ("ID_AA64PFR0_EL1", 0x22),
        // PARange 48 bits, 16 bits of ASID, and the 4K, 16K and 64K
        // granules.
        ("ID_AA64MMFR0_EL1", 0x0010_1125),
    ],
    ccsidr: CCSIDR,
};

#[cfg(test)]
mod tests;
