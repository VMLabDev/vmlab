use super::Operation;
use super::block::{Block, BlockOf, Event, ExitScalars, Recipe};
use super::{Graph, ScalarId};
use std::collections::BTreeSet;
use std::sync::Arc;

/// The scalars that `roots` reach, into `live`. `roots` is used up.
fn live(graph: &Graph, roots: &mut Vec<ScalarId>, live: &mut Vec<bool>) {
    live.clear();
    live.resize(graph.scalars.len(), false);
    while let Some(id) = roots.pop() {
        if std::mem::replace(&mut live[id], true) {
            continue;
        }
        match graph.scalars[id].operation {
            Operation::Refine(value)
            | Operation::Unary(_, value)
            | Operation::Slice { value, .. } => roots.push(value),
            Operation::Binary(_, a, b)
            | Operation::Concat {
                high: a, low: b, ..
            } => roots.extend([a, b]),
            Operation::Select {
                condition,
                when_true,
                when_false,
            } => roots.extend([condition, when_true, when_false]),
            Operation::Call { ref args, .. } => roots.extend(args.iter().copied()),
            _ => {}
        }
    }
}

/// The buffers of `prune_scalars_with`, which a translator keeps from one
/// block to the next (3e DESIGN.md J11).
#[derive(Default)]
pub struct PruneBuffers {
    roots: Vec<ScalarId>,
    live: Vec<bool>,
    map: Vec<ScalarId>,
}

/// Dead code elimination for a block whose exits name scalars directly
/// (R8): keep what the events and the exits reach, drop the rest, and
/// renumber. A register slot that a block writes twice keeps only the value
/// of the last write, because the exit names only that one.
pub fn prune_scalars<X: ExitScalars>(block: &mut BlockOf<X>) {
    prune_scalars_with(block, &mut PruneBuffers::default());
}

/// `prune_scalars` with the memory of `buffers`: no heap call of its own
/// once they have grown.
pub fn prune_scalars_with<X: ExitScalars>(block: &mut BlockOf<X>, buffers: &mut PruneBuffers) {
    let PruneBuffers { roots, live: alive, map } = buffers;
    roots.clear();
    for event in &block.events {
        match *event {
            Event::Guard { condition, .. } => roots.push(condition),
            Event::ReadMemory {
                address, result, ..
            } => roots.extend([address, result]),
            Event::WriteMemory {
                address,
                value,
                result,
                ..
            } => {
                roots.extend([address, value]);
                roots.extend(result);
            }
            // An ordered call stays, with its arguments and its results.
            Event::Call {
                ref args,
                ref results,
                ..
            } => {
                roots.extend(args.iter().copied());
                roots.extend(results.iter().copied());
            }
            Event::Checkpoint(_) | Event::Exit(_) => {}
        }
    }
    for exit in &mut block.exits {
        exit.for_each_scalar(&mut |scalar| roots.push(*scalar));
    }
    live(&block.graph, roots, alive);
    block.graph.retain_into(alive, map);
    for event in &mut block.events {
        match event {
            Event::Guard { condition, .. } => *condition = map[*condition],
            Event::ReadMemory {
                address, result, ..
            } => {
                *address = map[*address];
                *result = map[*result];
            }
            Event::WriteMemory {
                address,
                value,
                result,
                ..
            } => {
                *address = map[*address];
                *value = map[*value];
                if let Some(result) = result {
                    *result = map[*result];
                }
            }
            Event::Call { args, results, .. } => {
                for scalar in args.iter_mut().chain(results.iter_mut()) {
                    *scalar = map[*scalar];
                }
            }
            Event::Checkpoint(_) | Event::Exit(_) => {}
        }
    }
    for exit in &mut block.exits {
        exit.for_each_scalar(&mut |scalar| *scalar = map[*scalar]);
    }
}

/// Remove discarded specialization work while preserving every checkpoint,
/// ordered effect, guard continuation, and constant configuration assumption.
/// Returns the retained frontend constant IDs in their new order.
pub fn prune(block: &mut Block) -> Vec<usize> {
    let mut exits = vec![false; block.exits.len()];
    for event in &block.events {
        match *event {
            Event::Guard { exit, .. } | Event::Exit(exit) => exits[exit] = true,
            Event::ReadMemory { error, .. } => exits[error] = true,
            Event::WriteMemory {
                error, invalidated, ..
            } => {
                exits[error] = true;
                exits[invalidated] = true;
            }
            _ => {}
        }
    }
    let mut exit_map = vec![usize::MAX; exits.len()];
    let mut next = 0;
    for (id, keep) in exits.iter().enumerate() {
        if *keep {
            exit_map[id] = next;
            next += 1;
        }
    }
    block.exits = std::mem::take(&mut block.exits)
        .into_iter()
        .enumerate()
        .filter_map(|(id, exit)| exits[id].then_some(exit))
        .collect();
    let mut values = Vec::new();
    let mut scalars = Vec::new();
    for event in &mut block.events {
        match event {
            Event::Guard {
                condition, exit, ..
            } => {
                scalars.push(*condition);
                *exit = exit_map[*exit];
            }
            Event::ReadMemory {
                address,
                result,
                error,
                ..
            } => {
                scalars.extend([*address, *result]);
                *error = exit_map[*error];
            }
            Event::WriteMemory {
                address,
                value,
                result,
                error,
                invalidated,
                ..
            } => {
                scalars.extend([*address, *value]);
                scalars.extend(*result);
                *error = exit_map[*error];
                *invalidated = exit_map[*invalidated];
            }
            Event::Exit(exit) => *exit = exit_map[*exit],
            _ => {}
        }
    }
    for exit in &block.exits {
        values.extend(exit.globals.iter().map(|b| b.value));
        values.extend(
            exit.frames
                .iter()
                .flat_map(|f| f.locals.iter().map(|b| b.value)),
        );
        values.extend(exit.returned);
        values.extend(exit.fetch_address);
    }
    for checkpoint in &block.checkpoints {
        values.extend(checkpoint.globals.iter().map(|b| b.value));
        values.push(checkpoint.waiting);
        values.extend(checkpoint.fetch_address);
    }
    let mut constants: BTreeSet<_> = block
        .context_guards
        .iter()
        .map(|guard| guard.expected)
        .collect();
    let mut live_values = vec![false; block.values.len()];
    while let Some(id) = values.pop() {
        if std::mem::replace(&mut live_values[id], true) {
            continue;
        }
        match &block.values[id] {
            Recipe::Constant(id) => {
                constants.insert(*id);
            }
            Recipe::Scalar { value, .. } => scalars.push(*value),
            Recipe::Struct(fields) => values.extend(fields.iter().map(|(_, value)| *value)),
            Recipe::Union(_, value) => values.push(*value),
            Recipe::Vector(elements) => values.extend(elements),
            Recipe::Deferred { args, .. } => values.extend(args),
            _ => {}
        }
    }
    let mut live_scalars = Vec::new();
    live(&block.graph, &mut scalars, &mut live_scalars);
    let scalar_map = block.graph.retain(&live_scalars);
    let constants: Vec<_> = constants.into_iter().collect();
    let constant_map: std::collections::BTreeMap<_, _> = constants
        .iter()
        .copied()
        .enumerate()
        .map(|(new, old)| (old, new))
        .collect();
    let mut value_map = vec![usize::MAX; live_values.len()];
    next = 0;
    for (id, keep) in live_values.iter().enumerate() {
        if *keep {
            value_map[id] = next;
            next += 1;
        }
    }
    block.values = std::mem::take(&mut block.values)
        .into_iter()
        .enumerate()
        .filter_map(|(id, mut recipe)| {
            if !live_values[id] {
                return None;
            }
            match &mut recipe {
                Recipe::Constant(id) => *id = constant_map[id],
                Recipe::Scalar { value, .. } => *value = scalar_map[*value],
                Recipe::Struct(fields) => {
                    for (_, value) in fields {
                        *value = value_map[*value];
                    }
                }
                Recipe::Union(_, value) => *value = value_map[*value],
                Recipe::Vector(values) | Recipe::Deferred { args: values, .. } => {
                    for value in values {
                        *value = value_map[*value];
                    }
                }
                _ => {}
            }
            Some(recipe)
        })
        .collect();
    for event in &mut block.events {
        match event {
            Event::Guard { condition, .. } => *condition = scalar_map[*condition],
            Event::ReadMemory {
                address, result, ..
            } => {
                *address = scalar_map[*address];
                *result = scalar_map[*result];
            }
            Event::WriteMemory {
                address,
                value,
                result,
                ..
            } => {
                *address = scalar_map[*address];
                *value = scalar_map[*value];
                if let Some(result) = result {
                    *result = scalar_map[*result];
                }
            }
            _ => {}
        }
    }
    for exit in &mut block.exits {
        for binding in Arc::make_mut(&mut exit.globals).iter_mut().chain(
            exit.frames
                .iter_mut()
                .flat_map(|frame| Arc::make_mut(&mut frame.locals).iter_mut()),
        ) {
            binding.value = value_map[binding.value];
        }
        exit.returned = exit.returned.map(|id| value_map[id]);
        exit.fetch_address = exit.fetch_address.map(|id| value_map[id]);
    }
    for checkpoint in &mut block.checkpoints {
        for binding in Arc::make_mut(&mut checkpoint.globals) {
            binding.value = value_map[binding.value];
        }
        checkpoint.waiting = value_map[checkpoint.waiting];
        checkpoint.fetch_address = checkpoint.fetch_address.map(|id| value_map[id]);
    }
    for guard in &mut block.context_guards {
        guard.expected = constant_map[&guard.expected];
    }
    fn intern(
        bindings: &mut Arc<Vec<super::block::Binding>>,
        pool: &mut super::fasthash::FastMap<
            Vec<super::block::Binding>,
            Arc<Vec<super::block::Binding>>,
        >,
    ) {
        if let Some(shared) = pool.get(bindings.as_ref()) {
            *bindings = shared.clone();
        } else {
            pool.insert(bindings.as_ref().clone(), bindings.clone());
        }
    }
    let mut pool = super::fasthash::FastMap::default();
    for exit in &mut block.exits {
        intern(&mut exit.globals, &mut pool);
        for frame in &mut exit.frames {
            intern(&mut frame.locals, &mut pool);
        }
    }
    for checkpoint in &mut block.checkpoints {
        intern(&mut checkpoint.globals, &mut pool);
    }
    constants
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::ScalarType;
    use crate::native::block::{Binding, Checkpoint, ContextGuard, Exit};

    #[test]
    fn discarded_snapshots_do_not_retain_values_but_ordered_reads_survive() {
        let mut block = Block::default();
        let dead = block.value(Recipe::Constant(0));
        let waiting = block.value(Recipe::Constant(4));
        let orphan = block.exit(Exit {
            frames: vec![],
            globals: vec![Binding {
                name: "unused".into(),
                value: dead,
            }]
            .into(),
            completed_steps: 0,
            returned: None,
            fetch_address: None,
        });
        let error = block.exit(Exit {
            frames: vec![],
            globals: vec![].into(),
            completed_steps: 0,
            returned: None,
            fetch_address: None,
        });
        assert_eq!(orphan, 0);
        let address = block.graph.constant(ScalarType::Bits(64), 8);
        let loaded = block.graph.effect(0, ScalarType::Bits(64));
        block.graph.input(100, ScalarType::Bits(64), None);
        block.events.push(Event::ReadMemory {
            address,
            bytes: 8,
            exclusive: false,
            result: loaded,
            error,
        });
        block.checkpoint(Checkpoint {
            globals: vec![].into(),
            waiting,
            completed_steps: 1,
            fetch_address: None,
        });
        block.context_guards.push(ContextGuard {
            name: "mode".into(),
            expected: 9,
            version: 1,
        });
        assert_eq!(prune(&mut block), vec![4, 9]);
        assert_eq!(block.exits.len(), 1);
        assert_eq!(block.values.len(), 1);
        assert_eq!(block.graph.scalars.len(), 2);
        assert!(matches!(
            block.events[0],
            Event::ReadMemory { error: 0, .. }
        ));
        assert!(matches!(block.values[0], Recipe::Constant(0)));
        assert_eq!(block.context_guards[0].expected, 1);
        assert_eq!(prune(&mut block), vec![0, 1]);
    }

    fn sum(args: &[u128]) -> u128 {
        args.iter().sum()
    }

    fn poke(context: &mut dyn std::any::Any, args: &[u128], results: &mut [u128]) {
        if let Some(count) = context.downcast_mut::<u32>() {
            *count += 1;
        }
        results[0] = args[0] + 1;
    }

    static SUM: crate::native::Helper = crate::native::Helper {
        name: "sum",
        args: 2,
        results: 1,
        effects: crate::native::Effects::NONE,
        function: crate::native::HelperFn::Pure(sum),
    };

    static POKE: crate::native::Helper = crate::native::Helper {
        name: "poke",
        args: 1,
        results: 1,
        effects: crate::native::Effects::WRITES_STATE,
        function: crate::native::HelperFn::Ordered(poke),
    };

    #[test]
    fn a_pure_call_is_one_scalar_and_dies_unused_and_an_ordered_call_stays() {
        use crate::native::regfile::Writeback;
        use crate::native::{HelperRef, Operation};
        let ty = ScalarType::Bits(64);
        let mut block: BlockOf<Writeback> = BlockOf::default();
        let a = block.graph.input(0, ty, None);
        let b = block.graph.input(1, ty, None);
        let first = block.graph.call(ty, &SUM, &[a, b]).unwrap();
        // The same helper on the same arguments is the same scalar.
        assert_eq!(block.graph.call(ty, &SUM, &[a, b]), Some(first));
        let other = block.graph.call(ty, &SUM, &[b, a]).unwrap();
        assert_ne!(other, first);
        // The count of arguments is checked, and an ordered helper is no scalar.
        assert_eq!(block.graph.call(ty, &SUM, &[a]), None);
        assert_eq!(block.graph.call(ty, &POKE, &[a]), None);
        let poked = block.graph.effect(0, ty);
        block.events.push(Event::Call {
            helper: HelperRef(&POKE),
            args: vec![b],
            results: vec![poked],
        });
        let exit = block.exit(Writeback {
            writes: vec![(0, first)],
        });
        block.events.push(Event::Exit(exit));
        prune_scalars(&mut block);
        // `other` is gone. The ordered call keeps its argument and its result,
        // though no exit names the result.
        let calls: Vec<_> = block
            .graph
            .scalars
            .iter()
            .filter(|scalar| matches!(scalar.operation, Operation::Call { .. }))
            .collect();
        assert_eq!(calls.len(), 1);
        let Operation::Call { helper, args } = &calls[0].operation else {
            unreachable!();
        };
        assert_eq!(*helper, HelperRef(&SUM));
        assert_eq!(args.len(), 2);
        let Event::Call { args, results, .. } = &block.events[0] else {
            panic!("the ordered call is the first event");
        };
        assert!(matches!(
            block.graph.scalars[args[0]].operation,
            Operation::Input(1)
        ));
        assert!(matches!(
            block.graph.scalars[results[0]].operation,
            Operation::Effect(0)
        ));
        // The stage of the call computes nothing: its argument is an input.
        let stages = crate::native::schedule::schedule(&block);
        assert_eq!(stages.len(), 2);
        assert!(!stages[0].contains(&results[0]));
        // Both functions run through their pointers.
        let crate::native::HelperFn::Pure(function) = SUM.function else {
            unreachable!();
        };
        assert_eq!(function(&[2, 3]), 5);
        let crate::native::HelperFn::Ordered(function) = POKE.function else {
            unreachable!();
        };
        let (mut count, mut results) = (0u32, [0u128]);
        function(&mut count, &[41], &mut results);
        assert_eq!((count, results[0]), (1, 42));
    }
}
