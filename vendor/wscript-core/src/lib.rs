//! Shared core types for the wscript scripting language.
//!
//! This crate holds everything the compiler and the VM must agree on:
//! source spans, diagnostics, the type representation, definition tables
//! (structs / enums / traits), the bytecode format, and the runtime value
//! model. The VM depends on this crate, never on the parser.

pub mod bytecode;
pub mod defs;
pub mod diag;
pub mod fault;
pub mod fmt_spec;
pub mod host;
pub mod module;
pub mod registry;
pub mod source_map;
pub mod span;
pub mod types;
pub mod value;

pub use bytecode::{Builtin, CallTarget, CompiledUnit, Const, FnProto, Instr};
pub use defs::{DefId, DefKind, DefTable, EnumDef, StructDef, TraitDef, VariantDef, VariantKind};
pub use diag::{Diagnostic, Severity};
pub use fault::{ScriptFault, TraceFrame};
pub use host::{
    Fault, FromValue, HostCallable, HostCtx, HostError, IntoValue, ScriptArgs, ScriptClosure,
    ScriptType,
};
pub use module::Module;
pub use registry::{ModuleDef, Registry};
pub use source_map::{SourceFileInfo, SourceMap};
pub use span::Span;
pub use types::{FnSig, Type};
pub use value::Value;
