//! [`Index`] — the checker's product for an editor.
//!
//! The check tables answer "what does this node mean?"; an editor asks the
//! inverse, "what is at this *offset*, and what could go here?". This
//! module is where that inversion lives, so a language server consumes an
//! answer instead of re-deriving one. Before it existed, `wscript lsp`
//! hand-walked the AST for spans and kept its own copies of the builtin
//! method tables, the keyword set and the prelude — copies that drifted,
//! leaving fourteen methods that typechecked but never completed
//! (issues #16, #17).
//!
//! Partial by nature, unlike the total lowering accessors: an editor asks
//! about positions that resolve to nothing, and that is not a bug.

use std::collections::HashMap;

use wscript_core::defs::{DefKind, DefTable};
use wscript_core::registry::{HostRef, Registry};
use wscript_core::span::Span;
use wscript_core::types::{FnSig, Type};

use crate::ast::{self, Ident, NodeId, SourceFile, TypeExpr, TypeExprKind, Visit};
use crate::token::SCRIPT_KEYWORDS;

use super::{CallKind, CheckResult, FnSource, MethodRes, PreludeFn, builtin_methods, resolve};

// -------------------------------------------------------------- span maps

/// Spans carrying a payload, searched by source offset. Every table in the
/// index is one of these, so "narrowest thing at this offset" is written
/// once rather than once per table.
struct SpanMap<T>(Vec<(Span, T)>);

// Hand-written: `derive` would demand `T: Default`, which the payloads
// have no reason to satisfy.
impl<T> Default for SpanMap<T> {
    fn default() -> SpanMap<T> {
        SpanMap(Vec::new())
    }
}

impl<T> SpanMap<T> {
    fn push(&mut self, span: Span, value: T) {
        self.0.push((span, value));
    }

    /// The narrowest entry whose span contains `offset`.
    ///
    /// Entries are recorded parents-first, so the reversed scan breaks a
    /// span tie towards the innermost — error-recovery wrappers share
    /// their child's span, and the child is the better answer.
    fn narrowest(&self, offset: u32, ends: Ends) -> Option<(Span, &T)> {
        self.0
            .iter()
            .rev()
            .filter(|(span, _)| {
                span.lo <= offset
                    && match ends {
                        Ends::Open => offset < span.hi,
                        Ends::Closed => offset <= span.hi,
                    }
            })
            .min_by_key(|(span, _)| span.len())
            .map(|(span, value)| (*span, value))
    }
}

/// Whether a span's end counts as inside it.
#[derive(Clone, Copy)]
enum Ends {
    /// A node *occupies* its span, so the offset after it is the next
    /// thing along.
    Open,
    /// A name *is* asked about from the caret, which sits between
    /// characters: the caret at the end of an identifier is still on that
    /// identifier, and the caret in the empty gap a `.` opened is on the
    /// member that would go there.
    Closed,
}

// ------------------------------------------------------------- the index

/// A source name that is not an expression node.
#[derive(Debug, Clone)]
enum Named {
    /// A type as written — annotation, `impl` head, field type, `dyn` head.
    Type(String),
    /// A type's own declared name.
    TypeDecl(String),
    /// A member as declared inside its owner: a struct field, an enum
    /// variant, a unit of a family, or a trait's method signature.
    Member { owner: String, name: String },
    /// A function's declared name. Located by *where it is* rather than by
    /// what it is called: a name is only unique per file, and the checker
    /// keys its own record of the function the same way.
    Fn(FnSource),
}

/// Position lookups over one analysis' AST.
///
/// Traversal is [`ast::Visit`]'s, so a new syntax form has to be handled
/// there before it can reach here at all — and it runs on the pipeline's
/// own stack, where that recursion has the headroom the parser assumed.
/// What each form *contributes* is still this module's own match, and a
/// new form contributing nothing is silently unindexed; the matches below
/// list every variant rather than ending in `_` so that stays a decision
/// someone made.
#[derive(Default)]
pub struct Index {
    /// Every expression and pattern node, parents before children.
    nodes: SpanMap<NodeId>,
    /// The inverse of `nodes`, for callers holding a node and wanting to
    /// point at it (`span_of`). Both directions are needed and neither is
    /// cheap to derive from the other.
    spans: HashMap<NodeId, Span>,
    /// The call an expression is the callee of. A call's resolution hangs
    /// off the call, not off its callee, so pointing at `atan2` in
    /// `math::atan2(1.0, 2.0)` lands on the callee path and would
    /// otherwise find nothing.
    call_of_callee: HashMap<NodeId, NodeId>,
    /// Where a member name is written — or, mid-typing, where it would be
    /// written — and the receiver whose type supplies the candidates.
    members: SpanMap<NodeId>,
    /// Where a path segment is written, and the segments qualifying it.
    paths: SpanMap<Vec<String>>,
    /// Names with a span but no node id.
    decls: SpanMap<Named>,
    /// The top-level item being visited, so a function's declared name is
    /// recorded with the same [`FnSource`] the checker gives it.
    at: ItemPos,
}

/// Where the item currently being visited sits.
#[derive(Default, Clone, Copy)]
struct ItemPos {
    file: usize,
    item: usize,
}

impl Index {
    /// Index every file of a compilation. Spans are rebased into one
    /// address space, so an entry-file offset can only match entry-file
    /// nodes.
    pub fn build(files: &[(String, &SourceFile)]) -> Index {
        let mut index = Index::default();
        // Items are walked by hand rather than through `ast::walk_file`
        // because their positions are part of what is recorded.
        for (file, (_, source)) in files.iter().enumerate() {
            for (item, node) in source.items.iter().enumerate() {
                index.at = ItemPos { file, item };
                index.visit_item(node);
            }
        }
        index
    }

    /// The span of `node`, if it is one this index covers.
    pub fn span_of(&self, node: NodeId) -> Option<Span> {
        self.spans.get(&node).copied()
    }

    /// Smallest node containing `offset`.
    pub fn node_at(&self, offset: u32) -> Option<NodeId> {
        self.nodes.narrowest(offset, Ends::Open).map(|(_, id)| *id)
    }

    /// The nodes a cursor at `offset` can carry a host registration: the
    /// node itself, and — because a call's resolution hangs off the call
    /// rather than its callee — the call it is the callee of.
    ///
    /// Nothing wider: an enclosing call reached any other way is one the
    /// cursor is not on. `v` in `v.get(k)` is the method call's receiver,
    /// and hovering it should say what `v` is, not what `get` takes.
    fn host_nodes_at(&self, offset: u32) -> Vec<NodeId> {
        let Some(node) = self.node_at(offset) else {
            return Vec::new();
        };
        let mut nodes = vec![node];
        nodes.extend(self.call_of_callee.get(&node).copied());
        nodes
    }

    /// The narrowest name at `offset`, among those with no node id.
    fn named_at(&self, offset: u32) -> Option<(Span, &Named)> {
        self.decls.narrowest(offset, Ends::Closed)
    }

    /// What kind of thing can be written at `offset`, decided from the
    /// parse rather than by re-scanning the text: the parser already
    /// records a member access with a missing name, and a path with a
    /// trailing `::`, precisely so this question has an answer.
    fn context_at(&self, offset: u32) -> Context<'_> {
        if let Some((_, recv)) = self.members.narrowest(offset, Ends::Closed) {
            return Context::Member(*recv);
        }
        match self.paths.narrowest(offset, Ends::Closed) {
            Some((_, qualifier)) if !qualifier.is_empty() => Context::Qualified(qualifier),
            _ => Context::Toplevel,
        }
    }
}

/// What the cursor sits in, for completion.
enum Context<'a> {
    /// After `recv.` — the members of the receiver's type.
    Member(NodeId),
    /// After `qualifier::` — module items or enum variants.
    Qualified(&'a [String]),
    /// Anything nameable here.
    Toplevel,
}

// ------------------------------------------------------------- traversal

impl<'a> Visit<'a> for Index {
    fn visit_item(&mut self, item: &'a ast::Item) {
        use ast::Item::*;
        match item {
            Fn(f) => {
                let ItemPos { file, item } = self.at;
                self.fn_decl(f, FnSource::Top { file, item })
            }
            Struct(s) => {
                self.type_decl(&s.name);
                for field in &s.fields {
                    self.member(&s.name.name, &field.name);
                    self.type_expr(&field.ty);
                }
            }
            Enum(e) => {
                self.type_decl(&e.name);
                for v in &e.variants {
                    self.member(&e.name.name, &v.name);
                    match &v.body {
                        ast::VariantBody::Unit => {}
                        ast::VariantBody::Tuple(tys) => {
                            for t in tys {
                                self.type_expr(t);
                            }
                        }
                        ast::VariantBody::Struct(fields) => {
                            for f in fields {
                                self.type_expr(&f.ty);
                            }
                        }
                    }
                }
            }
            Trait(t) => {
                self.type_decl(&t.name);
                for m in &t.methods {
                    self.member(&t.name.name, &m.name);
                    for p in &m.params {
                        if let Some(ty) = &p.ty {
                            self.type_expr(ty);
                        }
                    }
                    if let Some(ret) = &m.ret {
                        self.type_expr(ret);
                    }
                }
            }
            Impl(im) => {
                if let Some(tr) = &im.trait_name {
                    self.type_name(tr);
                }
                self.type_name(&im.ty_name);
                let ItemPos { file, item } = self.at;
                for (fn_idx, f) in im.fns.iter().enumerate() {
                    self.fn_decl(f, FnSource::Method { file, item, fn_idx });
                }
            }
            Units(u) => {
                self.type_decl(&u.name);
                self.type_expr(&u.base);
                for entry in &u.units {
                    self.member(&u.name.name, &entry.name);
                }
            }
            Const(c) => {
                self.type_expr(&c.ty);
            }
            Use(u) => self.use_decl(u),
            // Interface-only, and never part of an analysis. Descending
            // would record its functions against this item's position,
            // which belongs to the `mod` and not to them.
            Mod(_) => return,
        }
        ast::walk_item(self, item);
    }

    fn visit_stmt(&mut self, stmt: &'a ast::Stmt) {
        if let ast::Stmt::Let { ty: Some(ty), .. } = stmt {
            self.type_expr(ty);
        }
        ast::walk_stmt(self, stmt);
    }

    /// Descent into children is [`ast::walk_expr`]'s job; this records what
    /// the form itself contributes beyond its own span — a name an editor
    /// can complete, or a resolution that hangs off a parent.
    fn visit_expr(&mut self, e: &'a ast::Expr) {
        self.nodes.push(e.span, e.id);
        self.spans.insert(e.id, e.span);
        use ast::ExprKind::*;
        match &e.kind {
            Call { callee, .. } => {
                self.call_of_callee.insert(callee.id, e.id);
            }
            Field { obj, name } => self.members.push(name.span, obj.id),
            MethodCall { recv, name, .. } => self.members.push(name.span, recv.id),
            Path(segments) => self.path(segments, e.span),
            StructLit { path, .. } => self.path(path, e.span),
            Closure { params, ret, .. } => {
                for (_, ty) in params {
                    if let Some(ty) = ty {
                        self.type_expr(ty);
                    }
                }
                if let Some(ret) = ret {
                    self.type_expr(ret);
                }
            }
            // Forms whose span is all they contribute — listed rather than
            // caught by `_` so that a new expression form has to be
            // decided about here, not silently added to this list.
            IntLit(_)
            | FloatLit(_)
            | QuantityLit { .. }
            | BoolLit(_)
            | CharLit(_)
            | StrLit(_)
            | StrInterp(_)
            | UnitLit
            | Unary { .. }
            | Binary { .. }
            | Assign { .. }
            | Index { .. }
            | ListLit(_)
            | MapLit(_)
            | If { .. }
            | IfLet { .. }
            | Match { .. }
            | While { .. }
            | Loop { .. }
            | For { .. }
            | Range { .. }
            | Break
            | Continue
            | Return(_)
            | Block(_)
            | Try(_)
            | Error => {}
        }
        ast::walk_expr(self, e);
    }

    fn visit_pattern(&mut self, p: &'a ast::Pattern) {
        self.nodes.push(p.span, p.id);
        self.spans.insert(p.id, p.span);
        use ast::PatternKind::*;
        match &p.kind {
            Variant { path, .. } | Struct { path, .. } => self.path(path, p.span),
            // As in `visit_expr`: exhaustive so a new pattern form is a
            // decision rather than an omission.
            Wildcard
            | Binding(_)
            | IntLit(_)
            | QuantityLit { .. }
            | BoolLit(_)
            | CharLit(_)
            | StrLit(_)
            | Or(_)
            | Error => {}
        }
        ast::walk_pattern(self, p);
    }
}

impl Index {
    fn type_decl(&mut self, name: &Ident) {
        self.decls
            .push(name.span, Named::TypeDecl(name.name.clone()));
    }

    fn member(&mut self, owner: &str, name: &Ident) {
        self.decls.push(
            name.span,
            Named::Member {
                owner: owner.to_string(),
                name: name.name.clone(),
            },
        );
    }

    fn type_name(&mut self, name: &Ident) {
        self.decls.push(name.span, Named::Type(name.name.clone()));
    }

    fn fn_decl(&mut self, f: &ast::FnDecl, source: FnSource) {
        self.decls.push(f.name.span, Named::Fn(source));
        for p in &f.params {
            if let Some(ty) = &p.ty {
                self.type_expr(ty);
            }
        }
        if let Some(ret) = &f.ret {
            self.type_expr(ret);
        }
    }

    /// `use module::item` names the same things a qualified path does, so
    /// it completes the same way.
    fn use_decl(&mut self, u: &ast::UseDecl) {
        if u.path_lit.is_some() {
            // `use "./x.wscript" as y` — a file, not a module namespace.
            return;
        }
        self.paths.push(u.module.span, Vec::new());
        match &u.item {
            Some(item) => self.paths.push(item.span, vec![u.module.name.clone()]),
            None => {
                if let Some(gap) = gap_past_colon_colon(u.module.span, u.span) {
                    self.paths.push(gap, vec![u.module.name.clone()]);
                }
            }
        }
    }

    fn type_expr(&mut self, ty: &TypeExpr) {
        match &ty.kind {
            TypeExprKind::Name(n) | TypeExprKind::Dyn(n) => self.type_name(n),
            TypeExprKind::App(n, args) => {
                self.type_name(n);
                for a in args {
                    self.type_expr(a);
                }
            }
            TypeExprKind::Fn(params, ret) => {
                for p in params {
                    self.type_expr(p);
                }
                if let Some(ret) = ret {
                    self.type_expr(ret);
                }
            }
            TypeExprKind::Unit | TypeExprKind::Error => {}
        }
    }

    /// Record each segment of a written path, qualified by the segments
    /// before it — plus the empty region past a trailing `::`, which is
    /// where an editor asks "what belongs to this module?".
    fn path(&mut self, segments: &[Ident], span: Span) {
        for (i, seg) in segments.iter().enumerate() {
            let qualifier = segments[..i].iter().map(|s| s.name.clone()).collect();
            self.paths.push(seg.span, qualifier);
        }
        if let Some(last) = segments.last()
            && let Some(gap) = gap_past_colon_colon(last.span, span)
        {
            let all = segments.iter().map(|s| s.name.clone()).collect();
            self.paths.push(gap, all);
        }
    }
}

/// The empty region a trailing `::` opened, if `whole` runs past `name` by
/// at least the two characters that spell it.
///
/// `math::` parses as one segment whose span stops short of the
/// declaration's or expression's, because the `::` was consumed with
/// nothing after it — and that gap is exactly where the caret sits when an
/// editor asks what belongs to the module.
fn gap_past_colon_colon(name: Span, whole: Span) -> Option<Span> {
    let after = name.hi + 2;
    (whole.hi >= after).then(|| Span::new(after, whole.hi))
}

// -------------------------------------------------------------- the view

/// Everything an editor question needs at once: where things are, what the
/// checker made of them, and what the host registered.
///
/// Cheap to build (three references), so a language server makes one per
/// request rather than threading three arguments through its own helpers.
pub struct Editor<'a> {
    index: &'a Index,
    check: &'a CheckResult,
    reg: &'a Registry,
}

impl<'a> Editor<'a> {
    pub fn new(index: &'a Index, check: &'a CheckResult, reg: &'a Registry) -> Editor<'a> {
        Editor { index, check, reg }
    }

    pub fn index(&self) -> &'a Index {
        self.index
    }

    pub fn defs(&self) -> &'a DefTable {
        &self.check.defs
    }

    /// What the source at `offset` names.
    pub fn symbol_at(&self, offset: u32) -> Option<Symbol<'a>> {
        // Declared names first: they are always identifiers, and where one
        // sits inside a wider expression (a closure parameter's type
        // annotation) it is the narrower, better answer.
        if let Some((span, named)) = self.index.named_at(offset)
            && let Some(symbol) = self.named_symbol(span, named)
        {
            return Some(symbol);
        }
        let node = self.index.node_at(offset)?;
        Some(Symbol {
            span: self.index.span_of(node),
            ty: self.check.types.get(&node).cloned(),
            def_span: self.check.def_spans.get(&node).copied(),
            host: self.host_at(offset),
        })
    }

    /// Everything that could be written at `offset`.
    pub fn completions_at(&self, offset: u32) -> Vec<Completion> {
        match self.index.context_at(offset) {
            Context::Member(recv) => match self.check.types.get(&recv) {
                Some(ty) => self.members_of(ty),
                None => Vec::new(),
            },
            Context::Qualified(qualifier) => self.qualified(qualifier),
            Context::Toplevel => self.toplevel(),
        }
    }

    /// Every member reachable through `.` on a value of type `ty`:
    /// builtin methods, script and host methods, struct fields, and the
    /// units of a family.
    ///
    /// Keyed by type rather than by `DefId` because most receivers that
    /// have members are not nominal — `string`, `List`, `Map`, `Option`,
    /// `Result`, `weak` and `dyn Trait` between them cover most of what a
    /// `.` is typed after, and none of them is a def. And members rather
    /// than methods, because `.` reaches fields and units too, and an
    /// editor asking what may follow a dot wants one answer.
    pub fn members_of(&self, ty: &Type) -> Vec<Completion> {
        let defs = self.defs();
        let mut out = Vec::new();
        for (name, sig) in builtin_methods(ty) {
            out.push(Completion::new(
                name,
                CompletionKind::Method,
                Some(render_sig(&sig, None, defs)),
            ));
        }
        match ty {
            Type::Named(def) => {
                if let Some(methods) = self.check.methods_by_type.get(def) {
                    for (name, sig) in methods {
                        out.push(Completion::new(
                            name,
                            CompletionKind::Method,
                            Some(render_sig(sig, None, defs)),
                        ));
                    }
                }
                for m in self.reg.methods_of(*def) {
                    out.push(Completion::new(
                        &m.name,
                        CompletionKind::Method,
                        Some(render_sig(&m.sig, m.param_names(), defs)),
                    ));
                }
                if let Some(s) = defs.as_struct(*def)
                    && !s.opaque
                {
                    for (name, fty) in &s.fields {
                        out.push(Completion::new(
                            name,
                            CompletionKind::Field,
                            Some(fty.display(defs)),
                        ));
                    }
                }
                // Units of a family: `d.` offers `ms`, `s`, `min`, …
                if let Some(u) = defs.as_unit(*def) {
                    let base = u.base.display(defs);
                    for (name, factor) in &u.units {
                        out.push(Completion::new(
                            name,
                            CompletionKind::UnitMember,
                            Some(format!("-> {base} (1 {name} = {})", factor.display())),
                        ));
                    }
                }
            }
            Type::Dyn(tr) => {
                if let Some(td) = defs.as_trait(*tr) {
                    for (name, sig) in &td.methods {
                        out.push(Completion::new(
                            name,
                            CompletionKind::Method,
                            Some(render_sig(sig, None, defs)),
                        ));
                    }
                }
            }
            _ => {}
        }
        out
    }

    /// Members of `qualifier::` — a host module's functions and constants,
    /// or an enum's variants.
    fn qualified(&self, qualifier: &[String]) -> Vec<Completion> {
        let defs = self.defs();
        let Some(name) = qualifier.last() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(module) = self.reg.module(name) {
            for f in &module.fns {
                let sig = render_sig(&f.sig, f.param_names(), defs);
                let doc = f
                    .doc
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default();
                out.push(Completion::new(
                    &f.name,
                    CompletionKind::Function,
                    Some(format!("{sig}{doc}")),
                ));
            }
            for (cname, ty, _) in &module.consts {
                out.push(Completion::new(
                    cname,
                    CompletionKind::Constant,
                    Some(ty.display(defs)),
                ));
            }
        }
        for def in &defs.defs {
            if let DefKind::Enum(e) = def
                && &e.name == name
            {
                for v in &e.variants {
                    out.push(Completion::new(&v.name, CompletionKind::EnumMember, None));
                }
            }
        }
        out
    }

    /// Keywords, prelude functions, script functions, host modules and
    /// every type in scope.
    fn toplevel(&self) -> Vec<Completion> {
        let defs = self.defs();
        let mut out = Vec::new();
        for k in SCRIPT_KEYWORDS {
            out.push(Completion::new(k, CompletionKind::Keyword, None));
        }
        for p in PreludeFn::ALL {
            out.push(Completion::new(p.name(), CompletionKind::Function, None));
        }
        for (name, (_, sig)) in &self.check.exports {
            out.push(Completion::new(
                name,
                CompletionKind::Function,
                Some(render_sig(sig, None, defs)),
            ));
        }
        for module in self.reg.modules() {
            out.push(Completion::new(
                &module.name,
                CompletionKind::Module,
                module.doc.clone(),
            ));
        }
        for def in &defs.defs {
            let (name, kind, detail) = match def {
                DefKind::Struct(s) => (&s.name, CompletionKind::Struct, None),
                DefKind::Enum(e) => (&e.name, CompletionKind::Enum, None),
                DefKind::Trait(t) => (&t.name, CompletionKind::Trait, None),
                DefKind::Unit(u) => (
                    &u.name,
                    CompletionKind::UnitFamily,
                    Some(format!(
                        "unit family, base `{}` ({})",
                        u.base_name(),
                        u.base.display(defs)
                    )),
                ),
            };
            out.push(Completion::new(name, kind, detail));
        }
        out
    }

    /// The host registration named at `offset`, if any — the first of the
    /// candidate nodes that resolves to one, innermost first. Hover and
    /// goto-definition want the same answer, and both want exactly one.
    fn host_at(&self, offset: u32) -> Option<HostRef<'a>> {
        self.index.host_nodes_at(offset).into_iter().find_map(|n| {
            let idx = match (self.check.call(n), self.check.method(n)) {
                (Some(CallKind::Host(idx)), _) => *idx,
                (_, Some(MethodRes::Host(idx))) => *idx,
                _ => return None,
            };
            self.reg.host_ref(idx)
        })
    }

    fn named_symbol(&self, span: Span, named: &Named) -> Option<Symbol<'a>> {
        let defs = self.defs();
        let (ty, def_span) = match named {
            Named::Type(name) => {
                let ty = self.named_type(name)?;
                let def_span = match ty {
                    Type::Named(id) | Type::Dyn(id) => self.check.def_decl_spans.get(&id).copied(),
                    _ => None,
                };
                (ty, def_span)
            }
            // A declaration defines itself: goto on it stays put, which is
            // what an editor expects.
            Named::TypeDecl(name) => (self.named_type(name)?, Some(span)),
            Named::Fn(source) => {
                let info = self.check.fn_infos.iter().find(|i| i.source == *source)?;
                (Type::Fn(Box::new(info.sig.clone())), Some(span))
            }
            Named::Member { owner, name } => {
                let id = defs.by_name(owner)?;
                let ty = match defs.defs.get(id.index())? {
                    DefKind::Struct(s) => s
                        .fields
                        .iter()
                        .find(|(f, _)| f == name)
                        .map(|(_, t)| t.clone())?,
                    // A variant and a unit are both described by the family
                    // they belong to.
                    DefKind::Enum(_) | DefKind::Unit(_) => Type::Named(id),
                    DefKind::Trait(t) => t
                        .methods
                        .iter()
                        .find(|(m, _)| m == name)
                        .map(|(_, sig)| Type::Fn(Box::new(sig.clone())))?,
                };
                (ty, Some(span))
            }
        };
        Some(Symbol {
            span: Some(span),
            ty: Some(ty),
            def_span,
            host: None,
        })
    }

    /// The type a written type name denotes.
    ///
    /// The primitives come from [`resolve::primitive_named`], the same
    /// list the checker and the `.wscripti` loader resolve against.
    /// Unlike them, a bare trait name answers `dyn Trait` rather than an
    /// error: this is asked of a name already written — in an `impl` head
    /// or a `dyn` annotation — and the question is what it denotes, not
    /// whether it would be well-formed as a value's type.
    fn named_type(&self, name: &str) -> Option<Type> {
        if let Some(primitive) = resolve::primitive_named(name) {
            return Some(primitive);
        }
        let defs = self.defs();
        let id = defs.by_name(name)?;
        Some(match defs.defs.get(id.index())? {
            DefKind::Trait(_) => Type::Dyn(id),
            _ => Type::Named(id),
        })
    }
}

/// A signature for display. Declared parameter names are shown
/// (`(y: float, x: float)` — the point of declaring them); where none were
/// declared the types stand alone rather than gaining an invented name.
pub fn render_sig(sig: &FnSig, names: Option<&[String]>, defs: &DefTable) -> String {
    let params: Vec<String> = sig
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| match names.and_then(|names| names.get(i)) {
            Some(name) => format!("{name}: {}", p.display(defs)),
            None => p.display(defs),
        })
        .collect();
    if sig.ret == Type::Unit {
        format!("({})", params.join(", "))
    } else {
        format!("({}) -> {}", params.join(", "), sig.ret.display(defs))
    }
}

// ----------------------------------------------------------- the answers

/// What the source at a position names.
pub struct Symbol<'a> {
    /// The source range described, where it came from a node.
    pub span: Option<Span>,
    /// Its type, when the checker got that far.
    pub ty: Option<Type>,
    /// Where it is defined within this compilation (goto-definition).
    pub def_span: Option<Span>,
    /// The host registration it names, if any. Its declaration lives in a
    /// `.wscripti` interface, which only the caller knows how to locate.
    pub host: Option<HostRef<'a>>,
}

/// One completion candidate.
pub struct Completion {
    pub label: String,
    pub kind: CompletionKind,
    /// Signature, type or documentation — whatever describes it in one
    /// line.
    pub detail: Option<String>,
}

impl Completion {
    fn new(label: &str, kind: CompletionKind, detail: Option<String>) -> Completion {
        Completion {
            label: label.to_string(),
            kind,
            detail,
        }
    }
}

/// What a completion is, in wscript's own terms. A client maps these onto
/// whatever its protocol calls them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Keyword,
    Function,
    Constant,
    Method,
    Field,
    Module,
    Struct,
    Enum,
    EnumMember,
    Trait,
    /// A unit family (`Duration`).
    UnitFamily,
    /// One unit of a family (`ms`).
    UnitMember,
}

#[cfg(test)]
mod tests {
    use wscript_core::registry::Registry;

    use super::*;
    use crate::Analysis;

    /// Analyse a source that marks the caret with `~`. Positions are what
    /// this module is about, so the tests say where the caret is in the
    /// source rather than counting bytes.
    fn at(marked: &str) -> (Analysis, Registry, u32) {
        let offset = marked.find('~').expect("mark the caret with `~`") as u32;
        let source = marked.replace('~', "");
        let registry = Registry::new();
        (crate::analyze(&source, &registry), registry, offset)
    }

    fn ty_at(marked: &str) -> Option<String> {
        let (analysis, registry, offset) = at(marked);
        let editor = analysis.editor(&registry);
        let symbol = editor.symbol_at(offset)?;
        Some(symbol.ty?.display(editor.defs()))
    }

    fn labels_at(marked: &str) -> Vec<String> {
        let (analysis, registry, offset) = at(marked);
        analysis
            .editor(&registry)
            .completions_at(offset)
            .into_iter()
            .map(|c| c.label)
            .collect()
    }

    #[test]
    fn symbol_at_types_an_expression() {
        assert_eq!(
            ty_at("fn main() {\n    let n = 1\n    println(~n)\n}\n"),
            Some("int".to_string())
        );
    }

    /// A pattern binding is not an expression, and used to answer nothing.
    #[test]
    fn symbol_at_types_a_pattern_binding() {
        let src = "fn main() {\n    let v = Some(1)\n    match v {\n        Some(~n) => println(n),\n        None => {}\n    }\n}\n";
        assert_eq!(ty_at(src), Some("int".to_string()));
    }

    #[test]
    fn symbol_at_resolves_a_type_annotation() {
        assert_eq!(
            ty_at("fn main() {\n    let n: ~int = 1\n}\n"),
            Some("int".to_string())
        );
        assert_eq!(
            ty_at("struct P { x: int }\nfn f(p: ~P) -> int { p.x }\n"),
            Some("P".to_string())
        );
    }

    /// Hover on a struct field *declaration* — one of the positions the
    /// old expression-only index could not reach at all.
    #[test]
    fn symbol_at_reaches_a_field_declaration() {
        assert_eq!(
            ty_at("struct P {\n    ~x: int,\n    y: int,\n}\n"),
            Some("int".to_string())
        );
    }

    #[test]
    fn symbol_at_describes_an_item_name() {
        assert_eq!(
            ty_at("fn ~add(a: int, b: int) -> int { a + b }\n"),
            Some("fn(int, int) -> int".to_string())
        );
        assert_eq!(ty_at("struct ~P { x: int }\n"), Some("P".to_string()));
    }

    /// A function is found by *where it is*, not by what it is called —
    /// so the ones a name lookup cannot reach are answered too: methods
    /// inside an `impl` (whose names are not program-unique) and generic
    /// functions (which are never exported).
    #[test]
    fn symbol_at_describes_a_function_a_name_lookup_would_miss() {
        // A script method's recorded signature carries its receiver (a
        // host method's does not — see `HostFnDecl`), and completion has
        // always shown it that way. The declaration reads the same as the
        // use, which is the property worth keeping.
        let src = "struct P { x: int }\nimpl P {\n    fn ~double(self) -> int { self.x * 2 }\n}\n";
        assert_eq!(ty_at(src), Some("fn(P) -> int".to_string()));

        assert_eq!(
            ty_at("fn ~pick[T](a: T, b: T) -> T { a }\n"),
            Some("fn(T, T) -> T".to_string())
        );
    }

    /// A trait's method signature is a declaration too.
    #[test]
    fn symbol_at_describes_a_trait_method_signature() {
        assert_eq!(
            ty_at("trait Draw {\n    fn ~area(self) -> float\n}\n"),
            Some("fn() -> float".to_string())
        );
    }

    /// A declaration defines itself, so goto-definition on one stays put
    /// rather than returning nothing.
    #[test]
    fn symbol_at_gives_a_declaration_its_own_span() {
        let (analysis, registry, offset) = at("struct ~P { x: int }\n");
        let symbol = analysis.editor(&registry).symbol_at(offset).unwrap();
        let span = symbol.def_span.expect("a declaration has a definition");
        assert_eq!(span.lo, offset);
    }

    #[test]
    fn symbol_at_answers_nothing_off_any_node() {
        assert_eq!(ty_at("fn main() {\n~\n}\n"), None);
    }

    /// The whole point of the enumerable tables: every combinator the
    /// checker knows is offered, including the fourteen that were missing
    /// while the editor kept its own copy of the list.
    #[test]
    fn completions_after_a_dot_list_every_builtin_method() {
        let labels = labels_at("fn main() {\n    let xs = [1, 2, 3]\n    xs.~\n}\n");
        for (name, _) in builtin_methods(&Type::List(Box::new(Type::Int))) {
            assert!(labels.contains(&name.to_string()), "`{name}` not offered");
        }
        assert!(labels.contains(&"zip_with".to_string()));
    }

    /// Completion is asked for again on every keystroke, so a partly
    /// typed member has to be recognised as one too.
    #[test]
    fn completions_work_part_way_through_a_member_name() {
        let labels = labels_at("fn main() {\n    let s = \"x\"\n    s.to_u~\n}\n");
        assert!(labels.contains(&"to_upper".to_string()), "{labels:?}");
    }

    #[test]
    fn completions_after_a_dot_list_fields_and_methods() {
        let src = "struct P { x: int }\nimpl P {\n    fn double(self) -> int { self.x * 2 }\n}\nfn main() {\n    let p = P { x: 1 }\n    p.~\n}\n";
        let labels = labels_at(src);
        assert!(labels.contains(&"x".to_string()), "field: {labels:?}");
        assert!(labels.contains(&"double".to_string()), "method: {labels:?}");
    }

    #[test]
    fn completions_after_colon_colon_list_enum_variants() {
        let src = "enum Colour { Red, Green }\nfn main() {\n    let c = Colour::~\n}\n";
        let labels = labels_at(src);
        assert!(labels.contains(&"Red".to_string()), "{labels:?}");
        assert!(labels.contains(&"Green".to_string()), "{labels:?}");
        // Nothing unqualified leaks into a qualified position.
        assert!(!labels.contains(&"match".to_string()), "{labels:?}");
    }

    #[test]
    fn completions_elsewhere_offer_keywords_prelude_and_items() {
        let labels = labels_at("fn helper() {}\nfn main() {\n    hel~\n}\n");
        assert!(labels.contains(&"match".to_string()), "keyword: {labels:?}");
        assert!(
            labels.contains(&"println".to_string()),
            "prelude: {labels:?}"
        );
        assert!(
            labels.contains(&"helper".to_string()),
            "script fn: {labels:?}"
        );
    }
}
