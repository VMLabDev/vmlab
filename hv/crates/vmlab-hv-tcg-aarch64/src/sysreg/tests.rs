//! The tests of the register table. Every row is run through `MRS` and
//! `MSR` on the interpreter loop, at EL1 and at EL0, and compared with what
//! QEMU's `cortex-a53` answered to the probe
//! (`tests/qemu-cortex-a53-sysregs.txt`).

use super::*;
use crate::exception::Cause;
use crate::exec::Cpu;
use crate::generated::sysreg::SYSREGS;
use crate::timer::Clock;
use std::collections::{BTreeMap, BTreeSet};
use vmlab_hv_tcg::{FlatMemory, Memory};

const BASE: u64 = 0x4000_0000;
const VECTORS: u64 = BASE + 0x800;

/// The 101 registers that the four template boots touch (DESIGN.md, "System
/// registers the templates touch").
const MEASURED: &str = "AIDR_EL1 CLIDR_EL1 CNTFRQ_EL0 CNTKCTL_EL1 CNTVCT_EL0 CNTV_CTL_EL0 \
    CNTV_CVAL_EL0 CONTEXTIDR_EL1 CPACR_EL1 CTR_EL0 CurrentEL DAIF \
    DBGBCR0_EL1 DBGBCR1_EL1 DBGBCR2_EL1 DBGBCR3_EL1 DBGBCR4_EL1 DBGBCR5_EL1 \
    DBGBVR0_EL1 DBGBVR1_EL1 DBGBVR2_EL1 DBGBVR3_EL1 DBGBVR4_EL1 DBGBVR5_EL1 \
    DBGWCR0_EL1 DBGWCR1_EL1 DBGWCR2_EL1 DBGWCR3_EL1 \
    DBGWVR0_EL1 DBGWVR1_EL1 DBGWVR2_EL1 DBGWVR3_EL1 DCZID_EL0 \
    ELR_EL1 ESR_EL1 FAR_EL1 FPCR FPSR ID_AA64DFR0_EL1 ID_AA64DFR1_EL1 \
    ID_AA64FPFR0_EL1 ID_AA64ISAR0_EL1 ID_AA64ISAR1_EL1 ID_AA64ISAR2_EL1 ID_AA64ISAR3_EL1 \
    ID_AA64MMFR0_EL1 ID_AA64MMFR1_EL1 ID_AA64MMFR2_EL1 ID_AA64MMFR3_EL1 ID_AA64MMFR4_EL1 \
    ID_AA64PFR0_EL1 ID_AA64PFR1_EL1 ID_AA64PFR2_EL1 \
    ID_AA64SMFR0_EL1 ID_AA64ZFR0_EL1 ID_DFR0_EL1 ID_DFR1_EL1 \
    ID_ISAR0_EL1 ID_ISAR1_EL1 ID_ISAR2_EL1 ID_ISAR3_EL1 ID_ISAR4_EL1 ID_ISAR5_EL1 ID_ISAR6_EL1 \
    ID_MMFR0_EL1 ID_MMFR1_EL1 ID_MMFR2_EL1 ID_MMFR3_EL1 ID_MMFR4_EL1 ID_MMFR5_EL1 \
    ID_PFR0_EL1 ID_PFR1_EL1 ID_PFR2_EL1 MAIR_EL1 MDSCR_EL1 MIDR_EL1 MPIDR_EL1 \
    MVFR0_EL1 MVFR1_EL1 MVFR2_EL1 NZCV OSDLR_EL1 OSLAR_EL1 PMCEID0_EL0 PMCEID1_EL0 \
    PMCNTENCLR_EL0 PMCR_EL0 PMINTENCLR_EL1 PMOVSCLR_EL0 PMUSERENR_EL0 \
    REVIDR_EL1 SCTLR_EL1 SPSR_EL1 SP_EL0 TCR_EL1 TPIDRRO_EL0 TPIDR_EL0 \
    TPIDR_EL1 TTBR0_EL1 TTBR1_EL1 VBAR_EL1";

/// What one `MRS` or `MSR` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// It ran. X0 after it.
    Ran(u64),
    /// It took an exception with this syndrome.
    Fault(u64),
}

impl Answer {
    /// As the probe writes a read.
    fn read(self) -> String {
        match self {
            Self::Ran(value) => format!("{value:x}"),
            other => other.wrote(),
        }
    }

    /// As the probe writes a write.
    fn wrote(self) -> String {
        match self {
            Self::Ran(_) => "ok".into(),
            Self::Fault(0x0200_0000) => "u".into(),
            Self::Fault(esr) => format!("t{esr:x}"),
        }
    }
}

/// One CPU that runs single system instructions from a fixed start.
struct Bench {
    cpu: Cpu<FlatMemory>,
    start: State,
}

impl Bench {
    fn new(model: &'static CpuModel, el0: bool, fp: bool) -> Self {
        let mut state = State::reset_as(BASE, model);
        state.system.regs[reg::VBAR_EL1] = VECTORS;
        if fp {
            state.enable_fp();
        }
        if el0 {
            assert!(state.set_cpsr(0x3c0));
        }
        let cpu = Cpu::new(FlatMemory::new(BASE, vec![0; 0x1000]), state.clone());
        Self { cpu, start: state }
    }

    /// Run `word` with `x` in X0, and go on from the state after it unless
    /// it took an exception.
    fn run(&mut self, word: u32, x: u64) -> Answer {
        self.cpu.memory.write(BASE, &word.to_le_bytes()).unwrap();
        self.cpu.state.set_pc(BASE);
        self.cpu.state.set_x(0, x);
        self.cpu.step().unwrap();
        match self.cpu.taken {
            None => Answer::Ran(self.cpu.state.x(0)),
            Some(taken) => {
                assert!(matches!(taken.cause, Cause::Exception(_)));
                let esr = self.cpu.state.system.regs[reg::ESR_EL1];
                // Back to the level of the bench, with what was written.
                let mut state = self.start.clone();
                state.system = self.cpu.state.system.clone();
                state.system.regs[reg::ESR_EL1] = self.start.system.regs[reg::ESR_EL1];
                state.system.regs[reg::ELR_EL1] = self.start.system.regs[reg::ELR_EL1];
                state.system.regs[reg::SPSR_EL1] = self.start.system.regs[reg::SPSR_EL1];
                state.slots = self.cpu.state.slots;
                state.refresh();
                self.cpu.state = state;
                Answer::Fault(esr)
            }
        }
    }

    fn fresh(&mut self) {
        self.cpu.state = self.start.clone();
    }

    fn mrs(&mut self, encoding: u16) -> Answer {
        self.run(0xd520_0000 | u32::from(encoding) << 5, 0)
    }

    fn msr(&mut self, encoding: u16, value: u64) -> Answer {
        self.run(0xd500_0000 | u32::from(encoding) << 5, value)
    }

    fn named(&mut self, name: &str) -> Answer {
        self.mrs(ROWS[index(name)].encoding)
    }

    fn set(&mut self, name: &str, value: u64) -> Answer {
        self.msr(ROWS[index(name)].encoding, value)
    }
}

#[test]
fn every_row_has_the_name_and_the_encoding_of_the_llvm_tables() {
    let mut names = BTreeSet::new();
    let mut encodings = BTreeSet::new();
    for row in ROWS {
        assert!(names.insert(row.name), "{} is there twice", row.name);
        assert!(
            encodings.insert(row.encoding),
            "{:#x} is there twice",
            row.encoding
        );
        if row.name == ID_RESERVED {
            continue;
        }
        let found = SYSREGS
            .iter()
            .find(|register| register.name == row.name)
            .unwrap_or_else(|| panic!("LLVM has no register {}", row.name));
        assert_eq!(found.encoding, row.encoding, "{}", row.name);
        assert_eq!(find(row.encoding), Some(index(row.name)), "{}", row.name);
        // A register that LLVM calls read-only or write-only is one here.
        let writes = matches!(row.el1, Rw::Rw | Rw::Wo);
        let reads = matches!(row.el1, Rw::Rw | Rw::Ro);
        if !matches!(row.kind, Kind::Absent(_)) {
            assert!(!writes || found.writeable || row.name == "DBGDTRRX_EL0", "{}", row.name);
            assert!(!reads || found.readable, "{}", row.name);
        }
        // A stored word that nothing can write and nothing resets is zero.
        assert!(row.mask != 0 || row.kind != Kind::Stored || row.el1 != Rw::Wo);
    }
    const { assert!(COUNT < 255, "the lookup table holds a row number in a byte") };
    assert_eq!(position("NO_SUCH_REGISTER"), COUNT);
    assert_eq!(encoding(3, 3, 14, 0, 2), 0xdf02);
    assert_eq!(name(0xdf02), "CNTVCT_EL0");
    assert_eq!(name(0xc780), "S3_0_C15_C0_0");
    // The unallocated ID space reads as zero, and nothing else is made up.
    assert_eq!(find(0xc01b), Some(reg::ID_RESERVED));
    assert_eq!(find(0xc03f), Some(reg::ID_RESERVED));
    assert_eq!(find(0xc007), None);
    assert_eq!(find(0xc040), None);
    assert_eq!(find(0x4000), None);
}

#[test]
fn each_of_the_101_measured_registers_has_a_row_and_answers_at_el1() {
    let names: Vec<&str> = MEASURED.split_whitespace().collect();
    assert_eq!(names.len(), 101);
    assert_eq!(names.iter().collect::<BTreeSet<_>>().len(), 101);
    let mut bench = Bench::new(&V8_0, false, true);
    let (mut read, mut written, mut absent) = (0, 0, 0);
    for name in names {
        let at = position(name);
        assert!(at < COUNT, "{name} has no row");
        let row = &ROWS[at];
        bench.fresh();
        if matches!(row.kind, Kind::Absent(_)) {
            // The PMU registers: undefined, and named in the log of the CPU.
            assert_eq!(bench.mrs(row.encoding), Answer::Fault(0x0200_0000), "{name}");
            assert_eq!(bench.msr(row.encoding, 0), Answer::Fault(0x0200_0000), "{name}");
            assert!(name.starts_with("PM"), "{name}");
            absent += 1;
            continue;
        }
        if row.el1.allows(false) {
            let value = bench.mrs(row.encoding);
            assert!(matches!(value, Answer::Ran(_)), "{name}: {value:?}");
            read += 1;
        } else {
            assert_eq!(bench.mrs(row.encoding), Answer::Fault(0x0200_0000), "{name}");
        }
        if row.el1.allows(true) {
            // Never SCTLR_EL1.M: translation is card C8.
            let wrote = bench.msr(row.encoding, 0x5555_5555_5555_5554);
            assert!(matches!(wrote, Answer::Ran(_)), "{name}: {wrote:?}");
            written += 1;
        } else {
            assert_eq!(bench.msr(row.encoding, 0), Answer::Fault(0x0200_0000), "{name}");
        }
    }
    assert_eq!((read, written, absent), (93, 47, 7));
    // The seven are in the log, each as a read and as a write.
    assert_eq!(bench.cpu.unknown.len(), 14);
}

/// One line of the probe file: the columns by name.
fn probe() -> BTreeMap<u16, (String, BTreeMap<String, String>)> {
    let text = include_str!("../../tests/qemu-cortex-a53-sysregs.txt");
    let mut lines = BTreeMap::new();
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let mut parts = line.split_whitespace();
        let encoding = u16::from_str_radix(parts.next().unwrap(), 16).unwrap();
        let name = parts.next().unwrap().split('/').next().unwrap().to_string();
        let columns = parts
            .map(|part| {
                let (key, value) = part.split_once('=').unwrap();
                (key.to_string(), value.to_string())
            })
            .collect();
        assert!(lines.insert(encoding, (name, columns)).is_none());
    }
    lines
}

/// The names of the encodings where our CPU as `model` answers something
/// else than the probe file says.
fn differences(model: &'static CpuModel) -> BTreeSet<String> {
    let probe = probe();
    assert!(probe.len() > 150);
    let mut el1 = Bench::new(model, false, false);
    let mut el0 = Bench::new(model, true, false);
    let mut fp = Bench::new(model, false, true);
    let mut found = BTreeSet::new();
    for encoding in 0x8000..=0xffffu16 {
        let mut ours = BTreeMap::new();
        for (bench, read, wrote) in [(&mut el1, "el1r", "el1w"), (&mut el0, "el0r", "el0w")] {
            bench.fresh();
            let value = bench.mrs(encoding);
            let back = match value {
                Answer::Ran(value) => value,
                Answer::Fault(_) => 0,
            };
            ours.insert(read, value.read());
            ours.insert(wrote, bench.msr(encoding, back).wrote());
        }
        fp.fresh();
        let value = fp.mrs(encoding);
        let back = match value {
            Answer::Ran(value) => value,
            Answer::Fault(_) => 0,
        };
        let with_fp = (value.read(), fp.msr(encoding, back).wrote());
        let all_undefined = ours.values().all(|column| column == "u") && with_fp.0 == "u";
        let Some((name, theirs)) = probe.get(&encoding) else {
            if !all_undefined {
                found.insert(super::name(encoding));
            }
            continue;
        };
        let mut same = true;
        for (key, value) in &ours {
            let expected = &theirs[*key];
            // A counter, and a register that the probe itself had set.
            let any = (expected == "time" || expected == "probe") && value.len() < 17;
            same &= any || expected == value || (value != "u" && value != "ok" && expected == "probe");
        }
        // With FP enabled, where the file gives it.
        let expected_fp = theirs.get("fp").unwrap_or(&theirs["el1r"]);
        let expected_fpw = theirs.get("fpw").unwrap_or(&theirs["el1w"]);
        if expected_fp != "time" && expected_fp != "probe" {
            same &= *expected_fp == with_fp.0;
        }
        same &= *expected_fpw == with_fp.1;
        // What a write of all ones and of zero leaves, where the probe
        // tried: all ones, but never SCTLR_EL1.M.
        if let (Some(ones), Some(zeros)) = (theirs.get("ones"), theirs.get("zeros")) {
            fp.fresh();
            let pattern = if encoding == 0xc080 { !1 } else { u64::MAX };
            fp.msr(encoding, pattern);
            same &= fp.mrs(encoding).read() == *ones;
            fp.msr(encoding, 0);
            same &= fp.mrs(encoding).read() == *zeros;
        }
        if !same {
            found.insert(name.clone());
        }
    }
    found
}

#[test]
fn the_table_is_what_qemu_answers_outside_the_allow_list() {
    let allowed: BTreeSet<String> = QEMU_DIFFERENCES
        .iter()
        .map(|difference| difference.name.to_string())
        .collect();
    assert_eq!(allowed.len(), QEMU_DIFFERENCES.len(), "a name is there twice");
    assert_eq!(differences(&V8_0), allowed);
    // The value that the allow-list gives for QEMU is the value of the probe.
    let probe = probe();
    for difference in QEMU_DIFFERENCES {
        let (_, columns) = probe
            .values()
            .find(|(name, _)| name == difference.name)
            .unwrap_or_else(|| panic!("the probe has no {}", difference.name));
        let expected = match difference.qemu {
            Some(value) => format!("{value:x}"),
            None => "u".into(),
        };
        assert_eq!(columns["el1r"], expected, "{}", difference.name);
        assert!(!difference.why.is_empty());
    }
}

#[test]
fn as_qemus_cpu_only_the_registers_that_we_leave_out_differ() {
    // The model of the lockstep: the ID values of the allow-list are back.
    let missing: BTreeSet<String> = QEMU_DIFFERENCES
        .iter()
        .filter(|difference| {
            let at = position(difference.name);
            at == COUNT || matches!(ROWS[at].kind, Kind::Absent(_))
        })
        .map(|difference| difference.name.to_string())
        .collect();
    assert_eq!(differences(&QEMU_CORTEX_A53), missing);
    assert_eq!(missing.len(), 37);
    // Every reset of the model is a row, and differs from the table.
    for (name, value) in QEMU_CORTEX_A53.resets {
        assert_ne!(ROWS[index(name)].reset, *value, "{name}");
    }
    assert!(V8_0.resets.is_empty());
}

#[test]
fn the_3d_cpu_says_what_it_is_in_its_id_registers() {
    let mut bench = Bench::new(&V8_0, false, false);
    let mut value = |name: &str| match bench.named(name) {
        Answer::Ran(value) => value,
        other => panic!("{name}: {other:?}"),
    };
    // EL0 and EL1 in AArch64 only, no EL2, no EL3, FP and AdvSIMD.
    let pfr0 = value("ID_AA64PFR0_EL1");
    assert_eq!(pfr0 & 0xffff, 0x0011);
    assert_eq!((pfr0 >> 16) & 0xff, 0);
    // AES with PMULL, SHA1, SHA2 and CRC32, and nothing of a later version.
    let isar0 = value("ID_AA64ISAR0_EL1");
    assert_eq!(
        (
            (isar0 >> 4) & 15,
            (isar0 >> 8) & 15,
            (isar0 >> 12) & 15,
            (isar0 >> 16) & 15
        ),
        (2, 1, 1, 1)
    );
    assert_eq!(isar0 >> 20, 0);
    assert_eq!(value("ID_AA64ISAR1_EL1"), 0);
    // No PMU, six breakpoints and four watchpoints.
    let dfr0 = value("ID_AA64DFR0_EL1");
    assert_eq!((dfr0 >> 8) & 15, 0);
    assert_eq!(((dfr0 >> 12) & 15, (dfr0 >> 20) & 15), (5, 3));
    // The 4K granule and no other.
    let mmfr0 = value("ID_AA64MMFR0_EL1");
    assert_eq!(
        ((mmfr0 >> 28) & 15, (mmfr0 >> 24) & 15, (mmfr0 >> 20) & 15),
        (0, 15, 0)
    );
    // No AArch32, and nothing of v8.1 or later, SVE or SME.
    for name in [
        "ID_PFR0_EL1",
        "ID_ISAR5_EL1",
        "MVFR0_EL1",
        "ID_AA64MMFR1_EL1",
        "ID_AA64MMFR2_EL1",
        "ID_AA64PFR1_EL1",
        "ID_AA64ZFR0_EL1",
        "ID_AA64SMFR0_EL1",
    ] {
        assert_eq!(value(name), 0, "{name}");
    }
    assert_eq!(value("MIDR_EL1") >> 24, 0);
    assert_eq!(value("MPIDR_EL1"), 0x8000_0000);
    assert_eq!(value("CTR_EL0"), 0x8444_8004);
}

#[test]
fn a_write_changes_the_bits_of_the_mask_and_no_other() {
    let mut bench = Bench::new(&V8_0, false, true);
    for (name, wrote, read) in [
        ("VBAR_EL1", u64::MAX, !0x1fu64),
        ("DBGWVR2_EL1", u64::MAX, !3),
        ("CSSELR_EL1", u64::MAX, 15),
        ("OSDLR_EL1", u64::MAX, 1),
        ("SCTLR_EL1", !1, 0xf3ff_f01f_ffff_fffe),
        ("ACTLR_EL1", u64::MAX, 0),
        ("AMAIR_EL1", u64::MAX, 0),
        ("DBGCLAIMSET_EL1", 0, 0xff),
        ("NZCV", u64::MAX, 0xf000_0000),
        ("DAIF", u64::MAX, 0x3c0),
        ("SPSel", u64::MAX, 1),
        ("FPCR", u64::MAX, 0x07f7_0000),
        ("FPSR", u64::MAX, 0xf800_009f),
        ("TPIDR_EL1", 0x1234_5678_9abc_def0, 0x1234_5678_9abc_def0),
        ("ESR_EL1", u64::MAX, u64::MAX),
    ] {
        bench.fresh();
        assert!(matches!(bench.set(name, wrote), Answer::Ran(_)), "{name}");
        assert_eq!(bench.named(name), Answer::Ran(read), "{name}");
    }
    // FPCR and FPSR are the register slots that the FP helpers use.
    bench.fresh();
    bench.set("FPCR", 3 << 22);
    assert_eq!(bench.cpu.state.fpcr(), 3 << 22);
    bench.cpu.state.set_fpsr(0x1f);
    assert_eq!(bench.named("FPSR"), Answer::Ran(0x1f));
    // NZCV is the four flag slots.
    bench.set("NZCV", 0x6000_0000);
    assert_eq!(bench.cpu.state.nzcv(), 0b0110);
    // X31 is the zero register for MSR, and MRS to it reads and drops.
    bench.cpu.state.set_nzcv(0xf);
    let msr_xzr = 0xd500_0000 | u32::from(ROWS[index("NZCV")].encoding) << 5 | 31;
    assert_eq!(bench.run(msr_xzr, 7), Answer::Ran(7));
    assert_eq!(bench.cpu.state.nzcv(), 0);
    let mrs_xzr = 0xd520_0000 | u32::from(ROWS[index("TPIDR_EL1")].encoding) << 5 | 31;
    assert_eq!(bench.run(mrs_xzr, 7), Answer::Ran(7));
}

#[test]
fn the_special_registers() {
    let mut bench = Bench::new(&V8_0, false, false);
    assert_eq!(bench.named("CurrentEL"), Answer::Ran(4));
    assert_eq!(bench.named("SPSel"), Answer::Ran(1));
    assert_eq!(bench.named("DAIF"), Answer::Ran(0x3c0));
    // SP_EL0 is a register while SP_EL1 is the stack pointer, and only then.
    bench.cpu.state.slots[crate::state::slot::SP_EL0] = 0x1000;
    assert_eq!(bench.named("SP_EL0"), Answer::Ran(0x1000));
    bench.set("SP_EL0", 0x2000);
    assert_eq!(bench.cpu.state.slots[crate::state::slot::SP_EL0], 0x2000);
    bench.set("SPSel", 0);
    assert_eq!(bench.cpu.state.sp(), 0x2000);
    assert_eq!(bench.named("SP_EL0"), Answer::Fault(0x0200_0000));
    // The cache that CSSELR_EL1 selects.
    bench.fresh();
    assert_eq!(bench.named("CCSIDR_EL1"), Answer::Ran(0x700f_e01a));
    bench.set("CSSELR_EL1", 2);
    assert_eq!(bench.named("CCSIDR_EL1"), Answer::Ran(0x70ff_e07a));
    bench.set("CSSELR_EL1", 7);
    assert_eq!(bench.named("CCSIDR_EL1"), Answer::Ran(0));
    // The OS lock: OSLAR_EL1 is write-only and shows in OSLSR_EL1.OSLK.
    assert_eq!(bench.named("OSLSR_EL1"), Answer::Ran(0xa));
    assert_eq!(bench.named("OSLAR_EL1"), Answer::Fault(0x0200_0000));
    bench.set("OSLAR_EL1", 0);
    assert_eq!(bench.named("OSLSR_EL1"), Answer::Ran(0x8));
    bench.set("OSLAR_EL1", 0xffff_ffff_ffff_ffff);
    assert_eq!(bench.named("OSLSR_EL1"), Answer::Ran(0xa));
    // ISR_EL1 shows the two lines.
    assert_eq!(bench.named("ISR_EL1"), Answer::Ran(0));
    bench.cpu.irq = true;
    assert_eq!(bench.named("ISR_EL1"), Answer::Ran(0x80));
    bench.cpu.fiq = true;
    assert_eq!(bench.named("ISR_EL1"), Answer::Ran(0xc0));
    // DCZID_EL0: 64 bytes, and DC ZVA is prohibited at EL0 until
    // SCTLR_EL1.DZE allows it.
    assert_eq!(bench.named("DCZID_EL0"), Answer::Ran(4));
    let mut user = Bench::new(&V8_0, true, false);
    assert_eq!(user.named("DCZID_EL0"), Answer::Ran(0x14));
    user.cpu.state.system.regs[reg::SCTLR_EL1] |= sctlr::DZE;
    assert_eq!(user.named("DCZID_EL0"), Answer::Ran(4));
}

#[test]
fn the_timer_registers_read_the_counter_of_the_clock() {
    let mut bench = Bench::new(&V8_0, false, false);
    bench.cpu.clock = Clock::Manual(1000);
    assert_eq!(bench.named("CNTFRQ_EL0"), Answer::Ran(62_500_000));
    assert_eq!(bench.named("CNTVCT_EL0"), Answer::Ran(1000));
    assert_eq!(bench.named("CNTPCT_EL0"), Answer::Ran(1000));
    for (control, compare, value, which) in [
        ("CNTV_CTL_EL0", "CNTV_CVAL_EL0", "CNTV_TVAL_EL0", 1),
        ("CNTP_CTL_EL0", "CNTP_CVAL_EL0", "CNTP_TVAL_EL0", 0),
    ] {
        bench.set(value, 500);
        assert_eq!(bench.named(compare), Answer::Ran(1500));
        assert_eq!(bench.named(value), Answer::Ran(500));
        bench.set(control, 0xffff_ffff_ffff_fff1);
        assert_eq!(bench.named(control), Answer::Ran(1));
        bench.cpu.clock = Clock::Manual(1600);
        // ISTATUS is computed, and the timer value went below zero.
        assert_eq!(bench.named(control), Answer::Ran(5));
        assert_eq!(bench.named(value), Answer::Ran(0xffff_ff9c));
        assert!(bench.cpu.state.system.timers[which].line(1600));
        bench.set(compare, u64::MAX);
        assert_eq!(bench.named(control), Answer::Ran(1));
        bench.cpu.clock = Clock::Manual(1000);
    }
    // The other timer was not touched by the first pass of the loop.
    assert_eq!(bench.cpu.state.system.timers[1].compare, u64::MAX);
}

#[test]
fn el0_reaches_what_el1_opens() {
    // Each gate: the register, the row that holds the control bit, the bit,
    // and the syndrome of the read while the gate is closed.
    let gates: [(&str, usize, u64, u64); 9] = [
        ("DAIF", reg::SCTLR_EL1, sctlr::UMA, 0x6232_d005),
        ("CTR_EL0", reg::SCTLR_EL1, sctlr::UCT, 0x6232_c001),
        ("CNTPCT_EL0", reg::CNTKCTL_EL1, 1, 0x6232_f801),
        ("CNTVCT_EL0", reg::CNTKCTL_EL1, 2, 0x6234_f801),
        ("CNTFRQ_EL0", reg::CNTKCTL_EL1, 1, 0x6230_f801),
        ("CNTFRQ_EL0", reg::CNTKCTL_EL1, 2, 0x6230_f801),
        ("CNTV_CTL_EL0", reg::CNTKCTL_EL1, 1 << 8, 0x6232_f807),
        ("CNTP_CVAL_EL0", reg::CNTKCTL_EL1, 1 << 9, 0x6234_f805),
        ("CNTP_TVAL_EL0", reg::CNTKCTL_EL1, 1 << 9, 0x6230_f805),
    ];
    for (name, control, bit, syndrome) in gates {
        let mut bench = Bench::new(&V8_0, true, false);
        assert_eq!(bench.named(name), Answer::Fault(syndrome), "{name}");
        // Every other bit of the control register leaves the gate closed.
        bench.cpu.state.system.regs[control] = !bit & ROWS[control].mask;
        if name != "CNTFRQ_EL0" {
            assert_eq!(bench.named(name), Answer::Fault(syndrome), "{name}");
        }
        bench.cpu.state.system.regs[control] = bit;
        assert!(matches!(bench.named(name), Answer::Ran(_)), "{name}");
    }
    // A write through an open gate, and the syndrome of a closed one: the
    // direction bit is clear and Rt is in the bits 9 to 5.
    let mut bench = Bench::new(&V8_0, true, false);
    let daif = ROWS[index("DAIF")].encoding;
    assert_eq!(bench.msr(daif, 0), Answer::Fault(0x6232_d004));
    assert_eq!(
        bench.run(0xd500_0000 | u32::from(daif) << 5 | 9, 0),
        Answer::Fault(0x6232_d124)
    );
    bench.cpu.state.system.regs[reg::SCTLR_EL1] |= sctlr::UMA;
    assert_eq!(bench.msr(daif, 0x40), Answer::Ran(0x40));
    assert_eq!(bench.cpu.state.daif, 1);
    // The debug channel closes when MDSCR_EL1.TDCC is set.
    assert_eq!(bench.named("MDCCSR_EL0"), Answer::Ran(0));
    bench.cpu.state.system.regs[reg::MDSCR_EL1] = 1 << 12;
    assert_eq!(bench.named("MDCCSR_EL0"), Answer::Fault(0x6220_c003));
    // What EL0 has without a gate, and what it never has.
    assert!(matches!(bench.set("TPIDR_EL0", 5), Answer::Ran(_)));
    assert_eq!(bench.named("TPIDR_EL0"), Answer::Ran(5));
    assert_eq!(bench.named("TPIDRRO_EL0"), Answer::Ran(0));
    assert_eq!(bench.set("TPIDRRO_EL0", 5), Answer::Fault(0x0200_0000));
    assert_eq!(bench.set("CNTFRQ_EL0", 5), Answer::Fault(0x0200_0000));
    for name in ["SCTLR_EL1", "MIDR_EL1", "TPIDR_EL1", "CurrentEL", "SPSel", "ELR_EL1"] {
        assert_eq!(bench.named(name), Answer::Fault(0x0200_0000), "{name}");
    }
    // FPCR and FPSR trap as FP does, at both levels.
    assert_eq!(bench.named("FPCR"), Answer::Fault(0x1fe0_0000));
    let mut kernel = Bench::new(&V8_0, false, false);
    assert_eq!(kernel.set("FPSR", 0), Answer::Fault(0x1fe0_0000));
    let mut enabled = Bench::new(&V8_0, true, true);
    assert_eq!(enabled.named("FPCR"), Answer::Ran(0));
}

#[test]
fn the_pstate_forms_of_msr() {
    // msr daifclr, #imm is d50340ff | imm << 8, daifset d50340df, spsel d50040bf.
    let (daifset, daifclr, spsel) = (0xd503_40dfu32, 0xd503_40ffu32, 0xd500_40bfu32);
    let mut bench = Bench::new(&V8_0, false, false);
    assert_eq!(bench.run(daifclr | 0x3 << 8, 0), Answer::Ran(0));
    assert_eq!(bench.cpu.state.daif, 0b1100);
    assert_eq!(bench.run(daifset | 0x1 << 8, 0), Answer::Ran(0));
    assert_eq!(bench.cpu.state.daif, 0b1101);
    assert_eq!(bench.run(spsel, 0), Answer::Ran(0));
    assert!(!bench.cpu.state.context.sp_sel);
    assert_eq!(bench.run(spsel | 1 << 8, 0), Answer::Ran(0));
    assert!(bench.cpu.state.context.sp_sel);
    // At EL0 SPSel is undefined, and DAIF is behind SCTLR_EL1.UMA.
    let mut user = Bench::new(&V8_0, true, false);
    assert_eq!(user.run(spsel, 0), Answer::Fault(0x0200_0000));
    assert_eq!(user.run(daifclr | 0xf << 8, 0), Answer::Fault(0x620e_d3fe));
    assert_eq!(user.run(daifset | 0x2 << 8, 0), Answer::Fault(0x620c_d3e4));
    user.cpu.state.system.regs[reg::SCTLR_EL1] |= sctlr::UMA;
    assert_eq!(user.run(daifclr | 0xf << 8, 0), Answer::Ran(0));
    assert_eq!(user.cpu.state.daif, 0);
}
