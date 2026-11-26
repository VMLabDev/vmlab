//! The follower (DESIGN.md O3, steps 3 to 5): one round of lockstep.
//!
//! A round loads memory and registers into QEMU and into our CPU, lets QEMU
//! run, and steps our interpreter beside the records of the plugin. Before
//! each instruction the registers of both sides must be the same. A round
//! ends at the first exception, at its instruction budget, or at the first
//! difference.

use super::leader::Leader;
use super::{Core, Trace};
use crate::exec::{Cpu, Stop};
use crate::generated::decode::{Record, decode};
use std::io;
use std::time::Duration;
use vmlab_hv_tcg::{FlatMemory, Memory};

/// What a round starts from.
#[derive(Clone, Debug)]
pub struct Round {
    /// Memory to load on both sides, as address and bytes.
    pub memory: Vec<(u64, Vec<u8>)>,
    pub start: Core,
    /// The most instructions to follow.
    pub budget: u64,
}

/// One instruction that both sides ran from the same registers, with the
/// registers that QEMU had after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub encoding: u32,
    pub record: Record,
    pub before: Core,
    pub after: Core,
}

/// How a round ended when the two sides agreed all the way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The budget of the round was used up.
    Budget,
    /// Both sides took the Undefined Instruction exception at an encoding
    /// that no selected record matches.
    Undefined,
    /// Both sides could not fetch at the PC: outside the memory.
    FetchAbort,
    /// Both sides refused a PC that is not a multiple of four.
    PcAlignment,
    /// QEMU took an exception at a record that we decode and have no lifter
    /// for. Nothing to compare: the exception cards (C6) decide these.
    UnliftedException(Record),
    /// QEMU ran a record that we decode and have no lifter for. The round
    /// cannot be followed further. This is a gap of the stream, not a
    /// difference.
    Unlifted(Record),
}

/// The first difference of a round.
#[derive(Clone, Debug)]
pub struct Difference {
    pub what: String,
    /// The last instruction that both sides ran before the difference
    /// showed, with the registers before it. `None`: the round differed at
    /// its first record.
    pub instruction: Option<(u32, Core)>,
    pub ours: Core,
    pub qemu: Option<Core>,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "difference: {}", self.what)?;
        if let Some((encoding, before)) = &self.instruction {
            let name = decode(*encoding).map_or("(undefined)", |decoded| decoded.record.name());
            writeln!(f, "  instruction {encoding:08x} {name}")?;
            writeln!(f, "  before {before}")?;
        }
        writeln!(f, "  ours   {}", self.ours)?;
        if let Some(qemu) = &self.qemu {
            writeln!(f, "  qemu   {qemu}")?;
            for line in self.ours.difference(qemu) {
                writeln!(f, "  ours / qemu  {line}")?;
            }
        }
        Ok(())
    }
}

pub enum Outcome {
    Agreed { end: End, verified: Vec<Verified> },
    Differed { difference: Box<Difference>, verified: Vec<Verified> },
}

/// How long QEMU may take to write the next record.
const RECORD_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Follower {
    pub leader: Leader,
    pub cpu: Cpu<FlatMemory>,
}

/// What our side expects from QEMU next, after it stopped at an instruction.
#[derive(Clone, Copy)]
enum Expect {
    /// The next instruction record.
    Instruction,
    /// An exception at this PC, for this reason.
    Exception(u64, End),
}

impl Follower {
    /// `cpu` holds the memory that mirrors the RAM of the leader.
    pub fn new(leader: Leader, cpu: Cpu<FlatMemory>) -> Self {
        Self { leader, cpu }
    }

    fn load(&mut self, round: &Round) -> io::Result<()> {
        for (address, bytes) in &round.memory {
            self.leader.load(*address, bytes)?;
            self.cpu
                .memory
                .write(*address, bytes)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        let state = round
            .start
            .state()
            .ok_or_else(|| io::Error::other("a round must start at EL1 with SP_EL1"))?;
        self.leader.set_core(&round.start)?;
        self.cpu.state = state;
        Ok(())
    }

    /// The word at the PC of our CPU, when the memory holds one there.
    fn word(&mut self, pc: u64) -> Option<u32> {
        let mut bytes = [0u8; 4];
        self.cpu.memory.read(pc, &mut bytes).ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    pub fn run(&mut self, round: &Round) -> io::Result<Outcome> {
        self.load(round)?;
        self.leader.resume()?;
        let mut verified = Vec::new();
        // The last instruction that our side ran, with the registers before it.
        let mut last: Option<(u32, Record, Core)> = None;
        let mut expect = Expect::Instruction;
        let mut followed = 0u64;
        let differed = |what: String, last: &Option<(u32, Record, Core)>, ours: Core, qemu: Option<Core>| {
            Box::new(Difference {
                what,
                instruction: last.as_ref().map(|(encoding, _, before)| (*encoding, *before)),
                ours,
                qemu,
            })
        };
        let result = loop {
            let ours = Core::of(&self.cpu.state);
            match self.leader.next_record(RECORD_TIMEOUT)? {
                Trace::Instruction { encoding, core } => {
                    if let Expect::Exception(pc, _) = expect {
                        break Err(differed(
                            format!("we stop with an exception at {pc:#x} and QEMU runs on"),
                            &last,
                            ours,
                            Some(core),
                        ));
                    }
                    if core != ours {
                        break Err(differed(
                            "the registers before an instruction".into(),
                            &last,
                            ours,
                            Some(core),
                        ));
                    }
                    // The registers agree, so the last instruction did the same on both sides.
                    if let Some((word, record, before)) = last.take() {
                        verified.push(Verified {
                            encoding: word,
                            record,
                            before,
                            after: core,
                        });
                    }
                    if core.pc & 3 == 0 && self.word(core.pc) != Some(encoding) {
                        break Err(differed(
                            format!("QEMU runs the word {encoding:08x}, which our memory does not hold at the PC"),
                            &last,
                            ours,
                            Some(core),
                        ));
                    }
                    if followed >= round.budget {
                        self.leader.stop(true)?;
                        break Ok(End::Budget);
                    }
                    followed += 1;
                    match self.cpu.step() {
                        Ok(()) => {
                            if let Some(decoded) = decode(encoding) {
                                last = Some((encoding, decoded.record, core));
                            }
                        }
                        Err(Stop::Undefined { pc, .. }) => expect = Expect::Exception(pc, End::Undefined),
                        Err(Stop::PcAlignment { pc }) => expect = Expect::Exception(pc, End::PcAlignment),
                        Err(Stop::Unimplemented { pc, record }) => {
                            // QEMU goes on or takes an exception. The next record says which.
                            match self.leader.next_record(RECORD_TIMEOUT)? {
                                Trace::Discontinuity { kind: 2, from_pc, .. } if from_pc == pc => {
                                    self.leader.stop(false)?;
                                    break Ok(End::UnliftedException(record));
                                }
                                _ => {
                                    self.leader.stop(true)?;
                                    break Ok(End::Unlifted(record));
                                }
                            }
                        }
                        Err(stop) => {
                            break Err(differed(
                                format!("our side cannot run the instruction that QEMU runs: {stop:?}"),
                                &last,
                                ours,
                                Some(core),
                            ));
                        }
                    }
                }
                Trace::Discontinuity { kind, from_pc, to_pc } => {
                    if kind != 2 {
                        break Err(differed(
                            format!("QEMU took an interrupt or a host call (kind {kind}) from {from_pc:#x} to {to_pc:#x}"),
                            &last,
                            ours,
                            None,
                        ));
                    }
                    let end = match expect {
                        Expect::Exception(pc, end) if pc == from_pc => end,
                        Expect::Exception(pc, _) => {
                            break Err(differed(
                                format!("we stop with an exception at {pc:#x} and QEMU takes one from {from_pc:#x}"),
                                &last,
                                ours,
                                None,
                            ));
                        }
                        // No record for the instruction at our PC: QEMU could
                        // not fetch it. Our memory must hold nothing there.
                        Expect::Instruction if from_pc == ours.pc && ours.pc & 3 == 0 && self.word(ours.pc).is_none() => {
                            End::FetchAbort
                        }
                        Expect::Instruction => {
                            break Err(differed(
                                format!("QEMU took an exception from {from_pc:#x} where we would run on"),
                                &last,
                                ours,
                                None,
                            ));
                        }
                    };
                    // The breakpoint on the vector stops QEMU. An exception
                    // changes no general register, so they are the registers
                    // after the last instruction.
                    self.leader.stop(false)?;
                    let stopped = self.leader.core()?;
                    let mut after = stopped;
                    after.pc = ours.pc;
                    after.cpsr = ours.cpsr;
                    if after != ours {
                        break Err(differed(
                            "the general registers at an exception".into(),
                            &last,
                            ours,
                            Some(after),
                        ));
                    }
                    if let Some((word, record, before)) = last.take() {
                        verified.push(Verified {
                            encoding: word,
                            record,
                            before,
                            after: ours,
                        });
                    }
                    break Ok(end);
                }
                Trace::Write { address, size, .. } => {
                    break Err(differed(
                        format!("QEMU wrote {size} bytes at {address:#x} and no lifter of ours writes memory yet"),
                        &last,
                        ours,
                        None,
                    ));
                }
            }
        };
        Ok(match result {
            Ok(end) => Outcome::Agreed { end, verified },
            Err(difference) => {
                // Leave QEMU stopped, so that the next round can start.
                self.leader.stop(true)?;
                Outcome::Differed { difference, verified }
            }
        })
    }
}
