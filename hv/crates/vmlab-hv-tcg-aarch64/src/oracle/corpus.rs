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
//! A line that starts with `#` is a comment.

use super::{Core, Vectors};
use crate::exec::Cpu;
use vmlab_hv_tcg::FlatMemory;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub encoding: u32,
    pub before: Core,
    pub after: Core,
    /// The vector and FP registers before and after.
    pub vectors: Option<(Vectors, Vectors)>,
}

/// The vector and FP registers of one side of a line, as far as it gives them.
#[derive(Default)]
struct VectorsText {
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
    pub fn line(&self) -> String {
        let Some((before, after)) = &self.vectors else {
            return format!("{:08x} {} => {}", self.encoding, self.before, self.after);
        };
        let mut line = format!(
            "{:08x} {} {before} => {} fpcr={:x} fpsr={:x}",
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
        let (before, before_vectors) = parse_core(before)?;
        let (after, after_vectors) = parse_core(after)?;
        Ok(Self {
            encoding: u32::from_str_radix(encoding, 16)
                .map_err(|_| format!("`{encoding}` is not an encoding"))?,
            before,
            after,
            vectors: parse_vectors(before_vectors, after_vectors)?,
        })
    }

    /// Run the instruction on our interpreter from `before`, and compare
    /// with `after`.
    pub fn replay(&self) -> Result<(), String> {
        let mut state = self
            .before
            .state()
            .ok_or("the entry does not start at EL1 with SP_EL1")?;
        if let Some((before, _)) = &self.vectors {
            before.apply(&mut state);
        }
        let page = self.before.pc & !0xfff;
        let mut data = vec![0u8; 0x1000];
        let at = (self.before.pc - page) as usize;
        if at + 4 > data.len() {
            return Err("the instruction crosses a page".into());
        }
        data[at..at + 4].copy_from_slice(&self.encoding.to_le_bytes());
        let mut cpu = Cpu::new(FlatMemory::new(page, data), state);
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
