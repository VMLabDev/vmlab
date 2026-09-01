//! The abstract syntax tree.
//!
//! Every expression and pattern carries a `NodeId`; the checker records
//! types and resolutions in side tables keyed by id, which the emitter then
//! consumes. The parser produces a *partial* AST plus diagnostics on broken
//! input (PRD §5.1) — error recovery inserts `ExprKind::Error` nodes.

use wscript_core::span::Span;

pub type NodeId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Debug)]
pub struct SourceFile {
    pub items: Vec<Item>,
}

#[derive(Debug)]
pub enum Item {
    Use(UseDecl),
    Fn(FnDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Trait(TraitDecl),
    Impl(ImplDecl),
    /// `units Duration: int { ns = 1, ms = 1_000_000 }`
    Units(UnitsDecl),
    /// `mod name { ... }` — interface files (`.wscripti`) only.
    Mod(ModDecl),
    /// `const NAME: type` — interface files (`.wscripti`) only.
    Const(ConstDecl),
}

/// A module declaration block, valid only in `.wscripti` interface files
/// (PRD §9.1); the checker rejects it in scripts.
#[derive(Debug)]
pub struct ModDecl {
    pub name: Ident,
    pub items: Vec<Item>,
    pub doc: Option<String>,
    pub span: Span,
}

#[derive(Debug)]
pub struct ConstDecl {
    pub name: Ident,
    pub ty: TypeExpr,
    /// `= 16` — the value. `const` items exist only in `.wscripti`
    /// interface files, where the value is what the host registered and
    /// therefore what `wscript check` must fold; `None` means the
    /// declaration omitted it, which the loader reports.
    pub value: Option<Expr>,
    pub doc: Option<String>,
    pub span: Span,
}

/// `use module` / `use module::item`
#[derive(Debug)]
pub struct UseDecl {
    /// Module name as referenced in the script — for the path form
    /// (`use "./x.wscript" as y`) this is the alias (or the file stem).
    pub module: Ident,
    pub item: Option<Ident>,
    /// `use "./helpers.wscript" [as name]` — an explicit script-file
    /// import; `None` for the bare `use name` form (host module first,
    /// then sibling/`src_roots` file resolution).
    pub path_lit: Option<String>,
    pub span: Span,
}

#[derive(Debug)]
pub struct FnDecl {
    pub name: Ident,
    /// `fn f[T, U: Ord](...)` — generic type parameters (top-level fns
    /// only; empty for monomorphic fns).
    pub type_params: Vec<TypeParam>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Block,
    /// `false` for bodyless declarations (`.wscripti` interface files only).
    pub has_body: bool,
    pub doc: Option<String>,
    pub span: Span,
    /// Span of `fn name(params) -> ret` for hover/goto.
    pub sig_span: Span,
}

/// One declared type parameter: `T` or `T: Ord`.
#[derive(Debug)]
pub struct TypeParam {
    pub name: Ident,
    pub bound: Option<Ident>,
}

#[derive(Debug)]
pub struct Param {
    pub name: Ident,
    /// `None` only for `self` receivers.
    pub ty: Option<TypeExpr>,
    pub is_self: bool,
    pub span: Span,
}

#[derive(Debug)]
pub struct StructDecl {
    pub name: Ident,
    pub fields: Vec<FieldDecl>,
    pub derives: Vec<Ident>,
    /// `#[opaque]` — interface files only (host handle types).
    pub opaque: bool,
    pub doc: Option<String>,
    pub span: Span,
}

#[derive(Debug)]
pub struct FieldDecl {
    pub name: Ident,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug)]
pub struct EnumDecl {
    pub name: Ident,
    pub variants: Vec<VariantDecl>,
    pub derives: Vec<Ident>,
    pub doc: Option<String>,
    pub span: Span,
}

#[derive(Debug)]
pub enum VariantBody {
    Unit,
    Tuple(Vec<TypeExpr>),
    Struct(Vec<FieldDecl>),
}

#[derive(Debug)]
pub struct VariantDecl {
    pub name: Ident,
    pub body: VariantBody,
    pub span: Span,
}

/// `units Name: base { unit = factor, ... }` — a unit family (PRD §3.10).
/// Factors are ordinary expressions here; the checker const-evaluates them
/// in declaration order against the units already declared above.
#[derive(Debug)]
pub struct UnitsDecl {
    pub name: Ident,
    /// The backing numeric type — `int` or `float`, validated by the checker.
    pub base: TypeExpr,
    pub units: Vec<UnitEntry>,
    pub derives: Vec<Ident>,
    pub doc: Option<String>,
    pub span: Span,
}

#[derive(Debug)]
pub struct UnitEntry {
    pub name: Ident,
    pub factor: Expr,
    pub span: Span,
}

#[derive(Debug)]
pub struct TraitDecl {
    pub name: Ident,
    pub methods: Vec<TraitMethodDecl>,
    pub span: Span,
}

/// A method signature inside a `trait` block (no body in v1).
#[derive(Debug)]
pub struct TraitMethodDecl {
    pub name: Ident,
    /// Parameters *after* the mandatory `self`.
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub span: Span,
}

/// `impl Type { ... }` or `impl Trait for Type { ... }`
#[derive(Debug)]
pub struct ImplDecl {
    pub trait_name: Option<Ident>,
    pub ty_name: Ident,
    pub fns: Vec<FnDecl>,
    pub span: Span,
}

// ---------------------------------------------------------------- types

#[derive(Debug)]
pub struct TypeExpr {
    pub kind: TypeExprKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum TypeExprKind {
    /// `int`, `Point`, `weak` … (validated during checking).
    Name(Ident),
    /// `List[int]`, `Map[string, int]`, `Option[T]`, `Result[T, E]`,
    /// `weak[T]` — square-bracket application is reserved for builtins in
    /// v1 (PRD §3.6).
    App(Ident, Vec<TypeExpr>),
    /// `fn(int, string) -> bool`
    Fn(Vec<TypeExpr>, Option<Box<TypeExpr>>),
    /// `dyn Trait`
    Dyn(Ident),
    /// `()` — spelled `unit` normally, but `()` is accepted.
    Unit,
    /// Recovery placeholder.
    Error,
}

// ----------------------------------------------------------- statements

#[derive(Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
    pub id: NodeId,
}

#[derive(Debug)]
pub enum Stmt {
    /// `let name[: ty] = init`
    Let {
        name: Ident,
        ty: Option<TypeExpr>,
        init: Expr,
        span: Span,
        id: NodeId,
    },
    /// `let pat = init else { ... }` — else block must diverge.
    LetElse {
        pat: Pattern,
        init: Expr,
        else_block: Block,
        span: Span,
        id: NodeId,
    },
    /// An expression statement. `terminated` records a trailing `;`, which
    /// forces the statement's value to be discarded even in tail position.
    Expr { expr: Expr, terminated: bool },
}

// ---------------------------------------------------------- expressions

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
    pub id: NodeId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

/// The numeric part of a suffixed literal, as written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LitNum {
    Int(i64),
    Float(f64),
}

/// One piece of an interpolated string (`ExprKind::StrInterp`).
#[derive(Debug)]
pub enum InterpPart {
    Lit(String),
    Hole(Box<Expr>),
}

#[derive(Debug)]
pub enum ExprKind {
    IntLit(i64),
    FloatLit(f64),
    /// A numeric literal with a unit suffix: `500ms`, `1.5s`, `4MiB`. The
    /// checker resolves the suffix to a unit family and folds the whole
    /// thing to a base-unit constant.
    QuantityLit {
        value: LitNum,
        unit: Ident,
    },
    BoolLit(bool),
    CharLit(char),
    StrLit(String),
    /// `"hp: {p.hp}"` — an interpolated string; evaluates to `string`.
    StrInterp(Vec<InterpPart>),
    UnitLit,
    /// `a`, `self`, `module::item`, `Enum::Variant`,
    /// `module::Enum::Variant` — resolved by the checker.
    Path(Vec<Ident>),
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `place = value` (an expression of type unit, Rust-style).
    /// `op` is `Some` for compound assignment (`place += value` reads,
    /// applies the operator, and writes back — the place is evaluated
    /// once).
    Assign {
        target: Box<Expr>,
        value: Box<Expr>,
        op: Option<BinOp>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    MethodCall {
        recv: Box<Expr>,
        name: Ident,
        args: Vec<Expr>,
    },
    Field {
        obj: Box<Expr>,
        name: Ident,
    },
    Index {
        obj: Box<Expr>,
        idx: Box<Expr>,
    },
    /// `Point { x: 1, y: 2 }` — path may be `module::Type`.
    StructLit {
        path: Vec<Ident>,
        fields: Vec<(Ident, Expr)>,
    },
    ListLit(Vec<Expr>),
    /// `#{ key: value, ... }`
    MapLit(Vec<(Expr, Expr)>),
    If {
        cond: Box<Expr>,
        then: Block,
        /// `Block` or nested `If` expression.
        else_: Option<Box<Expr>>,
    },
    IfLet {
        pat: Pattern,
        scrutinee: Box<Expr>,
        then: Block,
        else_: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    While {
        cond: Box<Expr>,
        body: Block,
    },
    Loop {
        body: Block,
    },
    For {
        var: Ident,
        iter: Box<Expr>,
        body: Block,
    },
    /// `a..b` / `a..=b` — only valid as a `for` iterable in v1.
    Range {
        lo: Box<Expr>,
        hi: Box<Expr>,
        inclusive: bool,
    },
    Break,
    Continue,
    Return(Option<Box<Expr>>),
    Block(Block),
    /// `|x, y| expr` / `|x: int| -> int { ... }`
    Closure {
        params: Vec<(Ident, Option<TypeExpr>)>,
        ret: Option<TypeExpr>,
        body: Box<Expr>,
    },
    /// `expr?`
    Try(Box<Expr>),
    /// Parse-error recovery node.
    Error,
}

#[derive(Debug)]
pub struct MatchArm {
    pub pat: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

// ------------------------------------------------------------- patterns

#[derive(Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
    pub id: NodeId,
}

#[derive(Debug)]
pub enum PatternKind {
    Wildcard,
    /// Lowercase identifier: binds the value. (An uppercase-resolving path
    /// is a unit variant pattern — disambiguated by the checker.)
    Binding(Ident),
    IntLit(i64),
    /// `500ms` in pattern position — folded like the expression form.
    QuantityLit {
        value: LitNum,
        unit: Ident,
    },
    BoolLit(bool),
    CharLit(char),
    StrLit(String),
    /// `Variant`, `Enum::Variant`, `Enum::Variant(p1, p2)`,
    /// `Enum::Variant { f1, f2: pat }`
    Variant {
        path: Vec<Ident>,
        args: VariantPatArgs,
    },
    /// `Point { x, y: pat, .. }` — shorthand fields are desugared to
    /// binding patterns by the parser.
    Struct {
        path: Vec<Ident>,
        fields: Vec<(Ident, Pattern)>,
        has_rest: bool,
    },
    /// `a | b`
    Or(Vec<Pattern>),
    Error,
}

#[derive(Debug)]
pub enum VariantPatArgs {
    Unit,
    Tuple(Vec<Pattern>),
    Struct {
        fields: Vec<(Ident, Pattern)>,
        has_rest: bool,
    },
}

// ------------------------------------------------------------- traversal
//
// One structural walk of the tree, so consumers that only need to reach
// every node do not each hand-roll a match over `ExprKind`. Before this
// existed there were three such walks — the emitter's closure finder, the
// LSP's span index, and the golden renderer — and two ended in a catch-all
// arm, so a new expression form silently stopped being visited.
//
// Override the `visit_*` method you care about and call the matching
// `walk_*` to keep descending; the default methods do exactly that.

/// A read-only structural traversal of the AST.
///
/// The default methods recurse into children, so an implementation only
/// overrides what it needs:
///
/// ```ignore
/// struct Spans(Vec<(Span, NodeId)>);
/// impl<'a> Visit<'a> for Spans {
///     fn visit_expr(&mut self, e: &'a Expr) {
///         self.0.push((e.span, e.id));
///         walk_expr(self, e);
///     }
/// }
/// ```
pub trait Visit<'a>: Sized {
    fn visit_item(&mut self, item: &'a Item) {
        walk_item(self, item);
    }
    fn visit_block(&mut self, block: &'a Block) {
        walk_block(self, block);
    }
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        walk_stmt(self, stmt);
    }
    fn visit_expr(&mut self, expr: &'a Expr) {
        walk_expr(self, expr);
    }
    fn visit_pattern(&mut self, pat: &'a Pattern) {
        walk_pattern(self, pat);
    }
}

pub fn walk_file<'a, V: Visit<'a>>(v: &mut V, file: &'a SourceFile) {
    for item in &file.items {
        v.visit_item(item);
    }
}

pub fn walk_item<'a, V: Visit<'a>>(v: &mut V, item: &'a Item) {
    match item {
        Item::Fn(f) => v.visit_block(&f.body),
        Item::Impl(im) => {
            for f in &im.fns {
                v.visit_block(&f.body);
            }
        }
        // Factors are ordinary expressions the checker const-evaluates.
        Item::Units(u) => {
            for entry in &u.units {
                v.visit_expr(&entry.factor);
            }
        }
        Item::Mod(m) => {
            for item in &m.items {
                v.visit_item(item);
            }
        }
        Item::Use(_) | Item::Struct(_) | Item::Enum(_) | Item::Trait(_) | Item::Const(_) => {}
    }
}

pub fn walk_block<'a, V: Visit<'a>>(v: &mut V, block: &'a Block) {
    for stmt in &block.stmts {
        v.visit_stmt(stmt);
    }
}

pub fn walk_stmt<'a, V: Visit<'a>>(v: &mut V, stmt: &'a Stmt) {
    match stmt {
        Stmt::Let { init, .. } => v.visit_expr(init),
        Stmt::LetElse {
            pat,
            init,
            else_block,
            ..
        } => {
            v.visit_pattern(pat);
            v.visit_expr(init);
            v.visit_block(else_block);
        }
        Stmt::Expr { expr, .. } => v.visit_expr(expr),
    }
}

pub fn walk_expr<'a, V: Visit<'a>>(v: &mut V, expr: &'a Expr) {
    match &expr.kind {
        // Interpolation holes are children; every consumer used to have to
        // remember that separately.
        ExprKind::StrInterp(parts) => {
            for part in parts {
                if let InterpPart::Hole(h) = part {
                    v.visit_expr(h);
                }
            }
        }
        ExprKind::Unary { expr: inner, .. } | ExprKind::Try(inner) => v.visit_expr(inner),
        ExprKind::Binary { lhs, rhs, .. } => {
            v.visit_expr(lhs);
            v.visit_expr(rhs);
        }
        ExprKind::Assign { target, value, .. } => {
            v.visit_expr(target);
            v.visit_expr(value);
        }
        ExprKind::Call { callee, args } => {
            v.visit_expr(callee);
            for a in args {
                v.visit_expr(a);
            }
        }
        ExprKind::MethodCall { recv, args, .. } => {
            v.visit_expr(recv);
            for a in args {
                v.visit_expr(a);
            }
        }
        ExprKind::Field { obj, .. } => v.visit_expr(obj),
        ExprKind::Index { obj, idx } => {
            v.visit_expr(obj);
            v.visit_expr(idx);
        }
        ExprKind::StructLit { fields, .. } => {
            for (_, value) in fields {
                v.visit_expr(value);
            }
        }
        ExprKind::ListLit(items) => {
            for i in items {
                v.visit_expr(i);
            }
        }
        ExprKind::MapLit(entries) => {
            for (k, value) in entries {
                v.visit_expr(k);
                v.visit_expr(value);
            }
        }
        ExprKind::If { cond, then, else_ } => {
            v.visit_expr(cond);
            v.visit_block(then);
            if let Some(e) = else_ {
                v.visit_expr(e);
            }
        }
        ExprKind::IfLet {
            pat,
            scrutinee,
            then,
            else_,
        } => {
            v.visit_pattern(pat);
            v.visit_expr(scrutinee);
            v.visit_block(then);
            if let Some(e) = else_ {
                v.visit_expr(e);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            v.visit_expr(scrutinee);
            for arm in arms {
                v.visit_pattern(&arm.pat);
                if let Some(g) = &arm.guard {
                    v.visit_expr(g);
                }
                v.visit_expr(&arm.body);
            }
        }
        ExprKind::While { cond, body } => {
            v.visit_expr(cond);
            v.visit_block(body);
        }
        ExprKind::Loop { body } => v.visit_block(body),
        ExprKind::For { iter, body, .. } => {
            v.visit_expr(iter);
            v.visit_block(body);
        }
        ExprKind::Range { lo, hi, .. } => {
            v.visit_expr(lo);
            v.visit_expr(hi);
        }
        ExprKind::Return(value) => {
            if let Some(value) = value {
                v.visit_expr(value);
            }
        }
        ExprKind::Block(b) => v.visit_block(b),
        ExprKind::Closure { body, .. } => v.visit_expr(body),
        // Leaves. Listed rather than caught by `_` so a new expression form
        // is a compile error here, which is the point of this module.
        ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::QuantityLit { .. }
        | ExprKind::BoolLit(_)
        | ExprKind::CharLit(_)
        | ExprKind::StrLit(_)
        | ExprKind::UnitLit
        | ExprKind::Path(_)
        | ExprKind::Break
        | ExprKind::Continue
        | ExprKind::Error => {}
    }
}

pub fn walk_pattern<'a, V: Visit<'a>>(v: &mut V, pat: &'a Pattern) {
    match &pat.kind {
        PatternKind::Variant { args, .. } => match args {
            VariantPatArgs::Tuple(pats) => {
                for p in pats {
                    v.visit_pattern(p);
                }
            }
            VariantPatArgs::Struct { fields, .. } => {
                for (_, p) in fields {
                    v.visit_pattern(p);
                }
            }
            VariantPatArgs::Unit => {}
        },
        PatternKind::Struct { fields, .. } => {
            for (_, p) in fields {
                v.visit_pattern(p);
            }
        }
        PatternKind::Or(alts) => {
            for p in alts {
                v.visit_pattern(p);
            }
        }
        PatternKind::Wildcard
        | PatternKind::Binding(_)
        | PatternKind::IntLit(_)
        | PatternKind::QuantityLit { .. }
        | PatternKind::BoolLit(_)
        | PatternKind::CharLit(_)
        | PatternKind::StrLit(_)
        | PatternKind::Error => {}
    }
}
