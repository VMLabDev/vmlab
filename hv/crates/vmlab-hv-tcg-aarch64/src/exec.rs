//! The interpreter loop: translate a block at the PC, run it on the slot
//! executor of `vmlab-hv-tcg-ir`, write the registers back, repeat.
//!
//! Addresses are physical addresses on a `Memory`: no MMU, no exception
//! entry, no interrupts. An instruction that the loop cannot run stops it
//! with the reason, and the caller decides. The exception card (C6) turns
//! the stops into exception entries.
//!
//! A block is lowered once into its pre-decoded form (`block`, the gate of
//! card C5) and kept under its PC. That form is pure scalar code with memory
//! events between it: loads, stores and guards, in the order of the
//! instructions. Each event names an exit
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
//! an exception keeps it (the exception return of card C6 ends it).
//!
//! With more CPUs the store also asks that no other writer reached the
//! granules of the load (`Memory::store_conditional`), so it fails more
//! often than QEMU's: a write of another CPU fails it also when it left the
//! value as it was.
//!
//! Sources read: none. This file holds no instruction semantics.

use crate::generated::decode::{Record, decode};
use crate::lift::{A64Exit, A64Next, Builder, Lifted, lift};
use crate::state::{Context, State, slot};
use vmlab_hv_tcg::{Error, Memory};
use crate::block::{self, Code, FIRST_EFFECT, Generic, Lowering, Op, Place, Slot};
use vmlab_hv_tcg_ir::native::block::{BlockOf, Event};
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

/// Why the loop stopped. The PC of the state is the instruction that did
/// not run, and that instruction has changed no register. A store of
/// several parts (a pair, `ST2` to `ST4`) has written the parts before the
/// one that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The instruction budget of `run` is used up.
    Budget,
    /// No selected record matches the word: the Undefined Instruction
    /// exception.
    Undefined { pc: u64, word: u32 },
    /// The record is decoded and has no lifter yet.
    Unimplemented { pc: u64, record: Record },
    /// The memory holds nothing at the PC: an instruction abort.
    FetchAbort { pc: u64 },
    /// The PC is not a multiple of four: the PC alignment exception.
    PcAlignment { pc: u64 },
    /// The memory holds nothing at the address of a load or a store: a data
    /// abort.
    DataAbort { pc: u64, address: u64, write: bool },
    /// An access to an address that is not a multiple of its size: an
    /// alignment fault. On this flat memory every load and store asks for
    /// its alignment, as QEMU does with the MMU off.
    Alignment { pc: u64, address: u64 },
    /// An error of the host, not of the guest.
    Host(String),
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

/// A load or a store that has no code of its own in the block form: one of
/// 16 bytes, or an exclusive one.
#[derive(Clone, Copy, Debug)]
enum Access16 {
    Read {
        address: Slot,
        result: Place,
        bytes: u8,
        exclusive: bool,
        exit: u32,
    },
    Write {
        address: Slot,
        value: Place,
        bytes: u8,
        /// The word of the result of an exclusive store.
        exclusive: Option<Slot>,
        exit: u32,
    },
}

/// Where a block can leave before its end.
#[derive(Clone, Debug)]
struct Leave {
    /// Register slot, words of the frame: copied out when the block leaves
    /// here. The PC is one of them.
    outputs: Vec<(u32, Place)>,
    /// The instructions of the block that are complete here.
    completed: u32,
    /// The word of the address of an alignment fault.
    address: Option<Slot>,
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
    /// A store into a page that holds translated code, in a block of more
    /// than one instruction. Nothing of the instruction is lost: the loop
    /// runs it alone and then drops the translated blocks.
    Retry,
}

/// A block as the loop runs it: the pre-decoded form of `block`.
struct Compiled {
    pc: u64,
    context: Context,
    instructions: u32,
    /// The operations in the order in which they run: the pure ones, and
    /// between them the loads, the stores, the guards and the helper calls.
    ops: Vec<Op>,
    generics: Vec<Generic>,
    accesses: Vec<Access16>,
    leaves: Vec<Leave>,
    /// The words of the scalars. The constants are in place.
    frame: Vec<u64>,
    /// The helper calls (R10).
    calls: Vec<Call>,
    /// Words of the frame, register slot: copied in before the block runs.
    inputs: Vec<(Place, u32)>,
    /// Register slot, words of the frame: copied out after it.
    outputs: Vec<(u32, Place)>,
}

/// The most arguments of a helper.
const CALL_ARGUMENTS: usize = 8;

/// One helper call of a block. Its place among the operations is that of
/// its `Code::Call`.
struct Call {
    function: HelperFn,
    /// The words of the arguments.
    args: Vec<Place>,
    /// The words of each result, with the mask of its type.
    results: Vec<(Place, u128)>,
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

/// What `compile` gathers beside the operations of the block.
#[derive(Default)]
struct Parts {
    accesses: Vec<Access16>,
    leaves: Vec<Leave>,
    calls: Vec<Call>,
    inputs: Vec<(Place, u32)>,
}

impl Parts {
    /// Put a helper call after the operations that the block has so far.
    fn call(&mut self, lowering: &mut Lowering, call: Call) -> Result<(), Stop> {
        let index = narrow(self.calls.len())?;
        self.calls.push(call);
        lowering.effect(Code::Call, 0, 0, 0, 0, index.into());
        Ok(())
    }

    /// Append the pure scalars that `roots` need and the block has not
    /// computed yet: constants and inputs are bound, a pure helper call
    /// becomes a `Call`, and the rest is lowered.
    fn emit(
        &mut self,
        lowering: &mut Lowering,
        graph: &Graph,
        seen: &mut [bool],
        roots: &[ScalarId],
    ) -> Result<(), Stop> {
        for id in schedule::scalars(graph, roots, seen) {
            match &graph.scalars[id].operation {
                Operation::Constant(value) => lowering.constant(id, *value),
                Operation::Input(register) => {
                    self.inputs.push((lowering.place(id), narrow(*register)?));
                }
                // The event of the effect writes its words.
                Operation::Effect(_) => {}
                // A pure helper call runs at its place among the operations.
                Operation::Call { helper, args } => {
                    if args.len() > CALL_ARGUMENTS {
                        return Err(host("a helper with too many arguments"));
                    }
                    let call = Call {
                        function: helper.0.function,
                        args: args.iter().map(|arg| lowering.place(*arg)).collect(),
                        results: vec![(lowering.place(id), result_mask(graph, id))],
                    };
                    self.call(lowering, call)?;
                }
                _ => lowering.scalar(graph, id),
            }
        }
        Ok(())
    }
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
    let mut lowering = Lowering::new(graph).ok_or_else(|| host("a block with too many slots"))?;
    let mut seen = vec![false; graph.scalars.len()];
    let mut parts = Parts::default();
    let mut block_outputs = None;
    // The exits that events leave through, by their number in the block.
    let mut leave_of = vec![None; block.exits.len()];
    for event in &block.events {
        if block_outputs.is_some() {
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
                parts.emit(&mut lowering, graph, &mut seen, args)?;
                for result in results {
                    seen[*result] = true;
                }
                let call = Call {
                    function: helper.0.function,
                    args: args.iter().map(|arg| lowering.place(*arg)).collect(),
                    results: results
                        .iter()
                        .map(|result| (lowering.place(*result), result_mask(graph, *result)))
                        .collect(),
                };
                parts.call(&mut lowering, call)?;
                continue;
            }
            Event::Exit(exit) => (*exit, Vec::new()),
            Event::Checkpoint(_) => return Err(host("an aarch64 block has no checkpoint")),
        };
        let payload = &block.exits[exit];
        roots.extend(exit_roots(payload));
        parts.emit(&mut lowering, graph, &mut seen, &roots)?;
        let mut outputs = Vec::new();
        for (register, scalar) in &payload.writeback.writes {
            outputs.push((narrow(*register)?, lowering.place(*scalar)));
        }
        if let Event::Exit(_) = event {
            let A64Next::Pc(next) = payload.next else {
                return Err(host(
                    "a block that ends in an exception has no executor yet",
                ));
            };
            outputs.push((slot::PC as u32, lowering.place(next)));
            block_outputs = Some(outputs);
            continue;
        }
        let leave = match leave_of[exit] {
            Some(leave) => leave,
            None => {
                let A64Next::Exception { syndrome, .. } = &payload.next else {
                    return Err(host("an event that leaves a block without an exception"));
                };
                let leave = narrow(parts.leaves.len())?;
                parts.leaves.push(Leave {
                    outputs,
                    completed: payload.completed,
                    address: syndrome.first().map(|scalar| lowering.word(*scalar)),
                });
                leave_of[exit] = Some(leave);
                leave
            }
        };
        match event {
            Event::Guard {
                condition,
                expected,
                ..
            } => {
                let condition = lowering.word(*condition);
                lowering.effect(Code::Guard, u8::from(*expected), 0, condition, 0, leave.into());
            }
            Event::ReadMemory {
                address,
                bytes,
                exclusive,
                result,
                ..
            } => {
                let place = lowering.place(*result);
                let code = match (*bytes, *exclusive || place.wide) {
                    (1, false) => Code::Load1,
                    (2, false) => Code::Load2,
                    (4, false) => Code::Load4,
                    (8, false) => Code::Load8,
                    _ => Code::Access,
                };
                let address = lowering.word(*address);
                if code == Code::Access {
                    let index = narrow(parts.accesses.len())?;
                    parts.accesses.push(Access16::Read {
                        address,
                        result: place,
                        bytes: *bytes,
                        exclusive: *exclusive,
                        exit: leave,
                    });
                    lowering.effect(code, 0, 0, 0, 0, index.into());
                } else {
                    lowering.effect(code, *bytes, place.slot, address, 0, leave.into());
                }
            }
            Event::WriteMemory {
                address,
                value,
                bytes,
                exclusive,
                result,
                ..
            } => {
                let code = match (*bytes, *exclusive) {
                    (1, false) => Code::Store1,
                    (2, false) => Code::Store2,
                    (4, false) => Code::Store4,
                    (8, false) => Code::Store8,
                    _ => Code::Access,
                };
                let at = lowering.word(*address);
                if code == Code::Access {
                    let index = narrow(parts.accesses.len())?;
                    parts.accesses.push(Access16::Write {
                        address: at,
                        value: lowering.place(*value),
                        bytes: *bytes,
                        exclusive: match (exclusive, result) {
                            (false, _) => None,
                            (true, Some(result)) => Some(lowering.word(*result)),
                            (true, None) => {
                                return Err(host("an exclusive store without a result"));
                            }
                        },
                        exit: leave,
                    });
                    lowering.effect(code, 0, 0, 0, 0, index.into());
                } else {
                    let value = lowering.word(*value);
                    lowering.effect(code, *bytes, 0, at, value, leave.into());
                }
            }
            Event::Call { .. } | Event::Exit(_) | Event::Checkpoint(_) => {}
        }
    }
    let Some(outputs) = block_outputs else {
        return Err(host("a block without an exit"));
    };
    Ok(Compiled {
        pc,
        context,
        instructions,
        ops: lowering.ops,
        generics: lowering.generics,
        accesses: parts.accesses,
        leaves: parts.leaves,
        frame: lowering.frame,
        calls: parts.calls,
        inputs: parts.inputs,
        outputs,
    })
}

fn bits_mask(bytes: u8) -> u128 {
    if bytes >= 16 {
        u128::MAX
    } else {
        (1u128 << (u32::from(bytes) * 8)) - 1
    }
}

/// A load of `N` bytes, 1 to 8, into one word.
#[inline(always)]
fn load<M: Memory, const N: usize>(
    op: &Op,
    frame: &mut [u64],
    memory: &mut M,
    side: &mut Side,
) -> Result<(), (u32, Failed)> {
    let at = frame[usize::from(op.a)];
    // The common case: nothing keeps the accesses, and the memory holds the
    // address. A read that fails has changed nothing, and the general path
    // reads again to say why.
    if side.log.is_none() {
        let mut bytes = [0u8; 8];
        if memory.read(at, &mut bytes[..N]).is_ok() {
            frame[usize::from(op.dst)] = u64::from_le_bytes(bytes);
            return Ok(());
        }
    }
    match side.load(memory, at, N as u8, false) {
        Ok(value) => {
            frame[usize::from(op.dst)] = value as u64;
            Ok(())
        }
        Err(fault) => Err((op.imm as u32, Failed::Access(fault, at, false))),
    }
}

/// A store of the low `N` bytes, 1 to 8, of one word.
#[inline(always)]
fn store<M: Memory, const N: usize>(
    op: &Op,
    frame: &[u64],
    memory: &mut M,
    side: &mut Side,
    single: bool,
) -> Result<(), (u32, Failed)> {
    let at = frame[usize::from(op.a)];
    let value = frame[usize::from(op.b)];
    let exit = op.imm as u32;
    let code = side.is_code(at, N as u8);
    if !single && code {
        return Err((exit, Failed::Retry));
    }
    // The common case: no exclusive mark to carry over the store, and
    // nothing keeps the accesses. A write that fails has changed nothing,
    // and the general path writes again to say why.
    if side.exclusive.is_none()
        && side.log.is_none()
        && memory.write(at, &value.to_le_bytes()[..N]).is_ok()
    {
        side.own_writes += 1;
        side.code_written |= code;
        return Ok(());
    }
    side.store(memory, at, N as u8, u128::from(value) & bits_mask(N as u8))
        .map_err(|fault| (exit, Failed::Access(fault, at, true)))
}

impl Compiled {
    /// Make one effect. The error: the exit to leave through, and why.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn effect<M: Memory>(
        op: &Op,
        frame: &mut [u64],
        accesses: &[Access16],
        calls: &[Call],
        state: &mut State,
        memory: &mut M,
        side: &mut Side,
        single: bool,
    ) -> Result<(), (u32, Failed)> {
        match op.code {
            Code::Guard => {
                if (frame[usize::from(op.a)] != 0) != (op.aux != 0) {
                    return Err((op.imm as u32, Failed::Guard));
                }
                Ok(())
            }
            Code::Load1 => load::<M, 1>(op, frame, memory, side),
            Code::Load2 => load::<M, 2>(op, frame, memory, side),
            Code::Load4 => load::<M, 4>(op, frame, memory, side),
            Code::Load8 => load::<M, 8>(op, frame, memory, side),
            Code::Store1 => store::<M, 1>(op, frame, memory, side, single),
            Code::Store2 => store::<M, 2>(op, frame, memory, side, single),
            Code::Store4 => store::<M, 4>(op, frame, memory, side, single),
            Code::Store8 => store::<M, 8>(op, frame, memory, side, single),
            Code::Access => Self::access(accesses[op.imm as usize], frame, memory, side, single),
            Code::Call => {
                Self::call(&calls[op.imm as usize], frame, state);
                Ok(())
            }
            // `block::step` runs every other code.
            _ => Ok(()),
        }
    }

    #[inline(never)]
    fn call(call: &Call, frame: &mut [u64], state: &mut State) {
        let mut args = [0u128; CALL_ARGUMENTS];
        for (arg, place) in args.iter_mut().zip(&call.args) {
            *arg = place.read(frame);
        }
        let args = &args[..call.args.len()];
        match call.function {
            HelperFn::Pure(function) => {
                let (place, mask) = call.results[0];
                place.write(frame, function(args) & mask);
            }
            // An ordered helper sees the state as it was at the entry of the
            // block for every register slot: the slots are written back at
            // the exit. What it may touch is the state that is not in a slot.
            HelperFn::Ordered(function) => {
                let mut results = [0u128; CALL_ARGUMENTS];
                let count = call.results.len().min(CALL_ARGUMENTS);
                function(&mut *state, args, &mut results[..count]);
                for ((place, mask), result) in call.results.iter().zip(results) {
                    place.write(frame, result & mask);
                }
            }
        }
    }

    /// A load or a store of 16 bytes, or an exclusive one.
    #[inline(never)]
    fn access<M: Memory>(
        access: Access16,
        frame: &mut [u64],
        memory: &mut M,
        side: &mut Side,
        single: bool,
    ) -> Result<(), (u32, Failed)> {
        match access {
            Access16::Read {
                address,
                result,
                bytes,
                exclusive,
                exit,
            } => {
                let at = frame[usize::from(address)];
                match side.load(memory, at, bytes, exclusive) {
                    Ok(value) => result.write(frame, value),
                    Err(fault) => return Err((exit, Failed::Access(fault, at, false))),
                }
            }
            Access16::Write {
                address,
                value,
                bytes,
                exclusive,
                exit,
            } => {
                let at = frame[usize::from(address)];
                let value = value.read(frame) & bits_mask(bytes);
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
                        frame[usize::from(result)] = u64::from(!succeeded);
                    }
                }
            }
        }
        Ok(())
    }

    /// Leave through `exit`, which `failed` asked for: the registers as they
    /// were before the instruction that could not complete.
    #[cold]
    fn leave(&self, state: &mut State, exit: u32, failed: Failed) -> (u32, Option<Halt>) {
        let leave = &self.leaves[exit as usize];
        for (register, place) in &leave.outputs {
            state.slots[*register as usize] = place.read(&self.frame);
        }
        let pc = state.pc();
        let halt = match failed {
            Failed::Retry => Halt::Retry,
            // The guards of this card are alignment checks, and their exit
            // holds the address.
            Failed::Guard => Halt::Stop(Stop::Alignment {
                pc,
                address: leave
                    .address
                    .map_or(0, |slot| self.frame[usize::from(slot)]),
            }),
            Failed::Access(Fault::Memory, address, write) => {
                Halt::Stop(Stop::DataAbort { pc, address, write })
            }
            Failed::Access(Fault::Unaligned, address, _) => {
                Halt::Stop(Stop::Alignment { pc, address })
            }
            Failed::Access(Fault::Host(message), ..) => Halt::Stop(Stop::Host(message)),
        };
        (leave.completed, Some(halt))
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
        for (place, register) in &self.inputs {
            place.write(&mut self.frame, state.slots[*register as usize]);
        }
        // A block of one instruction makes its stores also into a page of
        // translated code, and the loop drops the blocks after it.
        let single = self.instructions == 1;
        for op in &self.ops {
            if (op.code as u8) < FIRST_EFFECT {
                if !block::step(op, &mut self.frame, &self.generics) {
                    return (
                        0,
                        Some(Halt::Stop(host(
                            "an arithmetic operation outside its domain",
                        ))),
                    );
                }
                continue;
            }
            let done = Self::effect(
                op,
                &mut self.frame,
                &self.accesses,
                &self.calls,
                state,
                memory,
                side,
                single,
            );
            if let Err((exit, failed)) = done {
                return self.leave(state, exit, failed);
            }
        }
        for (register, place) in &self.outputs {
            state.slots[*register as usize] = place.read(&self.frame);
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
    /// Instructions that have run since the CPU was made.
    pub instructions: u64,
    /// Blocks translated since the CPU was made.
    pub translations: u64,
    blocks: Cache,
    /// Blocks of one instruction, for `step` and for the end of a budget.
    singles: Cache,
    /// The write generation of the memory that the caches were filled under.
    generation: u64,
    side: Side,
}

impl<M: Memory> Cpu<M> {
    pub fn new(memory: M, state: State) -> Self {
        let generation = memory.generation();
        Self {
            state,
            memory,
            instructions: 0,
            translations: 0,
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

    /// Translate the block at the PC of the state, of at most `limit`
    /// instructions.
    fn translate(&mut self, limit: u32) -> Result<Compiled, Stop> {
        let pc = self.state.pc();
        let context = self.state.context;
        if pc & 3 != 0 {
            return Err(Stop::PcAlignment { pc });
        }
        let mut builder = Builder::new(context, pc);
        let mut at = pc;
        let mut count = 0;
        loop {
            let mut bytes = [0u8; 4];
            match self.memory.read(at, &mut bytes) {
                Ok(()) => {}
                Err(Error::Memory(_)) if count == 0 => return Err(Stop::FetchAbort { pc }),
                // The block ends before the instruction. It is looked at
                // again as the first of the next block.
                Err(Error::Memory(_)) => break,
                Err(error) => return Err(Stop::Host(error.to_string())),
            }
            let word = u32::from_le_bytes(bytes);
            let Some(decoded) = decode(word) else {
                if count == 0 {
                    return Err(Stop::Undefined { pc, word });
                }
                break;
            };
            builder.begin(at);
            match lift(&mut builder, &decoded) {
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
                    return Err(Stop::Unimplemented {
                        pc,
                        record: decoded.record,
                    });
                }
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
        compile(pc, context, count, builder.finish(at))
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

    /// Run the block at the PC: one instruction when `single`, else a whole
    /// block. `Err`: no block could be made.
    fn run_block(&mut self, single: bool) -> Result<(u32, Option<Halt>), Stop> {
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
            let compiled = Box::new(self.translate(limit)?);
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
        Ok(block.run(&mut self.state, &mut self.memory, &mut self.side))
    }

    /// Run exactly one instruction.
    pub fn step(&mut self) -> Result<(), Stop> {
        self.check_generation();
        let result = self.run_block(true);
        self.settle();
        match result? {
            (count, None) => {
                self.instructions += u64::from(count);
                Ok(())
            }
            (_, Some(Halt::Stop(stop))) => Err(stop),
            (_, Some(Halt::Retry)) => Err(host("a block of one instruction asked to run alone")),
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
            // The common case: the block is translated and fits the budget.
            let outcome = if let Some(block) = self.blocks.entries[Cache::index(pc)].as_mut()
                && block.pc == pc
                && block.context == context
                && u64::from(block.instructions) <= left
            {
                block.run(&mut self.state, &mut self.memory, &mut self.side)
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
                            if let Some(Halt::Stop(stop)) = halt {
                                return stop;
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
        // The word after the loop is zero: UDF, which is not in the group.
        let stop = cpu.run(u64::MAX);
        assert!(
            matches!(stop, Stop::Unimplemented { pc, .. } if pc == BASE + 12),
            "{stop:?}"
        );
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

    #[test]
    fn what_cannot_run_stops_the_loop_with_its_reason_and_an_unchanged_state() {
        // Outside the memory.
        let mut cpu = cpu(&[]);
        cpu.state.set_pc(0x1000);
        let before = cpu.state.clone();
        assert_eq!(cpu.run(10), Stop::FetchAbort { pc: 0x1000 });
        assert_eq!(cpu.state, before);
        // Not a multiple of four.
        cpu.state.set_pc(BASE + 2);
        assert_eq!(cpu.step(), Err(Stop::PcAlignment { pc: BASE + 2 }));
        // An encoding that no record of the selected sets matches: an add
        // with the immediate shift 2, which tables/constraints.toml refuses.
        let mut cpu = self::cpu(&[0x9180_0400]);
        assert_eq!(
            cpu.run(10),
            Stop::Undefined {
                pc: BASE,
                word: 0x9180_0400
            }
        );
        // A block ends before an instruction that cannot run, and the next
        // attempt names it.
        let mut cpu = self::cpu(&[0x9100_0400, 0x9180_0400]);
        assert_eq!(
            cpu.run(10),
            Stop::Undefined {
                pc: BASE + 4,
                word: 0x9180_0400
            }
        );
        assert_eq!(cpu.state.x(0), 1);
        assert_eq!(cpu.instructions, 1);
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
        // The word after them is zero: UDF, which has no lifter.
        let stop = cpu.run(u64::MAX);
        assert!(
            matches!(stop, Stop::Unimplemented { pc, .. } if pc == BASE + 16),
            "{stop:?}"
        );
        // A scalar result clears the register above it.
        assert_eq!(cpu.state.v(0), u128::from(3.75f64.to_bits()));
        assert_eq!(cpu.state.v(3), u128::from(f64::INFINITY.to_bits()));
        assert_eq!(cpu.state.x(5), 3);
        // 1.5 < 2.25: N.
        assert_eq!(cpu.state.nzcv(), 0b1000);
        // A division by zero, then an inexact conversion. The bits stay.
        assert_eq!(cpu.state.fpsr(), u32::from(fpsr::DZC | fpsr::IXC));
        // The four instructions were one block, and stepping gives the same.
        assert_eq!(cpu.translations, 1);
        let mut step = self::cpu(&[0x1e62_2820, 0x1e64_1823, 0x1e78_0005, 0x1e62_2020]);
        step.state.set_v(0, u128::MAX);
        step.state
            .set_v(1, u128::from(1.5f64.to_bits()) | 0xdead << 64);
        step.state.set_v(2, u128::from(2.25f64.to_bits()));
        step.state.set_v(3, u128::MAX);
        step.state.set_x(5, u64::MAX);
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
        assert_eq!(cpu.run(1), Stop::FetchAbort { pc: BASE + 0x2000 });
    }
}
