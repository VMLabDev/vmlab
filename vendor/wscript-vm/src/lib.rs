//! The wscript virtual machine: a register-based bytecode interpreter
//! (PRD §5.2).
//!
//! One `Vm` per thread — values are `Rc`-managed and `!Send` (PRD §4.3).
//! VM-level faults (out-of-bounds indexing, division by zero, borrow
//! conflicts at the host boundary) are trappable [`RuntimeError`]s returned
//! to the host; script code never observes a panic (PRD §3.5).
//!
//! Bytecode is trusted: instruction operands (proto/register/capture/
//! vtable/field indices) are the wscript compiler's own output and are
//! not re-validated per instruction — the dispatch loop indexes with
//! them directly. Everything that crosses the *script* boundary (values,
//! arithmetic, collection indices, recursion depth) is checked and
//! faults; running hand-crafted or corrupted `CompiledUnit`s is outside
//! the threat model and may panic.

mod builtins;
mod mem;
mod ops;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use wscript_core::bytecode::{CallTarget, CompiledUnit, FaultCode, Instr};
use wscript_core::host::{HostCallable, HostCtx, HostError};
use wscript_core::registry::Registry;
use wscript_core::value::{Closure, DynObj, Key, Value};

/// The fault types live in `wscript-core` so `HostError` can carry a
/// script fault across the host boundary (script callbacks); the VM's
/// historical names are aliases.
pub use wscript_core::fault::TraceFrame;
/// A trappable runtime fault (alias of [`wscript_core::ScriptFault`]).
pub type RuntimeError = wscript_core::fault::ScriptFault;

// ---------------------------------------------------------------- output
//
// `print`/`println` write to stdout by default, but an embedder may need
// to capture script output (a TUI host whose stdout is a live screen, or
// a tool whose stdout carries machine-readable output). The sink belongs
// to the `Vm`: it is already one-per-thread and `!Send`, so a thread-local
// bought nothing a plain field does not, and leaked between VMs and across
// tests on the same thread.

/// Receives printed text and `true` for `println` (trailing newline).
pub type PrintHook = Box<dyn FnMut(&str, bool)>;

/// Asked after every host function returns whether the tank is to be filled
/// again; see [`VmConfig::refuel`].
pub type RefuelHook = Box<dyn FnMut() -> bool>;

/// The default sink: the process's stdout.
pub fn stdout_sink() -> PrintHook {
    Box::new(|s: &str, newline: bool| {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        if newline {
            let _ = out.write_all(b"\n");
        }
        let _ = out.flush();
    })
}

/// Everything a [`Vm`] is configured with, supplied at construction.
///
/// The limits previously had three different shapes — a setter pair, a
/// setter pair over `Option`, and a private constant — and output was a
/// thread-local installed by a free function. Collecting them here means a
/// `Vm` is fully described by the value it was built from.
pub struct VmConfig {
    /// Execution budget in dispatched instructions; see [`Vm::set_fuel`].
    /// `None` (the default) is unmetered.
    pub fuel: Option<u64>,
    /// Asked after every host function returns (not after a builtin). On
    /// `true` the tank goes back to the amount it was last set to, so the
    /// fuel bounds the instructions between two host calls and not those of
    /// the whole run. For an embedder whose host calls are where a run can
    /// be stopped from outside: there the fuel only has to end a script
    /// that never reaches one. `None` (the default): the tank is never
    /// filled again. (vmlab delta, card #1086.)
    pub refuel: Option<RefuelHook>,
    /// Limit, in bytes, on the values a script holds at one time; see the
    /// `mem` module. Over it a run faults with "memory limit of N bytes
    /// exceeded". The count is an estimate and is taken at intervals, so a
    /// run can hold up to about half as much again before it is stopped.
    /// `None` (the default): no limit. (vmlab delta, card #1086.)
    pub memory: Option<u64>,
    /// Script call-depth limit; see [`Vm::set_call_depth_limit`].
    pub call_depth: usize,
    /// Maximum concurrently-nested host→script re-entries. Each level holds
    /// a full nested dispatch-loop native frame (tens of KiB in debug
    /// builds), so this is sized to fit a 2 MiB thread stack with a wide
    /// margin — deeper ping-pong than this is pathological.
    pub reentry_depth: usize,
    /// Where `print`/`println` deliver their text.
    pub out: PrintHook,
}

impl Default for VmConfig {
    fn default() -> VmConfig {
        VmConfig {
            fuel: None,
            refuel: None,
            memory: None,
            call_depth: DEFAULT_CALL_DEPTH_LIMIT,
            reentry_depth: REENTRY_DEPTH_LIMIT,
            out: stdout_sink(),
        }
    }
}

impl std::fmt::Debug for VmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmConfig")
            .field("fuel", &self.fuel)
            .field("refuel", &self.refuel.as_ref().map(|_| "<hook>"))
            .field("memory", &self.memory)
            .field("call_depth", &self.call_depth)
            .field("reentry_depth", &self.reentry_depth)
            .field("out", &"<sink>")
            .finish()
    }
}

struct UnitState {
    unit: CompiledUnit,
    consts: Vec<Value>,
}

struct Frame {
    proto: u32,
    /// Absolute stack index of register 0.
    base: usize,
    pc: usize,
    /// Absolute stack slot receiving the return value (`usize::MAX` for
    /// entry frames).
    ret_slot: usize,
    closure: Option<Rc<Closure>>,
}

/// Where a new frame's arguments come from. The two sources are the only
/// difference between a call made by the host and one made by a call
/// instruction, so [`Vm::push_frame`] takes this rather than existing
/// twice — and the depth limit is then checked in exactly one place.
enum Args<'a> {
    /// Values the host already holds: an entry call, or a closure invoked
    /// from a builtin.
    Values(&'a [Value]),
    /// `n` values sitting on the register stack from absolute index `at`,
    /// where a call instruction's compiler-assigned argument window put
    /// them.
    Stack { at: usize, n: usize },
}

impl Args<'_> {
    fn len(&self) -> usize {
        match *self {
            Args::Values(vs) => vs.len(),
            Args::Stack { n, .. } => n,
        }
    }
}

/// A wscript virtual machine. Not `Send`: script values are `Rc`-managed
/// (PRD §4.3) — spin one `Vm` per thread from a shared `Context`.
pub struct Vm {
    host_fns: Vec<Arc<dyn HostCallable>>,
    units: Vec<UnitState>,
    unit_ids: HashMap<u64, usize>,
    cur_unit: usize,
    stack: Vec<Value>,
    frames: Vec<Frame>,
    /// Recursion guard; see [`Vm::set_call_depth_limit`].
    depth_limit: usize,
    /// Execution budget; see [`Vm::set_fuel`]. `None` means unmetered.
    fuel: Option<u64>,
    /// The amount the tank was last set to: what a refuel fills it to.
    fuel_full: Option<u64>,
    /// See [`VmConfig::refuel`].
    refuel: Option<RefuelHook>,
    /// See [`VmConfig::memory`].
    mem: Option<mem::Memory>,
    /// Live host→script re-entries (`HostCtx::call_value`). Each one adds
    /// *native* stack (a nested dispatch loop plus the host closure), so
    /// it gets its own, much smaller limit than script frame depth.
    reentry_depth: usize,
    /// Ceiling for `reentry_depth`; see [`VmConfig::reentry_depth`].
    reentry_limit: usize,
    /// Where `print`/`println` deliver their text.
    out: PrintHook,
    /// Defs visible before any unit is loaded (builtins + host
    /// registrations), so structural ops work on a freshly-built `Vm`.
    base_defs: wscript_core::defs::DefTable,
    /// Returned by [`Vm::unit_impls`] when no unit is loaded.
    no_impls: wscript_core::bytecode::ImplMaps,
}

/// Maximum concurrently-nested host→script re-entries; see
/// [`Vm::reentry_depth`]. Each level holds a full nested dispatch-loop
/// native frame (tens of KiB in debug builds), so this is sized to fit
/// a 2 MiB thread stack with a wide margin — host↔script ping-pong
/// deeper than this is pathological.
pub const REENTRY_DEPTH_LIMIT: usize = 32;

/// What a call into native code is counted as against the memory limit,
/// beside the value it returns: the element or entry a builtin may have
/// added to its receiver.
const CALL_BYTES: u64 = 64;

/// Default script call-depth limit (frames, not bytes — script frames
/// live on the heap-allocated register stack).
pub const DEFAULT_CALL_DEPTH_LIMIT: usize = 10_000;

impl Vm {
    /// Build a VM against `registry`, configured by `config`.
    ///
    /// The result is usable immediately: structural operations fall back
    /// to the registry's defs until a unit is loaded, so nothing panics on
    /// a freshly-built `Vm`.
    pub fn new(registry: &Registry, config: VmConfig) -> Vm {
        Vm {
            host_fns: registry.host_fns.iter().map(|e| e.imp.clone()).collect(),
            units: Vec::new(),
            unit_ids: HashMap::new(),
            cur_unit: 0,
            stack: Vec::new(),
            frames: Vec::new(),
            depth_limit: config.call_depth,
            fuel: config.fuel,
            fuel_full: config.fuel,
            refuel: config.refuel,
            mem: config.memory.map(mem::Memory::new),
            reentry_depth: 0,
            reentry_limit: config.reentry_depth,
            out: config.out,
            base_defs: registry.defs.clone(),
            no_impls: wscript_core::bytecode::ImplMaps::default(),
        }
    }

    /// Deliver script output to the configured sink.
    pub(crate) fn print_text(&mut self, s: &str, newline: bool) {
        (self.out)(s, newline);
    }

    /// Set the script call-depth limit (default
    /// [`DEFAULT_CALL_DEPTH_LIMIT`]). Exceeding it faults with a
    /// trappable "stack overflow" [`RuntimeError`] rather than growing
    /// without bound. A limit of 0 makes every call fault.
    pub fn set_call_depth_limit(&mut self, limit: usize) {
        self.depth_limit = limit;
    }

    /// The current script call-depth limit.
    pub fn call_depth_limit(&self) -> usize {
        self.depth_limit
    }

    /// Set the fuel tank. `Some(n)`: subsequent execution may dispatch at
    /// most `n` instructions (1 instruction = 1 fuel, uniformly; a host
    /// call costs 1 for the dispatch, what the host function does
    /// internally is not metered) before faulting with a trappable
    /// "fuel exhausted" [`RuntimeError`]. `None` (the default): execution
    /// is unmetered. The tank belongs to the `Vm` and depletes across
    /// calls until set again. Accounting is exact but charged at
    /// control-transfer points (jumps, calls, returns), so exhaustion —
    /// including the `Some(0)` edge — surfaces at the end of the current
    /// straight-line run of instructions: never past a host call or a
    /// loop iteration.
    ///
    /// Structural builtins (`==`/`<` on containers, `.clone()`,
    /// `print`/`str` of containers) additionally charge ~1 fuel per value
    /// visited, so comparing or rendering a huge structure cannot run
    /// unmetered inside a single dispatched instruction.
    pub fn set_fuel(&mut self, fuel: Option<u64>) {
        self.fuel = fuel;
        self.fuel_full = fuel;
    }

    /// Remaining fuel, or `None` if unmetered. Exact after a successful
    /// call and after exhaustion (0); a call that ends in some *other*
    /// fault leaves its final straight-line run uncharged.
    pub fn fuel(&self) -> Option<u64> {
        self.fuel
    }

    /// Load (or find the cached copy of) a compiled unit.
    pub fn load(&mut self, unit: &CompiledUnit) -> usize {
        if let Some(&idx) = self.unit_ids.get(&unit.id) {
            return idx;
        }
        let consts = unit.consts.iter().map(Value::from_const).collect();
        let idx = self.units.len();
        self.units.push(UnitState {
            unit: unit.clone(),
            consts,
        });
        self.unit_ids.insert(unit.id, idx);
        idx
    }

    /// Call a function by proto index with already-converted values. The
    /// typed wrappers live in the `wscript` umbrella crate.
    pub fn call_proto(
        &mut self,
        unit: &CompiledUnit,
        proto: u32,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let unit_idx = self.load(unit);
        self.cur_unit = unit_idx;
        let entry_depth = self.frames.len();
        self.push_frame(proto, usize::MAX, None, Args::Values(&args))?;
        let result = self.execute(entry_depth);
        if result.is_err() {
            // Unwind frames left behind by the fault.
            while self.frames.len() > entry_depth {
                let f = self.frames.pop().unwrap();
                self.stack.truncate(f.base);
            }
        }
        result
    }

    /// Call a function by exported name (signature checking happens in the
    /// `wscript` umbrella crate at the typed boundary).
    pub fn call_name(
        &mut self,
        unit: &CompiledUnit,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let Some(&(proto, _)) = unit.exports.get(name) else {
            let message = if unit.generic_fns.iter().any(|f| f == name) {
                format!(
                    "`{name}` is generic; the host cannot call generic script functions \
                     — wrap it in a monomorphic script fn"
                )
            } else {
                format!("no function named `{name}` in the compiled script")
            };
            return Err(RuntimeError {
                message,
                span: None,
                trace: vec![],
                exit_code: None,
            });
        };
        self.call_proto(unit, proto, args)
    }

    fn stack_top(&self) -> usize {
        match self.frames.last() {
            Some(f) => {
                let n_regs = self.units[self.cur_unit].unit.protos[f.proto as usize].n_regs;
                f.base + n_regs as usize
            }
            None => 0,
        }
    }

    /// Push a call frame for `proto` on top of the register stack, with
    /// `args` copied into its leading registers. The one place a script
    /// frame comes into existence, so the one place the recursion guard
    /// has to be checked.
    fn push_frame(
        &mut self,
        proto: u32,
        ret_slot: usize,
        closure: Option<Rc<Closure>>,
        args: Args<'_>,
    ) -> Result<(), RuntimeError> {
        if self.frames.len() >= self.depth_limit {
            return Err(self.fault("stack overflow: too many nested calls"));
        }
        let base = self.stack_top();
        let n_regs = self.units[self.cur_unit].unit.protos[proto as usize].n_regs as usize;
        // A proto always has registers for its own parameters, so the max
        // only bites if a caller passes more arguments than the callee
        // declares — impossible from checked bytecode and rejected at the
        // typed host boundary, but it costs one comparison to write the
        // args somewhere addressable rather than off the end of the stack.
        let needed = base + n_regs.max(args.len());
        if self.stack.len() < needed {
            self.stack.resize(needed, Value::Unit);
        }
        match args {
            Args::Values(vs) => {
                for (i, a) in vs.iter().enumerate() {
                    self.stack[base + i] = a.clone();
                }
            }
            Args::Stack { at, n } => {
                for i in 0..n {
                    let v = self.stack[at + i].clone();
                    self.stack[base + i] = v;
                }
            }
        }
        self.frames.push(Frame {
            proto,
            base,
            pc: 0,
            ret_slot,
            closure,
        });
        Ok(())
    }

    /// Call a function value from inside a builtin (map/filter/custom
    /// impls): runs a nested dispatch loop until the frame returns.
    pub(crate) fn call_function(
        &mut self,
        f: &Value,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        match f {
            Value::Closure(c) => {
                let entry_depth = self.frames.len();
                self.push_frame(c.proto, usize::MAX, Some(c.clone()), Args::Values(&args))?;
                self.execute(entry_depth)
            }
            other => Err(self.fault(format!("cannot call a {} value", other.kind_name()))),
        }
    }

    /// Host→script re-entry (`HostCtx::call_value`): run a script
    /// function value from inside a running host function. Unlike
    /// `call_function`, frames are unwound on `Err` — the host may catch
    /// the error and continue, so no stale frames can be left behind.
    fn reenter(&mut self, f: &Value, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if self.reentry_depth >= self.reentry_limit {
            return Err(
                self.fault("host re-entry too deep (host function and script calling each other?)")
            );
        }
        self.reentry_depth += 1;
        let entry_depth = self.frames.len();
        let result = self.call_function(f, args);
        if result.is_err() {
            while self.frames.len() > entry_depth {
                let fr = self.frames.pop().unwrap();
                self.stack.truncate(fr.base);
            }
        }
        self.reentry_depth -= 1;
        // The host function holds the result where the register stack does
        // not show it.
        if let Ok(v) = &result {
            self.mem_hold(v)?;
        }
        result
    }

    pub(crate) fn call_proto_nested(
        &mut self,
        proto: u32,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let entry_depth = self.frames.len();
        self.push_frame(proto, usize::MAX, None, Args::Values(&args))?;
        self.execute(entry_depth)
    }

    /// Charge one fuel for a structural-op node visit (eq/cmp/clone/
    /// display). These builtins walk arbitrarily large value graphs in a
    /// single dispatched instruction, so without this a huge (or DAG-
    /// shaped) comparison would run unmetered and unkillable. Draws from
    /// `self.fuel` directly — sound because a builtin is only ever reached
    /// through [`Transfer::transfer_native`], which lends `self.fuel` the
    /// dispatch loop's tank for the duration of the call.
    pub(crate) fn charge_structural(&mut self) -> Result<(), RuntimeError> {
        match self.fuel {
            Some(0) => Err(self.fault("fuel exhausted")),
            Some(n) => {
                self.fuel = Some(n - 1);
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub(crate) fn fault(&self, message: impl Into<String>) -> RuntimeError {
        // NOTE: all frames are resolved against `self.cur_unit`. v1 only ever
        // executes within a single unit (`cur_unit` is set once on entry and
        // closures carry a proto but no unit), so this is correct. A future
        // cross-unit call chain would need a per-frame unit index on `Frame`.
        let trace: Vec<TraceFrame> = self
            .frames
            .iter()
            .rev()
            .map(|f| {
                let proto = &self.units[self.cur_unit].unit.protos[f.proto as usize];
                TraceFrame {
                    function: proto.name.clone(),
                    span: proto.spans.get(f.pc.saturating_sub(1)).copied(),
                }
            })
            .collect();
        let span = trace.first().and_then(|t| t.span);
        RuntimeError {
            message: message.into(),
            span,
            trace,
            exit_code: None,
        }
    }

    /// Defs of the current unit, falling back to the registry's when none
    /// is loaded. Total, so structural ops are safe on a fresh `Vm`.
    pub(crate) fn unit_defs(&self) -> &wscript_core::defs::DefTable {
        match self.units.get(self.cur_unit) {
            Some(u) => &u.unit.defs,
            None => &self.base_defs,
        }
    }

    /// Custom operator impls of the current unit; empty when none is
    /// loaded (a unit-less VM has no script `impl`s by construction).
    pub(crate) fn unit_impls(&self) -> &wscript_core::bytecode::ImplMaps {
        match self.units.get(self.cur_unit) {
            Some(u) => &u.unit.impls,
            None => &self.no_impls,
        }
    }

    // --------------------------------------------------------- dispatch

    fn execute(&mut self, entry_depth: usize) -> Result<Value, RuntimeError> {
        // Monomorphized dispatch: the unmetered loop carries no fuel
        // bookkeeping at all, and the metered loop charges in
        // straight-line blocks rather than per instruction (the benchmark
        // gate for this feature was ≤2% unmetered / ≤5% metered; a
        // per-instruction check blew both). Both are [`Transfer`]'s
        // doing — see there for the accounting; the tank it carries is a
        // local the optimizer can hold in a register, so it is written
        // back to `self.fuel` here.
        match self.fuel {
            Some(tank) => {
                let mut transfer = Transfer::<true>::new(self, tank);
                let result = self.execute_impl(entry_depth, &mut transfer);
                self.fuel = Some(transfer.fuel);
                result
            }
            None => {
                let mut transfer = Transfer::<false>::new(self, 0);
                self.execute_impl(entry_depth, &mut transfer)
            }
        }
    }

    fn execute_impl<const METERED: bool>(
        &mut self,
        entry_depth: usize,
        transfer: &mut Transfer<METERED>,
    ) -> Result<Value, RuntimeError> {
        macro_rules! reg {
            ($base:expr, $r:expr) => {
                self.stack[$base + $r as usize]
            };
        }
        macro_rules! prim {
            ($base:expr, $r:expr, $variant:ident, $what:literal) => {
                match &reg!($base, $r) {
                    Value::$variant(x) => *x,
                    other => {
                        return Err(self.fault(format!(
                            concat!("type confusion: expected ", $what, ", found {}"),
                            other.kind_name()
                        )));
                    }
                }
            };
        }
        macro_rules! int {
            ($b:expr, $r:expr) => {
                prim!($b, $r, Int, "int")
            };
        }
        macro_rules! float {
            ($b:expr, $r:expr) => {
                prim!($b, $r, Float, "float")
            };
        }
        macro_rules! boolean {
            ($b:expr, $r:expr) => {
                prim!($b, $r, Bool, "bool")
            };
        }
        macro_rules! ch {
            ($b:expr, $r:expr) => {
                prim!($b, $r, Char, "char")
            };
        }
        macro_rules! string {
            ($base:expr, $r:expr) => {
                match &reg!($base, $r) {
                    Value::Str(s) => s.clone(),
                    other => {
                        return Err(self.fault(format!(
                            "type confusion: expected string, found {}",
                            other.kind_name()
                        )));
                    }
                }
            };
        }

        // No arm below charges fuel or restarts a block: every mutation
        // of `pc`, and every escape into native code that could spend
        // fuel of its own, goes through a `transfer.*` call, which does
        // both. See [`Transfer`].
        loop {
            let (instr, base) = {
                let frame = self.frames.last().unwrap();
                let proto = &self.units[self.cur_unit].unit.protos[frame.proto as usize];
                if frame.pc >= proto.code.len() {
                    (Instr::RetUnit, frame.base)
                } else {
                    (proto.code[frame.pc], frame.base)
                }
            };
            transfer.advance(self);

            match instr {
                Instr::Nop => {}
                Instr::LoadConst { dst, k } => {
                    reg!(base, dst) = self.units[self.cur_unit].consts[k as usize].clone();
                }
                Instr::LoadUnit { dst } => reg!(base, dst) = Value::Unit,
                Instr::LoadBool { dst, v } => reg!(base, dst) = Value::Bool(v),
                Instr::LoadInt { dst, v } => reg!(base, dst) = Value::Int(v as i64),
                Instr::Move { dst, src } => {
                    reg!(base, dst) = reg!(base, src).clone();
                }

                Instr::AddI { dst, a, b } => {
                    reg!(base, dst) = Value::Int(int!(base, a).wrapping_add(int!(base, b)));
                }
                Instr::SubI { dst, a, b } => {
                    reg!(base, dst) = Value::Int(int!(base, a).wrapping_sub(int!(base, b)));
                }
                Instr::MulI { dst, a, b } => {
                    reg!(base, dst) = Value::Int(int!(base, a).wrapping_mul(int!(base, b)));
                }
                Instr::DivI { dst, a, b } => {
                    let d = int!(base, b);
                    if d == 0 {
                        return Err(self.fault("division by zero"));
                    }
                    reg!(base, dst) = Value::Int(int!(base, a).wrapping_div(d));
                }
                Instr::RemI { dst, a, b } => {
                    let d = int!(base, b);
                    if d == 0 {
                        return Err(self.fault("remainder by zero"));
                    }
                    reg!(base, dst) = Value::Int(int!(base, a).wrapping_rem(d));
                }
                Instr::NegI { dst, src } => {
                    reg!(base, dst) = Value::Int(int!(base, src).wrapping_neg());
                }

                Instr::AddF { dst, a, b } => {
                    reg!(base, dst) = Value::Float(float!(base, a) + float!(base, b));
                }
                Instr::SubF { dst, a, b } => {
                    reg!(base, dst) = Value::Float(float!(base, a) - float!(base, b));
                }
                Instr::MulF { dst, a, b } => {
                    reg!(base, dst) = Value::Float(float!(base, a) * float!(base, b));
                }
                Instr::DivF { dst, a, b } => {
                    reg!(base, dst) = Value::Float(float!(base, a) / float!(base, b));
                }
                Instr::RemF { dst, a, b } => {
                    reg!(base, dst) = Value::Float(float!(base, a) % float!(base, b));
                }
                Instr::NegF { dst, src } => {
                    reg!(base, dst) = Value::Float(-float!(base, src));
                }

                Instr::ConcatStr { dst, a, b } => {
                    let a = string!(base, a);
                    let b = string!(base, b);
                    // The one instruction that can ask for any number of
                    // bytes: it has to fit before it is allocated.
                    self.mem_reserve((a.len() + b.len()) as u64)?;
                    let mut s = String::with_capacity(a.len() + b.len());
                    s.push_str(&a);
                    s.push_str(&b);
                    reg!(base, dst) = Value::Str(Rc::from(s.as_str()));
                }

                Instr::Not { dst, src } => {
                    reg!(base, dst) = Value::Bool(!boolean!(base, src));
                }

                Instr::EqI { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(int!(base, a) == int!(base, b));
                }
                Instr::EqF { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(float!(base, a) == float!(base, b));
                }
                Instr::EqBool { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(boolean!(base, a) == boolean!(base, b));
                }
                Instr::EqChar { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(ch!(base, a) == ch!(base, b));
                }
                Instr::EqStr { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(string!(base, a) == string!(base, b));
                }
                Instr::LtI { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(int!(base, a) < int!(base, b));
                }
                Instr::LeI { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(int!(base, a) <= int!(base, b));
                }
                Instr::LtF { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(float!(base, a) < float!(base, b));
                }
                Instr::LeF { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(float!(base, a) <= float!(base, b));
                }
                Instr::LtChar { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(ch!(base, a) < ch!(base, b));
                }
                Instr::LeChar { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(ch!(base, a) <= ch!(base, b));
                }
                Instr::LtStr { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(string!(base, a) < string!(base, b));
                }
                Instr::LeStr { dst, a, b } => {
                    reg!(base, dst) = Value::Bool(string!(base, a) <= string!(base, b));
                }

                Instr::Jump { off } => transfer.transfer_to(self, off)?,
                Instr::JumpIfFalse { cond, off } => {
                    if !boolean!(base, cond) {
                        transfer.transfer_to(self, off)?;
                    }
                }
                Instr::JumpIfTrue { cond, off } => {
                    if boolean!(base, cond) {
                        transfer.transfer_to(self, off)?;
                    }
                }

                Instr::Call {
                    dst,
                    base: abase,
                    nargs,
                    target,
                } => {
                    let args_at = base + abase as usize;
                    match target {
                        CallTarget::Proto(p) => {
                            let ret_slot = base + dst as usize;
                            let args = Args::Stack {
                                at: args_at,
                                n: nargs as usize,
                            };
                            transfer
                                .transfer_into(self, |vm| vm.push_frame(p, ret_slot, None, args))?;
                        }
                        CallTarget::Host(h) => {
                            let host_args: Vec<Value> = (0..nargs as usize)
                                .map(|i| self.stack[args_at + i].clone())
                                .collect();
                            let imp = self.host_fns[h as usize].clone();
                            // A host function may re-enter the VM through
                            // HostCtx::call_value (script callbacks), so it
                            // runs with the tank lent to it.
                            let result = transfer.transfer_native(self, |vm| {
                                let mut ctx = VmHostCtx { vm };
                                imp.call(&mut ctx, host_args)
                            })?;
                            transfer.refuel(self);
                            match result {
                                Ok(v) => {
                                    let fresh = self.mem_fresh(&v);
                                    reg!(base, dst) = v;
                                    self.mem_note(CALL_BYTES + fresh)?;
                                }
                                Err(e) => return Err(self.host_fault(e)),
                            }
                        }
                        CallTarget::Builtin(b) => {
                            // Builtins re-enter a nested dispatch loop
                            // (map/filter/custom impls) and charge for the
                            // value graphs they walk, so they too run with
                            // the tank lent to them.
                            let r = transfer
                                .transfer_native(self, |vm| vm.call_builtin(b, args_at, nargs))?;
                            let r = r?;
                            // What the builtin made, and the element or
                            // entry it may have added to its receiver.
                            let fresh = self.mem_fresh(&r);
                            reg!(base, dst) = r;
                            self.mem_note(CALL_BYTES + fresh)?;
                        }
                    }
                }
                Instr::CallValue {
                    dst,
                    f,
                    base: abase,
                    nargs,
                } => {
                    let callee = reg!(base, f).clone();
                    let ret_slot = base + dst as usize;
                    let args = Args::Stack {
                        at: base + abase as usize,
                        n: nargs as usize,
                    };
                    transfer.transfer_into(self, |vm| match callee {
                        Value::Closure(c) => vm.push_frame(c.proto, ret_slot, Some(c), args),
                        other => {
                            Err(vm.fault(format!("cannot call a {} value", other.kind_name())))
                        }
                    })?;
                }
                Instr::CallVirtual {
                    dst,
                    base: abase,
                    nargs,
                    slot,
                } => {
                    let args_at = base + abase as usize;
                    let ret_slot = base + dst as usize;
                    transfer.transfer_into(self, |vm| {
                        let recv = vm.stack[args_at].clone();
                        let Value::Dyn(d) = recv else {
                            return Err(vm.fault(format!(
                                "type confusion: dynamic dispatch on a {} value",
                                recv.kind_name()
                            )));
                        };
                        let target = vm.units[vm.cur_unit].unit.vtables[d.vtable as usize].targets
                            [slot as usize];
                        let CallTarget::Proto(p) = target else {
                            return Err(vm.fault("invalid vtable entry"));
                        };
                        // Unwrap the receiver for the concrete method.
                        vm.stack[args_at] = d.inner.clone();
                        vm.push_frame(
                            p,
                            ret_slot,
                            None,
                            Args::Stack {
                                at: args_at,
                                n: nargs as usize,
                            },
                        )
                    })?;
                }
                Instr::Ret { src } => {
                    let v = reg!(base, src).clone();
                    if let Some(v) = transfer.transfer_out(self, entry_depth, v)? {
                        return Ok(v);
                    }
                }
                Instr::RetUnit => {
                    if let Some(v) = transfer.transfer_out(self, entry_depth, Value::Unit)? {
                        return Ok(v);
                    }
                }

                Instr::NewStruct {
                    dst,
                    def,
                    base: abase,
                    n,
                } => {
                    let fields: Vec<Value> = (0..n as usize)
                        .map(|i| self.stack[base + abase as usize + i].clone())
                        .collect();
                    reg!(base, dst) = Value::new_struct(wscript_core::defs::DefId(def), fields);
                    self.mem_note(mem::node(n as u64))?;
                }
                Instr::GetField { dst, obj, idx } => {
                    let v = match &reg!(base, obj) {
                        Value::Struct(s) => s.fields.borrow().get(idx as usize).cloned(),
                        Value::Enum(e) => e.fields.borrow().get(idx as usize).cloned(),
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: field access on {}",
                                other.kind_name()
                            )));
                        }
                    };
                    match v {
                        Some(v) => reg!(base, dst) = v,
                        None => return Err(self.fault("field index out of range")),
                    }
                }
                Instr::SetField { obj, idx, src } => {
                    let v = reg!(base, src).clone();
                    let fields = match &reg!(base, obj) {
                        Value::Struct(s) => &s.fields,
                        Value::Enum(e) => &e.fields,
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: field write on {}",
                                other.kind_name()
                            )));
                        }
                    };
                    let mut fields = fields.borrow_mut();
                    if (idx as usize) < fields.len() {
                        fields[idx as usize] = v;
                    } else {
                        drop(fields);
                        return Err(self.fault("field index out of range"));
                    }
                }
                Instr::NewEnum {
                    dst,
                    def,
                    tag,
                    base: abase,
                    n,
                } => {
                    let fields: Vec<Value> = (0..n as usize)
                        .map(|i| self.stack[base + abase as usize + i].clone())
                        .collect();
                    reg!(base, dst) =
                        Value::new_enum(wscript_core::defs::DefId(def), tag as u32, fields);
                    self.mem_note(mem::node(n as u64))?;
                }
                Instr::GetTag { dst, obj } => {
                    let tag = match &reg!(base, obj) {
                        Value::Enum(e) => e.tag as i64,
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: tag read on {}",
                                other.kind_name()
                            )));
                        }
                    };
                    reg!(base, dst) = Value::Int(tag);
                }

                Instr::NewList {
                    dst,
                    base: abase,
                    n,
                } => {
                    let items: Vec<Value> = (0..n as usize)
                        .map(|i| self.stack[base + abase as usize + i].clone())
                        .collect();
                    reg!(base, dst) = Value::new_list(items);
                    self.mem_note(mem::node(n as u64))?;
                }
                Instr::NewMap {
                    dst,
                    base: abase,
                    n,
                } => {
                    let mut map = std::collections::BTreeMap::new();
                    for i in 0..n as usize {
                        let k = self.stack[base + abase as usize + i * 2].clone();
                        let v = self.stack[base + abase as usize + i * 2 + 1].clone();
                        let Some(key) = Key::from_value(&k) else {
                            return Err(
                                self.fault(format!("invalid map key of type {}", k.kind_name()))
                            );
                        };
                        map.insert(key, v);
                    }
                    reg!(base, dst) = Value::new_map(map);
                    self.mem_note(mem::node(n as u64 * 3))?;
                }
                Instr::ListIndexGet { dst, list, idx } => {
                    let i = int!(base, idx);
                    let v = match &reg!(base, list) {
                        Value::List(items) => {
                            let items = items.borrow();
                            if i < 0 || i as usize >= items.len() {
                                let len = items.len();
                                drop(items);
                                return Err(
                                    self.fault(format!("list index {i} out of bounds (len {len})"))
                                );
                            }
                            items[i as usize].clone()
                        }
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: list index on {}",
                                other.kind_name()
                            )));
                        }
                    };
                    reg!(base, dst) = v;
                }
                Instr::ListIndexSet { list, idx, src } => {
                    let i = int!(base, idx);
                    let v = reg!(base, src).clone();
                    match &reg!(base, list) {
                        Value::List(items) => {
                            let mut items = items.borrow_mut();
                            if i < 0 || i as usize >= items.len() {
                                let len = items.len();
                                drop(items);
                                return Err(
                                    self.fault(format!("list index {i} out of bounds (len {len})"))
                                );
                            }
                            items[i as usize] = v;
                        }
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: list index on {}",
                                other.kind_name()
                            )));
                        }
                    }
                }
                Instr::MapIndexGet { dst, map, key } => {
                    let k = reg!(base, key).clone();
                    let Some(key) = Key::from_value(&k) else {
                        return Err(
                            self.fault(format!("invalid map key of type {}", k.kind_name()))
                        );
                    };
                    let v = match &reg!(base, map) {
                        Value::Map(entries) => entries.borrow().get(&key).cloned(),
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: map index on {}",
                                other.kind_name()
                            )));
                        }
                    };
                    match v {
                        Some(v) => reg!(base, dst) = v,
                        None => {
                            let ks = k.display(&self.units[self.cur_unit].unit.defs);
                            return Err(self.fault(format!(
                                "missing map key: {ks} (use .get() for an Option)"
                            )));
                        }
                    }
                }
                Instr::MapIndexSet { map, key, src } => {
                    let k = reg!(base, key).clone();
                    let v = reg!(base, src).clone();
                    let Some(key) = Key::from_value(&k) else {
                        return Err(
                            self.fault(format!("invalid map key of type {}", k.kind_name()))
                        );
                    };
                    match &reg!(base, map) {
                        Value::Map(entries) => {
                            entries.borrow_mut().insert(key, v);
                        }
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: map index on {}",
                                other.kind_name()
                            )));
                        }
                    }
                    self.mem_note(mem::node(0))?;
                }

                Instr::NewCell { dst, src } => {
                    let v = reg!(base, src).clone();
                    reg!(base, dst) = Value::Cell(Rc::new(std::cell::RefCell::new(v)));
                    self.mem_note(mem::node(0))?;
                }
                Instr::CellGet { dst, cell } => {
                    let v = match &reg!(base, cell) {
                        Value::Cell(c) => c.borrow().clone(),
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: cell read on {}",
                                other.kind_name()
                            )));
                        }
                    };
                    reg!(base, dst) = v;
                }
                Instr::CellSet { cell, src } => {
                    let v = reg!(base, src).clone();
                    match &reg!(base, cell) {
                        Value::Cell(c) => *c.borrow_mut() = v,
                        other => {
                            return Err(self.fault(format!(
                                "type confusion: cell write on {}",
                                other.kind_name()
                            )));
                        }
                    }
                }
                Instr::MakeClosure { dst, proto } => {
                    let cap_srcs = self.units[self.cur_unit].unit.protos[proto as usize]
                        .captures
                        .clone();
                    let mut captures = Vec::with_capacity(cap_srcs.len());
                    for src in cap_srcs {
                        match src {
                            wscript_core::bytecode::CaptureSrc::Reg(r) => match &reg!(base, r) {
                                Value::Cell(c) => captures.push(c.clone()),
                                other => {
                                    return Err(self.fault(format!(
                                        "type confusion: capture of non-cell {}",
                                        other.kind_name()
                                    )));
                                }
                            },
                            wscript_core::bytecode::CaptureSrc::Capture(i) => {
                                let frame = self.frames.last().unwrap();
                                match &frame.closure {
                                    Some(c) => captures.push(c.captures[i as usize].clone()),
                                    None => {
                                        return Err(
                                            self.fault("type confusion: capture outside closure")
                                        );
                                    }
                                }
                            }
                        }
                    }
                    let n = captures.len() as u64;
                    reg!(base, dst) = Value::Closure(Rc::new(Closure { proto, captures }));
                    self.mem_note(mem::node(n))?;
                }
                Instr::LoadCapture { dst, slot } => {
                    let frame = self.frames.last().unwrap();
                    let cell = match &frame.closure {
                        Some(c) => c.captures[slot as usize].clone(),
                        None => {
                            return Err(self.fault("type confusion: capture outside closure"));
                        }
                    };
                    reg!(base, dst) = Value::Cell(cell);
                }

                Instr::MakeDyn { dst, src, vt } => {
                    let inner = reg!(base, src).clone();
                    reg!(base, dst) = Value::Dyn(Rc::new(DynObj { vtable: vt, inner }));
                    self.mem_note(mem::node(0))?;
                }

                Instr::Fault { code } => {
                    let msg = match code {
                        FaultCode::UnreachableMatch => {
                            "internal error: a match proven exhaustive fell through \
                             (this is a wscript bug — please report it)"
                        }
                    };
                    return Err(self.fault(msg));
                }
            }
        }
    }

    fn host_fault(&self, e: HostError) -> RuntimeError {
        // A fault raised inside a script callback (HostCtx::call_value)
        // that the host function propagated: re-raise it with the
        // callback's own frames first, a <host function> marker, then
        // the outer script frames — one coherent trace across the
        // boundary.
        if let Some(inner) = e.fault {
            let mut f = *inner;
            f.trace.push(TraceFrame {
                function: "<host function>".into(),
                span: None,
            });
            f.trace.extend(self.fault("").trace);
            return f;
        }
        let mut f = self.fault(e.message);
        f.exit_code = e.exit_code;
        f.trace.insert(
            0,
            TraceFrame {
                function: "<host function>".into(),
                span: None,
            },
        );
        f
    }
}

// ------------------------------------------------------- control transfer

/// Control transfer for one running dispatch loop: the only thing that
/// moves a frame's `pc` — [`advance`](Transfer::advance) by one for the
/// instruction just fetched, the `transfer_*` methods for everything else
/// — and so the only thing that charges fuel for the instructions it
/// moved past.
///
/// Fuel is 1 instruction = 1 fuel, exact, but *charged* a straight-line
/// block at a time: `block_start` marks the pc where the current uncharged
/// run began, and each transfer pays for the run that ends at it before
/// starting the next one. Straight-line dispatch therefore carries no fuel
/// overhead at all, and unmetered dispatch (`METERED = false`, a separate
/// monomorphization) carries none of the bookkeeping either.
///
/// The visible consequence is the one [`Vm::set_fuel`] documents:
/// exhaustion surfaces at the charge point, so up to one basic block may
/// run past the budget — but never a host call, a script call or a loop
/// iteration, because each of those *is* a charge point. A fault other
/// than exhaustion abandons its block uncharged.
///
/// Three obligations used to sit on each dispatch arm that transferred
/// control, and each was a comment rather than a check: charge before
/// moving `pc`, restart the block at the right pc for that kind of
/// transfer, and lend the tank to anything that might re-enter the VM.
/// They live here now, so an arm cannot honour two of the three, and a new
/// control-transfer instruction gets them by construction.
///
/// Every method takes the `Vm` it is transferring within rather than
/// holding it: the dispatch loop runs as `&mut self` on the `Vm`, so a
/// `Transfer` that borrowed it could not be a local of that loop — which
/// is exactly what keeping the tank in a register requires.
struct Transfer<const METERED: bool> {
    /// The pc where the current uncharged straight-line run began.
    block_start: usize,
    /// The dispatch loop's fuel tank: a local mirror of [`Vm::fuel`] the
    /// optimizer can keep in a register. Meaningless when `!METERED`.
    fuel: u64,
}

impl<const METERED: bool> Transfer<METERED> {
    /// Begin accounting at the frame the loop starts in. Its pc is 0 —
    /// every entry into a dispatch loop pushes the frame it runs — but
    /// the block starts wherever the frame is, so that the accounting
    /// does not rest on it.
    fn new(vm: &Vm, fuel: u64) -> Transfer<METERED> {
        Transfer {
            block_start: vm.frames.last().unwrap().pc,
            fuel,
        }
    }

    /// Step past the instruction just fetched. The one pc movement that
    /// is not a transfer — and the unit the block length that every
    /// transfer pays for is measured in.
    fn advance(&self, vm: &mut Vm) {
        vm.frames.last_mut().unwrap().pc += 1;
    }

    /// Pay for the straight-line run that ends at the current pc.
    fn charge(&mut self, vm: &Vm) -> Result<(), RuntimeError> {
        if METERED {
            let ran = (vm.frames.last().unwrap().pc - self.block_start) as u64;
            let Some(rest) = self.fuel.checked_sub(ran) else {
                self.fuel = 0;
                return Err(vm.fault("fuel exhausted"));
            };
            self.fuel = rest;
        }
        Ok(())
    }

    /// Jump within the current frame: `pc += off`, relative to the next
    /// instruction (the pc has already been advanced past this one).
    fn transfer_to(&mut self, vm: &mut Vm, off: i32) -> Result<(), RuntimeError> {
        self.charge(vm)?;
        let frame = vm.frames.last_mut().unwrap();
        frame.pc = (frame.pc as i64 + off as i64) as usize;
        self.block_start = frame.pc;
        Ok(())
    }

    /// Call into a new script frame. `push` does the pushing, plus any
    /// resolution that must happen after the caller's block is paid for —
    /// so a callee that cannot be resolved faults with the same fuel spent
    /// as one that can.
    fn transfer_into(
        &mut self,
        vm: &mut Vm,
        push: impl FnOnce(&mut Vm) -> Result<(), RuntimeError>,
    ) -> Result<(), RuntimeError> {
        self.charge(vm)?;
        push(vm)?;
        // The callee starts at the top of its own code.
        self.block_start = 0;
        Ok(())
    }

    /// Return `v` from the current frame. `Ok(None)` means the caller's
    /// frame took the value and dispatch continues there; `Ok(Some(v))`
    /// means the frame that returned was this loop's entry frame, so the
    /// loop is done.
    fn transfer_out(
        &mut self,
        vm: &mut Vm,
        entry_depth: usize,
        v: Value,
    ) -> Result<Option<Value>, RuntimeError> {
        self.charge(vm)?;
        let frame = vm.frames.pop().unwrap();
        vm.stack.truncate(frame.base);
        if vm.frames.len() == entry_depth {
            return Ok(Some(v));
        }
        // The caller resumes mid-block, at the instruction after its call.
        self.block_start = vm.frames.last().unwrap().pc;
        vm.stack[frame.ret_slot] = v;
        Ok(None)
    }

    /// Leave the dispatch loop for native code — a host function or a
    /// builtin — and come back. The pc does not move, but the block ends
    /// here all the same: the callee may spend fuel of its own, either by
    /// re-entering a nested `execute` (script callbacks, `map`/`filter`,
    /// custom operator impls) or through [`Vm::charge_structural`]. Both
    /// draw from `Vm::fuel`, so the tank is lent across the call and taken
    /// back afterwards — on the error path too, since a host may catch a
    /// callback's fault and run on.
    fn transfer_native<R>(
        &mut self,
        vm: &mut Vm,
        call: impl FnOnce(&mut Vm) -> R,
    ) -> Result<R, RuntimeError> {
        self.charge(vm)?;
        self.block_start = vm.frames.last().unwrap().pc;
        if METERED {
            vm.fuel = Some(self.fuel);
        }
        // What the callee holds off the register stack while it runs
        // (`Vm::mem_hold`) is in its result, or dropped, when it returns.
        let held = vm.mem_held();
        let result = call(vm);
        vm.set_mem_held(held);
        if METERED {
            self.fuel = vm.fuel.unwrap_or(0);
        }
        Ok(result)
    }

    /// A host function has returned: fill the tank again if the embedder
    /// says so ([`VmConfig::refuel`]). Not for a builtin, which is the
    /// script's own work.
    fn refuel(&mut self, vm: &mut Vm) {
        if METERED
            && let Some(full) = vm.fuel_full
            && let Some(hook) = &mut vm.refuel
            && hook()
        {
            self.fuel = full;
        }
    }
}

/// `HostCtx` implementation handed to host functions while they run.
struct VmHostCtx<'a> {
    vm: &'a mut Vm,
}

impl<'a> HostCtx for VmHostCtx<'a> {
    fn defs(&self) -> &wscript_core::defs::DefTable {
        &self.vm.units[self.vm.cur_unit].unit.defs
    }

    fn display_value(&self, v: &Value) -> String {
        // Structural rendering (custom Display impls are not consulted at
        // the host boundary — documented limitation).
        v.display(self.defs())
    }

    fn call_value(&mut self, f: &Value, args: Vec<Value>) -> Result<Value, HostError> {
        self.vm.reenter(f, args).map_err(|e| HostError {
            message: e.message.clone(),
            exit_code: e.exit_code,
            fault: Some(Box::new(e)),
        })
    }
}
