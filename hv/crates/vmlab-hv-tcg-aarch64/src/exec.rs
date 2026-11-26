//! The interpreter loop: translate a block at the PC, run it on the slot
//! executor of `vmlab-hv-tcg-ir`, write the registers back, repeat.
//!
//! Addresses are physical addresses on a `Memory`: the MMU is card C8. The
//! loop takes the exceptions and the interrupts of the guest itself
//! (`exception.rs`) and runs the system instructions that end a block
//! (`Trap`). It stops for what only its owner can serve: a hypercall, a
//! `WFI` with no interrupt line high, the end of a budget, and an
//! instruction that has no lifter yet. A load or a store that fails still
//! stops it too (`Stop::DataAbort`, `Stop::Alignment`): the translation card
//! (C8) gives those their syndromes.
//!
//! A block is pure scalar code with memory events between it: loads, stores
//! and guards, in the order of the instructions. Each event names an exit
//! that holds the registers as they were before its instruction, so a failed
//! access leaves the registers of that instruction untouched.
//!
//! The exclusive monitor of a CPU is the address of its last exclusive
//! load, the value that the load read, and the write generation of the
//! memory's reservation granules at that moment (`Memory::load_reserved`).
//! What it does on one CPU follows what the lockstep saw QEMU do (S6,
//! DESIGN.md U1): an exclusive store writes when its address is the marked
//! one and the memory, read at the size of the store, still holds the value.
//! The sizes of the load and of the store need not be the same, which the
//! architecture leaves open. A store that finds another address, or no
//! mark, reads no memory and fails. Every exclusive store that completes
//! ends the mark. A store of this CPU between the two keeps the mark, and
//! an exception keeps it. An exception return and `CLREX` end it.
//!
//! With more CPUs the store also asks that no other writer reached the
//! granules of the load (`Memory::store_conditional`), so it fails more
//! often than QEMU's: a write of another CPU fails it also when it left the
//! value as it was.
//!
//! Sources read: S6 for what a system instruction does at each level (the
//! lockstep of card C6 and the probe of the system registers). This file
//! holds no other instruction semantics.

use crate::exception::{self, Cause, Exception};
use crate::generated::decode::{Record, decode};
use crate::lift::{A64Exit, A64Next, Builder, Lifted, PstateField, Trap, lift};
use crate::state::{Context, State, slot};
use crate::sysreg::{self, Denied, Environment, reg, sctlr};
use crate::timer::{self, Clock};
use vmlab_hv_tcg::{Error, Memory};
use vmlab_hv_tcg_ir::native::block::{BlockOf, Event};
use vmlab_hv_tcg_ir::native::linear;
use vmlab_hv_tcg_ir::native::packed::Packed;
use vmlab_hv_tcg_ir::native::{
    Graph, HelperFn, Operation, ScalarId, ScalarType, dce, mask, schedule,
};

/// The most instructions in one block.
pub const BLOCK_INSTRUCTIONS: u32 = 64;

/// The block cache is direct-mapped by the PC.
const CACHE_ENTRIES: usize = 1 << 12;

/// The pages that hold translated code are a bit set over the low bits of
/// the page number. Two pages with the same low bits share a bit, which
/// costs a slower store and nothing else.
const CODE_PAGE_BITS: usize = 1 << 16;

/// How a guest calls what is above EL1 (DESIGN.md G4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conduit {
    Hvc,
    Smc,
}

/// Why the loop stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The instruction budget of `run` is used up.
    Budget,
    /// The record is decoded and has no lifter yet. The PC of the state is
    /// the instruction, and no part of it has changed the state.
    Unimplemented { pc: u64, record: Record },
    /// The memory holds nothing at the address of a load or a store: a data
    /// abort. The PC of the state is the instruction, which has changed no
    /// register. A store of several parts (a pair, `ST2` to `ST4`) has
    /// written the parts before the one that failed.
    DataAbort { pc: u64, address: u64, write: bool },
    /// An access to an address that is not a multiple of its size: an
    /// alignment fault. On this flat memory every load and store asks for
    /// its alignment, as QEMU does with the MMU off.
    Alignment { pc: u64, address: u64 },
    /// `HVC` or `SMC` at EL1, at the address `pc`. The PC of the state is
    /// the instruction after it. The machine serves the call (PSCI on
    /// `virt`) and runs on, or refuses it with `Cpu::refuse_hypercall`.
    Hypercall {
        conduit: Conduit,
        immediate: u16,
        pc: u64,
    },
    /// `WFI` with no interrupt line high. The PC of the state is the
    /// instruction after it. The machine waits for a line and runs on.
    Halt,
    /// An error of the host, not of the guest.
    Host(String),
}

/// An exception or an interrupt that the CPU took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taken {
    pub cause: Cause,
    /// The instruction that took it, or that an interrupt came before.
    pub from: u64,
    /// The vector.
    pub to: u64,
}

/// What the guest asked for that this CPU does not have (DESIGN.md G3f). The
/// guest got the Undefined Instruction exception, as the architecture says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Unknown {
    /// A word that no selected record matches.
    Encoding(u32),
    /// `MRS` or `MSR` of a register without a row, or one that the CPU
    /// model leaves out.
    Register { encoding: u16, write: bool },
}

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encoding(word) => write!(f, "undefined instruction {word:08x}"),
            Self::Register { encoding, write } => write!(
                f,
                "{} of the system register {}, which this CPU does not have",
                if *write { "write" } else { "read" },
                sysreg::name(*encoding)
            ),
        }
    }
}

/// The hook for a register without a row. `write`: the value of an `MSR`.
/// `Some`: the machine has the register, and the value is what an `MRS`
/// reads.
pub type RegisterHook = fn(state: &mut State, encoding: u16, write: Option<u64>) -> Option<u64>;

/// The hook for a word that no selected record matches. `true`: the machine
/// ran it, and the CPU goes on at the next instruction.
pub type InstructionHook = fn(state: &mut State, word: u32) -> bool;

/// What a machine adds to the core (DESIGN.md G3e, A5): registers of the
/// IMPLEMENTATION DEFINED space and instructions of its own. Each hook is
/// asked before the Undefined Instruction exception is taken.
#[derive(Clone, Copy, Default)]
pub struct Hooks {
    pub register: Option<RegisterHook>,
    pub instruction: Option<InstructionHook>,
}

/// Why no block starts at the PC.
enum Refused {
    Stop(Stop),
    Exception(Exception),
}

/// One access that the CPU made, for a harness that compares them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub address: u64,
    /// 1, 2, 4, 8 or 16.
    pub bytes: u8,
    /// The bytes, little-endian.
    pub value: u128,
    pub write: bool,
}

/// The mark that an exclusive load leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reservation {
    address: u64,
    /// The size of the load.
    bytes: u8,
    generation: u64,
}

/// Why an access did not complete.
enum Fault {
    /// The memory holds nothing there.
    Memory,
    /// The address is not a multiple of the size of an exclusive store.
    Unaligned,
    Host(String),
}

impl From<Error> for Fault {
    fn from(error: Error) -> Self {
        match error {
            Error::Memory(_) => Self::Memory,
            other => Self::Host(other.to_string()),
        }
    }
}

/// What a block needs beside the registers and the memory.
struct Side {
    exclusive: Option<Reservation>,
    /// The value of the last exclusive load, with zeros above its size. A
    /// load of 16 bytes leaves its lower half here and its upper half in
    /// `exclusive_high`, which no other load changes.
    exclusive_value: u64,
    exclusive_high: u64,
    /// The accesses since the caller last took them, when it asked for them.
    log: Option<Vec<Access>>,
    code_pages: Vec<u64>,
    /// Stores of this CPU since the caches were last checked.
    own_writes: u64,
    /// One of them went into a page that holds translated code.
    code_written: bool,
}

impl Side {
    fn new() -> Self {
        Self {
            exclusive: None,
            exclusive_value: 0,
            exclusive_high: 0,
            log: None,
            code_pages: vec![0; CODE_PAGE_BITS / 64],
            own_writes: 0,
            code_written: false,
        }
    }

    fn page_bit(address: u64) -> (usize, u64) {
        let page = (address >> 12) as usize & (CODE_PAGE_BITS - 1);
        (page / 64, 1 << (page % 64))
    }

    fn mark_code(&mut self, address: u64) {
        let (word, bit) = Self::page_bit(address);
        self.code_pages[word] |= bit;
    }

    fn is_code(&self, address: u64, bytes: u8) -> bool {
        let (word, bit) = Self::page_bit(address);
        let (last_word, last_bit) = Self::page_bit(address.wrapping_add(u64::from(bytes) - 1));
        self.code_pages[word] & bit != 0 || self.code_pages[last_word] & last_bit != 0
    }

    fn wrote(&mut self, address: u64, bytes: u8, value: u128) {
        self.own_writes += 1;
        if self.is_code(address, bytes) {
            self.code_written = true;
        }
        if let Some(log) = &mut self.log {
            log.push(Access {
                address,
                bytes,
                value,
                write: true,
            });
        }
    }

    fn load<M: Memory>(
        &mut self,
        memory: &mut M,
        address: u64,
        bytes: u8,
        exclusive: bool,
    ) -> Result<u128, Fault> {
        let mut buffer = [0u8; 16];
        let part = &mut buffer[..usize::from(bytes)];
        if exclusive {
            // A failed load leaves the monitor as it was.
            let generation = memory.load_reserved(address, part)?;
            let value = u128::from_le_bytes(buffer);
            self.exclusive = Some(Reservation {
                address,
                bytes,
                generation,
            });
            self.exclusive_value = value as u64;
            if bytes == 16 {
                self.exclusive_high = (value >> 64) as u64;
            }
        } else {
            memory.read(address, part)?;
        }
        let value = u128::from_le_bytes(buffer);
        if let Some(log) = &mut self.log {
            log.push(Access {
                address,
                bytes,
                value,
                write: false,
            });
        }
        Ok(value)
    }

    fn store<M: Memory>(
        &mut self,
        memory: &mut M,
        address: u64,
        bytes: u8,
        value: u128,
    ) -> Result<(), Fault> {
        let buffer = value.to_le_bytes();
        let part = &buffer[..usize::from(bytes)];
        match self.exclusive {
            None => memory.write(address, part)?,
            Some(reservation) => {
                // The store moves the write generation of its granules, and
                // those can be the reserved ones. A reservation that was
                // intact takes the new generation. The lock keeps the store
                // of another CPU from landing between the check and the new
                // generation, where it would be taken for our own.
                let lock = memory.transaction_lock();
                let _atomic = lock.as_ref().map(|lock| lock.lock());
                let len = usize::from(reservation.bytes);
                let intact = memory.reservation_generation(reservation.address, len)
                    == reservation.generation;
                memory.write(address, part)?;
                if intact {
                    self.exclusive = Some(Reservation {
                        generation: memory.reservation_generation(reservation.address, len),
                        ..reservation
                    });
                }
            }
        }
        self.wrote(address, bytes, value);
        Ok(())
    }

    /// Whether the store wrote, and whether it reports success. The two are
    /// the same thing unless the store is smaller than the value of its
    /// load: QEMU then writes where the low bytes agree and reports failure
    /// unless the whole value agrees, and so does this.
    fn store_exclusive<M: Memory>(
        &mut self,
        memory: &mut M,
        address: u64,
        bytes: u8,
        value: u128,
    ) -> Result<(bool, bool), Fault> {
        // QEMU keeps "no mark" as the address 2^64 - 1, so for it a store
        // there is at the marked address also without a mark, and is an
        // access that can fault. The architecture leaves open whether an
        // exclusive store that fails takes the fault of its access, so the
        // same is done here. Such a store never writes.
        let marked = self.exclusive.map_or(u64::MAX, |mark| mark.address);
        if marked != address {
            // Another address: no memory is looked at.
            self.exclusive = None;
            return Ok((false, false));
        }
        // From here the store is an access. One that faults leaves the
        // mark, as its instruction did not complete.
        if address & (u64::from(bytes) - 1) != 0 {
            return Err(Fault::Unaligned);
        }
        let buffer = value.to_le_bytes();
        let part = &buffer[..usize::from(bytes)];
        let lock = memory.transaction_lock();
        let _atomic = lock.as_ref().map(|lock| lock.lock());
        let mut now = [0u8; 16];
        memory.read(address, &mut now[..usize::from(bytes)])?;
        let now = u128::from_le_bytes(now);
        let Some(reservation) = self.exclusive.take() else {
            return Ok((false, false));
        };
        let expected = u128::from(self.exclusive_value) | u128::from(self.exclusive_high) << 64;
        // The memory holds the value, as far as the store reaches.
        let same = now == expected & bits_mask(bytes);
        // The memory holds the whole value: 64 bits, or 128 for a pair of
        // 64-bit registers.
        let whole = same && (bytes == 16 || now == u128::from(self.exclusive_value));
        if !same {
            return Ok((false, false));
        }
        let wrote = if bytes == reservation.bytes {
            memory.store_conditional(address, part, reservation.generation)?
        } else if memory.reservation_generation(reservation.address, usize::from(reservation.bytes))
            == reservation.generation
        {
            // A store of another size than its load: the granules of the
            // load are untouched, and the lock is held.
            memory.write(address, part)?;
            true
        } else {
            false
        };
        if wrote {
            self.wrote(address, bytes, value);
        }
        Ok((wrote, wrote && whole))
    }
}

/// A memory event, a guard or a helper call of a block, between its pure
/// code.
#[derive(Clone, Copy, Debug)]
enum Effect {
    Read {
        address: u32,
        result: u32,
        bytes: u8,
        exclusive: bool,
        exit: u32,
    },
    Write {
        address: u32,
        value: u32,
        bytes: u8,
        /// The scratch slot of the result of an exclusive store.
        exclusive: Option<u32>,
        exit: u32,
    },
    Guard {
        condition: u32,
        expected: bool,
        exit: u32,
    },
    /// The helper call of this index in `calls` (R10).
    Call(u32),
}

/// Where a block can leave before its end.
#[derive(Clone, Debug)]
struct Leave {
    /// Register slot, scratch slot: copied out when the block leaves here.
    /// The PC is one of them.
    outputs: Vec<(u32, u32)>,
    /// The instructions of the block that are complete here.
    completed: u32,
    /// The scratch slot of the address of an alignment fault.
    address: Option<u32>,
}

/// Why an effect did not complete.
enum Failed {
    /// The condition of a guard did not hold.
    Guard,
    /// A load or a store, with its address and whether it is a store.
    Access(Fault, u64, bool),
    /// A store into a page of translated code, in a block of more than one
    /// instruction.
    Retry,
}

/// Why a block left before its end.
enum Halt {
    Stop(Stop),
    /// The CPU took an exception or an interrupt without an instruction of
    /// the block. It costs one instruction of a budget, so that a guest
    /// with a broken vector does not hold the loop for ever.
    Taken,
    /// A store into a page that holds translated code, in a block of more
    /// than one instruction. Nothing of the instruction is lost: the loop
    /// runs it alone and then drops the translated blocks.
    Retry,
}

/// A block as the loop runs it.
struct Compiled {
    pc: u64,
    context: Context,
    instructions: u32,
    code: Vec<Packed>,
    /// Each effect with the count of `code` entries that run before it.
    effects: Vec<(u32, Effect)>,
    leaves: Vec<Leave>,
    /// The scratch slots. The constants are in place.
    values: Vec<u128>,
    /// The helper calls (R10), in the order in which they run.
    calls: Vec<Call>,
    /// Scratch slot, register slot: copied in before the block runs.
    inputs: Vec<(u32, u32)>,
    /// Register slot, scratch slot: copied out after it.
    outputs: Vec<(u32, u32)>,
    /// The last instruction, when the loop runs it: its address and what
    /// it does.
    terminal: Option<(u64, Trap)>,
}

/// The most arguments of a helper.
const CALL_ARGUMENTS: usize = 8;

/// One helper call of a block. Its place among the pure code is that of its
/// `Effect::Call`.
struct Call {
    function: HelperFn,
    /// The scratch slots of the arguments.
    args: Vec<u32>,
    /// The scratch slot of each result, with the mask of its type.
    results: Vec<(u32, u128)>,
}

fn host(message: &str) -> Stop {
    Stop::Host(message.to_string())
}

fn narrow(value: usize) -> Result<u32, Stop> {
    u32::try_from(value).map_err(|_| host("a block with too many slots"))
}

/// The mask of the type of a scalar that a helper fills.
fn result_mask(graph: &Graph, id: ScalarId) -> u128 {
    match graph.scalars[id].ty {
        ScalarType::Bits(width) => mask(width),
        ScalarType::Bool => 1,
        ScalarType::Integer => u128::MAX,
    }
}

/// Put a helper call after the pure code that the block has so far.
fn push_call(compiled: &mut Compiled, call: Call) -> Result<(), Stop> {
    let index = narrow(compiled.calls.len())?;
    compiled.calls.push(call);
    compiled
        .effects
        .push((narrow(compiled.code.len())?, Effect::Call(index)));
    Ok(())
}

/// Append the pure scalars that `roots` need and the block has not computed
/// yet: constants and inputs are bound, a pure helper call becomes a `Call`,
/// and the rest is packed.
fn emit(
    compiled: &mut Compiled,
    graph: &Graph,
    slots: &[usize],
    seen: &mut [bool],
    roots: &[ScalarId],
) -> Result<(), Stop> {
    let slot_count = compiled.values.len();
    for id in schedule::scalars(graph, roots, seen) {
        match &graph.scalars[id].operation {
            Operation::Constant(value) => compiled.values[slots[id]] = *value,
            Operation::Input(register) => {
                compiled
                    .inputs
                    .push((narrow(slots[id])?, narrow(*register)?));
            }
            // A view of another scalar shares its slot.
            Operation::Refine(_) => {}
            // The event of the effect writes its slot.
            Operation::Effect(_) => {}
            // A pure helper call runs at its place among the pure code.
            Operation::Call { helper, args } => {
                if args.len() > CALL_ARGUMENTS {
                    return Err(host("a helper with too many arguments"));
                }
                let call = Call {
                    function: helper.0.function,
                    args: args
                        .iter()
                        .map(|arg| narrow(slots[*arg]))
                        .collect::<Result<_, _>>()?,
                    results: vec![(narrow(slots[id])?, result_mask(graph, id))],
                };
                push_call(compiled, call)?;
            }
            _ => {
                let mut instruction = linear::lower(graph, id)
                    .ok_or_else(|| host("a scalar without a linear form"))?;
                instruction.remap(slots);
                let mut packed = Packed::new(slots[id], instruction)
                    .ok_or_else(|| host("a scalar without a packed form"))?;
                if let Operation::Binary(_, _, right) = &graph.scalars[id].operation
                    && let Some(value) = graph.literal(*right)
                    && let Some(immediate) = packed.with_immediate(value)
                {
                    packed = immediate;
                }
                if !packed.valid_slots(slot_count, slot_count) {
                    return Err(host("a packed instruction outside the scratch slots"));
                }
                compiled.code.push(packed);
            }
        }
    }
    Ok(())
}

fn exit_roots(exit: &A64Exit) -> Vec<ScalarId> {
    let mut roots: Vec<ScalarId> = exit
        .writeback
        .writes
        .iter()
        .map(|(_, scalar)| *scalar)
        .collect();
    match &exit.next {
        A64Next::Pc(next) => roots.push(*next),
        A64Next::Exception { syndrome, .. } => roots.extend(syndrome),
        A64Next::Trap { .. } => {}
    }
    roots
}

fn compile(
    pc: u64,
    context: Context,
    instructions: u32,
    mut block: BlockOf<A64Exit>,
) -> Result<Compiled, Stop> {
    dce::prune_scalars(&mut block);
    let graph = &block.graph;
    let (slots, slot_count) = linear::slots(graph);
    let mut seen = vec![false; graph.scalars.len()];
    let mut compiled = Compiled {
        pc,
        context,
        instructions,
        code: Vec::new(),
        effects: Vec::new(),
        leaves: Vec::new(),
        values: vec![0; slot_count],
        calls: Vec::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        terminal: None,
    };
    // The exits that events leave through, by their number in the block.
    let mut leave_of = vec![None; block.exits.len()];
    let mut ended = false;
    for event in &block.events {
        if ended {
            return Err(host("an event after the exit of a block"));
        }
        let (exit, mut roots) = match event {
            Event::Guard {
                condition, exit, ..
            } => (*exit, vec![*condition]),
            Event::ReadMemory { address, error, .. } => (*error, vec![*address]),
            Event::WriteMemory {
                address,
                value,
                error,
                ..
            } => (*error, vec![*address, *value]),
            // An ordered helper call, after what its arguments need. It
            // leaves through no exit.
            Event::Call {
                helper,
                args,
                results,
            } => {
                if !matches!(helper.0.function, HelperFn::Ordered(_))
                    || args.len() > CALL_ARGUMENTS
                    || results.len() > CALL_ARGUMENTS
                {
                    return Err(host("an ordered call of a helper that is not one"));
                }
                emit(&mut compiled, graph, &slots, &mut seen, args)?;
                for result in results {
                    seen[*result] = true;
                }
                let call = Call {
                    function: helper.0.function,
                    args: args
                        .iter()
                        .map(|arg| narrow(slots[*arg]))
                        .collect::<Result<_, _>>()?,
                    results: results
                        .iter()
                        .map(|result| Ok((narrow(slots[*result])?, result_mask(graph, *result))))
                        .collect::<Result<_, Stop>>()?,
                };
                push_call(&mut compiled, call)?;
                continue;
            }
            Event::Exit(exit) => (*exit, Vec::new()),
            Event::Checkpoint(_) => return Err(host("an aarch64 block has no checkpoint")),
        };
        let payload = &block.exits[exit];
        roots.extend(exit_roots(payload));
        emit(&mut compiled, graph, &slots, &mut seen, &roots)?;
        let mut outputs = Vec::new();
        for (register, scalar) in &payload.writeback.writes {
            outputs.push((narrow(*register)?, narrow(slots[*scalar])?));
        }
        if let Event::Exit(_) = event {
            match payload.next {
                A64Next::Pc(next) => outputs.push((slot::PC as u32, narrow(slots[next])?)),
                // The loop runs the last instruction, with the PC at it.
                A64Next::Trap { pc, trap } => compiled.terminal = Some((pc, trap)),
                A64Next::Exception { .. } => {
                    return Err(host("a block that ends in the exit of an event"));
                }
            }
            compiled.outputs = outputs;
            ended = true;
            continue;
        }
        let leave = match leave_of[exit] {
            Some(leave) => leave,
            None => {
                let A64Next::Exception { syndrome, .. } = &payload.next else {
                    return Err(host("an event that leaves a block without an exception"));
                };
                let address = match syndrome.first() {
                    Some(scalar) => Some(narrow(slots[*scalar])?),
                    None => None,
                };
                let leave = narrow(compiled.leaves.len())?;
                compiled.leaves.push(Leave {
                    outputs,
                    completed: payload.completed,
                    address,
                });
                leave_of[exit] = Some(leave);
                leave
            }
        };
        let effect = match event {
            Event::Guard {
                condition,
                expected,
                ..
            } => Effect::Guard {
                condition: narrow(slots[*condition])?,
                expected: *expected,
                exit: leave,
            },
            Event::ReadMemory {
                address,
                bytes,
                exclusive,
                result,
                ..
            } => Effect::Read {
                address: narrow(slots[*address])?,
                result: narrow(slots[*result])?,
                bytes: *bytes,
                exclusive: *exclusive,
                exit: leave,
            },
            Event::WriteMemory {
                address,
                value,
                bytes,
                exclusive,
                result,
                ..
            } => Effect::Write {
                address: narrow(slots[*address])?,
                value: narrow(slots[*value])?,
                bytes: *bytes,
                exclusive: match (exclusive, result) {
                    (false, _) => None,
                    (true, Some(result)) => Some(narrow(slots[*result])?),
                    (true, None) => {
                        return Err(host("an exclusive store without a result"));
                    }
                },
                exit: leave,
            },
            Event::Call { .. } | Event::Exit(_) | Event::Checkpoint(_) => continue,
        };
        compiled
            .effects
            .push((narrow(compiled.code.len())?, effect));
    }
    if !ended {
        return Err(host("a block without an exit"));
    }
    Ok(compiled)
}

fn bits_mask(bytes: u8) -> u128 {
    if bytes >= 16 {
        u128::MAX
    } else {
        (1u128 << (u32::from(bytes) * 8)) - 1
    }
}

impl Compiled {
    #[inline(always)]
    fn pure(code: &[Packed], values: &mut [u128]) -> Result<(), Halt> {
        for packed in code {
            let Some(value) = packed.evaluate(values) else {
                return Err(Halt::Stop(host(
                    "an arithmetic operation outside its domain",
                )));
            };
            values[packed.output as usize] = value;
        }
        Ok(())
    }

    /// Leave through `exit`: the registers as they were before the
    /// instruction that could not complete.
    fn leave(&self, state: &mut State, exit: u32) -> &Leave {
        let leave = &self.leaves[exit as usize];
        for (register, scratch) in &leave.outputs {
            state.slots[*register as usize] = self.values[*scratch as usize];
        }
        leave
    }

    /// Make one effect. The error: the exit to leave through, and why.
    fn effect<M: Memory>(
        &mut self,
        effect: Effect,
        state: &mut State,
        memory: &mut M,
        side: &mut Side,
        single: bool,
    ) -> Result<(), (u32, Failed)> {
        let values = &mut self.values;
        match effect {
            Effect::Call(index) => {
                let call = &self.calls[index as usize];
                let mut args = [0u128; CALL_ARGUMENTS];
                for (arg, slot) in args.iter_mut().zip(&call.args) {
                    *arg = values[*slot as usize];
                }
                let args = &args[..call.args.len()];
                match call.function {
                    HelperFn::Pure(function) => {
                        let (slot, mask) = call.results[0];
                        values[slot as usize] = function(args) & mask;
                    }
                    // An ordered helper sees the state as it was at the
                    // entry of the block for every register slot: the slots
                    // are written back at the exit. What it may touch is
                    // the state that is not in a slot.
                    HelperFn::Ordered(function) => {
                        let mut results = [0u128; CALL_ARGUMENTS];
                        let count = call.results.len().min(CALL_ARGUMENTS);
                        function(&mut *state, args, &mut results[..count]);
                        for ((slot, mask), result) in call.results.iter().zip(results) {
                            values[*slot as usize] = result & mask;
                        }
                    }
                }
            }
            Effect::Guard {
                condition,
                expected,
                exit,
            } => {
                if (values[condition as usize] != 0) != expected {
                    return Err((exit, Failed::Guard));
                }
            }
            Effect::Read {
                address,
                result,
                bytes,
                exclusive,
                exit,
            } => {
                let at = values[address as usize] as u64;
                match side.load(memory, at, bytes, exclusive) {
                    Ok(value) => values[result as usize] = value,
                    Err(fault) => return Err((exit, Failed::Access(fault, at, false))),
                }
            }
            Effect::Write {
                address,
                value,
                bytes,
                exclusive,
                exit,
            } => {
                let at = values[address as usize] as u64;
                let value = values[value as usize] & bits_mask(bytes);
                if !single && side.is_code(at, bytes) {
                    return Err((exit, Failed::Retry));
                }
                match exclusive {
                    None => side
                        .store(memory, at, bytes, value)
                        .map_err(|fault| (exit, Failed::Access(fault, at, true)))?,
                    Some(result) => {
                        let (_, succeeded) = side
                            .store_exclusive(memory, at, bytes, value)
                            .map_err(|fault| (exit, Failed::Access(fault, at, true)))?;
                        values[result as usize] = u128::from(!succeeded);
                    }
                }
            }
        }
        Ok(())
    }

    /// Run the effects of the block, each after the pure code before it.
    /// `Ok`: the count of `code` entries that have run. `Err`: the block
    /// left before its end.
    #[inline(never)]
    fn effects<M: Memory>(
        &mut self,
        state: &mut State,
        memory: &mut M,
        side: &mut Side,
    ) -> Result<usize, (u32, Halt)> {
        // A block of one instruction makes its stores also into a page of
        // translated code, and the loop drops the blocks after it.
        let single = self.instructions == 1;
        let mut done = 0usize;
        for index in 0..self.effects.len() {
            let (before, effect) = self.effects[index];
            let before = before as usize;
            Self::pure(&self.code[done..before], &mut self.values).map_err(|halt| (0, halt))?;
            done = before;
            if let Err((exit, failed)) = self.effect(effect, state, memory, side, single) {
                let leave = self.leave(state, exit);
                let pc = state.pc();
                let halt = match failed {
                    Failed::Retry => Halt::Retry,
                    // The guards of this card are alignment checks, and
                    // their exit holds the address.
                    Failed::Guard => Halt::Stop(Stop::Alignment {
                        pc,
                        address: leave
                            .address
                            .map_or(0, |slot| self.values[slot as usize] as u64),
                    }),
                    Failed::Access(Fault::Memory, address, write) => {
                        Halt::Stop(Stop::DataAbort { pc, address, write })
                    }
                    Failed::Access(Fault::Unaligned, address, _) => {
                        Halt::Stop(Stop::Alignment { pc, address })
                    }
                    Failed::Access(Fault::Host(message), ..) => Halt::Stop(Stop::Host(message)),
                };
                return Err((leave.completed, halt));
            }
        }
        Ok(done)
    }

    /// Run the block. The count of instructions that completed, and why the
    /// block left before its end when it did.
    #[inline]
    fn run<M: Memory>(
        &mut self,
        state: &mut State,
        memory: &mut M,
        side: &mut Side,
    ) -> (u32, Option<Halt>) {
        for (scratch, register) in &self.inputs {
            self.values[*scratch as usize] = state.slots[*register as usize];
        }
        // A block without memory events is its pure code and nothing else.
        let done = if self.effects.is_empty() {
            0
        } else {
            match self.effects(state, memory, side) {
                Ok(done) => done,
                Err((completed, halt)) => return (completed, Some(halt)),
            }
        };
        if let Err(halt) = Self::pure(&self.code[done..], &mut self.values) {
            return (0, Some(halt));
        }
        for (register, scratch) in &self.outputs {
            state.slots[*register as usize] = self.values[*scratch as usize];
        }
        // The loop runs the last instruction with the PC at it.
        if let Some((pc, _)) = self.terminal {
            state.set_pc(pc);
        }
        (self.instructions, None)
    }
}

struct Cache {
    entries: Vec<Option<Box<Compiled>>>,
}

impl Cache {
    fn new() -> Self {
        let mut entries = Vec::new();
        entries.resize_with(CACHE_ENTRIES, || None);
        Self { entries }
    }

    fn index(pc: u64) -> usize {
        (pc >> 2) as usize & (CACHE_ENTRIES - 1)
    }

    fn clear(&mut self) {
        for entry in &mut self.entries {
            *entry = None;
        }
    }
}

/// One CPU on a memory.
pub struct Cpu<M> {
    pub state: State,
    pub memory: M,
    /// The two interrupt lines of the core (DESIGN.md F3, A6). A machine
    /// drives them from its interrupt controller. They are levels.
    pub irq: bool,
    pub fiq: bool,
    /// Where the system counter comes from.
    pub clock: Clock,
    pub hooks: Hooks,
    /// The last exception or interrupt that `step` or `run` took. `step`
    /// clears it first.
    pub taken: Option<Taken>,
    /// Exceptions and interrupts taken since the CPU was made.
    pub exceptions: u64,
    /// Instructions that have run since the CPU was made. An instruction
    /// that takes an exception counts.
    pub instructions: u64,
    /// Blocks translated since the CPU was made.
    pub translations: u64,
    /// What the guest asked for that the CPU does not have, each once. The
    /// owner of the CPU writes it to its log.
    pub unknown: std::collections::BTreeSet<Unknown>,
    blocks: Cache,
    /// Blocks of one instruction, for `step` and for the end of a budget.
    singles: Cache,
    /// The write generation of the memory that the caches were filled under.
    generation: u64,
    side: Side,
}

/// The most entries of `Cpu::unknown`. A guest that walks an encoding space
/// must not fill the memory of the host.
const UNKNOWN_ENTRIES: usize = 4096;

impl<M: Memory> Cpu<M> {
    pub fn new(memory: M, state: State) -> Self {
        let generation = memory.generation();
        Self {
            state,
            memory,
            irq: false,
            fiq: false,
            clock: Clock::Manual(0),
            hooks: Hooks::default(),
            taken: None,
            exceptions: 0,
            instructions: 0,
            translations: 0,
            unknown: std::collections::BTreeSet::new(),
            blocks: Cache::new(),
            singles: Cache::new(),
            generation,
            side: Side::new(),
        }
    }

    /// Keep the loads and stores of the CPU from now on, or stop keeping
    /// them. `take_accesses` empties the list.
    pub fn log_accesses(&mut self, on: bool) {
        self.side.log = on.then(Vec::new);
    }

    /// The loads and stores since the last call, in their order.
    pub fn take_accesses(&mut self) -> Vec<Access> {
        self.side
            .log
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// End the exclusive reservation of the CPU, as `CLREX` and an exception
    /// return do.
    pub fn clear_exclusive(&mut self) {
        self.side.exclusive = None;
    }

    /// What the system registers read that is not in the state.
    pub fn environment(&self) -> Environment {
        Environment {
            count: self.clock.count(),
            irq: self.irq,
            fiq: self.fiq,
        }
    }

    /// The output lines of the generic timer now.
    pub fn timer_lines(&self) -> timer::Lines {
        timer::lines(&self.state.system.timers, self.clock.count())
    }

    /// The count of the system counter at which a timer line rises next.
    pub fn timer_deadline(&self) -> Option<u64> {
        timer::deadline(&self.state.system.timers, self.clock.count())
    }

    fn take(&mut self, cause: Cause, pc: u64) {
        exception::enter(&mut self.state, cause, pc);
        self.exceptions += 1;
        self.taken = Some(Taken {
            cause,
            from: pc,
            to: self.state.pc(),
        });
    }

    fn raise(&mut self, exception: Exception, pc: u64) {
        self.take(Cause::Exception(exception), pc);
    }

    fn note(&mut self, unknown: Unknown) {
        if self.unknown.len() < UNKNOWN_ENTRIES {
            self.unknown.insert(unknown);
        }
    }

    /// The machine does not serve the hypercall that stopped the loop at
    /// `pc`: the guest takes the Undefined Instruction exception there, as
    /// on a CPU without EL2 and EL3.
    pub fn refuse_hypercall(&mut self, pc: u64) {
        self.raise(Exception::Undefined, pc);
    }

    /// `MRS` or `MSR` at `pc`. `Err`: the exception that it takes.
    fn register(&mut self, encoding: u16, rt: u8, write: bool) -> Result<(), Exception> {
        let value = match rt {
            31 => 0,
            n => self.state.x(usize::from(n)),
        };
        let Some(row) = sysreg::find(encoding) else {
            if let Some(hook) = self.hooks.register
                && let Some(read) = hook(&mut self.state, encoding, write.then_some(value))
            {
                if !write && rt != 31 {
                    self.state.set_x(usize::from(rt), read);
                }
                return Ok(());
            }
            self.note(Unknown::Register { encoding, write });
            return Err(Exception::Undefined);
        };
        match sysreg::check(row, write, &self.state) {
            Ok(()) => {}
            Err(Denied::Undefined) => {
                if matches!(sysreg::ROWS[row].kind, sysreg::Kind::Absent(_)) {
                    self.note(Unknown::Register { encoding, write });
                }
                return Err(Exception::Undefined);
            }
            Err(Denied::Trapped) => {
                let iss = sysreg::trap_iss(encoding, rt.into(), write);
                return Err(Exception::SystemTrap(iss));
            }
            Err(Denied::Fp) => return Err(Exception::FpAccess),
        }
        let environment = self.environment();
        if write {
            sysreg::write(row, &mut self.state, &environment, value);
            self.state.refresh();
        } else if rt != 31 {
            let value = sysreg::read(row, &self.state, &environment);
            self.state.set_x(usize::from(rt), value);
        }
        Ok(())
    }

    /// `MSR` of a PSTATE field with an immediate.
    fn pstate(&mut self, field: PstateField, immediate: u8) -> Result<(), Exception> {
        let el0 = self.state.context.el == 0;
        match field {
            PstateField::SpSel if el0 => return Err(Exception::Undefined),
            PstateField::SpSel => self.state.context.sp_sel = immediate & 1 != 0,
            PstateField::DaifSet | PstateField::DaifClr => {
                if el0 && self.state.system.regs[reg::SCTLR_EL1] & sctlr::UMA == 0 {
                    // op0 0, op1 3, CRn 4, CRm the immediate, Rt 31, a write.
                    // QEMU reports op1 in the place of op2 and op2 in the
                    // place of op1 for this form (the lockstep of card C6
                    // found ESR_EL1 0x620793f6 for `MSR DAIFSet, #11`). We
                    // follow it (DESIGN.md P2, U1).
                    let op2 = if field == PstateField::DaifSet { 6 } else { 7 };
                    let iss = 3 << 17 | op2 << 14 | 4 << 10 | 31 << 5 | u32::from(immediate) << 1;
                    return Err(Exception::SystemTrap(iss));
                }
                if field == PstateField::DaifSet {
                    self.state.daif |= immediate;
                } else {
                    self.state.daif &= !immediate;
                }
            }
        }
        Ok(())
    }

    /// Run the last instruction of a block, at `pc`.
    fn trap(&mut self, pc: u64, trap: Trap) -> Result<(), Stop> {
        let el0 = self.state.context.el == 0;
        let next = pc.wrapping_add(4);
        let sctlr = self.state.system.regs[reg::SCTLR_EL1];
        let done = match trap {
            Trap::Exception(exception) => Err(exception),
            Trap::Unknown(word) => {
                if let Some(hook) = self.hooks.instruction
                    && hook(&mut self.state, word)
                {
                    Ok(())
                } else {
                    self.note(Unknown::Encoding(word));
                    Err(Exception::Undefined)
                }
            }
            // A call from EL0 to what is above EL1 does not exist.
            Trap::Hvc(_) | Trap::Smc(_) if el0 => Err(Exception::Undefined),
            Trap::Hvc(immediate) | Trap::Smc(immediate) => {
                self.state.set_pc(next);
                return Err(Stop::Hypercall {
                    conduit: if matches!(trap, Trap::Hvc(_)) {
                        Conduit::Hvc
                    } else {
                        Conduit::Smc
                    },
                    immediate,
                    pc,
                });
            }
            Trap::Eret if el0 => Err(Exception::Undefined),
            Trap::Eret => {
                exception::exception_return(&mut self.state);
                self.clear_exclusive();
                return Ok(());
            }
            Trap::ClearExclusive => {
                self.clear_exclusive();
                Ok(())
            }
            Trap::Wfi if el0 && sctlr & sctlr::NTWI == 0 => Err(Exception::WaitTrap(false)),
            Trap::Wfi => {
                self.state.set_pc(next);
                // A line that is high wakes the CPU, masked or not.
                return if self.irq || self.fiq {
                    Ok(())
                } else {
                    Err(Stop::Halt)
                };
            }
            Trap::Wfe if el0 && sctlr & sctlr::NTWE == 0 => Err(Exception::WaitTrap(true)),
            Trap::Wfe => Ok(()),
            Trap::Mrs { encoding, rt } => self.register(encoding, rt, false),
            Trap::Msr { encoding, rt } => self.register(encoding, rt, true),
            Trap::MsrPstate { field, immediate } => self.pstate(field, immediate),
        };
        match done {
            Ok(()) => self.state.set_pc(next),
            Err(exception) => self.raise(exception, pc),
        }
        Ok(())
    }

    /// Translate the block at the PC of the state, of at most `limit`
    /// instructions.
    fn translate(&mut self, limit: u32) -> Result<Compiled, Refused> {
        let pc = self.state.pc();
        let context = self.state.context;
        if pc & 3 != 0 {
            return Err(Refused::Exception(Exception::PcAlignment));
        }
        let mut builder = Builder::new(context, pc);
        let mut at = pc;
        let mut count = 0;
        loop {
            let mut bytes = [0u8; 4];
            match self.memory.read(at, &mut bytes) {
                Ok(()) => {}
                Err(Error::Memory(_)) if count == 0 => {
                    // Translation is card C8: the PC is a physical address.
                    let status = if pc >> self.state.physical_address_bits() != 0 {
                        exception::ADDRESS_SIZE
                    } else {
                        exception::EXTERNAL
                    };
                    return Err(Refused::Exception(Exception::InstructionAbort(status)));
                }
                // The block ends before the instruction. It is looked at
                // again as the first of the next block.
                Err(Error::Memory(_)) => break,
                Err(error) => return Err(Refused::Stop(Stop::Host(error.to_string()))),
            }
            let word = u32::from_le_bytes(bytes);
            builder.begin(at);
            let lifted = match decode(word) {
                Some(decoded) => lift(&mut builder, &decoded),
                // `SMC` needs EL3 in the LLVM tables, so no selected record
                // matches it. The machine serves it as it serves `HVC`.
                None if word & 0xffe0_001f == 0xd400_0003 => {
                    builder.trap(Trap::Smc((word >> 5) as u16));
                    Lifted::Branch
                }
                None => {
                    builder.trap(Trap::Unknown(word));
                    Lifted::Branch
                }
            };
            match lifted {
                Lifted::Next => {
                    count += 1;
                    at = at.wrapping_add(4);
                }
                Lifted::Branch => {
                    count += 1;
                    at = at.wrapping_add(4);
                    break;
                }
                Lifted::Unsupported if count == 0 => {
                    let record = decode(word).map(|decoded| decoded.record);
                    return Err(Refused::Stop(match record {
                        Some(record) => Stop::Unimplemented { pc, record },
                        None => host("a word without a record has no lifter"),
                    }));
                }
                // The block ends before the instruction.
                Lifted::Unsupported => {
                    builder.unbegin();
                    break;
                }
            }
            // A block stays inside one page, so that the page of its code
            // names it for a later invalidation.
            if count >= limit || at & 0xfff == 0 {
                break;
            }
        }
        self.translations += 1;
        self.side.mark_code(pc);
        compile(pc, context, count, builder.finish(at)).map_err(Refused::Stop)
    }

    fn drop_blocks(&mut self) {
        self.blocks.clear();
        self.singles.clear();
        self.side.code_pages.fill(0);
    }

    /// Drop every block when a write has reached the memory since the
    /// caches were filled. Card C8 replaces this with the code stamps of the
    /// pages.
    fn check_generation(&mut self) {
        let generation = self.memory.generation();
        if generation != self.generation {
            self.generation = generation;
            self.drop_blocks();
        }
    }

    /// Account for the stores of this CPU since the last check. They moved
    /// the write generation of the memory. The blocks stay when no store
    /// went into a page of translated code and the generation moved by our
    /// stores alone. A memory whose generation does not count writes one by
    /// one drops the blocks here, which is slow and correct.
    ///
    /// A write of another CPU to our code is seen here and at the start of
    /// `run` and `step`, not between two blocks of one `run`. Card C8 closes
    /// that with the code stamps.
    fn settle(&mut self) {
        if self.side.own_writes == 0 {
            return;
        }
        let expected = self.generation.wrapping_add(self.side.own_writes);
        let generation = self.memory.generation();
        if self.side.code_written || generation != expected {
            self.drop_blocks();
        }
        self.generation = generation;
        self.side.own_writes = 0;
        self.side.code_written = false;
    }

    /// Take what does not wait for an instruction: an interrupt whose line
    /// is high and not masked, then the illegal state. `true`: the CPU is at
    /// a vector now.
    fn asynchronous(&mut self) -> bool {
        let pc = self.state.pc();
        if let Some(interrupt) = exception::pending(&self.state, self.irq, self.fiq) {
            self.take(Cause::Interrupt(interrupt), pc);
            return true;
        }
        if self.state.il {
            self.raise(Exception::IllegalState, pc);
            return true;
        }
        false
    }

    /// Run the block at the PC: one instruction when `single`, else a whole
    /// block, with the last instruction that the loop runs itself. An
    /// interrupt, the illegal state and a fetch that fails come before the
    /// block. `Err`: no block could be made.
    fn run_block(&mut self, single: bool) -> Result<(u32, Option<Halt>), Stop> {
        if (self.irq || self.fiq || self.state.il) && self.asynchronous() {
            return Ok((0, Some(Halt::Taken)));
        }
        let pc = self.state.pc();
        let context = self.state.context;
        let index = Cache::index(pc);
        let hit = {
            let cache = if single { &self.singles } else { &self.blocks };
            cache.entries[index]
                .as_ref()
                .is_some_and(|block| block.pc == pc && block.context == context)
        };
        if !hit {
            let limit = if single { 1 } else { BLOCK_INSTRUCTIONS };
            let compiled = match self.translate(limit) {
                Ok(compiled) => Box::new(compiled),
                Err(Refused::Stop(stop)) => return Err(stop),
                Err(Refused::Exception(exception)) => {
                    self.raise(exception, pc);
                    return Ok((0, Some(Halt::Taken)));
                }
            };
            let cache = if single {
                &mut self.singles
            } else {
                &mut self.blocks
            };
            cache.entries[index] = Some(compiled);
        }
        let cache = if single {
            &mut self.singles
        } else {
            &mut self.blocks
        };
        let Some(block) = cache.entries[index].as_mut() else {
            return Err(host("the block cache lost a block"));
        };
        let (count, halt) = block.run(&mut self.state, &mut self.memory, &mut self.side);
        let terminal = block.terminal;
        Ok((count, self.finish(halt, terminal)))
    }

    /// Run the last instruction of a block that ran to its end.
    fn finish(&mut self, halt: Option<Halt>, terminal: Option<(u64, Trap)>) -> Option<Halt> {
        match (halt, terminal) {
            (None, Some((pc, trap))) => self.trap(pc, trap).err().map(Halt::Stop),
            (halt, _) => halt,
        }
    }

    /// Run exactly one instruction, or take the interrupt or the exception
    /// that comes before it. `taken` says which.
    pub fn step(&mut self) -> Result<(), Stop> {
        self.taken = None;
        self.check_generation();
        let result = self.run_block(true);
        self.settle();
        let (count, halt) = result?;
        self.instructions += u64::from(count);
        match halt {
            None | Some(Halt::Taken) => Ok(()),
            Some(Halt::Stop(stop)) => Err(stop),
            Some(Halt::Retry) => Err(host("a block of one instruction asked to run alone")),
        }
    }

    /// Run up to `budget` instructions and say why the loop stopped.
    pub fn run(&mut self, budget: u64) -> Stop {
        self.check_generation();
        let stop = self.run_inner(budget);
        self.settle();
        stop
    }

    fn run_inner(&mut self, budget: u64) -> Stop {
        let mut left = budget;
        while left > 0 {
            let pc = self.state.pc();
            let context = self.state.context;
            let quiet = !(self.irq || self.fiq || self.state.il);
            // The common case: the block is translated and fits the budget.
            let outcome = if quiet
                && let Some(block) = self.blocks.entries[Cache::index(pc)].as_mut()
                && block.pc == pc
                && block.context == context
                && u64::from(block.instructions) <= left
            {
                let (count, halt) = block.run(&mut self.state, &mut self.memory, &mut self.side);
                let terminal = block.terminal;
                (count, self.finish(halt, terminal))
            } else {
                // A whole block where the budget allows one, else one
                // instruction. A block that is not translated yet can be as
                // long as `BLOCK_INSTRUCTIONS`.
                let cached = self.blocks.entries[Cache::index(pc)]
                    .as_ref()
                    .filter(|block| block.pc == pc && block.context == context)
                    .map(|block| u64::from(block.instructions));
                let single = match cached {
                    Some(length) => length > left,
                    None => left < u64::from(BLOCK_INSTRUCTIONS),
                };
                match self.run_block(single) {
                    Ok(outcome) => outcome,
                    Err(stop) => return stop,
                }
            };
            let (count, halt) = outcome;
            self.instructions += u64::from(count);
            left = left.saturating_sub(u64::from(count));
            match halt {
                None => {}
                Some(Halt::Taken) => left = left.saturating_sub(1),
                Some(Halt::Stop(stop)) => return stop,
                Some(Halt::Retry) => {
                    // The store goes into a page of translated code. The
                    // registers are those before its instruction: run it
                    // alone, and then drop the blocks.
                    if left == 0 {
                        break;
                    }
                    match self.run_block(true) {
                        Ok((count, halt)) => {
                            self.instructions += u64::from(count);
                            left = left.saturating_sub(u64::from(count));
                            match halt {
                                Some(Halt::Stop(stop)) => return stop,
                                Some(Halt::Taken) => left = left.saturating_sub(1),
                                _ => {}
                            }
                        }
                        Err(stop) => return stop,
                    }
                }
            }
            if self.side.code_written {
                self.settle();
            }
        }
        Stop::Budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmlab_hv_tcg::FlatMemory;

    const BASE: u64 = 0x4000_0000;

    fn cpu(words: &[u32]) -> Cpu<FlatMemory> {
        let mut data = vec![0u8; 0x2000];
        for (index, word) in words.iter().enumerate() {
            data[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        Cpu::new(FlatMemory::new(BASE, data), State::reset(BASE))
    }

    // add x0, x0, #1 ; subs x1, x1, #1 ; b.ne -8
    const LOOP: [u32; 3] = [0x9100_0400, 0xf100_0421, 0x54ff_ffc1];

    #[test]
    fn a_counted_loop_runs_to_its_end_with_the_flags_of_the_last_compare() {
        let mut cpu = cpu(&LOOP);
        cpu.state.set_x(1, 1000);
        assert_eq!(cpu.run(3000), Stop::Budget);
        assert_eq!(cpu.taken, None);
        assert_eq!(cpu.state.x(0), 1000);
        assert_eq!(cpu.state.x(1), 0);
        assert_eq!(cpu.state.pc(), BASE + 12);
        // 1 - 1: zero, and no borrow, so Z and C.
        assert_eq!(cpu.state.nzcv(), 0b0110);
        assert_eq!(cpu.instructions, 3000);
        // One block of three instructions, translated once.
        assert_eq!(cpu.translations, 1);
    }

    #[test]
    fn a_budget_stops_between_instructions_and_stepping_agrees_with_running() {
        let mut run = cpu(&LOOP);
        run.state.set_x(1, 50);
        let mut step = cpu(&LOOP);
        step.state.set_x(1, 50);
        for budget in [1u64, 2, 3, 4, 7, 64, 1] {
            assert_eq!(run.run(budget), Stop::Budget);
            for _ in 0..budget {
                step.step().unwrap();
            }
            assert_eq!(run.state, step.state, "after a budget of {budget}");
            assert_eq!(run.instructions, step.instructions);
        }
    }

    /// The vector table of the tests: `VBAR_EL1` points at the second page.
    const VECTORS: u64 = BASE + 0x1000;

    fn with_vectors(words: &[u32]) -> Cpu<FlatMemory> {
        let mut cpu = cpu(words);
        cpu.state.system.regs[reg::VBAR_EL1] = VECTORS;
        cpu
    }

    fn sync(exception: Exception, from: u64, to: u64) -> Option<Taken> {
        Some(Taken {
            cause: Cause::Exception(exception),
            from,
            to,
        })
    }

    #[test]
    fn what_cannot_run_is_an_exception_of_the_guest() {
        // Outside the memory: an instruction abort, with the PC in FAR_EL1.
        let mut cpu = with_vectors(&[]);
        cpu.state.set_pc(0x1000);
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(
                Exception::InstructionAbort(exception::EXTERNAL),
                0x1000,
                VECTORS + 0x200
            )
        );
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x8600_0010);
        assert_eq!(cpu.state.system.regs[reg::FAR_EL1], 0x1000);
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], 0x1000);
        // A PC above the 40 bits of a physical address: an address size fault.
        cpu.state.set_pc(1 << 40);
        cpu.step().unwrap();
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x8600_0000);
        assert_eq!(cpu.state.system.regs[reg::FAR_EL1], 1 << 40);
        // Not a multiple of four.
        cpu.state.set_pc(BASE + 2);
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(Exception::PcAlignment, BASE + 2, VECTORS + 0x200)
        );
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x8a00_0000);
        assert_eq!(cpu.state.system.regs[reg::FAR_EL1], BASE + 2);
        // An encoding that no record of the selected sets matches: an add
        // with the immediate shift 2, which tables/constraints.toml refuses.
        // The block ends in it, and the log of the CPU names it once.
        let mut cpu = with_vectors(&[0x9100_0400, 0x9180_0400]);
        assert_eq!(cpu.run(2), Stop::Budget);
        assert_eq!(
            cpu.taken,
            sync(Exception::Undefined, BASE + 4, VECTORS + 0x200)
        );
        assert_eq!(cpu.state.x(0), 1);
        assert_eq!(cpu.instructions, 2);
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x0200_0000);
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], BASE + 4);
        assert_eq!(cpu.state.system.regs[reg::SPSR_EL1], 0x3c5);
        assert_eq!(
            cpu.unknown.iter().copied().collect::<Vec<_>>(),
            vec![Unknown::Encoding(0x9180_0400)]
        );
        assert_eq!(
            Unknown::Encoding(0x9180_0400).to_string(),
            "undefined instruction 91800400"
        );
        // UDF is undefined and is no unknown encoding.
        let mut cpu = with_vectors(&[0]);
        cpu.step().unwrap();
        assert_eq!(cpu.taken, sync(Exception::Undefined, BASE, VECTORS + 0x200));
        assert!(cpu.unknown.is_empty());
        // A guest whose vector is not in the memory takes one abort after the
        // other, and the budget still ends the run.
        let mut cpu = self::cpu(&[0]);
        assert_eq!(cpu.run(10), Stop::Budget);
        assert_eq!(cpu.exceptions, 10);
        assert_eq!(cpu.state.pc(), 0x200);
    }

    // The words of the system tests.
    const SVC_7: u32 = 0xd400_00e1;
    const HVC_0: u32 = 0xd400_0002;
    const SMC_5: u32 = 0xd400_00a3;
    const ERET: u32 = 0xd69f_03e0;
    const WFI: u32 = 0xd503_207f;
    const WFE: u32 = 0xd503_205f;
    const NOP: u32 = 0xd503_201f;

    fn mrs(rt: u32, name: &str) -> u32 {
        0xd520_0000 | u32::from(sysreg::ROWS[sysreg::index(name)].encoding) << 5 | rt
    }

    fn msr(name: &str, rt: u32) -> u32 {
        0xd500_0000 | u32::from(sysreg::ROWS[sysreg::index(name)].encoding) << 5 | rt
    }

    /// Put `words` at the vector that EL0 takes its synchronous exceptions
    /// to, and at the one of EL1 with SP_EL1.
    fn handler(cpu: &mut Cpu<FlatMemory>, words: &[u32]) {
        for offset in [0x200u64, 0x400] {
            for (index, word) in words.iter().enumerate() {
                cpu.memory
                    .write(VECTORS + offset + 4 * index as u64, &word.to_le_bytes())
                    .unwrap();
            }
        }
    }

    #[test]
    fn a_system_call_from_el0_and_its_return() {
        // svc #7 ; add x0, x0, #1. The handler: add x1, x1, #1 ; eret.
        let mut cpu = with_vectors(&[SVC_7, 0x9100_0400]);
        handler(&mut cpu, &[0x9100_0421, ERET]);
        assert!(cpu.state.set_cpsr(0x2000_0000));
        cpu.state.slots[slot::SP_EL0] = 0x1000;
        cpu.state.slots[slot::SP_EL1] = 0x2000;
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(Exception::SupervisorCall(7), BASE, VECTORS + 0x400)
        );
        assert_eq!(cpu.state.cpsr(), 0x3c5);
        assert_eq!(cpu.state.sp(), 0x2000);
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x5600_0007);
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], BASE + 4);
        assert_eq!(cpu.state.system.regs[reg::SPSR_EL1], 0x2000_0000);
        assert_eq!(cpu.run(3), Stop::Budget);
        assert_eq!((cpu.state.x(0), cpu.state.x(1)), (1, 1));
        assert_eq!(cpu.state.cpsr(), 0x2000_0000);
        assert_eq!(cpu.state.sp(), 0x1000);
        assert_eq!(cpu.state.pc(), BASE + 8);
        // ERET at EL0 is undefined.
        let mut cpu = with_vectors(&[ERET]);
        assert!(cpu.state.set_cpsr(0));
        cpu.step().unwrap();
        assert_eq!(cpu.taken, sync(Exception::Undefined, BASE, VECTORS + 0x400));
        // A return to a state that does not exist leaves the level, and the
        // next instruction takes the Illegal Execution State exception.
        let mut cpu = with_vectors(&[ERET, NOP]);
        cpu.state.system.regs[reg::SPSR_EL1] = 0x3c9;
        cpu.state.system.regs[reg::ELR_EL1] = BASE + 4;
        cpu.step().unwrap();
        assert_eq!(cpu.taken, None);
        assert_eq!(cpu.state.cpsr(), 0x0010_03c5);
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(Exception::IllegalState, BASE + 4, VECTORS + 0x200)
        );
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x3a00_0000);
        assert_eq!(cpu.state.system.regs[reg::SPSR_EL1], 0x0010_03c5);
        assert_eq!(cpu.state.cpsr(), 0x3c5);
    }

    #[test]
    fn hypercalls_leave_the_core_with_their_conduit() {
        let mut cpu = with_vectors(&[HVC_0, SMC_5, NOP]);
        assert_eq!(
            cpu.run(10),
            Stop::Hypercall {
                conduit: Conduit::Hvc,
                immediate: 0,
                pc: BASE
            }
        );
        assert_eq!(cpu.state.pc(), BASE + 4);
        assert_eq!(cpu.taken, None);
        assert_eq!(
            cpu.step(),
            Err(Stop::Hypercall {
                conduit: Conduit::Smc,
                immediate: 5,
                pc: BASE + 4
            })
        );
        assert_eq!(cpu.state.pc(), BASE + 8);
        // A machine that does not serve the call refuses it.
        cpu.refuse_hypercall(BASE + 4);
        assert_eq!(
            cpu.taken,
            sync(Exception::Undefined, BASE + 4, VECTORS + 0x200)
        );
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], BASE + 4);
        // From EL0 neither call exists.
        for word in [HVC_0, SMC_5] {
            let mut cpu = with_vectors(&[word]);
            assert!(cpu.state.set_cpsr(0));
            cpu.step().unwrap();
            assert_eq!(cpu.taken, sync(Exception::Undefined, BASE, VECTORS + 0x400));
        }
        // SMC is no unknown encoding.
        assert!(cpu.unknown.is_empty());
    }

    #[test]
    fn an_interrupt_line_is_taken_between_blocks_when_its_mask_is_clear() {
        let mut cpu = with_vectors(&LOOP);
        cpu.state.set_x(1, 1000);
        cpu.irq = true;
        // Masked at the reset: the loop runs.
        assert_eq!(cpu.run(30), Stop::Budget);
        assert_eq!(cpu.taken, None);
        // msr daifclr, #2 in the loop would unmask. Here the owner does.
        cpu.state.daif = 0b1101;
        let before = cpu.state.pc();
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            Some(Taken {
                cause: Cause::Interrupt(exception::Interrupt::Irq),
                from: before,
                to: VECTORS + 0x280
            })
        );
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], before);
        assert_eq!(cpu.state.daif, 0xf);
        // FIQ has its own mask and its own vector, from EL0 too.
        let mut cpu = with_vectors(&LOOP);
        assert!(cpu.state.set_cpsr(0x80));
        cpu.fiq = true;
        cpu.irq = true;
        assert_eq!(cpu.run(1), Stop::Budget);
        assert_eq!(cpu.state.pc(), VECTORS + 0x500);
        assert_eq!(cpu.state.system.regs[reg::SPSR_EL1], 0x80);
        assert_eq!(cpu.instructions, 0);
    }

    #[test]
    fn wfi_halts_until_a_line_is_high_and_traps_from_el0() {
        let mut cpu = with_vectors(&[WFI, NOP, WFI, WFE, NOP]);
        assert_eq!(cpu.run(10), Stop::Halt);
        assert_eq!(cpu.state.pc(), BASE + 4);
        // A masked line still wakes the CPU: the second WFI runs through.
        cpu.irq = true;
        assert_eq!(cpu.run(4), Stop::Budget);
        assert_eq!(cpu.state.pc(), BASE + 20);
        assert_eq!(cpu.taken, None);
        // At EL0 with SCTLR_EL1.nTWI clear, WFI is trapped. WFE likewise.
        for (word, wfe, bit) in [(WFI, false, sctlr::NTWI), (WFE, true, sctlr::NTWE)] {
            let mut cpu = with_vectors(&[word, NOP]);
            assert!(cpu.state.set_cpsr(0x3c0));
            cpu.irq = true;
            cpu.state.system.regs[reg::SCTLR_EL1] &= !bit;
            cpu.step().unwrap();
            assert_eq!(
                cpu.taken,
                sync(Exception::WaitTrap(wfe), BASE, VECTORS + 0x400)
            );
            assert_eq!(
                cpu.state.system.regs[reg::ESR_EL1],
                0x07e0_0000 | u64::from(wfe)
            );
            // With the bit set, as it is after the reset, it runs.
            let mut cpu = with_vectors(&[word, NOP]);
            assert!(cpu.state.set_cpsr(0x3c0));
            cpu.irq = true;
            assert_ne!(cpu.state.system.regs[reg::SCTLR_EL1] & bit, 0);
            cpu.step().unwrap();
            assert_eq!(cpu.taken, None);
            assert_eq!(cpu.state.pc(), BASE + 4);
        }
    }

    #[test]
    fn the_timer_raises_its_line_and_the_interrupt_wakes_wfi() {
        // msr cntv_cval_el0, x0 ; msr cntv_ctl_el0, x1 ; mrs x2, cntv_ctl_el0 ;
        // mrs x3, cntvct_el0 ; wfi ; nop
        let mut cpu = with_vectors(&[
            msr("CNTV_CVAL_EL0", 0),
            msr("CNTV_CTL_EL0", 1),
            mrs(2, "CNTV_CTL_EL0"),
            mrs(3, "CNTVCT_EL0"),
            WFI,
            NOP,
        ]);
        cpu.clock = Clock::Manual(100);
        cpu.state.set_x(0, 500);
        cpu.state.set_x(1, 1);
        assert_eq!(cpu.run(10), Stop::Halt);
        assert_eq!((cpu.state.x(2), cpu.state.x(3)), (1, 100));
        assert_eq!(cpu.timer_lines(), timer::Lines::default());
        assert_eq!(cpu.timer_deadline(), Some(500));
        // The machine waits until the deadline and wires the line to IRQ.
        cpu.clock = Clock::Manual(500);
        assert!(cpu.timer_lines().virtual_);
        assert_eq!(cpu.timer_deadline(), None);
        cpu.irq = cpu.timer_lines().virtual_;
        cpu.state.daif = 0;
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken.map(|taken| (taken.from, taken.to)),
            Some((BASE + 20, VECTORS + 0x280))
        );
        // The handler masks the timer, and the line falls.
        cpu.state.system.timers[1].set_control(3);
        assert!(!cpu.timer_lines().virtual_);
    }

    #[test]
    fn an_fp_instruction_traps_while_cpacr_disables_fp() {
        // fadd d0, d1, d2 ; msr cpacr_el1, x0 ; fadd d0, d1, d2
        let mut cpu = with_vectors(&[0x1e62_2820, msr("CPACR_EL1", 0), 0x1e62_2820]);
        handler(
            &mut cpu,
            &[0x9100_0421, 0xd538_4031, 0x9100_1231, 0xd518_4031, ERET],
        );
        cpu.state.set_x(0, 3 << 20);
        cpu.step().unwrap();
        assert_eq!(cpu.taken, sync(Exception::FpAccess, BASE, VECTORS + 0x200));
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x1fe0_0000);
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], BASE);
        // The handler skips the instruction (mrs x17, elr_el1 ; add x17,
        // x17, #4 ; msr elr_el1, x17), the guest enables FP, and the block
        // that follows is lifted for the new context.
        cpu.state.set_v(1, u128::from(1.5f64.to_bits()));
        cpu.state.set_v(2, u128::from(2.25f64.to_bits()));
        assert_eq!(cpu.run(7), Stop::Budget);
        assert!(cpu.state.context.fp);
        assert_eq!(cpu.state.v(0), u128::from(3.75f64.to_bits()));
        assert_eq!(cpu.exceptions, 1);
    }

    #[test]
    fn a_read_of_a_stored_register_stays_in_its_block() {
        // mrs x0, tpidr_el1 ; add x0, x0, #1 ; mrs x1, tpidr_el0 ;
        // mrs x2, midr_el1 ; mrs xzr, tpidr_el1 ; mrs x3, cntvct_el0 ; b .
        let words = [
            mrs(0, "TPIDR_EL1"),
            0x9100_0400,
            mrs(1, "TPIDR_EL0"),
            mrs(2, "MIDR_EL1"),
            mrs(31, "TPIDR_EL1"),
            mrs(3, "CNTVCT_EL0"),
            0x1400_0000,
        ];
        let mut cpu = with_vectors(&words);
        cpu.state.system.regs[sysreg::index("TPIDR_EL1")] = 0x1000;
        cpu.state.system.regs[sysreg::index("TPIDR_EL0")] = 0x2000;
        cpu.clock = Clock::Manual(77);
        assert_eq!(cpu.run(64), Stop::Budget);
        // The counter is computed, so it ends the block. The five
        // instructions before it are one block with it, and the branch to
        // itself is the second block.
        assert_eq!(cpu.translations, 2);
        assert_eq!(
            (
                cpu.state.x(0),
                cpu.state.x(1),
                cpu.state.x(2),
                cpu.state.x(3)
            ),
            (0x1001, 0x2000, 0x000f_3d00, 77)
        );
        // Stepping gives the same, and a second run of the cached block
        // reads the words as they are then.
        let mut step = with_vectors(&words);
        step.state.system.regs[sysreg::index("TPIDR_EL1")] = 0x1000;
        step.state.system.regs[sysreg::index("TPIDR_EL0")] = 0x2000;
        step.clock = Clock::Manual(77);
        for _ in 0..7 {
            step.step().unwrap();
        }
        assert_eq!(step.state.slots[..31], cpu.state.slots[..31]);
        cpu.state.system.regs[sysreg::index("TPIDR_EL1")] = 0x5000;
        cpu.state.set_pc(BASE);
        assert_eq!(cpu.run(7), Stop::Budget);
        assert_eq!(cpu.state.x(0), 0x5001);
        assert_eq!(cpu.translations, 2);
        // At EL0 only what EL0 may read without a control bit stays inside:
        // TPIDR_EL1 is undefined there, and CTR_EL0 asks SCTLR_EL1.
        assert!(sysreg::reads_stored(sysreg::index("TPIDR_EL0"), 0));
        assert!(!sysreg::reads_stored(sysreg::index("TPIDR_EL1"), 0));
        assert!(!sysreg::reads_stored(sysreg::index("CTR_EL0"), 0));
        assert!(sysreg::reads_stored(sysreg::index("CTR_EL0"), 1));
        assert!(!sysreg::reads_stored(sysreg::index("OSLAR_EL1"), 1));
        let mut user = with_vectors(&[mrs(0, "TPIDR_EL1")]);
        assert!(user.state.set_cpsr(0x3c0));
        user.step().unwrap();
        assert_eq!(
            user.taken,
            sync(Exception::Undefined, BASE, VECTORS + 0x400)
        );
    }

    #[test]
    fn a_machine_adds_registers_and_instructions_through_the_hooks() {
        // S3_0_C15_C0_0: IMPLEMENTATION DEFINED, no row.
        const ENCODING: u16 = sysreg::encoding(3, 0, 15, 0, 0);
        fn register(state: &mut State, encoding: u16, write: Option<u64>) -> Option<u64> {
            if encoding != ENCODING {
                return None;
            }
            if let Some(value) = write {
                state.set_x(9, value + 1);
            }
            Some(0x1234)
        }
        fn instruction(state: &mut State, word: u32) -> bool {
            state.set_x(10, word.into());
            word == 0x0020_1234
        }
        let read = 0xd520_0000 | u32::from(ENCODING) << 5 | 3;
        let write = 0xd500_0000 | u32::from(ENCODING) << 5 | 4;
        let other = read + (1 << 5);
        let words = [read, write, 0x0020_1234, other, 0x0020_9999];
        let mut cpu = with_vectors(&words);
        cpu.hooks.register = Some(register);
        cpu.hooks.instruction = Some(instruction);
        cpu.state.set_x(4, 41);
        assert_eq!(cpu.run(3), Stop::Budget);
        assert_eq!(cpu.taken, None);
        assert_eq!((cpu.state.x(3), cpu.state.x(9)), (0x1234, 42));
        assert_eq!(cpu.state.x(10), 0x0020_1234);
        assert_eq!(cpu.state.pc(), BASE + 12);
        // What the machine does not claim is undefined and is in the log.
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(Exception::Undefined, BASE + 12, VECTORS + 0x200)
        );
        cpu.state.set_pc(BASE + 16);
        cpu.step().unwrap();
        assert_eq!(
            cpu.unknown
                .iter()
                .map(Unknown::to_string)
                .collect::<Vec<_>>(),
            vec![
                "undefined instruction 00209999".to_string(),
                "read of the system register S3_0_C15_C0_1, which this CPU does not have"
                    .to_string(),
            ]
        );
        // Without hooks the first word is undefined too.
        let mut cpu = with_vectors(&words);
        cpu.step().unwrap();
        assert_eq!(cpu.taken, sync(Exception::Undefined, BASE, VECTORS + 0x200));
    }

    #[test]
    fn a_write_to_the_memory_drops_the_translated_blocks() {
        let mut cpu = cpu(&[0x9100_0400, 0x1400_0000]);
        assert_eq!(cpu.run(1), Stop::Budget);
        assert_eq!(cpu.state.x(0), 1);
        // add x0, x0, #2 in place of add x0, x0, #1.
        cpu.memory
            .write(BASE, &0x9100_0800u32.to_le_bytes())
            .unwrap();
        cpu.state.set_pc(BASE);
        assert_eq!(cpu.run(1), Stop::Budget);
        assert_eq!(cpu.state.x(0), 3);
    }

    #[test]
    fn scalar_fp_runs_through_the_helpers_and_collects_its_exceptions() {
        use crate::helpers::fp::fpsr;
        // fadd d0, d1, d2 ; fdiv d3, d1, d4 ; fcvtzs w5, d0 ; fcmp d1, d2
        let mut cpu = cpu(&[0x1e62_2820, 0x1e64_1823, 0x1e78_0005, 0x1e62_2020]);
        cpu.state.set_v(0, u128::MAX);
        cpu.state
            .set_v(1, u128::from(1.5f64.to_bits()) | 0xdead << 64);
        cpu.state.set_v(2, u128::from(2.25f64.to_bits()));
        cpu.state.set_v(3, u128::MAX);
        cpu.state.set_x(5, u64::MAX);
        cpu.state.enable_fp();
        assert_eq!(cpu.run(4), Stop::Budget);
        // A scalar result clears the register above it.
        assert_eq!(cpu.state.v(0), u128::from(3.75f64.to_bits()));
        assert_eq!(cpu.state.v(3), u128::from(f64::INFINITY.to_bits()));
        assert_eq!(cpu.state.x(5), 3);
        // 1.5 < 2.25: N.
        assert_eq!(cpu.state.nzcv(), 0b1000);
        // A division by zero, then an inexact conversion. The bits stay.
        assert_eq!(cpu.state.fpsr(), u32::from(fpsr::DZC | fpsr::IXC));
        // Stepping gives the same.
        let mut step = self::cpu(&[0x1e62_2820, 0x1e64_1823, 0x1e78_0005, 0x1e62_2020]);
        step.state.set_v(0, u128::MAX);
        step.state
            .set_v(1, u128::from(1.5f64.to_bits()) | 0xdead << 64);
        step.state.set_v(2, u128::from(2.25f64.to_bits()));
        step.state.set_v(3, u128::MAX);
        step.state.set_x(5, u64::MAX);
        step.state.enable_fp();
        for _ in 0..4 {
            step.step().unwrap();
        }
        assert_eq!(step.state, cpu.state);
    }

    #[test]
    fn the_rounding_mode_of_fpcr_reaches_the_helpers() {
        // fdiv s0, s1, s2: 1 / 3 in each of the four modes.
        for (mode, expected) in [
            (0u32, 0x3eaa_aaabu32),
            (1, 0x3eaa_aaab),
            (2, 0x3eaa_aaaa),
            (3, 0x3eaa_aaaa),
        ] {
            let mut cpu = cpu(&[0x1e22_1820]);
            cpu.state.set_v(1, u128::from(1.0f32.to_bits()));
            cpu.state.set_v(2, u128::from(3.0f32.to_bits()));
            cpu.state.set_fpcr(mode << 22);
            cpu.state.enable_fp();
            cpu.step().unwrap();
            assert_eq!(cpu.state.v(0), u128::from(expected), "mode {mode}");
            assert_eq!(cpu.state.fpsr(), 0x10);
            assert_eq!(cpu.state.fpcr(), mode << 22);
        }
    }

    fn bump(context: &mut dyn std::any::Any, args: &[u128], results: &mut [u128]) {
        let state = context
            .downcast_mut::<State>()
            .expect("the context of an aarch64 helper is the state");
        state.daif ^= 0xf;
        results[0] = args[0] + args[1];
    }

    static BUMP: vmlab_hv_tcg_ir::native::Helper = vmlab_hv_tcg_ir::native::Helper {
        name: "bump",
        args: 2,
        results: 1,
        effects: vmlab_hv_tcg_ir::native::Effects::WRITES_STATE,
        function: HelperFn::Ordered(bump),
    };

    #[test]
    fn an_ordered_helper_runs_once_at_its_place_with_the_state() {
        use crate::state::REGISTER_FILE;
        use vmlab_hv_tcg_ir::native::regfile::Writeback;
        use vmlab_hv_tcg_ir::native::{Binary, HelperRef};
        let ty = ScalarType::Bits(64);
        let mut block: BlockOf<A64Exit> = BlockOf::default();
        let x0 = REGISTER_FILE.read(&mut block.graph, slot::X0);
        let x1 = REGISTER_FILE.read(&mut block.graph, slot::X0 + 1);
        // x2 = bump(x0 + x1, x1), and nothing reads the result of a second call.
        let sum = block.graph.binary(ty, Binary::Add, x0, x1).unwrap();
        let first = block.graph.effect(0, ty);
        let second = block.graph.effect(1, ty);
        for (args, result) in [(vec![sum, x1], first), (vec![x0, x0], second)] {
            block.events.push(Event::Call {
                helper: HelperRef(&BUMP),
                args,
                results: vec![result],
            });
        }
        let next = block.graph.constant(ty, 0x4000_0004);
        let exit = block.exit(A64Exit {
            writeback: Writeback {
                writes: vec![(slot::X0 + 2, first)],
            },
            next: A64Next::Pc(next),
            completed: 1,
        });
        block.events.push(Event::Exit(exit));
        let context = State::reset(0).context;
        let mut compiled = compile(0x4000_0000, context, 1, block).unwrap();
        let mut state = State::reset(0x4000_0000);
        state.set_x(0, 5);
        state.set_x(1, 7);
        let mut memory = FlatMemory::new(0x4000_0000, vec![0; 16]);
        let (count, halt) = compiled.run(&mut state, &mut memory, &mut Side::new());
        assert!(count == 1 && halt.is_none());
        assert_eq!(state.x(2), 19);
        assert_eq!(state.pc(), 0x4000_0004);
        // Both calls ran: the masks were flipped twice.
        assert_eq!(state.daif, 0xf);
        // An ordered call of a pure helper is refused.
        let mut block: BlockOf<A64Exit> = BlockOf::default();
        let x0 = REGISTER_FILE.read(&mut block.graph, slot::X0);
        let result = block.graph.effect(0, ty);
        block.events.push(Event::Call {
            helper: HelperRef(&crate::helpers::fp::FP_UNARY),
            args: vec![x0],
            results: vec![result],
        });
        let next = block.graph.constant(ty, 0);
        let exit = block.exit(A64Exit {
            writeback: Writeback::default(),
            next: A64Next::Pc(next),
            completed: 1,
        });
        block.events.push(Event::Exit(exit));
        assert!(matches!(compile(0, context, 1, block), Err(Stop::Host(_))));
    }

    #[test]
    fn a_block_is_one_page_at_most_and_sixty_four_instructions() {
        // Fill two pages with add x0, x0, #1.
        let words = vec![0x9100_0400u32; 0x800];
        let mut cpu = cpu(&words);
        assert_eq!(cpu.run(0x800), Stop::Budget);
        assert_eq!(cpu.state.x(0), 0x800);
        assert_eq!(cpu.translations, u64::from(0x800 / BLOCK_INSTRUCTIONS));
        assert_eq!(cpu.state.pc(), BASE + 0x2000);
        // The memory ends there: an instruction abort.
        assert_eq!(cpu.run(1), Stop::Budget);
        assert_eq!(
            cpu.taken.map(|taken| (taken.cause, taken.from)),
            Some((
                Cause::Exception(Exception::InstructionAbort(exception::EXTERNAL)),
                BASE + 0x2000
            ))
        );
    }
}
