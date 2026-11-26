//! The interpreter loop: translate a block at the PC, run it on the slot
//! executor of `vmlab-hv-tcg-ir`, write the registers back, repeat.
//!
//! This is the minimal loop of card C1: physical addresses on a `Memory`
//! (no MMU, no exception entry, no interrupts). An instruction that it
//! cannot run stops it with the reason, and the caller decides.
//!
//! Sources read: none. This file holds no instruction semantics.

use crate::generated::decode::{Record, decode};
use crate::lift::{A64Exit, A64Next, Builder, Lifted, lift};
use crate::state::{Context, State, slot};
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

/// Why the loop stopped. The PC of the state is the instruction that did
/// not run, and no part of that instruction has changed the state.
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
    /// An error of the host, not of the guest.
    Host(String),
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
    let A64Next::Pc(next) = exit.next else {
        return Err(host("an exception exit has no executor yet"));
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
    roots.push(next);
    let order = schedule::scalars(graph, &roots, &mut seen);
    emit(graph, &slots, slot_count, order, &mut compiled)?;
    for (register, scalar) in &exit.writeback.writes {
        compiled
            .outputs
            .push((narrow(*register)?, narrow(slots[*scalar])?));
    }
    compiled
        .outputs
        .push((slot::PC as u32, narrow(slots[next])?));
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
    /// Instructions that have run since the CPU was made.
    pub instructions: u64,
    /// Blocks translated since the CPU was made.
    pub translations: u64,
    blocks: Cache,
    /// Blocks of one instruction, for `step` and for the end of a budget.
    singles: Cache,
    /// The write generation of the memory that the caches were filled under.
    generation: u64,
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
        }
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
                Lifted::Unsupported => break,
            }
            // A block stays inside one page, so that the page of its code
            // names it for a later invalidation.
            if count >= limit || at & 0xfff == 0 {
                break;
            }
        }
        self.translations += 1;
        compile(pc, context, count, builder.finish(at))
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

    fn run_block(&mut self, single: bool) -> Result<u32, Stop> {
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
        block.run(&mut self.state)?;
        Ok(block.instructions)
    }

    /// Run exactly one instruction.
    pub fn step(&mut self) -> Result<(), Stop> {
        self.check_generation();
        let count = self.run_block(true)?;
        self.instructions += u64::from(count);
        Ok(())
    }

    /// Run up to `budget` instructions and say why the loop stopped.
    pub fn run(&mut self, budget: u64) -> Stop {
        self.check_generation();
        let mut left = budget;
        while left > 0 {
            let pc = self.state.pc();
            let context = self.state.context;
            // The common case: the block is translated and fits the budget.
            if let Some(block) = self.blocks.entries[Cache::index(pc)].as_mut()
                && block.pc == pc
                && block.context == context
                && u64::from(block.instructions) <= left
            {
                if let Err(stop) = block.run(&mut self.state) {
                    return stop;
                }
                let count = u64::from(block.instructions);
                self.instructions += count;
                left -= count;
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
                Ok(count) => {
                    let count = u64::from(count);
                    self.instructions += count;
                    left = left.saturating_sub(count);
                }
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
        assert_eq!(cpu.run(1), Stop::FetchAbort { pc: BASE + 0x2000 });
    }
}
