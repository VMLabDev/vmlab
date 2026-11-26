//! The instruction lockstep against QEMU (DESIGN.md O3).
//!
//! A `qemu-system-aarch64` process is the oracle. It runs with the plugin
//! `plugin/a64lock.c`, which writes one record before each instruction. The
//! harness loads an image and a register state into QEMU through its gdbstub
//! (`rsp`), lets it run (`leader`), and steps our interpreter beside the
//! records (`follow`). The images come from the stream generator (`stream`).
//! `corpus` keeps instructions with the states that QEMU gave, so that
//! `just ci::check` replays them without QEMU.
//!
//! QEMU is a black box here: its register values, its exceptions and its
//! gdbstub answers (S6). Nothing in this module is part of a product path.

pub mod corpus;
pub mod follow;
pub mod leader;
pub mod rsp;
pub mod stream;

use crate::state::State;

/// CPSR of the state that every round starts from, without the flags: EL1
/// with SP_EL1, and D, A, I and F set. QEMU's `virt` resets to it.
pub const CPSR_EL1H: u32 = 0x3c5;

/// The core registers as the gdbstub and the plugin give them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Core {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub cpsr: u32,
}

impl Core {
    pub fn of(state: &State) -> Self {
        let mut x = [0; 31];
        for (n, value) in x.iter_mut().enumerate() {
            *value = state.x(n);
        }
        Self {
            x,
            sp: state.sp(),
            pc: state.pc(),
            cpsr: state.cpsr(),
        }
    }

    /// Our state with these registers. The level, the stack pointer choice
    /// and the interrupt masks must be those of `CPSR_EL1H`.
    pub fn state(&self) -> Option<State> {
        if self.cpsr & 0x0fff_ffff != CPSR_EL1H {
            return None;
        }
        let mut state = State::reset(self.pc);
        for (n, value) in self.x.iter().enumerate() {
            state.set_x(n, *value);
        }
        state.set_sp(self.sp);
        state.set_nzcv((self.cpsr >> 28) as u8);
        Some(state)
    }

    /// The names of the registers that differ, with both values.
    pub fn difference(&self, other: &Self) -> Vec<String> {
        let mut found = Vec::new();
        for n in 0..31 {
            if self.x[n] != other.x[n] {
                found.push(format!("x{n}: {:#x} / {:#x}", self.x[n], other.x[n]));
            }
        }
        if self.sp != other.sp {
            found.push(format!("sp: {:#x} / {:#x}", self.sp, other.sp));
        }
        if self.pc != other.pc {
            found.push(format!("pc: {:#x} / {:#x}", self.pc, other.pc));
        }
        if self.cpsr != other.cpsr {
            found.push(format!("cpsr: {:#x} / {:#x}", self.cpsr, other.cpsr));
        }
        found
    }
}

impl std::fmt::Display for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pc={:x} cpsr={:x} sp={:x} x=", self.pc, self.cpsr, self.sp)?;
        for (n, value) in self.x.iter().enumerate() {
            write!(f, "{}{value:x}", if n == 0 { "" } else { "," })?;
        }
        Ok(())
    }
}

/// One record of the plugin stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trace {
    /// The registers before the instruction `encoding` at `core.pc` runs.
    Instruction { encoding: u32, core: Core },
    /// A memory write of the last instruction.
    Write { address: u64, size: u32, low: u64, high: u64 },
    /// An exception (kind 2), an interrupt (1) or a host call (4).
    Discontinuity { kind: u32, from_pc: u64, to_pc: u64 },
}

/// The first bytes of the plugin stream.
pub const STREAM_MAGIC: &[u8; 8] = b"A64LOCK1";
pub const STREAM_HEADER_BYTES: usize = 16;

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32_at(bytes, at)) | u64::from(u32_at(bytes, at + 4)) << 32
}

/// Parse the record at the start of `bytes`. `Ok(None)`: the record is not
/// whole yet. Otherwise the record and its length. `vectors`: the stream
/// carries V0 to V31, FPSR and FPCR in each instruction record. They are
/// skipped until a card compares them.
pub fn parse(bytes: &[u8], vectors: bool) -> Result<Option<(Trace, usize)>, String> {
    if bytes.len() < 8 {
        return Ok(None);
    }
    let tag = u32_at(bytes, 0);
    let second = u32_at(bytes, 4);
    match tag {
        1 => {
            let len = 8 + 34 * 8 + if vectors { 32 * 16 + 16 } else { 0 };
            if bytes.len() < len {
                return Ok(None);
            }
            let mut x = [0; 31];
            for (n, value) in x.iter_mut().enumerate() {
                *value = u64_at(bytes, 8 + n * 8);
            }
            let core = Core {
                x,
                sp: u64_at(bytes, 8 + 31 * 8),
                pc: u64_at(bytes, 8 + 32 * 8),
                cpsr: u64_at(bytes, 8 + 33 * 8) as u32,
            };
            Ok(Some((
                Trace::Instruction {
                    encoding: second,
                    core,
                },
                len,
            )))
        }
        2 => {
            if bytes.len() < 32 {
                return Ok(None);
            }
            Ok(Some((
                Trace::Write {
                    size: second,
                    address: u64_at(bytes, 8),
                    low: u64_at(bytes, 16),
                    high: u64_at(bytes, 24),
                },
                32,
            )))
        }
        3 => {
            if bytes.len() < 24 {
                return Ok(None);
            }
            Ok(Some((
                Trace::Discontinuity {
                    kind: second,
                    from_pc: u64_at(bytes, 8),
                    to_pc: u64_at(bytes, 16),
                },
                24,
            )))
        }
        other => Err(format!("the plugin stream has a record with the tag {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_parse_whole_or_not_at_all() {
        let mut bytes = Vec::new();
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(0x9100_0400u32.to_le_bytes());
        for n in 0..31u64 {
            bytes.extend((n * 3).to_le_bytes());
        }
        bytes.extend(0x1000u64.to_le_bytes());
        bytes.extend(0x4200_0000u64.to_le_bytes());
        bytes.extend(0x6000_03c5u64.to_le_bytes());
        for cut in 0..bytes.len() {
            assert_eq!(parse(&bytes[..cut], false), Ok(None));
        }
        let (trace, len) = parse(&bytes, false).unwrap().unwrap();
        assert_eq!(len, bytes.len());
        let Trace::Instruction { encoding, core } = trace else {
            panic!("an instruction record");
        };
        assert_eq!(encoding, 0x9100_0400);
        assert_eq!(core.x[30], 90);
        assert_eq!((core.sp, core.pc, core.cpsr), (0x1000, 0x4200_0000, 0x6000_03c5));
        // With vectors the record is longer, and the same core comes out.
        assert_eq!(parse(&bytes, true), Ok(None));
        bytes.extend([0u8; 32 * 16 + 16]);
        assert_eq!(parse(&bytes, true).unwrap().unwrap().1, bytes.len());

        let mut discontinuity = Vec::new();
        discontinuity.extend(3u32.to_le_bytes());
        discontinuity.extend(2u32.to_le_bytes());
        discontinuity.extend(0x4200_0004u64.to_le_bytes());
        discontinuity.extend(0x200u64.to_le_bytes());
        assert_eq!(
            parse(&discontinuity, false),
            Ok(Some((
                Trace::Discontinuity {
                    kind: 2,
                    from_pc: 0x4200_0004,
                    to_pc: 0x200
                },
                24
            )))
        );
        assert!(parse(&[9, 0, 0, 0, 0, 0, 0, 0], false).is_err());
    }

    #[test]
    fn a_core_goes_to_a_state_and_back() {
        let mut core = Core {
            x: [0; 31],
            sp: 0x8000,
            pc: 0x4200_0010,
            cpsr: 0x9000_03c5,
        };
        core.x[7] = 77;
        let state = core.state().unwrap();
        assert_eq!(Core::of(&state), core);
        assert_eq!(state.nzcv(), 0b1001);
        // Another level is not a state that the harness can start from.
        core.cpsr = 0x3c0;
        assert!(core.state().is_none());
        let other = Core { pc: 0, ..core };
        assert_eq!(core.difference(&other), vec!["pc: 0x42000010 / 0x0".to_string()]);
    }
}
