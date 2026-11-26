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

pub mod asm;
pub mod corpus;
pub mod follow;
pub mod leader;
pub mod rsp;
pub mod stream;
pub mod testfloat;

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
        write!(
            f,
            "pc={:x} cpsr={:x} sp={:x} x=",
            self.pc, self.cpsr, self.sp
        )?;
        for (n, value) in self.x.iter().enumerate() {
            write!(f, "{}{value:x}", if n == 0 { "" } else { "," })?;
        }
        Ok(())
    }
}

/// The vector registers and the two FP registers, as the gdbstub and the
/// plugin give them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vectors {
    pub v: [u128; 32],
    pub fpsr: u32,
    pub fpcr: u32,
}

impl Vectors {
    pub fn of(state: &State) -> Self {
        let mut v = [0; 32];
        for (n, value) in v.iter_mut().enumerate() {
            *value = state.v(n);
        }
        Self {
            v,
            fpsr: state.fpsr(),
            fpcr: state.fpcr(),
        }
    }

    /// Put these registers into `state`.
    pub fn apply(&self, state: &mut State) {
        for (n, value) in self.v.iter().enumerate() {
            state.set_v(n, *value);
        }
        state.set_fpsr(self.fpsr);
        state.set_fpcr(self.fpcr);
    }

    /// The names of the registers that differ, with both values.
    pub fn difference(&self, other: &Self) -> Vec<String> {
        let mut found = Vec::new();
        for n in 0..32 {
            if self.v[n] != other.v[n] {
                found.push(format!("v{n}: {:#x} / {:#x}", self.v[n], other.v[n]));
            }
        }
        if self.fpsr != other.fpsr {
            found.push(format!("fpsr: {:#x} / {:#x}", self.fpsr, other.fpsr));
        }
        if self.fpcr != other.fpcr {
            found.push(format!("fpcr: {:#x} / {:#x}", self.fpcr, other.fpcr));
        }
        found
    }
}

/// `fpcr`, `fpsr` and V0 to V31, in the form of a corpus line.
impl std::fmt::Display for Vectors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fpcr={:x} fpsr={:x} v=", self.fpcr, self.fpsr)?;
        for (n, value) in self.v.iter().enumerate() {
            write!(f, "{}{value:x}", if n == 0 { "" } else { "," })?;
        }
        Ok(())
    }
}

/// One record of the plugin stream. Nearly every record is an instruction,
/// so the size of that variant is the size that matters.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trace {
    /// The registers before the instruction `encoding` at `core.pc` runs.
    /// `vectors`: when the stream carries them.
    /// `system`: the system registers that the stream carries, in the order
    /// in which the plugin was asked for them.
    Instruction {
        encoding: u32,
        core: Core,
        vectors: Option<Vectors>,
        system: Vec<u64>,
    },
    /// A memory write of the last instruction.
    Write {
        address: u64,
        size: u32,
        low: u64,
        high: u64,
    },
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
/// carries V0 to V31, FPSR and FPCR in each instruction record. `system`:
/// the count of system registers that follow them.
pub fn parse(
    bytes: &[u8],
    vectors: bool,
    system: usize,
) -> Result<Option<(Trace, usize)>, String> {
    if bytes.len() < 8 {
        return Ok(None);
    }
    let tag = u32_at(bytes, 0);
    let second = u32_at(bytes, 4);
    match tag {
        1 => {
            let system_at = 8 + 34 * 8 + if vectors { 32 * 16 + 16 } else { 0 };
            let len = system_at + 8 * system;
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
            let vectors = vectors.then(|| {
                let at = 8 + 34 * 8;
                let mut v = [0u128; 32];
                for (n, value) in v.iter_mut().enumerate() {
                    *value = u128::from(u64_at(bytes, at + n * 16))
                        | u128::from(u64_at(bytes, at + n * 16 + 8)) << 64;
                }
                Vectors {
                    v,
                    fpsr: u32_at(bytes, at + 32 * 16),
                    fpcr: u32_at(bytes, at + 32 * 16 + 8),
                }
            });
            Ok(Some((
                Trace::Instruction {
                    encoding: second,
                    core,
                    vectors,
                    system: (0..system)
                        .map(|n| u64_at(bytes, system_at + 8 * n))
                        .collect(),
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
        other => Err(format!(
            "the plugin stream has a record with the tag {other}"
        )),
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
            assert_eq!(parse(&bytes[..cut], false, 0), Ok(None));
        }
        let (trace, len) = parse(&bytes, false, 0).unwrap().unwrap();
        assert_eq!(len, bytes.len());
        let Trace::Instruction {
            encoding,
            core,
            vectors,
            system,
        } = trace
        else {
            panic!("an instruction record");
        };
        assert_eq!(vectors, None);
        assert!(system.is_empty());
        assert_eq!(encoding, 0x9100_0400);
        assert_eq!(core.x[30], 90);
        assert_eq!(
            (core.sp, core.pc, core.cpsr),
            (0x1000, 0x4200_0000, 0x6000_03c5)
        );
        // With vectors the record is longer, and the same core comes out.
        assert_eq!(parse(&bytes, true, 0), Ok(None));
        for n in 0..32u128 {
            bytes.extend((n << 100 | 7).to_le_bytes());
        }
        bytes.extend(0x1fu64.to_le_bytes());
        bytes.extend(0x0340_0000u64.to_le_bytes());
        let (trace, len) = parse(&bytes, true, 0).unwrap().unwrap();
        assert_eq!(len, bytes.len());
        // Two system registers follow the vectors.
        assert_eq!(parse(&bytes, true, 2), Ok(None));
        let mut longer = bytes.clone();
        longer.extend(0x5600_0001u64.to_le_bytes());
        longer.extend(0x4200_0004u64.to_le_bytes());
        let Some((Trace::Instruction { system, .. }, used)) = parse(&longer, true, 2).unwrap()
        else {
            panic!("an instruction record with system registers");
        };
        assert_eq!(used, longer.len());
        assert_eq!(system, vec![0x5600_0001, 0x4200_0004]);
        let Trace::Instruction {
            core: with_vectors,
            vectors: Some(vectors),
            ..
        } = trace
        else {
            panic!("an instruction record with vectors");
        };
        assert_eq!(with_vectors, core);
        assert_eq!(vectors.v[31], 31 << 100 | 7);
        assert_eq!((vectors.fpsr, vectors.fpcr), (0x1f, 0x0340_0000));
        // The registers go to a state and back, and to a corpus line.
        let mut state = State::reset(0);
        vectors.apply(&mut state);
        assert_eq!(Vectors::of(&state), vectors);
        assert!(
            vectors
                .to_string()
                .starts_with("fpcr=3400000 fpsr=1f v=7,10000000000000000000000007,")
        );
        let mut other = vectors;
        other.v[2] = 0;
        other.fpsr = 0;
        assert_eq!(
            vectors.difference(&other),
            vec![
                "v2: 0x20000000000000000000000007 / 0x0".to_string(),
                "fpsr: 0x1f / 0x0".to_string()
            ]
        );

        let mut discontinuity = Vec::new();
        discontinuity.extend(3u32.to_le_bytes());
        discontinuity.extend(2u32.to_le_bytes());
        discontinuity.extend(0x4200_0004u64.to_le_bytes());
        discontinuity.extend(0x200u64.to_le_bytes());
        assert_eq!(
            parse(&discontinuity, false, 0),
            Ok(Some((
                Trace::Discontinuity {
                    kind: 2,
                    from_pc: 0x4200_0004,
                    to_pc: 0x200
                },
                24
            )))
        );
        assert!(parse(&[9, 0, 0, 0, 0, 0, 0, 0], false, 0).is_err());
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
        assert_eq!(
            core.difference(&other),
            vec!["pc: 0x42000010 / 0x0".to_string()]
        );
    }
}
