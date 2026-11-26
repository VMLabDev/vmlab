//! The follower (DESIGN.md O3, steps 3 to 5): one round of lockstep.
//!
//! A round loads memory and registers into QEMU and into our CPU, lets QEMU
//! run, and steps our interpreter beside the records of the plugin. Before
//! each instruction the registers of both sides must be the same. Both sides
//! take the exceptions of the stream, and the round goes on at the vector
//! when the vector is in the memory that the round mirrors. A round ends at
//! its instruction budget, where QEMU leaves the mirrored memory, or at the
//! first difference.
//!
//! QEMU leads for what is not a function of the CPU state (DESIGN.md O4):
//! the value of a counter read and the answer of a host call are taken from
//! its next record.

use super::leader::{Leader, VECTOR};
use super::{Compared, Core, Trace, Vectors, normalise, system_difference, system_of};
use crate::exception::{Cause, Exception};
use crate::exec::{Cpu, Stop, Taken};
use crate::generated::decode::{Record, decode};
use crate::state::State;
use crate::sysreg::QEMU_CORTEX_A53;
use crate::timer::Clock;
use std::io;
use std::time::Duration;
use vmlab_hv_tcg::{FlatMemory, Memory};

/// What a round starts from.
#[derive(Clone, Debug)]
pub struct Round {
    /// Memory to load on both sides, as address and bytes.
    pub memory: Vec<(u64, Vec<u8>)>,
    pub start: Core,
    /// V0 to V31, FPSR and FPCR at the start. `Some`: they are loaded on both
    /// sides and compared before every instruction, and the leader must have
    /// been started with vectors. `None`: they are left alone and not compared.
    pub vectors: Option<Vectors>,
    /// The most instructions to follow.
    pub budget: u64,
    /// The round ends when one PC is about to run more often than this. It
    /// keeps a loop of the stream from filling the round with repeats.
    pub visits: u32,
    /// Reset both sides first, so that every system register starts from its
    /// reset value. A round of system instructions does. Without, our side
    /// starts from a new state with FP enabled when the round has vectors,
    /// as the leader enabled it once.
    pub reset: bool,
}

/// One instruction that both sides ran from the same registers, with the
/// registers that QEMU had after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub encoding: u32,
    pub record: Record,
    pub before: Core,
    pub after: Core,
    /// The vector and FP registers before and after, in a round that
    /// compares them.
    pub vectors: Option<(Vectors, Vectors)>,
    /// The compared system registers before and after, in a round that
    /// compares them.
    pub system: Option<(Vec<u64>, Vec<u64>)>,
    /// The result is a function of the registers before: the instruction
    /// can be replayed without QEMU. A counter read and a host call are not.
    pub replayable: bool,
}

/// How a round ended when the two sides agreed all the way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The budget of the round was used up.
    Budget,
    /// A PC came up more often than the round allows: a loop.
    Loop,
    /// Both sides took the Undefined Instruction exception to a vector
    /// outside the mirrored memory.
    Undefined,
    /// Both sides could not fetch at the PC, and the vector is outside the
    /// mirrored memory.
    FetchAbort,
    /// Both sides refused a PC that is not a multiple of four, likewise.
    PcAlignment,
    /// Both sides took another exception to a vector outside the mirrored
    /// memory.
    Exception,
    /// QEMU went on at a PC outside the memory that the round mirrors, where
    /// it has flash or a device. The branch that went there is verified.
    OutsideMemory,
    /// QEMU took an exception at a record that we decode and have no lifter
    /// for. Nothing to compare.
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
    /// The vector and FP registers of both sides, ours first, in a round
    /// that compares them.
    pub vectors: Option<(Vectors, Vectors)>,
    /// The vector and FP registers before the instruction.
    pub vectors_before: Option<Vectors>,
    /// The compared system registers that differ, as `name: ours / qemu`.
    pub system: Vec<String>,
    /// The exception that our side took at the instruction.
    pub taken: Option<Taken>,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "difference: {}", self.what)?;
        if let Some((encoding, before)) = &self.instruction {
            let name = decode(*encoding).map_or("(undefined)", |decoded| decoded.record.name());
            writeln!(f, "  instruction {encoding:08x} {name}")?;
            writeln!(f, "  before {before}")?;
        }
        if let Some(taken) = &self.taken {
            writeln!(
                f,
                "  we took {:?} from {:#x} to {:#x}",
                taken.cause, taken.from, taken.to
            )?;
        }
        writeln!(f, "  ours   {}", self.ours)?;
        if let Some(qemu) = &self.qemu {
            writeln!(f, "  qemu   {qemu}")?;
            for line in self.ours.difference(qemu) {
                writeln!(f, "  ours / qemu  {line}")?;
            }
        }
        if let Some(before) = &self.vectors_before {
            writeln!(f, "  before {before}")?;
        }
        if let Some((ours, qemu)) = &self.vectors {
            for line in ours.difference(qemu) {
                writeln!(f, "  ours / qemu  {line}")?;
            }
        }
        for line in &self.system {
            writeln!(f, "  ours / qemu  {line}")?;
        }
        Ok(())
    }
}

pub enum Outcome {
    Agreed {
        end: End,
        verified: Vec<Verified>,
    },
    Differed {
        difference: Box<Difference>,
        verified: Vec<Verified>,
    },
}

/// How long QEMU may take to write the next record.
const RECORD_TIMEOUT: Duration = Duration::from_secs(8);

/// The count of the system counter that our side starts a round with. The
/// streams keep the compare value of a timer far below it or far above it,
/// so that the status of a timer does not depend on the time of the host.
pub const ROUND_COUNT: u64 = 1 << 32;

pub struct Follower {
    pub leader: Leader,
    pub cpu: Cpu<FlatMemory>,
    /// The system registers that the leader sends with each instruction.
    pub compared: Vec<Compared>,
}

/// The instruction that our side ran last and that the next record of QEMU
/// verifies.
struct Last {
    encoding: u32,
    record: Option<Record>,
    before: Core,
    vectors: Option<Vectors>,
    system: Option<Vec<u64>>,
    /// What our side takes from the next record of QEMU.
    adopt: Adopt,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Adopt {
    Nothing,
    /// The value of a counter or of a timer value in this register. `true`:
    /// it is the count itself, and our clock takes it.
    Counter(usize, bool),
    /// X0 to X3 after a host call.
    Call,
}

/// What a step of our side led to.
enum Stepped {
    /// The round goes on with the next record.
    On(Adopt),
    End(End),
    Differ(String),
}

/// The register that an `MRS` of a counter or of a timer value writes.
fn counter_read(encoding: u32) -> Adopt {
    if encoding & 0xffe0_0000 != 0xd520_0000 {
        return Adopt::Nothing;
    }
    let rt = (encoding & 31) as usize;
    if rt == 31 {
        return Adopt::Nothing;
    }
    match (encoding >> 5) & 0xffff {
        // CNTPCT_EL0 and CNTVCT_EL0.
        0xdf01 | 0xdf02 => Adopt::Counter(rt, true),
        // CNTP_TVAL_EL0 and CNTV_TVAL_EL0.
        0xdf10 | 0xdf18 => Adopt::Counter(rt, false),
        _ => Adopt::Nothing,
    }
}

fn end_of(cause: Cause) -> End {
    match cause {
        Cause::Exception(Exception::Undefined) => End::Undefined,
        Cause::Exception(Exception::InstructionAbort) => End::FetchAbort,
        Cause::Exception(Exception::PcAlignment) => End::PcAlignment,
        _ => End::Exception,
    }
}

impl Follower {
    /// `cpu` holds the memory that mirrors the RAM of the leader.
    pub fn new(leader: Leader, cpu: Cpu<FlatMemory>) -> Self {
        Self {
            leader,
            cpu,
            compared: Vec::new(),
        }
    }

    fn load(&mut self, round: &Round) -> io::Result<()> {
        if round.reset {
            self.leader.reset()?;
        }
        for (address, bytes) in &round.memory {
            self.leader.load(*address, bytes)?;
            self.cpu
                .memory
                .write(*address, bytes)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        if round.start.cpsr & 0x0fff_ffff != super::CPSR_EL1H {
            return Err(io::Error::other("a round must start at EL1 with SP_EL1"));
        }
        let mut state = State::reset_as(round.start.pc, &QEMU_CORTEX_A53);
        round.start.apply(&mut state);
        self.leader.set_core(&round.start)?;
        if let Some(vectors) = &round.vectors {
            self.leader.set_vectors(vectors)?;
            vectors.apply(&mut state);
            if !round.reset {
                state.enable_fp();
            }
        }
        self.cpu.state = state;
        self.cpu.clock = Clock::Manual(ROUND_COUNT);
        self.cpu.taken = None;
        Ok(())
    }

    /// The word at the PC of our CPU, when the memory holds one there.
    fn word(&mut self, pc: u64) -> Option<u32> {
        let mut bytes = [0u8; 4];
        self.cpu.memory.read(pc, &mut bytes).ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    /// Stop QEMU after an exception to `vector`: at the breakpoint when the
    /// vector is the one that has it, else by asking.
    fn halt(&mut self, vector: u64) -> io::Result<()> {
        self.leader.stop(vector != VECTOR)
    }

    /// Both sides took the same exception to `taken.to`.
    fn after_exception(&mut self, taken: Taken) -> io::Result<Stepped> {
        if taken.to & 3 == 0 && self.word(taken.to).is_some() {
            return Ok(Stepped::On(Adopt::Nothing));
        }
        // The vector is outside the mirrored memory: the round ends.
        self.halt(taken.to)?;
        if taken.to == VECTOR {
            // The breakpoint stopped QEMU before the first instruction of
            // the vector, so its registers are those after the entry.
            let stopped = self.leader.core()?;
            let ours = Core::of(&self.cpu.state);
            if stopped != ours {
                return Ok(Stepped::Differ(format!(
                    "the registers at the vector of an exception: {}",
                    ours.difference(&stopped).join("; ")
                )));
            }
        }
        Ok(Stepped::End(end_of(taken.cause)))
    }

    /// The discontinuity that QEMU must show for an exception or a call of
    /// ours at `pc`, as its kind and its target. The target is compared by
    /// the caller, so a source that is one instruction off cannot hide a
    /// difference. `seen`: it came already.
    /// `Err`: QEMU does something else.
    fn expect(
        &mut self,
        pc: u64,
        seen: Option<(u32, u64, u64)>,
    ) -> io::Result<Result<(u32, u64), String>> {
        let (kind, from, to) = match seen {
            Some(seen) => seen,
            None => match self.leader.next_record(RECORD_TIMEOUT)? {
                Trace::Discontinuity {
                    kind,
                    from_pc,
                    to_pc,
                } => (kind, from_pc, to_pc),
                Trace::Instruction { core, .. } => {
                    return Ok(Err(format!(
                        "we leave the stream at {pc:#x} and QEMU runs on at {:#x}",
                        core.pc
                    )));
                }
                Trace::Write { address, .. } => {
                    return Ok(Err(format!(
                        "we leave the stream at {pc:#x} and QEMU writes memory at {address:#x}"
                    )));
                }
            },
        };
        // QEMU names the instruction after a call as the source.
        if from != pc && from != pc.wrapping_add(4) {
            return Ok(Err(format!(
                "we leave the stream at {pc:#x} and QEMU at {from:#x}"
            )));
        }
        Ok(Ok((kind, to)))
    }

    /// Bring QEMU's reaction to the step of our side together with ours.
    /// `seen`: a discontinuity of QEMU that came before our step, as kind,
    /// source and target.
    fn react(
        &mut self,
        stepped: Result<(), Stop>,
        encoding: Option<u32>,
        seen: Option<(u32, u64, u64)>,
    ) -> io::Result<Stepped> {
        match stepped {
            Ok(()) => match self.cpu.taken {
                None => match seen {
                    None => Ok(Stepped::On(encoding.map_or(Adopt::Nothing, counter_read))),
                    Some((kind, from, to)) => Ok(Stepped::Differ(format!(
                        "QEMU took a discontinuity of the kind {kind} from {from:#x} to {to:#x} where we run on"
                    ))),
                },
                Some(taken) => {
                    let (kind, to) = match self.expect(taken.from, seen)? {
                        Ok(found) => found,
                        Err(what) => return Ok(Stepped::Differ(what)),
                    };
                    if kind != 2 || to != taken.to {
                        return Ok(Stepped::Differ(format!(
                            "we take an exception to {:#x} and QEMU a discontinuity of the kind {kind} to {to:#x}",
                            taken.to
                        )));
                    }
                    self.after_exception(taken)
                }
            },
            Err(Stop::Hypercall { pc, .. }) => {
                let (kind, to) = match self.expect(pc, seen)? {
                    Ok(found) => found,
                    Err(what) => return Ok(Stepped::Differ(what)),
                };
                match kind {
                    // QEMU served the call: its answer is in the next record.
                    4 if to == pc.wrapping_add(4) => Ok(Stepped::On(Adopt::Call)),
                    // QEMU has nobody for the call: undefined.
                    2 => {
                        self.cpu.refuse_hypercall(pc);
                        match self.cpu.taken {
                            Some(taken) if taken.to == to => self.after_exception(taken),
                            _ => Ok(Stepped::Differ(format!(
                                "QEMU takes a refused call at {pc:#x} to {to:#x}"
                            ))),
                        }
                    }
                    kind => Ok(Stepped::Differ(format!(
                        "QEMU answers the call at {pc:#x} with a discontinuity of the kind {kind} to {to:#x}"
                    ))),
                }
            }
            Err(Stop::Unimplemented { pc, record }) => {
                // QEMU goes on or takes an exception. The next record says which.
                let next = match seen {
                    Some(seen) => Some(seen),
                    None => match self.leader.next_record(RECORD_TIMEOUT)? {
                        Trace::Discontinuity {
                            kind,
                            from_pc,
                            to_pc,
                        } => Some((kind, from_pc, to_pc)),
                        _ => None,
                    },
                };
                match next {
                    Some((2, from, to)) if from == pc => {
                        self.halt(to)?;
                        Ok(Stepped::End(End::UnliftedException(record)))
                    }
                    _ => {
                        self.leader.stop(true)?;
                        Ok(Stepped::End(End::Unlifted(record)))
                    }
                }
            }
            Err(stop) => Ok(Stepped::Differ(format!(
                "our side cannot run the instruction that QEMU runs: {stop:?}"
            ))),
        }
    }

    pub fn run(&mut self, round: &Round) -> io::Result<Outcome> {
        self.load(round)?;
        self.leader.resume()?;
        let mut verified = Vec::new();
        let mut last: Option<Last> = None;
        let compare_vectors = round.vectors.is_some();
        let compare_system = !self.compared.is_empty();
        let mut followed = 0u64;
        let mut visits: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let differed = |what: String, last: &Option<Last>, ours: Core, qemu: Option<Core>| {
            Box::new(Difference {
                what,
                instruction: last.as_ref().map(|last| (last.encoding, last.before)),
                ours,
                qemu,
                vectors: None,
                vectors_before: last.as_ref().and_then(|last| last.vectors),
                system: Vec::new(),
                taken: None,
            })
        };
        let result = loop {
            let record = match self.leader.next_record(RECORD_TIMEOUT) {
                Ok(record) => record,
                // QEMU stands still, for example in a WFI: the round names
                // the instruction that it ran last.
                Err(error) => {
                    let ours = Core::of(&self.cpu.state);
                    let mut difference = differed(error.to_string(), &last, ours, None);
                    difference.taken = self.cpu.taken;
                    break Err(difference);
                }
            };
            let stepped = match record {
                Trace::Instruction {
                    encoding,
                    core,
                    vectors,
                    mut system,
                } => {
                    // What QEMU leads in: a counter, the answer of a call.
                    match last.as_ref().map_or(Adopt::Nothing, |last| last.adopt) {
                        Adopt::Nothing => {}
                        Adopt::Counter(rt, count) => {
                            self.cpu.state.set_x(rt, core.x[rt]);
                            if count {
                                self.cpu.clock = Clock::Manual(core.x[rt]);
                            }
                        }
                        Adopt::Call => {
                            for n in 0..4 {
                                self.cpu.state.set_x(n, core.x[n]);
                            }
                        }
                    }
                    let ours = Core::of(&self.cpu.state);
                    if core != ours {
                        let mut difference = differed(
                            "the registers before an instruction".into(),
                            &last,
                            ours,
                            Some(core),
                        );
                        difference.taken = self.cpu.taken;
                        break Err(difference);
                    }
                    let ours_vectors = compare_vectors.then(|| Vectors::of(&self.cpu.state));
                    if let Some(ours_vectors) = ours_vectors {
                        let Some(theirs) = vectors else {
                            return Err(io::Error::other(
                                "a round compares the vector registers and the leader does not send them",
                            ));
                        };
                        if theirs != ours_vectors {
                            let mut difference = differed(
                                "the vector and FP registers before an instruction".into(),
                                &last,
                                ours,
                                Some(core),
                            );
                            difference.vectors = Some((ours_vectors, theirs));
                            break Err(difference);
                        }
                    }
                    let ours_system =
                        compare_system.then(|| system_of(&self.cpu.state, &self.compared));
                    if let Some(ours_system) = &ours_system {
                        normalise(&mut system, &self.compared, &core);
                        if system != *ours_system {
                            let mut difference = differed(
                                "the system registers before an instruction".into(),
                                &last,
                                ours,
                                Some(core),
                            );
                            difference.system =
                                system_difference(&self.compared, ours_system, &system);
                            difference.taken = self.cpu.taken;
                            break Err(difference);
                        }
                    }
                    // The registers agree, so the last instruction did the same on both sides.
                    if let Some(last) = last.take()
                        && let Some(record) = last.record
                    {
                        verified.push(Verified {
                            encoding: last.encoding,
                            record,
                            before: last.before,
                            after: core,
                            vectors: last.vectors.zip(ours_vectors),
                            system: last.system.zip(ours_system.clone()),
                            replayable: last.adopt == Adopt::Nothing,
                        });
                    }
                    // QEMU has memory that the round does not mirror: flash
                    // at 0 and devices. A branch that lands there is verified
                    // by the registers above, and the round ends.
                    let Some(word) = self.word(core.pc) else {
                        self.leader.stop(true)?;
                        break Ok(End::OutsideMemory);
                    };
                    if core.pc & 3 == 0 && word != encoding {
                        break Err(differed(
                            format!(
                                "QEMU runs the word {encoding:08x} where our memory holds {word:08x}"
                            ),
                            &last,
                            ours,
                            Some(core),
                        ));
                    }
                    if followed >= round.budget {
                        self.leader.stop(true)?;
                        break Ok(End::Budget);
                    }
                    let seen = visits.entry(core.pc).or_default();
                    *seen += 1;
                    if *seen > round.visits {
                        self.leader.stop(true)?;
                        break Ok(End::Loop);
                    }
                    followed += 1;
                    let stepped = self.cpu.step();
                    let reaction = self.react(stepped, Some(encoding), None)?;
                    if let Stepped::On(adopt) = reaction {
                        last = Some(Last {
                            encoding,
                            record: decode(encoding).map(|decoded| decoded.record),
                            before: core,
                            vectors: ours_vectors,
                            system: ours_system,
                            adopt,
                        });
                    } else {
                        last = Some(Last {
                            encoding,
                            record: None,
                            before: core,
                            vectors: ours_vectors,
                            system: None,
                            adopt: Adopt::Nothing,
                        });
                    }
                    reaction
                }
                // QEMU left the stream without a record for the instruction:
                // it could not fetch it, the PC is not a multiple of four, or
                // the instruction is a call that it does not retire.
                Trace::Discontinuity {
                    kind,
                    from_pc,
                    to_pc,
                } => {
                    let ours = Core::of(&self.cpu.state);
                    if from_pc != ours.pc {
                        break Err(differed(
                            format!(
                                "QEMU took a discontinuity of the kind {kind} from {from_pc:#x} to {to_pc:#x} and we are at {:#x}",
                                ours.pc
                            ),
                            &last,
                            ours,
                            None,
                        ));
                    }
                    // The registers before it are verified by nothing: the
                    // last instruction is not counted. A difference names
                    // the instruction that QEMU did not retire.
                    last = self.word(from_pc).map(|encoding| Last {
                        encoding,
                        record: None,
                        before: ours,
                        vectors: None,
                        system: None,
                        adopt: Adopt::Nothing,
                    });
                    let stepped = self.cpu.step();
                    self.react(stepped, None, Some((kind, from_pc, to_pc)))?
                }
                Trace::Write { address, size, .. } => {
                    let ours = Core::of(&self.cpu.state);
                    break Err(differed(
                        format!(
                            "QEMU wrote {size} bytes at {address:#x} and no lifter of ours writes memory yet"
                        ),
                        &last,
                        ours,
                        None,
                    ));
                }
            };
            match stepped {
                Stepped::On(_) => {}
                Stepped::End(end) => break Ok(end),
                Stepped::Differ(mut what) => {
                    let ours = Core::of(&self.cpu.state);
                    // What QEMU's exception registers say, when it took an
                    // exception that we did not: they are in its next record.
                    if compare_system
                        && let Ok(Trace::Instruction { system, core, .. }) =
                            self.leader.next_record(Duration::from_secs(1))
                    {
                        what.push_str(&format!("; QEMU goes on at {:#x} with", core.pc));
                        for (register, value) in self.compared.iter().zip(&system) {
                            if matches!(
                                register.qemu,
                                "ESR_EL1" | "FAR_EL1" | "ELR_EL1" | "SPSR_EL1" | "MDSCR_EL1"
                            ) {
                                what.push_str(&format!(" {}={value:#x}", register.qemu));
                            }
                        }
                    }
                    let mut difference = differed(what, &last, ours, None);
                    difference.taken = self.cpu.taken;
                    break Err(difference);
                }
            }
        };
        Ok(match result {
            Ok(end) => Outcome::Agreed { end, verified },
            Err(difference) => {
                // Leave QEMU stopped, so that the next round can start.
                self.leader.stop(true)?;
                Outcome::Differed {
                    difference,
                    verified,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_read_names_the_register_that_qemu_leads() {
        // mrs x5, cntvct_el0 ; mrs x6, cntp_tval_el0 ; mrs xzr, cntvct_el0 ;
        // mrs x5, tpidr_el0 ; msr cntv_tval_el0, x5
        assert!(counter_read(0xd53b_e045) == Adopt::Counter(5, true));
        assert!(counter_read(0xd53b_e206) == Adopt::Counter(6, false));
        assert!(counter_read(0xd53b_e05f) == Adopt::Nothing);
        assert!(counter_read(0xd53b_d045) == Adopt::Nothing);
        assert!(counter_read(0xd51b_e305) == Adopt::Nothing);
        assert_eq!(end_of(Cause::Exception(Exception::Undefined)), End::Undefined);
        assert_eq!(
            end_of(Cause::Exception(Exception::SupervisorCall(1))),
            End::Exception
        );
    }
}
