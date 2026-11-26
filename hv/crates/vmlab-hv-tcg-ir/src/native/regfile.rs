//! A register file as numbered slots of fixed type (R11 of the phase 3d
//! design).
//!
//! A lifter that owns its guest state, and does not interpret a Sail model,
//! gives each register a slot. A slot enters a block as an `Input` of the
//! graph with the number of the slot, and an exit names the slots that it
//! writes back and the scalar for each. The Sail recorder keeps its globals
//! by name and does not use this module.
use super::block::ExitScalars;
use super::{Graph, InputId, ScalarId, ScalarType};

/// One slot of a register file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub name: &'static str,
    pub ty: ScalarType,
}

/// The layout of a register file: slot `n` is `slots[n]`.
#[derive(Clone, Copy, Debug)]
pub struct RegisterFile {
    pub slots: &'static [Slot],
}

impl RegisterFile {
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The scalar that holds the value of `slot` at the entry of the block.
    pub fn read(&self, graph: &mut Graph, slot: InputId) -> ScalarId {
        graph.input(slot, self.slots[slot].ty, None)
    }
}

/// The slots that an exit writes, each with its scalar. A slot that is not
/// named keeps the value that it had at the entry of the block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Writeback {
    pub writes: Vec<(InputId, ScalarId)>,
}

impl ExitScalars for Writeback {
    fn for_each_scalar(&mut self, visit: &mut dyn FnMut(&mut ScalarId)) {
        for (_, scalar) in &mut self.writes {
            visit(scalar);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::block::{BlockOf, Event};
    use super::super::{Binary, Operation, dce};
    use super::*;

    const FILE: RegisterFile = RegisterFile {
        slots: &[
            Slot {
                name: "a",
                ty: ScalarType::Bits(64),
            },
            Slot {
                name: "flag",
                ty: ScalarType::Bits(1),
            },
        ],
    };

    #[test]
    fn a_slot_written_twice_keeps_only_the_last_value_after_dce() {
        let mut block: BlockOf<Writeback> = BlockOf::default();
        let a = FILE.read(&mut block.graph, 0);
        assert_eq!(FILE.read(&mut block.graph, 0), a);
        assert_eq!(block.graph.scalars[a].ty, ScalarType::Bits(64));
        let ty = ScalarType::Bits(64);
        let one = block.graph.constant(ty, 1);
        let two = block.graph.constant(ty, 2);
        // The first write of the flag is dead: the exit names the second.
        let first = block.graph.binary(ty, Binary::Add, a, one).unwrap();
        let dead_flag = block.graph.slice(first, 63, 1);
        let second = block.graph.binary(ty, Binary::Xor, a, two).unwrap();
        let flag = block.graph.slice(second, 63, 1);
        let exit = block.exit(Writeback {
            writes: vec![(0, second), (1, flag)],
        });
        block.events.push(Event::Exit(exit));
        let before = block.graph.scalars.len();
        dce::prune_scalars(&mut block);
        assert!(block.graph.scalars.len() < before);
        assert!(
            !block
                .graph
                .scalars
                .iter()
                .any(|scalar| matches!(scalar.operation, Operation::Binary(Binary::Add, _, _)))
        );
        let _ = dead_flag;
        let writes = &block.exits[0].writes;
        assert!(matches!(
            block.graph.scalars[writes[0].1].operation,
            Operation::Binary(Binary::Xor, _, _)
        ));
        assert!(matches!(
            block.graph.scalars[writes[1].1].operation,
            Operation::Slice { offset: 63, .. }
        ));
    }
}
