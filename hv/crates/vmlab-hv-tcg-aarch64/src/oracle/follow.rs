//! The follower (DESIGN.md O3, steps 3 to 5): one round of lockstep.
//!
//! A round loads memory and registers into QEMU and into our CPU, lets QEMU
//! run, and steps our interpreter beside the records of the plugin. Before
//! each instruction the registers of both sides must be the same. A round
//! ends at the first exception, at its instruction budget, or at the first
//! difference.

use super::leader::{Ahead, Leader};
use super::{Core, Trace, Vectors};
use crate::exec::{Access, Cpu, Stop};
use crate::generated::decode::{Record, decode};
use std::collections::BTreeMap;
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
    /// The round ends when one PC is about to run more often than this. It
    /// keeps a loop of the stream from filling the round with repeats.
    pub visits: u32,
}

/// One instruction that both sides ran from the same registers, with the
/// registers that QEMU had after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub encoding: u32,
    pub record: Record,
    pub before: Core,
    pub after: Core,
    /// The vector registers of QEMU before and after, when the plugin
    /// gives them.
    pub vectors: Option<(Vectors, Vectors)>,
    /// The loads and stores that our side made. The stores are those that
    /// QEMU made: the round compares them.
    pub accesses: Vec<Access>,
}

/// How a round ended when the two sides agreed all the way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The budget of the round was used up.
    Budget,
    /// A PC came up more often than the round allows: a loop.
    Loop,
    /// Both sides took the Undefined Instruction exception at an encoding
    /// that no selected record matches.
    Undefined,
    /// Both sides could not fetch at the PC: outside the memory.
    FetchAbort,
    /// Both sides refused a PC that is not a multiple of four.
    PcAlignment,
    /// QEMU went on at a PC outside the memory that the round mirrors, where
    /// it has flash or a device. The branch that went there is verified.
    OutsideMemory,
    /// Both sides could not make a load or a store: the address is outside
    /// the memory. No register of the instruction changed on either side.
    DataAbort,
    /// Both sides refused an access to an address that is not a multiple of
    /// its size.
    Alignment,
    /// Our side has no memory at the address of a load or a store, and QEMU
    /// has flash or a device there and ran on. Nothing to compare.
    Device,
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
const RECORD_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Follower {
    pub leader: Leader,
    pub cpu: Cpu<FlatMemory>,
    /// QEMU ran instructions that our side did not follow, so our vector
    /// registers are not QEMU's any more. The next round reads them.
    stale: bool,
    /// QEMU ran instructions that our side did not follow, and one of them
    /// can have been an exclusive load: the exclusive monitors of the two
    /// sides may differ. `sync_monitor` makes them the same again.
    pub monitor_stale: bool,
    /// Rounds that ended at a store of several parts which failed after a
    /// different count of parts on the two sides. Our memory took QEMU's
    /// bytes there.
    pub partial_stores: u64,
}

/// What our side expects from QEMU next, after it stopped at an instruction.
#[derive(Clone, Copy)]
enum Expect {
    /// The next instruction record.
    Instruction,
    /// An exception at this PC, for this reason.
    Exception(u64, End),
    /// A data abort at this PC. QEMU runs on instead when it has a device
    /// or flash at the address.
    Abort(u64),
}

/// The last instruction that our side ran.
struct Last {
    encoding: u32,
    record: Record,
    before: Core,
    vectors: Option<Vectors>,
    accesses: Vec<Access>,
}

type QemuWrite = (u64, u32, u64, u64);

fn qemu_bytes(write: &QemuWrite) -> (u64, Vec<u8>) {
    let (address, size, low, high) = *write;
    let value = u128::from(low) | u128::from(high) << 64;
    (
        address,
        value.to_le_bytes()[..(size as usize).min(16)].to_vec(),
    )
}

/// The bytes that a list of writes leaves, by address.
fn written(writes: impl Iterator<Item = (u64, Vec<u8>)>) -> BTreeMap<u64, u8> {
    let mut bytes = BTreeMap::new();
    for (address, data) in writes {
        for (index, byte) in data.into_iter().enumerate() {
            bytes.insert(address.wrapping_add(index as u64), byte);
        }
    }
    bytes
}

fn our_writes(accesses: &[Access]) -> BTreeMap<u64, u8> {
    written(accesses.iter().filter(|access| access.write).map(|access| {
        (
            access.address,
            access.value.to_le_bytes()[..usize::from(access.bytes)].to_vec(),
        )
    }))
}

fn describe(bytes: &BTreeMap<u64, u8>) -> String {
    let mut text = String::new();
    let mut next = None;
    for (address, byte) in bytes {
        if next != Some(*address) {
            text.push_str(&format!(" {address:#x}:"));
        }
        text.push_str(&format!("{byte:02x}"));
        next = Some(address.wrapping_add(1));
    }
    if text.is_empty() {
        text.push_str(" nothing");
    }
    text
}

impl Follower {
    /// `cpu` holds the memory that mirrors the RAM of the leader.
    pub fn new(leader: Leader, mut cpu: Cpu<FlatMemory>) -> Self {
        cpu.log_accesses(true);
        // The vector registers of QEMU are not ours until they are read.
        let stale = leader.has_vectors();
        Self {
            leader,
            cpu,
            stale,
            monitor_stale: false,
            partial_stores: 0,
        }
    }

    /// Put `bytes` at `address` on both sides.
    pub fn load_memory(&mut self, address: u64, bytes: &[u8]) -> io::Result<()> {
        self.leader.load(address, bytes)?;
        self.cpu
            .memory
            .write(address, bytes)
            .map_err(|error| io::Error::other(error.to_string()))
    }

    /// Give the vector register `n` a value on both sides.
    pub fn set_vector(&mut self, n: usize, value: u128) -> io::Result<()> {
        self.leader.set_vector(n, value)?;
        self.cpu.state.slots[crate::state::slot::V0 + n] = value;
        Ok(())
    }

    fn load(&mut self, round: &Round) -> io::Result<()> {
        for (address, bytes) in &round.memory {
            self.load_memory(*address, bytes)?;
        }
        let mut state = round
            .start
            .state()
            .ok_or_else(|| io::Error::other("a round must start at EL1 with SP_EL1"))?;
        // A round sets the core registers. The vector registers stay what
        // the last round left on both sides.
        if self.leader.has_vectors() {
            if self.stale {
                let vectors = self.leader.vectors()?;
                vectors.apply(&mut self.cpu.state);
                self.stale = false;
            }
            Vectors::of(&self.cpu.state).apply(&mut state);
        }
        self.leader.set_core(&round.start)?;
        self.cpu.state = state;
        self.cpu.take_accesses();
        Ok(())
    }

    /// The word at the PC of our CPU, when the memory holds one there.
    fn word(&mut self, pc: u64) -> Option<u32> {
        let mut bytes = [0u8; 4];
        self.cpu.memory.read(pc, &mut bytes).ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    /// The memory writes of the last instruction, ours against QEMU's:
    /// every byte that our side wrote, QEMU wrote, and our memory holds now
    /// what QEMU wrote. QEMU reports an exclusive store that fails its
    /// compare as a write of the bytes that were there, so the two lists
    /// are not compared one to one.
    fn writes_differ(&mut self, last: &Option<Last>, qemu: &[QemuWrite]) -> Option<String> {
        let ours = last
            .as_ref()
            .map(|last| our_writes(&last.accesses))
            .unwrap_or_default();
        let theirs = written(qemu.iter().map(qemu_bytes));
        let mut same = ours.keys().all(|address| theirs.contains_key(address));
        for (address, byte) in &theirs {
            let mut now = [0u8; 1];
            same &= self.cpu.memory.read(*address, &mut now).is_ok() && now[0] == *byte;
        }
        (!same).then(|| {
            format!(
                "the memory writes of an instruction: ours{}, QEMU's{}",
                describe(&ours),
                describe(&theirs)
            )
        })
    }

    /// Take into our memory what QEMU wrote and our side did not.
    fn mirror(&mut self, writes: &[QemuWrite]) {
        for write in writes {
            let (address, bytes) = qemu_bytes(write);
            // A write to a device of QEMU has no place in our memory.
            let _ = self.cpu.memory.write(address, &bytes);
        }
    }

    /// Stop QEMU, which runs ahead of the records that our side has read,
    /// and take over what it did since: its memory writes now, its vector
    /// registers at the start of the next round.
    fn interrupt(&mut self, pending: &[QemuWrite]) -> io::Result<()> {
        let ahead: Ahead = self.leader.stop(true)?;
        self.mirror(pending);
        self.mirror(&ahead.writes);
        self.stale = self.leader.has_vectors();
        self.monitor_stale = true;
        Ok(())
    }

    /// Give the exclusive monitors of both sides one state, which nothing
    /// reads from outside: an exclusive load of a pair at `data` and its
    /// exclusive store, which writes back what it loaded. The load sets
    /// everything that a monitor holds, and the store ends the mark. `pc`:
    /// where the three words of the round go. `data`: 16 bytes of RAM at a
    /// multiple of 16.
    pub fn sync_monitor(&mut self, pc: u64, data: u64) -> io::Result<()> {
        let build = |record: Record, fields: &[(&str, u32)]| {
            fields.iter().fold(record.info().value, |word, (name, value)| {
                crate::with_field(record, name, word, *value).unwrap_or(word)
            })
        };
        let words = [
            // ldxp x1, x3, [x2] ; stxp w4, x1, x3, [x2]
            build(Record::LDXPX, &[("rt", 1), ("rt2", 3), ("rn", 2)]),
            build(Record::STXPX, &[("ws", 4), ("rt", 1), ("rt2", 3), ("rn", 2)]),
            0,
        ];
        let mut start = Core {
            x: [0; 31],
            sp: data,
            pc,
            cpsr: super::CPSR_EL1H,
        };
        start.x[2] = data;
        let round = Round {
            memory: vec![(pc, words.iter().flat_map(|word| word.to_le_bytes()).collect())],
            start,
            budget: 3,
            visits: 1,
        };
        match self.run(&round)? {
            Outcome::Agreed { verified, .. }
                if verified.len() == 2
                    && verified[1].after.x[4] == 0
                    && !self.monitor_stale => Ok(()),
            Outcome::Agreed { end, verified } => Err(io::Error::other(format!(
                "the round that makes the exclusive monitors the same ended with {end:?} after {} instructions",
                verified.len()
            ))),
            Outcome::Differed { difference, .. } => Err(io::Error::other(format!(
                "the round that makes the exclusive monitors the same differs: {difference}"
            ))),
        }
    }

    pub fn run(&mut self, round: &Round) -> io::Result<Outcome> {
        self.monitor_stale = false;
        self.load(round)?;
        self.leader.resume()?;
        let mut verified = Vec::new();
        // The last instruction that our side ran, with the registers before it.
        let mut last: Option<Last> = None;
        // The memory writes of QEMU since its last instruction record.
        let mut qemu_writes: Vec<QemuWrite> = Vec::new();
        // The accesses of an instruction of ours that ended in a data abort.
        let mut partial: Vec<Access> = Vec::new();
        let mut expect = Expect::Instruction;
        let mut followed = 0u64;
        let mut visits: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let differed = |what: String, last: &Option<Last>, ours: Core, qemu: Option<Core>| {
            Box::new(Difference {
                what,
                instruction: last.as_ref().map(|last| (last.encoding, last.before)),
                ours,
                qemu,
            })
        };
        let result = loop {
            let ours = Core::of(&self.cpu.state);
            match self.leader.next_record(RECORD_TIMEOUT)? {
                Trace::Instruction {
                    encoding,
                    core,
                    vectors,
                } => {
                    match expect {
                        Expect::Instruction => {}
                        Expect::Exception(pc, _) => {
                            break Err(differed(
                                format!("we stop with an exception at {pc:#x} and QEMU runs on"),
                                &last,
                                ours,
                                Some(core),
                            ));
                        }
                        Expect::Abort(_) => {
                            self.interrupt(&qemu_writes)?;
                            break Ok(End::Device);
                        }
                    }
                    if let Some(what) = self.writes_differ(&last, &qemu_writes) {
                        break Err(differed(what, &last, ours, Some(core)));
                    }
                    qemu_writes.clear();
                    if core != ours {
                        break Err(differed(
                            "the registers before an instruction".into(),
                            &last,
                            ours,
                            Some(core),
                        ));
                    }
                    if let Some(vectors) = &vectors {
                        let found = Vectors::of(&self.cpu.state).difference(vectors);
                        if !found.is_empty() {
                            break Err(differed(
                                format!(
                                    "the vector registers before an instruction, ours / QEMU's: {}",
                                    found.join("; ")
                                ),
                                &last,
                                ours,
                                Some(core),
                            ));
                        }
                    }
                    // The registers agree, so the last instruction did the same on both sides.
                    if let Some(last) = last.take() {
                        verified.push(Verified {
                            encoding: last.encoding,
                            record: last.record,
                            before: last.before,
                            after: core,
                            vectors: last.vectors.zip(vectors),
                            accesses: last.accesses,
                        });
                    }
                    // QEMU has memory that the round does not mirror: flash
                    // at 0 and devices. A branch that lands there is verified
                    // by the registers above, and the round ends.
                    let Some(word) = self.word(core.pc) else {
                        self.interrupt(&[])?;
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
                        self.interrupt(&[])?;
                        break Ok(End::Budget);
                    }
                    let seen = visits.entry(core.pc).or_default();
                    *seen += 1;
                    if *seen > round.visits {
                        self.interrupt(&[])?;
                        break Ok(End::Loop);
                    }
                    followed += 1;
                    let stepped = self.cpu.step();
                    let accesses = self.cpu.take_accesses();
                    match stepped {
                        Ok(()) => {
                            if let Some(decoded) = decode(encoding) {
                                last = Some(Last {
                                    encoding,
                                    record: decoded.record,
                                    before: core,
                                    vectors,
                                    accesses,
                                });
                            }
                        }
                        Err(Stop::Undefined { pc, .. }) => {
                            expect = Expect::Exception(pc, End::Undefined)
                        }
                        Err(Stop::PcAlignment { pc }) => {
                            expect = Expect::Exception(pc, End::PcAlignment)
                        }
                        Err(Stop::Alignment { pc, .. }) => {
                            expect = Expect::Exception(pc, End::Alignment)
                        }
                        Err(Stop::DataAbort { pc, .. }) => {
                            partial = accesses;
                            expect = Expect::Abort(pc);
                        }
                        Err(Stop::Unimplemented { pc, record }) => {
                            // QEMU goes on or takes an exception. The next record says which.
                            match self.leader.next_record(RECORD_TIMEOUT)? {
                                Trace::Discontinuity {
                                    kind: 2, from_pc, ..
                                } if from_pc == pc => {
                                    self.leader.stop(false)?;
                                    break Ok(End::UnliftedException(record));
                                }
                                Trace::Write {
                                    address,
                                    size,
                                    low,
                                    high,
                                } => {
                                    self.interrupt(&[(address, size, low, high)])?;
                                    break Ok(End::Unlifted(record));
                                }
                                _ => {
                                    self.interrupt(&[])?;
                                    break Ok(End::Unlifted(record));
                                }
                            }
                        }
                        Err(stop) => {
                            break Err(differed(
                                format!(
                                    "our side cannot run the instruction that QEMU runs: {stop:?}"
                                ),
                                &last,
                                ours,
                                Some(core),
                            ));
                        }
                    }
                }
                Trace::Discontinuity {
                    kind,
                    from_pc,
                    to_pc,
                } => {
                    if kind != 2 {
                        break Err(differed(
                            format!(
                                "QEMU took an interrupt or a host call (kind {kind}) from {from_pc:#x} to {to_pc:#x}"
                            ),
                            &last,
                            ours,
                            None,
                        ));
                    }
                    // The breakpoint on the vector stops QEMU.
                    self.leader.stop(false)?;
                    let end = match expect {
                        Expect::Exception(pc, end) if pc == from_pc => end,
                        Expect::Abort(pc) if pc == from_pc => End::DataAbort,
                        Expect::Exception(pc, _) | Expect::Abort(pc) => {
                            break Err(differed(
                                format!(
                                    "we stop with an exception at {pc:#x} and QEMU takes one from {from_pc:#x}"
                                ),
                                &last,
                                ours,
                                None,
                            ));
                        }
                        // No record for the instruction at our PC. Either the
                        // PC is not a multiple of four (QEMU writes a record
                        // for some of those and not for others), or QEMU
                        // could not fetch, and then our memory must hold
                        // nothing there. Which exception it is when both
                        // hold, the exception card (C6) compares.
                        Expect::Instruction if from_pc == ours.pc && ours.pc & 3 != 0 => {
                            End::PcAlignment
                        }
                        Expect::Instruction
                            if from_pc == ours.pc && self.word(ours.pc).is_none() =>
                        {
                            End::FetchAbort
                        }
                        Expect::Instruction => {
                            break Err(differed(
                                format!(
                                    "QEMU took an exception from {from_pc:#x} where we would run on"
                                ),
                                &last,
                                ours,
                                None,
                            ));
                        }
                    };
                    // An exception changes no general register, so they are
                    // the registers after the last instruction.
                    if end == End::DataAbort {
                        // A store of several parts can fail after a different
                        // count of parts on the two sides. That is not
                        // compared: our memory takes QEMU's bytes.
                        let ours = our_writes(&partial);
                        let theirs = written(qemu_writes.iter().map(qemu_bytes));
                        if ours != theirs {
                            self.partial_stores += 1;
                            for address in ours.keys().chain(theirs.keys()) {
                                if let Ok(byte) = self.leader.memory(*address, 1) {
                                    let _ = self.cpu.memory.write(*address, &byte);
                                }
                            }
                        }
                        qemu_writes.clear();
                    }
                    if let Some(what) = self.writes_differ(&last, &qemu_writes) {
                        break Err(differed(what, &last, ours, None));
                    }
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
                    let mut vectors_after = None;
                    if last.is_some() && self.leader.has_vectors() {
                        // No instruction record follows the last
                        // instruction, so its vector registers are read.
                        let theirs = self.leader.vectors()?;
                        let mut mine = Vectors::of(&self.cpu.state);
                        mine.fpsr = 0;
                        mine.fpcr = 0;
                        let found = mine.difference(&theirs);
                        if !found.is_empty() {
                            break Err(differed(
                                format!(
                                    "the vector registers at an exception, ours / QEMU's: {}",
                                    found.join("; ")
                                ),
                                &last,
                                ours,
                                Some(after),
                            ));
                        }
                        vectors_after = Some(Vectors::of(&self.cpu.state));
                    }
                    if let Some(last) = last.take() {
                        verified.push(Verified {
                            encoding: last.encoding,
                            record: last.record,
                            before: last.before,
                            after: ours,
                            vectors: last.vectors.zip(vectors_after),
                            accesses: last.accesses,
                        });
                    }
                    break Ok(end);
                }
                Trace::Write {
                    address,
                    size,
                    low,
                    high,
                } => qemu_writes.push((address, size, low, high)),
            }
        };
        Ok(match result {
            Ok(end) => Outcome::Agreed { end, verified },
            Err(difference) => {
                // Leave QEMU stopped, so that the next round can start.
                self.interrupt(&qemu_writes)?;
                Outcome::Differed {
                    difference,
                    verified,
                }
            }
        })
    }
}
