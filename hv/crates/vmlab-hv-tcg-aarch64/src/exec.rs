//! The interpreter loop: translate a block at the PC, run it on the slot
//! executor of `vmlab-hv-tcg-ir`, write the registers back, repeat.
//!
//! Addresses are physical addresses on a `Memory`: no MMU, no exception
//! entry, no interrupts. An instruction that the loop cannot run stops it
//! with the reason, and the caller decides. The exception card (C6) turns
//! the stops into exception entries.
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
use vmlab_hv_tcg_ir::native::block::{BlockOf, Event};
use vmlab_hv_tcg_ir::native::linear;
use vmlab_hv_tcg_ir::native::packed::Packed;
use vmlab_hv_tcg_ir::native::{Graph, Operation, ScalarId, dce, schedule};

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
    /// The block that runs is one instruction: a store into a code page is
    /// made, and the caches are dropped after it.
    single: bool,
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
            single: false,
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
        } else if memory
            .reservation_generation(reservation.address, usize::from(reservation.bytes))
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

/// A memory event or a guard of a block, between its pure code.
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
    /// Scratch slot, register slot: copied in before the block runs.
    inputs: Vec<(u32, u32)>,
    /// Register slot, scratch slot: copied out after it.
    outputs: Vec<(u32, u32)>,
}

fn host(message: &str) -> Stop {
    Stop::Host(message.to_string())
}

fn narrow(value: usize) -> Result<u32, Stop> {
    u32::try_from(value).map_err(|_| host("a block with too many slots"))
}

/// Append the pure scalars that `roots` need and the block has not computed
/// yet.
fn emit(
    compiled: &mut Compiled,
    graph: &Graph,
    slots: &[usize],
    seen: &mut [bool],
    roots: &[ScalarId],
) -> Result<(), Stop> {
    let slot_count = compiled.values.len();
    for id in schedule::scalars(graph, roots, seen) {
        match graph.scalars[id].operation {
            Operation::Constant(value) => compiled.values[slots[id]] = value,
            Operation::Input(register) => {
                compiled
                    .inputs
                    .push((narrow(slots[id])?, narrow(register)?));
            }
            // A view of another scalar shares its slot.
            Operation::Refine(_) => {}
            // The event of the effect writes its slot.
            Operation::Effect(_) => {}
            _ => {
                let mut instruction = linear::lower(graph, id)
                    .ok_or_else(|| host("a scalar without a linear form"))?;
                instruction.remap(slots);
                let mut packed = Packed::new(slots[id], instruction)
                    .ok_or_else(|| host("a scalar without a packed form"))?;
                if let Operation::Binary(_, _, right) = graph.scalars[id].operation
                    && let Some(value) = graph.literal(right)
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
        inputs: Vec::new(),
        outputs: Vec::new(),
    };
    // The exits that events leave through, by their number in the block.
    let mut leave_of = vec![None; block.exits.len()];
    let mut ended = false;
    for event in &block.events {
        if ended {
            return Err(host("an event after the exit of a block"));
        }
        let (exit, mut roots) = match *event {
            Event::Guard {
                condition, exit, ..
            } => (exit, vec![condition]),
            Event::ReadMemory { address, error, .. } => (error, vec![address]),
            Event::WriteMemory {
                address,
                value,
                error,
                ..
            } => (error, vec![address, value]),
            Event::Exit(exit) => (exit, Vec::new()),
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
            let A64Next::Pc(next) = payload.next else {
                return Err(host(
                    "a block that ends in an exception has no executor yet",
                ));
            };
            outputs.push((slot::PC as u32, narrow(slots[next])?));
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
        let effect = match *event {
            Event::Guard {
                condition,
                expected,
                ..
            } => Effect::Guard {
                condition: narrow(slots[condition])?,
                expected,
                exit: leave,
            },
            Event::ReadMemory {
                address,
                bytes,
                exclusive,
                result,
                ..
            } => Effect::Read {
                address: narrow(slots[address])?,
                result: narrow(slots[result])?,
                bytes,
                exclusive,
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
                address: narrow(slots[address])?,
                value: narrow(slots[value])?,
                bytes,
                exclusive: match (exclusive, result) {
                    (false, _) => None,
                    (true, Some(result)) => Some(narrow(slots[result])?),
                    (true, None) => {
                        return Err(host("an exclusive store without a result"));
                    }
                },
                exit: leave,
            },
            Event::Exit(_) | Event::Checkpoint(_) => continue,
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
    #[inline]
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
        memory: &mut M,
        side: &mut Side,
    ) -> Result<(), (u32, Failed)> {
        let values = &mut self.values;
        match effect {
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
                if !side.single && side.is_code(at, bytes) {
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
        let mut done = 0usize;
        for index in 0..self.effects.len() {
            let (before, effect) = self.effects[index];
            let before = before as usize;
            if let Err(halt) = Self::pure(&self.code[done..before], &mut self.values) {
                return (0, Some(halt));
            }
            done = before;
            if let Err((exit, failed)) = self.effect(effect, memory, side) {
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
                return (leave.completed, Some(halt));
            }
        }
        if let Err(halt) = Self::pure(&self.code[done..], &mut self.values) {
            return (0, Some(halt));
        }
        for (register, scratch) in &self.outputs {
            state.slots[*register as usize] = self.values[*scratch as usize];
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
        // A block of one instruction makes its stores also into a code page.
        self.side.single = single || block.instructions == 1;
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
                self.side.single = block.instructions == 1;
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
