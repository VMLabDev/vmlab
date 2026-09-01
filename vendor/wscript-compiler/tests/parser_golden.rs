//! Parser snapshots (PRD §11): for each fixture under `tests/fixtures/parser/`,
//! the parsed AST with every expression's span and node id, plus the
//! diagnostics parsing produced.
//!
//! Renderings are compact structural forms (not `Debug` dumps) so they stay
//! reviewable; any parser change that reshapes the tree, moves a span or
//! renumbers a node shows up as a readable diff. Regenerate with
//! `just snap-regen`.

mod common;

use wscript_compiler::ast::*;
use wscript_core::span::Span;

fn render(src: &str, file: &SourceFile) -> String {
    let mut out = String::new();
    for item in &file.items {
        render_item(src, item, &mut out, 0);
    }
    out
}

/// Append ` @1:5-1:9 #7` to the node's own line — the first line written
/// since `start`. Children have already annotated themselves, so the first
/// newline after `start` always terminates this node's header.
fn annotate(src: &str, out: &mut String, start: usize, span: Span, id: NodeId) {
    let Some(nl) = out[start..].find('\n').map(|i| start + i) else {
        return;
    };
    let ann = format!("  @{} #{id}", common::span_str(src, span.lo, span.hi));
    out.insert_str(nl, &ann);
}

fn pad(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn render_item(src: &str, item: &Item, out: &mut String, depth: usize) {
    pad(out, depth);
    match item {
        Item::Use(u) => {
            out.push_str(&format!(
                "use {}{}\n",
                u.module.name,
                u.item
                    .as_ref()
                    .map(|i| format!("::{}", i.name))
                    .unwrap_or_default()
            ));
        }
        Item::Fn(f) => {
            let params: Vec<String> = f
                .params
                .iter()
                .map(|p| {
                    if p.is_self {
                        "self".into()
                    } else {
                        format!("{}:{}", p.name.name, render_ty(p.ty.as_ref()))
                    }
                })
                .collect();
            out.push_str(&format!(
                "fn {}({}) -> {}{}\n",
                f.name.name,
                params.join(", "),
                render_ty(f.ret.as_ref()),
                if f.has_body { "" } else { " <decl>" }
            ));
            render_block(src, &f.body, out, depth + 1);
        }
        Item::Units(u) => {
            let entries: Vec<String> = u.units.iter().map(|e| e.name.name.clone()).collect();
            out.push_str(&format!(
                "units {}: {} [{}]\n",
                u.name.name,
                render_ty(Some(&u.base)),
                entries.join(", ")
            ));
            for e in &u.units {
                render_expr(src, &e.factor, out, depth + 1);
            }
        }
        Item::Struct(s) => {
            out.push_str(&format!(
                "struct {} [{}]{}\n",
                s.name.name,
                s.derives
                    .iter()
                    .map(|d| d.name.clone())
                    .collect::<Vec<_>>()
                    .join(","),
                if s.opaque { " opaque" } else { "" }
            ));
            for f in &s.fields {
                pad(out, depth + 1);
                out.push_str(&format!("{}: {}\n", f.name.name, render_ty(Some(&f.ty))));
            }
        }
        Item::Enum(e) => {
            out.push_str(&format!("enum {}\n", e.name.name));
            for v in &e.variants {
                pad(out, depth + 1);
                match &v.body {
                    VariantBody::Unit => out.push_str(&format!("{}\n", v.name.name)),
                    VariantBody::Tuple(tys) => out.push_str(&format!(
                        "{}({})\n",
                        v.name.name,
                        tys.iter()
                            .map(|t| render_ty(Some(t)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                    VariantBody::Struct(fs) => out.push_str(&format!(
                        "{} {{ {} }}\n",
                        v.name.name,
                        fs.iter()
                            .map(|f| format!("{}: {}", f.name.name, render_ty(Some(&f.ty))))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                }
            }
        }
        Item::Trait(t) => {
            out.push_str(&format!("trait {}\n", t.name.name));
            for m in &t.methods {
                pad(out, depth + 1);
                out.push_str(&format!(
                    "fn {}({}) -> {}\n",
                    m.name.name,
                    m.params
                        .iter()
                        .map(|p| format!("{}:{}", p.name.name, render_ty(p.ty.as_ref())))
                        .collect::<Vec<_>>()
                        .join(", "),
                    render_ty(m.ret.as_ref())
                ));
            }
        }
        Item::Impl(im) => {
            match &im.trait_name {
                Some(tr) => out.push_str(&format!("impl {} for {}\n", tr.name, im.ty_name.name)),
                None => out.push_str(&format!("impl {}\n", im.ty_name.name)),
            }
            for f in &im.fns {
                render_item_fn_shallow(src, f, out, depth + 1);
            }
        }
        Item::Mod(m) => {
            out.push_str(&format!("mod {}\n", m.name.name));
            for item in &m.items {
                render_item(src, item, out, depth + 1);
            }
        }
        Item::Const(c) => {
            out.push_str(&format!(
                "const {}: {}\n",
                c.name.name,
                render_ty(Some(&c.ty))
            ));
        }
    }
}

fn render_item_fn_shallow(src: &str, f: &FnDecl, out: &mut String, depth: usize) {
    pad(out, depth);
    out.push_str(&format!("fn {}(..)\n", f.name.name));
    render_block(src, &f.body, out, depth + 1);
}

fn render_block(src: &str, b: &Block, out: &mut String, depth: usize) {
    for stmt in &b.stmts {
        match stmt {
            Stmt::Let { name, ty, init, .. } => {
                pad(out, depth);
                out.push_str(&format!("let {}:{} =\n", name.name, render_ty(ty.as_ref())));
                render_expr(src, init, out, depth + 1);
            }
            Stmt::LetElse {
                pat,
                init,
                else_block,
                ..
            } => {
                pad(out, depth);
                out.push_str(&format!("let-else {} =\n", render_pat(pat)));
                render_expr(src, init, out, depth + 1);
                pad(out, depth);
                out.push_str("else\n");
                render_block(src, else_block, out, depth + 1);
            }
            Stmt::Expr { expr, terminated } => {
                pad(out, depth);
                out.push_str(if *terminated { "expr; \n" } else { "expr\n" });
                render_expr(src, expr, out, depth + 1);
            }
        }
    }
}

fn render_expr(src: &str, e: &Expr, out: &mut String, depth: usize) {
    let start = out.len();
    render_expr_inner(src, e, out, depth);
    annotate(src, out, start, e.span, e.id);
}

fn render_expr_inner(src: &str, e: &Expr, out: &mut String, depth: usize) {
    pad(out, depth);
    match &e.kind {
        ExprKind::IntLit(n) => out.push_str(&format!("int {n}\n")),
        ExprKind::FloatLit(f) => out.push_str(&format!("float {f}\n")),
        ExprKind::BoolLit(b) => out.push_str(&format!("bool {b}\n")),
        ExprKind::CharLit(c) => out.push_str(&format!("char {c:?}\n")),
        ExprKind::StrLit(s) => out.push_str(&format!("str {s:?}\n")),
        ExprKind::StrInterp(parts) => {
            out.push_str("str-interp\n");
            for p in parts {
                match p {
                    wscript_compiler::ast::InterpPart::Lit(s) => {
                        pad(out, depth + 1);
                        out.push_str(&format!("lit {s:?}\n"));
                    }
                    wscript_compiler::ast::InterpPart::Hole(h) => {
                        pad(out, depth + 1);
                        out.push_str("hole\n");
                        render_expr(src, h, out, depth + 2);
                    }
                }
            }
        }
        ExprKind::QuantityLit { value, unit } => {
            out.push_str(&format!(
                "quantity {} {}\n",
                render_lit_num(*value),
                unit.name
            ));
        }
        ExprKind::UnitLit => out.push_str("unit\n"),
        ExprKind::Path(segs) => out.push_str(&format!(
            "path {}\n",
            segs.iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join("::")
        )),
        ExprKind::Unary { op, expr } => {
            out.push_str(&format!("unary {op:?}\n"));
            render_expr(src, expr, out, depth + 1);
        }
        ExprKind::Binary { op, lhs, rhs } => {
            out.push_str(&format!("binary {op:?}\n"));
            render_expr(src, lhs, out, depth + 1);
            render_expr(src, rhs, out, depth + 1);
        }
        ExprKind::Assign { target, value, .. } => {
            out.push_str("assign\n");
            render_expr(src, target, out, depth + 1);
            render_expr(src, value, out, depth + 1);
        }
        ExprKind::Call { callee, args } => {
            out.push_str("call\n");
            render_expr(src, callee, out, depth + 1);
            for a in args {
                render_expr(src, a, out, depth + 1);
            }
        }
        ExprKind::MethodCall { recv, name, args } => {
            out.push_str(&format!("method .{}\n", name.name));
            render_expr(src, recv, out, depth + 1);
            for a in args {
                render_expr(src, a, out, depth + 1);
            }
        }
        ExprKind::Field { obj, name } => {
            out.push_str(&format!("field .{}\n", name.name));
            render_expr(src, obj, out, depth + 1);
        }
        ExprKind::Index { obj, idx } => {
            out.push_str("index\n");
            render_expr(src, obj, out, depth + 1);
            render_expr(src, idx, out, depth + 1);
        }
        ExprKind::StructLit { path, fields } => {
            out.push_str(&format!(
                "structlit {}\n",
                path.iter()
                    .map(|s| s.name.clone())
                    .collect::<Vec<_>>()
                    .join("::")
            ));
            for (n, v) in fields {
                pad(out, depth + 1);
                out.push_str(&format!(".{} =\n", n.name));
                render_expr(src, v, out, depth + 2);
            }
        }
        ExprKind::ListLit(items) => {
            out.push_str("list\n");
            for i in items {
                render_expr(src, i, out, depth + 1);
            }
        }
        ExprKind::MapLit(entries) => {
            out.push_str("map\n");
            for (k, v) in entries {
                render_expr(src, k, out, depth + 1);
                render_expr(src, v, out, depth + 1);
            }
        }
        ExprKind::If { cond, then, else_ } => {
            out.push_str("if\n");
            render_expr(src, cond, out, depth + 1);
            render_block(src, then, out, depth + 1);
            if let Some(e) = else_ {
                pad(out, depth);
                out.push_str("else\n");
                render_expr(src, e, out, depth + 1);
            }
        }
        ExprKind::IfLet {
            pat,
            scrutinee,
            then,
            else_,
        } => {
            out.push_str(&format!("if-let {}\n", render_pat(pat)));
            render_expr(src, scrutinee, out, depth + 1);
            render_block(src, then, out, depth + 1);
            if let Some(e) = else_ {
                render_expr(src, e, out, depth + 1);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            out.push_str("match\n");
            render_expr(src, scrutinee, out, depth + 1);
            for arm in arms {
                pad(out, depth + 1);
                out.push_str(&format!("arm {}\n", render_pat(&arm.pat)));
                // The guard is a full expression; rendering it as `<guard>`
                // hid its subtree from the goldens entirely.
                if let Some(guard) = &arm.guard {
                    pad(out, depth + 2);
                    out.push_str("guard\n");
                    render_expr(src, guard, out, depth + 3);
                }
                render_expr(src, &arm.body, out, depth + 2);
            }
        }
        ExprKind::While { cond, body } => {
            out.push_str("while\n");
            render_expr(src, cond, out, depth + 1);
            render_block(src, body, out, depth + 1);
        }
        ExprKind::Loop { body } => {
            out.push_str("loop\n");
            render_block(src, body, out, depth + 1);
        }
        ExprKind::For { var, iter, body } => {
            out.push_str(&format!("for {}\n", var.name));
            render_expr(src, iter, out, depth + 1);
            render_block(src, body, out, depth + 1);
        }
        ExprKind::Range { lo, hi, inclusive } => {
            out.push_str(&format!("range{}\n", if *inclusive { "=" } else { "" }));
            render_expr(src, lo, out, depth + 1);
            render_expr(src, hi, out, depth + 1);
        }
        ExprKind::Break => out.push_str("break\n"),
        ExprKind::Continue => out.push_str("continue\n"),
        ExprKind::Return(v) => {
            out.push_str("return\n");
            if let Some(v) = v {
                render_expr(src, v, out, depth + 1);
            }
        }
        ExprKind::Block(b) => {
            out.push_str("block\n");
            render_block(src, b, out, depth + 1);
        }
        ExprKind::Closure { params, body, .. } => {
            out.push_str(&format!(
                "closure |{}|\n",
                params
                    .iter()
                    .map(|(n, t)| format!("{}:{}", n.name, render_ty(t.as_ref())))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            render_expr(src, body, out, depth + 1);
        }
        ExprKind::Try(inner) => {
            out.push_str("try?\n");
            render_expr(src, inner, out, depth + 1);
        }
        ExprKind::Error => out.push_str("<error>\n"),
    }
}

fn render_ty(t: Option<&TypeExpr>) -> String {
    let Some(t) = t else { return "_".into() };
    match &t.kind {
        TypeExprKind::Name(n) => n.name.clone(),
        TypeExprKind::App(n, args) => format!(
            "{}[{}]",
            n.name,
            args.iter()
                .map(|a| render_ty(Some(a)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeExprKind::Fn(params, ret) => format!(
            "fn({}) -> {}",
            params
                .iter()
                .map(|p| render_ty(Some(p)))
                .collect::<Vec<_>>()
                .join(", "),
            ret.as_ref()
                .map(|r| render_ty(Some(r)))
                .unwrap_or("_".into())
        ),
        TypeExprKind::Dyn(n) => format!("dyn {}", n.name),
        TypeExprKind::Unit => "unit".into(),
        TypeExprKind::Error => "<error>".into(),
    }
}

fn render_lit_num(n: wscript_compiler::ast::LitNum) -> String {
    match n {
        wscript_compiler::ast::LitNum::Int(v) => v.to_string(),
        wscript_compiler::ast::LitNum::Float(v) => v.to_string(),
    }
}

fn render_pat(p: &Pattern) -> String {
    match &p.kind {
        PatternKind::Wildcard => "_".into(),
        PatternKind::Binding(n) => n.name.clone(),
        PatternKind::IntLit(n) => n.to_string(),
        PatternKind::QuantityLit { value, unit } => {
            format!("{}{}", render_lit_num(*value), unit.name)
        }
        PatternKind::BoolLit(b) => b.to_string(),
        PatternKind::CharLit(c) => format!("{c:?}"),
        PatternKind::StrLit(s) => format!("{s:?}"),
        PatternKind::Variant { path, args } => {
            let p: Vec<String> = path.iter().map(|s| s.name.clone()).collect();
            match args {
                VariantPatArgs::Unit => p.join("::"),
                VariantPatArgs::Tuple(pats) => format!(
                    "{}({})",
                    p.join("::"),
                    pats.iter().map(render_pat).collect::<Vec<_>>().join(", ")
                ),
                VariantPatArgs::Struct { fields, has_rest } => format!(
                    "{} {{ {}{} }}",
                    p.join("::"),
                    fields
                        .iter()
                        .map(|(n, sub)| format!("{}: {}", n.name, render_pat(sub)))
                        .collect::<Vec<_>>()
                        .join(", "),
                    if *has_rest { ", .." } else { "" }
                ),
            }
        }
        PatternKind::Struct {
            path,
            fields,
            has_rest,
        } => format!(
            "{} {{ {}{} }}",
            path.iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join("::"),
            fields
                .iter()
                .map(|(n, sub)| format!("{}: {}", n.name, render_pat(sub)))
                .collect::<Vec<_>>()
                .join(", "),
            if *has_rest { ", .." } else { "" }
        ),
        PatternKind::Or(alts) => alts.iter().map(render_pat).collect::<Vec<_>>().join(" | "),
        PatternKind::Error => "<error>".into(),
    }
}

fn parser_snapshot(fixture: &std::path::Path) -> Result<(), String> {
    let src = std::fs::read_to_string(fixture)
        .unwrap_or_else(|e| panic!("reading {}: {e}", fixture.display()));
    let parsed = wscript_compiler::parse(&src);

    let mut actual = render(&src, &parsed.file);
    // Diagnostics are part of the snapshot, not a precondition: recovery
    // behaviour is exactly what this suite needs to pin for the parser
    // refactor, and the old harness asserted `diags.is_empty()` so it could
    // never express a recovery case.
    actual.push_str("\n--- diagnostics ---\n");
    if parsed.diags.is_empty() {
        actual.push_str("(none)\n");
    }
    for d in &parsed.diags {
        actual.push_str(&format!(
            "{}  {}\n  {}\n",
            d.code,
            common::span_str(&src, d.span.lo, d.span.hi),
            d.message
        ));
    }
    common::check_snapshot(fixture, &actual)
}

#[test]
fn parser_snapshots() {
    let root = std::path::Path::new("tests/fixtures/parser");
    let fixtures = common::files(root, "wscript");
    assert!(!fixtures.is_empty(), "no fixtures under {}", root.display());
    let mut failures = Vec::new();
    for fixture in fixtures {
        if let Err(msg) = parser_snapshot(&fixture) {
            failures.push(msg);
        }
    }
    common::report(failures);
}

/// `ast::Visit` and this renderer are two independent traversals of the
/// same tree, so the risk is that they drift: someone adds an `ExprKind`,
/// updates one and not the other. Both are exhaustive matches, so neither
/// can silently *skip* a variant — but only comparing them catches a
/// variant wired into one walk and not the other.
///
/// Once the LSP's `expr_index` is reachable from a test (it needs the lib
/// target from #6), it belongs in this comparison too — it is a third
/// traversal, kept iterative for stack reasons.
#[test]
fn visit_reaches_the_same_nodes_as_the_renderer() {
    struct Ids(std::collections::HashSet<NodeId>);
    impl<'a> Visit<'a> for Ids {
        fn visit_expr(&mut self, e: &'a Expr) {
            self.0.insert(e.id);
            walk_expr(self, e);
        }
    }

    for fixture in common::files(std::path::Path::new("tests/fixtures/parser"), "wscript") {
        let src = std::fs::read_to_string(&fixture).unwrap();
        let parsed = wscript_compiler::parse(&src);

        let mut visited = Ids(std::collections::HashSet::new());
        walk_file(&mut visited, &parsed.file);

        let rendered: std::collections::HashSet<NodeId> = render(&src, &parsed.file)
            .split('#')
            .skip(1)
            .filter_map(|s| s.split_whitespace().next())
            .filter_map(|s| s.parse().ok())
            .collect();

        let only_visit: Vec<_> = visited.0.difference(&rendered).collect();
        let only_render: Vec<_> = rendered.difference(&visited.0).collect();
        assert!(
            only_visit.is_empty() && only_render.is_empty(),
            "{}: traversals disagree\n  ast::Visit only: {only_visit:?}\n  renderer only:   {only_render:?}",
            fixture.display()
        );
    }
}

/// Node ids must be unique across a file — the checker's side tables are
/// keyed by them, so a collision silently merges two nodes' resolutions.
/// The snapshots render every id, but only a scan proves uniqueness.
#[test]
fn node_ids_are_unique() {
    for fixture in common::files(std::path::Path::new("tests/fixtures/parser"), "wscript") {
        let src = std::fs::read_to_string(&fixture).unwrap();
        let rendered = render(&src, &wscript_compiler::parse(&src).file);
        let mut seen = std::collections::HashSet::new();
        for id in rendered
            .split('#')
            .skip(1)
            .filter_map(|s| s.split_whitespace().next())
        {
            assert!(
                seen.insert(id.to_string()),
                "duplicate node id #{id} in {}",
                fixture.display()
            );
        }
    }
}
