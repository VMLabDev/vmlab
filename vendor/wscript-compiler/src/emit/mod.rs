//! Bytecode emitter: walks the checked AST and produces a `CompiledUnit`.
//!
//! Register conventions per frame:
//! `[0 .. n_locals)` — locals in checker `LocalId` order (params first);
//! `[n_locals .. n_locals + n_captures)` — capture cells loaded in the
//! prologue (closures only); temps grow above that, stack-style.
//!
//! Locals captured by closures hold a `Cell` value instead of the value
//! itself; reads/writes go through `CellGet`/`CellSet`.
//!
//! That layout is not prose here: it is [`code::RegAlloc`], which mints
//! every register the emitter uses, and [`code::CodeBuf`], which owns the
//! instruction stream and the labels branching through it. Temps are
//! allocated by the scoped `with_scratch` / `with_window` / `in_temps`
//! methods below and released when their scope ends, so a frame is sized
//! by the deepest expression rather than by the sum of every expression.
//! `wscript_core::verify` is the other half of that contract: it checks
//! that no register operand escaped the frame the emitter claimed.

mod code;

use std::collections::HashMap;

use code::{CodeBuf, Label, Reg, RegAlloc, ValueReg, Window};

use wscript_core::bytecode::{
    Builtin, CallTarget, CaptureSrc, CompiledUnit, Const, FaultCode, FnProto, Instr, VTable,
};
use wscript_core::defs::{self, Factor};
use wscript_core::diag::Diagnostic;
use wscript_core::span::Span;

use crate::ast::*;
use crate::check::{
    BinOpKind, CallKind, CapSrc, CheckResult, ConvKind, FnSource, ForKind, IndexKind, LocalId,
    MethodRes, PathRes, PreludeFn, PrimKind, StructLitRes, TryKind, UnOpKind, VarRes,
};

/// Emit a whole (possibly multi-file) program into one merged unit.
///
/// `files` must be in the same order the checker saw them. The returned
/// diagnostics are *internal* errors (`E9999`): emit runs only after the
/// checker reported none, so anything here is a compiler bug rather than a
/// fault in the script. They are returned instead of panicking because
/// wscript is an embedding library — a host should survive a compiler bug
/// with an error, not a crash.
pub fn emit_files(files: &[&SourceFile], res: &CheckResult) -> (CompiledUnit, Vec<Diagnostic>) {
    let mut em = Emitter {
        files,
        res,
        consts: Vec::new(),
        const_map: HashMap::new(),
        protos: (0..res.fn_infos.len()).map(|_| None).collect(),
        ices: Vec::new(),
    };
    for proto in 0..res.fn_infos.len() {
        em.ensure_proto(proto as u32);
    }
    static NEXT_UNIT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let ices = std::mem::take(&mut em.ices);
    let unit = CompiledUnit {
        id: NEXT_UNIT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        protos: em
            .protos
            .into_iter()
            .map(|p| p.expect("all protos emitted"))
            .collect(),
        consts: em.consts,
        defs: res.defs.clone(),
        vtables: res
            .vtables
            .iter()
            .map(|targets| VTable {
                targets: targets.iter().map(|&p| CallTarget::Proto(p)).collect(),
            })
            .collect(),
        impls: res.impl_maps.clone(),
        exports: res.exports.clone(),
        generic_fns: res.generic_fns.clone(),
        // Filled by the compile pipeline (which knows the file layout).
        source_map: Default::default(),
    };
    (unit, ices)
}

#[derive(PartialEq, Eq, Hash)]
enum ConstKey {
    Int(i64),
    Float(u64),
    Char(char),
    Str(String),
}

struct Emitter<'a> {
    files: &'a [&'a SourceFile],
    res: &'a CheckResult,
    consts: Vec<Const>,
    const_map: HashMap<ConstKey, u32>,
    protos: Vec<Option<FnProto>>,
    /// Internal errors: a node the checker should have resolved but did
    /// not. See [`emit_files`].
    ices: Vec<Diagnostic>,
}

impl<'a> Emitter<'a> {
    fn ensure_proto(&mut self, proto: u32) {
        if self.protos[proto as usize].is_some() {
            return;
        }
        // Placeholder to stop recursion (closures referencing themselves
        // can't occur, but keep it robust).
        self.protos[proto as usize] = Some(FnProto {
            name: String::new(),
            n_params: 0,
            n_regs: 0,
            code: vec![],
            spans: vec![],
            captures: vec![],
        });
        let info = &self.res.fn_infos[proto as usize];
        let built = match info.source {
            FnSource::Top { file, item } => {
                let Item::Fn(f) = &self.files[file].items[item] else {
                    unreachable!()
                };
                self.emit_fn(proto, &f.params, FnBody::Block(&f.body), f.span)
            }
            FnSource::Method { file, item, fn_idx } => {
                let Item::Impl(im) = &self.files[file].items[item] else {
                    unreachable!()
                };
                let f = &im.fns[fn_idx];
                self.emit_fn(proto, &f.params, FnBody::Block(&f.body), f.span)
            }
            FnSource::Closure { node } => {
                let Some(body) = self.files.iter().find_map(|f| find_closure(f, node)) else {
                    unreachable!("closure node not found")
                };
                self.emit_fn(proto, &[], FnBody::Expr(body), body.span)
            }
            FnSource::Synthesized => FnProto {
                name: self.res.fn_infos[proto as usize].name.clone(),
                n_params: 0,
                n_regs: 1,
                code: vec![Instr::RetUnit],
                spans: vec![Span::DUMMY],
                captures: vec![],
            },
        };
        self.protos[proto as usize] = Some(built);
    }

    fn emit_fn(&mut self, proto: u32, _params: &[Param], body: FnBody<'a>, span: Span) -> FnProto {
        let info = self.res.fn_infos[proto as usize].clone();
        let n_locals = info.n_locals.max(info.sig.params.len() as u32);
        let n_caps = info.captures.len() as u16;
        let mut f = FnEmitter {
            em: self,
            code: CodeBuf::new(span),
            regs: RegAlloc::new(n_locals as u16, n_caps),
            captured: info.captured.clone(),
            loops: Vec::new(),
        };
        // Prologue: load capture cells, box captured params.
        for slot in 0..n_caps {
            let dst = f.cap_reg(slot);
            f.push(Instr::LoadCapture { dst: dst.0, slot });
        }
        let n_params = info.sig.params.len() as u16;
        for p in 0..n_params {
            if f.captured.contains(&(p as u32)) {
                let reg = f.local_reg(p as LocalId);
                f.push(Instr::NewCell {
                    dst: reg.0,
                    src: reg.0,
                });
            }
        }
        // The return value's register outlives every temp under it, so it
        // is allocated outside any scope.
        let ret = f.regs.scratch().reg();
        match body {
            FnBody::Block(b) => f.emit_block(b, Some(ret)),
            FnBody::Expr(e) => f.emit_into(e, ret),
        }
        f.push(Instr::Ret { src: ret.0 });
        let n_regs = f.regs.frame_size();
        let (code, spans) = f.code.finish(&info.name);
        FnProto {
            name: info.name.clone(),
            n_params,
            n_regs,
            code,
            spans,
            captures: info
                .captures
                .iter()
                .map(|c| match c {
                    CapSrc::Local(l) => CaptureSrc::Reg(*l as u16),
                    CapSrc::Capture(s) => CaptureSrc::Capture(*s),
                })
                .collect(),
        }
    }

    /// Record an internal error against `span`.
    fn ice(&mut self, span: Span, what: &str) {
        self.ices.push(Diagnostic::error(
            "E9999",
            span,
            format!("internal compiler error: {what} was not resolved"),
        ));
    }

    fn intern_const(&mut self, c: Const) -> u32 {
        let key = match &c {
            Const::Int(n) => ConstKey::Int(*n),
            Const::Float(x) => ConstKey::Float(x.to_bits()),
            Const::Char(ch) => ConstKey::Char(*ch),
            Const::Str(s) => ConstKey::Str(s.to_string()),
            Const::Bool(_) | Const::Unit => {
                // Never interned (LoadBool / LoadUnit exist).
                let idx = self.consts.len() as u32;
                self.consts.push(c);
                return idx;
            }
        };
        if let Some(&idx) = self.const_map.get(&key) {
            return idx;
        }
        let idx = self.consts.len() as u32;
        self.consts.push(c);
        self.const_map.insert(key, idx);
        idx
    }
}

enum FnBody<'a> {
    Block(&'a Block),
    Expr(&'a Expr),
}

/// Find the closure expression with the given node id (closure protos are
/// emitted from their AST node).
fn find_closure(file: &SourceFile, node: NodeId) -> Option<&Expr> {
    struct Finder<'a> {
        node: NodeId,
        found: Option<&'a Expr>,
    }
    impl<'a> Visit<'a> for Finder<'a> {
        fn visit_expr(&mut self, e: &'a Expr) {
            if self.found.is_some() {
                return;
            }
            if let ExprKind::Closure { body, .. } = &e.kind
                && e.id == self.node
            {
                self.found = Some(body);
                return;
            }
            walk_expr(self, e);
        }
    }
    let mut finder = Finder { node, found: None };
    walk_file(&mut finder, file);
    finder.found
}

/// The two targets a `break` or `continue` in this loop's body jumps to.
///
/// One `continue` lowering for every loop: the frame carries the label,
/// and where it lands is the loop's business — the top of the test for
/// `while`/`loop`, the step for `for`.
#[derive(Clone, Copy)]
struct LoopFrame {
    /// Bound past the end of the loop; also where the loop's own exit test
    /// jumps.
    brk: Label,
    cont: Label,
}

struct FnEmitter<'e, 'a> {
    em: &'e mut Emitter<'a>,
    code: CodeBuf,
    regs: RegAlloc,
    captured: std::collections::HashSet<LocalId>,
    loops: Vec<LoopFrame>,
}

impl<'e, 'a> FnEmitter<'e, 'a> {
    // ----------------------------------------------------------- helpers

    fn push(&mut self, i: Instr) {
        self.code.push(i);
    }

    fn local_reg(&self, local: LocalId) -> Reg {
        self.regs.local(local)
    }

    fn cap_reg(&self, slot: u16) -> Reg {
        self.regs.capture(slot)
    }

    // ------------------------------------------------------- temp scopes
    //
    // Scoped rather than guard-based: a guard holding `&mut self.regs`
    // would block the `&mut self` that `emit_into` needs, so the scope is
    // a closure that gets the emitter back.

    /// Run `f` in a temp scope: every scratch and window allocated inside
    /// it is released when it returns.
    fn in_temps<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let mark = self.regs.mark();
        let out = f(self);
        self.regs.release(mark);
        out
    }

    /// Run `f` with one scratch register, released when it returns.
    fn with_scratch<R>(&mut self, f: impl FnOnce(&mut Self, Reg) -> R) -> R {
        self.in_temps(|c| {
            let r = c.regs.scratch().reg();
            f(c, r)
        })
    }

    /// Run `f` with a contiguous `n`-register window, released when it
    /// returns.
    ///
    /// Anything `f` allocates comes from above the window, so the window
    /// stays unbroken however deeply the argument expressions nest. What
    /// must *not* go inside is a value the window's own base depends on —
    /// `emit_call` evaluates a callee before opening the window for its
    /// arguments, not inside it.
    fn with_window<R>(&mut self, n: u16, f: impl FnOnce(&mut Self, Window) -> R) -> R {
        self.in_temps(|c| {
            let w = c.regs.window(n);
            f(c, w)
        })
    }

    // ------------------------------------------------------------ blocks

    /// Emit a block; the tail expression value (if any) lands in `dst`.
    /// If the block has no value, `dst` (when given) is set to unit.
    fn emit_block(&mut self, block: &Block, dst: Option<Reg>) {
        self.in_temps(|c| {
            let n = block.stmts.len();
            let mut produced = false;
            for (i, stmt) in block.stmts.iter().enumerate() {
                let last = i + 1 == n;
                // Each statement's temps die with it.
                c.in_temps(|c| match stmt {
                    Stmt::Let { init, id, span, .. } => {
                        c.code.set_span(*span);
                        let local = *c.em.res.decl_locals.get(id).expect("let stmt resolved");
                        c.emit_init_local(local, init);
                    }
                    Stmt::LetElse {
                        pat,
                        init,
                        else_block,
                        span,
                        ..
                    } => {
                        c.code.set_span(*span);
                        c.emit_let_else(pat, init, else_block);
                    }
                    Stmt::Expr { expr, terminated } => {
                        match dst.filter(|_| last && !*terminated) {
                            Some(d) => {
                                c.emit_into(expr, d);
                                produced = true;
                            }
                            // Evaluated for its effects; the value is dropped.
                            None => c.with_scratch(|c, scratch| c.emit_into(expr, scratch)),
                        }
                    }
                });
            }
            if !produced && let Some(d) = dst {
                c.push(Instr::LoadUnit { dst: d.0 });
            }
        })
    }

    /// `let pat = init else { … }` — the else block diverges, so a fall
    /// through it is a compiler bug and faults.
    fn emit_let_else(&mut self, pat: &Pattern, init: &Expr, else_block: &Block) {
        let done = self.code.label();
        let fail = self.code.label();
        self.with_scratch(|c, v| {
            c.emit_into(init, v);
            c.emit_pattern(pat, v, fail);
        });
        self.code.jump(done);
        self.code.bind(fail);
        self.with_scratch(|c, scratch| c.emit_block(else_block, Some(scratch)));
        self.push(Instr::Fault {
            code: FaultCode::UnreachableMatch,
        });
        self.code.bind(done);
    }

    /// `let local = init` — store into the local's register, boxing into a
    /// cell when the local is captured.
    fn emit_init_local(&mut self, local: LocalId, init: &Expr) {
        let reg = self.local_reg(local);
        if self.captured.contains(&local) {
            self.with_scratch(|c, tmp| {
                c.emit_into(init, tmp);
                c.push(Instr::NewCell {
                    dst: reg.0,
                    src: tmp.0,
                });
            });
        } else {
            self.emit_into(init, reg);
        }
    }

    // ------------------------------------------------------- expressions

    /// Emit `e`, returning where its value landed. A read of a plain local
    /// borrows the local's own register instead of copying it into a temp,
    /// so the [`ValueReg`] says which of the two the caller got rather than
    /// leaving it to be inferred from the expression.
    ///
    /// The scratch, when there is one, belongs to the caller's temp scope —
    /// it stays live until that scope ends, which is what lets an operand
    /// outlive the sub-expression that produced it.
    fn emit_value(&mut self, e: &Expr) -> ValueReg {
        if !self.em.res.dyn_wraps.contains_key(&e.id)
            && let ExprKind::Path(_) = &e.kind
            && let Some(VarRes::Local(l)) = self.em.res.var_ref(e.id)
            && !self.captured.contains(l)
        {
            return ValueReg::Borrowed(self.local_reg(*l));
        }
        let dst = self.regs.scratch();
        self.emit_into(e, dst.reg());
        ValueReg::Owned(dst)
    }

    /// Emit `e` into `dst`. Everything the expression allocated to get
    /// there is dead once it lands, so the whole lowering runs in a temp
    /// scope: a frame is sized by its deepest expression, not by the sum
    /// of all of them.
    fn emit_into(&mut self, e: &Expr, dst: Reg) {
        let saved_span = self.code.span();
        self.code.set_span(e.span);
        self.in_temps(|c| c.emit_into_inner(e, dst));
        if let Some(&vt) = self.em.res.dyn_wraps.get(&e.id) {
            self.push(Instr::MakeDyn {
                dst: dst.0,
                src: dst.0,
                vt,
            });
        }
        self.code.set_span(saved_span);
    }

    fn emit_into_inner(&mut self, e: &Expr, dst: Reg) {
        match &e.kind {
            ExprKind::IntLit(n) => self.emit_int(*n, dst),
            // Already folded into base units by the checker.
            ExprKind::QuantityLit { .. } => match self.em.res.quantity_lit(e.id).as_ref() {
                Some(Factor::Int(n)) => self.emit_int(*n, dst),
                Some(Factor::Float(f)) => {
                    let k = self.em.intern_const(Const::Float(*f));
                    self.push(Instr::LoadConst { dst: dst.0, k });
                }
                None => {
                    self.push(Instr::LoadUnit { dst: dst.0 });
                }
            },
            ExprKind::FloatLit(x) => {
                let k = self.em.intern_const(Const::Float(*x));
                self.push(Instr::LoadConst { dst: dst.0, k });
            }
            ExprKind::BoolLit(b) => {
                self.push(Instr::LoadBool { dst: dst.0, v: *b });
            }
            ExprKind::CharLit(c) => {
                let k = self.em.intern_const(Const::Char(*c));
                self.push(Instr::LoadConst { dst: dst.0, k });
            }
            ExprKind::StrLit(s) => {
                let k = self.em.intern_const(Const::Str(s.as_str().into()));
                self.push(Instr::LoadConst { dst: dst.0, k });
            }
            ExprKind::StrInterp(parts) => self.emit_str_interp(parts, dst),
            ExprKind::UnitLit => {
                self.push(Instr::LoadUnit { dst: dst.0 });
            }
            ExprKind::Error => {
                self.push(Instr::LoadUnit { dst: dst.0 });
            }
            ExprKind::Path(_) => self.emit_path(e, dst),
            ExprKind::Unary { expr, .. } => self.emit_unary(e, expr, dst),
            ExprKind::Binary { lhs, rhs, .. } => self.emit_binary(e, lhs, rhs, dst),
            ExprKind::Assign { target, value, op } => {
                self.emit_assign(e, target, value, op.is_some());
                self.push(Instr::LoadUnit { dst: dst.0 });
            }
            ExprKind::Call { callee, args } => self.emit_call(e, callee, args, dst),
            ExprKind::MethodCall { recv, args, .. } => self.emit_method_call(e, recv, args, dst),
            ExprKind::Field { obj, .. } => {
                // `d.ms` on a unit value is a constant divide, not a field
                // read — unit values are plain numbers at runtime.
                if let Some(&conv) = self.em.res.unit_conv(e.id).as_ref() {
                    self.emit_unit_conv(obj, conv, dst);
                    return;
                }
                let o = self.emit_value(obj);
                let idx = match self.em.res.field_idx(e.id) {
                    Some(idx) => idx,
                    None => {
                        self.em.ice(e.span, "field access");
                        0
                    }
                };
                self.push(Instr::GetField {
                    dst: dst.0,
                    obj: o.reg().0,
                    idx,
                });
            }
            ExprKind::Index { obj, idx } => {
                let kind = self.em.res.index(e.id).cloned();
                match kind {
                    Some(IndexKind::List) => {
                        let o = self.emit_value(obj);
                        let i = self.emit_value(idx);
                        self.push(Instr::ListIndexGet {
                            dst: dst.0,
                            list: o.reg().0,
                            idx: i.reg().0,
                        });
                    }
                    Some(IndexKind::Map) => {
                        let o = self.emit_value(obj);
                        let i = self.emit_value(idx);
                        self.push(Instr::MapIndexGet {
                            dst: dst.0,
                            map: o.reg().0,
                            key: i.reg().0,
                        });
                    }
                    Some(IndexKind::UserGet { proto }) => self.with_window(2, |c, w| {
                        c.emit_into(obj, w.at(0));
                        c.emit_into(idx, w.at(1));
                        c.push(Instr::Call {
                            dst: dst.0,
                            base: w.base().0,
                            nargs: w.len(),
                            target: CallTarget::Proto(proto),
                        });
                    }),
                    None => {
                        self.push(Instr::LoadUnit { dst: dst.0 });
                    }
                }
            }
            ExprKind::StructLit { fields, .. } => self.emit_struct_lit(e, fields, dst),
            ExprKind::ListLit(items) => self.with_window(items.len() as u16, |c, w| {
                for (i, item) in items.iter().enumerate() {
                    c.emit_into(item, w.at(i as u16));
                }
                c.push(Instr::NewList {
                    dst: dst.0,
                    base: w.base().0,
                    n: w.len(),
                });
            }),
            ExprKind::MapLit(entries) => self.with_window(entries.len() as u16 * 2, |c, w| {
                for (i, (k, v)) in entries.iter().enumerate() {
                    c.emit_into(k, w.at(i as u16 * 2));
                    c.emit_into(v, w.at(i as u16 * 2 + 1));
                }
                c.push(Instr::NewMap {
                    dst: dst.0,
                    base: w.base().0,
                    n: entries.len() as u16,
                });
            }),
            ExprKind::If { cond, then, else_ } => {
                let to_else = self.code.label();
                let to_end = self.code.label();
                let c = self.emit_value(cond);
                self.code.jump_if_false(c.reg(), to_else);
                self.emit_block(then, Some(dst));
                self.code.jump(to_end);
                self.code.bind(to_else);
                match else_ {
                    Some(else_expr) => self.emit_into(else_expr, dst),
                    None => {
                        self.push(Instr::LoadUnit { dst: dst.0 });
                    }
                }
                self.code.bind(to_end);
            }
            ExprKind::IfLet {
                pat,
                scrutinee,
                then,
                else_,
            } => {
                let fail = self.code.label();
                let to_end = self.code.label();
                self.with_scratch(|c, v| {
                    c.emit_into(scrutinee, v);
                    c.emit_pattern(pat, v, fail);
                    c.emit_block(then, Some(dst));
                });
                self.code.jump(to_end);
                self.code.bind(fail);
                match else_ {
                    Some(else_expr) => self.emit_into(else_expr, dst),
                    None => {
                        self.push(Instr::LoadUnit { dst: dst.0 });
                    }
                }
                self.code.bind(to_end);
            }
            ExprKind::Match { scrutinee, arms } => self.emit_match(scrutinee, arms, dst),
            ExprKind::While { cond, body } => {
                let frame = self.enter_loop_at_top();
                let c = self.emit_value(cond);
                self.code.jump_if_false(c.reg(), frame.brk);
                self.emit_block(body, None);
                self.code.jump(frame.cont);
                self.exit_loop();
                self.push(Instr::LoadUnit { dst: dst.0 });
            }
            ExprKind::Loop { body } => {
                let frame = self.enter_loop_at_top();
                self.emit_block(body, None);
                self.code.jump(frame.cont);
                self.exit_loop();
                self.push(Instr::LoadUnit { dst: dst.0 });
            }
            ExprKind::For { iter, body, .. } => self.emit_for(e, iter, body, dst),
            ExprKind::Range { .. } => {
                // Only reachable on checker-rejected input.
                self.push(Instr::LoadUnit { dst: dst.0 });
            }
            // Outside a loop these are checker errors, and emit never runs
            // on a program that has one — so there is no target to invent.
            ExprKind::Break => {
                if let Some(brk) = self.loops.last().map(|f| f.brk) {
                    self.code.jump(brk);
                }
            }
            ExprKind::Continue => {
                if let Some(cont) = self.loops.last().map(|f| f.cont) {
                    self.code.jump(cont);
                }
            }
            ExprKind::Return(value) => match value {
                Some(v) => {
                    let r = self.emit_value(v);
                    self.push(Instr::Ret { src: r.reg().0 });
                }
                None => {
                    self.push(Instr::RetUnit);
                }
            },
            ExprKind::Block(b) => self.emit_block(b, Some(dst)),
            ExprKind::Closure { .. } => {
                let proto = self
                    .em
                    .res
                    .closure(e.id)
                    .map(|c| c.proto)
                    .expect("closure resolved");
                self.em.ensure_proto(proto);
                self.push(Instr::MakeClosure { dst: dst.0, proto });
            }
            ExprKind::Try(inner) => {
                let propagate_tag = match self.em.res.try_kind(e.id) {
                    Some(TryKind::Result) => defs::TAG_ERR,
                    _ => defs::TAG_NONE,
                };
                let skip = self.code.label();
                self.with_scratch(|c, v| {
                    c.emit_into(inner, v);
                    c.with_scratch(|c, cond| {
                        c.in_temps(|c| {
                            let tag = c.regs.scratch().reg();
                            let lit = c.regs.scratch().reg();
                            c.push(Instr::GetTag {
                                dst: tag.0,
                                obj: v.0,
                            });
                            c.emit_int(propagate_tag as i64, lit);
                            c.push(Instr::EqI {
                                dst: cond.0,
                                a: tag.0,
                                b: lit.0,
                            });
                        });
                        c.code.jump_if_false(cond, skip);
                    });
                    // Propagate the same None/Err value (PRD §3.5).
                    c.push(Instr::Ret { src: v.0 });
                    c.code.bind(skip);
                    c.push(Instr::GetField {
                        dst: dst.0,
                        obj: v.0,
                        idx: 0,
                    });
                });
            }
        }
    }

    /// Open a loop: `break` and `continue` inside its body jump to the
    /// frame's labels, and where those land is this loop's business — the
    /// step, for a `for`. The frame is returned by value so a lowering can
    /// jump to its labels without borrowing `self.loops`.
    fn enter_loop(&mut self) -> LoopFrame {
        let frame = LoopFrame {
            brk: self.code.label(),
            cont: self.code.label(),
        };
        self.loops.push(frame);
        frame
    }

    /// Open a loop whose `continue` target is the instruction about to be
    /// emitted — the test of a `while`, the top of a `loop`.
    fn enter_loop_at_top(&mut self) -> LoopFrame {
        let frame = self.enter_loop();
        self.code.bind(frame.cont);
        frame
    }

    /// Close the innermost loop: `break` — and the loop's own exit test —
    /// land here.
    fn exit_loop(&mut self) {
        let frame = self.loops.pop().expect("a loop frame to close");
        self.code.bind(frame.brk);
    }

    fn emit_int(&mut self, n: i64, dst: Reg) {
        if let Ok(v) = i32::try_from(n) {
            self.push(Instr::LoadInt { dst: dst.0, v });
        } else {
            let k = self.em.intern_const(Const::Int(n));
            self.push(Instr::LoadConst { dst: dst.0, k });
        }
    }

    fn emit_path(&mut self, e: &Expr, dst: Reg) {
        if let Some(res) = self.em.res.var_ref(e.id) {
            match res {
                VarRes::Local(l) => {
                    let src = self.local_reg(*l);
                    if self.captured.contains(l) {
                        self.push(Instr::CellGet {
                            dst: dst.0,
                            cell: src.0,
                        });
                    } else {
                        self.push(Instr::Move {
                            dst: dst.0,
                            src: src.0,
                        });
                    }
                }
                VarRes::Capture(slot) => {
                    let cell = self.cap_reg(*slot);
                    self.push(Instr::CellGet {
                        dst: dst.0,
                        cell: cell.0,
                    });
                }
            }
            return;
        }
        match self.em.res.path_res(e.id) {
            Some(PathRes::FnValue(proto)) => {
                let proto = *proto;
                self.em.ensure_proto(proto);
                self.push(Instr::MakeClosure { dst: dst.0, proto });
            }
            Some(PathRes::Const(c)) => match c {
                Const::Unit => {
                    self.push(Instr::LoadUnit { dst: dst.0 });
                }
                Const::Bool(b) => {
                    let v = *b;
                    self.push(Instr::LoadBool { dst: dst.0, v });
                }
                other => {
                    let k = self.em.intern_const(other.clone());
                    self.push(Instr::LoadConst { dst: dst.0, k });
                }
            },
            Some(PathRes::Variant { def, tag }) => {
                let (def, tag) = (def.0, *tag as u16);
                // No payload, so there is no window to reserve and the VM
                // reads no register from `base`.
                self.push(Instr::NewEnum {
                    dst: dst.0,
                    def,
                    tag,
                    base: 0,
                    n: 0,
                });
            }
            None => {
                // Checker error path.
                self.push(Instr::LoadUnit { dst: dst.0 });
            }
        }
    }

    fn emit_unary(&mut self, e: &Expr, operand: &Expr, dst: Reg) {
        let kind = self.em.res.un_op(e.id);
        match kind {
            Some(UnOpKind::NegInt) => {
                let src = self.emit_value(operand);
                self.push(Instr::NegI {
                    dst: dst.0,
                    src: src.reg().0,
                });
            }
            Some(UnOpKind::NegFloat) => {
                let src = self.emit_value(operand);
                self.push(Instr::NegF {
                    dst: dst.0,
                    src: src.reg().0,
                });
            }
            Some(UnOpKind::Not) => {
                let src = self.emit_value(operand);
                self.push(Instr::Not {
                    dst: dst.0,
                    src: src.reg().0,
                });
            }
            Some(UnOpKind::NegCall { proto }) => self.with_window(1, |c, w| {
                c.emit_into(operand, w.at(0));
                c.push(Instr::Call {
                    dst: dst.0,
                    base: w.base().0,
                    nargs: w.len(),
                    target: CallTarget::Proto(proto),
                });
            }),
            None => {
                self.push(Instr::LoadUnit { dst: dst.0 });
            }
        }
    }

    fn emit_binary(&mut self, e: &Expr, lhs: &Expr, rhs: &Expr, dst: Reg) {
        use crate::ast::BinOp as B;
        let Some(kind) = self.em.res.bin_op(e.id) else {
            self.em.ice(e.span, "binary operator");
            self.push(Instr::LoadUnit { dst: dst.0 });
            return;
        };
        // The operand registers of the three-address forms.
        let operands = |c: &mut Self| {
            let a = c.emit_value(lhs).reg().0;
            let b = c.emit_value(rhs).reg().0;
            (a, b)
        };
        let dst = dst.0;
        match kind {
            BinOpKind::And => {
                let short = self.code.label();
                self.emit_into(lhs, Reg(dst));
                self.code.jump_if_false(Reg(dst), short);
                self.emit_into(rhs, Reg(dst));
                self.code.bind(short);
            }
            BinOpKind::Or => {
                let short = self.code.label();
                self.emit_into(lhs, Reg(dst));
                self.code.jump_if_true(Reg(dst), short);
                self.emit_into(rhs, Reg(dst));
                self.code.bind(short);
            }
            BinOpKind::IntArith(op) => {
                let (a, b) = operands(self);
                let i = match op {
                    B::Add => Instr::AddI { dst, a, b },
                    B::Sub => Instr::SubI { dst, a, b },
                    B::Mul => Instr::MulI { dst, a, b },
                    B::Div => Instr::DivI { dst, a, b },
                    _ => Instr::RemI { dst, a, b },
                };
                self.push(i);
            }
            BinOpKind::FloatArith(op) => {
                let (a, b) = operands(self);
                let i = match op {
                    B::Add => Instr::AddF { dst, a, b },
                    B::Sub => Instr::SubF { dst, a, b },
                    B::Mul => Instr::MulF { dst, a, b },
                    B::Div => Instr::DivF { dst, a, b },
                    _ => Instr::RemF { dst, a, b },
                };
                self.push(i);
            }
            BinOpKind::Concat => {
                let (a, b) = operands(self);
                self.push(Instr::ConcatStr { dst, a, b });
            }
            BinOpKind::EqPrim { kind, negate } => {
                let (a, b) = operands(self);
                let i = match kind {
                    PrimKind::Int => Instr::EqI { dst, a, b },
                    PrimKind::Float => Instr::EqF { dst, a, b },
                    PrimKind::Bool => Instr::EqBool { dst, a, b },
                    PrimKind::Char => Instr::EqChar { dst, a, b },
                    PrimKind::Str => Instr::EqStr { dst, a, b },
                };
                self.push(i);
                if negate {
                    self.push(Instr::Not { dst, src: dst });
                }
            }
            BinOpKind::CmpPrim { kind, op } => {
                let (a, b) = operands(self);
                self.emit_cmp_prim(kind, op, Reg(a), Reg(b), Reg(dst));
            }
            BinOpKind::EqValue { negate } => {
                self.emit_operand_call(lhs, rhs, Reg(dst), CallTarget::Builtin(Builtin::ValueEq));
                if negate {
                    self.push(Instr::Not { dst, src: dst });
                }
            }
            BinOpKind::EqCall { proto, negate } => {
                self.emit_operand_call(lhs, rhs, Reg(dst), CallTarget::Proto(proto));
                if negate {
                    self.push(Instr::Not { dst, src: dst });
                }
            }
            BinOpKind::CmpValue { op } => {
                self.emit_cmp_call(
                    lhs,
                    rhs,
                    op,
                    Reg(dst),
                    CallTarget::Builtin(Builtin::ValueCmp),
                );
            }
            BinOpKind::CmpCall { proto, op } => {
                self.emit_cmp_call(lhs, rhs, op, Reg(dst), CallTarget::Proto(proto));
            }
            BinOpKind::ArithCall { proto } => {
                self.emit_operand_call(lhs, rhs, Reg(dst), CallTarget::Proto(proto));
            }
        }
    }

    /// `target(lhs, rhs)` — the two-argument call an operator lowers to
    /// when the operand type implements it (`Eq`, `Ord`, `Add`, …).
    fn emit_operand_call(&mut self, lhs: &Expr, rhs: &Expr, dst: Reg, target: CallTarget) {
        self.with_window(2, |c, w| {
            c.emit_into(lhs, w.at(0));
            c.emit_into(rhs, w.at(1));
            c.push(Instr::Call {
                dst: dst.0,
                base: w.base().0,
                nargs: w.len(),
                target,
            });
        });
    }

    /// `lhs <op> rhs` where the comparison lowers to a three-way compare:
    /// call it, then test the result against zero.
    fn emit_cmp_call(
        &mut self,
        lhs: &Expr,
        rhs: &Expr,
        op: crate::ast::BinOp,
        dst: Reg,
        target: CallTarget,
    ) {
        self.in_temps(|c| {
            let ord = c.regs.scratch().reg();
            c.emit_operand_call(lhs, rhs, ord, target);
            let zero = c.regs.scratch().reg();
            c.push(Instr::LoadInt { dst: zero.0, v: 0 });
            c.emit_cmp_prim(PrimKind::Int, op, ord, zero, dst);
        });
    }

    fn emit_cmp_prim(&mut self, kind: PrimKind, op: crate::ast::BinOp, a: Reg, b: Reg, dst: Reg) {
        use crate::ast::BinOp as B;
        // Gt/Ge are emitted as swapped Lt/Le.
        let (x, y, le) = match op {
            B::Lt => (a.0, b.0, false),
            B::Le => (a.0, b.0, true),
            B::Gt => (b.0, a.0, false),
            _ => (b.0, a.0, true),
        };
        let dst = dst.0;
        let i = match (kind, le) {
            (PrimKind::Int, false) => Instr::LtI { dst, a: x, b: y },
            (PrimKind::Int, true) => Instr::LeI { dst, a: x, b: y },
            (PrimKind::Float, false) => Instr::LtF { dst, a: x, b: y },
            (PrimKind::Float, true) => Instr::LeF { dst, a: x, b: y },
            (PrimKind::Char, false) => Instr::LtChar { dst, a: x, b: y },
            (PrimKind::Char, true) => Instr::LeChar { dst, a: x, b: y },
            (PrimKind::Str, false) => Instr::LtStr { dst, a: x, b: y },
            (PrimKind::Str, true) => Instr::LeStr { dst, a: x, b: y },
            (PrimKind::Bool, _) => Instr::EqBool { dst, a: x, b: y },
        };
        self.push(i);
    }

    fn emit_assign(&mut self, e: &Expr, target: &Expr, value: &Expr, compound: bool) {
        // Compound assignment: the checker recorded the operator lowering
        // under the Assign node's id. The place is evaluated ONCE — read
        // current value, apply the operator, write back.
        let arith = if compound {
            self.em.res.bin_op(e.id)
        } else {
            None
        };
        if compound && arith.is_none() {
            // Checker rejected the operator; evaluate for effects.
            self.emit_for_effect(value);
            return;
        }
        match &target.kind {
            ExprKind::Path(_) => match self.em.res.var_ref(target.id) {
                Some(VarRes::Local(l)) => {
                    let l = *l;
                    let reg = self.local_reg(l);
                    if self.captured.contains(&l) {
                        self.emit_into_cell(reg, value, arith.as_ref());
                    } else if let Some(kind) = &arith {
                        self.with_scratch(|c, tmp| {
                            c.emit_into(value, tmp);
                            c.emit_arith_kind(kind, reg, tmp, reg);
                        });
                    } else {
                        self.emit_into(value, reg);
                    }
                }
                Some(VarRes::Capture(slot)) => {
                    let cell = self.cap_reg(*slot);
                    self.emit_into_cell(cell, value, arith.as_ref());
                }
                None => self.emit_for_effect(value),
            },
            ExprKind::Field { obj, .. } => {
                let idx = self.em.res.field_idx(target.id).unwrap_or(0);
                self.in_temps(|c| {
                    let o = c.emit_value(obj).reg();
                    let tmp = c.regs.scratch().reg();
                    let src = match &arith {
                        Some(kind) => {
                            let cur = c.regs.scratch().reg();
                            c.push(Instr::GetField {
                                dst: cur.0,
                                obj: o.0,
                                idx,
                            });
                            c.emit_into(value, tmp);
                            c.emit_arith_kind(kind, cur, tmp, cur);
                            cur
                        }
                        None => {
                            c.emit_into(value, tmp);
                            tmp
                        }
                    };
                    c.push(Instr::SetField {
                        obj: o.0,
                        idx,
                        src: src.0,
                    });
                });
            }
            ExprKind::Index { obj, idx } => {
                let index_kind = self.em.res.index(target.id).cloned();
                let is_map = matches!(index_kind, Some(IndexKind::Map));
                self.in_temps(|c| {
                    let o = c.emit_value(obj).reg();
                    let i = c.emit_value(idx).reg();
                    let tmp = c.regs.scratch().reg();
                    let src = match &arith {
                        Some(kind) => {
                            let cur = c.regs.scratch().reg();
                            if is_map {
                                c.push(Instr::MapIndexGet {
                                    dst: cur.0,
                                    map: o.0,
                                    key: i.0,
                                });
                            } else {
                                c.push(Instr::ListIndexGet {
                                    dst: cur.0,
                                    list: o.0,
                                    idx: i.0,
                                });
                            }
                            c.emit_into(value, tmp);
                            c.emit_arith_kind(kind, cur, tmp, cur);
                            cur
                        }
                        None => {
                            c.emit_into(value, tmp);
                            tmp
                        }
                    };
                    if is_map {
                        c.push(Instr::MapIndexSet {
                            map: o.0,
                            key: i.0,
                            src: src.0,
                        });
                    } else {
                        c.push(Instr::ListIndexSet {
                            list: o.0,
                            idx: i.0,
                            src: src.0,
                        });
                    }
                });
            }
            // Checker rejected; evaluate for effects.
            _ => self.emit_for_effect(value),
        }
    }

    /// Assign through a cell (a captured local, or a capture slot):
    /// read-modify-write for a compound assignment, plain write otherwise.
    fn emit_into_cell(&mut self, cell: Reg, value: &Expr, arith: Option<&BinOpKind>) {
        self.with_scratch(|c, cur| {
            match arith {
                Some(kind) => {
                    c.push(Instr::CellGet {
                        dst: cur.0,
                        cell: cell.0,
                    });
                    c.with_scratch(|c, tmp| {
                        c.emit_into(value, tmp);
                        c.emit_arith_kind(kind, cur, tmp, cur);
                    });
                }
                None => c.emit_into(value, cur),
            }
            c.push(Instr::CellSet {
                cell: cell.0,
                src: cur.0,
            });
        });
    }

    /// Emit an expression whose value is thrown away — the checker-error
    /// paths, which still evaluate their operand for its effects.
    fn emit_for_effect(&mut self, e: &Expr) {
        self.with_scratch(|c, tmp| c.emit_into(e, tmp));
    }

    /// Interpolated string: literal parts load constants, holes render
    /// through `Builtin::Str` (so custom Display impls — and their
    /// faults — behave exactly like `str(x)`), folded left-to-right with
    /// `ConcatStr`.
    fn emit_str_interp(&mut self, parts: &[crate::ast::InterpPart], dst: Reg) {
        use crate::ast::InterpPart;
        let mut started = false;
        for part in parts {
            // The first piece is built in `dst`; every later one is built
            // in a scratch and folded onto it.
            self.in_temps(|c| {
                let piece = if started { c.regs.scratch().reg() } else { dst };
                match part {
                    InterpPart::Lit(s) => {
                        let k = c.em.intern_const(Const::Str(s.as_str().into()));
                        let dst = piece.0;
                        c.push(Instr::LoadConst { dst, k });
                    }
                    InterpPart::Hole(h) => c.with_window(1, |c, w| {
                        c.emit_display_into(h, w.at(0));
                        c.push(Instr::Call {
                            dst: piece.0,
                            base: w.base().0,
                            nargs: w.len(),
                            target: CallTarget::Builtin(Builtin::Str),
                        });
                    }),
                }
                if started {
                    c.push(Instr::ConcatStr {
                        dst: dst.0,
                        a: dst.0,
                        b: piece.0,
                    });
                }
            });
            started = true;
        }
        if !started {
            let k = self.em.intern_const(Const::Str("".into()));
            self.push(Instr::LoadConst { dst: dst.0, k });
        }
    }

    /// Convert between a unit family and its backing number: one constant
    /// multiply (into base units) or divide (out of them). A factor of 1 —
    /// the base unit itself — is a plain move.
    fn emit_unit_conv(&mut self, operand: &Expr, conv: ConvKind, dst: Reg) {
        let (factor, into_base) = match conv {
            ConvKind::In { factor } => (factor, true),
            ConvKind::Out { factor } => (factor, false),
        };
        let src = self.emit_value(operand).reg();
        if factor.is_one() {
            self.push(Instr::Move {
                dst: dst.0,
                src: src.0,
            });
            return;
        }
        self.with_scratch(|c, k| {
            let (dst, a, b) = (dst.0, src.0, k.0);
            match factor {
                Factor::Int(n) => {
                    c.emit_int(n, k);
                    if into_base {
                        c.push(Instr::MulI { dst, a, b });
                    } else {
                        c.push(Instr::DivI { dst, a, b });
                    }
                }
                Factor::Float(f) => {
                    let konst = c.em.intern_const(Const::Float(f));
                    c.push(Instr::LoadConst { dst: b, k: konst });
                    if into_base {
                        c.push(Instr::MulF { dst, a, b });
                    } else {
                        c.push(Instr::DivF { dst, a, b });
                    }
                }
            }
        });
    }

    /// Emit `dst = a <op> b` for an arithmetic lowering the checker
    /// recorded (compound assignment read-modify-write).
    fn emit_arith_kind(&mut self, kind: &BinOpKind, a: Reg, b: Reg, dst: Reg) {
        use crate::ast::BinOp as B;
        let (a, b, dst) = (a.0, b.0, dst.0);
        match kind {
            BinOpKind::IntArith(op) => {
                let i = match op {
                    B::Add => Instr::AddI { dst, a, b },
                    B::Sub => Instr::SubI { dst, a, b },
                    B::Mul => Instr::MulI { dst, a, b },
                    B::Div => Instr::DivI { dst, a, b },
                    _ => Instr::RemI { dst, a, b },
                };
                self.push(i);
            }
            BinOpKind::FloatArith(op) => {
                let i = match op {
                    B::Add => Instr::AddF { dst, a, b },
                    B::Sub => Instr::SubF { dst, a, b },
                    B::Mul => Instr::MulF { dst, a, b },
                    B::Div => Instr::DivF { dst, a, b },
                    _ => Instr::RemF { dst, a, b },
                };
                self.push(i);
            }
            BinOpKind::Concat => {
                self.push(Instr::ConcatStr { dst, a, b });
            }
            BinOpKind::ArithCall { proto } => {
                let proto = *proto;
                self.with_window(2, |c, w| {
                    c.push(Instr::Move {
                        dst: w.at(0).0,
                        src: a,
                    });
                    c.push(Instr::Move {
                        dst: w.at(1).0,
                        src: b,
                    });
                    c.push(Instr::Call {
                        dst,
                        base: w.base().0,
                        nargs: w.len(),
                        target: CallTarget::Proto(proto),
                    });
                });
            }
            // Not an arithmetic lowering (checker error path).
            _ => {
                self.push(Instr::LoadUnit { dst });
            }
        }
    }

    fn emit_call(&mut self, e: &Expr, callee: &Expr, args: &[Expr], dst: Reg) {
        let kind = self.em.res.call(e.id).cloned();
        match kind {
            Some(CallKind::Proto(proto)) => {
                self.em.ensure_proto(proto);
                self.emit_args_call(args, dst, CallTarget::Proto(proto));
            }
            Some(CallKind::Host(idx)) => {
                self.emit_args_call(args, dst, CallTarget::Host(idx));
            }
            Some(CallKind::Prelude(p)) => {
                let builtin = match p {
                    PreludeFn::Print => Builtin::Print,
                    PreludeFn::Println => Builtin::Println,
                    PreludeFn::Str => Builtin::Str,
                    PreludeFn::Fmt => Builtin::Fmt,
                    PreludeFn::Same => Builtin::Same,
                    PreludeFn::Weak => Builtin::WeakNew,
                    PreludeFn::Int => Builtin::IntCast,
                    PreludeFn::Float => Builtin::FloatCast,
                };
                // The displaying prelude fns render unit args by family.
                if matches!(
                    p,
                    PreludeFn::Print | PreludeFn::Println | PreludeFn::Str | PreludeFn::Fmt
                ) {
                    self.emit_display_args_call(args, dst, CallTarget::Builtin(builtin));
                } else {
                    self.emit_args_call(args, dst, CallTarget::Builtin(builtin));
                }
            }
            // `Duration::ms(n)` — scale the one argument inline. Not a
            // call at all, so it is its own lowering rather than a
            // `CallKind` whose factor lived in a second table.
            None if self.em.res.unit_conv(e.id).is_some() => {
                match (self.em.res.unit_conv(e.id), args) {
                    (Some(conv), [arg]) => self.emit_unit_conv(arg, conv, dst),
                    _ => {
                        self.push(Instr::LoadUnit { dst: dst.0 });
                    }
                }
            }
            Some(CallKind::Variant { def, tag }) => {
                self.with_window(args.len() as u16, |c, w| {
                    c.emit_args_into(args, w);
                    c.push(Instr::NewEnum {
                        dst: dst.0,
                        def: def.0,
                        tag: tag as u16,
                        base: w.base().0,
                        n: w.len(),
                    });
                });
            }
            Some(CallKind::Value) => {
                // The callee is evaluated *before* the window opens: a
                // scratch taken after it would sit inside the argument run.
                let f = self.emit_value(callee).reg();
                self.with_window(args.len() as u16, |c, w| {
                    c.emit_args_into(args, w);
                    c.push(Instr::CallValue {
                        dst: dst.0,
                        f: f.0,
                        base: w.base().0,
                        nargs: w.len(),
                    });
                });
            }
            None => {
                self.push(Instr::LoadUnit { dst: dst.0 });
            }
        }
    }

    /// One argument per window slot, in source order.
    fn emit_args_into(&mut self, args: &[Expr], w: Window) {
        for (i, a) in args.iter().enumerate() {
            self.emit_into(a, w.at(i as u16));
        }
    }

    fn emit_args_call(&mut self, args: &[Expr], dst: Reg, target: CallTarget) {
        self.with_window(args.len() as u16, |c, w| {
            c.emit_args_into(args, w);
            c.push(Instr::Call {
                dst: dst.0,
                base: w.base().0,
                nargs: w.len(),
                target,
            });
        });
    }

    /// Like `emit_args_call`, but renders unit-family arguments to text
    /// first. Unit values are plain numbers at runtime, so the static type
    /// is the only place their family survives — this is where `println(d)`
    /// becomes `1.5s` instead of `1500000000`.
    fn emit_display_args_call(&mut self, args: &[Expr], dst: Reg, target: CallTarget) {
        self.with_window(args.len() as u16, |c, w| {
            for (i, a) in args.iter().enumerate() {
                c.emit_display_into(a, w.at(i as u16));
            }
            c.push(Instr::Call {
                dst: dst.0,
                base: w.base().0,
                nargs: w.len(),
                target,
            });
        });
    }

    /// Emit `e` into `dst`, replacing it with its rendered text when its
    /// static type is a unit family.
    fn emit_display_into(&mut self, e: &Expr, dst: Reg) {
        self.emit_into(e, dst);
        let Some(def) = self.unit_family_of(e) else {
            return;
        };
        // A user `impl Display` for the family wins, mirroring how the VM
        // treats structs and enums.
        if let Some(&proto) = self.em.res.impl_maps.display.get(&def.0) {
            self.em.ensure_proto(proto);
            self.with_window(1, |c, w| {
                c.push(Instr::Move {
                    dst: w.at(0).0,
                    src: dst.0,
                });
                c.push(Instr::Call {
                    dst: dst.0,
                    base: w.base().0,
                    nargs: w.len(),
                    target: CallTarget::Proto(proto),
                });
            });
            return;
        }
        self.with_window(2, |c, w| {
            c.push(Instr::Move {
                dst: w.at(0).0,
                src: dst.0,
            });
            c.emit_int(def.0 as i64, w.at(1));
            c.push(Instr::Call {
                dst: dst.0,
                base: w.base().0,
                nargs: w.len(),
                target: CallTarget::Builtin(Builtin::FmtQuantity),
            });
        });
    }

    /// The unit family of an expression's checked type, if it has one.
    fn unit_family_of(&self, e: &Expr) -> Option<defs::DefId> {
        match self.em.res.types.get(&e.id) {
            Some(wscript_core::types::Type::Named(id)) if self.em.res.defs.is_quantity(*id) => {
                Some(*id)
            }
            _ => None,
        }
    }

    fn emit_method_call(&mut self, e: &Expr, recv: &Expr, args: &[Expr], dst: Reg) {
        let Some(res) = self.em.res.method(e.id).cloned() else {
            self.em.ice(e.span, "method call");
            self.push(Instr::LoadUnit { dst: dst.0 });
            return;
        };
        if let MethodRes::Proto(proto) = res {
            self.em.ensure_proto(proto);
        }
        // The receiver is argument 0.
        self.with_window(args.len() as u16 + 1, |c, w| {
            c.emit_into(recv, w.at(0));
            for (i, a) in args.iter().enumerate() {
                c.emit_into(a, w.at(i as u16 + 1));
            }
            let (dst, base, nargs) = (dst.0, w.base().0, w.len());
            let target = match res {
                MethodRes::Proto(proto) => CallTarget::Proto(proto),
                MethodRes::Host(idx) => CallTarget::Host(idx),
                MethodRes::Builtin(b) => CallTarget::Builtin(b),
                MethodRes::Virtual { slot } => {
                    c.push(Instr::CallVirtual {
                        dst,
                        base,
                        nargs,
                        slot,
                    });
                    return;
                }
            };
            c.push(Instr::Call {
                dst,
                base,
                nargs,
                target,
            });
        });
    }

    fn emit_struct_lit(&mut self, e: &Expr, fields: &[(Ident, Expr)], dst: Reg) {
        let Some((res, order)) = self.em.res.struct_lit(e.id) else {
            self.em.ice(e.span, "struct literal");
            self.push(Instr::LoadUnit { dst: dst.0 });
            return;
        };
        let (res, order) = (res.clone(), order.to_vec());
        let n_fields = match &res {
            StructLitRes::Struct(def) => self
                .em
                .res
                .defs
                .as_struct(*def)
                .map(|s| s.fields.len())
                .unwrap_or(0),
            StructLitRes::Variant { def, tag } => self
                .em
                .res
                .defs
                .as_enum(*def)
                .and_then(|en| en.variants.get(*tag as usize))
                .map(|v| v.fields.len())
                .unwrap_or(0),
        };
        self.with_window(n_fields as u16, |c, w| {
            // Evaluate in source order, placing each value at its declared
            // slot.
            for (i, (_, value)) in fields.iter().enumerate() {
                match order.get(i) {
                    Some(&idx) if idx < w.len() => c.emit_into(value, w.at(idx)),
                    // The checker rejected this field; evaluate it anyway.
                    _ => c.emit_for_effect(value),
                }
            }
            let (dst, base, n) = (dst.0, w.base().0, w.len());
            match res {
                StructLitRes::Struct(def) => {
                    c.push(Instr::NewStruct {
                        dst,
                        def: def.0,
                        base,
                        n,
                    });
                }
                StructLitRes::Variant { def, tag } => {
                    c.push(Instr::NewEnum {
                        dst,
                        def: def.0,
                        tag: tag as u16,
                        base,
                        n,
                    });
                }
            }
        });
    }

    // ------------------------------------------------------------- match

    fn emit_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], dst: Reg) {
        let end = self.code.label();
        self.with_scratch(|c, v| {
            c.emit_into(scrutinee, v);
            for arm in arms {
                let next = c.code.label();
                c.in_temps(|c| {
                    c.emit_pattern(&arm.pat, v, next);
                    if let Some(guard) = &arm.guard {
                        let g = c.emit_value(guard);
                        c.code.jump_if_false(g.reg(), next);
                    }
                    c.emit_into(&arm.body, dst);
                });
                c.code.jump(end);
                c.code.bind(next);
            }
        });
        // The checker proved exhaustiveness; reaching here is a bug. It has
        // to be emitted before `end` is bound, or the last arm's jump lands
        // on the fault it is meant to skip.
        self.push(Instr::Fault {
            code: FaultCode::UnreachableMatch,
        });
        self.code.bind(end);
    }

    fn emit_int_pattern_test(&mut self, n: i64, reg: Reg, fail: Label) {
        self.in_temps(|c| {
            let lit = c.regs.scratch().reg();
            c.emit_int(n, lit);
            let cond = c.regs.scratch().reg();
            c.push(Instr::EqI {
                dst: cond.0,
                a: reg.0,
                b: lit.0,
            });
            c.code.jump_if_false(cond, fail);
        });
    }

    /// Test `reg` against constant `k` with `eq`, jumping to `fail` when
    /// they differ — `'x'` and `"lit"` patterns, which differ only in the
    /// comparison instruction.
    fn emit_const_pattern_test(
        &mut self,
        k: u32,
        reg: Reg,
        fail: Label,
        eq: impl FnOnce(u16, u16, u16) -> Instr,
    ) {
        self.in_temps(|c| {
            let lit = c.regs.scratch().reg();
            c.push(Instr::LoadConst { dst: lit.0, k });
            let cond = c.regs.scratch().reg();
            c.push(eq(cond.0, reg.0, lit.0));
            c.code.jump_if_false(cond, fail);
        });
    }

    /// Emit tests for `pat` against the value in `reg`, jumping to `fail`
    /// if any of them does not hold — the caller binds that label at
    /// whatever comes next (the following arm, the `else` block). Emits
    /// binding stores along the way.
    fn emit_pattern(&mut self, pat: &Pattern, reg: Reg, fail: Label) {
        self.code.set_span(pat.span);
        match &pat.kind {
            PatternKind::Wildcard | PatternKind::Error => {}
            PatternKind::Binding(_) => {
                // A binding names either a unit variant (a tag test) or a
                // new local (a slot in `decl_locals`) — the checker
                // records exactly one of the two, never neither.
                if let Some((_, tag, _)) = self.em.res.pat_variant(pat.id) {
                    self.emit_tag_test(reg, tag, fail);
                } else if let Some(&local) = self.em.res.decl_locals.get(&pat.id) {
                    let dst = self.local_reg(local).0;
                    if self.captured.contains(&local) {
                        self.push(Instr::NewCell { dst, src: reg.0 });
                    } else {
                        self.push(Instr::Move { dst, src: reg.0 });
                    }
                } else {
                    self.em.ice(pat.span, "binding pattern");
                }
            }
            PatternKind::IntLit(n) => {
                self.emit_int_pattern_test(*n, reg, fail);
            }
            // Folded to a base-unit constant by the checker; float-backed
            // families never reach here (they cannot be matched on).
            PatternKind::QuantityLit { .. } => match self.em.res.pat_quantity_lit(pat.id) {
                Some(Factor::Int(n)) => self.emit_int_pattern_test(n, reg, fail),
                _ => self.em.ice(pat.span, "unit literal pattern"),
            },
            PatternKind::BoolLit(b) => {
                if *b {
                    self.code.jump_if_false(reg, fail);
                } else {
                    self.code.jump_if_true(reg, fail);
                }
            }
            PatternKind::CharLit(c) => {
                let k = self.em.intern_const(Const::Char(*c));
                self.emit_const_pattern_test(k, reg, fail, |dst, a, b| Instr::EqChar { dst, a, b });
            }
            PatternKind::StrLit(s) => {
                let k = self.em.intern_const(Const::Str(s.as_str().into()));
                self.emit_const_pattern_test(k, reg, fail, |dst, a, b| Instr::EqStr { dst, a, b });
            }
            PatternKind::Variant { args, .. } => {
                // Tag and field order come out of one lookup: the checker
                // wrote them as one value. (`res` is copied out so `order`
                // outlives the `&mut self` calls below.)
                let res = self.em.res;
                let Some((_, tag, order)) = res.pat_variant(pat.id) else {
                    self.em.ice(pat.span, "variant pattern");
                    return;
                };
                self.emit_tag_test(reg, tag, fail);
                match args {
                    VariantPatArgs::Unit => {}
                    VariantPatArgs::Tuple(pats) => {
                        for (i, p) in pats.iter().enumerate() {
                            // A wildcard binds nothing, so its field is
                            // never read.
                            if matches!(p.kind, PatternKind::Wildcard) {
                                continue;
                            }
                            self.emit_field_pattern(p, reg, i as u16, fail);
                        }
                    }
                    VariantPatArgs::Struct { fields, .. } => {
                        self.emit_named_field_patterns(fields, order, reg, fail)
                    }
                }
            }
            PatternKind::Struct { fields, .. } => {
                let res = self.em.res;
                let Some((_, order)) = res.pat_struct(pat.id) else {
                    self.em.ice(pat.span, "struct pattern");
                    return;
                };
                self.emit_named_field_patterns(fields, order, reg, fail)
            }
            PatternKind::Or(alts) => {
                // Succeed if any alternative matches (no bindings inside,
                // enforced by the checker).
                let matched = self.code.label();
                let n = alts.len();
                for (i, alt) in alts.iter().enumerate() {
                    if i + 1 == n {
                        // The last alternative failing fails the pattern.
                        self.emit_pattern(alt, reg, fail);
                    } else {
                        let next = self.code.label();
                        self.emit_pattern(alt, reg, next);
                        self.code.jump(matched);
                        self.code.bind(next);
                    }
                }
                self.code.bind(matched);
            }
        }
    }

    /// Test each named field's sub-pattern against the field it names, for
    /// struct patterns and struct-variant patterns alike. `order[i]` is
    /// the runtime index of the `i`th field as written; `u16::MAX` marks a
    /// field the checker could not resolve.
    fn emit_named_field_patterns(
        &mut self,
        fields: &[(Ident, Pattern)],
        order: &[u16],
        reg: Reg,
        fail: Label,
    ) {
        for (i, (_, p)) in fields.iter().enumerate() {
            let Some(&idx) = order.get(i) else { continue };
            // A wildcard binds nothing, so neither field is read.
            if idx == u16::MAX || matches!(p.kind, PatternKind::Wildcard) {
                continue;
            }
            self.emit_field_pattern(p, reg, idx, fail);
        }
    }

    /// Read field `idx` out of `reg` and match `p` against it.
    fn emit_field_pattern(&mut self, p: &Pattern, reg: Reg, idx: u16, fail: Label) {
        // The field register outlives this scope's tests only as far as the
        // sub-pattern, which either binds it into a local or is done with
        // it.
        self.with_scratch(|c, field| {
            c.push(Instr::GetField {
                dst: field.0,
                obj: reg.0,
                idx,
            });
            c.emit_pattern(p, field, fail);
        });
    }

    fn emit_tag_test(&mut self, reg: Reg, tag: u32, fail: Label) {
        self.in_temps(|c| {
            let t = c.regs.scratch().reg();
            c.push(Instr::GetTag {
                dst: t.0,
                obj: reg.0,
            });
            let lit = c.regs.scratch().reg();
            c.emit_int(tag as i64, lit);
            let cond = c.regs.scratch().reg();
            c.push(Instr::EqI {
                dst: cond.0,
                a: t.0,
                b: lit.0,
            });
            c.code.jump_if_false(cond, fail);
        });
    }

    // --------------------------------------------------------------- for

    fn emit_for(&mut self, e: &Expr, iter: &Expr, body: &Block, dst: Reg) {
        let kind = match self.em.res.for_kind(e.id) {
            Some(kind) => *kind,
            None => {
                self.em.ice(e.span, "`for` loop");
                ForKind::List
            }
        };
        let var_local = *self
            .em
            .res
            .decl_locals
            .get(&e.id)
            .expect("for var resolved");
        let var_reg = self.local_reg(var_local);
        let var_captured = self.captured.contains(&var_local);

        // The loop's state — cursor, limit, the list being walked — lives
        // across the body, so it is allocated in one scope around the whole
        // lowering rather than by the pieces that write it.
        self.in_temps(|c| match kind {
            ForKind::RangeExclusive | ForKind::RangeInclusive => {
                let ExprKind::Range { lo, hi, .. } = &iter.kind else {
                    unreachable!("range for-kind without range iter")
                };
                let cursor = c.regs.scratch().reg();
                c.emit_into(lo, cursor);
                let limit = c.regs.scratch().reg();
                c.emit_into(hi, limit);
                let one = c.regs.scratch().reg();
                c.push(Instr::LoadInt { dst: one.0, v: 1 });

                let frame = c.enter_loop();
                let test = c.code.label_here();
                c.with_scratch(|c, cond| {
                    let (dst, a, b) = (cond.0, cursor.0, limit.0);
                    if matches!(kind, ForKind::RangeInclusive) {
                        c.push(Instr::LeI { dst, a, b });
                    } else {
                        c.push(Instr::LtI { dst, a, b });
                    }
                    c.code.jump_if_false(cond, frame.brk);
                });
                c.bind_loop_var(var_reg, var_captured, cursor);
                c.emit_block(body, None);
                // `continue` skips to the step, not to the test.
                c.code.bind(frame.cont);
                c.push(Instr::AddI {
                    dst: cursor.0,
                    a: cursor.0,
                    b: one.0,
                });
                c.code.jump(test);
                c.exit_loop();
            }
            ForKind::List | ForKind::MapKeys | ForKind::StrChars => {
                // Materialize the iterable (keys()/chars() create a list).
                let list = c.regs.scratch().reg();
                match kind {
                    ForKind::List => c.emit_into(iter, list),
                    ForKind::MapKeys => c.emit_builtin_of(iter, list, Builtin::MapKeys),
                    _ => c.emit_builtin_of(iter, list, Builtin::StrChars),
                }
                let idx = c.regs.scratch().reg();
                c.push(Instr::LoadInt { dst: idx.0, v: 0 });
                let one = c.regs.scratch().reg();
                c.push(Instr::LoadInt { dst: one.0, v: 1 });

                let frame = c.enter_loop();
                let test = c.code.label_here();
                c.in_temps(|c| {
                    // Re-check the length each iteration: mutation during
                    // iteration shrinks/extends the walk instead of
                    // faulting.
                    let len = c.regs.scratch().reg();
                    c.with_window(1, |c, w| {
                        c.push(Instr::Move {
                            dst: w.at(0).0,
                            src: list.0,
                        });
                        c.push(Instr::Call {
                            dst: len.0,
                            base: w.base().0,
                            nargs: w.len(),
                            target: CallTarget::Builtin(Builtin::ListLen),
                        });
                    });
                    let cond = c.regs.scratch().reg();
                    c.push(Instr::LtI {
                        dst: cond.0,
                        a: idx.0,
                        b: len.0,
                    });
                    c.code.jump_if_false(cond, frame.brk);
                });
                // An uncaptured loop variable is read straight into its
                // register; a captured one has to land somewhere first,
                // because the cell is built *from* the element.
                if var_captured {
                    c.with_scratch(|c, elem| {
                        c.push(Instr::ListIndexGet {
                            dst: elem.0,
                            list: list.0,
                            idx: idx.0,
                        });
                        c.bind_loop_var(var_reg, var_captured, elem);
                    });
                } else {
                    c.push(Instr::ListIndexGet {
                        dst: var_reg.0,
                        list: list.0,
                        idx: idx.0,
                    });
                }
                c.emit_block(body, None);
                c.code.bind(frame.cont);
                c.push(Instr::AddI {
                    dst: idx.0,
                    a: idx.0,
                    b: one.0,
                });
                c.code.jump(test);
                c.exit_loop();
            }
        });
        self.push(Instr::LoadUnit { dst: dst.0 });
    }

    /// `var = cursor` at the top of a `for` body, boxed when the loop
    /// variable is captured by a closure.
    fn bind_loop_var(&mut self, var_reg: Reg, captured: bool, src: Reg) {
        if captured {
            self.push(Instr::NewCell {
                dst: var_reg.0,
                src: src.0,
            });
        } else {
            self.push(Instr::Move {
                dst: var_reg.0,
                src: src.0,
            });
        }
    }

    /// `dst = builtin(iter)` — the one-argument builtin call that turns a
    /// map or a string into the list a `for` walks.
    fn emit_builtin_of(&mut self, iter: &Expr, dst: Reg, builtin: Builtin) {
        let src = self.emit_value(iter).reg();
        self.with_window(1, |c, w| {
            c.push(Instr::Move {
                dst: w.at(0).0,
                src: src.0,
            });
            c.push(Instr::Call {
                dst: dst.0,
                base: w.base().0,
                nargs: w.len(),
                target: CallTarget::Builtin(builtin),
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wscript_core::registry::Registry;

    /// Compile `src` and hand back the emitted unit.
    fn emit(src: &str) -> wscript_core::bytecode::CompiledUnit {
        let reg = Registry::new();
        let parsed = crate::parse(src);
        let refs: Vec<(String, &SourceFile)> = vec![(String::new(), &parsed.file)];
        let checked = crate::check::check_files(&refs, &reg);
        assert!(
            checked.diags.is_empty(),
            "fixture must compile cleanly: {:?}",
            checked.diags
        );
        let (unit, ices) = emit_files(&[&parsed.file], &checked);
        assert!(ices.is_empty(), "fixture must emit cleanly: {ices:?}");
        unit
    }

    /// The frame size `main` was emitted with.
    fn main_frame(src: &str) -> u16 {
        let unit = emit(src);
        let (proto, _) = unit.exports["main"];
        unit.protos[proto as usize].n_regs
    }

    /// Temps are released, not merely grown — and the release that matters
    /// is *inside* one expression, which is where the frame used only ever
    /// to rise. Each argument's own temps die when the argument lands in
    /// its window slot, so widening a call from two arguments to four costs
    /// two registers (the window) rather than two per argument.
    #[test]
    fn an_arguments_temps_die_with_the_argument() {
        const FNS: &str = "fn f(a: int, b: int) -> int { a + b }\n\
                           fn g2(a: int, b: int) -> int { a + b }\n\
                           fn g4(a: int, b: int, c: int, d: int) -> int { a + b + c + d }\n";
        let two = main_frame(&format!("{FNS}fn main() {{ g2(f(1, 2), f(3, 4)); }}"));
        let four = main_frame(&format!(
            "{FNS}fn main() {{ g4(f(1, 2), f(3, 4), f(5, 6), f(7, 8)); }}"
        ));
        assert_eq!(
            four - two,
            2,
            "only the argument window should grow ({two} -> {four})"
        );
    }

    /// The frame does still cover the deepest expression — otherwise the
    /// test above would pass on an allocator that handed out one register.
    #[test]
    fn a_deeper_expression_needs_a_bigger_frame() {
        let shallow = main_frame("fn f(a: int, b: int) -> int { a + b }\nfn main() { f(1, 2); }");
        let deep = main_frame(
            "fn f(a: int, b: int) -> int { a + b }\n\
             fn main() { f(f(f(1, 2), f(3, 4)), f(f(5, 6), f(7, 8))); }",
        );
        assert!(
            deep > shallow,
            "a nested call needs more registers than a flat one ({deep} vs {shallow})"
        );
    }

    /// Every proto the emitter produces has to satisfy the contract the VM
    /// trusts: no register operand past `n_regs`, no jump out of the body.
    /// The whole script corpus is held to this in
    /// `wscript-cli/tests/verify_corpus.rs`; this is the unit-level canary
    /// for the shapes that allocate and branch the most.
    #[test]
    fn emitted_bytecode_verifies() {
        let unit = emit(
            "enum Shape { Circle(int), Rect { w: int, h: int } }\n\
             fn area(s: Shape) -> int {\n\
                 match s {\n\
                     Shape::Circle(r) if r > 0 => r * r,\n\
                     Shape::Circle(_) => 0,\n\
                     Shape::Rect { w, h } => w * h,\n\
                 }\n\
             }\n\
             fn main() -> int {\n\
                 let total = 0\n\
                 for i in 0..5 {\n\
                     if i == 2 { continue }\n\
                     total += area(Shape::Rect { w: i, h: i })\n\
                 }\n\
                 while total > 100 { total -= 1 }\n\
                 let add = |x: int| x + total\n\
                 add(total)\n\
             }",
        );
        if let Err(report) = wscript_core::verify::verify_report(&unit) {
            panic!("emitted bytecode does not verify:\n{report}");
        }
    }

    /// A lowering the checker should have recorded but did not is a
    /// compiler bug, and must surface as a diagnostic rather than as a
    /// plausible instruction. The previous side tables lowered `LoadUnit`
    /// in this case and produced a silently wrong program.
    #[test]
    fn a_dropped_lowering_reports_an_internal_error() {
        let reg = Registry::new();
        let parsed = crate::parse("fn main() -> int { 1 + 2 }");
        let refs: Vec<(String, &SourceFile)> = vec![(String::new(), &parsed.file)];
        let mut checked = crate::check::check_files(&refs, &reg);
        assert!(checked.diags.is_empty(), "fixture must compile cleanly");

        // Emitting the intact result is silent.
        let (_, clean) = emit_files(&[&parsed.file], &checked);
        assert!(clean.is_empty(), "a well-formed unit reports nothing");

        assert!(checked.drop_a_bin_op(), "fixture should have a binary op");
        let (_, ices) = emit_files(&[&parsed.file], &checked);
        assert_eq!(ices.len(), 1, "one internal error");
        assert_eq!(ices[0].code, "E9999");
        assert!(
            ices[0].message.contains("binary operator"),
            "message should name what was missing: {}",
            ices[0].message
        );
        // No script can reach an ICE, so no fixture can show that this one
        // explains itself. Here is the only place that can.
        assert!(
            ices[0].help_text().is_some_and(|h| h.contains("report")),
            "an ICE must tell the reader it is the compiler's fault and ask \
             to be reported"
        );
    }

    /// The pattern space has the same contract: a pattern the checker
    /// resolved but did not record must not silently emit a match that
    /// skips its tag test — that would run the wrong arm.
    #[test]
    fn a_dropped_pattern_lowering_reports_an_internal_error() {
        let reg = Registry::new();
        // One variant pattern only, so `drop_a_pat_variant` has no choice
        // about which node it drops (`None` would be a binding pattern).
        let parsed = crate::parse("fn main() -> int { match Some(1) { Some(n) => n, _ => 0 } }");
        let refs: Vec<(String, &SourceFile)> = vec![(String::new(), &parsed.file)];
        let mut checked = crate::check::check_files(&refs, &reg);
        assert!(checked.diags.is_empty(), "fixture must compile cleanly");

        let (_, clean) = emit_files(&[&parsed.file], &checked);
        assert!(clean.is_empty(), "a well-formed unit reports nothing");

        assert!(
            checked.drop_a_pat_variant(),
            "fixture should have a variant pattern"
        );
        let (_, ices) = emit_files(&[&parsed.file], &checked);
        assert_eq!(ices.len(), 1, "one internal error");
        assert_eq!(ices[0].code, "E9999");
        assert!(
            ices[0].message.contains("variant pattern"),
            "message should name what was missing: {}",
            ices[0].message
        );
    }
}
