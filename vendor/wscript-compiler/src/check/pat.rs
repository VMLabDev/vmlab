//! Pattern checking, `match` checking, and exhaustiveness analysis.
//!
//! Exhaustiveness uses the classic usefulness algorithm (Maranget-style
//! pattern matrices). The PRD only *guarantees* one level of nesting
//! (§3.4); this implementation is exact for arbitrarily nested variant /
//! struct / bool / literal patterns, and conservatively requires `_` for
//! scrutinee positions that cannot be destructured. Guarded arms never
//! count toward exhaustiveness.

use wscript_core::defs::{self, DefId, DefKind, Factor, VariantKind};
use wscript_core::types::Type;

use crate::ast::*;

use super::{Checker, PatLowering};

// ------------------------------------------------------------ checking

impl<'a> Checker<'a> {
    /// Type-check a pattern against the scrutinee type, binding pattern
    /// variables in the current scope and recording resolutions.
    pub(crate) fn check_pattern(&mut self, pat: &Pattern, expected: &Type) {
        match &pat.kind {
            PatternKind::Wildcard | PatternKind::Error => {}
            PatternKind::Binding(name) => {
                let rt = self.resolve(expected);
                if let Some(def) = enum_def_of(&rt)
                    && let Some((tag, kind, n)) = self.variant_info(def, &name.name)
                {
                    match kind {
                        VariantKind::Unit => {
                            self.out.set_pat_lowering(
                                pat.id,
                                PatLowering::Variant {
                                    def,
                                    tag,
                                    order: Vec::new(),
                                },
                            );
                            return;
                        }
                        _ => {
                            let vname = name.name.clone();
                            self.error_help(
                                "E0261",
                                pat.span,
                                format!(
                                    "variant `{vname}` has a payload of {n} field{}",
                                    if n == 1 { "" } else { "s" }
                                ),
                                "destructure the payload, or use `_` to ignore it",
                            );
                            return;
                        }
                    }
                }
                if self.or_depth > 0 {
                    let n = name.name.clone();
                    self.error_help(
                        "E0262",
                        pat.span,
                        format!("binding `{n}` is not allowed inside an or-pattern in v1"),
                        "split the alternatives into separate match arms",
                    );
                    return;
                }
                let local = self.declare_local(name, expected.clone());
                self.out.decl_locals.insert(pat.id, local);
                self.record_type(pat.id, expected.clone());
            }
            PatternKind::IntLit(_) => {
                self.unify_or_err(&Type::Int, expected, pat.span, "integer pattern");
            }
            PatternKind::QuantityLit { value, unit } => {
                let Some((def, folded)) =
                    self.fold_quantity(pat.span, *value, unit, Some(expected))
                else {
                    return;
                };
                match folded {
                    Factor::Int(_) => {
                        self.out
                            .set_pat_lowering(pat.id, PatLowering::QuantityLit(folded));
                        self.unify_or_err(
                            &Type::Named(def),
                            expected,
                            pat.span,
                            "unit literal pattern",
                        );
                    }
                    // Same rule as plain floats: no float patterns.
                    Factor::Float(_) => {
                        let n = self.out.defs.name_of(def).to_string();
                        self.error_help(
                            "E0113",
                            pat.span,
                            format!("`{n}` is backed by `float` and cannot be matched on"),
                            "compare with `==` in a guard, or match a range of values \
                             with `if`",
                        );
                    }
                }
            }
            PatternKind::BoolLit(_) => {
                self.unify_or_err(&Type::Bool, expected, pat.span, "bool pattern");
            }
            PatternKind::CharLit(_) => {
                self.unify_or_err(&Type::Char, expected, pat.span, "char pattern");
            }
            PatternKind::StrLit(_) => {
                self.unify_or_err(&Type::Str, expected, pat.span, "string pattern");
            }
            PatternKind::Variant { path, args } => {
                self.check_variant_pattern(pat, path, args, expected);
            }
            PatternKind::Struct {
                path,
                fields,
                has_rest,
            } => {
                self.check_struct_pattern(pat, path, fields, *has_rest, expected);
            }
            PatternKind::Or(alts) => {
                self.or_depth += 1;
                for alt in alts {
                    self.check_pattern(alt, expected);
                }
                self.or_depth -= 1;
            }
        }
    }

    fn check_variant_pattern(
        &mut self,
        pat: &Pattern,
        path: &[Ident],
        args: &VariantPatArgs,
        expected: &Type,
    ) {
        let rt = self.resolve(expected);
        // Resolve the enum def and the variant name.
        let (def, vname): (DefId, &Ident) = match path {
            [v] => match enum_def_of(&rt) {
                Some(def) => (def, v),
                None => {
                    // Ambient Option/Result constructors work regardless.
                    if matches!(v.name.as_str(), "Some" | "None") {
                        (defs::DEF_OPTION, v)
                    } else if matches!(v.name.as_str(), "Ok" | "Err") {
                        (defs::DEF_RESULT, v)
                    } else {
                        let ts = self.ty_str(&rt);
                        let n = v.name.clone();
                        self.error_help(
                            "E0263",
                            pat.span,
                            format!("cannot match variant `{n}` against type `{ts}`"),
                            "qualify the variant with its enum: `Enum::Variant(...)`",
                        );
                        return;
                    }
                }
            },
            [e, v] => match self.enum_by_name(&e.name) {
                Some(def) => (def, v),
                None => {
                    let n = e.name.clone();
                    self.error("E0212", e.span, format!("unknown enum `{n}`"));
                    return;
                }
            },
            [_m, e, v] => match self.enum_by_name(&e.name) {
                Some(def) => (def, v),
                None => {
                    let n = e.name.clone();
                    self.error("E0212", e.span, format!("unknown enum `{n}`"));
                    return;
                }
            },
            _ => {
                self.error("E0231", pat.span, "path has too many segments");
                return;
            }
        };
        let Some((tag, kind, n_fields)) = self.variant_info(def, &vname.name) else {
            let enum_name = self.out.defs.name_of(def).to_string();
            let n = vname.name.clone();
            self.error(
                "E0232",
                vname.span,
                format!("enum `{enum_name}` has no variant `{n}`"),
            );
            return;
        };
        // The scrutinee must actually be this enum.
        let enum_ty = self.enum_value_type(def);
        self.unify_or_err(
            &enum_ty,
            expected,
            pat.span,
            "the pattern's enum must match the scrutinee type",
        );
        let payload = self.variant_payload_types(def, tag, expected);
        // The field order of a struct variant; empty for every other shape.
        // Collected first so the lowering below is written in one piece.
        let order = match (kind, args) {
            (VariantKind::Unit, VariantPatArgs::Unit) => Vec::new(),
            (VariantKind::Tuple, VariantPatArgs::Tuple(pats)) => {
                if pats.len() != n_fields {
                    let n = vname.name.clone();
                    self.error(
                        "E0264",
                        pat.span,
                        format!(
                            "variant `{n}` has {n_fields} field{}, pattern has {}",
                            if n_fields == 1 { "" } else { "s" },
                            pats.len()
                        ),
                    );
                }
                for (p, t) in pats.iter().zip(payload.iter()) {
                    self.check_pattern(p, t);
                }
                for p in pats.iter().skip(payload.len()) {
                    self.check_pattern(p, &Type::Error);
                }
                Vec::new()
            }
            (VariantKind::Struct, VariantPatArgs::Struct { fields, has_rest }) => {
                let decl: Vec<(String, Type)> = self
                    .out
                    .defs
                    .as_enum(def)
                    .map(|ed| ed.variants[tag as usize].fields.clone())
                    .unwrap_or_default();
                let decl: Vec<(String, Type)> = decl
                    .iter()
                    .map(|(n, _)| n.clone())
                    .zip(payload.iter().cloned())
                    .collect();
                self.check_pattern_fields(pat, &decl, fields, *has_rest, &vname.name)
            }
            (VariantKind::Unit, _) => {
                let n = vname.name.clone();
                self.error("E0264", pat.span, format!("variant `{n}` has no payload"));
                Vec::new()
            }
            (VariantKind::Tuple, _) => {
                let n = vname.name.clone();
                self.error_help(
                    "E0264",
                    pat.span,
                    format!("variant `{n}` is a tuple variant"),
                    format!("write `{n}(...)`"),
                );
                Vec::new()
            }
            (VariantKind::Struct, _) => {
                let n = vname.name.clone();
                self.error_help(
                    "E0264",
                    pat.span,
                    format!("variant `{n}` has named fields"),
                    format!("write `{n} {{ ... }}`"),
                );
                Vec::new()
            }
        };
        self.out
            .set_pat_lowering(pat.id, PatLowering::Variant { def, tag, order });
    }

    fn check_struct_pattern(
        &mut self,
        pat: &Pattern,
        path: &[Ident],
        fields: &[(Ident, Pattern)],
        has_rest: bool,
        expected: &Type,
    ) {
        let [ty_ident] = path else {
            self.error("E0231", pat.span, "path has too many segments");
            return;
        };
        let Some(def) = self.type_name(&ty_ident.name) else {
            let n = ty_ident.name.clone();
            self.error("E0212", ty_ident.span, format!("unknown type `{n}`"));
            return;
        };
        let decl = match self.out.defs.get(def) {
            DefKind::Struct(s) => {
                if s.opaque {
                    let n = s.name.clone();
                    self.error_help(
                        "E0244",
                        pat.span,
                        format!("`{n}` is an opaque host type and cannot be destructured"),
                        "opaque types expose methods only (PRD §6.2)",
                    );
                    return;
                }
                s.fields.clone()
            }
            _ => {
                let n = ty_ident.name.clone();
                self.error("E0263", pat.span, format!("`{n}` is not a struct"));
                return;
            }
        };
        self.unify_or_err(
            &Type::Named(def),
            expected,
            pat.span,
            "the pattern's type must match the scrutinee type",
        );
        let order = self.check_pattern_fields(pat, &decl, fields, has_rest, &ty_ident.name);
        self.out
            .set_pat_lowering(pat.id, PatLowering::Struct { def, order });
    }

    /// Shared field handling for struct patterns and struct-variant
    /// patterns: resolves indices, recurses, enforces completeness.
    ///
    /// Returns the runtime field index of each field as written, rather
    /// than recording it: only the caller knows whether the pattern is a
    /// `Struct` or a `Variant`, so only the caller can write a complete
    /// [`PatLowering`].
    fn check_pattern_fields(
        &mut self,
        pat: &Pattern,
        decl: &[(String, Type)],
        fields: &[(Ident, Pattern)],
        has_rest: bool,
        what: &str,
    ) -> Vec<u16> {
        let mut order: Vec<u16> = Vec::with_capacity(fields.len());
        let mut seen = vec![false; decl.len()];
        for (fname, sub) in fields {
            match decl.iter().position(|(n, _)| *n == fname.name) {
                Some(idx) => {
                    if std::mem::replace(&mut seen[idx], true) {
                        let n = fname.name.clone();
                        self.error("E0247", fname.span, format!("field `{n}` matched twice"));
                    }
                    order.push(idx as u16);
                    let t = decl[idx].1.clone();
                    self.check_pattern(sub, &t);
                }
                None => {
                    let n = fname.name.clone();
                    self.error_help(
                        "E0247",
                        fname.span,
                        format!("`{what}` has no field `{n}`"),
                        format!(
                            "available fields: {}",
                            decl.iter()
                                .map(|(n, _)| n.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    );
                    order.push(u16::MAX);
                    self.check_pattern(sub, &Type::Error);
                }
            }
        }
        if !has_rest {
            let missing: Vec<String> = decl
                .iter()
                .enumerate()
                .filter(|(i, _)| !seen[*i])
                .map(|(_, (n, _))| n.clone())
                .collect();
            if !missing.is_empty() {
                self.error_help(
                    "E0247",
                    pat.span,
                    format!("pattern does not mention fields: {}", missing.join(", ")),
                    "list every field, or end the pattern with `..` to ignore the rest",
                );
            }
        }
        order
    }

    /// Conservative refutability test (used for `if let` / `let else`
    /// lints). Read-only: never records or binds.
    pub(crate) fn pattern_is_refutable(&mut self, pat: &Pattern, ty: &Type) -> bool {
        let rt = self.resolve(ty);
        match &pat.kind {
            PatternKind::Wildcard | PatternKind::Error => false,
            PatternKind::Binding(name) => {
                if let Some(def) = enum_def_of(&rt)
                    && self.variant_info(def, &name.name).is_some()
                {
                    self.variant_count(def) > 1
                } else {
                    false
                }
            }
            PatternKind::IntLit(_)
            | PatternKind::QuantityLit { .. }
            | PatternKind::BoolLit(_)
            | PatternKind::CharLit(_)
            | PatternKind::StrLit(_) => true,
            PatternKind::Variant { path, args } => {
                let def = match &path[..] {
                    [v] => enum_def_of(&rt).or(match v.name.as_str() {
                        "Some" | "None" => Some(defs::DEF_OPTION),
                        "Ok" | "Err" => Some(defs::DEF_RESULT),
                        _ => None,
                    }),
                    [e, _] => self.enum_by_name(&e.name),
                    [_, e, _] => self.enum_by_name(&e.name),
                    _ => None,
                };
                let Some(def) = def else { return true };
                if self.variant_count(def) > 1 {
                    return true;
                }
                match args {
                    VariantPatArgs::Unit => false,
                    VariantPatArgs::Tuple(pats) => pats
                        .iter()
                        .any(|p| self.pattern_is_refutable(p, &Type::Error)),
                    VariantPatArgs::Struct { fields, .. } => fields
                        .iter()
                        .any(|(_, p)| self.pattern_is_refutable(p, &Type::Error)),
                }
            }
            PatternKind::Struct { fields, .. } => fields
                .iter()
                .any(|(_, p)| self.pattern_is_refutable(p, &Type::Error)),
            // Conservative: skip the lint for or-patterns.
            PatternKind::Or(_) => true,
        }
    }

    fn variant_count(&self, def: DefId) -> usize {
        self.out
            .defs
            .as_enum(def)
            .map(|e| e.variants.len())
            .unwrap_or(0)
    }

    // ------------------------------------------------------------ match

    pub(crate) fn check_match(
        &mut self,
        e: &Expr,
        scrutinee: &Expr,
        arms: &[MatchArm],
        expect: Option<&Type>,
    ) -> Type {
        let scrut_ty = self.check_expr(scrutinee, None);
        let rt = self.resolve(&scrut_ty);
        let mut result: Option<Type> = None;
        let mut unguarded: Vec<Vec<DPat>> = Vec::new();
        for arm in arms {
            let body_ty = self.in_scope(|c| {
                c.check_pattern(&arm.pat, &scrut_ty);
                let row = vec![c.lower_pattern(&arm.pat)];
                if !unguarded.is_empty()
                    && c.is_useful(&unguarded, &row, std::slice::from_ref(&rt))
                        .is_none()
                {
                    c.warn(
                        "W0002",
                        arm.pat.span,
                        "unreachable match arm: previous patterns already cover this case",
                    );
                }
                let guarded = match &arm.guard {
                    Some(g) => {
                        let gt = c.check_expr(g, Some(&Type::Bool));
                        c.unify_or_err(&Type::Bool, &gt, g.span, "match guards are `bool`");
                        true
                    }
                    None => false,
                };
                if !guarded {
                    unguarded.push(row);
                }
                c.check_expr(&arm.body, expect)
            });
            let body_rt = self.resolve(&body_ty);
            if !matches!(body_rt, Type::Never) {
                result = Some(match result {
                    None => body_rt,
                    Some(acc) => {
                        self.unify_or_err(
                            &acc,
                            &body_rt,
                            arm.body.span,
                            "all match arms must produce the same type",
                        );
                        self.resolve(&acc)
                    }
                });
            }
        }
        // Exhaustiveness (PRD §3.4: compile error on missing variants).
        if !matches!(rt, Type::Error)
            && let Some(witness) =
                self.is_useful(&unguarded, &[DPat::Wild], std::slice::from_ref(&rt))
        {
            let w = witness.first().cloned().unwrap_or_else(|| "_".into());
            let ts = self.ty_str(&rt);
            self.error_help(
                "E0260",
                e.span,
                format!("non-exhaustive match: pattern `{w}` is not covered"),
                format!(
                    "add an arm matching `{w}`, or a trailing `_ => ...` arm; note \
                         that arms with `if` guards never count toward exhaustiveness \
                         (matching on `{ts}`)"
                ),
            );
        }
        result.unwrap_or(Type::Never)
    }

    // ----------------------------------------------- usefulness analysis

    /// Lower an AST pattern to the matrix representation. Uses resolutions
    /// recorded by `check_pattern` (so it must run after it).
    fn lower_pattern(&self, pat: &Pattern) -> DPat {
        match &pat.kind {
            PatternKind::Wildcard | PatternKind::Error => DPat::Wild,
            PatternKind::Binding(_) => match self.out.pat_variant(pat.id) {
                Some((def, tag, _)) => DPat::Ctor(
                    Ctor::Variant {
                        def,
                        tag,
                        arity: self.variant_arity(def, tag),
                    },
                    vec![],
                ),
                None => DPat::Wild,
            },
            PatternKind::IntLit(n) => DPat::Ctor(Ctor::Int(*n), vec![]),
            // Unit patterns are erased to their base-unit constant, so they
            // share the int ctor space (and its exhaustiveness rules).
            PatternKind::QuantityLit { .. } => match self.out.pat_quantity_lit(pat.id) {
                Some(Factor::Int(n)) => DPat::Ctor(Ctor::Int(n), vec![]),
                _ => DPat::Wild,
            },
            PatternKind::BoolLit(b) => DPat::Ctor(Ctor::Bool(*b), vec![]),
            PatternKind::CharLit(c) => DPat::Ctor(Ctor::Char(*c), vec![]),
            PatternKind::StrLit(s) => DPat::Ctor(Ctor::Str(s.clone()), vec![]),
            PatternKind::Variant { args, .. } => {
                let Some((def, tag, order)) = self.out.pat_variant(pat.id) else {
                    return DPat::Wild;
                };
                let arity = self.variant_arity(def, tag);
                let mut sub = vec![DPat::Wild; arity];
                match args {
                    VariantPatArgs::Unit => {}
                    VariantPatArgs::Tuple(pats) => {
                        for (i, p) in pats.iter().enumerate().take(arity) {
                            sub[i] = self.lower_pattern(p);
                        }
                    }
                    VariantPatArgs::Struct { fields, .. } => {
                        self.scatter_named_fields(fields, order, &mut sub);
                    }
                }
                DPat::Ctor(Ctor::Variant { def, tag, arity }, sub)
            }
            PatternKind::Struct { fields, .. } => {
                let Some((def, order)) = self.out.pat_struct(pat.id) else {
                    return DPat::Wild;
                };
                let arity = self
                    .out
                    .defs
                    .as_struct(def)
                    .map(|s| s.fields.len())
                    .unwrap_or(0);
                let mut sub = vec![DPat::Wild; arity];
                self.scatter_named_fields(fields, order, &mut sub);
                DPat::Ctor(Ctor::Struct { def, arity }, sub)
            }
            PatternKind::Or(alts) => DPat::Or(alts.iter().map(|a| self.lower_pattern(a)).collect()),
        }
    }

    /// Place each named field's sub-pattern at its declared position in
    /// `sub` (the matrix wants declaration order, not written order).
    /// Fields the checker could not resolve carry `u16::MAX` and stay
    /// wildcards, as do positions the pattern did not mention.
    fn scatter_named_fields(&self, fields: &[(Ident, Pattern)], order: &[u16], sub: &mut [DPat]) {
        for (i, (_, p)) in fields.iter().enumerate() {
            if let Some(&idx) = order.get(i)
                && (idx as usize) < sub.len()
            {
                sub[idx as usize] = self.lower_pattern(p);
            }
        }
    }

    fn variant_arity(&self, def: DefId, tag: u32) -> usize {
        self.out
            .defs
            .as_enum(def)
            .and_then(|e| e.variants.get(tag as usize))
            .map(|v| v.fields.len())
            .unwrap_or(0)
    }

    /// Is pattern vector `v` useful w.r.t. `matrix`? Returns a witness
    /// (one rendered pattern per column) of a value matched by `v` but no
    /// row — i.e. for `v = [_]`, a missing case.
    fn is_useful(&self, matrix: &[Vec<DPat>], v: &[DPat], tys: &[Type]) -> Option<Vec<String>> {
        if v.is_empty() {
            return if matrix.is_empty() {
                Some(vec![])
            } else {
                None
            };
        }
        let head_ty = self.resolve(&tys[0]);
        match &v[0] {
            DPat::Or(alts) => {
                for alt in alts {
                    let mut candidate = vec![alt.clone()];
                    candidate.extend_from_slice(&v[1..]);
                    if let Some(w) = self.is_useful(matrix, &candidate, tys) {
                        return Some(w);
                    }
                }
                None
            }
            DPat::Ctor(c, args) => {
                let spec = specialize_matrix(matrix, c);
                let mut sub_tys = self.ctor_arg_types(c, &head_ty);
                sub_tys.extend_from_slice(&tys[1..]);
                let mut sub_v = args.clone();
                sub_v.extend_from_slice(&v[1..]);
                let w = self.is_useful(&spec, &sub_v, &sub_tys)?;
                let (head_w, rest_w) = w.split_at(c.arity());
                let mut out = vec![self.render_ctor(c, head_w)];
                out.extend_from_slice(rest_w);
                Some(out)
            }
            DPat::Wild => {
                let used = used_ctors(matrix);
                let universe = self.ctor_universe(&head_ty);
                match &universe {
                    Universe::Finite(all)
                        if all.iter().all(|c| used.iter().any(|u| ctor_eq(u, c))) =>
                    {
                        // Complete signature: try each constructor.
                        for c in all {
                            let spec = specialize_matrix(matrix, c);
                            let mut sub_tys = self.ctor_arg_types(c, &head_ty);
                            sub_tys.extend_from_slice(&tys[1..]);
                            let mut sub_v = vec![DPat::Wild; c.arity()];
                            sub_v.extend_from_slice(&v[1..]);
                            if let Some(w) = self.is_useful(&spec, &sub_v, &sub_tys) {
                                let (head_w, rest_w) = w.split_at(c.arity());
                                let mut out = vec![self.render_ctor(c, head_w)];
                                out.extend_from_slice(rest_w);
                                return Some(out);
                            }
                        }
                        None
                    }
                    _ => {
                        // Incomplete signature: take the default matrix.
                        let def_matrix = default_matrix(matrix);
                        let w = self.is_useful(&def_matrix, &v[1..], &tys[1..])?;
                        let missing = match &universe {
                            Universe::Finite(all) => all
                                .iter()
                                .find(|c| !used.iter().any(|u| ctor_eq(u, c)))
                                .map(|c| {
                                    let wilds = vec!["_".to_string(); c.arity()];
                                    self.render_ctor(c, &wilds)
                                })
                                .unwrap_or_else(|| "_".into()),
                            Universe::Infinite => "_".into(),
                        };
                        let mut out = vec![missing];
                        out.extend(w);
                        Some(out)
                    }
                }
            }
        }
    }

    fn ctor_arg_types(&self, c: &Ctor, ty: &Type) -> Vec<Type> {
        match c {
            Ctor::Variant { def, tag, .. } => self.variant_payload_types(*def, *tag, ty),
            Ctor::Struct { def, .. } => self
                .out
                .defs
                .as_struct(*def)
                .map(|s| s.fields.iter().map(|(_, t)| t.clone()).collect())
                .unwrap_or_default(),
            _ => vec![],
        }
    }

    fn ctor_universe(&self, ty: &Type) -> Universe {
        match ty {
            Type::Bool => Universe::Finite(vec![Ctor::Bool(true), Ctor::Bool(false)]),
            Type::Option(_) => Universe::Finite(vec![
                Ctor::Variant {
                    def: defs::DEF_OPTION,
                    tag: defs::TAG_NONE,
                    arity: 0,
                },
                Ctor::Variant {
                    def: defs::DEF_OPTION,
                    tag: defs::TAG_SOME,
                    arity: 1,
                },
            ]),
            Type::Result(..) => Universe::Finite(vec![
                Ctor::Variant {
                    def: defs::DEF_RESULT,
                    tag: defs::TAG_OK,
                    arity: 1,
                },
                Ctor::Variant {
                    def: defs::DEF_RESULT,
                    tag: defs::TAG_ERR,
                    arity: 1,
                },
            ]),
            Type::Named(def) => match self.out.defs.get(*def) {
                DefKind::Enum(e) => Universe::Finite(
                    e.variants
                        .iter()
                        .enumerate()
                        .map(|(tag, v)| Ctor::Variant {
                            def: *def,
                            tag: tag as u32,
                            arity: v.fields.len(),
                        })
                        .collect(),
                ),
                DefKind::Struct(s) if !s.opaque => Universe::Finite(vec![Ctor::Struct {
                    def: *def,
                    arity: s.fields.len(),
                }]),
                _ => Universe::Infinite,
            },
            _ => Universe::Infinite,
        }
    }

    /// Render a constructor with its sub-witnesses for diagnostics.
    fn render_ctor(&self, c: &Ctor, args: &[String]) -> String {
        match c {
            Ctor::Bool(b) => b.to_string(),
            Ctor::Int(n) => n.to_string(),
            Ctor::Char(ch) => format!("{ch:?}"),
            Ctor::Str(s) => format!("{s:?}"),
            Ctor::Struct { def, .. } => {
                let name = self.out.defs.name_of(*def);
                if args.iter().all(|a| a == "_") {
                    format!("{name} {{ .. }}")
                } else {
                    let fields = self
                        .out
                        .defs
                        .as_struct(*def)
                        .map(|s| s.fields.clone())
                        .unwrap_or_default();
                    let parts: Vec<String> = fields
                        .iter()
                        .zip(args)
                        .map(|((n, _), a)| format!("{n}: {a}"))
                        .collect();
                    format!("{name} {{ {} }}", parts.join(", "))
                }
            }
            Ctor::Variant { def, tag, arity } => {
                let (vname, kind) = self
                    .out
                    .defs
                    .as_enum(*def)
                    .and_then(|e| e.variants.get(*tag as usize))
                    .map(|v| (v.name.clone(), v.kind))
                    .unwrap_or(("?".into(), VariantKind::Unit));
                let enum_name = self.out.defs.name_of(*def);
                let prefix = if enum_name == "Option" || enum_name == "Result" {
                    vname
                } else {
                    format!("{enum_name}::{vname}")
                };
                match kind {
                    VariantKind::Unit => prefix,
                    VariantKind::Tuple => {
                        let inner = if args.is_empty() {
                            vec!["_".to_string(); *arity]
                        } else {
                            args.to_vec()
                        };
                        format!("{prefix}({})", inner.join(", "))
                    }
                    VariantKind::Struct => {
                        if args.iter().all(|a| a == "_") {
                            format!("{prefix} {{ .. }}")
                        } else {
                            let fields = self
                                .out
                                .defs
                                .as_enum(*def)
                                .and_then(|e| e.variants.get(*tag as usize))
                                .map(|v| v.fields.clone())
                                .unwrap_or_default();
                            let parts: Vec<String> = fields
                                .iter()
                                .zip(args)
                                .map(|((n, _), a)| format!("{n}: {a}"))
                                .collect();
                            format!("{prefix} {{ {} }}", parts.join(", "))
                        }
                    }
                }
            }
        }
    }
}

// ----------------------------------------------------- matrix machinery

#[derive(Debug, Clone)]
enum DPat {
    Wild,
    Ctor(Ctor, Vec<DPat>),
    Or(Vec<DPat>),
}

#[derive(Debug, Clone)]
enum Ctor {
    Variant { def: DefId, tag: u32, arity: usize },
    Struct { def: DefId, arity: usize },
    Bool(bool),
    Int(i64),
    Char(char),
    Str(String),
}

impl Ctor {
    fn arity(&self) -> usize {
        match self {
            Ctor::Variant { arity, .. } | Ctor::Struct { arity, .. } => *arity,
            _ => 0,
        }
    }
}

fn ctor_eq(a: &Ctor, b: &Ctor) -> bool {
    match (a, b) {
        (
            Ctor::Variant {
                def: d1, tag: t1, ..
            },
            Ctor::Variant {
                def: d2, tag: t2, ..
            },
        ) => d1 == d2 && t1 == t2,
        (Ctor::Struct { def: d1, .. }, Ctor::Struct { def: d2, .. }) => d1 == d2,
        (Ctor::Bool(x), Ctor::Bool(y)) => x == y,
        (Ctor::Int(x), Ctor::Int(y)) => x == y,
        (Ctor::Char(x), Ctor::Char(y)) => x == y,
        (Ctor::Str(x), Ctor::Str(y)) => x == y,
        _ => false,
    }
}

enum Universe {
    Finite(Vec<Ctor>),
    Infinite,
}

/// Constructors appearing at the head of matrix rows (or-patterns
/// flattened).
fn used_ctors(matrix: &[Vec<DPat>]) -> Vec<Ctor> {
    let mut out = Vec::new();
    fn visit(p: &DPat, out: &mut Vec<Ctor>) {
        match p {
            DPat::Wild => {}
            DPat::Ctor(c, _) => out.push(c.clone()),
            DPat::Or(alts) => {
                for a in alts {
                    visit(a, out);
                }
            }
        }
    }
    for row in matrix {
        if let Some(head) = row.first() {
            visit(head, &mut out);
        }
    }
    out
}

fn specialize_matrix(matrix: &[Vec<DPat>], c: &Ctor) -> Vec<Vec<DPat>> {
    let mut out = Vec::new();
    for row in matrix {
        specialize_row(row, c, &mut out);
    }
    out
}

fn specialize_row(row: &[DPat], c: &Ctor, out: &mut Vec<Vec<DPat>>) {
    match &row[0] {
        DPat::Wild => {
            let mut new_row = vec![DPat::Wild; c.arity()];
            new_row.extend_from_slice(&row[1..]);
            out.push(new_row);
        }
        DPat::Ctor(c2, args) => {
            if ctor_eq(c2, c) {
                let mut new_row = args.clone();
                new_row.extend_from_slice(&row[1..]);
                out.push(new_row);
            }
        }
        DPat::Or(alts) => {
            for alt in alts {
                let mut sub = vec![alt.clone()];
                sub.extend_from_slice(&row[1..]);
                specialize_row(&sub, c, out);
            }
        }
    }
}

fn default_matrix(matrix: &[Vec<DPat>]) -> Vec<Vec<DPat>> {
    let mut out = Vec::new();
    for row in matrix {
        default_row(row, &mut out);
    }
    out
}

fn default_row(row: &[DPat], out: &mut Vec<Vec<DPat>>) {
    match &row[0] {
        DPat::Wild => out.push(row[1..].to_vec()),
        DPat::Ctor(..) => {}
        DPat::Or(alts) => {
            for alt in alts {
                let mut sub = vec![alt.clone()];
                sub.extend_from_slice(&row[1..]);
                default_row(&sub, out);
            }
        }
    }
}

/// `Named(enum)` / `Option` / `Result` → the enum's def id.
fn enum_def_of(ty: &Type) -> Option<DefId> {
    match ty {
        Type::Option(_) => Some(defs::DEF_OPTION),
        Type::Result(..) => Some(defs::DEF_RESULT),
        Type::Named(def) => Some(*def),
        _ => None,
    }
}
