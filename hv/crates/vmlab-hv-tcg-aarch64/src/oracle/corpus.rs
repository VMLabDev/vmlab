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
//! An instruction that reads or writes memory or a vector register has more
//! after a `|`, in any order:
//!
//! ```text
//! ... | r=42100000:8877665544332211 w=42100008:00 v3=<32 hex digits>:<32 hex digits>
//! ```
//!
//! `r`: bytes that the memory holds before the instruction, lowest address
//! first. `w`: bytes that the instruction writes. `v<n>`: a vector register
//! before and after. A vector register that is not named is zero before and
//! after. Memory that is not named is zero.
//!
//! A line that starts with `#` is a comment.

use super::Core;
use crate::exec::Cpu;
use crate::state::slot;
use std::collections::BTreeMap;
use vmlab_hv_tcg::FlatMemory;

/// The most memory that a replay spans, from the lowest address that an
/// entry names to the highest.
const REPLAY_SPAN: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub encoding: u32,
    pub before: Core,
    pub after: Core,
    /// Memory before the instruction: address and bytes.
    pub reads: Vec<(u64, Vec<u8>)>,
    /// What the instruction writes: address and bytes.
    pub writes: Vec<(u64, Vec<u8>)>,
    /// Vector registers: the number, the value before and the value after.
    pub vectors: Vec<(u8, u128, u128)>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return Err(format!("`{text}` is not a run of hex bytes"));
    }
    (0..text.len())
        .step_by(2)
        .map(|at| {
            u8::from_str_radix(&text[at..at + 2], 16)
                .map_err(|_| format!("`{text}` is not a run of hex bytes"))
        })
        .collect()
}

/// `address:bytes`.
fn parse_memory(text: &str) -> Result<(u64, Vec<u8>), String> {
    let (address, bytes) = text
        .split_once(':')
        .ok_or_else(|| format!("`{text}` is not address:bytes"))?;
    Ok((
        u64::from_str_radix(address, 16).map_err(|_| format!("`{address}` is not an address"))?,
        unhex(bytes)?,
    ))
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
    /// An entry of an instruction that touches no memory and no vector
    /// register.
    pub fn core(encoding: u32, before: Core, after: Core) -> Self {
        Self {
            encoding,
            before,
            after,
            reads: Vec::new(),
            writes: Vec::new(),
            vectors: Vec::new(),
        }
    }

    pub fn line(&self) -> String {
        let mut line = format!("{:08x} {} => {}", self.encoding, self.before, self.after);
        if self.reads.is_empty() && self.writes.is_empty() && self.vectors.is_empty() {
            return line;
        }
        line.push_str(" |");
        for (address, bytes) in &self.reads {
            line.push_str(&format!(" r={address:x}:{}", hex(bytes)));
        }
        for (address, bytes) in &self.writes {
            line.push_str(&format!(" w={address:x}:{}", hex(bytes)));
        }
        for (n, before, after) in &self.vectors {
            line.push_str(&format!(" v{n}={before:032x}:{after:032x}"));
        }
        line
    }

    pub fn parse(line: &str) -> Result<Self, String> {
        let (line, more) = match line.split_once('|') {
            Some((line, more)) => (line, more),
            None => (line, ""),
        };
        let (encoding, rest) = line
            .trim()
            .split_once(' ')
            .ok_or("an entry starts with its encoding")?;
        let (before, after) = rest
            .split_once("=>")
            .ok_or("an entry has `=>` between its two states")?;
        let mut entry = Self::core(
            u32::from_str_radix(encoding, 16)
                .map_err(|_| format!("`{encoding}` is not an encoding"))?,
            parse_core(before)?,
            parse_core(after)?,
        );
        for part in more.split_whitespace() {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| format!("`{part}` is not key=value"))?;
            match key {
                "r" => entry.reads.push(parse_memory(value)?),
                "w" => entry.writes.push(parse_memory(value)?),
                _ => {
                    let n: u8 = key
                        .strip_prefix('v')
                        .and_then(|n| n.parse().ok())
                        .filter(|n| *n < 32)
                        .ok_or_else(|| format!("`{key}` is not r, w or a vector register"))?;
                    let (before, after) = value
                        .split_once(':')
                        .ok_or_else(|| format!("`{value}` is not before:after"))?;
                    let number = |text: &str| {
                        u128::from_str_radix(text, 16)
                            .map_err(|_| format!("`{text}` is not a hex number"))
                    };
                    entry.vectors.push((n, number(before)?, number(after)?));
                }
            }
        }
        Ok(entry)
    }

    /// Run the instruction on our interpreter from `before`, and compare
    /// with `after`: the registers, the vector registers and the bytes that
    /// it writes.
    pub fn replay(&self) -> Result<(), String> {
        let mut state = self
            .before
            .state()
            .ok_or("the entry does not start at EL1 with SP_EL1")?;
        for (n, before, _) in &self.vectors {
            state.slots[slot::V0 + usize::from(*n)] = *before;
        }
        // One memory from the page of the lowest address that the entry
        // names to the page of the highest.
        if self.before.pc & 0xfff > 0xffc {
            return Err("the instruction crosses a page".into());
        }
        let page = self.before.pc & !0xfff;
        let mut low = page;
        let mut high = page
            .checked_add(0x1000)
            .ok_or("an instruction at the end of the address space")?;
        for (address, bytes) in self.reads.iter().chain(&self.writes) {
            low = low.min(*address & !0xfff);
            let end = address
                .checked_add(bytes.len() as u64 + 0xfff)
                .ok_or("an access at the end of the address space")?
                & !0xfff;
            high = high.max(end);
        }
        if high - low > REPLAY_SPAN {
            return Err(format!(
                "the entry spans {:#x} bytes of memory, more than a replay holds",
                high - low
            ));
        }
        let mut data = vec![0u8; (high - low) as usize];
        for (address, bytes) in &self.reads {
            let at = (address - low) as usize;
            data[at..at + bytes.len()].copy_from_slice(bytes);
        }
        let at = (self.before.pc - low) as usize;
        data[at..at + 4].copy_from_slice(&self.encoding.to_le_bytes());
        let mut cpu = Cpu::new(FlatMemory::new(low, data), state);
        cpu.log_accesses(true);
        cpu.step()
            .map_err(|stop| format!("{:08x}: our interpreter stops: {stop:?}", self.encoding))?;
        let mut found = Core::of(&cpu.state).difference(&self.after);
        for n in 0..32u8 {
            let expected = self
                .vectors
                .iter()
                .find(|(register, _, _)| *register == n)
                .map_or(0, |(_, _, after)| *after);
            let ours = cpu.state.slots[slot::V0 + usize::from(n)];
            if ours != expected {
                found.push(format!("v{n}: {ours:#x} / {expected:#x}"));
            }
        }
        let bytes = |writes: &mut dyn Iterator<Item = (u64, Vec<u8>)>| {
            let mut map = BTreeMap::new();
            for (address, data) in writes {
                for (index, byte) in data.into_iter().enumerate() {
                    map.insert(address + index as u64, byte);
                }
            }
            map
        };
        let ours = bytes(
            &mut cpu
                .take_accesses()
                .into_iter()
                .filter(|access| access.write)
                .map(|access| {
                    (
                        access.address,
                        access.value.to_le_bytes()[..usize::from(access.bytes)].to_vec(),
                    )
                }),
        );
        let expected = bytes(&mut self.writes.iter().cloned());
        if ours != expected {
            found.push(format!(
                "memory writes: {} bytes / {} bytes, or other values",
                ours.len(),
                expected.len()
            ));
        }
        if !found.is_empty() {
            return Err(format!("{:08x}: {}", self.encoding, found.join("; ")));
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
        Entry::core(0x9100_0400, before, after)
    }

    #[test]
    fn an_entry_with_memory_and_vector_registers_goes_to_a_line_and_back_and_replays() {
        let mut before = Core {
            x: [0; 31],
            sp: 0,
            pc: 0x4200_0000,
            cpsr: CPSR_EL1H,
        };
        before.x[0] = 0x4210_0ff8;
        let mut after = before;
        after.pc += 4;
        // stp q1, q2, [x0]: 32 bytes across a page boundary.
        let value = |n: u8| u128::from_le_bytes(core::array::from_fn(|index| n + index as u8));
        let mut store = Entry::core(0xad00_0801, before, after);
        store.writes = vec![
            (0x4210_0ff8, value(0x10).to_le_bytes().to_vec()),
            (0x4210_1008, value(0x20).to_le_bytes().to_vec()),
        ];
        store.vectors = vec![(1, value(0x10), value(0x10)), (2, value(0x20), value(0x20))];
        let line = store.line();
        assert!(line.contains(" | w=42100ff8:101112"), "{line}");
        assert!(line.contains(" v2=2f2e2d"), "{line}");
        assert_eq!(Entry::parse(&line), Ok(store.clone()));
        assert_eq!(store.replay(), Ok(()));
        // A wrong byte, and a write that the entry does not have.
        let mut wrong = store.clone();
        wrong.writes[1].1[0] ^= 1;
        assert!(wrong.replay().unwrap_err().contains("memory writes"));
        let mut wrong = store.clone();
        wrong.writes.pop();
        assert!(wrong.replay().unwrap_err().contains("memory writes"));
        // ldr w3, [x0]: the memory before is part of the entry.
        let mut after = before;
        after.pc += 4;
        after.x[3] = 0x8877_6655;
        let mut load = Entry::core(0xb940_0003, before, after);
        load.reads = vec![(0x4210_0ff8, vec![0x55, 0x66, 0x77, 0x88])];
        assert_eq!(Entry::parse(&load.line()), Ok(load.clone()));
        assert_eq!(load.replay(), Ok(()));
        let mut wrong = load.clone();
        wrong.vectors.push((5, 0, 1));
        assert!(wrong.replay().unwrap_err().contains("v5: 0x0 / 0x1"));
        assert!(Entry::parse(&format!("{} | q=1", load.line())).is_err());
        assert!(Entry::parse(&format!("{} | v32=0:0", load.line())).is_err());
        assert!(Entry::parse(&format!("{} | r=42100000:123", load.line())).is_err());
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
