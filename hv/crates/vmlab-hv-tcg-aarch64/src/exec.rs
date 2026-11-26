//! The interpreter loop: translate a block at the PC, run it on the slot
//! executor of `vmlab-hv-tcg-ir`, write the registers back, repeat.
//!
//! Addresses are physical addresses on a `Memory`: the MMU is card C8. The
//! loop takes the exceptions and the interrupts of the guest itself
//! (`exception.rs`) and runs the system instructions that end a block
//! (`Trap`). It stops for what only its owner can serve: a hypercall, a
//! `WFI` with no interrupt line high, the end of a budget, and an
//! instruction that has no lifter yet.
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

/// A block as the loop runs it.
struct Compiled {
    pc: u64,
    context: Context,
    instructions: u32,
    code: Vec<Packed>,
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

/// One helper call of a block.
struct Call {
    /// It runs before the packed instruction of this index.
    at: u32,
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

/// Append the scalars of `order` to the block: constants and inputs are
/// bound, a pure helper call becomes a `Call`, and the rest is packed.
fn emit(
    graph: &Graph,
    slots: &[usize],
    slot_count: usize,
    order: Vec<ScalarId>,
    compiled: &mut Compiled,
) -> Result<(), Stop> {
    for id in order {
        match &graph.scalars[id].operation {
            Operation::Constant(value) => compiled.values[slots[id]] = *value,
            Operation::Input(register) => {
                compiled
                    .inputs
                    .push((narrow(slots[id])?, narrow(*register)?));
            }
            // A view of another scalar shares its slot.
            Operation::Refine(_) => {}
            Operation::Effect(_) => return Err(host("a memory effect has no executor yet")),
            Operation::Call { helper, args } => {
                if args.len() > CALL_ARGUMENTS {
                    return Err(host("a helper with too many arguments"));
                }
                compiled.calls.push(Call {
                    at: narrow(compiled.code.len())?,
                    function: helper.0.function,
                    args: args
                        .iter()
                        .map(|arg| narrow(slots[*arg]))
                        .collect::<Result<_, _>>()?,
                    results: vec![(narrow(slots[id])?, result_mask(graph, id))],
                });
            }
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

fn compile(
    pc: u64,
    context: Context,
    instructions: u32,
    mut block: BlockOf<A64Exit>,
) -> Result<Compiled, Stop> {
    dce::prune_scalars(&mut block);
    let graph = &block.graph;
    let Some((Event::Exit(exit), before)) = block.events.split_last() else {
        return Err(host("an aarch64 block ends in its one exit"));
    };
    let exit = &block.exits[*exit];
    let (next, terminal) = match exit.next {
        A64Next::Pc(next) => (Some(next), None),
        A64Next::Trap { pc, trap } => (None, Some((pc, trap))),
    };
    let (slots, slot_count) = linear::slots(graph);
    let mut seen = vec![false; graph.scalars.len()];
    let mut compiled = Compiled {
        pc,
        context,
        instructions,
        code: Vec::new(),
        values: vec![0; slot_count],
        calls: Vec::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        terminal,
    };
    // The ordered helper calls, each after what its arguments need.
    for event in before {
        let Event::Call {
            helper,
            args,
            results,
        } = event
        else {
            return Err(host(
                "an event of an aarch64 block that has no executor yet",
            ));
        };
        if !matches!(helper.0.function, HelperFn::Ordered(_)) || args.len() > CALL_ARGUMENTS {
            return Err(host("an ordered call of a helper that is not one"));
        }
        let order = schedule::scalars(graph, args, &mut seen);
        emit(graph, &slots, slot_count, order, &mut compiled)?;
        for result in results {
            seen[*result] = true;
        }
        compiled.calls.push(Call {
            at: narrow(compiled.code.len())?,
            function: helper.0.function,
            args: args
                .iter()
                .map(|arg| narrow(slots[*arg]))
                .collect::<Result<_, _>>()?,
            results: results
                .iter()
                .map(|result| Ok((narrow(slots[*result])?, result_mask(graph, *result))))
                .collect::<Result<_, Stop>>()?,
        });
    }
    let mut roots: Vec<_> = exit
        .writeback
        .writes
        .iter()
        .map(|(_, scalar)| *scalar)
        .collect();
    roots.extend(next);
    let order = schedule::scalars(graph, &roots, &mut seen);
    emit(graph, &slots, slot_count, order, &mut compiled)?;
    for (register, scalar) in &exit.writeback.writes {
        compiled
            .outputs
            .push((narrow(*register)?, narrow(slots[*scalar])?));
    }
    if let Some(next) = next {
        compiled
            .outputs
            .push((slot::PC as u32, narrow(slots[next])?));
    }
    Ok(compiled)
}

/// Run the packed instructions of a block that has helper calls, each call
/// at its place.
fn run_with_calls(
    code: &[Packed],
    calls: &[Call],
    values: &mut [u128],
    state: &mut State,
) -> Result<(), Stop> {
    let mut from = 0;
    let ends = calls
        .iter()
        .map(|call| (call.at as usize, Some(call)))
        .chain([(code.len(), None)]);
    for (until, call) in ends {
        for packed in &code[from..until] {
            let Some(value) = packed.evaluate(values) else {
                return Err(host("an arithmetic operation outside its domain"));
            };
            values[packed.output as usize] = value;
        }
        from = until;
        let Some(call) = call else {
            break;
        };
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
            // An ordered helper sees the state as it was at the entry of the
            // block for every register slot: the slots are written back at
            // the exit. What it may touch is the state that is not in a slot.
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
    Ok(())
}

impl Compiled {
    #[inline]
    fn run(&mut self, state: &mut State) -> Result<(), Stop> {
        let values = &mut self.values;
        for (scratch, register) in &self.inputs {
            values[*scratch as usize] = state.slots[*register as usize];
        }
        if self.calls.is_empty() {
            for packed in &self.code {
                let Some(value) = packed.evaluate(values) else {
                    return Err(host("an arithmetic operation outside its domain"));
                };
                values[packed.output as usize] = value;
            }
        } else {
            run_with_calls(&self.code, &self.calls, values, state)?;
        }
        for (register, scratch) in &self.outputs {
            state.slots[*register as usize] = values[*scratch as usize];
        }
        // The loop runs the last instruction with the PC at it.
        if let Some((pc, _)) = self.terminal {
            state.set_pc(pc);
        }
        Ok(())
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
        }
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
                return Ok(());
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
                Lifted::Unsupported => break,
            }
            // A block stays inside one page, so that the page of its code
            // names it for a later invalidation.
            if count >= limit || at & 0xfff == 0 {
                break;
            }
        }
        self.translations += 1;
        compile(pc, context, count, builder.finish(at)).map_err(Refused::Stop)
    }

    /// Drop every block when a write has reached the memory since the
    /// caches were filled. Card C8 replaces this with the code stamps of the
    /// pages.
    fn check_generation(&mut self) {
        let generation = self.memory.generation();
        if generation != self.generation {
            self.generation = generation;
            self.blocks.clear();
            self.singles.clear();
        }
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

    /// Run one block, or take what comes before it. The count of
    /// instructions that it used of a budget, which is one for an exception
    /// without an instruction, so that a guest with a broken vector does not
    /// hold the loop for ever.
    fn run_block(&mut self, single: bool) -> Result<u32, Stop> {
        if (self.irq || self.fiq || self.state.il) && self.asynchronous() {
            return Ok(1);
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
                    return Ok(1);
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
        block.run(&mut self.state)?;
        let (count, terminal) = (block.instructions, block.terminal);
        self.instructions += u64::from(count);
        if let Some((pc, trap)) = terminal {
            self.trap(pc, trap)?;
        }
        Ok(count)
    }

    /// Run exactly one instruction, or take the interrupt or the exception
    /// that comes before it. `taken` says which.
    pub fn step(&mut self) -> Result<(), Stop> {
        self.taken = None;
        self.check_generation();
        self.run_block(true).map(|_| ())
    }

    /// Run up to `budget` instructions and say why the loop stopped.
    pub fn run(&mut self, budget: u64) -> Stop {
        self.check_generation();
        let mut left = budget;
        while left > 0 {
            let pc = self.state.pc();
            let context = self.state.context;
            let quiet = !(self.irq || self.fiq || self.state.il);
            // The common case: the block is translated and fits the budget.
            if quiet
                && let Some(block) = self.blocks.entries[Cache::index(pc)].as_mut()
                && block.pc == pc
                && block.context == context
                && u64::from(block.instructions) <= left
            {
                if let Err(stop) = block.run(&mut self.state) {
                    return stop;
                }
                let count = u64::from(block.instructions);
                let terminal = block.terminal;
                self.instructions += count;
                left -= count;
                if let Some((pc, trap)) = terminal
                    && let Err(stop) = self.trap(pc, trap)
                {
                    return stop;
                }
                continue;
            }
            // A whole block where the budget allows one, else one
            // instruction. A block that is not translated yet can be as long
            // as `BLOCK_INSTRUCTIONS`.
            let cached = self.blocks.entries[Cache::index(pc)]
                .as_ref()
                .filter(|block| block.pc == pc && block.context == context)
                .map(|block| u64::from(block.instructions));
            let single = match cached {
                Some(length) => length > left,
                None => left < u64::from(BLOCK_INSTRUCTIONS),
            };
            match self.run_block(single) {
                Ok(count) => left = left.saturating_sub(u64::from(count)),
                Err(stop) => return stop,
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
        handler(&mut cpu, &[0x9100_0421, 0xd538_4031, 0x9100_1231, 0xd518_4031, ERET]);
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
            (cpu.state.x(0), cpu.state.x(1), cpu.state.x(2), cpu.state.x(3)),
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
        assert_eq!(user.taken, sync(Exception::Undefined, BASE, VECTORS + 0x400));
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
            cpu.unknown.iter().map(Unknown::to_string).collect::<Vec<_>>(),
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
        });
        block.events.push(Event::Exit(exit));
        let context = State::reset(0).context;
        let mut compiled = compile(0x4000_0000, context, 1, block).unwrap();
        let mut state = State::reset(0x4000_0000);
        state.set_x(0, 5);
        state.set_x(1, 7);
        compiled.run(&mut state).unwrap();
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
