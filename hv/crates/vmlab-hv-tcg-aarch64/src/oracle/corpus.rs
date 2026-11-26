//! The regression corpus (DESIGN.md O3, step 5): instructions with the
//! registers that QEMU had before and after them. `just ci::check` replays
//! the committed corpus on our interpreter alone, without QEMU.
//!
//! One entry is one line:
//!
//! ```text
//! 91000400 pc=42000000 cpsr=3c5 sp=0 x=0,...,0 => pc=42000004 cpsr=3c5 sp=0 x=1,...,0
//! ```
//!
//! An entry of a group that compares the vector and FP registers has them
//! after the general ones, on both sides: `fpcr=`, `fpsr=` and `v=` with V0
//! to V31. The state after the instruction has `dv=` in place of `v=`, with
//! only the registers that the instruction changed, as `number:value`.
//!
//! An entry of a group that compares system registers (card C6) has
//! `sys=` before the arrow: the compared registers that do not hold their
//! reset value, as `NAME:value` (or `-` for none). After the arrow `dsys=`
//! has those that the instruction changed. CPSR can then name EL0 or EL1
//! with either stack pointer.
//!
//! A line that starts with `#` is a comment.

use super::follow::ROUND_COUNT;
use super::{Compared, Core, Vectors, apply_system, compared, system_of};
use crate::exec::Cpu;
use crate::state::State;
use crate::sysreg::QEMU_CORTEX_A53;
use crate::timer::Clock;
use vmlab_hv_tcg::FlatMemory;

/// System registers by the name of the compared list, with their values.
pub type Named = Vec<(String, u64)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub encoding: u32,
    pub before: Core,
    pub after: Core,
    /// The vector and FP registers before and after.
    pub vectors: Option<(Vectors, Vectors)>,
    /// The compared system registers that do not hold their reset value
    /// before the instruction, and those that it changed.
    pub system: Option<(Named, Named)>,
}

/// The values of the compared registers after a reset.
fn reset_system(compared: &[Compared]) -> Vec<u64> {
    system_of(&State::reset_as(0, &QEMU_CORTEX_A53), compared)
}

/// The registers of `values` that differ from `base`, by name.
fn named(compared: &[Compared], base: &[u64], values: &[u64]) -> Named {
    compared
        .iter()
        .zip(base.iter().zip(values))
        .filter(|(_, (base, value))| base != value)
        .map(|(register, (_, value))| (register.qemu.to_string(), *value))
        .collect()
}

/// `base` with the registers of `named` set.
fn with_named(compared: &[Compared], base: &[u64], named: &Named) -> Result<Vec<u64>, String> {
    let mut values = base.to_vec();
    for (name, value) in named {
        let at = compared
            .iter()
            .position(|register| register.qemu == name)
            .ok_or_else(|| format!("`{name}` is not a compared system register"))?;
        values[at] = *value;
    }
    Ok(values)
}

fn named_text(key: &str, named: &Named) -> String {
    if named.is_empty() {
        return format!("{key}=-");
    }
    let parts: Vec<String> = named
        .iter()
        .map(|(name, value)| format!("{name}:{value:x}"))
        .collect();
    format!("{key}={}", parts.join(","))
}

fn parse_named(text: &str) -> Result<Named, String> {
    if text == "-" {
        return Ok(Vec::new());
    }
    text.split(',')
        .map(|part| {
            let (name, value) = part
                .split_once(':')
                .ok_or_else(|| format!("`{part}` is not NAME:value"))?;
            let value = u64::from_str_radix(value, 16)
                .map_err(|_| format!("`{value}` is not a hex number"))?;
            Ok((name.to_string(), value))
        })
        .collect()
}

/// The vector and FP registers of one side of a line, as far as it gives them.
#[derive(Default)]
struct VectorsText {
    /// `sys=` or `dsys=`.
    system: Option<Named>,
    fpcr: Option<u32>,
    fpsr: Option<u32>,
    /// `v=`: all 32 registers.
    all: Option<[u128; 32]>,
    /// `dv=`: the registers that changed.
    changed: Vec<(usize, u128)>,
}

fn wide(text: &str) -> Result<u128, String> {
    u128::from_str_radix(text, 16).map_err(|_| format!("`{text}` is not a hex number"))
}

fn parse_core(text: &str) -> Result<(Core, VectorsText), String> {
    let mut vectors = VectorsText::default();
    let mut core = Core {
        x: [0; 31],
        sp: 0,
        pc: 0,
        cpsr: 0,
    };
    let mut seen = 0;
    for part in text.split_whitespace() {
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| format!("`{part}` is not key=value"))?;
        let number = |text: &str| {
            u64::from_str_radix(text, 16).map_err(|_| format!("`{text}` is not a hex number"))
        };
        match key {
            "pc" => core.pc = number(value)?,
            "sp" => core.sp = number(value)?,
            "cpsr" => {
                core.cpsr = u32::try_from(number(value)?).map_err(|_| "cpsr has 32 bits")?;
            }
            "x" => {
                let values: Vec<&str> = value.split(',').collect();
                if values.len() != 31 {
                    return Err(format!("{} X registers, not 31", values.len()));
                }
                for (n, value) in values.iter().enumerate() {
                    core.x[n] = number(value)?;
                }
            }
            "fpcr" => {
                vectors.fpcr = Some(u32::try_from(number(value)?).map_err(|_| "fpcr has 32 bits")?);
                continue;
            }
            "fpsr" => {
                vectors.fpsr = Some(u32::try_from(number(value)?).map_err(|_| "fpsr has 32 bits")?);
                continue;
            }
            "v" => {
                let values: Vec<&str> = value.split(',').collect();
                if values.len() != 32 {
                    return Err(format!("{} V registers, not 32", values.len()));
                }
                let mut all = [0u128; 32];
                for (n, value) in values.iter().enumerate() {
                    all[n] = wide(value)?;
                }
                vectors.all = Some(all);
                continue;
            }
            "sys" | "dsys" => {
                vectors.system = Some(parse_named(value)?);
                continue;
            }
            "dv" => {
                for part in value.split(',') {
                    let (n, value) = part
                        .split_once(':')
                        .ok_or_else(|| format!("`{part}` is not number:value"))?;
                    let n: usize = n.parse().map_err(|_| format!("`{n}` is not a number"))?;
                    if n >= 32 {
                        return Err(format!("V{n} does not exist"));
                    }
                    vectors.changed.push((n, wide(value)?));
                }
                continue;
            }
            other => return Err(format!("the register `{other}` is not known")),
        }
        seen += 1;
    }
    if seen != 4 {
        return Err("a state is pc, cpsr, sp and x".into());
    }
    Ok((core, vectors))
}

/// The vector and FP registers of both sides of a line. `None`: the line
/// has none.
fn parse_vectors(
    before: VectorsText,
    after: VectorsText,
) -> Result<Option<(Vectors, Vectors)>, String> {
    let (Some(fpcr), Some(fpsr), Some(v)) = (before.fpcr, before.fpsr, before.all) else {
        if before.fpcr.is_some() || before.fpsr.is_some() || before.all.is_some() {
            return Err("the state before has fpcr, fpsr and v, or none of them".into());
        }
        if after.fpcr.is_some() || after.fpsr.is_some() || !after.changed.is_empty() {
            return Err("the state after has vector registers and the state before none".into());
        }
        return Ok(None);
    };
    if !before.changed.is_empty() || after.all.is_some() {
        return Err("the state before has v, and the state after dv".into());
    }
    let before = Vectors { v, fpsr, fpcr };
    let mut after_vectors = Vectors {
        v,
        fpsr: after.fpsr.ok_or("the state after has no fpsr")?,
        fpcr: after.fpcr.ok_or("the state after has no fpcr")?,
    };
    for (n, value) in after.changed {
        after_vectors.v[n] = value;
    }
    Ok(Some((before, after_vectors)))
}

impl Entry {
    /// The entry of an instruction that the lockstep verified.
    pub fn of(verified: &super::follow::Verified, compared: &[Compared]) -> Self {
        let reset = reset_system(compared);
        Self {
            encoding: verified.encoding,
            before: verified.before,
            after: verified.after,
            vectors: verified.vectors,
            system: verified.system.as_ref().map(|(before, after)| {
                (
                    named(compared, &reset, before),
                    named(compared, before, after),
                )
            }),
        }
    }

    pub fn line(&self) -> String {
        let (sys, dsys) = match &self.system {
            Some((before, after)) => (
                format!(" {}", named_text("sys", before)),
                format!(" {}", named_text("dsys", after)),
            ),
            None => (String::new(), String::new()),
        };
        let Some((before, after)) = &self.vectors else {
            return format!(
                "{:08x} {}{sys} => {}{dsys}",
                self.encoding, self.before, self.after
            );
        };
        let mut line = format!(
            "{:08x} {} {before}{sys} => {} fpcr={:x} fpsr={:x}",
            self.encoding, self.before, self.after, after.fpcr, after.fpsr
        );
        let changed: Vec<String> = (0..32)
            .filter(|n| before.v[*n] != after.v[*n])
            .map(|n| format!("{n}:{:x}", after.v[n]))
            .collect();
        if !changed.is_empty() {
            line.push_str(" dv=");
            line.push_str(&changed.join(","));
        }
        line.push_str(&dsys);
        line
    }

    pub fn parse(line: &str) -> Result<Self, String> {
        let (encoding, rest) = line
            .trim()
            .split_once(' ')
            .ok_or("an entry starts with its encoding")?;
        let (before, after) = rest
            .split_once("=>")
            .ok_or("an entry has `=>` between its two states")?;
        let (before, mut before_vectors) = parse_core(before)?;
        let (after, mut after_vectors) = parse_core(after)?;
        let system = match (before_vectors.system.take(), after_vectors.system.take()) {
            (Some(before), Some(after)) => Some((before, after)),
            (None, None) => None,
            _ => return Err("an entry has sys before the arrow and dsys after it, or neither".into()),
        };
        Ok(Self {
            encoding: u32::from_str_radix(encoding, 16)
                .map_err(|_| format!("`{encoding}` is not an encoding"))?,
            before,
            after,
            vectors: parse_vectors(before_vectors, after_vectors)?,
            system,
        })
    }

    /// Run the instruction on our interpreter from `before`, and compare
    /// with `after`. Our CPU is the CPU of the lockstep after its reset,
    /// with the registers of the entry.
    pub fn replay(&self) -> Result<(), String> {
        let compared = compared();
        let reset = reset_system(&compared);
        let mut state = State::reset_as(self.before.pc, &QEMU_CORTEX_A53);
        if let Some((before, _)) = &self.system {
            let values = with_named(&compared, &reset, before)?;
            apply_system(&mut state, &compared, &values);
        }
        if !self.before.apply(&mut state) {
            return Err("the entry starts in a state that the CPU does not have".into());
        }
        // With the stack pointer of the core registers in its slot.
        let before_system = self
            .system
            .is_some()
            .then(|| system_of(&state, &compared));
        if let Some((before, _)) = &self.vectors {
            before.apply(&mut state);
            // An entry without system registers is of a group that ran with
            // FP enabled.
            if self.system.is_none() {
                state.enable_fp();
            }
        }
        let page = self.before.pc & !0xfff;
        let mut data = vec![0u8; 0x1000];
        let at = (self.before.pc - page) as usize;
        if at + 4 > data.len() {
            return Err("the instruction crosses a page".into());
        }
        data[at..at + 4].copy_from_slice(&self.encoding.to_le_bytes());
        let mut cpu = Cpu::new(FlatMemory::new(page, data), state);
        cpu.clock = Clock::Manual(ROUND_COUNT);
        cpu.step()
            .map_err(|stop| format!("{:08x}: our interpreter stops: {stop:?}", self.encoding))?;
        let ours = Core::of(&cpu.state);
        if ours != self.after {
            return Err(format!(
                "{:08x}: {}",
                self.encoding,
                ours.difference(&self.after).join("; ")
            ));
        }
        if let Some((_, after)) = &self.vectors {
            let ours = Vectors::of(&cpu.state);
            if ours != *after {
                return Err(format!(
                    "{:08x}: {}",
                    self.encoding,
                    ours.difference(after).join("; ")
                ));
            }
        }
        if let (Some(before), Some((_, changed))) = (before_system, &self.system) {
            let expected = with_named(&compared, &before, changed)?;
            let ours = system_of(&cpu.state, &compared);
            if ours != expected {
                return Err(format!(
                    "{:08x}: {}",
                    self.encoding,
                    super::system_difference(&compared, &ours, &expected).join("; ")
                ));
            }
        }
        Ok(())
    }
}

/// The entries of a corpus file.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|(number, line)| {
            Entry::parse(line).map_err(|error| format!("line {}: {error}", number + 1))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::CPSR_EL1H;

    fn entry() -> Entry {
        let mut before = Core {
            x: [0; 31],
            sp: 0x10,
            pc: 0x4200_0ffc,
            cpsr: CPSR_EL1H | 0x2000_0000,
        };
        before.x[0] = 41;
        let mut after = before;
        after.x[0] = 42;
        after.pc += 4;
        // add x0, x0, #1
        Entry {
            encoding: 0x9100_0400,
            before,
            after,
            vectors: None,
            system: None,
        }
    }

    #[test]
    fn an_entry_with_vector_registers_goes_to_a_line_and_back_and_replays() {
        // fadd d0, d1, d2 with 1.5 + 2.25, and V0 full of ones before it.
        let mut entry = entry();
        entry.encoding = 0x1e62_2820;
        entry.after.x[0] = entry.before.x[0];
        let mut before = Vectors {
            fpcr: 3 << 22,
            fpsr: 1,
            ..Vectors::default()
        };
        before.v[0] = u128::MAX;
        before.v[1] = u128::from(1.5f64.to_bits());
        before.v[2] = u128::from(2.25f64.to_bits());
        let mut after = before;
        after.v[0] = u128::from(3.75f64.to_bits());
        entry.vectors = Some((before, after));
        let line = entry.line();
        assert!(
            line.contains(
                " fpcr=c00000 fpsr=1 v=ffffffffffffffffffffffffffffffff,3ff8000000000000,"
            )
        );
        assert!(
            line.ends_with(" fpcr=c00000 fpsr=1 dv=0:400e000000000000"),
            "{line}"
        );
        assert_eq!(Entry::parse(&line), Ok(entry.clone()));
        assert_eq!(entry.replay(), Ok(()));
        // A wrong expectation names the register.
        let mut wrong = entry.clone();
        wrong.vectors = Some((before, before));
        let error = wrong.replay().unwrap_err();
        assert!(
            error.contains("v0: 0x400e000000000000 / 0xffffffffffffffffffffffffffffffff"),
            "{error}"
        );
        let mut flags = entry.clone();
        after.fpsr = 0x11;
        flags.vectors = Some((before, after));
        assert!(flags.replay().unwrap_err().contains("fpsr: 0x1 / 0x11"));
        // An instruction that changes no vector register has no dv.
        entry.vectors = Some((before, before));
        assert!(entry.line().ends_with(" fpcr=c00000 fpsr=1"));
        assert_eq!(Entry::parse(&entry.line()), Ok(entry.clone()));
        // Half of the vector registers is not a state.
        assert!(Entry::parse(&line.replace(" fpsr=1 v=", " v=")).is_err());
        assert!(Entry::parse(&line.replace("dv=0:", "dv=40:")).is_err());
    }

    #[test]
    fn an_entry_with_system_registers_replays_an_exception_from_el0() {
        // svc #0x21 at EL0, with the vectors at 0x42080000.
        let mut before = Core {
            x: [0; 31],
            sp: 0x1000,
            pc: 0x4200_0010,
            cpsr: 0x6000_0000,
        };
        before.x[3] = 7;
        let mut after = before;
        after.pc = 0x4208_0400;
        after.cpsr = 0x6000_03c5;
        // SP_EL1 is the stack pointer after the entry.
        after.sp = 0x9000;
        let entry = Entry {
            encoding: 0xd400_0421,
            before,
            after,
            vectors: None,
            system: Some((
                vec![
                    ("VBAR_EL1".to_string(), 0x4208_0000),
                    ("SP_EL1".to_string(), 0x9000),
                    ("SP_EL0".to_string(), 0x1000),
                ],
                vec![
                    ("SPSR_EL1".to_string(), 0x6000_0000),
                    ("ELR_EL1".to_string(), 0x4200_0014),
                    ("ESR_EL1".to_string(), 0x5600_0021),
                ],
            )),
        };
        let line = entry.line();
        assert!(
            line.contains(" sys=VBAR_EL1:42080000,SP_EL1:9000,SP_EL0:1000 => "),
            "{line}"
        );
        assert!(
            line.ends_with(" dsys=SPSR_EL1:60000000,ELR_EL1:42000014,ESR_EL1:56000021"),
            "{line}"
        );
        assert_eq!(Entry::parse(&line), Ok(entry.clone()));
        assert_eq!(entry.replay(), Ok(()));
        // A wrong syndrome names the register.
        let mut wrong = entry.clone();
        wrong.system.as_mut().unwrap().1[2].1 = 0x5600_0022;
        let error = wrong.replay().unwrap_err();
        assert!(error.contains("ESR_EL1: 0x56000021 / 0x56000022"), "{error}");
        // No register differs from its reset value, and none changes.
        let mut plain = entry();
        plain.system = Some((Vec::new(), Vec::new()));
        assert!(plain.line().contains(" sys=- => ") && plain.line().ends_with(" dsys=-"));
        assert_eq!(Entry::parse(&plain.line()), Ok(plain.clone()));
        assert_eq!(plain.replay(), Ok(()));
        assert!(Entry::parse(&plain.line().replace(" dsys=-", "")).is_err());
        assert!(
            Entry::parse(&line.replace("VBAR_EL1", "NO_SUCH"))
                .unwrap()
                .replay()
                .is_err()
        );
    }

    #[test]
    fn an_entry_goes_to_a_line_and_back_and_replays() {
        let entry = entry();
        let line = entry.line();
        assert!(line.starts_with("91000400 pc=42000ffc cpsr=200003c5 sp=10 x=29,0,"));
        assert_eq!(Entry::parse(&line), Ok(entry.clone()));
        assert_eq!(
            parse(&format!("# a comment\n\n{line}\n")),
            Ok(vec![entry.clone()])
        );
        assert_eq!(entry.replay(), Ok(()));
    }

    #[test]
    fn a_wrong_expectation_names_the_register() {
        let mut entry = entry();
        entry.after.x[0] = 43;
        let error = entry.replay().unwrap_err();
        assert!(error.contains("x0: 0x2a / 0x2b"), "{error}");
        assert!(parse("zz pc=0 => pc=0").unwrap_err().starts_with("line 1"));
        assert!(Entry::parse("91000400 pc=0 cpsr=3c5 sp=0 x=0 => pc=0").is_err());
    }
}
