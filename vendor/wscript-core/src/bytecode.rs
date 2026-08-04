//! The wscript bytecode format.
//!
//! Register-based, Lua-5.x-flavoured (PRD §5.2): each function has its own
//! register file, instructions name registers directly, and a per-unit
//! constant table holds literals. Instructions are a fixed-width Rust enum.
//!
//! **The format is internal and unstable in v1** — there is no serialization
//! guarantee; it exists only in memory between `compile` and `run`.
//!
//! Conventions:
//! - Registers `0..n_params` of a frame hold the arguments (the receiver is
//!   argument 0 for method calls).
//! - Calls pass arguments in a contiguous window: `base..base + nargs`.
//! - Jump offsets are relative to the *next* instruction.

use std::collections::HashMap;
use std::sync::Arc;

use crate::defs::DefTable;
use crate::span::Span;
use crate::types::FnSig;

/// A compile-time constant. Unlike [`crate::Value`] this is `Send + Sync`,
/// so a `CompiledUnit` can be shared across threads and instantiated by
/// per-thread VMs (PRD §4.3).
#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    Unit,
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    Str(Arc<str>),
}

/// Where a call lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTarget {
    /// A script function: index into `CompiledUnit::protos`.
    Proto(u32),
    /// A host-registered function: index into the context's host fn table.
    Host(u32),
    /// A VM-native builtin (container/string methods, prelude functions).
    Builtin(Builtin),
}

/// VM-native builtin routines. These back the methods of `string`, `List`,
/// `Map`, `Option`, `Result`, `weak` and the prelude functions; they live in
/// the VM (not host fns) so they can re-enter script code (e.g. `map(f)`).
///
/// The list grows with the milestones; adding a variant is not a format
/// break (the format is unstable, see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Builtin {
    // ---- prelude functions ----
    Print,
    Println,
    /// `str(x)` — convert any displayable value to string.
    Str,
    /// `fmt(template, args…)` — `{}` placeholder formatting (v1 stand-in for
    /// interpolation).
    Fmt,
    /// `same(a, b)` — reference identity (PRD §3.7).
    Same,
    /// `weak(x)` — downgrade to a weak reference (PRD §4.2).
    WeakNew,
    /// `int(x)`, `float(x)` — numeric conversions.
    IntCast,
    FloatCast,
    /// Render a unit-family value: `(raw, def_id)` → string. Emitted in
    /// place of `Str` wherever the static type says a value belongs to a
    /// unit family (PRD §3.9).
    FmtQuantity,
    // ---- weak methods ----
    WeakUpgrade,
    // ---- string methods ----
    StrLen,
    StrSplit,
    StrTrim,
    StrTrimStart,
    StrTrimEnd,
    StrToUpper,
    StrToLower,
    StrStartsWith,
    StrEndsWith,
    StrContains,
    StrFind,
    StrReplace,
    StrRepeat,
    StrPadLeft,
    StrPadRight,
    StrChars,
    StrSlice,
    StrParseInt,
    StrParseFloat,
    StrIsEmpty,
    StrBytesLen,
    // ---- list methods ----
    ListLen,
    ListPush,
    ListPop,
    ListGet,
    ListSet,
    ListInsert,
    ListRemove,
    ListClear,
    ListContains,
    ListIndexOf,
    ListReverse,
    ListSort,
    ListJoin,
    ListMap,
    ListFilter,
    ListFold,
    ListFirst,
    ListLast,
    ListSlice,
    ListConcat,
    ListIsEmpty,
    ListClone,
    /// `any`/`all`/`find`/`position`/`count` — short-circuiting predicate
    /// combinators.
    ListAny,
    ListAll,
    ListFind,
    ListPosition,
    ListCount,
    /// `sum` — the checker selects Int vs Float from the element type so
    /// empty lists stay type-correct.
    ListSumInt,
    ListSumFloat,
    ListMin,
    ListMax,
    /// `sort_by(cmp)` — stable sort with a script comparator returning
    /// negative/0/positive.
    ListSortBy,
    /// `map_indexed(|i, x| ...)` / `zip_with(other, |a, b| ...)` — the
    /// tuple-free stand-ins for enumerate/zip.
    ListMapIndexed,
    ListZipWith,
    // ---- map methods ----
    MapLen,
    MapInsert,
    MapRemove,
    MapGet,
    MapContainsKey,
    MapKeys,
    MapValues,
    MapClear,
    MapIsEmpty,
    MapClone,
    /// `each`/`map`/`filter` over (key, value) pairs — two-parameter
    /// closures replace tuple-returning `entries()`.
    MapEach,
    MapMapEntries,
    MapFilter,
    // ---- option methods ----
    OptionIsSome,
    OptionIsNone,
    OptionUnwrap,
    OptionUnwrapOr,
    OptionExpect,
    // ---- result methods ----
    ResultIsOk,
    ResultIsErr,
    ResultUnwrap,
    ResultUnwrapOr,
    ResultUnwrapErr,
    ResultExpect,
    // ---- derive support ----
    /// Deep clone (derive `Clone`, PRD §3.8).
    DeepClone,
    /// Structural equality; consults `CompiledUnit::impls.eq` for nested
    /// values with custom `Eq` impls.
    ValueEq,
    /// Structural three-way comparison (-1/0/1); consults `impls.cmp`.
    ValueCmp,
}

/// VM fault codes for compiled-in unreachable paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultCode {
    /// A `match` proven exhaustive by the checker fell through — a compiler
    /// or VM bug, surfaced as a trappable error rather than UB.
    UnreachableMatch,
}

/// One bytecode instruction. Register operands are `u16`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Instr {
    // ---- constants & moves ----
    LoadConst {
        dst: u16,
        k: u32,
    },
    LoadUnit {
        dst: u16,
    },
    LoadBool {
        dst: u16,
        v: bool,
    },
    /// Small integer immediate (larger ints go through the const table).
    LoadInt {
        dst: u16,
        v: i32,
    },
    Move {
        dst: u16,
        src: u16,
    },

    // ---- integer arithmetic (wrapping; div/rem fault on zero) ----
    AddI {
        dst: u16,
        a: u16,
        b: u16,
    },
    SubI {
        dst: u16,
        a: u16,
        b: u16,
    },
    MulI {
        dst: u16,
        a: u16,
        b: u16,
    },
    DivI {
        dst: u16,
        a: u16,
        b: u16,
    },
    RemI {
        dst: u16,
        a: u16,
        b: u16,
    },
    NegI {
        dst: u16,
        src: u16,
    },

    // ---- float arithmetic ----
    AddF {
        dst: u16,
        a: u16,
        b: u16,
    },
    SubF {
        dst: u16,
        a: u16,
        b: u16,
    },
    MulF {
        dst: u16,
        a: u16,
        b: u16,
    },
    DivF {
        dst: u16,
        a: u16,
        b: u16,
    },
    RemF {
        dst: u16,
        a: u16,
        b: u16,
    },
    NegF {
        dst: u16,
        src: u16,
    },

    // ---- string ----
    ConcatStr {
        dst: u16,
        a: u16,
        b: u16,
    },

    // ---- bool ----
    Not {
        dst: u16,
        src: u16,
    },

    // ---- comparisons (Gt/Ge are emitted as swapped Lt/Le) ----
    EqI {
        dst: u16,
        a: u16,
        b: u16,
    },
    EqF {
        dst: u16,
        a: u16,
        b: u16,
    },
    EqBool {
        dst: u16,
        a: u16,
        b: u16,
    },
    EqChar {
        dst: u16,
        a: u16,
        b: u16,
    },
    EqStr {
        dst: u16,
        a: u16,
        b: u16,
    },
    LtI {
        dst: u16,
        a: u16,
        b: u16,
    },
    LeI {
        dst: u16,
        a: u16,
        b: u16,
    },
    LtF {
        dst: u16,
        a: u16,
        b: u16,
    },
    LeF {
        dst: u16,
        a: u16,
        b: u16,
    },
    LtChar {
        dst: u16,
        a: u16,
        b: u16,
    },
    LeChar {
        dst: u16,
        a: u16,
        b: u16,
    },
    LtStr {
        dst: u16,
        a: u16,
        b: u16,
    },
    LeStr {
        dst: u16,
        a: u16,
        b: u16,
    },

    // ---- control flow ----
    Jump {
        off: i32,
    },
    JumpIfFalse {
        cond: u16,
        off: i32,
    },
    JumpIfTrue {
        cond: u16,
        off: i32,
    },

    // ---- calls ----
    /// Call a statically-resolved target with args in `base..base+nargs`.
    Call {
        dst: u16,
        base: u16,
        nargs: u16,
        target: CallTarget,
    },
    /// Call a function value (closure) held in register `f`.
    CallValue {
        dst: u16,
        f: u16,
        base: u16,
        nargs: u16,
    },
    /// Dynamic dispatch: receiver (a `dyn` value) in `base`, method `slot`
    /// looked up in its vtable.
    CallVirtual {
        dst: u16,
        base: u16,
        nargs: u16,
        slot: u16,
    },
    Ret {
        src: u16,
    },
    RetUnit,

    // ---- structs & enums ----
    /// Construct a struct of `def` from `n` field values at `base..`.
    NewStruct {
        dst: u16,
        def: u32,
        base: u16,
        n: u16,
    },
    GetField {
        dst: u16,
        obj: u16,
        idx: u16,
    },
    SetField {
        obj: u16,
        idx: u16,
        src: u16,
    },
    /// Construct an enum value of `def`/`tag` from `n` payload values.
    NewEnum {
        dst: u16,
        def: u32,
        tag: u16,
        base: u16,
        n: u16,
    },
    /// Load an enum value's tag as an int.
    GetTag {
        dst: u16,
        obj: u16,
    },

    // ---- containers ----
    NewList {
        dst: u16,
        base: u16,
        n: u16,
    },
    /// `n` is the number of key/value *pairs*; 2n registers at `base..`.
    NewMap {
        dst: u16,
        base: u16,
        n: u16,
    },
    /// `list[i]` — faults on out-of-bounds (use `.get` for `Option`).
    ListIndexGet {
        dst: u16,
        list: u16,
        idx: u16,
    },
    ListIndexSet {
        list: u16,
        idx: u16,
        src: u16,
    },
    /// `map[k]` — faults on missing key (use `.get` for `Option`).
    MapIndexGet {
        dst: u16,
        map: u16,
        key: u16,
    },
    /// `map[k] = v` — inserts or overwrites.
    MapIndexSet {
        map: u16,
        key: u16,
        src: u16,
    },

    // ---- closures & capture cells ----
    /// Box a value into a fresh mutable cell (for captured locals).
    NewCell {
        dst: u16,
        src: u16,
    },
    CellGet {
        dst: u16,
        cell: u16,
    },
    CellSet {
        cell: u16,
        src: u16,
    },
    /// Instantiate a closure over `proto`, snapshotting the capture list
    /// described by `FnProto::captures`.
    MakeClosure {
        dst: u16,
        proto: u32,
    },
    /// Load capture cell `slot` of the currently executing closure into a
    /// register (emitted in closure prologues).
    LoadCapture {
        dst: u16,
        slot: u16,
    },

    // ---- traits ----
    /// Coerce a concrete value to `dyn Trait` by attaching vtable `vt`.
    MakeDyn {
        dst: u16,
        src: u16,
        vt: u32,
    },

    // ---- misc ----
    Fault {
        code: FaultCode,
    },
    Nop,
}

/// Where a closure capture is sourced from at `MakeClosure` time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSrc {
    /// A register of the enclosing frame currently holding a cell.
    Reg(u16),
    /// A capture slot of the enclosing closure.
    Capture(u16),
}

/// A compiled function body.
#[derive(Debug, Clone)]
pub struct FnProto {
    pub name: String,
    pub n_params: u16,
    /// Total register-file size for a frame of this function.
    pub n_regs: u16,
    pub code: Vec<Instr>,
    /// Span per instruction (parallel to `code`) for runtime error reporting.
    pub spans: Vec<Span>,
    /// Capture sources consumed by `MakeClosure` for this proto.
    pub captures: Vec<CaptureSrc>,
}

/// A method table for one `(concrete type, trait)` impl, used by `dyn Trait`
/// dispatch. Slot order matches the trait's method declaration order.
#[derive(Debug, Clone)]
pub struct VTable {
    pub targets: Vec<CallTarget>,
}

/// Custom (non-derived) operator impls, consulted by the runtime's
/// structural equality/ordering/display routines when they descend into
/// nested values. Keyed by `DefId.0` → proto index.
#[derive(Debug, Clone, Default)]
pub struct ImplMaps {
    pub eq: HashMap<u32, u32>,
    pub cmp: HashMap<u32, u32>,
    pub display: HashMap<u32, u32>,
}

/// The output of compilation: everything a VM needs to run the script.
///
/// `Send + Sync` by construction (no `Rc` values) so one compilation can be
/// shared across threads and executed by per-thread VMs (PRD §4.3).
#[derive(Debug, Clone)]
pub struct CompiledUnit {
    /// Unique id (process-wide), used by VMs to cache per-unit state.
    pub id: u64,
    pub protos: Vec<FnProto>,
    pub consts: Vec<Const>,
    /// Builtins + host defs + script defs (script defs appended at the end).
    pub defs: DefTable,
    pub vtables: Vec<VTable>,
    pub impls: ImplMaps,
    /// Top-level script functions callable from the host: name → (proto
    /// index, signature). Signatures are checked at the host boundary.
    pub exports: HashMap<String, (u32, FnSig)>,
    /// Names of top-level *generic* fns — not exported (their erased
    /// signatures can never match host types); kept so the host boundary
    /// can explain why a lookup failed.
    pub generic_fns: Vec<String>,
    /// Span address space → file mapping (multi-file compilations).
    /// Single-file units have one entry; fault renderers use it to name
    /// the right file per frame.
    pub source_map: crate::source_map::SourceMap,
}

impl CompiledUnit {
    pub fn proto_of(&self, name: &str) -> Option<u32> {
        self.exports.get(name).map(|(i, _)| *i)
    }
}

// Compile-time guarantee that units can be shared across threads (PRD §4.3).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CompiledUnit>();
};
