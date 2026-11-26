//! The system register probe of card C6 (DESIGN.md S6, G2): what QEMU's
//! `cortex-a53` on `virt` answers to `MRS` and `MSR` of every one of the
//! 32,768 register encodings, at EL1 and at EL0, after its reset. For the
//! named registers that are writeable at EL1 it also reads back a write of
//! all ones and a write of zero, which gives the bits that a write changes.
//!
//! `just tcg-a64-sysprobe` in `hv/` builds the plugin and runs this. The
//! output is the committed file `tests/qemu-cortex-a53-sysregs.txt`, which
//! the tests of `sysreg.rs` compare our register table with: a row that
//! differs must be in the allow-list.
//!
//! Usage:
//!   a64_sysprobe --plugin <a64lock.so> --dir <scratch directory>
//!       [--qemu <binary>] [--out <file>] [--names-out <file>]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use vmlab_hv_tcg_aarch64::generated::sysreg::SYSREGS;
use vmlab_hv_tcg_aarch64::oracle::asm::{self, reg};
use vmlab_hv_tcg_aarch64::oracle::leader::{Leader, Options, RAM_BASE};
use vmlab_hv_tcg_aarch64::oracle::{CPSR_EL1H, Core, Trace};

type Failure = Box<dyn std::error::Error>;

const RAM_MIB: u32 = 64;
const VECTORS: u64 = RAM_BASE + 32 * 1024 * 1024;
const CODE: u64 = VECTORS + 0x1000;
/// The encodings with op0 of 2 or 3: what `MRS` and `MSR` can name.
const FIRST: u32 = 0x8000;
const COUNT: u32 = 0x8000;

/// What one instruction of a sweep did.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// It ran. The registers after it.
    Ran(Box<Core>),
    /// It took an exception with this syndrome.
    Fault(u32),
}

/// Sixteen entries of one handler: note the syndrome in X1, and return to
/// the instruction after the one that faulted.
fn vector_table() -> Vec<u32> {
    let handler = [
        asm::mrs(1, reg::ESR_EL1),
        asm::mrs(17, reg::ELR_EL1),
        asm::add(17, 17, 4),
        asm::msr(reg::ELR_EL1, 17),
        asm::ERET,
    ];
    let mut words = Vec::new();
    for _ in 0..16 {
        words.extend(handler);
        words.resize(words.len() + 32 - handler.len(), asm::NOP);
    }
    words
}

/// Sixteen entries of the handler of the PSTATE pass: note the saved PSTATE
/// in X1, and return to EL1 with SP_EL1 at the instruction after the one
/// that faulted.
fn pstate_table() -> Vec<u32> {
    let handler = [
        asm::mrs(1, reg::SPSR_EL1),
        asm::mrs(17, reg::ELR_EL1),
        asm::add(17, 17, 4),
        asm::msr(reg::ELR_EL1, 17),
        asm::movz(16, 0x3c5, 0),
        asm::msr(reg::SPSR_EL1, 16),
        asm::ERET,
    ];
    let mut words = Vec::new();
    for _ in 0..16 {
        words.extend(handler);
        words.resize(words.len() + 32 - handler.len(), asm::NOP);
    }
    words
}

/// Which bits of a saved PSTATE an exception return takes into PSTATE: for
/// each base, the bits above the mode whose value follows SPSR_EL1.
fn pstate(arguments: &Arguments) -> Result<String, Failure> {
    const CASE: usize = 10;
    let bases = [("el1h", 0x3c5u64), ("el0t", 0x3c0), ("illegal", 0x3c9)];
    let mut body = Vec::new();
    for (_, base) in bases {
        for bit in 5..64 {
            body.extend(asm::load(16, base ^ (1 << bit)));
            body.push(asm::msr(reg::SPSR_EL1, 16));
            let at = CODE + 4 * (5 + body.len() as u64);
            // adr x16, .+12
            body.push(0x1000_0070);
            let _ = at;
            body.push(asm::msr(reg::ELR_EL1, 16));
            body.push(asm::ERET);
            // The marker: BRK, whose handler notes the PSTATE it came from.
            body.push(0xd420_0000);
            body.push(asm::NOP);
        }
    }
    let outcomes = phase_with(arguments, &pstate_table(), false, false, &body, None)?;
    let mut text = String::new();
    for (number, (name, base)) in bases.iter().enumerate() {
        let (mut kept, mut entry) = (0u64, 0u64);
        for bit in 5..64u64 {
            let case = (number * 59 + (bit as usize - 5)) * CASE;
            let saved = base ^ (1 << bit);
            match outcomes[&(case + 8)] {
                Outcome::Fault(pstate) => {
                    let pstate = u64::from(pstate);
                    if (pstate ^ saved) & (1 << bit) == 0 {
                        kept |= 1 << bit;
                    }
                    // What an illegal return or a return with IL leaves set
                    // beyond the bit itself.
                    entry |= pstate & !saved & !0x1f;
                }
                Outcome::Ran(_) => return Err("the marker of the PSTATE pass ran".into()),
            }
        }
        text.push_str(&format!(
            "pstate return={name} spsr={base:x} follows={kept:x} extra={entry:x}\n"
        ));
    }
    Ok(text)
}

/// One QEMU from its reset: run `body` at EL1 or EL0 and say what each of
/// its instructions did, by its index in `body`.
fn phase(
    arguments: &Arguments,
    el0: bool,
    fp: bool,
    body: &[u32],
    names: Option<&Path>,
) -> Result<HashMap<usize, Outcome>, Failure> {
    phase_with(arguments, &vector_table(), el0, fp, body, names)
}

/// As `phase`, with another vector table. An entry of the table leaves in X1
/// what `Outcome::Fault` then holds.
fn phase_with(
    arguments: &Arguments,
    table: &[u32],
    el0: bool,
    fp: bool,
    body: &[u32],
    names: Option<&Path>,
) -> Result<HashMap<usize, Outcome>, Failure> {
    let options = Options {
        names: names.map(Path::to_path_buf),
        ..Options::default()
    };
    let mut leader = Leader::start_options(
        &arguments.qemu,
        &arguments.plugin,
        &arguments.directory,
        RAM_MIB,
        &options,
    )?;
    let mut words = Vec::new();
    words.extend(asm::load(16, VECTORS));
    words.push(asm::msr(reg::VBAR_EL1, 16));
    if fp {
        words.extend(asm::load(16, 3 << 20));
        words.push(asm::msr(reg::CPACR_EL1, 16));
    }
    if el0 {
        // EL0 with SP_EL0 and every interrupt masked.
        words.extend(asm::load(16, 0x3c0));
        words.push(asm::msr(reg::SPSR_EL1, 16));
        let target = CODE + 4 * (words.len() as u64 + 6);
        words.extend(asm::load(16, target));
        words.push(asm::msr(reg::ELR_EL1, 16));
        words.push(asm::ERET);
    }
    let first = CODE + 4 * words.len() as u64;
    words.extend(body);
    let end = CODE + 4 * words.len() as u64;
    words.push(asm::b(0));
    leader.load(VECTORS, &asm::bytes(table))?;
    leader.load(CODE, &asm::bytes(&words))?;
    leader.set_core(&Core {
        x: [0; 31],
        sp: 0,
        pc: CODE,
        cpsr: CPSR_EL1H,
    })?;
    leader.resume()?;
    let mut outcomes = HashMap::new();
    let mut fault: Option<u64> = None;
    let mut previous: Option<u64> = None;
    let index = |pc: u64| ((pc - first) / 4) as usize;
    loop {
        match leader.next_record(Duration::from_secs(30))? {
            Trace::Instruction { core, .. } => {
                if (VECTORS..VECTORS + 0x800).contains(&core.pc) {
                    if core.pc & 0x7f == 4
                        && let Some(pc) = fault.take()
                        && (first..end).contains(&pc)
                    {
                        outcomes.insert(index(pc), Outcome::Fault(core.x[1] as u32));
                    }
                    continue;
                }
                if let Some(pc) = previous
                    && core.pc == pc + 4
                    && (first..end).contains(&pc)
                {
                    outcomes.entry(index(pc)).or_insert(Outcome::Ran(Box::new(core)));
                }
                if core.pc == end {
                    break;
                }
                previous = Some(core.pc);
            }
            Trace::Discontinuity {
                kind: 2, from_pc, ..
            } => fault = Some(from_pc),
            Trace::Discontinuity {
                kind,
                from_pc,
                to_pc,
            } => {
                return Err(format!(
                    "QEMU took a discontinuity of the kind {kind} from {from_pc:#x} to {to_pc:#x}"
                )
                .into());
            }
            Trace::Write { .. } => {}
        }
    }
    leader.stop(true)?;
    if outcomes.len() != body.len() {
        return Err(format!(
            "{} of {} instructions of a sweep have an outcome",
            outcomes.len(),
            body.len()
        )
        .into());
    }
    Ok(outcomes)
}

struct Arguments {
    qemu: String,
    plugin: PathBuf,
    directory: PathBuf,
}

/// The LLVM names of an encoding, or its generic name.
fn name(encoding: u16) -> String {
    let names: Vec<&str> = SYSREGS
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

/// A register that the mask pass must not write all ones or zero to.
fn mask_denied(name: &str) -> bool {
    // A generic name is `S<op0>_...`: a register that LLVM does not know.
    let generic = name.starts_with('S') && name[1..].starts_with(|c: char| c.is_ascii_digit());
    generic
        || ["PM", "RMR", "OSLAR", "DBGDTR", "ICC_", "ICH_", "ICV_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || name.contains("TVAL")
        || name.contains('/')
}

/// What a read of the register gives is no property of the CPU: the count
/// of that moment, or what the probe itself left in an exception register.
fn unstable(name: &str) -> Option<&'static str> {
    match name {
        "CNTPCT_EL0" | "CNTVCT_EL0" | "CNTP_TVAL_EL0" | "CNTV_TVAL_EL0" => Some("time"),
        "SPSR_EL1" | "ELR_EL1" | "ESR_EL1" | "VBAR_EL1" => Some("probe"),
        _ => None,
    }
}

fn read(outcome: &Outcome, register: usize) -> String {
    match outcome {
        Outcome::Ran(core) => format!("{:x}", core.x[register]),
        Outcome::Fault(0x0200_0000) => "u".into(),
        Outcome::Fault(esr) => format!("t{esr:x}"),
    }
}

fn wrote(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ran(_) => "ok".into(),
        Outcome::Fault(0x0200_0000) => "u".into(),
        Outcome::Fault(esr) => format!("t{esr:x}"),
    }
}

fn main() -> Result<(), Failure> {
    let mut qemu = String::from("qemu-system-aarch64");
    let (mut plugin, mut directory, mut out, mut names_out) = (None, None, None, None);
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().ok_or(format!("{argument} needs a value"));
        match argument.as_str() {
            "--qemu" => qemu = value()?,
            "--plugin" => plugin = Some(PathBuf::from(value()?)),
            "--dir" => directory = Some(PathBuf::from(value()?)),
            "--out" => out = Some(value()?),
            "--names-out" => names_out = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let arguments = Arguments {
        qemu,
        plugin: plugin.ok_or("give --plugin <a64lock.so>")?,
        directory: directory.ok_or("give --dir <scratch directory>")?,
    };
    let encodings: Vec<u16> = (FIRST..FIRST + COUNT).map(|value| value as u16).collect();

    // Reads at EL1 and at EL0.
    let reads: Vec<u32> = encodings.iter().map(|encoding| asm::mrs(0, *encoding)).collect();
    let el1_read = phase(&arguments, false, false, &reads, names_out.as_deref())?;
    let el0_read = phase(&arguments, true, false, &reads, None)?;
    // Writes: the value that was read where a read works, else zero.
    let writes = |read: &HashMap<usize, Outcome>| -> Vec<u32> {
        encodings
            .iter()
            .enumerate()
            .flat_map(|(index, encoding)| {
                let first = match &read[&index] {
                    Outcome::Ran(_) => asm::mrs(0, *encoding),
                    Outcome::Fault(_) => asm::movz(0, 0, 0),
                };
                [first, asm::msr(*encoding, 0)]
            })
            .collect()
    };
    let el1_write = phase(&arguments, false, false, &writes(&el1_read), None)?;
    let el0_write = phase(&arguments, true, false, &writes(&el0_read), None)?;
    // Masks, with FP enabled so that FPCR and FPSR are reached.
    let fp_read = phase(&arguments, false, true, &reads, None)?;
    let fp_write = phase(&arguments, false, true, &writes(&fp_read), None)?;
    let masked: Vec<usize> = (0..encodings.len())
        .filter(|index| {
            matches!(fp_read[index], Outcome::Ran(_))
                && matches!(fp_write[&(2 * index + 1)], Outcome::Ran(_))
                && !mask_denied(&name(encodings[*index]))
        })
        .collect();
    const SLOT: usize = 8;
    let mut body = Vec::new();
    for index in &masked {
        let encoding = encodings[*index];
        // All ones, but never SCTLR_EL1.M: no page table exists.
        let ones = if encoding == reg::SCTLR_EL1 {
            asm::eor(0, 0, 5)
        } else {
            asm::NOP
        };
        body.extend([
            asm::mrs(2, encoding),
            0x9280_0000, // movn x0, #0
            ones,
            asm::msr(encoding, 0),
            asm::mrs(3, encoding),
            asm::msr(encoding, 31),
            asm::mrs(4, encoding),
            asm::msr(encoding, 2),
        ]);
    }
    // X5 is 1 for the exclusive-or that keeps SCTLR_EL1.M clear.
    let mut with_setup = vec![asm::movz(5, 1, 0)];
    with_setup.extend(&body);
    let masks = phase(&arguments, false, true, &with_setup, None)?;

    let mut text = String::from(
        "# What QEMU's cortex-a53 on virt answers to MRS and MSR of each register encoding after\n\
         # its reset: el1r and el0r are the value read at EL1 and EL0, el1w and el0w whether a\n\
         # write of that value (or of zero where no read works) runs. u: the Undefined Instruction\n\
         # exception (ESR 0x02000000). t<esr>: another exception. fp: el1r with CPACR_EL1.FPEN\n\
         # set, where it differs. ones and zeros: the value read back after a write of all ones\n\
         # and of zero at EL1. An encoding that is not listed is u in all four. time: a\n\
         # count, which is another one at every read. probe: an exception register that\n\
         # the probe itself had written by then.\n\
         # Made by `just tcg-a64-sysprobe` (examples/a64_sysprobe.rs).\n",
    );
    for (index, encoding) in encodings.iter().enumerate() {
        let name = name(*encoding);
        let stable = |text: String| match (unstable(&name), text.as_str()) {
            (Some(why), text) if text != "u" && !text.starts_with('t') => why.to_string(),
            _ => text,
        };
        let columns = [
            stable(read(&el1_read[&index], 0)),
            wrote(&el1_write[&(2 * index + 1)]),
            read(&el0_read[&index], 0),
            wrote(&el0_write[&(2 * index + 1)]),
        ];
        let fp = stable(read(&fp_read[&index], 0));
        let fp_wrote = wrote(&fp_write[&(2 * index + 1)]);
        if columns.iter().all(|column| column == "u") && fp == "u" && fp_wrote == "u" {
            continue;
        }
        let mut line = format!(
            "{encoding:04x} {name} el1r={} el1w={} el0r={} el0w={}",
            columns[0],
            columns[1],
            columns[2],
            columns[3]
        );
        if fp != columns[0] || fp_wrote != columns[1] {
            line.push_str(&format!(" fp={fp} fpw={fp_wrote}"));
        }
        if let Some(slot) = masked.iter().position(|masked| *masked == index) {
            let at = 1 + slot * SLOT;
            let value = |offset: usize, register: usize| read(&masks[&(at + offset)], register);
            line.push_str(&format!(" ones={} zeros={}", value(4, 3), value(6, 4)));
            for offset in [3, 5, 7] {
                if let Outcome::Fault(esr) = &masks[&(at + offset)] {
                    line.push_str(&format!(" maskfault{offset}={esr:x}"));
                }
            }
        }
        text.push_str(&line);
        text.push('\n');
    }
    text.push_str(
        "# Which bits of PSTATE an exception return takes from SPSR_EL1: for a return to EL1\n\
         # with SP_EL1, to EL0, and for a saved state of EL2, which is illegal. follows: the\n\
         # bits from 5 up whose value in PSTATE after the return is their value in SPSR_EL1.\n\
         # extra: bits that are set after the return and were clear in SPSR_EL1.\n",
    );
    text.push_str(&pstate(&arguments)?);
    match out {
        Some(path) => std::fs::write(&path, text)?,
        None => print!("{text}"),
    }
    Ok(())
}
