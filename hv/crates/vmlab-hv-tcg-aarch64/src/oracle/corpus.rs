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
//! A line that starts with `#` is a comment.

use super::Core;
use crate::exec::Cpu;
use vmlab_hv_tcg::FlatMemory;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub encoding: u32,
    pub before: Core,
    pub after: Core,
}

fn parse_core(text: &str) -> Result<Core, String> {
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
            other => return Err(format!("the register `{other}` is not known")),
        }
        seen += 1;
    }
    if seen != 4 {
        return Err("a state is pc, cpsr, sp and x".into());
    }
    Ok(core)
}

impl Entry {
    pub fn line(&self) -> String {
        format!("{:08x} {} => {}", self.encoding, self.before, self.after)
    }

    pub fn parse(line: &str) -> Result<Self, String> {
        let (encoding, rest) = line
            .trim()
            .split_once(' ')
            .ok_or("an entry starts with its encoding")?;
        let (before, after) = rest
            .split_once("=>")
            .ok_or("an entry has `=>` between its two states")?;
        Ok(Self {
            encoding: u32::from_str_radix(encoding, 16)
                .map_err(|_| format!("`{encoding}` is not an encoding"))?,
            before: parse_core(before)?,
            after: parse_core(after)?,
        })
    }

    /// Run the instruction on our interpreter from `before`, and compare
    /// with `after`.
    pub fn replay(&self) -> Result<(), String> {
        let state = self
            .before
            .state()
            .ok_or("the entry does not start at EL1 with SP_EL1")?;
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
        }
    }

    #[test]
    fn an_entry_goes_to_a_line_and_back_and_replays() {
        let entry = entry();
        let line = entry.line();
        assert!(line.starts_with("91000400 pc=42000ffc cpsr=200003c5 sp=10 x=29,0,"));
        assert_eq!(Entry::parse(&line), Ok(entry.clone()));
        assert_eq!(parse(&format!("# a comment\n\n{line}\n")), Ok(vec![entry.clone()]));
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
