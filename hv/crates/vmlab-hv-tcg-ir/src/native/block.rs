use super::{Graph, InputId, ScalarId, ScalarType};
use crate::{Function, Location, NativeOperation};
use std::sync::Arc;

pub type ValueId = usize;
pub type ConstantId = usize;
pub type ExitId = usize;

/// Runtime constants are held by the frontend; native code needs only IDs.
#[derive(Clone, Debug)]
pub enum Recipe {
    Constant(ConstantId),
    Scalar {
        value: ScalarId,
        ty: ScalarType,
    },
    Input(InputId),
    Struct(Vec<(String, ValueId)>),
    Union(String, ValueId),
    Vector(Vec<ValueId>),
    Deferred {
        operation: DeferredOperation,
        args: Vec<ValueId>,
        ty: crate::Type,
    },
}

#[derive(Clone, Debug)]
pub enum DeferredOperation {
    Intrinsic {
        name: String,
        width: Option<u32>,
    },
    Native {
        operation: NativeOperation,
        width: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    pub name: String,
    pub value: ValueId,
}

#[derive(Clone, Debug)]
pub struct SourceFrame {
    pub function: Arc<Function>,
    pub locals: Arc<Vec<Binding>>,
    /// Dead values are omitted, but their binding scope must survive a resume.
    pub local_names: Arc<Vec<String>>,
    pub declared: Vec<u64>,
    /// Next source operation, not the beginning of the guest instruction.
    pub pc: usize,
    pub pending_result: Option<Location>,
    pub call_site: usize,
}

#[derive(Clone, Debug)]
pub struct Exit {
    /// Root-to-leaf order, matching the named interpreter's stack.
    pub frames: Vec<SourceFrame>,
    pub globals: Arc<Vec<Binding>>,
    pub completed_steps: u32,
    /// A child call/effect has completed but its caller has not assigned it.
    pub returned: Option<ValueId>,
    pub fetch_address: Option<ValueId>,
}

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub globals: Arc<Vec<Binding>>,
    pub waiting: ValueId,
    pub completed_steps: u32,
    pub fetch_address: Option<ValueId>,
}

#[derive(Clone, Debug)]
pub struct CodeGuard {
    pub address: u64,
    pub len: usize,
    pub generation: u64,
    pub identity: u64,
    pub mapping: u64,
    pub first_page: u64,
    pub last_page: u64,
}

#[derive(Clone, Debug)]
pub struct ContextGuard {
    pub name: String,
    pub expected: ConstantId,
    pub version: u64,
}

#[derive(Clone, Debug)]
pub enum Event {
    Guard {
        condition: ScalarId,
        expected: bool,
        exit: ExitId,
    },
    ReadMemory {
        address: ScalarId,
        bytes: u8,
        exclusive: bool,
        result: ScalarId,
        error: ExitId,
    },
    WriteMemory {
        address: ScalarId,
        value: ScalarId,
        bytes: u8,
        exclusive: bool,
        result: Option<ScalarId>,
        error: ExitId,
        invalidated: ExitId,
    },
    /// An ordered helper call (R10): it runs at its place among the events,
    /// once. `results` are `Effect` scalars that the call fills. A pure
    /// helper is not an event: it is the scalar `Operation::Call`.
    Call {
        helper: super::HelperRef,
        args: Vec<ScalarId>,
        results: Vec<ScalarId>,
    },
    Checkpoint(usize),
    Exit(ExitId),
}

/// A block whose exits carry the payload `X`: what the frontend needs to
/// resume after the block (R8 of the phase 3d design). The Sail recorder
/// resumes named frames (`Exit`). A lifter with a register file resumes from
/// slots that the exit writes back (`regfile::Writeback`).
#[derive(Clone, Debug)]
pub struct BlockOf<X> {
    pub graph: Graph,
    pub values: Vec<Recipe>,
    pub events: Vec<Event>,
    pub exits: Vec<X>,
    pub checkpoints: Vec<Checkpoint>,
    pub code_guards: Vec<CodeGuard>,
    pub context_guards: Vec<ContextGuard>,
}

/// The block of the Sail recorder.
pub type Block = BlockOf<Exit>;

impl<X> Default for BlockOf<X> {
    fn default() -> Self {
        Self {
            graph: Graph::default(),
            values: Vec::new(),
            events: Vec::new(),
            exits: Vec::new(),
            checkpoints: Vec::new(),
            code_guards: Vec::new(),
            context_guards: Vec::new(),
        }
    }
}

/// An exit payload that names scalars of the graph directly. Dead code
/// elimination keeps those scalars and renumbers them through this trait.
pub trait ExitScalars {
    fn for_each_scalar(&mut self, visit: &mut dyn FnMut(&mut ScalarId));
}

impl<X> BlockOf<X> {
    /// Empty the block, keeping the memory of its lists and of its graph,
    /// for the next block of a translator (3e DESIGN.md J11).
    pub fn clear(&mut self) {
        self.graph.clear();
        self.values.clear();
        self.events.clear();
        self.exits.clear();
        self.checkpoints.clear();
        self.code_guards.clear();
        self.context_guards.clear();
    }

    pub fn value(&mut self, recipe: Recipe) -> ValueId {
        let id = self.values.len();
        self.values.push(recipe);
        id
    }

    pub fn exit(&mut self, snapshot: X) -> ExitId {
        let id = self.exits.len();
        self.exits.push(snapshot);
        id
    }

    pub fn checkpoint(&mut self, checkpoint: Checkpoint) {
        let id = self.checkpoints.len();
        self.checkpoints.push(checkpoint);
        self.events.push(Event::Checkpoint(id));
    }
}
