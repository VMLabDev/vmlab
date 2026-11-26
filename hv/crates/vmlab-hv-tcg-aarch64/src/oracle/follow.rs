//! The follower (DESIGN.md O3, steps 3 to 5): one round of lockstep.
//!
//! A round loads memory and registers into QEMU and into our CPU, lets QEMU
//! run, and steps our interpreter beside the records of the plugin. Before
//! each instruction the registers of both sides must be the same: X0 to
//! X30, SP, PC and CPSR, and V0 to V31, FPSR and FPCR when the plugin gives
//! them. The memory writes of each instruction must be the same too. A
//! round ends at the first exception, at its instruction budget, or at the
//! first difference.
//!
//! Our memory mirrors the RAM of QEMU from one round to the next, and the
//! vector registers are not set by a round, so both must stay the same on
//! the two sides also where QEMU runs on after our side stopped following.
//! QEMU runs ahead of the records that our side has read, by what the FIFO
//! of the plugin holds. When a round ends without an exception of QEMU, the
//! follower takes the memory writes that QEMU made since, reads its vector
//! registers before the next round, and says that the exclusive monitors
//! may differ (`monitor_stale`).
//!
//! A round of system instructions (card C6, `Round::reset`) is followed in
//! another way: both sides start from a reset, both take the exceptions of
//! the stream, and the round goes on at the vector when the vector is in
//! the mirrored memory. Such a round also compares the system registers of
//! `Follower::compared`. QEMU leads there for what is not a function of the
//! CPU state (DESIGN.md O4): the value of a counter read and the answer of a
//! host call are taken from its next record.

use super::leader::{Ahead, Leader, VECTOR};
use super::{Compared, Core, Trace, Vectors, normalise, system_difference, system_of};
use crate::exception::{Cause, Exception};
use crate::exec::{Access, Cpu, Stop, Taken};
use crate::generated::decode::{Record, decode};
use crate::state::State;
use crate::sysreg::QEMU_CORTEX_A53;
use crate::timer::Clock;
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
    /// V0 to V31, FPSR and FPCR at the start. `Some`: they are loaded on both
    /// sides and compared before every instruction, and the leader must have
    /// been started with vectors. `None`: they are left alone. A leader that
    /// was started with vectors still sends them, and then they carry over
    /// from the last round on both sides and are compared too.
    pub vectors: Option<Vectors>,
    /// The most instructions to follow.
    pub budget: u64,
    /// The round ends when one PC is about to run more often than this. It
    /// keeps a loop of the stream from filling the round with repeats.
    pub visits: u32,
    /// Addresses from the first up to the second that do not count for
    /// `visits`: the vector table of a system round, which every exception
    /// of the stream runs through.
    pub exempt: (u64, u64),
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
    /// The vector registers of QEMU before and after, when the plugin
    /// gives them.
    pub vectors: Option<(Vectors, Vectors)>,
    /// The loads and stores that our side made. The stores are those that
    /// QEMU made: the round compares them.
    pub accesses: Vec<Access>,
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
    /// A store of the round went into its own code, and QEMU ran the word
    /// that was there before, from the block that it had translated. Our
    /// side runs the new word. Both are what a CPU with caches may do until
    /// the guest synchronizes them, so nothing is compared (DESIGN.md U3).
    StaleCode,
    /// Both sides took another exception to a vector outside the mirrored
    /// memory.
    Exception,
    /// The stream wrote a value or ran an instruction that the lockstep does
    /// not follow (`system::unfollowed`): the round ends before it.
    Unfollowed(&'static str),
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
const RECORD_TIMEOUT: Duration = Duration::from_secs(20);

/// The count of the system counter that our side starts a round with. The
/// streams keep the compare value of a timer far below it or far above it,
/// so that the status of a timer does not depend on the time of the host.
pub const ROUND_COUNT: u64 = 1 << 32;

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
    /// The system registers that the leader sends with each instruction.
    pub compared: Vec<Compared>,
    /// The exceptions that both sides took in the rounds of system
    /// instructions, by their kind.
    pub exceptions: BTreeMap<&'static str, u64>,
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

/// What a step of our side is for a round that ends at the first exception:
/// such a round wants the registers as they were before the exception.
enum Plain {
    Stop(Stop),
    Undefined(u64),
    PcAlignment(u64),
    /// Another exception of the guest, at this PC.
    Exception(u64),
}

/// The instruction that our side ran last and that the next record of QEMU
/// verifies.
struct SystemLast {
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

/// The kind of an exception, for the counts of a run.
fn kind_of(cause: Cause) -> &'static str {
    match cause {
        Cause::Interrupt(_) => "interrupt",
        Cause::Exception(exception) => match exception {
            Exception::Undefined => "undefined instruction",
            Exception::WaitTrap(_) => "trapped WFI or WFE",
            Exception::FpAccess => "FP access trap",
            Exception::IllegalState => "illegal execution state",
            Exception::SupervisorCall(_) => "SVC",
            Exception::SystemTrap(_) => "trapped system instruction",
            Exception::InstructionAbort(_) => "instruction abort",
            Exception::PcAlignment => "PC alignment",
            Exception::Breakpoint(_) => "BRK",
        },
    }
}

fn end_of(cause: Cause) -> End {
    match cause {
        Cause::Exception(Exception::Undefined) => End::Undefined,
        Cause::Exception(Exception::InstructionAbort(_)) => End::FetchAbort,
        Cause::Exception(Exception::PcAlignment) => End::PcAlignment,
        _ => End::Exception,
    }
}

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
            compared: Vec::new(),
            exceptions: BTreeMap::new(),
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

    /// Give the vector registers, FPSR and FPCR these values on both sides.
    pub fn set_vectors(&mut self, vectors: &Vectors) -> io::Result<()> {
        self.leader.set_vectors(vectors)?;
        vectors.apply(&mut self.cpu.state);
        self.stale = false;
        Ok(())
    }

    fn load(&mut self, round: &Round) -> io::Result<()> {
        if round.reset {
            self.leader.reset()?;
        }
        for (address, bytes) in &round.memory {
            self.load_memory(*address, bytes)?;
        }
        if round.start.cpsr & 0x0fff_ffff != super::CPSR_EL1H {
            return Err(io::Error::other("a round must start at EL1 with SP_EL1"));
        }
        // Our CPU after a reset, as the CPU of the lockstep.
        let mut state = State::reset_as(round.start.pc, &QEMU_CORTEX_A53);
        round.start.apply(&mut state);
        if !round.reset && self.leader.has_vectors() {
            // The leader enabled FP once, and no round of this kind writes
            // CPACR_EL1.
            state.enable_fp();
        }
        // A round sets the core registers. The vector registers stay what
        // the last round left on both sides.
        if self.leader.has_vectors() && round.vectors.is_none() {
            if self.stale {
                let vectors = self.leader.vectors()?;
                vectors.apply(&mut self.cpu.state);
                self.stale = false;
            }
            Vectors::of(&self.cpu.state).apply(&mut state);
        }
        self.leader.set_core(&round.start)?;
        self.cpu.state = state;
        if let Some(vectors) = &round.vectors {
            self.leader.set_vectors(vectors)?;
            vectors.apply(&mut self.cpu.state);
            self.stale = false;
        }
        self.cpu.clock = Clock::Manual(ROUND_COUNT);
        self.cpu.taken = None;
        self.cpu.take_accesses();
        Ok(())
    }

    /// One step of our side for a round that ends at the first exception.
    /// The CPU takes its exceptions itself (card C6). Such a round compares
    /// the registers before the exception, so they are put back.
    fn step_plain(&mut self) -> Result<(), Plain> {
        let before = self.cpu.state.clone();
        self.cpu.step().map_err(Plain::Stop)?;
        let Some(taken) = self.cpu.taken.take() else {
            return Ok(());
        };
        self.cpu.state = before;
        Err(match taken.cause {
            Cause::Exception(Exception::Undefined) => Plain::Undefined(taken.from),
            Cause::Exception(Exception::PcAlignment) => Plain::PcAlignment(taken.from),
            _ => Plain::Exception(taken.from),
        })
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
            fields
                .iter()
                .fold(record.info().value, |word, (name, value)| {
                    crate::with_field(record, name, word, *value).unwrap_or(word)
                })
        };
        let words = [
            // ldxp x1, x3, [x2] ; stxp w4, x1, x3, [x2]
            build(Record::LDXPX, &[("rt", 1), ("rt2", 3), ("rn", 2)]),
            build(
                Record::STXPX,
                &[("ws", 4), ("rt", 1), ("rt2", 3), ("rn", 2)],
            ),
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
            memory: vec![(
                pc,
                words.iter().flat_map(|word| word.to_le_bytes()).collect(),
            )],
            start,
            vectors: None,
            budget: 3,
            visits: 1,
            exempt: (0, 0),
            reset: false,
        };
        match self.run(&round)? {
            Outcome::Agreed { verified, .. }
                if verified.len() == 2 && verified[1].after.x[4] == 0 && !self.monitor_stale =>
            {
                Ok(())
            }
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
        if round.reset {
            self.run_system(round)
        } else {
            self.run_plain(round)
        }
    }

    /// A round that ends at the first exception, with its memory writes
    /// compared (cards C1 to C4).
    fn run_plain(&mut self, round: &Round) -> io::Result<Outcome> {
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
        // Every store of our side in this round: address and size.
        let mut stored: Vec<(u64, u8)> = Vec::new();
        let mut expect = Expect::Instruction;
        let mut followed = 0u64;
        let mut visits: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let differed = |what: String, last: &Option<Last>, ours: Core, qemu: Option<Core>| {
            Box::new(Difference {
                what,
                instruction: last.as_ref().map(|last| (last.encoding, last.before)),
                ours,
                qemu,
                vectors: None,
                vectors_before: None,
                system: Vec::new(),
                taken: None,
            })
        };
        let result = loop {
            let ours = Core::of(&self.cpu.state);
            match self.leader.next_record(RECORD_TIMEOUT)? {
                Trace::Instruction {
                    encoding,
                    core,
                    vectors,
                    ..
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
                    if round.vectors.is_some() && vectors.is_none() {
                        return Err(io::Error::other(
                            "a round compares the vector registers and the leader does not send them",
                        ));
                    }
                    if let Some(theirs) = vectors {
                        let ours_vectors = Vectors::of(&self.cpu.state);
                        if theirs != ours_vectors {
                            let mut difference = differed(
                                "the vector and FP registers before an instruction".into(),
                                &last,
                                ours,
                                Some(core),
                            );
                            difference.vectors = Some((ours_vectors, theirs));
                            difference.vectors_before = last.as_ref().and_then(|last| last.vectors);
                            break Err(difference);
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
                            system: None,
                            replayable: true,
                        });
                    }
                    // QEMU has memory that the round does not mirror: flash
                    // at 0 and devices. A branch that lands there is verified
                    // by the registers above, and the round ends.
                    let Some(word) = self.word(core.pc) else {
                        self.interrupt(&[])?;
                        break Ok(End::OutsideMemory);
                    };
                    if core.pc & 3 == 0
                        && word != encoding
                        && stored.iter().any(|(address, bytes)| {
                            core.pc.wrapping_sub(*address) < u64::from(*bytes)
                                || address.wrapping_sub(core.pc) < 4
                        })
                    {
                        self.interrupt(&[])?;
                        break Ok(End::StaleCode);
                    }
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
                    let stepped = self.step_plain();
                    let accesses = self.cpu.take_accesses();
                    stored.extend(
                        accesses
                            .iter()
                            .filter(|access| access.write)
                            .map(|access| (access.address, access.bytes)),
                    );
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
                        Err(Plain::Undefined(pc)) => expect = Expect::Exception(pc, End::Undefined),
                        Err(Plain::PcAlignment(pc)) => {
                            expect = Expect::Exception(pc, End::PcAlignment)
                        }
                        Err(Plain::Exception(pc)) => expect = Expect::Exception(pc, End::Exception),
                        Err(Plain::Stop(Stop::Alignment { pc, .. })) => {
                            expect = Expect::Exception(pc, End::Alignment)
                        }
                        Err(Plain::Stop(Stop::DataAbort { pc, .. })) => {
                            partial = accesses;
                            expect = Expect::Abort(pc);
                        }
                        Err(Plain::Stop(Stop::Unimplemented { pc, record })) => {
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
                        Err(Plain::Stop(stop)) => {
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
                    // An exception changes no vector register either. A
                    // round that sets them reads them at every exception.
                    // Another one reads them where no instruction record
                    // follows the last instruction.
                    let mut vectors_after = None;
                    if round.vectors.is_some() || (last.is_some() && self.leader.has_vectors()) {
                        let theirs = self.leader.vectors()?;
                        let ours_vectors = Vectors::of(&self.cpu.state);
                        if theirs != ours_vectors {
                            let mut difference = differed(
                                "the vector and FP registers at an exception".into(),
                                &last,
                                ours,
                                Some(after),
                            );
                            difference.vectors = Some((ours_vectors, theirs));
                            difference.vectors_before = last.as_ref().and_then(|last| last.vectors);
                            break Err(difference);
                        }
                        vectors_after = Some(ours_vectors);
                    }
                    if let Some(last) = last.take() {
                        verified.push(Verified {
                            encoding: last.encoding,
                            record: last.record,
                            before: last.before,
                            after: ours,
                            vectors: last.vectors.zip(vectors_after),
                            accesses: last.accesses,
                            system: None,
                            replayable: true,
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

    /// Stop QEMU after an exception to `vector`: at the breakpoint when the
    /// vector is the one that has it, else by asking.
    fn halt(&mut self, vector: u64) -> io::Result<()> {
        if self.at_break(vector) {
            // QEMU stopped at the vector and ran nothing after it.
            self.leader.stop(false)?;
            Ok(())
        } else {
            self.interrupt(&[])
        }
    }

    /// Whether QEMU stops by itself at an exception to `vector`.
    fn at_break(&self, vector: u64) -> bool {
        self.leader.vector_break() && vector == VECTOR
    }

    /// Both sides took the same exception to `taken.to`.
    fn after_exception(&mut self, taken: Taken) -> io::Result<Stepped> {
        *self.exceptions.entry(kind_of(taken.cause)).or_default() += 1;
        if taken.to & 3 == 0 && self.word(taken.to).is_some() {
            return Ok(Stepped::On(Adopt::Nothing));
        }
        // The vector is outside the mirrored memory: the round ends.
        self.halt(taken.to)?;
        if self.at_break(taken.to) {
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
                        self.interrupt(&[])?;
                        Ok(Stepped::End(End::Unlifted(record)))
                    }
                }
            }
            Err(stop) => Ok(Stepped::Differ(format!(
                "our side cannot run the instruction that QEMU runs: {stop:?}"
            ))),
        }
    }

    /// A round of system instructions: both sides start from a reset and
    /// take the exceptions of the stream (card C6).
    fn run_system(&mut self, round: &Round) -> io::Result<Outcome> {
        self.monitor_stale = false;
        self.load(round)?;
        self.leader.resume()?;
        let mut verified = Vec::new();
        let mut last: Option<SystemLast> = None;
        let compare_vectors = round.vectors.is_some();
        let compare_system = !self.compared.is_empty();
        let mut followed = 0u64;
        let mut visits: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let differed = |what: String, last: &Option<SystemLast>, ours: Core, qemu: Option<Core>| {
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
                            accesses: Vec::new(),
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
                    if !(round.exempt.0..round.exempt.1).contains(&core.pc) {
                        *seen += 1;
                    }
                    if *seen > round.visits {
                        self.interrupt(&[])?;
                        break Ok(End::Loop);
                    }
                    if compare_system
                        && let Some(why) =
                            super::system::unfollowed(encoding, &core.x, &self.cpu.state)
                    {
                        self.interrupt(&[])?;
                        break Ok(End::Unfollowed(why));
                    }
                    followed += 1;
                    let stepped = self.cpu.step();
                    // A system round holds no load and no store.
                    self.cpu.take_accesses();
                    let reaction = self.react(stepped, Some(encoding), None)?;
                    if let Stepped::On(adopt) = reaction {
                        last = Some(SystemLast {
                            encoding,
                            record: decode(encoding).map(|decoded| decoded.record),
                            before: core,
                            vectors: ours_vectors,
                            system: ours_system,
                            adopt,
                        });
                    } else {
                        last = Some(SystemLast {
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
                    last = self.word(from_pc).map(|encoding| SystemLast {
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
                    if compare_system {
                        // Our system registers that are not at their reset
                        // value. They were QEMU's before the instruction.
                        let reset = super::corpus::reset_system(&self.compared);
                        let now = system_of(&self.cpu.state, &self.compared);
                        let named = super::corpus::named(&self.compared, &reset, &now);
                        what.push_str(&format!("; {}", super::corpus::named_text("sys", &named)));
                    }
                    for _ in 0..4 {
                        if !compare_system {
                            break;
                        }
                        match self.leader.next_record(Duration::from_secs(1)) {
                            Ok(Trace::Instruction { system, core, .. }) => {
                                what.push_str(&format!("; QEMU goes on at {:#x} with", core.pc));
                                for (register, value) in self.compared.iter().zip(&system) {
                                    if matches!(
                                        register.qemu,
                                        "ESR_EL1" | "FAR_EL1" | "ELR_EL1" | "SPSR_EL1" | "MDSCR_EL1"
                                    ) {
                                        what.push_str(&format!(" {}={value:#x}", register.qemu));
                                    }
                                }
                                break;
                            }
                            Ok(Trace::Discontinuity {
                                kind,
                                from_pc,
                                to_pc,
                            }) => what.push_str(&format!(
                                "; then a discontinuity of the kind {kind} from {from_pc:#x} to {to_pc:#x}"
                            )),
                            Ok(Trace::Write { .. }) => {}
                            Err(error) => {
                                what.push_str(&format!("; then {error}"));
                                break;
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
                self.interrupt(&[])?;
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
        assert_eq!(
            end_of(Cause::Exception(Exception::Undefined)),
            End::Undefined
        );
        assert_eq!(
            kind_of(Cause::Exception(Exception::FpAccess)),
            "FP access trap"
        );
        assert_eq!(
            end_of(Cause::Exception(Exception::SupervisorCall(1))),
            End::Exception
        );
    }
}
