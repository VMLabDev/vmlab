//! The type checker (PRD §5.1): bidirectional checking with local
//! inference. Consumes the AST and the host registry, produces a fully
//! typed view of the program in side tables keyed by `NodeId`, which the
//! bytecode emitter consumes. All host signatures are known before any
//! script code is checked (PRD §2's key invariant).

mod expr;
mod infer;
mod methods;
mod pat;

use std::collections::{HashMap, HashSet};

use wscript_core::bytecode::Const;
use wscript_core::defs::{
    self, DefId, DefKind, DefTable, EnumDef, Factor, StructDef, TraitDef, UnitDef, VariantDef,
    VariantKind,
};
use wscript_core::diag::Diagnostic;
use wscript_core::registry::Registry;
use wscript_core::span::Span;
use wscript_core::types::{FnSig, Type};

use crate::ast::*;
use infer::Infer;

pub use infer::subst_params;

pub type LocalId = u32;

// ------------------------------------------------------------ resolutions

/// How a variable reference is reached at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarRes {
    Local(LocalId),
    /// Capture slot of the enclosing closure.
    Capture(u16),
}

/// Resolution of a non-variable path expression.
#[derive(Debug, Clone)]
pub enum PathRes {
    /// A top-level script function used as a value.
    FnValue(u32),
    /// A registered module constant.
    Const(Const),
    /// A unit enum variant used as a value.
    Variant { def: DefId, tag: u32 },
}

/// The prelude functions (always in scope, VM-native).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreludeFn {
    Print,
    Println,
    Str,
    Fmt,
    Same,
    Weak,
    Int,
    Float,
}

/// What a call expression lowers to.
#[derive(Debug, Clone)]
pub enum CallKind {
    Proto(u32),
    Host(u32),
    Prelude(PreludeFn),
    /// Enum variant constructor: `Some(x)`, `Event::Key(c)`.
    Variant {
        def: DefId,
        tag: u32,
    },
    /// Calling a function value: callee is evaluated.
    Value,
    /// `Duration::ms(n)` — not a call at all: the argument is scaled into
    /// base units inline. The factor is in `CheckResult::unit_convs`.
    UnitConv,
}

#[derive(Debug, Clone)]
pub enum MethodRes {
    Proto(u32),
    Host(u32),
    Builtin(wscript_core::Builtin),
    /// `dyn Trait` dispatch through the receiver's vtable.
    Virtual {
        slot: u16,
    },
}

#[derive(Debug, Clone)]
pub enum StructLitRes {
    Struct(DefId),
    Variant { def: DefId, tag: u32 },
}

/// A conversion between a unit family and its backing primitive. Both
/// directions lower to one constant multiply or divide — unit values are
/// erased to the backing number at runtime.
#[derive(Debug, Clone, Copy)]
pub enum ConvKind {
    /// `d.ms` — divide the base count by the unit's factor.
    Out { factor: Factor },
    /// `Duration::ms(n)` — multiply into base units.
    In { factor: Factor },
}

#[derive(Debug, Clone)]
pub enum IndexKind {
    List,
    Map,
    /// User `Index` trait impl.
    UserGet {
        proto: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimKind {
    Int,
    Float,
    Bool,
    Char,
    Str,
}

/// Resolved lowering of a binary operator.
#[derive(Debug, Clone)]
pub enum BinOpKind {
    IntArith(BinOp),
    FloatArith(BinOp),
    Concat,
    And,
    Or,
    EqPrim {
        kind: PrimKind,
        negate: bool,
    },
    /// `< <= > >=` on primitives.
    CmpPrim {
        kind: PrimKind,
        op: BinOp,
    },
    /// Structural equality (derived `Eq`, containers).
    EqValue {
        negate: bool,
    },
    /// Custom `Eq` impl: direct call.
    EqCall {
        proto: u32,
        negate: bool,
    },
    /// Structural comparison (derived `Ord`).
    CmpValue {
        op: BinOp,
    },
    /// Custom `Ord` impl: call `cmp`, compare result with 0.
    CmpCall {
        proto: u32,
        op: BinOp,
    },
    /// Arithmetic operator trait impl (`Add`…`Rem`).
    ArithCall {
        proto: u32,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum UnOpKind {
    NegInt,
    NegFloat,
    Not,
    NegCall { proto: u32 },
}

#[derive(Debug, Clone, Copy)]
pub enum ForKind {
    RangeExclusive,
    RangeInclusive,
    List,
    MapKeys,
    StrChars,
}

#[derive(Debug, Clone, Copy)]
pub enum TryKind {
    Option,
    Result,
}

/// How a closure capture slot is filled at `MakeClosure` time.
#[derive(Debug, Clone, Copy)]
pub enum CapSrc {
    /// A (cell-allocated) local of the directly enclosing function.
    Local(LocalId),
    /// A capture slot of the directly enclosing closure.
    Capture(u16),
}

#[derive(Debug, Clone)]
pub struct ClosureRes {
    pub proto: u32,
}

/// Where a function's AST lives (the emitter walks it by this reference).
#[derive(Debug, Clone, Copy)]
pub enum FnSource {
    /// `files[file].items[item]` is an `Item::Fn`.
    Top { file: usize, item: usize },
    /// `files[file].items[item]` is an `Item::Impl`; method `fns[fn_idx]`.
    Method {
        file: usize,
        item: usize,
        fn_idx: usize,
    },
    /// A closure expression with this node id.
    Closure { node: NodeId },
    // (Top/Method carry the file index of a multi-file compilation.)
    /// Synthesized — no AST (not used in v1; reserved).
    Synthesized,
}

/// A built-in bound on a generic type parameter (`fn f[T: Ord]`).
/// `Ord` implies `Eq` (mirroring the derive rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundKind {
    Eq,
    Ord,
    Clone,
}

/// A generic call deferred to end-of-function bound/inference checking.
pub(crate) struct PendingInstantiation {
    pub span: Span,
    pub fn_name: String,
    pub type_params: Vec<(String, Option<BoundKind>)>,
    /// The fresh inference vars the call was instantiated with.
    pub subst: Vec<Type>,
}

#[derive(Debug, Clone)]
pub struct FnInfo {
    pub name: String,
    pub sig: FnSig,
    /// Generic type parameters (name, bound) — the sig's `Type::Param(i)`
    /// indices refer to this list. Empty for monomorphic fns.
    pub type_params: Vec<(String, Option<BoundKind>)>,
    pub source: FnSource,
    /// Total locals (params first). Filled after body checking.
    pub n_locals: u32,
    /// Locals captured by some nested closure (need cell allocation).
    pub captured: HashSet<LocalId>,
    /// For closures: how each capture slot is sourced from the enclosing
    /// frame.
    pub captures: Vec<CapSrc>,
    pub span: Span,
    /// `true` while the body has not been checked yet.
    pub pending: bool,
}

pub use wscript_core::bytecode::ImplMaps;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Derives {
    pub eq: bool,
    pub ord: bool,
    pub display: bool,
    pub clone: bool,
}

/// Everything the checker learned, keyed by AST node ids.
#[derive(Default)]
pub struct CheckResult {
    pub defs: DefTable,
    pub diags: Vec<Diagnostic>,
    /// Type of every expression node (fully resolved).
    pub types: HashMap<NodeId, Type>,
    /// Variable references (`Path` exprs that name a local or capture).
    pub var_refs: HashMap<NodeId, VarRes>,
    /// Local slot for `let` statements, `for` loop variables and pattern
    /// bindings (keyed by stmt id / for-expr id / pattern id).
    pub decl_locals: HashMap<NodeId, LocalId>,
    pub paths: HashMap<NodeId, PathRes>,
    pub calls: HashMap<NodeId, CallKind>,
    pub methods: HashMap<NodeId, MethodRes>,
    /// Field expr → runtime field index.
    pub fields: HashMap<NodeId, u16>,
    /// Unit-family conversions: `d.ms` (field exprs) and `Duration::ms(n)`
    /// (call exprs) → the constant scale to apply.
    pub unit_convs: HashMap<NodeId, ConvKind>,
    /// Suffixed literals (`500ms`), already folded to a base-unit constant.
    pub quantity_lits: HashMap<NodeId, Factor>,
    pub struct_lits: HashMap<NodeId, StructLitRes>,
    /// Struct literal / struct pattern: for each field as written, the
    /// runtime field index.
    pub field_orders: HashMap<NodeId, Vec<u16>>,
    pub indexes: HashMap<NodeId, IndexKind>,
    pub bin_ops: HashMap<NodeId, BinOpKind>,
    pub un_ops: HashMap<NodeId, UnOpKind>,
    pub for_kinds: HashMap<NodeId, ForKind>,
    pub try_kinds: HashMap<NodeId, TryKind>,
    /// Variant patterns (and bindings reinterpreted as unit variants).
    pub pattern_variants: HashMap<NodeId, (DefId, u32)>,
    /// Struct patterns → the struct def they destructure.
    pub pattern_structs: HashMap<NodeId, DefId>,
    pub closures: HashMap<NodeId, ClosureRes>,
    /// Exprs needing a `MakeDyn` wrap after evaluation → vtable id.
    pub dyn_wraps: HashMap<NodeId, u32>,
    pub fn_infos: Vec<FnInfo>,
    /// Names of top-level generic fns (not exported; see collect_exports).
    pub generic_fns: Vec<String>,
    /// vtable id → method slot targets (script proto indices).
    pub vtables: Vec<Vec<u32>>,
    pub impl_maps: ImplMaps,
    pub exports: HashMap<String, (u32, FnSig)>,
    /// Reference → definition span (locals and script functions), for the
    /// LSP's goto-definition.
    pub def_spans: HashMap<NodeId, Span>,
    /// Script methods per type (inherent + trait impls), for the LSP's
    /// completion.
    pub methods_by_type: HashMap<DefId, Vec<(String, FnSig)>>,
}

// ----------------------------------------------------------------- scope

#[derive(Clone)]
struct Binding {
    local: LocalId,
    ty: Type,
    /// Definition span (for the LSP's goto-definition).
    #[allow(dead_code)] // consumed by the LSP (M6)
    span: Span,
}

struct Scope {
    bindings: HashMap<String, Binding>,
    /// Index into `fn_states` of the owning function.
    fn_depth: usize,
}

struct LoopCtx {
    has_break: bool,
}

struct FnState {
    ret: Type,
    n_locals: u32,
    captured: HashSet<LocalId>,
    captures: Vec<CapSrc>,
    /// Dedup: (owner fn depth, local) → capture slot.
    capture_map: HashMap<(usize, LocalId), u16>,
    loops: Vec<LoopCtx>,
}

/// Item imported via `use module::item`.
#[derive(Clone)]
enum Imported {
    HostFn(u32),
    Const(Type, Const),
    /// `use helpers::foo` where `helpers` is a script file: resolved to
    /// a proto lazily (fn protos are collected after uses).
    ScriptItem {
        file: usize,
    },
}

/// What a `use`d module name refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModuleRef {
    /// Registry module index (host/stdlib).
    Host(usize),
    /// Imported script file index.
    Script(usize),
}

/// A resolved single-name import (see `Checker::imported`).
pub(crate) enum ImportedRef {
    HostFn(u32),
    Const(Type, Const),
    ScriptFn(u32),
}

pub struct Checker<'a> {
    /// All files of the program: (module name, AST). Index 0 is the
    /// entry file; the module name of the entry is unused.
    pub(crate) files: &'a [(String, &'a SourceFile)],
    /// File whose items/bodies are currently being processed.
    pub(crate) cur_file: usize,
    pub(crate) reg: &'a Registry,
    pub(crate) out: CheckResult,
    pub(crate) infer: Infer,

    // module-level scope
    type_names: HashMap<String, DefId>,
    /// Per-file: top-level fn name → proto (file-scoped visibility).
    fn_names: Vec<HashMap<String, u32>>,
    /// Per-file: `use`d module names.
    modules_in_scope: Vec<HashMap<String, ModuleRef>>,
    /// Per-file: `use module::item` imports.
    imports: Vec<HashMap<String, Imported>>,
    /// Script module name → file index (imported files; program-wide).
    script_modules: HashMap<String, usize>,

    // impls
    pub(crate) inherent: HashMap<DefId, HashMap<String, u32>>,
    /// Associated functions (no-self fns in inherent impls): `Type::func`.
    pub(crate) assoc: HashMap<DefId, HashMap<String, u32>>,
    /// Type parameters in scope — set while resolving a generic fn's
    /// signature and while checking its body. `Type::Param(i)` indexes
    /// this list (rigid: unifies only with itself).
    pub(crate) current_type_params: Vec<(String, Option<BoundKind>)>,
    /// Generic calls whose instantiation wasn't resolved at the callsite:
    /// re-checked (bounds + inference) when the enclosing top-level fn
    /// finishes. (call span, fn name, type_params, fresh instantiation).
    pub(crate) pending_instantiations: Vec<PendingInstantiation>,
    /// (type, trait) → method protos in trait declaration order.
    pub(crate) trait_impls: HashMap<(DefId, DefId), Vec<u32>>,
    pub(crate) derives: HashMap<DefId, Derives>,
    vtable_cache: HashMap<(DefId, DefId), u32>,
    /// Unit name → the families declaring it. Program-wide, like types.
    /// More than one entry means a suffix needs an expected type to
    /// disambiguate.
    unit_suffixes: HashMap<String, Vec<DefId>>,

    // body-checking state
    scopes: Vec<Scope>,
    fn_states: Vec<FnState>,
    /// Nodes whose recorded types must be finalized when the current
    /// top-level function completes (inference vars are per top-level fn).
    nodes_this_fn: Vec<NodeId>,
    /// Nodes (let statements) whose types must be fully inferred by the
    /// end of the enclosing function.
    pub(crate) must_resolve: Vec<(NodeId, Span)>,
    /// > 0 while checking the alternatives of an or-pattern (bindings are
    /// > rejected there in v1).
    pub(crate) or_depth: u32,
    /// Current `check_expr` recursion depth — capped so a pathologically
    /// deep AST produces E0271 instead of overflowing the stack (the
    /// parser bounds its own recursion, but builds operator/postfix
    /// chains like `x[0][0]…` iteratively).
    pub(crate) expr_depth: u32,
    /// E0271 already reported — deeper nodes error silently.
    pub(crate) expr_depth_reported: bool,
}

pub fn check(file: &SourceFile, registry: &Registry) -> CheckResult {
    let files = [(String::new(), file)];
    check_files(&files, registry)
}

/// Whole-program check over a multi-file compilation. `files[0]` is the
/// entry file (only its fns are exported); every other entry is an
/// imported script module named by its `String`.
pub fn check_files<'a>(files: &'a [(String, &'a SourceFile)], registry: &Registry) -> CheckResult {
    let script_modules: HashMap<String, usize> = files
        .iter()
        .enumerate()
        .skip(1)
        .map(|(i, (name, _))| (name.clone(), i))
        .collect();
    let mut checker = Checker {
        files,
        cur_file: 0,
        reg: registry,
        out: CheckResult {
            defs: registry.defs.clone(),
            ..CheckResult::default()
        },
        infer: Infer::default(),
        type_names: HashMap::new(),
        fn_names: vec![HashMap::new(); files.len()],
        modules_in_scope: vec![HashMap::new(); files.len()],
        imports: vec![HashMap::new(); files.len()],
        script_modules,
        inherent: HashMap::new(),
        assoc: HashMap::new(),
        current_type_params: Vec::new(),
        pending_instantiations: Vec::new(),
        trait_impls: HashMap::new(),
        derives: HashMap::new(),
        vtable_cache: HashMap::new(),
        unit_suffixes: HashMap::new(),
        scopes: Vec::new(),
        fn_states: Vec::new(),
        nodes_this_fn: Vec::new(),
        must_resolve: Vec::new(),
        or_depth: 0,
        expr_depth: 0,
        expr_depth_reported: false,
    };
    checker.run();
    checker.out
}

impl<'a> Checker<'a> {
    pub(crate) fn error(&mut self, code: &'static str, span: Span, msg: impl Into<String>) {
        self.out.diags.push(Diagnostic::error(code, span, msg));
    }

    pub(crate) fn error_help(
        &mut self,
        code: &'static str,
        span: Span,
        msg: impl Into<String>,
        help: impl Into<String>,
    ) {
        self.out
            .diags
            .push(Diagnostic::error(code, span, msg).with_help(help));
    }

    pub(crate) fn warn(&mut self, code: &'static str, span: Span, msg: impl Into<String>) {
        self.out.diags.push(Diagnostic::warning(code, span, msg));
    }

    pub(crate) fn ty_str(&self, t: &Type) -> String {
        self.infer.resolve(t).display(&self.out.defs)
    }

    /// The file currently being processed.
    pub(crate) fn file(&self) -> &'a SourceFile {
        self.files[self.cur_file].1
    }

    fn run(&mut self) {
        self.register_host_names();
        // Item passes run per file, whole-pass-at-a-time, so every
        // name exists program-wide before anything is resolved (types
        // are ambient across files; fns and uses are file-scoped).
        for pass in [
            Checker::reject_interface_items,
            Checker::collect_uses,
            Checker::collect_type_names,
            Checker::fill_type_defs,
            Checker::collect_fns,
        ] {
            for fi in 0..self.files.len() {
                self.cur_file = fi;
                pass(self);
            }
        }
        self.validate_script_imports();
        self.collect_methods_by_type();
        self.validate_derives();
        self.check_bodies();
        self.collect_exports();
    }

    /// `mod` blocks, `const` items, bodyless fns and `#[opaque]` are the
    /// `.wscripti` interface grammar (PRD §9.1) — reject them in scripts.
    fn reject_interface_items(&mut self) {
        for item in &self.file().items {
            match item {
                Item::Mod(m) => {
                    let span = m.name.span;
                    self.out.diags.push(
                        Diagnostic::error(
                            "E0270",
                            span,
                            "`mod` blocks are only valid in `.wscripti` interface files",
                        )
                        .with_help(
                            "modules are registered by the host (PRD §3.9); scripts import \
                             them with `use`",
                        ),
                    );
                }
                Item::Const(c) => {
                    let span = c.name.span;
                    self.out.diags.push(
                        Diagnostic::error(
                            "E0270",
                            span,
                            "`const` items are only valid in `.wscripti` interface files",
                        )
                        .with_help("use a `let` binding inside a function instead"),
                    );
                }
                Item::Fn(f) if !f.has_body => {
                    let span = f.sig_span;
                    self.out.diags.push(
                        Diagnostic::error(
                            "E0270",
                            span,
                            format!("function `{}` is missing a body", f.name.name),
                        )
                        .with_help(
                            "bodyless declarations are only valid in `.wscripti` interface files",
                        ),
                    );
                }
                Item::Struct(s) if s.opaque => {
                    let span = s.name.span;
                    self.out.diags.push(
                        Diagnostic::error(
                            "E0270",
                            span,
                            "`#[opaque]` is only valid in `.wscripti` interface files",
                        )
                        .with_help(
                            "opaque types are registered by the host with \
                             #[derive(Script)] #[script(opaque)] (PRD §6.2)",
                        ),
                    );
                }
                Item::Units(u) if !u.derives.is_empty() => {
                    let span = u.name.span;
                    self.out.diags.push(
                        Diagnostic::error(
                            "E0101",
                            span,
                            "`#[derive(...)]` is not allowed on a unit family",
                        )
                        .with_help(
                            "unit values compare, order, clone and display as their \
                             backing number already",
                        ),
                    );
                }
                Item::Impl(im) => {
                    for f in &im.fns {
                        if !f.has_body {
                            let span = f.sig_span;
                            self.out.diags.push(
                                Diagnostic::error(
                                    "E0270",
                                    span,
                                    format!("method `{}` is missing a body", f.name.name),
                                )
                                .with_help(
                                    "bodyless declarations are only valid in `.wscripti` \
                                     interface files",
                                ),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Host-registered types are ambient in the type namespace (modules
    /// namespace functions and constants; types are global).
    fn register_host_names(&mut self) {
        self.type_names.insert("Option".into(), defs::DEF_OPTION);
        self.type_names.insert("Result".into(), defs::DEF_RESULT);
        for (i, def) in self.reg.defs.defs.iter().enumerate() {
            if (i as u32) < defs::FIRST_FREE_DEF {
                // Builtins: Option/Result are referenced by their dedicated
                // Type variants; operator traits are registered by name so
                // `impl Add for T` resolves.
                if let DefKind::Trait(t) = def {
                    self.type_names.insert(t.name.clone(), DefId(i as u32));
                }
                continue;
            }
            let name = self.reg.defs.name_of(DefId(i as u32)).to_string();
            self.type_names.insert(name, DefId(i as u32));
        }
        self.type_names.extend(
            self.reg
                .type_aliases
                .iter()
                .map(|(name, target)| (name.clone(), *target)),
        );
    }

    fn collect_uses(&mut self) {
        for item in &self.file().items {
            let Item::Use(u) = item else { continue };
            // Resolution order (documented): registered host module
            // first, then imported script file. Path-form uses
            // (`use "./x.wscript"`) are script files by construction.
            let host_idx = if u.path_lit.is_some() {
                None
            } else {
                self.reg
                    .modules
                    .iter()
                    .position(|m| m.name == u.module.name)
            };
            let mref = match host_idx {
                Some(idx) => ModuleRef::Host(idx),
                None => match self.script_modules.get(&u.module.name) {
                    Some(&fi) => ModuleRef::Script(fi),
                    None => {
                        if u.path_lit.is_some() {
                            // The loader failed to resolve the file and
                            // already reported it — stay quiet here.
                            continue;
                        }
                        let known: Vec<&str> =
                            self.reg.modules.iter().map(|m| m.name.as_str()).collect();
                        let span = u.module.span;
                        let name = u.module.name.clone();
                        self.error_help(
                            "E0200",
                            span,
                            format!("unknown module `{name}`"),
                            if known.is_empty() {
                                "no modules are registered in this context, and no matching \
                                 script file was found"
                                    .to_string()
                            } else {
                                format!(
                                    "registered modules: {}; script files import by name \
                                     (sibling file or src_roots) or path (`use \"./x.wscript\"`)",
                                    known.join(", ")
                                )
                            },
                        );
                        continue;
                    }
                },
            };
            match &u.item {
                None => {
                    self.modules_in_scope[self.cur_file].insert(u.module.name.clone(), mref);
                }
                Some(item_name) => match mref {
                    ModuleRef::Host(mod_idx) => {
                        let module = &self.reg.modules[mod_idx];
                        if let Some((_, _, idx, _)) =
                            module.fns.iter().find(|(n, ..)| *n == item_name.name)
                        {
                            self.imports[self.cur_file]
                                .insert(item_name.name.clone(), Imported::HostFn(*idx));
                        } else if let Some((_, ty, c)) =
                            module.consts.iter().find(|(n, ..)| *n == item_name.name)
                        {
                            self.imports[self.cur_file].insert(
                                item_name.name.clone(),
                                Imported::Const(ty.clone(), c.clone()),
                            );
                        } else {
                            let span = item_name.span;
                            let msg = format!(
                                "module `{}` has no item `{}`",
                                u.module.name, item_name.name
                            );
                            self.error_help(
                                "E0201",
                                span,
                                msg,
                                "check the module's `.wscripti` interface for available items",
                            );
                        }
                    }
                    ModuleRef::Script(fi) => {
                        // Fn protos are collected after uses — validated
                        // in validate_script_imports, resolved lazily.
                        self.imports[self.cur_file]
                            .insert(item_name.name.clone(), Imported::ScriptItem { file: fi });
                    }
                },
            }
        }
    }

    /// `use module::item` imports of script fns are recorded before fn
    /// collection — verify each names a real fn now.
    fn validate_script_imports(&mut self) {
        let mut errors = Vec::new();
        for file_imports in &self.imports {
            for (name, imp) in file_imports {
                if let Imported::ScriptItem { file } = imp
                    && !self.fn_names[*file].contains_key(name)
                {
                    errors.push((self.files[*file].0.clone(), name.clone()));
                }
            }
        }
        for (module, name) in errors {
            // Best-effort span: search every file's use items.
            let span = self
                .files
                .iter()
                .flat_map(|(_, f)| &f.items)
                .find_map(|i| match i {
                    Item::Use(u) if u.item.as_ref().is_some_and(|it| it.name == name) => {
                        Some(u.span)
                    }
                    _ => None,
                })
                .unwrap_or(Span::DUMMY);
            self.error_help(
                "E0201",
                span,
                format!("script module `{module}` has no function `{name}`"),
                "only top-level fns can be imported from script files",
            );
        }
    }

    fn collect_type_names(&mut self) {
        for item in &self.file().items {
            let (name, span, kind) = match item {
                Item::Struct(s) => (
                    &s.name,
                    s.span,
                    DefKind::Struct(StructDef {
                        name: s.name.name.clone(),
                        fields: vec![],
                        opaque: false,
                        host: false,
                        rust_type: None,
                    }),
                ),
                Item::Enum(e) => (
                    &e.name,
                    e.span,
                    DefKind::Enum(EnumDef {
                        name: e.name.name.clone(),
                        variants: vec![],
                        host: false,
                        rust_type: None,
                    }),
                ),
                Item::Trait(t) => (
                    &t.name,
                    t.span,
                    DefKind::Trait(TraitDef {
                        name: t.name.name.clone(),
                        methods: vec![],
                        operator: false,
                    }),
                ),
                Item::Units(u) => (
                    &u.name,
                    u.span,
                    DefKind::Unit(UnitDef {
                        name: u.name.name.clone(),
                        base: Type::Int,
                        base_unit: 0,
                        units: vec![],
                    }),
                ),
                _ => continue,
            };
            if self.type_names.contains_key(&name.name)
                || matches!(
                    name.name.as_str(),
                    "int"
                        | "float"
                        | "bool"
                        | "char"
                        | "unit"
                        | "string"
                        | "List"
                        | "Map"
                        | "Option"
                        | "Result"
                        | "weak"
                )
            {
                let span = name.span;
                let n = name.name.clone();
                self.error_help(
                    "E0202",
                    span,
                    format!("the type name `{n}` is already defined"),
                    "type names must be unique across the script, host-registered types, \
                     and built-ins",
                );
                continue;
            }
            let _ = span;
            let id = self.out.defs.push(kind);
            self.type_names.insert(name.name.clone(), id);
        }
    }

    /// Resolve field/variant/trait-method types now that all names exist.
    fn fill_type_defs(&mut self) {
        for item in &self.file().items {
            match item {
                Item::Struct(s) => {
                    if let Some(id) = self.script_def_id(&s.name.name) {
                        self.fill_struct_def(id, s);
                    }
                }
                Item::Enum(e) => {
                    if let Some(id) = self.script_def_id(&e.name.name) {
                        self.fill_enum_def(id, e);
                    }
                }
                Item::Trait(t) => {
                    if let Some(id) = self.script_def_id(&t.name.name) {
                        self.fill_trait_def(id, t);
                    }
                }
                Item::Units(u) => {
                    if let Some(id) = self.script_def_id(&u.name.name) {
                        self.fill_unit_def(id, u);
                    }
                }
                _ => {}
            }
        }
    }

    /// Const-evaluate a unit family's factors and index its suffixes.
    ///
    /// Factors are resolved in declaration order, so later units can be
    /// written in terms of earlier ones (`s = 1_000 * ms`).
    fn fill_unit_def(&mut self, id: DefId, decl: &UnitsDecl) {
        let base = self.resolve_type(&decl.base);
        let base = match base {
            Type::Int | Type::Float => base,
            Type::Error => Type::Int,
            other => {
                let ts = self.ty_str(&other);
                self.error_help(
                    "E0264",
                    decl.base.span,
                    format!("a unit family must be backed by `int` or `float`, found `{ts}`"),
                    "unit values are stored as their backing number, so only the two \
                     numeric types work",
                );
                Type::Int
            }
        };
        let is_float = base == Type::Float;

        let mut units: Vec<(String, Factor)> = Vec::new();
        let mut seen: HashMap<String, Span> = HashMap::new();
        for entry in &decl.units {
            if seen.contains_key(&entry.name.name) {
                let n = entry.name.name.clone();
                self.error_help(
                    "E0265",
                    entry.name.span,
                    format!("duplicate unit `{n}`"),
                    "each unit may be declared once per family",
                );
                continue;
            }
            let Some(factor) = self.eval_factor(&entry.factor, is_float, &units) else {
                continue;
            };
            let non_positive = match factor {
                Factor::Int(n) => n <= 0,
                // `!is_finite` first, so NaN counts as non-positive too.
                Factor::Float(f) => !f.is_finite() || f <= 0.0,
            };
            if non_positive {
                let n = entry.name.name.clone();
                self.error_help(
                    "E0266",
                    entry.factor.span,
                    format!("the factor for `{n}` must be a positive, finite number"),
                    "a factor says how many base units one of this unit is worth",
                );
                continue;
            }
            seen.insert(entry.name.name.clone(), entry.name.span);
            units.push((entry.name.name.clone(), factor));
        }

        // Exactly one unit is the base — the one worth a single base unit.
        let ones: Vec<usize> = units
            .iter()
            .enumerate()
            .filter(|(_, (_, f))| f.is_one())
            .map(|(i, _)| i)
            .collect();
        let base_unit = match ones.as_slice() {
            [i] => *i,
            [] => {
                let n = decl.name.name.clone();
                self.error_help(
                    "E0267",
                    decl.name.span,
                    format!("unit family `{n}` has no base unit"),
                    "exactly one unit must have the factor 1 — values are stored in it",
                );
                if units.is_empty() {
                    return;
                }
                0
            }
            more => {
                let names: Vec<String> =
                    more.iter().map(|i| format!("`{}`", units[*i].0)).collect();
                let n = decl.name.name.clone();
                self.error_help(
                    "E0267",
                    decl.name.span,
                    format!("unit family `{n}` has more than one base unit"),
                    format!(
                        "{} all have the factor 1; exactly one may",
                        names.join(", ")
                    ),
                );
                more[0]
            }
        };

        for (name, _) in &units {
            self.unit_suffixes.entry(name.clone()).or_default().push(id);
        }
        if let DefKind::Unit(u) = &mut self.out.defs.defs[id.index()] {
            u.base = base;
            u.base_unit = base_unit;
            u.units = units;
        }
    }

    /// Const-evaluate one conversion factor: numeric literals, the units
    /// already declared in this family, and `+ - * /` over them.
    fn eval_factor(
        &mut self,
        e: &Expr,
        is_float: bool,
        so_far: &[(String, Factor)],
    ) -> Option<Factor> {
        let unsupported = |c: &mut Self| {
            c.error_help(
                "E0268",
                e.span,
                "a conversion factor must be a constant expression",
                "use numeric literals, units declared earlier in this family, and \
                 `+ - * /`",
            );
            None
        };
        match &e.kind {
            ExprKind::IntLit(n) => Some(if is_float {
                Factor::Float(*n as f64)
            } else {
                Factor::Int(*n)
            }),
            ExprKind::FloatLit(f) => {
                if is_float {
                    Some(Factor::Float(*f))
                } else {
                    self.error_help(
                        "E0268",
                        e.span,
                        "a float factor in an `int`-backed unit family",
                        "int-backed families take whole factors; back the family with \
                         `float` if you need fractions",
                    );
                    None
                }
            }
            ExprKind::Unary {
                op: UnOp::Neg,
                expr,
            } => match self.eval_factor(expr, is_float, so_far)? {
                Factor::Int(n) => Some(Factor::Int(-n)),
                Factor::Float(f) => Some(Factor::Float(-f)),
            },
            ExprKind::Path(segs) if segs.len() == 1 => {
                match so_far.iter().find(|(n, _)| *n == segs[0].name) {
                    Some((_, f)) => Some(*f),
                    None => {
                        let n = segs[0].name.clone();
                        self.error_help(
                            "E0268",
                            e.span,
                            format!("unknown unit `{n}` in a conversion factor"),
                            "only units declared earlier in the same family can be \
                             referenced",
                        );
                        None
                    }
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let a = self.eval_factor(lhs, is_float, so_far)?;
                let b = self.eval_factor(rhs, is_float, so_far)?;
                match (a, b) {
                    (Factor::Int(x), Factor::Int(y)) => {
                        let r = match op {
                            BinOp::Add => x.checked_add(y),
                            BinOp::Sub => x.checked_sub(y),
                            BinOp::Mul => x.checked_mul(y),
                            BinOp::Div => x.checked_div(y),
                            _ => return unsupported(self),
                        };
                        match r {
                            Some(v) => Some(Factor::Int(v)),
                            None => {
                                self.error_help(
                                    "E0266",
                                    e.span,
                                    "this conversion factor overflows `int`",
                                    "factors are stored in the family's backing type",
                                );
                                None
                            }
                        }
                    }
                    (Factor::Float(x), Factor::Float(y)) => Some(Factor::Float(match op {
                        BinOp::Add => x + y,
                        BinOp::Sub => x - y,
                        BinOp::Mul => x * y,
                        BinOp::Div => x / y,
                        _ => return unsupported(self),
                    })),
                    _ => unsupported(self),
                }
            }
            _ => unsupported(self),
        }
    }

    /// The def this script declaration owns, if any — `None` when the
    /// name didn't register or name-collision left it pointing at a host
    /// def.
    fn script_def_id(&self, name: &str) -> Option<DefId> {
        let id = *self.type_names.get(name)?;
        self.owns_script_def(id).then_some(id)
    }

    /// Resolve named fields, dropping duplicates with an E0203. Shared by
    /// struct declarations and struct-variant bodies.
    fn resolve_field_defs(&mut self, fields: &[FieldDecl]) -> Vec<(String, Type)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for f in fields {
            if !seen.insert(f.name.name.clone()) {
                let span = f.name.span;
                let msg = format!("duplicate field `{}`", f.name.name);
                self.error("E0203", span, msg);
                continue;
            }
            let ty = self.resolve_type(&f.ty);
            out.push((f.name.name.clone(), ty));
        }
        out
    }

    fn fill_struct_def(&mut self, id: DefId, s: &StructDecl) {
        let fields = self.resolve_field_defs(&s.fields);
        if let DefKind::Struct(sd) = &mut self.out.defs.defs[id.index()] {
            sd.fields = fields;
        }
        self.record_derives(id, &s.derives);
    }

    fn fill_enum_def(&mut self, id: DefId, e: &EnumDecl) {
        let mut variants = Vec::new();
        let mut seen = HashSet::new();
        for v in &e.variants {
            if !seen.insert(v.name.name.clone()) {
                let span = v.name.span;
                let msg = format!("duplicate variant `{}`", v.name.name);
                self.error("E0203", span, msg);
                continue;
            }
            let (kind, fields) = match &v.body {
                VariantBody::Unit => (VariantKind::Unit, vec![]),
                VariantBody::Tuple(tys) => (
                    VariantKind::Tuple,
                    tys.iter()
                        .enumerate()
                        .map(|(i, t)| (i.to_string(), self.resolve_type(t)))
                        .collect(),
                ),
                VariantBody::Struct(fs) => (VariantKind::Struct, self.resolve_field_defs(fs)),
            };
            variants.push(VariantDef {
                name: v.name.name.clone(),
                kind,
                fields,
            });
        }
        if let DefKind::Enum(ed) = &mut self.out.defs.defs[id.index()] {
            ed.variants = variants;
        }
        self.record_derives(id, &e.derives);
    }

    fn fill_trait_def(&mut self, id: DefId, t: &TraitDecl) {
        let mut methods = Vec::new();
        let mut seen = HashSet::new();
        for m in &t.methods {
            if !seen.insert(m.name.name.clone()) {
                let span = m.name.span;
                let msg = format!("duplicate trait method `{}`", m.name.name);
                self.error("E0203", span, msg);
                continue;
            }
            let params: Vec<Type> = m
                .params
                .iter()
                .map(|p| match &p.ty {
                    Some(t) => self.resolve_type(t),
                    None => Type::Error,
                })
                .collect();
            let ret = match &m.ret {
                Some(t) => self.resolve_type(t),
                None => Type::Unit,
            };
            methods.push((m.name.name.clone(), FnSig::new(params, ret)));
        }
        if let DefKind::Trait(td) = &mut self.out.defs.defs[id.index()] {
            td.methods = methods;
        }
    }

    /// Guard against filling in a def that name-collision left pointing at
    /// a host def.
    fn owns_script_def(&self, id: DefId) -> bool {
        id.0 as usize >= self.reg.defs.len()
    }

    fn record_derives(&mut self, id: DefId, derives: &[Ident]) {
        let mut d = Derives::default();
        for ident in derives {
            match ident.name.as_str() {
                "Eq" => d.eq = true,
                "Ord" => d.ord = true,
                "Display" => d.display = true,
                "Clone" => d.clone = true,
                other => {
                    let span = ident.span;
                    let msg = format!("cannot derive `{other}`");
                    self.error_help(
                        "E0204",
                        span,
                        msg,
                        "derivable traits are: Eq, Ord, Display, Clone (PRD §3.8)",
                    );
                }
            }
        }
        if d.ord && !d.eq {
            // Ord implies Eq for usability; require both to be explicit.
            let span = derives
                .iter()
                .find(|i| i.name == "Ord")
                .map(|i| i.span)
                .unwrap_or(Span::DUMMY);
            self.error_help(
                "E0204",
                span,
                "deriving `Ord` also requires deriving `Eq`",
                "write `#[derive(Eq, Ord)]`",
            );
        }
        self.derives.insert(id, d);
    }

    /// Collect all function signatures (top-level fns, impl methods) and
    /// validate impl blocks. Bodies are checked afterwards so calls can
    /// reference any function regardless of declaration order.
    fn collect_fns(&mut self) {
        // Top-level functions.
        for (item_idx, item) in self.file().items.iter().enumerate() {
            let Item::Fn(f) = item else { continue };
            if self.fn_names[self.cur_file].contains_key(&f.name.name) {
                let span = f.name.span;
                let msg = format!("duplicate function `{}`", f.name.name);
                self.error("E0205", span, msg);
                continue;
            }
            // Generic fns: validate the type-parameter list and resolve
            // the signature with those params in scope.
            let type_params = self.collect_type_params(f);
            self.current_type_params = type_params.clone();
            let sig = self.fn_decl_sig(f, None);
            self.current_type_params.clear();
            for (i, (name, _)) in type_params.iter().enumerate() {
                if !sig_mentions_param(&sig, i as u32) {
                    let span = f
                        .type_params
                        .get(i)
                        .map(|tp| tp.name.span)
                        .unwrap_or(f.sig_span);
                    self.error_help(
                        "E0255",
                        span,
                        format!("type parameter `{name}` is never used in the signature"),
                        "every type parameter must appear in a parameter type or the \
                         return type (there is no explicit instantiation syntax)",
                    );
                }
            }
            let proto = self.out.fn_infos.len() as u32;
            self.out.fn_infos.push(FnInfo {
                name: f.name.name.clone(),
                sig,
                type_params,
                source: FnSource::Top {
                    file: self.cur_file,
                    item: item_idx,
                },
                n_locals: 0,
                captured: HashSet::new(),
                captures: Vec::new(),
                span: f.sig_span,
                pending: true,
            });
            self.fn_names[self.cur_file].insert(f.name.name.clone(), proto);
        }
        // Impl blocks.
        for (item_idx, item) in self.file().items.iter().enumerate() {
            let Item::Impl(im) = item else { continue };
            self.collect_impl(item_idx, im);
        }
    }

    /// Validate a fn's declared type parameters → (name, bound) list.
    fn collect_type_params(&mut self, f: &FnDecl) -> Vec<(String, Option<BoundKind>)> {
        let mut out: Vec<(String, Option<BoundKind>)> = Vec::new();
        for tp in &f.type_params {
            if out.iter().any(|(n, _)| *n == tp.name.name) {
                let span = tp.name.span;
                let msg = format!("duplicate type parameter `{}`", tp.name.name);
                self.error("E0255", span, msg);
                continue;
            }
            if self.type_names.contains_key(&tp.name.name) {
                let span = tp.name.span;
                let msg = format!("type parameter `{}` shadows an existing type", tp.name.name);
                self.error_help("E0255", span, msg, "rename the type parameter");
                // Declared anyway so the body doesn't cascade.
            }
            let bound = match &tp.bound {
                None => None,
                Some(b) => match b.name.as_str() {
                    "Eq" => Some(BoundKind::Eq),
                    "Ord" => Some(BoundKind::Ord),
                    "Clone" => Some(BoundKind::Clone),
                    other => {
                        let span = b.span;
                        let msg = format!("unsupported bound `{other}`");
                        self.error_help(
                            "E0254",
                            span,
                            msg,
                            "only the built-in bounds `Eq`, `Ord` and `Clone` are \
                             supported in this release",
                        );
                        None
                    }
                },
            };
            out.push((tp.name.name.clone(), bound));
        }
        out
    }

    fn collect_impl(&mut self, item_idx: usize, im: &ImplDecl) {
        let Some(&ty_id) = self.type_names.get(&im.ty_name.name) else {
            let span = im.ty_name.span;
            let msg = format!("unknown type `{}`", im.ty_name.name);
            self.error("E0206", span, msg);
            return;
        };
        if ty_id.0 < defs::FIRST_FREE_DEF {
            let span = im.ty_name.span;
            let msg = format!(
                "cannot write an impl block for built-in type `{}`",
                im.ty_name.name
            );
            self.error("E0206", span, msg);
            return;
        }
        match self.out.defs.get(ty_id) {
            DefKind::Struct(s) if !s.host => {}
            DefKind::Enum(e) if !e.host => {}
            DefKind::Trait(_) => {
                let span = im.ty_name.span;
                let msg = format!("cannot write an impl block for trait `{}`", im.ty_name.name);
                self.error_help(
                    "E0206",
                    span,
                    msg,
                    "impl blocks target struct or enum types",
                );
                return;
            }
            // Unit families get their operators from the built-in
            // arithmetic rules, so a user operator impl would silently
            // never be called. `Display` is the one worth overriding, and
            // inherent methods are free.
            DefKind::Unit(u) => {
                let units: Vec<String> = u.units.iter().map(|(n, _)| n.clone()).collect();
                if let Some(tr) = &im.trait_name
                    && tr.name != "Display"
                {
                    let (span, ty, tn) = (tr.span, im.ty_name.name.clone(), tr.name.clone());
                    self.error_help(
                        "E0206",
                        span,
                        format!("cannot implement `{tn}` for the unit family `{ty}`"),
                        format!(
                            "`{ty}` already has arithmetic, comparison and equality from \
                             its backing number; only `Display` can be overridden"
                        ),
                    );
                    return;
                }
                for f in &im.fns {
                    if units.contains(&f.name.name) {
                        let (span, n, ty) =
                            (f.name.span, f.name.name.clone(), im.ty_name.name.clone());
                        self.error_help(
                            "E0206",
                            span,
                            format!("`{n}` is a unit of `{ty}` and cannot also be a method"),
                            format!("`{ty}::{n}(n)` already converts a number into `{ty}`"),
                        );
                        return;
                    }
                }
            }
            _ => {
                let span = im.ty_name.span;
                let msg = format!(
                    "cannot write an impl block for host-registered type `{}`",
                    im.ty_name.name
                );
                self.error_help(
                    "E0206",
                    span,
                    msg,
                    "methods on host types are registered from Rust (PRD §6.2); \
                     v1 scripts can only implement methods on script-declared types",
                );
                return;
            }
        }
        let self_ty = Type::Named(ty_id);

        // Allocate protos for every method in the block. No-self fns in
        // inherent impls are associated functions (`Type::func(...)`);
        // trait impls still require `self` on every method.
        let mut method_protos: Vec<(String, u32, &FnDecl)> = Vec::new();
        let mut assoc_protos: Vec<(String, u32, &FnDecl)> = Vec::new();
        for (fn_idx, f) in im.fns.iter().enumerate() {
            if !f.type_params.is_empty() {
                let span = f.name.span;
                self.error_help(
                    "E0254",
                    span,
                    "generic methods are not supported yet",
                    "only top-level `fn`s take type parameters in this release",
                );
                continue;
            }
            let has_self = f.params.first().is_some_and(|p| p.is_self);
            if !has_self && im.trait_name.is_some() {
                let span = f.name.span;
                let msg = format!(
                    "trait method `{}` must take `self` as its first parameter",
                    f.name.name
                );
                self.error_help(
                    "E0207",
                    span,
                    msg,
                    "trait impls provide methods only; associated functions go in an \
                     inherent `impl` block",
                );
                continue;
            }
            let sig = self.fn_decl_sig(f, Some(self_ty.clone()));
            let proto = self.out.fn_infos.len() as u32;
            self.out.fn_infos.push(FnInfo {
                name: format!("{}::{}", im.ty_name.name, f.name.name),
                sig,
                type_params: Vec::new(),
                source: FnSource::Method {
                    file: self.cur_file,
                    item: item_idx,
                    fn_idx,
                },
                n_locals: 0,
                captured: HashSet::new(),
                captures: Vec::new(),
                span: f.sig_span,
                pending: true,
            });
            if has_self {
                method_protos.push((f.name.name.clone(), proto, f));
            } else {
                assoc_protos.push((f.name.name.clone(), proto, f));
            }
        }

        match &im.trait_name {
            None => {
                // Inherent impl. Methods and associated functions share a
                // namespace (like Rust) — duplicates across either are
                // errors.
                let mut dups = Vec::new();
                {
                    let table = self.inherent.entry(ty_id).or_default();
                    for (name, proto, f) in &method_protos {
                        if table.insert(name.clone(), *proto).is_some() {
                            dups.push((f.name.span, name.clone()));
                        }
                    }
                }
                {
                    let atable = self.assoc.entry(ty_id).or_default();
                    for (name, proto, f) in &assoc_protos {
                        if atable.insert(name.clone(), *proto).is_some() {
                            dups.push((f.name.span, name.clone()));
                        }
                    }
                }
                let mut seen = HashSet::new();
                let cross: Vec<(Span, String)> = assoc_protos
                    .iter()
                    .chain(method_protos.iter())
                    .filter(|(name, ..)| {
                        let in_both = self
                            .inherent
                            .get(&ty_id)
                            .is_some_and(|t| t.contains_key(name))
                            && self.assoc.get(&ty_id).is_some_and(|t| t.contains_key(name));
                        in_both && seen.insert(name.clone())
                    })
                    .map(|(name, _, f)| (f.name.span, name.clone()))
                    .collect();
                for (span, name) in dups {
                    let msg = format!("duplicate method `{name}` for `{}`", im.ty_name.name);
                    self.error("E0205", span, msg);
                }
                for (span, name) in cross {
                    let msg = format!(
                        "`{name}` is declared both with and without `self` for `{}`",
                        im.ty_name.name
                    );
                    self.error("E0205", span, msg);
                }
            }
            Some(trait_ident) => {
                let Some(&trait_id) = self.type_names.get(&trait_ident.name) else {
                    let span = trait_ident.span;
                    let msg = format!("unknown trait `{}`", trait_ident.name);
                    self.error("E0206", span, msg);
                    return;
                };
                let Some(td) = self.out.defs.as_trait(trait_id).cloned() else {
                    let span = trait_ident.span;
                    let msg = format!("`{}` is not a trait", trait_ident.name);
                    self.error("E0206", span, msg);
                    return;
                };
                if self.trait_impls.contains_key(&(ty_id, trait_id)) {
                    let span = im.span;
                    let msg = format!(
                        "duplicate impl of `{}` for `{}`",
                        trait_ident.name, im.ty_name.name
                    );
                    self.error("E0205", span, msg);
                    return;
                }
                // Match provided methods against the trait's declarations.
                let mut slots: Vec<Option<u32>> = vec![None; td.methods.len()];
                for (name, proto, f) in &method_protos {
                    let Some(slot) = td.methods.iter().position(|(n, _)| n == name) else {
                        let span = f.name.span;
                        let msg = format!(
                            "method `{name}` is not a member of trait `{}`",
                            trait_ident.name
                        );
                        self.error("E0208", span, msg);
                        continue;
                    };
                    // Signature must match the trait declaration; for
                    // operator traits `Self` (Param 0) is the impl type and
                    // Param(1)+ are impl-chosen (Index).
                    let expected = &td.methods[slot].1;
                    let actual = &self.out.fn_infos[*proto as usize].sig;
                    let subst: Vec<Type> = if td.operator {
                        // Param(0) = Self. Param(1)/Param(2) are
                        // impl-chosen (Index's key and output types): bind
                        // them to whatever the impl declared so the shape
                        // check only enforces structure.
                        let p1 = expected
                            .params
                            .iter()
                            .position(|p| *p == Type::Param(1))
                            .and_then(|i| actual.params.get(1 + i).cloned());
                        let p2 = if expected.ret == Type::Param(2) {
                            Some(actual.ret.clone())
                        } else {
                            None
                        };
                        vec![
                            self_ty.clone(),
                            p1.unwrap_or(Type::Error),
                            p2.unwrap_or(Type::Error),
                        ]
                    } else {
                        vec![]
                    };
                    let exp_params: Vec<Type> = expected
                        .params
                        .iter()
                        .map(|p| subst_params(p, &subst))
                        .collect();
                    let exp_ret = subst_params(&expected.ret, &subst);
                    // actual.params[0] is the receiver.
                    let actual_params = &actual.params[1..];
                    if actual_params != exp_params.as_slice() || actual.ret != exp_ret {
                        let span = f.sig_span;
                        let exp_str = FnSig::new(exp_params.clone(), exp_ret.clone());
                        let msg = format!("method `{name}` does not match the trait signature");
                        let help = format!(
                            "trait `{}` declares `fn {name}({}){}`",
                            trait_ident.name,
                            exp_str
                                .params
                                .iter()
                                .map(|p| p.display(&self.out.defs))
                                .collect::<Vec<_>>()
                                .join(", "),
                            if exp_str.ret == Type::Unit {
                                String::new()
                            } else {
                                format!(" -> {}", exp_str.ret.display(&self.out.defs))
                            }
                        );
                        self.error_help("E0208", span, msg, help);
                    }
                    slots[slot] = Some(*proto);
                }
                let mut protos = Vec::with_capacity(slots.len());
                let mut complete = true;
                for (slot, p) in slots.iter().enumerate() {
                    match p {
                        Some(p) => protos.push(*p),
                        None => {
                            complete = false;
                            let span = im.span;
                            let (mname, _) = &td.methods[slot];
                            let msg = format!(
                                "missing method `{mname}` in impl of `{}` for `{}`",
                                trait_ident.name, im.ty_name.name
                            );
                            self.error("E0208", span, msg);
                        }
                    }
                }
                if complete {
                    // Operator impls feed the runtime structural-op tables.
                    if td.operator {
                        let proto = protos[0];
                        match trait_ident.name.as_str() {
                            "Eq" => {
                                self.out.impl_maps.eq.insert(ty_id.0, proto);
                            }
                            "Ord" => {
                                self.out.impl_maps.cmp.insert(ty_id.0, proto);
                            }
                            "Display" => {
                                self.out.impl_maps.display.insert(ty_id.0, proto);
                            }
                            _ => {}
                        }
                    }
                    self.trait_impls.insert((ty_id, trait_id), protos);
                }
            }
        }
    }

    fn fn_decl_sig(&mut self, f: &FnDecl, self_ty: Option<Type>) -> FnSig {
        let mut params = Vec::new();
        for p in &f.params {
            if p.is_self {
                match &self_ty {
                    Some(t) => params.push(t.clone()),
                    None => params.push(Type::Error),
                }
                continue;
            }
            match &p.ty {
                Some(t) => params.push(self.resolve_type(t)),
                None => params.push(Type::Error), // parser already reported
            }
        }
        let ret = match &f.ret {
            Some(t) => self.resolve_type(t),
            None => Type::Unit,
        };
        FnSig::new(params, ret)
    }

    fn collect_methods_by_type(&mut self) {
        for (&def, table) in &self.inherent {
            let entry = self.out.methods_by_type.entry(def).or_default();
            for (name, &proto) in table {
                entry.push((name.clone(), self.out.fn_infos[proto as usize].sig.clone()));
            }
        }
        for (&(def, _trait_id), protos) in &self.trait_impls {
            let entry = self.out.methods_by_type.entry(def).or_default();
            for &proto in protos {
                let info = &self.out.fn_infos[proto as usize];
                let name = info
                    .name
                    .rsplit("::")
                    .next()
                    .unwrap_or(&info.name)
                    .to_string();
                entry.push((name, info.sig.clone()));
            }
        }
        for methods in self.out.methods_by_type.values_mut() {
            methods.sort_by(|a, b| a.0.cmp(&b.0));
        }
    }

    fn validate_derives(&mut self) {
        let entries: Vec<(DefId, Derives)> = self.derives.iter().map(|(k, v)| (*k, *v)).collect();
        for (id, d) in entries {
            let span = self.def_decl_span(id);
            if d.eq && !self.fields_satisfy(id, |c, t| c.eq_able(t)) {
                let name = self.out.defs.name_of(id).to_string();
                self.error_help(
                    "E0209",
                    span,
                    format!("cannot derive `Eq` for `{name}`: not all fields support `==`"),
                    "every field must be a primitive, string, container of comparable \
                     values, or a type that itself implements Eq",
                );
            }
            if d.ord && !self.fields_satisfy(id, |c, t| c.ord_able(t)) {
                let name = self.out.defs.name_of(id).to_string();
                self.error_help(
                    "E0209",
                    span,
                    format!("cannot derive `Ord` for `{name}`: not all fields are orderable"),
                    "every field must be int, float, char, string, or a type that \
                     implements Ord",
                );
            }
            if d.clone && !self.fields_satisfy(id, |c, t| c.clone_able(t)) {
                let name = self.out.defs.name_of(id).to_string();
                self.error_help(
                    "E0209",
                    span,
                    format!("cannot derive `Clone` for `{name}`: a field cannot be deep-cloned"),
                    "opaque host handles cannot be cloned",
                );
            }
        }
    }

    fn def_decl_span(&self, id: DefId) -> Span {
        for item in &self.file().items {
            match item {
                Item::Struct(s) if self.type_names.get(&s.name.name) == Some(&id) => {
                    return s.name.span;
                }
                Item::Enum(e) if self.type_names.get(&e.name.name) == Some(&id) => {
                    return e.name.span;
                }
                _ => {}
            }
        }
        Span::DUMMY
    }

    fn fields_satisfy(&self, id: DefId, pred: impl Fn(&Self, &Type) -> bool) -> bool {
        match self.out.defs.get(id) {
            DefKind::Struct(s) => s.fields.iter().all(|(_, t)| pred(self, t)),
            DefKind::Enum(e) => e
                .variants
                .iter()
                .all(|v| v.fields.iter().all(|(_, t)| pred(self, t))),
            DefKind::Trait(_) | DefKind::Unit(_) => false,
        }
    }

    /// Can `==` be used on this type? (PRD §3.7: structs/enums require Eq.)
    pub(crate) fn eq_able(&self, t: &Type) -> bool {
        match t {
            Type::Int | Type::Float | Type::Bool | Type::Char | Type::Unit | Type::Str => true,
            Type::List(e) | Type::Option(e) => self.eq_able(e),
            Type::Map(_, v) => self.eq_able(v),
            Type::Result(a, b) => self.eq_able(a) && self.eq_able(b),
            Type::Named(id) => self.named_has_eq(*id),
            Type::Param(i) => self.param_has_bound(*i, BoundKind::Eq),
            Type::Error => true,
            _ => false,
        }
    }

    /// Does the in-scope type parameter `i` carry (at least) `need`?
    /// `Ord` implies `Eq`.
    pub(crate) fn param_has_bound(&self, i: u32, need: BoundKind) -> bool {
        match self
            .current_type_params
            .get(i as usize)
            .and_then(|(_, b)| *b)
        {
            Some(BoundKind::Ord) => matches!(need, BoundKind::Ord | BoundKind::Eq),
            Some(b) => b == need,
            None => false,
        }
    }

    /// The declared name of the in-scope type parameter `i` (for
    /// diagnostics), falling back to the display letter.
    pub(crate) fn param_name(&self, i: u32) -> String {
        self.current_type_params
            .get(i as usize)
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| Type::Param(i).display(&self.out.defs))
    }

    /// Report E0253 unless `t` satisfies `bound` (a generic call's
    /// resolved instantiation).
    pub(crate) fn check_bound_satisfied(
        &mut self,
        span: Span,
        fn_name: &str,
        pname: &str,
        bound: BoundKind,
        t: &Type,
    ) {
        if matches!(t, Type::Error | Type::Never) {
            return;
        }
        let ok = match bound {
            BoundKind::Eq => self.eq_able(t),
            BoundKind::Ord => self.ord_able(t),
            BoundKind::Clone => self.clone_able(t),
        };
        if !ok {
            let ts = self.ty_str(t);
            let (bname, fix) = match bound {
                BoundKind::Eq => ("Eq", "add `#[derive(Eq)]` or `impl Eq for` the type"),
                BoundKind::Ord => ("Ord", "add `#[derive(Eq, Ord)]` or `impl Ord for` the type"),
                BoundKind::Clone => ("Clone", "add `#[derive(Clone)]` to the type"),
            };
            self.error_help(
                "E0253",
                span,
                format!(
                    "`{ts}` does not satisfy the bound `{pname}: {bname}` required by \
                     `{fn_name}`"
                ),
                fix,
            );
        }
    }

    pub(crate) fn named_has_eq(&self, id: DefId) -> bool {
        if self.derives.get(&id).is_some_and(|d| d.eq) || self.out.impl_maps.eq.contains_key(&id.0)
        {
            return true;
        }
        // Host data types compare structurally (they are plain data).
        match self.out.defs.get(id) {
            DefKind::Struct(s) => s.host && !s.opaque,
            DefKind::Enum(e) => e.host,
            // Unit families compare as their backing primitive.
            DefKind::Unit(_) => true,
            DefKind::Trait(_) => false,
        }
    }

    pub(crate) fn ord_able(&self, t: &Type) -> bool {
        match t {
            Type::Int | Type::Float | Type::Char | Type::Str | Type::Bool | Type::Unit => true,
            Type::List(e) | Type::Option(e) => self.ord_able(e),
            Type::Result(a, b) => self.ord_able(a) && self.ord_able(b),
            Type::Param(i) => self.param_has_bound(*i, BoundKind::Ord),
            Type::Named(id) => {
                self.out.defs.is_quantity(*id)
                    || self.derives.get(id).is_some_and(|d| d.ord)
                    || self.out.impl_maps.cmp.contains_key(&id.0)
            }
            Type::Error => true,
            _ => false,
        }
    }

    pub(crate) fn clone_able(&self, t: &Type) -> bool {
        match t {
            Type::Param(i) => self.param_has_bound(*i, BoundKind::Clone),
            Type::Named(id) => match self.out.defs.get(*id) {
                DefKind::Struct(s) => !s.opaque,
                DefKind::Enum(_) | DefKind::Unit(_) => true,
                DefKind::Trait(_) => false,
            },
            Type::List(e) | Type::Option(e) | Type::Weak(e) => self.clone_able(e),
            Type::Map(_, v) => self.clone_able(v),
            Type::Result(a, b) => self.clone_able(a) && self.clone_able(b),
            Type::Dyn(_) => false,
            _ => true,
        }
    }

    // ------------------------------------------------------------- types

    pub(crate) fn resolve_type(&mut self, t: &TypeExpr) -> Type {
        match &t.kind {
            TypeExprKind::Unit => Type::Unit,
            TypeExprKind::Error => Type::Error,
            TypeExprKind::Name(ident) => match ident.name.as_str() {
                "int" => Type::Int,
                "float" => Type::Float,
                "bool" => Type::Bool,
                "char" => Type::Char,
                "unit" => Type::Unit,
                "string" => Type::Str,
                "List" | "Map" | "Option" | "Result" | "weak" => {
                    let span = t.span;
                    let name = ident.name.clone();
                    let arity = match ident.name.as_str() {
                        "Map" | "Result" => 2,
                        _ => 1,
                    };
                    self.error_help(
                        "E0210",
                        span,
                        format!("`{name}` requires type arguments"),
                        format!(
                            "write `{name}[{}]`",
                            (0..arity).map(|_| "T").collect::<Vec<_>>().join(", ")
                        ),
                    );
                    Type::Error
                }
                other => {
                    // In-scope generic type parameters resolve first
                    // (shadowing an existing type name is an error at the
                    // declaration, so no ambiguity survives here).
                    if let Some(i) = self
                        .current_type_params
                        .iter()
                        .position(|(n, _)| n == other)
                    {
                        return Type::Param(i as u32);
                    }
                    match self.type_names.get(other) {
                        Some(&id) => match self.out.defs.get(id) {
                            DefKind::Trait(_) => {
                                let span = ident.span;
                                let msg =
                                    format!("trait `{other}` cannot be used as a type directly");
                                self.error_help(
                                    "E0211",
                                    span,
                                    msg,
                                    format!("use `dyn {other}` for a dynamically dispatched value"),
                                );
                                Type::Error
                            }
                            _ => Type::Named(id),
                        },
                        None => {
                            let span = ident.span;
                            let msg = format!("unknown type `{other}`");
                            self.error("E0212", span, msg);
                            Type::Error
                        }
                    }
                }
            },
            TypeExprKind::App(ident, args) => {
                let mut arg_tys: Vec<Type> = args.iter().map(|a| self.resolve_type(a)).collect();
                let expect = |me: &mut Self, n: usize, arg_tys: &mut Vec<Type>| {
                    if arg_tys.len() != n {
                        let span = t.span;
                        let msg = format!(
                            "`{}` takes {n} type argument{}, found {}",
                            ident.name,
                            if n == 1 { "" } else { "s" },
                            arg_tys.len()
                        );
                        me.error("E0210", span, msg);
                        arg_tys.resize(n, Type::Error);
                    }
                };
                match ident.name.as_str() {
                    "List" => {
                        expect(self, 1, &mut arg_tys);
                        Type::List(Box::new(arg_tys.remove(0)))
                    }
                    "Option" => {
                        expect(self, 1, &mut arg_tys);
                        Type::Option(Box::new(arg_tys.remove(0)))
                    }
                    "weak" => {
                        expect(self, 1, &mut arg_tys);
                        let inner = arg_tys.remove(0);
                        if !self.is_reference_type(&inner) {
                            let span = t.span;
                            let msg = format!(
                                "`weak[{}]` is invalid: weak references only apply to \
                                 reference types",
                                self.ty_str(&inner)
                            );
                            self.error_help(
                                "E0213",
                                span,
                                msg,
                                "structs, enums, List, Map and functions can be weakly \
                                 referenced; primitives and strings cannot",
                            );
                        }
                        Type::Weak(Box::new(inner))
                    }
                    "Map" => {
                        expect(self, 2, &mut arg_tys);
                        let v = arg_tys.remove(1);
                        let k = arg_tys.remove(0);
                        if !matches!(
                            k,
                            Type::Int | Type::Bool | Type::Char | Type::Str | Type::Error
                        ) {
                            let span = args.first().map(|a| a.span).unwrap_or(t.span);
                            let msg = format!("`{}` cannot be a map key", self.ty_str(&k));
                            self.error_help(
                                "E0214",
                                span,
                                msg,
                                "map keys must be int, bool, char, or string",
                            );
                        }
                        Type::Map(Box::new(k), Box::new(v))
                    }
                    "Result" => {
                        expect(self, 2, &mut arg_tys);
                        let e = arg_tys.remove(1);
                        let ok = arg_tys.remove(0);
                        Type::Result(Box::new(ok), Box::new(e))
                    }
                    other => {
                        let span = t.span;
                        let msg = format!("`{other}` does not take type arguments");
                        let help = if self.current_type_params.iter().any(|(n, _)| n == other) {
                            "type parameters do not take type arguments".to_string()
                        } else {
                            "user-defined generic *types* are not supported yet; generic \
                             functions are — declare type parameters on the function: \
                             `fn f[T](x: T)`"
                                .to_string()
                        };
                        self.error_help("E0215", span, msg, help);
                        Type::Error
                    }
                }
            }
            TypeExprKind::Fn(params, ret) => {
                let params: Vec<Type> = params.iter().map(|p| self.resolve_type(p)).collect();
                let ret = match ret {
                    Some(r) => self.resolve_type(r),
                    None => Type::Unit,
                };
                Type::Fn(Box::new(FnSig::new(params, ret)))
            }
            TypeExprKind::Dyn(ident) => match self.type_names.get(&ident.name) {
                Some(&id) if self.out.defs.as_trait(id).is_some() => {
                    if self.out.defs.as_trait(id).is_some_and(|t| t.operator) {
                        let span = ident.span;
                        let msg =
                            format!("operator trait `{}` cannot be used as `dyn`", ident.name);
                        self.error("E0211", span, msg);
                        return Type::Error;
                    }
                    Type::Dyn(id)
                }
                Some(_) => {
                    let span = ident.span;
                    let msg = format!("`{}` is not a trait", ident.name);
                    self.error("E0211", span, msg);
                    Type::Error
                }
                None => {
                    let span = ident.span;
                    let msg = format!("unknown trait `{}`", ident.name);
                    self.error("E0212", span, msg);
                    Type::Error
                }
            },
        }
    }

    pub(crate) fn is_reference_type(&self, t: &Type) -> bool {
        matches!(
            t,
            Type::List(_)
                | Type::Map(..)
                | Type::Named(_)
                | Type::Fn(_)
                | Type::Dyn(_)
                | Type::Option(_)
                | Type::Result(..)
                | Type::Error
        )
    }

    // ----------------------------------------------------------- vtables

    /// Get (or build) the vtable for coercing `concrete` to `dyn trait_id`.
    pub(crate) fn vtable_for(&mut self, concrete: DefId, trait_id: DefId) -> Option<u32> {
        if let Some(&vt) = self.vtable_cache.get(&(concrete, trait_id)) {
            return Some(vt);
        }
        let protos = self.trait_impls.get(&(concrete, trait_id))?.clone();
        let vt = self.out.vtables.len() as u32;
        self.out.vtables.push(protos);
        self.vtable_cache.insert((concrete, trait_id), vt);
        Some(vt)
    }

    // ------------------------------------------------------------ bodies

    fn check_bodies(&mut self) {
        let total = self.out.fn_infos.len();
        for proto in 0..total {
            if !self.out.fn_infos[proto].pending {
                continue;
            }
            self.check_fn_body(proto as u32);
        }
    }

    fn check_fn_body(&mut self, proto: u32) {
        self.infer.reset();
        self.nodes_this_fn.clear();
        let info = self.out.fn_infos[proto as usize].clone();
        // Generic fns: their rigid type parameters are in scope for the
        // whole body (cleared after finalize below).
        self.current_type_params = info.type_params.clone();
        let source = info.source;
        let (decl, _item_idx) = match source {
            FnSource::Top { file, item } => {
                self.cur_file = file;
                match &self.file().items[item] {
                    Item::Fn(f) => (f, item),
                    _ => return,
                }
            }
            FnSource::Method { file, item, fn_idx } => {
                self.cur_file = file;
                match &self.file().items[item] {
                    Item::Impl(im) => (&im.fns[fn_idx], item),
                    _ => return,
                }
            }
            FnSource::Closure { .. } | FnSource::Synthesized => return,
        };

        self.fn_states.push(FnState {
            ret: info.sig.ret.clone(),
            n_locals: 0,
            captured: HashSet::new(),
            captures: Vec::new(),
            capture_map: HashMap::new(),
            loops: Vec::new(),
        });
        self.push_scope();
        for (i, p) in decl.params.iter().enumerate() {
            let ty = info.sig.params.get(i).cloned().unwrap_or(Type::Error);
            self.declare_local(&p.name, ty);
        }

        let ret = info.sig.ret.clone();
        let body_ty = self.check_block(&decl.body, Some(&ret));
        self.unify_or_err(
            &ret,
            &body_ty,
            last_meaningful_span(&decl.body).unwrap_or(decl.sig_span),
            "function body does not match the declared return type",
        );

        self.pop_scope();
        let state = self.fn_states.pop().unwrap();
        let fi = &mut self.out.fn_infos[proto as usize];
        fi.n_locals = state.n_locals;
        fi.captured = state.captured;
        fi.pending = false;

        self.finalize_types();
        self.current_type_params.clear();
    }

    /// After a top-level function (and its closures) is checked, substitute
    /// all inference variables in recorded node types and report bindings
    /// whose type never became known (inference is local, PRD §3.3).
    fn finalize_types(&mut self) {
        // Deferred generic instantiations: by end-of-function every
        // inference var must have resolved; report uninferable type
        // parameters (E0252) and late bound violations (E0253).
        let pending = std::mem::take(&mut self.pending_instantiations);
        for p in pending {
            for (i, subst) in p.subst.iter().enumerate() {
                let resolved = self.infer.resolve(subst);
                let (pname, bound) = &p.type_params[i];
                if self.infer.contains_unbound(&resolved) {
                    self.error_help(
                        "E0252",
                        p.span,
                        format!("cannot infer type parameter `{pname}` of `{}`", p.fn_name),
                        "annotate the binding or a surrounding expression so the \
                         parameter is determined",
                    );
                    continue;
                }
                if let Some(bound) = bound {
                    self.check_bound_satisfied(p.span, &p.fn_name, pname, *bound, &resolved);
                }
            }
        }
        let required = std::mem::take(&mut self.must_resolve);
        for (node, span) in required {
            if let Some(t) = self.out.types.get(&node).cloned()
                && self.infer.contains_unbound(&t)
            {
                self.error_help(
                    "E0251",
                    span,
                    "cannot infer the type of this binding",
                    "add a type annotation: `let name: Type = ...`",
                );
            }
        }
        let nodes = std::mem::take(&mut self.nodes_this_fn);
        for node in nodes {
            if let Some(t) = self.out.types.get(&node) {
                let resolved = self.infer.resolve(t);
                self.out.types.insert(node, resolved);
            }
        }
    }

    fn collect_exports(&mut self) {
        // Only the ENTRY file's fns are host-callable; imported files'
        // fns get protos but no exports (documented).
        for (name, &proto) in &self.fn_names[0] {
            let info = &self.out.fn_infos[proto as usize];
            // Generic fns are not exported: their erased signature
            // (`fn(T) -> T`) can never match host-side types. Call them
            // through a monomorphic wrapper fn instead.
            if !info.type_params.is_empty() {
                self.out.generic_fns.push(name.clone());
                continue;
            }
            // Unit families are erased at runtime, so the host sees the
            // backing number: `fn tick(dt: Duration)` exports as
            // `fn(i64) -> ...` in base units.
            let sig = info.sig.clone();
            let sig = FnSig::new(
                sig.params
                    .iter()
                    .map(|p| self.out.defs.erase_units(p))
                    .collect(),
                self.out.defs.erase_units(&sig.ret),
            );
            self.out.exports.insert(name.clone(), (proto, sig));
        }
    }

    // -------------------------------------------------- scopes & locals

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(Scope {
            bindings: HashMap::new(),
            fn_depth: self.fn_states.len() - 1,
        });
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub(crate) fn declare_local(&mut self, name: &Ident, ty: Type) -> LocalId {
        let state = self.fn_states.last_mut().unwrap();
        let local = state.n_locals;
        state.n_locals += 1;
        self.scopes.last_mut().unwrap().bindings.insert(
            name.name.clone(),
            Binding {
                local,
                ty,
                span: name.span,
            },
        );
        local
    }

    /// Resolve a variable name, wiring captures through any intervening
    /// closures. Returns the reference kind and the variable's type.
    pub(crate) fn lookup_var(&mut self, name: &str) -> Option<(VarRes, Type)> {
        let current_depth = self.fn_states.len() - 1;
        // Find the binding, innermost scope first.
        let mut found: Option<(usize, Binding)> = None;
        for scope in self.scopes.iter().rev() {
            if let Some(b) = scope.bindings.get(name) {
                found = Some((scope.fn_depth, b.clone()));
                break;
            }
        }
        let (owner_depth, binding) = found?;
        if owner_depth == current_depth {
            return Some((VarRes::Local(binding.local), binding.ty));
        }
        // Captured: mark the local in its owner and thread capture slots
        // through every closure between owner and current.
        self.fn_states[owner_depth].captured.insert(binding.local);
        let mut src = CapSrc::Local(binding.local);
        let mut slot = 0u16;
        for depth in (owner_depth + 1)..=current_depth {
            let key = (owner_depth, binding.local);
            let state = &mut self.fn_states[depth];
            slot = match state.capture_map.get(&key) {
                Some(&s) => s,
                None => {
                    let s = state.captures.len() as u16;
                    state.captures.push(src);
                    state.capture_map.insert(key, s);
                    s
                }
            };
            src = CapSrc::Capture(slot);
        }
        Some((VarRes::Capture(slot), binding.ty))
    }

    /// Span of a local's definition (for LSP goto-definition).
    pub(crate) fn lookup_var_span(&self, name: &str) -> Option<Span> {
        for scope in self.scopes.iter().rev() {
            if let Some(b) = scope.bindings.get(name) {
                return Some(b.span);
            }
        }
        None
    }

    // ----------------------------------------------------- type recording

    pub(crate) fn record_type(&mut self, node: NodeId, ty: Type) -> Type {
        self.out.types.insert(node, ty.clone());
        self.nodes_this_fn.push(node);
        ty
    }

    pub(crate) fn unify_or_err(
        &mut self,
        expected: &Type,
        found: &Type,
        span: Span,
        context: &str,
    ) -> bool {
        match self.infer.unify(expected, found) {
            Ok(()) => true,
            Err(e) => {
                let expected_s = self.ty_str(&e.expected);
                let found_s = self.ty_str(&e.found);
                self.error_help(
                    "E0220",
                    span,
                    format!("type mismatch: expected `{expected_s}`, found `{found_s}`"),
                    context.to_string(),
                );
                false
            }
        }
    }

    // ------------------------------------------------------ name lookups

    pub(crate) fn type_name(&self, name: &str) -> Option<DefId> {
        self.type_names.get(name).copied()
    }

    pub(crate) fn fn_by_name(&self, name: &str) -> Option<u32> {
        self.fn_names[self.cur_file].get(name).copied()
    }

    /// Look up a fn in another script file's top-level scope
    /// (`module::fn` cross-file references).
    pub(crate) fn fn_in_file(&self, file: usize, name: &str) -> Option<u32> {
        self.fn_names[file].get(name).copied()
    }

    /// The module name of an imported script file (diagnostics).
    pub(crate) fn script_module_name(&self, file: usize) -> &str {
        &self.files[file].0
    }

    pub(crate) fn module_ref(&self, name: &str) -> Option<ModuleRef> {
        self.modules_in_scope[self.cur_file].get(name).copied()
    }

    pub(crate) fn module_is_registered(&self, name: &str) -> bool {
        self.reg.modules.iter().any(|m| m.name == name)
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn imported(&self, name: &str) -> Option<ImportedRef> {
        Some(match self.imports[self.cur_file].get(name)? {
            Imported::HostFn(idx) => ImportedRef::HostFn(*idx),
            Imported::Const(t, c) => ImportedRef::Const(t.clone(), c.clone()),
            Imported::ScriptItem { file } => {
                // Resolved lazily (validated in validate_script_imports).
                ImportedRef::ScriptFn(self.fn_names[*file].get(name).copied()?)
            }
        })
    }

    pub(crate) fn prelude_fn(name: &str) -> Option<PreludeFn> {
        Some(match name {
            "print" => PreludeFn::Print,
            "println" => PreludeFn::Println,
            "str" => PreludeFn::Str,
            "fmt" => PreludeFn::Fmt,
            "same" => PreludeFn::Same,
            "weak" => PreludeFn::Weak,
            "int" => PreludeFn::Int,
            "float" => PreludeFn::Float,
            _ => return None,
        })
    }

    pub(crate) fn current_ret(&self) -> Type {
        self.fn_states
            .last()
            .map(|s| s.ret.clone())
            .unwrap_or(Type::Error)
    }

    pub(crate) fn enter_loop(&mut self) {
        self.fn_states
            .last_mut()
            .unwrap()
            .loops
            .push(LoopCtx { has_break: false });
    }

    /// Returns whether the loop contained a `break`.
    pub(crate) fn exit_loop(&mut self) -> bool {
        self.fn_states
            .last_mut()
            .unwrap()
            .loops
            .pop()
            .map(|l| l.has_break)
            .unwrap_or(false)
    }

    pub(crate) fn mark_break(&mut self, span: Span) -> bool {
        match self.fn_states.last_mut().unwrap().loops.last_mut() {
            Some(l) => {
                l.has_break = true;
                true
            }
            None => {
                self.error("E0221", span, "`break` outside of a loop");
                false
            }
        }
    }

    pub(crate) fn in_loop(&self) -> bool {
        self.fn_states.last().is_some_and(|s| !s.loops.is_empty())
    }

    // -------------------------------------------------- closure checking

    /// Begin checking a closure body: allocates its proto and state.
    pub(crate) fn begin_closure(&mut self, node: NodeId, sig: FnSig, span: Span) -> u32 {
        let proto = self.out.fn_infos.len() as u32;
        self.out.fn_infos.push(FnInfo {
            name: format!("<closure@{}>", span.lo),
            sig,
            type_params: Vec::new(),
            source: FnSource::Closure { node },
            n_locals: 0,
            captured: HashSet::new(),
            captures: Vec::new(),
            span,
            pending: true,
        });
        self.fn_states.push(FnState {
            ret: Type::Error, // set by caller once known
            n_locals: 0,
            captured: HashSet::new(),
            captures: Vec::new(),
            capture_map: HashMap::new(),
            loops: Vec::new(),
        });
        self.push_scope();
        proto
    }

    pub(crate) fn set_closure_ret(&mut self, ret: Type) {
        self.fn_states.last_mut().unwrap().ret = ret;
    }

    pub(crate) fn end_closure(&mut self, proto: u32) {
        self.pop_scope();
        let state = self.fn_states.pop().unwrap();
        let fi = &mut self.out.fn_infos[proto as usize];
        fi.n_locals = state.n_locals;
        fi.captured = state.captured;
        fi.captures = state.captures;
        fi.pending = false;
    }
}

/// Span of the last statement in a block (for return-type mismatch
/// diagnostics).
/// Does the signature mention `Type::Param(i)` anywhere?
fn sig_mentions_param(sig: &FnSig, i: u32) -> bool {
    fn mentions(t: &Type, i: u32) -> bool {
        match t {
            Type::Param(p) => *p == i,
            Type::List(e) | Type::Option(e) | Type::Weak(e) => mentions(e, i),
            Type::Map(a, b) | Type::Result(a, b) => mentions(a, i) || mentions(b, i),
            Type::Fn(sig) => sig.params.iter().any(|p| mentions(p, i)) || mentions(&sig.ret, i),
            _ => false,
        }
    }
    sig.params.iter().any(|p| mentions(p, i)) || mentions(&sig.ret, i)
}

fn last_meaningful_span(block: &Block) -> Option<Span> {
    block.stmts.last().map(|s| match s {
        Stmt::Let { span, .. } | Stmt::LetElse { span, .. } => *span,
        Stmt::Expr { expr, .. } => expr.span,
    })
}
