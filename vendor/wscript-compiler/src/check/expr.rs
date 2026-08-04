//! Statement and expression checking.

use wscript_core::defs::{self, DefId, DefKind, Factor, VariantKind};
use wscript_core::span::Span;
use wscript_core::types::{FnSig, Type};

use crate::ast::*;

use super::methods::{self, SchemeConstraint};
use super::{
    BinOpKind, CallKind, Checker, ConvKind, ForKind, IndexKind, MethodRes, PathRes, PreludeFn,
    PrimKind, StructLitRes, TryKind, UnOpKind,
};

/// AST-depth budget for `check_expr` — the backstop behind the parser's
/// `MAX_NESTING_DEPTH`, sized so the checker stays well within the LSP's
/// smaller tokio stacks.
const MAX_EXPR_DEPTH: u32 = 500;

impl<'a> Checker<'a> {
    pub(crate) fn check_block(&mut self, block: &Block, expect: Option<&Type>) -> Type {
        self.push_scope();
        let n = block.stmts.len();
        let mut diverged = false;
        let mut tail: Option<Type> = None;
        for (i, stmt) in block.stmts.iter().enumerate() {
            let last = i + 1 == n;
            match stmt {
                Stmt::Let {
                    name,
                    ty,
                    init,
                    span,
                    id,
                } => {
                    let ann = ty.as_ref().map(|t| self.resolve_type(t));
                    let init_ty = match &ann {
                        Some(expected) => {
                            // Annotation is the source of truth; the
                            // initializer must fit it (incl. dyn coercion).
                            self.check_coerce(init, expected);
                            expected.clone()
                        }
                        None => self.check_expr(init, None),
                    };
                    let var_ty = ann.unwrap_or(init_ty.clone());
                    if matches!(self.resolve(&init_ty), Type::Never) {
                        diverged = true;
                    }
                    let local = self.declare_local(name, var_ty.clone());
                    self.out.decl_locals.insert(*id, local);
                    self.record_type(*id, var_ty);
                    self.require_resolved(*id, *span);
                }
                Stmt::LetElse {
                    pat,
                    init,
                    else_block,
                    span,
                    id,
                } => {
                    let init_ty = self.check_expr(init, None);
                    // The else block must diverge (PRD §3.4).
                    let else_ty = self.check_block(else_block, None);
                    if !matches!(self.resolve(&else_ty), Type::Never) {
                        let span = else_block.span;
                        self.error_help(
                            "E0222",
                            span,
                            "the `else` block of `let ... else` must diverge",
                            "end it with `return`, `break`, or `continue`",
                        );
                    }
                    if !self.pattern_is_refutable(pat, &init_ty) {
                        self.warn(
                            "W0001",
                            *span,
                            "irrefutable pattern in `let ... else`: the else block never runs",
                        );
                    }
                    // Bindings live in the enclosing scope.
                    self.check_pattern(pat, &init_ty);
                    self.record_type(*id, Type::Unit);
                }
                Stmt::Expr { expr, terminated } => {
                    if last && !*terminated {
                        let t = self.check_expr(expr, expect);
                        tail = Some(t);
                    } else {
                        let t = self.check_expr(expr, None);
                        if matches!(self.resolve(&t), Type::Never) {
                            diverged = true;
                        }
                    }
                }
            }
        }
        self.pop_scope();
        match tail {
            Some(t) => t,
            None if diverged => Type::Never,
            None => Type::Unit,
        }
    }

    pub(crate) fn resolve(&self, t: &Type) -> Type {
        self.infer.resolve(t)
    }

    fn require_resolved(&mut self, node: NodeId, span: Span) {
        self.must_resolve.push((node, span));
    }

    /// Check `e`, then make it fit `expected` (unification plus the one
    /// implicit coercion: concrete type → `dyn Trait` at typed boundaries,
    /// PRD §3.7).
    fn check_coerce(&mut self, e: &Expr, expected: &Type) -> Type {
        let found = self.check_expr(e, Some(expected));
        self.coerce(e.id, e.span, &found, expected);
        expected.clone()
    }

    pub(crate) fn coerce(&mut self, node: NodeId, span: Span, found: &Type, expected: &Type) {
        let exp = self.resolve(expected);
        let fnd = self.resolve(found);
        if let (Type::Dyn(trait_id), Type::Named(concrete)) = (&exp, &fnd) {
            let trait_id = *trait_id;
            let concrete = *concrete;
            match self.vtable_for(concrete, trait_id) {
                Some(vt) => {
                    self.out.dyn_wraps.insert(node, vt);
                }
                None => {
                    let ty_name = self.out.defs.name_of(concrete).to_string();
                    let tr_name = self.out.defs.name_of(trait_id).to_string();
                    self.error_help(
                        "E0223",
                        span,
                        format!("`{ty_name}` does not implement trait `{tr_name}`"),
                        format!("add `impl {tr_name} for {ty_name} {{ ... }}`"),
                    );
                }
            }
            return;
        }
        self.unify_or_err(
            expected,
            found,
            span,
            "the value's type must match what the context expects",
        );
    }

    pub(crate) fn check_expr(&mut self, e: &Expr, expect: Option<&Type>) -> Type {
        if self.expr_depth >= MAX_EXPR_DEPTH {
            if !self.expr_depth_reported {
                self.expr_depth_reported = true;
                self.error(
                    "E0271",
                    e.span,
                    format!("expression is nested more than {MAX_EXPR_DEPTH} levels deep"),
                );
            }
            return self.record_type(e.id, Type::Error);
        }
        self.expr_depth += 1;
        let ty = self.check_expr_inner(e, expect);
        self.expr_depth -= 1;
        self.record_type(e.id, ty)
    }

    fn check_expr_inner(&mut self, e: &Expr, expect: Option<&Type>) -> Type {
        match &e.kind {
            ExprKind::IntLit(_) => Type::Int,
            ExprKind::FloatLit(_) => Type::Float,
            ExprKind::BoolLit(_) => Type::Bool,
            ExprKind::CharLit(_) => Type::Char,
            ExprKind::StrLit(_) => Type::Str,
            ExprKind::StrInterp(parts) => {
                // Holes accept any value (same rule as `print`/`str`).
                for p in parts {
                    if let crate::ast::InterpPart::Hole(h) = p {
                        self.check_expr(h, None);
                    }
                }
                Type::Str
            }
            ExprKind::QuantityLit { value, unit } => {
                self.check_quantity_lit(e, *value, unit, expect)
            }
            ExprKind::UnitLit => Type::Unit,
            ExprKind::Error => Type::Error,
            ExprKind::Path(segments) => self.check_path_expr(e, segments),
            ExprKind::Unary { op, expr } => self.check_unary(e, *op, expr),
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(e, *op, lhs, rhs),
            ExprKind::Assign { target, value, op } => self.check_assign(e, target, value, *op),
            ExprKind::Call { callee, args } => self.check_call(e, callee, args, expect),
            ExprKind::MethodCall { recv, name, args } => {
                self.check_method_call(e, recv, name, args)
            }
            ExprKind::Field { obj, name } => self.check_field(e, obj, name),
            ExprKind::Index { obj, idx } => self.check_index(e, obj, idx),
            ExprKind::StructLit { path, fields } => self.check_struct_lit(e, path, fields),
            ExprKind::ListLit(items) => self.check_list_lit(items, expect),
            ExprKind::MapLit(entries) => self.check_map_lit(e, entries, expect),
            ExprKind::If { cond, then, else_ } => {
                self.check_if(cond, then, else_.as_deref(), expect)
            }
            ExprKind::IfLet {
                pat,
                scrutinee,
                then,
                else_,
            } => self.check_if_let(pat, scrutinee, then, else_.as_deref(), expect),
            ExprKind::Match { scrutinee, arms } => self.check_match(e, scrutinee, arms, expect),
            ExprKind::While { cond, body } => {
                let cond_ty = self.check_expr(cond, Some(&Type::Bool));
                self.expect_bool(&cond_ty, cond.span, "a `while` condition");
                self.enter_loop();
                // Loop body values are discarded.
                self.check_block(body, None);
                self.exit_loop();
                Type::Unit
            }
            ExprKind::Loop { body } => {
                self.enter_loop();
                self.check_block(body, None);
                let has_break = self.exit_loop();
                if has_break { Type::Unit } else { Type::Never }
            }
            ExprKind::For { var, iter, body } => self.check_for(e, var, iter, body),
            ExprKind::Range { .. } => {
                self.error_help(
                    "E0225",
                    e.span,
                    "range expressions are only usable as `for` loop iterables in v1",
                    "write `for i in a..b { ... }`",
                );
                Type::Error
            }
            ExprKind::Break => {
                self.mark_break(e.span);
                Type::Never
            }
            ExprKind::Continue => {
                if !self.in_loop() {
                    self.error("E0221", e.span, "`continue` outside of a loop");
                }
                Type::Never
            }
            ExprKind::Return(value) => self.check_return(e, value.as_deref()),
            ExprKind::Block(b) => self.check_block(b, expect),
            ExprKind::Closure { params, ret, body } => {
                self.check_closure(e, params, ret.as_ref(), body, expect)
            }
            ExprKind::Try(inner) => self.check_try(e, inner),
        }
    }

    /// `500ms` — resolve the suffix to a unit family and fold the literal
    /// into a base-unit constant.
    fn check_quantity_lit(
        &mut self,
        e: &Expr,
        value: LitNum,
        unit: &Ident,
        expect: Option<&Type>,
    ) -> Type {
        match self.fold_quantity(e.span, value, unit, expect) {
            Some((def, folded)) => {
                self.out.quantity_lits.insert(e.id, folded);
                Type::Named(def)
            }
            None => Type::Error,
        }
    }

    /// Resolve a suffix and multiply the literal into base units. Shared by
    /// the expression and pattern forms.
    pub(crate) fn fold_quantity(
        &mut self,
        span: Span,
        value: LitNum,
        unit: &Ident,
        expect: Option<&Type>,
    ) -> Option<(DefId, Factor)> {
        let def = self.resolve_unit_suffix(unit, expect)?;
        // The suffix resolved through this family's own table, so both
        // lookups are present by construction.
        let u = self.out.defs.as_unit(def)?;
        let factor = u.factor_of(&unit.name)?;
        let (family, base) = (u.name.clone(), u.base_name().to_string());
        let uname = unit.name.clone();
        let folded = match (value, factor) {
            (LitNum::Int(n), Factor::Int(f)) => match n.checked_mul(f) {
                Some(v) => Some(Factor::Int(v)),
                None => {
                    self.error_help(
                        "E0269",
                        span,
                        format!("`{n}{uname}` overflows `{family}`"),
                        format!("values are stored in `{base}`, and `int` is 64-bit"),
                    );
                    None
                }
            },
            (LitNum::Float(x), Factor::Float(f)) => Some(Factor::Float(x * f)),
            (LitNum::Int(n), Factor::Float(f)) => Some(Factor::Float(n as f64 * f)),
            // A fractional literal in an int-backed family is fine as long
            // as it lands exactly on a whole number of base units.
            (LitNum::Float(x), Factor::Int(f)) => {
                let scaled = x * f as f64;
                // 2^53 — past this an f64 can't name every integer, so the
                // "lands exactly" test stops meaning anything.
                if scaled.fract() == 0.0 && scaled.abs() <= 9_007_199_254_740_992.0 {
                    Some(Factor::Int(scaled as i64))
                } else if scaled.is_finite() && scaled.fract() != 0.0 {
                    self.error_help(
                        "E0269",
                        span,
                        format!("`{x}{uname}` is not a whole number of `{base}`"),
                        format!(
                            "`{family}` is backed by `int`, so every value must land on a \
                             whole `{base}`"
                        ),
                    );
                    None
                } else {
                    self.error_help(
                        "E0269",
                        span,
                        format!("`{x}{uname}` is out of range for `{family}`"),
                        format!("values are stored in `{base}`, and `int` is 64-bit"),
                    );
                    None
                }
            }
        };
        folded.map(|f| (def, f))
    }

    /// Which family does a unit suffix belong to? The expected type decides
    /// when there is one; otherwise the suffix must be unique program-wide.
    pub(crate) fn resolve_unit_suffix(
        &mut self,
        unit: &Ident,
        expect: Option<&Type>,
    ) -> Option<DefId> {
        let candidates = self
            .unit_suffixes
            .get(&unit.name)
            .cloned()
            .unwrap_or_default();
        // An expected unit family wins outright — that is what makes
        // `let t: Duration = 5s` work when another family also has `s`.
        if let Some(Type::Named(want)) = expect.map(|t| self.resolve(t))
            && candidates.contains(&want)
        {
            return Some(want);
        }
        match candidates.as_slice() {
            [only] => Some(*only),
            [] => {
                let n = unit.name.clone();
                let help = match self.nearest_unit(&n) {
                    Some((fam, u)) => format!("did you mean `{u}` (from `{fam}`)?"),
                    None => "declare one with `units Name: int { ... }`".to_string(),
                };
                self.error_help("E0262", unit.span, format!("unknown unit `{n}`"), help);
                None
            }
            many => {
                let n = unit.name.clone();
                let names: Vec<String> = many
                    .iter()
                    .map(|d| format!("`{}`", self.out.defs.name_of(*d)))
                    .collect();
                let first = self.out.defs.name_of(many[0]).to_string();
                self.error_help(
                    "E0260",
                    unit.span,
                    format!("`{n}` is a unit of {}", names.join(" and ")),
                    format!(
                        "annotate the binding (`let x: {first} = ...`) or convert \
                         explicitly with `{first}::{n}(...)`"
                    ),
                );
                None
            }
        }
    }

    /// A unit whose name differs only by case — the usual typo (`MB` for
    /// `MiB`, `S` for `s`).
    fn nearest_unit(&self, name: &str) -> Option<(String, String)> {
        let lower = name.to_lowercase();
        self.unit_suffixes
            .iter()
            .find(|(u, _)| u.to_lowercase() == lower)
            .map(|(u, defs)| (self.out.defs.name_of(defs[0]).to_string(), u.clone()))
    }

    fn check_list_lit(&mut self, items: &[Expr], expect: Option<&Type>) -> Type {
        let elem = match expect.map(|t| self.resolve(t)) {
            Some(Type::List(e)) => *e,
            _ => self.infer.fresh(),
        };
        for item in items {
            self.check_coerce(item, &elem);
        }
        Type::List(Box::new(elem))
    }

    fn check_map_lit(&mut self, e: &Expr, entries: &[(Expr, Expr)], expect: Option<&Type>) -> Type {
        let (key, val) = match expect.map(|t| self.resolve(t)) {
            Some(Type::Map(k, v)) => (*k, *v),
            _ => (self.infer.fresh(), self.infer.fresh()),
        };
        for (k, v) in entries {
            self.check_coerce(k, &key);
            self.check_coerce(v, &val);
        }
        let kr = self.resolve(&key);
        if !matches!(
            kr,
            Type::Int | Type::Bool | Type::Char | Type::Str | Type::Error | Type::Var(_)
        ) {
            let span = entries.first().map(|(k, _)| k.span).unwrap_or(e.span);
            let ks = self.ty_str(&kr);
            self.error_help(
                "E0214",
                span,
                format!("`{ks}` cannot be a map key"),
                "map keys must be int, bool, char, or string",
            );
        }
        Type::Map(Box::new(key), Box::new(val))
    }

    fn check_if(
        &mut self,
        cond: &Expr,
        then: &Block,
        else_: Option<&Expr>,
        expect: Option<&Type>,
    ) -> Type {
        let cond_ty = self.check_expr(cond, Some(&Type::Bool));
        self.expect_bool(&cond_ty, cond.span, "an `if` condition");
        let then_ty = self.check_block(then, expect);
        match else_ {
            None => {
                let tt = self.resolve(&then_ty);
                if !matches!(tt, Type::Unit | Type::Never | Type::Error | Type::Var(_)) {
                    let span = then.span;
                    let ts = self.ty_str(&tt);
                    self.error_help(
                        "E0224",
                        span,
                        format!(
                            "`if` without `else` evaluates to unit, but the branch \
                             has type `{ts}`"
                        ),
                        "add an `else` branch, or discard the value",
                    );
                }
                Type::Unit
            }
            Some(else_expr) => {
                let else_ty = self.check_expr(else_expr, expect);
                self.combine_branches(&then_ty, &else_ty, else_expr.span)
            }
        }
    }

    fn check_if_let(
        &mut self,
        pat: &Pattern,
        scrutinee: &Expr,
        then: &Block,
        else_: Option<&Expr>,
        expect: Option<&Type>,
    ) -> Type {
        let scrut_ty = self.check_expr(scrutinee, None);
        if !self.pattern_is_refutable(pat, &scrut_ty) {
            self.warn(
                "W0001",
                pat.span,
                "irrefutable pattern in `if let`: the branch always runs",
            );
        }
        self.push_scope();
        self.check_pattern(pat, &scrut_ty);
        let then_ty = self.check_block(then, expect);
        self.pop_scope();
        match else_ {
            None => Type::Unit,
            Some(else_expr) => {
                let else_ty = self.check_expr(else_expr, expect);
                self.combine_branches(&then_ty, &else_ty, else_expr.span)
            }
        }
    }

    fn check_return(&mut self, e: &Expr, value: Option<&Expr>) -> Type {
        let ret = self.current_ret();
        match value {
            Some(v) => {
                self.check_coerce(v, &ret);
            }
            None => {
                let rr = self.resolve(&ret);
                if !matches!(rr, Type::Unit | Type::Error | Type::Never) {
                    let rs = self.ty_str(&rr);
                    self.error_help(
                        "E0226",
                        e.span,
                        format!("`return` without a value in a function returning `{rs}`"),
                        "write `return <value>`",
                    );
                }
            }
        }
        Type::Never
    }

    fn expect_bool(&mut self, t: &Type, span: Span, what: &str) {
        let r = self.resolve(t);
        if matches!(r, Type::Bool | Type::Error | Type::Never) {
            return;
        }
        if self.infer.unify(&Type::Bool, t).is_ok() {
            return;
        }
        let ts = self.ty_str(&r);
        self.error_help(
            "E0227",
            span,
            format!("{what} must be `bool`, found `{ts}`"),
            "wscript has no truthiness: write an explicit comparison",
        );
    }

    /// Result type of two branches, treating divergence properly.
    fn combine_branches(&mut self, a: &Type, b: &Type, span: Span) -> Type {
        let ra = self.resolve(a);
        let rb = self.resolve(b);
        if matches!(ra, Type::Never) {
            return rb;
        }
        if matches!(rb, Type::Never) {
            return ra;
        }
        self.unify_or_err(
            &ra,
            &rb,
            span,
            "all branches of an `if`/`match` expression must have the same type",
        );
        self.resolve(&ra)
    }

    // ------------------------------------------------------------- paths

    fn check_path_expr(&mut self, e: &Expr, segments: &[Ident]) -> Type {
        match segments {
            [single] => {
                if let Some((res, ty)) = self.lookup_var(&single.name) {
                    self.out.var_refs.insert(e.id, res);
                    if let Some(span) = self.lookup_var_span(&single.name) {
                        self.out.def_spans.insert(e.id, span);
                    }
                    return ty;
                }
                if single.name == "self" {
                    self.error_help(
                        "E0228",
                        e.span,
                        "`self` is only available inside methods",
                        "methods are functions in `impl` blocks whose first parameter is `self`",
                    );
                    return Type::Error;
                }
                match self.imported(&single.name) {
                    Some(super::ImportedRef::Const(ty, c)) => {
                        self.out.paths.insert(e.id, PathRes::Const(c));
                        return ty;
                    }
                    Some(super::ImportedRef::HostFn(_)) => {
                        self.error_help(
                            "E0229",
                            e.span,
                            "host functions cannot be used as values in v1",
                            "call it directly, or wrap it in a closure: `|x| f(x)`",
                        );
                        return Type::Error;
                    }
                    Some(super::ImportedRef::ScriptFn(proto)) => {
                        return self.script_fn_value(e, proto, &single.name);
                    }
                    None => {}
                }
                if let Some(proto) = self.fn_by_name(&single.name) {
                    return self.script_fn_value(e, proto, &single.name);
                }
                if single.name == "None" {
                    self.out.paths.insert(
                        e.id,
                        PathRes::Variant {
                            def: defs::DEF_OPTION,
                            tag: defs::TAG_NONE,
                        },
                    );
                    return Type::Option(Box::new(self.infer.fresh()));
                }
                if Self::prelude_fn(&single.name).is_some() {
                    self.error_help(
                        "E0229",
                        e.span,
                        format!("`{}` is a built-in function, not a value", single.name),
                        "call it directly, or wrap it in a closure",
                    );
                    return Type::Error;
                }
                let name = single.name.clone();
                self.error_unknown_value(e.span, &name);
                Type::Error
            }
            [first, second] => {
                // module::item
                match self.module_ref(&first.name) {
                    Some(super::ModuleRef::Host(mod_idx)) => {
                        return self.check_module_item(e, mod_idx, second, /*as_value=*/ true);
                    }
                    Some(super::ModuleRef::Script(fi)) => {
                        if let Some(proto) = self.fn_in_file(fi, &second.name) {
                            return self.script_fn_value(e, proto, &second.name);
                        }
                        let module = self.script_module_name(fi).to_string();
                        let name = second.name.clone();
                        self.error_help(
                            "E0201",
                            second.span,
                            format!("script module `{module}` has no function `{name}`"),
                            "only top-level fns can be referenced from script files",
                        );
                        return Type::Error;
                    }
                    None => {}
                }
                // Enum::Variant
                if let Some(def) = self.enum_by_name(&first.name) {
                    return self.unit_variant_value(e, def, second);
                }
                if self.module_is_registered(&first.name) {
                    let name = first.name.clone();
                    self.error_help(
                        "E0230",
                        first.span,
                        format!("module `{name}` is not imported"),
                        format!("add `use {name}` at the top of the script"),
                    );
                    return Type::Error;
                }
                let name = first.name.clone();
                self.error_unknown_value(first.span, &name);
                Type::Error
            }
            [first, second, third] => {
                // module::Enum::Variant — types are ambient, so the module
                // qualifier is accepted but the enum resolves by name.
                if self.module_ref(&first.name).is_some() || self.module_is_registered(&first.name)
                {
                    if let Some(def) = self.enum_by_name(&second.name) {
                        return self.unit_variant_value(e, def, third);
                    }
                    let name = second.name.clone();
                    self.error("E0212", second.span, format!("unknown type `{name}`"));
                    return Type::Error;
                }
                let name = first.name.clone();
                self.error_unknown_value(first.span, &name);
                Type::Error
            }
            _ => {
                self.error("E0231", e.span, "path has too many segments");
                Type::Error
            }
        }
    }

    fn error_unknown_value(&mut self, span: Span, name: &str) {
        self.error_help(
            "E0230",
            span,
            format!("cannot find value `{name}` in this scope"),
            "check the spelling; variables must be declared with `let` before use",
        );
    }

    pub(crate) fn enum_by_name(&self, name: &str) -> Option<DefId> {
        let id = self.type_name(name)?;
        self.out.defs.as_enum(id)?;
        Some(id)
    }

    fn module_item_lookup(
        &self,
        mod_idx: usize,
        name: &str,
    ) -> Option<Result<(FnSig, u32), (Type, wscript_core::bytecode::Const)>> {
        let module = &self.reg.modules[mod_idx];
        if let Some((_, sig, idx, _)) = module.fns.iter().find(|(n, ..)| n == name) {
            return Some(Ok((sig.clone(), *idx)));
        }
        if let Some((_, ty, c)) = module.consts.iter().find(|(n, ..)| n == name) {
            return Some(Err((ty.clone(), c.clone())));
        }
        None
    }

    fn check_module_item(
        &mut self,
        e: &Expr,
        mod_idx: usize,
        item: &Ident,
        as_value: bool,
    ) -> Type {
        match self.module_item_lookup(mod_idx, &item.name) {
            Some(Ok(_)) => {
                if as_value {
                    self.error_help(
                        "E0229",
                        e.span,
                        "host functions cannot be used as values in v1",
                        "call it directly, or wrap it in a closure: `|x| f(x)`",
                    );
                    Type::Error
                } else {
                    unreachable!("call paths handled in check_call")
                }
            }
            Some(Err((ty, c))) => {
                self.out.paths.insert(e.id, PathRes::Const(c));
                ty
            }
            None => {
                let module = self.reg.modules[mod_idx].name.clone();
                let name = item.name.clone();
                self.error_help(
                    "E0201",
                    item.span,
                    format!("module `{module}` has no item `{name}`"),
                    "check the module's `.wscripti` interface for available items",
                );
                Type::Error
            }
        }
    }

    /// `Enum::Variant` used as a value (unit variants only).
    fn unit_variant_value(&mut self, e: &Expr, def: DefId, variant: &Ident) -> Type {
        let Some((tag, vdef_kind, n_fields)) = self.variant_info(def, &variant.name) else {
            let enum_name = self.out.defs.name_of(def).to_string();
            let vname = variant.name.clone();
            self.error_help(
                "E0232",
                variant.span,
                format!("enum `{enum_name}` has no variant `{vname}`"),
                "check the enum declaration for the available variants",
            );
            return Type::Error;
        };
        match vdef_kind {
            VariantKind::Unit => {
                self.out.paths.insert(e.id, PathRes::Variant { def, tag });
                self.enum_value_type(def)
            }
            VariantKind::Tuple => {
                let vname = variant.name.clone();
                self.error_help(
                    "E0233",
                    e.span,
                    format!("variant `{vname}` takes a payload"),
                    format!(
                        "write `{}::{vname}({})`",
                        self.out.defs.name_of(def),
                        (0..n_fields).map(|_| "...").collect::<Vec<_>>().join(", ")
                    ),
                );
                Type::Error
            }
            VariantKind::Struct => {
                let vname = variant.name.clone();
                self.error_help(
                    "E0233",
                    e.span,
                    format!("variant `{vname}` has named fields"),
                    format!("write `{}::{vname} {{ ... }}`", self.out.defs.name_of(def)),
                );
                Type::Error
            }
        }
    }

    pub(crate) fn variant_info(&self, def: DefId, name: &str) -> Option<(u32, VariantKind, usize)> {
        let ed = self.out.defs.as_enum(def)?;
        let (tag, v) = ed
            .variants
            .iter()
            .enumerate()
            .find(|(_, v)| v.name == name)?;
        Some((tag as u32, v.kind, v.fields.len()))
    }

    /// The script-level type of a value of enum `def`. `Option`/`Result`
    /// instantiate fresh payload vars.
    pub(crate) fn enum_value_type(&mut self, def: DefId) -> Type {
        if def == defs::DEF_OPTION {
            Type::Option(Box::new(self.infer.fresh()))
        } else if def == defs::DEF_RESULT {
            Type::Result(Box::new(self.infer.fresh()), Box::new(self.infer.fresh()))
        } else {
            Type::Named(def)
        }
    }

    /// Payload field types of `def::variant`, instantiated against the
    /// scrutinee/constructed type when the enum is Option/Result.
    pub(crate) fn variant_payload_types(&self, def: DefId, tag: u32, enum_ty: &Type) -> Vec<Type> {
        let Some(ed) = self.out.defs.as_enum(def) else {
            return vec![];
        };
        let fields: Vec<Type> = ed.variants[tag as usize]
            .fields
            .iter()
            .map(|(_, t)| t.clone())
            .collect();
        let args: Vec<Type> = match self.resolve(enum_ty) {
            Type::Option(t) => vec![*t],
            Type::Result(t, e) => vec![*t, *e],
            _ => return fields,
        };
        fields
            .iter()
            .map(|t| super::subst_params(t, &args))
            .collect()
    }

    // --------------------------------------------------------- operators

    fn check_unary(&mut self, e: &Expr, op: UnOp, operand: &Expr) -> Type {
        let t = self.check_expr(operand, None);
        let rt = self.resolve(&t);
        match op {
            UnOp::Not => {
                self.expect_bool(&rt, operand.span, "the operand of `!`");
                self.out.un_ops.insert(e.id, UnOpKind::Not);
                Type::Bool
            }
            UnOp::Neg => match rt {
                Type::Int => {
                    self.out.un_ops.insert(e.id, UnOpKind::NegInt);
                    Type::Int
                }
                Type::Float => {
                    self.out.un_ops.insert(e.id, UnOpKind::NegFloat);
                    Type::Float
                }
                Type::Var(_) => {
                    // Default unresolved numeric negation to int.
                    if self.infer.unify(&Type::Int, &t).is_ok() {
                        self.out.un_ops.insert(e.id, UnOpKind::NegInt);
                        Type::Int
                    } else {
                        Type::Error
                    }
                }
                // A unit value negates as the number it is stored in.
                Type::Named(def) if self.out.defs.is_quantity(def) => {
                    let kind = if self.base_of(def) == Type::Float {
                        UnOpKind::NegFloat
                    } else {
                        UnOpKind::NegInt
                    };
                    self.out.un_ops.insert(e.id, kind);
                    Type::Named(def)
                }
                Type::Named(def) => {
                    if let Some(protos) = self.trait_impls.get(&(def, defs::TRAIT_NEG)) {
                        let proto = protos[0];
                        self.out.un_ops.insert(e.id, UnOpKind::NegCall { proto });
                        Type::Named(def)
                    } else {
                        let name = self.out.defs.name_of(def).to_string();
                        self.error_help(
                            "E0234",
                            e.span,
                            format!("cannot negate `{name}`"),
                            format!("implement the `Neg` trait: `impl Neg for {name}`"),
                        );
                        Type::Error
                    }
                }
                Type::Error | Type::Never => Type::Error,
                other => {
                    let ts = self.ty_str(&other);
                    self.error("E0234", e.span, format!("cannot negate `{ts}`"));
                    Type::Error
                }
            },
        }
    }

    fn check_binary(&mut self, e: &Expr, op: BinOp, lhs: &Expr, rhs: &Expr) -> Type {
        use BinOp::*;
        match op {
            And | Or => {
                let lt = self.check_expr(lhs, Some(&Type::Bool));
                self.expect_bool(&lt, lhs.span, "the left operand of a logical operator");
                let rt = self.check_expr(rhs, Some(&Type::Bool));
                self.expect_bool(&rt, rhs.span, "the right operand of a logical operator");
                self.out.bin_ops.insert(
                    e.id,
                    if op == And {
                        BinOpKind::And
                    } else {
                        BinOpKind::Or
                    },
                );
                Type::Bool
            }
            Add | Sub | Mul | Div | Rem => self.check_arith(e, op, lhs, rhs),
            Eq | Ne => self.check_eq(e, op == Ne, lhs, rhs),
            Lt | Le | Gt | Ge => self.check_cmp(e, op, lhs, rhs),
        }
    }

    fn check_arith(&mut self, e: &Expr, op: BinOp, lhs: &Expr, rhs: &Expr) -> Type {
        let lt = self.check_expr(lhs, None);
        // Unit families scale by, and divide into, their backing number, so
        // their operands do not have to match. Everything else does.
        if let Some(ty) = self.check_unit_arith(e, op, &lt, rhs) {
            return ty;
        }
        let rt = self.check_expr(rhs, Some(&lt));
        self.unify_or_err(
            &lt,
            &rt,
            rhs.span,
            "arithmetic requires both operands to have the same type \
             (use `int(x)` / `float(x)` to convert)",
        );
        self.arith_result(e.id, e.span, op, &lt)
    }

    /// Arithmetic where either side is a unit family:
    ///
    /// ```text
    /// D + D → D    D - D → D    D % D → D
    /// D * n → D    n * D → D    D / n → D
    /// D / D → n                 (n is the backing type)
    /// ```
    ///
    /// Returns `None` when neither side is a unit family, so the caller
    /// falls through to the ordinary same-type rule.
    fn check_unit_arith(&mut self, e: &Expr, op: BinOp, lt: &Type, rhs: &Expr) -> Option<Type> {
        // `n * D` — the literal-first form. Only `*` makes sense here:
        // `2 / 5s` and `2 + 5s` have no meaning without dimensions.
        let Some(def) = self.unit_family(lt) else {
            if !matches!(lt, Type::Int | Type::Float) {
                return None;
            }
            let rt = self.check_expr(rhs, None);
            let def = self.unit_family(&rt)?;
            let base = self.base_of(def);
            if op != BinOp::Mul {
                let (n, ts) = (self.out.defs.name_of(def).to_string(), self.ty_str(lt));
                self.error_help(
                    "E0234",
                    e.span,
                    format!("no `{}` operator for `{ts}` and `{n}`", op_symbol(op)),
                    format!("a number can only scale a unit value: `{n} * n` or `n * {n}`"),
                );
                return Some(Type::Error);
            }
            self.unify_or_err(
                &base,
                lt,
                e.span,
                "scaling a unit value requires its backing type",
            );
            self.record_arith(e.id, &base, op);
            return Some(Type::Named(def));
        };

        let base = self.base_of(def);
        let self_ty = Type::Named(def);
        match op {
            // Same-family combination, or scaling by a plain number — the
            // right operand decides which.
            BinOp::Add | BinOp::Sub | BinOp::Rem => {
                let rt = self.check_expr(rhs, Some(&self_ty));
                self.unify_or_err(
                    &self_ty,
                    &rt,
                    rhs.span,
                    "both operands must be values of the same unit family",
                );
                self.record_arith(e.id, &base, op);
                Some(self_ty)
            }
            BinOp::Mul => {
                let rt = self.check_expr(rhs, Some(&base));
                if self.unit_family(&rt).is_some() {
                    let n = self.out.defs.name_of(def).to_string();
                    let rn = self.ty_str(&rt);
                    self.error_help(
                        "E0234",
                        e.span,
                        format!("cannot multiply `{n}` by `{rn}`"),
                        "multiplying two unit values would produce a new dimension, \
                         which this release does not model — scale by a plain number \
                         instead",
                    );
                    return Some(Type::Error);
                }
                self.unify_or_err(
                    &base,
                    &rt,
                    rhs.span,
                    "scaling a unit value requires its backing type",
                );
                self.record_arith(e.id, &base, op);
                Some(self_ty)
            }
            // `D / D` is a plain ratio; `D / n` scales down.
            BinOp::Div => {
                let rt = self.check_expr(rhs, None);
                match self.unit_family(&rt) {
                    Some(other) if other == def => {
                        self.record_arith(e.id, &base, op);
                        Some(base)
                    }
                    Some(other) => {
                        let (a, b) = (
                            self.out.defs.name_of(def).to_string(),
                            self.out.defs.name_of(other).to_string(),
                        );
                        self.error_help(
                            "E0234",
                            e.span,
                            format!("cannot divide `{a}` by `{b}`"),
                            "dividing across unit families would produce a new dimension, \
                             which this release does not model",
                        );
                        Some(Type::Error)
                    }
                    None => {
                        self.unify_or_err(
                            &base,
                            &rt,
                            rhs.span,
                            "dividing a unit value requires its own family or its \
                             backing type",
                        );
                        self.record_arith(e.id, &base, op);
                        Some(self_ty)
                    }
                }
            }
            _ => None,
        }
    }

    /// Record the int/float lowering for an operator on a unit family —
    /// the values are already plain numbers at runtime.
    fn record_arith(&mut self, node: NodeId, base: &Type, op: BinOp) {
        let kind = if *base == Type::Float {
            BinOpKind::FloatArith(op)
        } else {
            BinOpKind::IntArith(op)
        };
        self.out.bin_ops.insert(node, kind);
    }

    /// The primitive a unit family's values are stored in.
    ///
    /// Only call with a def that `unit_family`/`is_quantity` produced;
    /// anything else is a checker bug, and `Error` keeps it from cascading.
    pub(crate) fn base_of(&self, def: DefId) -> Type {
        match self.out.defs.as_unit(def) {
            Some(u) => u.base.clone(),
            None => Type::Error,
        }
    }

    /// Resolve `t`, replacing a unit family with the primitive it is
    /// stored in.
    pub(crate) fn backing_type(&mut self, t: &Type) -> Type {
        let t = self.resolve(t);
        match self.unit_family(&t) {
            Some(def) => self.base_of(def),
            None => t,
        }
    }

    /// The unit family behind a type, if it is one.
    pub(crate) fn unit_family(&mut self, t: &Type) -> Option<DefId> {
        match self.resolve(t) {
            Type::Named(id) if self.out.defs.is_quantity(id) => Some(id),
            _ => None,
        }
    }

    /// Classify an arithmetic operator application over `operand_ty`,
    /// recording the lowering into `bin_ops` under `node` (a Binary expr
    /// or a compound Assign) and returning the result type.
    fn arith_result(&mut self, node: NodeId, span: Span, op: BinOp, operand_ty: &Type) -> Type {
        let t = self.resolve(operand_ty);
        match &t {
            Type::Int => {
                self.out.bin_ops.insert(node, BinOpKind::IntArith(op));
                Type::Int
            }
            Type::Float => {
                self.out.bin_ops.insert(node, BinOpKind::FloatArith(op));
                Type::Float
            }
            Type::Str if op == BinOp::Add => {
                self.out.bin_ops.insert(node, BinOpKind::Concat);
                Type::Str
            }
            Type::Var(_) => {
                // Unconstrained operands (e.g. closure params used only
                // here) default to int.
                if self.infer.unify(&Type::Int, &t).is_ok() {
                    self.out.bin_ops.insert(node, BinOpKind::IntArith(op));
                    Type::Int
                } else {
                    Type::Error
                }
            }
            // Reached through paths that bypass `check_unit_arith` (both
            // operands already known equal). Same lowering: plain numbers.
            Type::Named(def) if self.out.defs.is_quantity(*def) => {
                let def = *def;
                let base = self.base_of(def);
                self.record_arith(node, &base, op);
                if op == BinOp::Div {
                    base
                } else {
                    Type::Named(def)
                }
            }
            Type::Named(def) => {
                let def = *def;
                let trait_id = match op {
                    BinOp::Add => defs::TRAIT_ADD,
                    BinOp::Sub => defs::TRAIT_SUB,
                    BinOp::Mul => defs::TRAIT_MUL,
                    BinOp::Div => defs::TRAIT_DIV,
                    _ => defs::TRAIT_REM,
                };
                if let Some(protos) = self.trait_impls.get(&(def, trait_id)) {
                    let proto = protos[0];
                    self.out
                        .bin_ops
                        .insert(node, BinOpKind::ArithCall { proto });
                    Type::Named(def)
                } else {
                    let name = self.out.defs.name_of(def).to_string();
                    let tr = self.out.defs.name_of(trait_id).to_string();
                    self.error_help(
                        "E0234",
                        span,
                        format!("no `{}` operator for `{name}`", op_symbol(op)),
                        format!("implement the `{tr}` trait: `impl {tr} for {name}`"),
                    );
                    Type::Error
                }
            }
            Type::Param(i) => {
                let pn = self.param_name(*i);
                self.error_help(
                    "E0253",
                    span,
                    format!(
                        "no `{}` operator for the type parameter `{pn}`",
                        op_symbol(op)
                    ),
                    "arithmetic bounds on type parameters arrive in a later release; \
                     take concrete numeric types for now",
                );
                Type::Error
            }
            Type::Error | Type::Never => Type::Error,
            other => {
                let ts = self.ty_str(other);
                let help = if matches!(other, Type::Str) {
                    "strings support `+` for concatenation only"
                } else {
                    "arithmetic operators work on int and float \
                     (and types implementing the operator traits)"
                };
                self.error_help(
                    "E0234",
                    span,
                    format!("no `{}` operator for `{ts}`", op_symbol(op)),
                    help,
                );
                Type::Error
            }
        }
    }

    fn check_eq(&mut self, e: &Expr, negate: bool, lhs: &Expr, rhs: &Expr) -> Type {
        let lt = self.check_expr(lhs, None);
        let rt = self.check_expr(rhs, Some(&lt));
        self.unify_or_err(
            &lt,
            &rt,
            rhs.span,
            "both sides of a comparison must have the same type",
        );
        // Unit values compare as the number they are stored in; the unify
        // above has already ruled out mixing families.
        let t = self.backing_type(&lt);
        let kind = match &t {
            Type::Int => Some(PrimKind::Int),
            Type::Float => Some(PrimKind::Float),
            Type::Bool => Some(PrimKind::Bool),
            Type::Char => Some(PrimKind::Char),
            Type::Str => Some(PrimKind::Str),
            _ => None,
        };
        if let Some(kind) = kind {
            self.out
                .bin_ops
                .insert(e.id, BinOpKind::EqPrim { kind, negate });
            return Type::Bool;
        }
        match &t {
            Type::Named(def) => {
                let def = *def;
                if let Some(&proto) = self.out.impl_maps.eq.get(&def.0) {
                    self.out
                        .bin_ops
                        .insert(e.id, BinOpKind::EqCall { proto, negate });
                    Type::Bool
                } else if self.named_has_eq(def) {
                    self.out.bin_ops.insert(e.id, BinOpKind::EqValue { negate });
                    Type::Bool
                } else {
                    let name = self.out.defs.name_of(def).to_string();
                    self.error_help(
                        "E0235",
                        e.span,
                        format!("`==` on `{name}` requires an `Eq` implementation"),
                        format!(
                            "add `#[derive(Eq)]` to `{name}`, or `impl Eq for {name}`; \
                             for reference identity use `same(a, b)` (PRD §3.7)"
                        ),
                    );
                    Type::Error
                }
            }
            Type::Option(_) | Type::Result(..) | Type::List(_) | Type::Map(..) => {
                if self.eq_able(&t) {
                    self.out.bin_ops.insert(e.id, BinOpKind::EqValue { negate });
                    Type::Bool
                } else {
                    let ts = self.ty_str(&t);
                    self.error_help(
                        "E0235",
                        e.span,
                        format!("`==` on `{ts}` requires the element type to support `==`"),
                        "element types must be primitives, strings, or Eq types",
                    );
                    Type::Error
                }
            }
            Type::Param(i) => {
                if self.param_has_bound(*i, super::BoundKind::Eq) {
                    self.out.bin_ops.insert(e.id, BinOpKind::EqValue { negate });
                    Type::Bool
                } else {
                    let pn = self.param_name(*i);
                    self.error_help(
                        "E0253",
                        e.span,
                        format!("`==` on `{pn}` requires an `Eq` bound"),
                        format!("declare the parameter with a bound: `[{pn}: Eq]`"),
                    );
                    Type::Error
                }
            }
            Type::Unit => {
                self.error_help(
                    "E0235",
                    e.span,
                    "cannot compare unit values",
                    "`unit` has only one value; the comparison is always true",
                );
                Type::Error
            }
            Type::Error | Type::Never | Type::Var(_) => {
                // Unconstrained: accept and lower to structural equality.
                self.out.bin_ops.insert(e.id, BinOpKind::EqValue { negate });
                Type::Bool
            }
            other => {
                let ts = self.ty_str(other);
                self.error_help(
                    "E0235",
                    e.span,
                    format!("`==` is not supported for `{ts}`"),
                    "function, weak and dyn values support `same(a, b)` reference \
                     identity only",
                );
                Type::Error
            }
        }
    }

    fn check_cmp(&mut self, e: &Expr, op: BinOp, lhs: &Expr, rhs: &Expr) -> Type {
        let lt = self.check_expr(lhs, None);
        let rt = self.check_expr(rhs, Some(&lt));
        self.unify_or_err(
            &lt,
            &rt,
            rhs.span,
            "both sides of a comparison must have the same type",
        );
        // Unit values compare as the number they are stored in; the unify
        // above has already ruled out mixing families.
        let t = self.backing_type(&lt);
        let kind = match &t {
            Type::Int => Some(PrimKind::Int),
            Type::Float => Some(PrimKind::Float),
            Type::Char => Some(PrimKind::Char),
            Type::Str => Some(PrimKind::Str),
            _ => None,
        };
        if let Some(kind) = kind {
            self.out
                .bin_ops
                .insert(e.id, BinOpKind::CmpPrim { kind, op });
            return Type::Bool;
        }
        match &t {
            Type::Var(_) => {
                if self.infer.unify(&Type::Int, &t).is_ok() {
                    self.out.bin_ops.insert(
                        e.id,
                        BinOpKind::CmpPrim {
                            kind: PrimKind::Int,
                            op,
                        },
                    );
                    Type::Bool
                } else {
                    Type::Error
                }
            }
            Type::Named(def) => {
                let def = *def;
                if let Some(&proto) = self.out.impl_maps.cmp.get(&def.0) {
                    self.out
                        .bin_ops
                        .insert(e.id, BinOpKind::CmpCall { proto, op });
                    Type::Bool
                } else if self.derives.get(&def).is_some_and(|d| d.ord) {
                    self.out.bin_ops.insert(e.id, BinOpKind::CmpValue { op });
                    Type::Bool
                } else {
                    let name = self.out.defs.name_of(def).to_string();
                    self.error_help(
                        "E0235",
                        e.span,
                        format!("ordering comparison on `{name}` requires `Ord`"),
                        format!("add `#[derive(Eq, Ord)]` to `{name}`, or `impl Ord for {name}`"),
                    );
                    Type::Error
                }
            }
            Type::Param(i) => {
                if self.param_has_bound(*i, super::BoundKind::Ord) {
                    self.out.bin_ops.insert(e.id, BinOpKind::CmpValue { op });
                    Type::Bool
                } else {
                    let pn = self.param_name(*i);
                    self.error_help(
                        "E0253",
                        e.span,
                        format!("ordering comparison on `{pn}` requires an `Ord` bound"),
                        format!("declare the parameter with a bound: `[{pn}: Ord]`"),
                    );
                    Type::Error
                }
            }
            Type::Error | Type::Never => Type::Error,
            other => {
                let ts = self.ty_str(other);
                self.error(
                    "E0235",
                    e.span,
                    format!("ordering comparison is not supported for `{ts}`"),
                );
                Type::Error
            }
        }
    }

    // ------------------------------------------------------- assignments

    fn check_assign(&mut self, e: &Expr, target: &Expr, value: &Expr, op: Option<BinOp>) -> Type {
        let place_ty = match &target.kind {
            ExprKind::Path(segments) if segments.len() == 1 => {
                let target_ty = self.check_expr(target, None);
                if self.out.var_refs.contains_key(&target.id) {
                    Some(target_ty)
                } else {
                    if !matches!(target_ty, Type::Error) {
                        self.error_help(
                            "E0236",
                            target.span,
                            "invalid assignment target",
                            "only variables, fields, and list/map elements can be assigned",
                        );
                    }
                    self.check_expr(value, None);
                    None
                }
            }
            ExprKind::Field { .. } => Some(self.check_expr(target, None)),
            ExprKind::Index { .. } => {
                let elem_ty = self.check_expr(target, None);
                if let Some(IndexKind::UserGet { .. }) = self.out.indexes.get(&target.id) {
                    self.error_help(
                        "E0236",
                        target.span,
                        "cannot assign through a user `Index` impl",
                        "the `Index` trait is read-only in v1",
                    );
                }
                Some(elem_ty)
            }
            _ => {
                self.error_help(
                    "E0236",
                    target.span,
                    "invalid assignment target",
                    "only variables, fields, and list/map elements can be assigned",
                );
                self.check_expr(target, None);
                self.check_expr(value, None);
                None
            }
        };
        if let Some(place_ty) = place_ty {
            match op {
                None => {
                    self.check_coerce(value, &place_ty);
                }
                Some(op) => {
                    // `place op= value` — the operator runs between the
                    // place's current value and `value`; the lowering is
                    // recorded under the Assign node's id.
                    //
                    // Unit places follow the same relaxed operand rules as
                    // the binary form, so `d *= 2` and `d += 500ms` both
                    // work; the result must still land back in the place.
                    if let Some(result) = self.check_unit_arith(e, op, &place_ty, value) {
                        self.unify_or_err(
                            &place_ty,
                            &result,
                            e.span,
                            "compound assignment must produce a value of the place's \
                             own unit family",
                        );
                        return Type::Unit;
                    }
                    let vt = self.check_expr(value, Some(&place_ty));
                    self.unify_or_err(
                        &place_ty,
                        &vt,
                        value.span,
                        "compound assignment requires the value to match the place's \
                         type (use `int(x)` / `float(x)` to convert)",
                    );
                    self.arith_result(e.id, e.span, op, &place_ty);
                }
            }
        }
        Type::Unit
    }

    // ------------------------------------------------------------- calls

    fn check_call(
        &mut self,
        e: &Expr,
        callee: &Expr,
        args: &[Expr],
        expect: Option<&Type>,
    ) -> Type {
        // Path callees resolve to functions/constructors; anything else is
        // a function value.
        if let ExprKind::Path(segments) = &callee.kind {
            if let Some((kind, ret)) = self.resolve_call_path(e, callee, segments, args, expect) {
                self.out.calls.insert(e.id, kind);
                return ret;
            }
            return Type::Error;
        }
        let callee_ty = self.check_expr(callee, None);
        self.check_value_call(e, &callee_ty, callee.span, args)
    }

    fn check_value_call(
        &mut self,
        e: &Expr,
        callee_ty: &Type,
        callee_span: Span,
        args: &[Expr],
    ) -> Type {
        let t = self.resolve(callee_ty);
        match t {
            Type::Fn(sig) => {
                self.check_args(e.span, "this function", &sig.params, args);
                self.out.calls.insert(e.id, CallKind::Value);
                sig.ret.clone()
            }
            Type::Error | Type::Never => Type::Error,
            other => {
                let ts = self.ty_str(&other);
                self.error_help(
                    "E0237",
                    callee_span,
                    format!("`{ts}` is not callable"),
                    "only functions and closures can be called",
                );
                for a in args {
                    self.check_expr(a, None);
                }
                Type::Error
            }
        }
    }

    fn check_args(&mut self, call_span: Span, what: &str, params: &[Type], args: &[Expr]) {
        if params.len() != args.len() {
            self.error_help(
                "E0238",
                call_span,
                format!(
                    "{what} takes {} argument{}, found {}",
                    params.len(),
                    if params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
                "check the function's signature",
            );
        }
        for (i, a) in args.iter().enumerate() {
            match params.get(i) {
                Some(p) => {
                    self.check_coerce(a, &p.clone());
                }
                None => {
                    self.check_expr(a, None);
                }
            }
        }
    }

    /// Resolve a call whose callee is a path. Returns (kind, return type),
    /// or None after reporting an error.
    fn resolve_call_path(
        &mut self,
        e: &Expr,
        callee: &Expr,
        segments: &[Ident],
        args: &[Expr],
        expect: Option<&Type>,
    ) -> Option<(CallKind, Type)> {
        let _ = &expect;
        match segments {
            [single] => {
                // Locals (closure values) shadow functions.
                if let Some((res, ty)) = self.lookup_var(&single.name) {
                    self.out.var_refs.insert(callee.id, res);
                    if let Some(span) = self.lookup_var_span(&single.name) {
                        self.out.def_spans.insert(callee.id, span);
                    }
                    self.record_type(callee.id, ty.clone());
                    let ret = self.check_value_call(e, &ty, callee.span, args);
                    return Some((CallKind::Value, ret));
                }
                match self.imported(&single.name) {
                    Some(super::ImportedRef::HostFn(idx)) => {
                        let sig = self.reg.host_fns[idx as usize].sig.clone();
                        self.check_args(e.span, &format!("`{}`", single.name), &sig.params, args);
                        return Some((CallKind::Host(idx), sig.ret));
                    }
                    Some(super::ImportedRef::ScriptFn(proto)) => {
                        return Some(self.script_fn_call(
                            e,
                            callee,
                            proto,
                            &single.name,
                            args,
                            expect,
                        ));
                    }
                    Some(super::ImportedRef::Const(..)) | None => {}
                }
                if let Some(proto) = self.fn_by_name(&single.name) {
                    return Some(self.script_fn_call(e, callee, proto, &single.name, args, expect));
                }
                // Ambient Option/Result constructors.
                match single.name.as_str() {
                    "Some" => {
                        let t = self.infer.fresh();
                        self.check_args(e.span, "`Some`", std::slice::from_ref(&t), args);
                        return Some((
                            CallKind::Variant {
                                def: defs::DEF_OPTION,
                                tag: defs::TAG_SOME,
                            },
                            Type::Option(Box::new(t)),
                        ));
                    }
                    "Ok" => {
                        let t = self.infer.fresh();
                        self.check_args(e.span, "`Ok`", std::slice::from_ref(&t), args);
                        return Some((
                            CallKind::Variant {
                                def: defs::DEF_RESULT,
                                tag: defs::TAG_OK,
                            },
                            Type::Result(Box::new(t), Box::new(self.infer.fresh())),
                        ));
                    }
                    "Err" => {
                        let t = self.infer.fresh();
                        self.check_args(e.span, "`Err`", std::slice::from_ref(&t), args);
                        return Some((
                            CallKind::Variant {
                                def: defs::DEF_RESULT,
                                tag: defs::TAG_ERR,
                            },
                            Type::Result(Box::new(self.infer.fresh()), Box::new(t)),
                        ));
                    }
                    _ => {}
                }
                if let Some(p) = Self::prelude_fn(&single.name) {
                    let ret = self.check_prelude_call(e, p, args)?;
                    return Some((CallKind::Prelude(p), ret));
                }
                let name = single.name.clone();
                self.error_help(
                    "E0230",
                    single.span,
                    format!("cannot find function `{name}`"),
                    "functions must be declared in the script or imported from a \
                     registered module",
                );
                None
            }
            [first, second] => {
                if let Some(super::ModuleRef::Script(fi)) = self.module_ref(&first.name) {
                    if let Some(proto) = self.fn_in_file(fi, &second.name) {
                        let label = format!("{}::{}", first.name, second.name);
                        return Some(self.script_fn_call(e, callee, proto, &label, args, expect));
                    }
                    let module = self.script_module_name(fi).to_string();
                    let name = second.name.clone();
                    self.error_help(
                        "E0201",
                        second.span,
                        format!("script module `{module}` has no function `{name}`"),
                        "only top-level fns can be called from script files",
                    );
                    return None;
                }
                if let Some(super::ModuleRef::Host(mod_idx)) = self.module_ref(&first.name) {
                    match self.module_item_lookup(mod_idx, &second.name) {
                        Some(Ok((sig, idx))) => {
                            self.check_args(
                                e.span,
                                &format!("`{}::{}`", first.name, second.name),
                                &sig.params,
                                args,
                            );
                            return Some((CallKind::Host(idx), sig.ret));
                        }
                        Some(Err((ty, _))) => {
                            let ts = self.ty_str(&ty);
                            self.error_help(
                                "E0237",
                                e.span,
                                format!(
                                    "`{}::{}` is a constant of type `{ts}`, not a function",
                                    first.name, second.name
                                ),
                                "remove the call parentheses",
                            );
                            return None;
                        }
                        None => {
                            let module = first.name.clone();
                            let name = second.name.clone();
                            self.error_help(
                                "E0201",
                                second.span,
                                format!("module `{module}` has no item `{name}`"),
                                "check the module's `.wscripti` interface for available items",
                            );
                            return None;
                        }
                    }
                }
                if let Some(def) = self.enum_by_name(&first.name) {
                    // Variants win over associated functions on a name
                    // collision (documented).
                    if self.variant_info(def, &second.name).is_some() {
                        return self.check_variant_ctor(e, def, second, args);
                    }
                    if let Some(r) = self.check_assoc_call(e, callee, def, first, second, args) {
                        return Some(r);
                    }
                    // Neither variant nor assoc fn: variant error (E0232).
                    return self.check_variant_ctor(e, def, second, args);
                }
                // Associated functions on structs: `Point::new(...)`.
                if let Some(&def) = self.type_names.get(&first.name) {
                    // Every unit doubles as a converter: `Duration::ms(n)`
                    // builds a value from a number that isn't a literal.
                    if self.out.defs.is_quantity(def)
                        && let Some(r) = self.check_unit_ctor(e, def, second, args)
                    {
                        return Some(r);
                    }
                    if let Some(r) = self.check_assoc_call(e, callee, def, first, second, args) {
                        return Some(r);
                    }
                    let tname = first.name.clone();
                    let fname = second.name.clone();
                    self.error_help(
                        "E0230",
                        second.span,
                        format!("type `{tname}` has no associated function `{fname}`"),
                        "associated functions are `fn`s without `self` declared in an \
                         inherent `impl` block",
                    );
                    return None;
                }
                if self.module_is_registered(&first.name) {
                    let name = first.name.clone();
                    self.error_help(
                        "E0230",
                        first.span,
                        format!("module `{name}` is not imported"),
                        format!("add `use {name}` at the top of the script"),
                    );
                    return None;
                }
                let name = first.name.clone();
                self.error_unknown_value(first.span, &name);
                None
            }
            [first, second, third] => {
                if (self.module_ref(&first.name).is_some()
                    || self.module_is_registered(&first.name))
                    && let Some(def) = self.enum_by_name(&second.name)
                {
                    return self.check_variant_ctor(e, def, third, args);
                }
                let name = first.name.clone();
                self.error_unknown_value(first.span, &name);
                None
            }
            _ => {
                self.error("E0231", e.span, "path has too many segments");
                None
            }
        }
    }

    /// A script fn referenced as a VALUE (`let f = helper` /
    /// `let f = mod::helper`): one erased proto serves generic fns too —
    /// instantiate with fresh vars and let the context bind them.
    fn script_fn_value(&mut self, e: &Expr, proto: u32, name: &str) -> Type {
        self.out.paths.insert(e.id, PathRes::FnValue(proto));
        let info = &self.out.fn_infos[proto as usize];
        self.out.def_spans.insert(e.id, info.span);
        let sig = info.sig.clone();
        if !info.type_params.is_empty() {
            let type_params = info.type_params.clone();
            let subst: Vec<Type> = type_params.iter().map(|_| self.infer.fresh()).collect();
            let inst = FnSig {
                params: sig
                    .params
                    .iter()
                    .map(|p| super::subst_params(p, &subst))
                    .collect(),
                ret: super::subst_params(&sig.ret, &subst),
            };
            self.pending_instantiations
                .push(super::PendingInstantiation {
                    span: e.span,
                    fn_name: name.to_string(),
                    type_params,
                    subst,
                });
            return Type::Fn(Box::new(inst));
        }
        Type::Fn(Box::new(sig))
    }

    /// A call to a script fn by proto (same-file, `use`-imported, or
    /// `module::fn`) — handles generic instantiation uniformly.
    fn script_fn_call(
        &mut self,
        e: &Expr,
        callee: &Expr,
        proto: u32,
        label: &str,
        args: &[Expr],
        expect: Option<&Type>,
    ) -> (CallKind, Type) {
        let info = &self.out.fn_infos[proto as usize];
        self.out.def_spans.insert(callee.id, info.span);
        let sig = info.sig.clone();
        if !info.type_params.is_empty() {
            let type_params = info.type_params.clone();
            let ret = self.check_generic_call(e, label, &type_params, &sig, args, expect);
            return (CallKind::Proto(proto), ret);
        }
        self.check_args(e.span, &format!("`{label}`"), &sig.params, args);
        (CallKind::Proto(proto), sig.ret)
    }

    /// A call to a generic fn: instantiate its type parameters with
    /// fresh inference vars, pre-unify the return type with the expected
    /// type (so return-only parameters resolve under local inference),
    /// check arguments non-closures-first (so closure params are pinned
    /// by the other arguments), then check bounds — deferring to
    /// end-of-function when a parameter is still unresolved (E0252/E0253
    /// there).
    fn check_generic_call(
        &mut self,
        e: &Expr,
        name: &str,
        type_params: &[(String, Option<super::BoundKind>)],
        sig: &FnSig,
        args: &[Expr],
        expect: Option<&Type>,
    ) -> Type {
        let subst: Vec<Type> = type_params.iter().map(|_| self.infer.fresh()).collect();
        let params: Vec<Type> = sig
            .params
            .iter()
            .map(|p| super::subst_params(p, &subst))
            .collect();
        let ret = super::subst_params(&sig.ret, &subst);
        if params.len() != args.len() {
            self.error_help(
                "E0238",
                e.span,
                format!(
                    "`{name}` takes {} argument{}, found {}",
                    params.len(),
                    if params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
                "check the function's signature",
            );
        }
        let is_closure = |a: &Expr| matches!(a.kind, ExprKind::Closure { .. });
        for pass in 0..2 {
            for (i, a) in args.iter().enumerate() {
                if (pass == 0) == is_closure(a) {
                    continue;
                }
                match params.get(i) {
                    Some(p) => {
                        self.check_coerce(a, &p.clone());
                    }
                    None => {
                        self.check_expr(a, None);
                    }
                }
            }
            if pass == 0 {
                // The expected type fills parameters the (non-closure)
                // arguments left open — e.g. return-only parameters
                // (`fn none_of[T]() -> Option[T]`). Argument types win;
                // a conflicting expectation is reported by the caller's
                // own coercion, so failures here are ignored.
                if let Some(exp) = expect {
                    let _ = self.infer.unify(exp, &ret);
                }
            }
        }
        let mut deferred = false;
        for (i, (pname, bound)) in type_params.iter().enumerate() {
            let resolved = self.infer.resolve(&subst[i]);
            if self.infer.contains_unbound(&resolved) {
                deferred = true;
                continue;
            }
            if let Some(b) = bound {
                self.check_bound_satisfied(e.span, name, pname, *b, &resolved);
            }
        }
        if deferred {
            self.pending_instantiations
                .push(super::PendingInstantiation {
                    span: e.span,
                    fn_name: name.to_string(),
                    type_params: type_params.to_vec(),
                    subst,
                });
        }
        ret
    }

    /// `Type::func(args)` — an associated function call, if one is
    /// registered for `def` under that name.
    fn check_assoc_call(
        &mut self,
        e: &Expr,
        callee: &Expr,
        def: DefId,
        ty_name: &Ident,
        fn_name: &Ident,
        args: &[Expr],
    ) -> Option<(CallKind, Type)> {
        let &proto = self.assoc.get(&def)?.get(&fn_name.name)?;
        let info = &self.out.fn_infos[proto as usize];
        self.out.def_spans.insert(callee.id, info.span);
        let sig = info.sig.clone();
        self.check_args(
            e.span,
            &format!("`{}::{}`", ty_name.name, fn_name.name),
            &sig.params,
            args,
        );
        Some((CallKind::Proto(proto), sig.ret))
    }

    /// `Duration::ms(n)` — build a unit value from a number that isn't a
    /// literal. Every unit of the family names one of these.
    fn check_unit_ctor(
        &mut self,
        e: &Expr,
        def: DefId,
        unit: &Ident,
        args: &[Expr],
    ) -> Option<(CallKind, Type)> {
        let u = self.out.defs.as_unit(def)?;
        let factor = u.factor_of(&unit.name)?;
        let (base, family) = (u.base.clone(), u.name.clone());
        let label = format!("`{family}::{}`", unit.name);
        self.check_args(e.span, &label, std::slice::from_ref(&base), args);
        self.out.unit_convs.insert(e.id, ConvKind::In { factor });
        Some((CallKind::UnitConv, Type::Named(def)))
    }

    fn check_variant_ctor(
        &mut self,
        e: &Expr,
        def: DefId,
        variant: &Ident,
        args: &[Expr],
    ) -> Option<(CallKind, Type)> {
        let Some((tag, kind, _)) = self.variant_info(def, &variant.name) else {
            let enum_name = self.out.defs.name_of(def).to_string();
            let vname = variant.name.clone();
            self.error(
                "E0232",
                variant.span,
                format!("enum `{enum_name}` has no variant `{vname}`"),
            );
            return None;
        };
        match kind {
            VariantKind::Tuple => {
                let result_ty = self.enum_value_type(def);
                let payload = self.variant_payload_types(def, tag, &result_ty);
                self.check_args(
                    e.span,
                    &format!("variant `{}`", variant.name),
                    &payload,
                    args,
                );
                Some((CallKind::Variant { def, tag }, result_ty))
            }
            VariantKind::Unit => {
                let vname = variant.name.clone();
                self.error_help(
                    "E0233",
                    e.span,
                    format!("variant `{vname}` takes no payload"),
                    format!(
                        "write `{}::{vname}` without parentheses",
                        self.out.defs.name_of(def)
                    ),
                );
                None
            }
            VariantKind::Struct => {
                let vname = variant.name.clone();
                self.error_help(
                    "E0233",
                    e.span,
                    format!("variant `{vname}` has named fields"),
                    format!(
                        "write `{}::{vname} {{ field: value, ... }}`",
                        self.out.defs.name_of(def)
                    ),
                );
                None
            }
        }
    }

    /// Type-check a prelude (builtin) call. Returns the call's type.
    fn check_prelude_call(&mut self, e: &Expr, p: PreludeFn, args: &[Expr]) -> Option<Type> {
        let arity_err = |me: &mut Self, name: &str, n: &str| {
            me.error_help(
                "E0238",
                e.span,
                format!("`{name}` takes {n}"),
                "see the language tour for the prelude functions",
            );
        };
        match p {
            PreludeFn::Print | PreludeFn::Str => {
                let name = if p == PreludeFn::Print {
                    "print"
                } else {
                    "str"
                };
                if args.len() != 1 {
                    arity_err(self, name, "exactly one argument");
                }
                for a in args {
                    self.check_expr(a, None);
                }
                Some(if p == PreludeFn::Print {
                    Type::Unit
                } else {
                    Type::Str
                })
            }
            PreludeFn::Println => {
                if args.len() > 1 {
                    arity_err(self, "println", "zero or one arguments");
                }
                for a in args {
                    self.check_expr(a, None);
                }
                Some(Type::Unit)
            }
            PreludeFn::Fmt => {
                if args.is_empty() {
                    arity_err(self, "fmt", "a template string plus arguments");
                    return Some(Type::Str);
                }
                let t0 = self.check_expr(&args[0], Some(&Type::Str));
                self.unify_or_err(
                    &Type::Str,
                    &t0,
                    args[0].span,
                    "the first argument of `fmt` is the template string",
                );
                for a in &args[1..] {
                    self.check_expr(a, None);
                }
                // If the template is a literal, validate the placeholder
                // count and every format spec right here at compile time
                // (same grammar the VM applies: wscript_core::fmt_spec).
                if let ExprKind::StrLit(template) = &args[0].kind {
                    match wscript_core::fmt_spec::analyze_template(template) {
                        Ok(placeholders) => {
                            if placeholders != args.len() - 1 {
                                let span = args[0].span;
                                self.error_help(
                                    "E0239",
                                    span,
                                    format!(
                                        "format template has {placeholders} placeholder{} \
                                         but {} argument{} given",
                                        if placeholders == 1 { "" } else { "s" },
                                        args.len() - 1,
                                        if args.len() - 1 == 1 {
                                            " was"
                                        } else {
                                            "s were"
                                        }
                                    ),
                                    "each `{}` or `{:spec}` consumes one argument; escape \
                                     literal braces as `{{` and `}}`",
                                );
                            }
                        }
                        Err(e) => {
                            self.error_help(
                                "E0243",
                                args[0].span,
                                format!("invalid format spec in template: {e}"),
                                "the spec grammar is `{:[[fill]align][0][width][.prec][type]}` \
                                 with align `< ^ >` and type `x X b o`",
                            );
                        }
                    }
                }
                Some(Type::Str)
            }
            PreludeFn::Same => {
                if args.len() != 2 {
                    arity_err(self, "same", "exactly two arguments");
                    for a in args {
                        self.check_expr(a, None);
                    }
                    return Some(Type::Bool);
                }
                let t0 = self.check_expr(&args[0], None);
                let _t1 = self.check_expr(&args[1], Some(&t0));
                Some(Type::Bool)
            }
            PreludeFn::Weak => {
                if args.len() != 1 {
                    arity_err(self, "weak", "exactly one argument");
                    return Some(Type::Error);
                }
                let t = self.check_expr(&args[0], None);
                let rt = self.resolve(&t);
                if !self.is_reference_type(&rt) || matches!(rt, Type::Option(_) | Type::Result(..))
                {
                    let ts = self.ty_str(&rt);
                    self.error_help(
                        "E0213",
                        args[0].span,
                        format!("cannot create a weak reference to `{ts}`"),
                        "weak references apply to structs, enums, List, Map, and functions \
                         (PRD §4.2)",
                    );
                    return Some(Type::Error);
                }
                Some(Type::Weak(Box::new(rt)))
            }
            PreludeFn::Int => {
                if args.len() != 1 {
                    arity_err(self, "int", "exactly one argument");
                    return Some(Type::Int);
                }
                let t = self.check_expr(&args[0], None);
                let rt = self.resolve(&t);
                if !matches!(
                    rt,
                    Type::Int | Type::Float | Type::Char | Type::Error | Type::Never
                ) {
                    let ts = self.ty_str(&rt);
                    self.error_help(
                        "E0240",
                        args[0].span,
                        format!("`int()` cannot convert from `{ts}`"),
                        "int() accepts int, float (truncates), and char (code point); \
                         to parse a string use `s.parse_int()`",
                    );
                }
                Some(Type::Int)
            }
            PreludeFn::Float => {
                if args.len() != 1 {
                    arity_err(self, "float", "exactly one argument");
                    return Some(Type::Float);
                }
                let t = self.check_expr(&args[0], None);
                let rt = self.resolve(&t);
                if !matches!(rt, Type::Int | Type::Float | Type::Error | Type::Never) {
                    let ts = self.ty_str(&rt);
                    self.error_help(
                        "E0240",
                        args[0].span,
                        format!("`float()` cannot convert from `{ts}`"),
                        "float() accepts int and float; to parse a string use \
                         `s.parse_float()`",
                    );
                }
                Some(Type::Float)
            }
        }
    }

    // ------------------------------------------------------ method calls

    fn check_method_call(&mut self, e: &Expr, recv: &Expr, name: &Ident, args: &[Expr]) -> Type {
        let recv_ty = self.check_expr(recv, None);
        let rt = self.resolve(&recv_ty);
        match &rt {
            Type::Error | Type::Never => Type::Error,
            Type::Var(_) => {
                self.error_help(
                    "E0241",
                    recv.span,
                    "cannot call a method on a value of unknown type",
                    "add a type annotation so the receiver's type is known here",
                );
                Type::Error
            }
            Type::Named(def) => self.check_named_method(e, *def, name, args),
            Type::Dyn(trait_id) => self.check_dyn_method(e, *trait_id, name, args),
            Type::Param(i) => {
                let i = *i;
                if name.name == "clone"
                    && args.is_empty()
                    && self.param_has_bound(i, super::BoundKind::Clone)
                {
                    self.out.methods.insert(
                        e.id,
                        MethodRes::Builtin(wscript_core::bytecode::Builtin::DeepClone),
                    );
                    return rt.clone();
                }
                let pn = self.param_name(i);
                let mname = name.name.clone();
                let help = if mname == "clone" {
                    format!("declare the parameter with a bound: `[{pn}: Clone]`")
                } else {
                    "a generic value supports only what its bounds provide \
                     (Eq: `==`; Ord: comparisons; Clone: `.clone()`)"
                        .to_string()
                };
                self.error_help(
                    "E0253",
                    name.span,
                    format!("no method `{mname}` on the type parameter `{pn}`"),
                    help,
                );
                for a in args {
                    self.check_expr(a, None);
                }
                Type::Error
            }
            other => {
                // Builtin container/string/Option/Result/weak methods.
                match methods::builtin_method(other, &name.name) {
                    Some(scheme) => {
                        self.apply_scheme(e, other, &name.name, scheme, name.span, args)
                    }
                    None => {
                        let ts = self.ty_str(other);
                        let mname = name.name.clone();
                        self.error_help(
                            "E0241",
                            name.span,
                            format!("no method `{mname}` on `{ts}`"),
                            "see the stdlib reference for the built-in methods of this type",
                        );
                        for a in args {
                            self.check_expr(a, None);
                        }
                        Type::Error
                    }
                }
            }
        }
    }

    fn apply_scheme(
        &mut self,
        e: &Expr,
        recv_ty: &Type,
        mname: &str,
        scheme: methods::Scheme,
        name_span: Span,
        args: &[Expr],
    ) -> Type {
        // Receiver type parameters + fresh vars for scheme-local params.
        let mut subst: Vec<Type> = match recv_ty {
            Type::List(t) => vec![(**t).clone()],
            Type::Map(k, v) => vec![(**k).clone(), (**v).clone()],
            Type::Option(t) => vec![(**t).clone()],
            Type::Result(t, err) => vec![(**t).clone(), (**err).clone()],
            Type::Weak(t) => vec![(**t).clone()],
            _ => vec![],
        };
        for _ in 0..scheme.fresh {
            let v = self.infer.fresh();
            subst.push(v);
        }
        let params: Vec<Type> = scheme
            .params
            .iter()
            .map(|p| super::subst_params(p, &subst))
            .collect();
        let ret = super::subst_params(&scheme.ret, &subst);
        self.check_args(e.span, &format!("`{mname}`"), &params, args);
        // Element constraints (e.g. `contains` needs comparable elements).
        if let Some(c) = scheme.constraint {
            let elem = self.resolve(subst.first().unwrap_or(&Type::Error));
            let ok = match c {
                SchemeConstraint::EqElem => self.eq_able(&elem),
                SchemeConstraint::OrdElem => self.ord_able(&elem),
                SchemeConstraint::StrElem => {
                    matches!(elem, Type::Str | Type::Error) || matches!(elem, Type::Var(_))
                }
                SchemeConstraint::NumElem => {
                    matches!(elem, Type::Int | Type::Float | Type::Error)
                }
            };
            if !ok {
                let es = self.ty_str(&elem);
                let (msg, help): (String, &str) = match c {
                    SchemeConstraint::EqElem => (
                        format!("`{mname}` requires `{es}` elements to support `==`"),
                        "element types must be primitives, strings, or Eq types",
                    ),
                    SchemeConstraint::OrdElem => (
                        format!("`{mname}` requires orderable elements, but found `{es}`"),
                        "orderable element types are primitives, strings, containers of \
                         orderables, and types with an Ord impl",
                    ),
                    SchemeConstraint::StrElem => (
                        format!("`{mname}` requires `List[string]`, but elements are `{es}`"),
                        "use `.map(...)` to convert elements to strings first",
                    ),
                    SchemeConstraint::NumElem => (
                        format!("`{mname}` requires int or float elements, but found `{es}`"),
                        "annotate the list's element type if it is empty or unresolved",
                    ),
                };
                self.error_help("E0242", name_span, msg, help);
            }
        }
        // `sum` compiles to a typed builtin so an empty List[float] sums
        // to 0.0, not 0 — the VM cannot know the element type at runtime.
        let mut builtin = scheme.builtin;
        if matches!(builtin, wscript_core::bytecode::Builtin::ListSumInt) {
            let elem = self.resolve(subst.first().unwrap_or(&Type::Error));
            if matches!(elem, Type::Float) {
                builtin = wscript_core::bytecode::Builtin::ListSumFloat;
            }
        }
        self.out.methods.insert(e.id, MethodRes::Builtin(builtin));
        ret
    }

    fn check_named_method(&mut self, e: &Expr, def: DefId, name: &Ident, args: &[Expr]) -> Type {
        // 1. Inherent script methods.
        if let Some(&proto) = self.inherent.get(&def).and_then(|m| m.get(&name.name)) {
            let sig = self.out.fn_infos[proto as usize].sig.clone();
            self.check_args(e.span, &format!("`{}`", name.name), &sig.params[1..], args);
            self.out.methods.insert(e.id, MethodRes::Proto(proto));
            return sig.ret;
        }
        // 2. Host-registered methods.
        if let Some(ms) = self.reg.methods.get(&def)
            && let Some(m) = ms.iter().find(|m| m.name == name.name)
        {
            let sig = m.sig.clone();
            let idx = m.host_idx;
            self.check_args(e.span, &format!("`{}`", name.name), &sig.params, args);
            self.out.methods.insert(e.id, MethodRes::Host(idx));
            return sig.ret;
        }
        // 3. Trait-impl methods (static dispatch on the concrete type).
        let mut candidates: Vec<(DefId, usize, u32)> = Vec::new();
        for (&(ty, trait_id), protos) in &self.trait_impls {
            if ty != def {
                continue;
            }
            if let Some(td) = self.out.defs.as_trait(trait_id)
                && let Some(slot) = td.methods.iter().position(|(n, _)| *n == name.name)
            {
                candidates.push((trait_id, slot, protos[slot]));
            }
        }
        if candidates.len() > 1 {
            let traits: Vec<String> = candidates
                .iter()
                .map(|(t, ..)| self.out.defs.name_of(*t).to_string())
                .collect();
            let mname = name.name.clone();
            self.error_help(
                "E0243",
                name.span,
                format!("ambiguous method `{mname}`"),
                format!(
                    "implemented by multiple traits: {}; rename one of the trait methods",
                    traits.join(", ")
                ),
            );
            return Type::Error;
        }
        if let Some((_, _, proto)) = candidates.pop() {
            let sig = self.out.fn_infos[proto as usize].sig.clone();
            self.check_args(e.span, &format!("`{}`", name.name), &sig.params[1..], args);
            self.out.methods.insert(e.id, MethodRes::Proto(proto));
            return sig.ret;
        }
        // 4. Derived clone.
        if name.name == "clone" && self.derives.get(&def).is_some_and(|d| d.clone) {
            self.check_args(e.span, "`clone`", &[], args);
            self.out
                .methods
                .insert(e.id, MethodRes::Builtin(wscript_core::Builtin::DeepClone));
            return Type::Named(def);
        }
        let ty_name = self.out.defs.name_of(def).to_string();
        let mname = name.name.clone();
        let help = if name.name == "clone" {
            format!("add `#[derive(Clone)]` to `{ty_name}` to enable deep cloning")
        } else {
            format!("no inherent, host, or trait method `{mname}` is defined for `{ty_name}`")
        };
        self.error_help(
            "E0241",
            name.span,
            format!("no method `{mname}` on `{ty_name}`"),
            help,
        );
        for a in args {
            self.check_expr(a, None);
        }
        Type::Error
    }

    fn check_dyn_method(&mut self, e: &Expr, trait_id: DefId, name: &Ident, args: &[Expr]) -> Type {
        let Some(td) = self.out.defs.as_trait(trait_id).cloned() else {
            return Type::Error;
        };
        let Some(slot) = td.methods.iter().position(|(n, _)| *n == name.name) else {
            let tr = td.name.clone();
            let mname = name.name.clone();
            self.error_help(
                "E0241",
                name.span,
                format!("no method `{mname}` on `dyn {tr}`"),
                format!(
                    "trait `{tr}` declares: {}",
                    td.methods
                        .iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            for a in args {
                self.check_expr(a, None);
            }
            return Type::Error;
        };
        let sig = td.methods[slot].1.clone();
        self.check_args(e.span, &format!("`{}`", name.name), &sig.params, args);
        self.out
            .methods
            .insert(e.id, MethodRes::Virtual { slot: slot as u16 });
        sig.ret
    }

    // ---------------------------------------------------- fields & index

    fn check_field(&mut self, e: &Expr, obj: &Expr, name: &Ident) -> Type {
        let obj_ty = self.check_expr(obj, None);
        let rt = self.resolve(&obj_ty);
        match &rt {
            Type::Named(def) => match self.out.defs.get(*def) {
                DefKind::Struct(s) => {
                    if s.opaque {
                        let ty_name = s.name.clone();
                        self.error_help(
                            "E0244",
                            name.span,
                            format!(
                                "`{ty_name}` is an opaque host type: fields are not accessible"
                            ),
                            "opaque types expose methods only (PRD §6.2)",
                        );
                        return Type::Error;
                    }
                    match s.fields.iter().position(|(n, _)| *n == name.name) {
                        Some(idx) => {
                            let ty = s.fields[idx].1.clone();
                            self.out.fields.insert(e.id, idx as u16);
                            ty
                        }
                        None => {
                            let ty_name = s.name.clone();
                            let avail: Vec<String> =
                                s.fields.iter().map(|(n, _)| n.clone()).collect();
                            let fname = name.name.clone();
                            self.error_help(
                                "E0244",
                                name.span,
                                format!("no field `{fname}` on `{ty_name}`"),
                                if avail.is_empty() {
                                    format!("`{ty_name}` has no fields")
                                } else {
                                    format!("available fields: {}", avail.join(", "))
                                },
                            );
                            Type::Error
                        }
                    }
                }
                DefKind::Enum(en) => {
                    let ty_name = en.name.clone();
                    let fname = name.name.clone();
                    self.error_help(
                        "E0244",
                        name.span,
                        format!("cannot access field `{fname}` on enum `{ty_name}`"),
                        "destructure the enum with `match` or `if let` to reach variant fields",
                    );
                    Type::Error
                }
                // `d.ms` on a unit value converts out of the family. The
                // receiver's family fixes the lookup, so this is never
                // ambiguous.
                DefKind::Unit(u) => match u.factor_of(&name.name) {
                    Some(factor) => {
                        let base = u.base.clone();
                        self.out.unit_convs.insert(e.id, ConvKind::Out { factor });
                        base
                    }
                    None => {
                        let ty_name = u.name.clone();
                        let avail: Vec<String> = u.units.iter().map(|(n, _)| n.clone()).collect();
                        let fname = name.name.clone();
                        self.error_help(
                            "E0244",
                            name.span,
                            format!("no unit `{fname}` in `{ty_name}`"),
                            format!("units of `{ty_name}`: {}", avail.join(", ")),
                        );
                        Type::Error
                    }
                },
                DefKind::Trait(_) => Type::Error,
            },
            Type::Error | Type::Never => Type::Error,
            other => {
                let ts = self.ty_str(other);
                let fname = name.name.clone();
                self.error_help(
                    "E0244",
                    name.span,
                    format!("`{ts}` has no field `{fname}`"),
                    "only struct values have fields; did you mean a method call \
                     `.{name}()`?"
                        .replace("{name}", &fname),
                );
                Type::Error
            }
        }
    }

    fn check_index(&mut self, e: &Expr, obj: &Expr, idx: &Expr) -> Type {
        let obj_ty = self.check_expr(obj, None);
        let rt = self.resolve(&obj_ty);
        match &rt {
            Type::List(elem) => {
                let it = self.check_expr(idx, Some(&Type::Int));
                self.unify_or_err(&Type::Int, &it, idx.span, "list indices are `int`");
                self.out.indexes.insert(e.id, IndexKind::List);
                (**elem).clone()
            }
            Type::Map(k, v) => {
                self.check_coerce(idx, &k.clone());
                self.out.indexes.insert(e.id, IndexKind::Map);
                (**v).clone()
            }
            Type::Str => {
                self.error_help(
                    "E0245",
                    e.span,
                    "strings cannot be indexed directly",
                    "use `s.chars()` for a List[char] or `s.slice(start, end)` for a \
                     substring",
                );
                self.check_expr(idx, None);
                Type::Error
            }
            Type::Named(def) => {
                let def = *def;
                if let Some(protos) = self.trait_impls.get(&(def, defs::TRAIT_INDEX)) {
                    let proto = protos[0];
                    let sig = self.out.fn_infos[proto as usize].sig.clone();
                    // sig.params[0] = receiver, [1] = index type.
                    let idx_ty = sig.params.get(1).cloned().unwrap_or(Type::Error);
                    self.check_coerce(idx, &idx_ty);
                    self.out.indexes.insert(e.id, IndexKind::UserGet { proto });
                    sig.ret
                } else {
                    let name = self.out.defs.name_of(def).to_string();
                    self.error_help(
                        "E0245",
                        e.span,
                        format!("`{name}` does not support indexing"),
                        format!("implement the `Index` trait: `impl Index for {name}`"),
                    );
                    self.check_expr(idx, None);
                    Type::Error
                }
            }
            Type::Error | Type::Never => {
                self.check_expr(idx, None);
                Type::Error
            }
            other => {
                let ts = self.ty_str(other);
                self.error("E0245", e.span, format!("`{ts}` does not support indexing"));
                self.check_expr(idx, None);
                Type::Error
            }
        }
    }

    // ----------------------------------------------------- struct literal

    fn check_struct_lit(&mut self, e: &Expr, path: &[Ident], fields: &[(Ident, Expr)]) -> Type {
        // Resolve the path: `Type { .. }` or `Enum::Variant { .. }` (with
        // an optional leading module qualifier on the enum).
        let (def, variant): (DefId, Option<&Ident>) = match path {
            [ty] => match self.type_name(&ty.name) {
                Some(def) => (def, None),
                None => {
                    let name = ty.name.clone();
                    self.error("E0212", ty.span, format!("unknown type `{name}`"));
                    self.check_lit_fields_poison(fields);
                    return Type::Error;
                }
            },
            [en, variant] => match self.enum_by_name(&en.name) {
                Some(def) => (def, Some(variant)),
                None => {
                    let name = en.name.clone();
                    self.error("E0212", en.span, format!("unknown enum `{name}`"));
                    self.check_lit_fields_poison(fields);
                    return Type::Error;
                }
            },
            [_module, en, variant] => match self.enum_by_name(&en.name) {
                Some(def) => (def, Some(variant)),
                None => {
                    let name = en.name.clone();
                    self.error("E0212", en.span, format!("unknown enum `{name}`"));
                    self.check_lit_fields_poison(fields);
                    return Type::Error;
                }
            },
            _ => {
                self.error("E0231", e.span, "path has too many segments");
                self.check_lit_fields_poison(fields);
                return Type::Error;
            }
        };

        let (decl_fields, lit_res, result_ty): (Vec<(String, Type)>, StructLitRes, Type) =
            match variant {
                None => match self.out.defs.get(def) {
                    DefKind::Struct(s) => {
                        if s.opaque {
                            let name = s.name.clone();
                            self.error_help(
                                "E0246",
                                e.span,
                                format!(
                                    "`{name}` is an opaque host type and cannot be \
                                     constructed in script"
                                ),
                                "opaque values are created by host functions (PRD §6.2)",
                            );
                            self.check_lit_fields_poison(fields);
                            return Type::Error;
                        }
                        (
                            s.fields.clone(),
                            StructLitRes::Struct(def),
                            Type::Named(def),
                        )
                    }
                    DefKind::Enum(en) => {
                        let name = en.name.clone();
                        self.error_help(
                            "E0246",
                            e.span,
                            format!("`{name}` is an enum, not a struct"),
                            format!("construct a variant: `{name}::Variant {{ ... }}`"),
                        );
                        self.check_lit_fields_poison(fields);
                        return Type::Error;
                    }
                    DefKind::Unit(u) => {
                        let (name, base) = (u.name.clone(), u.base_name().to_string());
                        self.error_help(
                            "E0246",
                            e.span,
                            format!("`{name}` is a unit family and has no fields"),
                            format!(
                                "write a value with a unit suffix (`5{base}`) or convert one: \
                                 `{name}::{base}(n)`"
                            ),
                        );
                        self.check_lit_fields_poison(fields);
                        return Type::Error;
                    }
                    DefKind::Trait(_) => {
                        self.check_lit_fields_poison(fields);
                        return Type::Error;
                    }
                },
                Some(v) => {
                    let Some((tag, kind, _)) = self.variant_info(def, &v.name) else {
                        let enum_name = self.out.defs.name_of(def).to_string();
                        let vname = v.name.clone();
                        self.error(
                            "E0232",
                            v.span,
                            format!("enum `{enum_name}` has no variant `{vname}`"),
                        );
                        self.check_lit_fields_poison(fields);
                        return Type::Error;
                    };
                    if kind != VariantKind::Struct {
                        let vname = v.name.clone();
                        self.error_help(
                            "E0233",
                            e.span,
                            format!("variant `{vname}` does not have named fields"),
                            "use parentheses for tuple variants, or no payload for unit \
                             variants",
                        );
                        self.check_lit_fields_poison(fields);
                        return Type::Error;
                    }
                    let result_ty = self.enum_value_type(def);
                    let names: Vec<(String, Type)> = self
                        .out
                        .defs
                        .as_enum(def)
                        .map(|ed| ed.variants[tag as usize].fields.clone())
                        .unwrap_or_default();
                    let payload = self.variant_payload_types(def, tag, &result_ty);
                    let decl: Vec<(String, Type)> =
                        names.iter().map(|(n, _)| n.clone()).zip(payload).collect();
                    (decl, StructLitRes::Variant { def, tag }, result_ty)
                }
            };

        // Every declared field exactly once.
        let mut provided: Vec<Option<()>> = vec![None; decl_fields.len()];
        let mut order: Vec<u16> = Vec::with_capacity(fields.len());
        for (fname, value) in fields {
            match decl_fields.iter().position(|(n, _)| *n == fname.name) {
                Some(idx) => {
                    if provided[idx].replace(()).is_some() {
                        let n = fname.name.clone();
                        self.error("E0247", fname.span, format!("field `{n}` set twice"));
                    }
                    order.push(idx as u16);
                    let expected = decl_fields[idx].1.clone();
                    self.check_coerce(value, &expected);
                }
                None => {
                    let n = fname.name.clone();
                    let ty_name = self.out.defs.name_of(def).to_string();
                    let avail: Vec<String> = decl_fields.iter().map(|(n, _)| n.clone()).collect();
                    self.error_help(
                        "E0247",
                        fname.span,
                        format!("`{ty_name}` has no field `{n}`"),
                        format!("available fields: {}", avail.join(", ")),
                    );
                    order.push(u16::MAX);
                    self.check_expr(value, None);
                }
            }
        }
        let missing: Vec<String> = decl_fields
            .iter()
            .enumerate()
            .filter(|(i, _)| provided[*i].is_none())
            .map(|(_, (n, _))| n.clone())
            .collect();
        if !missing.is_empty() {
            let ty_name = self.out.defs.name_of(def).to_string();
            self.error_help(
                "E0247",
                e.span,
                format!(
                    "missing fields in `{ty_name}` literal: {}",
                    missing.join(", ")
                ),
                "every field must be initialized",
            );
        }
        self.out.struct_lits.insert(e.id, lit_res);
        self.out.field_orders.insert(e.id, order);
        result_ty
    }

    fn check_lit_fields_poison(&mut self, fields: &[(Ident, Expr)]) {
        for (_, value) in fields {
            self.check_expr(value, None);
        }
    }

    // ---------------------------------------------------------- for / try

    fn check_for(&mut self, e: &Expr, var: &Ident, iter: &Expr, body: &Block) -> Type {
        // Ranges are handled as a `for` header form, not a value.
        let (kind, elem_ty) = if let ExprKind::Range { lo, hi, inclusive } = &iter.kind {
            let lt = self.check_expr(lo, Some(&Type::Int));
            self.unify_or_err(&Type::Int, &lt, lo.span, "range bounds are `int`");
            let ht = self.check_expr(hi, Some(&Type::Int));
            self.unify_or_err(&Type::Int, &ht, hi.span, "range bounds are `int`");
            self.record_type(iter.id, Type::Int);
            (
                if *inclusive {
                    ForKind::RangeInclusive
                } else {
                    ForKind::RangeExclusive
                },
                Type::Int,
            )
        } else {
            let it = self.check_expr(iter, None);
            match self.resolve(&it) {
                Type::List(t) => (ForKind::List, *t),
                Type::Map(k, _) => (ForKind::MapKeys, *k),
                Type::Str => (ForKind::StrChars, Type::Char),
                Type::Error | Type::Never => (ForKind::List, Type::Error),
                other => {
                    let ts = self.ty_str(&other);
                    self.error_help(
                        "E0248",
                        iter.span,
                        format!("`{ts}` is not iterable"),
                        "`for` iterates over ranges (a..b), List (elements), Map (keys), \
                         and string (chars)",
                    );
                    (ForKind::List, Type::Error)
                }
            }
        };
        self.out.for_kinds.insert(e.id, kind);
        self.push_scope();
        let local = self.declare_local(var, elem_ty);
        self.out.decl_locals.insert(e.id, local);
        self.enter_loop();
        self.check_block(body, None);
        self.exit_loop();
        self.pop_scope();
        Type::Unit
    }

    fn check_try(&mut self, e: &Expr, inner: &Expr) -> Type {
        let t = self.check_expr(inner, None);
        let rt = self.resolve(&t);
        let ret = self.resolve(&self.current_ret());
        match rt {
            Type::Option(payload) => {
                if !matches!(ret, Type::Option(_) | Type::Error) {
                    let rs = self.ty_str(&ret);
                    self.error_help(
                        "E0249",
                        e.span,
                        format!(
                            "`?` on an Option requires the function to return Option, \
                             but it returns `{rs}`"
                        ),
                        "change the return type to `Option[...]`, or handle the None case \
                         with `match`/`if let`",
                    );
                }
                self.out.try_kinds.insert(e.id, TryKind::Option);
                *payload
            }
            Type::Result(payload, err) => {
                match &ret {
                    Type::Result(_, ret_err) => {
                        self.unify_or_err(
                            ret_err,
                            &err,
                            e.span,
                            "the error type propagated by `?` must match the function's \
                             error type",
                        );
                    }
                    Type::Error => {}
                    other => {
                        let rs = self.ty_str(other);
                        self.error_help(
                            "E0249",
                            e.span,
                            format!(
                                "`?` on a Result requires the function to return Result, \
                                 but it returns `{rs}`"
                            ),
                            "change the return type to `Result[..., ...]`, or handle the \
                             Err case with `match`",
                        );
                    }
                }
                self.out.try_kinds.insert(e.id, TryKind::Result);
                *payload
            }
            Type::Error | Type::Never => Type::Error,
            other => {
                let ts = self.ty_str(&other);
                self.error_help(
                    "E0249",
                    e.span,
                    format!("`?` requires an Option or Result, found `{ts}`"),
                    "the `?` operator early-returns None/Err (PRD §3.5)",
                );
                Type::Error
            }
        }
    }

    // ----------------------------------------------------------- closures

    fn check_closure(
        &mut self,
        e: &Expr,
        params: &[(Ident, Option<TypeExpr>)],
        ret_ann: Option<&TypeExpr>,
        body: &Expr,
        expect: Option<&Type>,
    ) -> Type {
        // Parameter types: annotation > expectation > fresh var.
        let expected_sig = match expect.map(|t| self.resolve(t)) {
            Some(Type::Fn(sig)) => Some(sig),
            _ => None,
        };
        let mut param_tys = Vec::with_capacity(params.len());
        for (i, (_, ann)) in params.iter().enumerate() {
            let ty = match ann {
                Some(t) => self.resolve_type(t),
                None => match expected_sig.as_ref().and_then(|s| s.params.get(i)) {
                    Some(t) => t.clone(),
                    None => self.infer.fresh(),
                },
            };
            param_tys.push(ty);
        }
        let ret_ty = match ret_ann {
            Some(t) => self.resolve_type(t),
            None => match expected_sig.as_ref() {
                Some(s) => s.ret.clone(),
                None => self.infer.fresh(),
            },
        };
        if let Some(s) = &expected_sig
            && s.params.len() != params.len()
        {
            self.error_help(
                "E0238",
                e.span,
                format!(
                    "closure takes {} parameter{}, but the context expects {}",
                    params.len(),
                    if params.len() == 1 { "" } else { "s" },
                    s.params.len()
                ),
                "match the expected function signature",
            );
        }

        let sig = FnSig::new(param_tys.clone(), ret_ty.clone());
        let proto = self.begin_closure(e.id, sig.clone(), e.span);
        self.set_closure_ret(ret_ty.clone());
        for ((name, _), ty) in params.iter().zip(&param_tys) {
            self.declare_local(name, ty.clone());
        }
        let body_ty = self.check_expr(body, Some(&ret_ty));
        let body_rt = self.resolve(&body_ty);
        if !matches!(body_rt, Type::Never) {
            self.unify_or_err(
                &ret_ty,
                &body_ty,
                body.span,
                "the closure body must produce the closure's return type",
            );
        }
        self.end_closure(proto);

        // Unresolved parameter types are an error: inference is local.
        for ((name, _), ty) in params.iter().zip(&param_tys) {
            if self.infer.contains_unbound(ty) {
                let n = name.name.clone();
                self.error_help(
                    "E0250",
                    name.span,
                    format!("cannot infer the type of closure parameter `{n}`"),
                    "add an annotation: `|x: int| ...` (closure parameters are inferred \
                     only where the context determines them, PRD §3.3)",
                );
            }
        }

        self.out.closures.insert(e.id, super::ClosureRes { proto });
        Type::Fn(Box::new(FnSig::new(param_tys, self.resolve(&ret_ty))))
    }
}

fn op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
    }
}
