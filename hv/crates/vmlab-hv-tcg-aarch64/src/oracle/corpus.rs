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
//! An instruction that reads or writes memory has more after a `|`, in any
//! order:
//!
//! ```text
//! ... | r=42100000:8877665544332211 w=42100008:00
//! ```
//!
//! `r`: bytes that the memory holds before the instruction, lowest address
//! first. `w`: bytes that the instruction writes. Memory that is not named
//! is zero.
//!
//! A line that starts with `#` is a comment.

use super::{Core, Vectors};
use crate::exec::Cpu;
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
    /// The vector and FP registers before and after.
    pub vectors: Option<(Vectors, Vectors)>,
    /// Memory before the instruction: address and bytes.
    pub reads: Vec<(u64, Vec<u8>)>,
    /// What the instruction writes: address and bytes.
    pub writes: Vec<(u64, Vec<u8>)>,
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
    /// An entry of an instruction that touches no memory and no vector
    /// register.
    pub fn core(encoding: u32, before: Core, after: Core) -> Self {
        Self {
            encoding,
            before,
            after,
            vectors: None,
            reads: Vec::new(),
            writes: Vec::new(),
        }
    }

    /// The registers of the line, without its memory.
    fn registers_line(&self) -> String {
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

    pub fn line(&self) -> String {
        let mut line = self.registers_line();
        if self.reads.is_empty() && self.writes.is_empty() {
            return line;
        }
        line.push_str(" |");
        for (address, bytes) in &self.reads {
            line.push_str(&format!(" r={address:x}:{}", hex(bytes)));
        }
        for (address, bytes) in &self.writes {
            line.push_str(&format!(" w={address:x}:{}", hex(bytes)));
        }
        line
    }

    pub fn parse(line: &str) -> Result<Self, String> {
        let (line, memory) = match line.split_once('|') {
            Some((line, memory)) => (line, memory),
            None => (line, ""),
        };
        let (mut reads, mut writes) = (Vec::new(), Vec::new());
        for part in memory.split_whitespace() {
            match part.split_once('=') {
                Some(("r", value)) => reads.push(parse_memory(value)?),
                Some(("w", value)) => writes.push(parse_memory(value)?),
                _ => {
                    return Err(format!(
                        "`{part}` is not r=address:bytes or w=address:bytes"
                    ));
                }
            }
        }
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
            reads,
            writes,
        })
    }

    /// Run the instruction on our interpreter from `before`, and compare
    /// with `after`: the registers, the vector and FP registers where the
    /// entry has them, and the bytes that the instruction writes.
    pub fn replay(&self) -> Result<(), String> {
        let mut state = self
            .before
            .state()
            .ok_or("the entry does not start at EL1 with SP_EL1")?;
        if let Some((before, _)) = &self.vectors {
            before.apply(&mut state);
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
        if let Some((_, after)) = &self.vectors {
            found.extend(Vectors::of(&cpu.state).difference(after));
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
    fn an_entry_with_memory_goes_to_a_line_and_back_and_replays() {
        let mut before = Core {
            x: [0; 31],
            sp: 0,
            pc: 0x4200_0000,
            cpsr: CPSR_EL1H,
        };
        before.x[0] = 0x4210_0ff0;
        let mut after = before;
        after.pc += 4;
        // stp q1, q2, [x0]: 32 bytes across a page boundary.
        let value = |n: u8| u128::from_le_bytes(core::array::from_fn(|index| n + index as u8));
        let mut store = Entry::core(0xad00_0801, before, after);
        store.writes = vec![
            (0x4210_0ff0, value(0x10).to_le_bytes().to_vec()),
            (0x4210_1000, value(0x20).to_le_bytes().to_vec()),
        ];
        let mut vectors = Vectors::default();
        vectors.v[1] = value(0x10);
        vectors.v[2] = value(0x20);
        store.vectors = Some((vectors, vectors));
        let line = store.line();
        assert!(line.contains(" | w=42100ff0:101112"), "{line}");
        assert!(line.contains(" fpcr=0 fpsr=0 v=0,1f1e1d"), "{line}");
        assert_eq!(Entry::parse(&line), Ok(store.clone()));
        assert_eq!(store.replay(), Ok(()));
        // A wrong byte, and a write that is shorter than the instruction's.
        let mut wrong = store.clone();
        wrong.writes[1].1[0] ^= 1;
        assert!(wrong.replay().unwrap_err().contains("memory writes"));
        let mut wrong = store.clone();
        wrong.writes[1].1.pop();
        assert!(wrong.replay().unwrap_err().contains("memory writes"));
        // ldr w3, [x0]: the memory before is part of the entry.
        let mut after = before;
        after.pc += 4;
        after.x[3] = 0x8877_6655;
        let mut load = Entry::core(0xb940_0003, before, after);
        load.reads = vec![(0x4210_0ff0, vec![0x55, 0x66, 0x77, 0x88])];
        assert_eq!(Entry::parse(&load.line()), Ok(load.clone()));
        assert_eq!(load.replay(), Ok(()));
        let mut wrong = load.clone();
        wrong.after.x[3] = 1;
        assert!(wrong.replay().unwrap_err().contains("x3: 0x88776655 / 0x1"));
        assert!(Entry::parse(&format!("{} | q=1", load.line())).is_err());
        assert!(Entry::parse(&format!("{} | r=42100000:123", load.line())).is_err());
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
