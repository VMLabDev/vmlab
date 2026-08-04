//! Bytecode emitter: walks the checked AST and produces a `CompiledUnit`.
//!
//! Register conventions per frame:
//! `[0 .. n_locals)` — locals in checker `LocalId` order (params first);
//! `[n_locals .. n_locals + n_captures)` — capture cells loaded in the
//! prologue (closures only); temps grow above that, stack-style.
//!
//! Locals captured by closures hold a `Cell` value instead of the value
//! itself; reads/writes go through `CellGet`/`CellSet`.

use std::collections::HashMap;

use wscript_core::bytecode::{
    Builtin, CallTarget, CaptureSrc, CompiledUnit, Const, FaultCode, FnProto, Instr, VTable,
};
use wscript_core::defs::{self, Factor};
use wscript_core::span::Span;

use crate::ast::*;
use crate::check::{
    BinOpKind, CallKind, CapSrc, CheckResult, ConvKind, FnSource, ForKind, IndexKind, LocalId,
    MethodRes, PathRes, PreludeFn, PrimKind, StructLitRes, TryKind, UnOpKind, VarRes,
};

pub fn emit(file: &SourceFile, res: &CheckResult) -> CompiledUnit {
    emit_files(&[file], res)
}

/// Emit a whole (possibly multi-file) program into one merged unit.
/// `files` must be in the same order the checker saw them.
pub fn emit_files(files: &[&SourceFile], res: &CheckResult) -> CompiledUnit {
    let mut em = Emitter {
        files,
        res,
        consts: Vec::new(),
        const_map: HashMap::new(),
        protos: (0..res.fn_infos.len()).map(|_| None).collect(),
    };
    for proto in 0..res.fn_infos.len() {
        em.ensure_proto(proto as u32);
    }
    static NEXT_UNIT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    CompiledUnit {
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
    }
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
                let Some((params_len, body)) =
                    self.files.iter().find_map(|f| find_closure(f, node))
                else {
                    unreachable!("closure node not found")
                };
                let _ = params_len;
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
            code: Vec::new(),
            spans: Vec::new(),
            cur_span: span,

            cap_base: n_locals as u16,
            temp_top: n_locals as u16 + n_caps,
            max_regs: n_locals as u16 + n_caps,
            captured: info.captured.clone(),
            loops: Vec::new(),
        };
        // Prologue: load capture cells, box captured params.
        for slot in 0..n_caps {
            let dst = f.cap_base + slot;
            f.push(Instr::LoadCapture { dst, slot });
        }
        let n_params = info.sig.params.len() as u16;
        for p in 0..n_params {
            if f.captured.contains(&(p as u32)) {
                f.push(Instr::NewCell { dst: p, src: p });
            }
        }
        match body {
            FnBody::Block(b) => {
                let dst = f.alloc_temp();
                f.emit_block(b, Some(dst));
                f.push(Instr::Ret { src: dst });
            }
            FnBody::Expr(e) => {
                let dst = f.alloc_temp();
                f.emit_into(e, dst);
                f.push(Instr::Ret { src: dst });
            }
        }
        let max_regs = f.max_regs;
        let code = std::mem::take(&mut f.code);
        let spans = std::mem::take(&mut f.spans);
        drop(f);
        FnProto {
            name: info.name.clone(),
            n_params,
            n_regs: max_regs.max(1),
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
fn find_closure(file: &SourceFile, node: NodeId) -> Option<(usize, &Expr)> {
    struct Finder {
        node: NodeId,
    }
    impl Finder {
        fn in_expr<'a>(&self, e: &'a Expr) -> Option<&'a Expr> {
            if let ExprKind::Closure { body, .. } = &e.kind
                && e.id == self.node
            {
                return Some(body);
            }
            match &e.kind {
                ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => self.in_expr(expr),
                ExprKind::StrInterp(parts) => parts.iter().find_map(|p| match p {
                    crate::ast::InterpPart::Hole(h) => self.in_expr(h),
                    crate::ast::InterpPart::Lit(_) => None,
                }),
                ExprKind::Binary { lhs, rhs, .. } => {
                    self.in_expr(lhs).or_else(|| self.in_expr(rhs))
                }
                ExprKind::Assign { target, value, .. } => {
                    self.in_expr(target).or_else(|| self.in_expr(value))
                }
                ExprKind::Call { callee, args } => self
                    .in_expr(callee)
                    .or_else(|| args.iter().find_map(|a| self.in_expr(a))),
                ExprKind::MethodCall { recv, args, .. } => self
                    .in_expr(recv)
                    .or_else(|| args.iter().find_map(|a| self.in_expr(a))),
                ExprKind::Field { obj, .. } => self.in_expr(obj),
                ExprKind::Index { obj, idx } => self.in_expr(obj).or_else(|| self.in_expr(idx)),
                ExprKind::StructLit { fields, .. } => {
                    fields.iter().find_map(|(_, v)| self.in_expr(v))
                }
                ExprKind::ListLit(items) => items.iter().find_map(|i| self.in_expr(i)),
                ExprKind::MapLit(entries) => entries
                    .iter()
                    .find_map(|(k, v)| self.in_expr(k).or_else(|| self.in_expr(v))),
                ExprKind::If { cond, then, else_ } => self
                    .in_expr(cond)
                    .or_else(|| self.in_block(then))
                    .or_else(|| else_.as_ref().and_then(|e| self.in_expr(e))),
                ExprKind::IfLet {
                    scrutinee,
                    then,
                    else_,
                    ..
                } => self
                    .in_expr(scrutinee)
                    .or_else(|| self.in_block(then))
                    .or_else(|| else_.as_ref().and_then(|e| self.in_expr(e))),
                ExprKind::Match { scrutinee, arms } => self.in_expr(scrutinee).or_else(|| {
                    arms.iter().find_map(|a| {
                        a.guard
                            .as_ref()
                            .and_then(|g| self.in_expr(g))
                            .or_else(|| self.in_expr(&a.body))
                    })
                }),
                ExprKind::While { cond, body } => {
                    self.in_expr(cond).or_else(|| self.in_block(body))
                }
                ExprKind::Loop { body } => self.in_block(body),
                ExprKind::For { iter, body, .. } => {
                    self.in_expr(iter).or_else(|| self.in_block(body))
                }
                ExprKind::Range { lo, hi, .. } => self.in_expr(lo).or_else(|| self.in_expr(hi)),
                ExprKind::Return(Some(v)) => self.in_expr(v),
                ExprKind::Block(b) => self.in_block(b),
                ExprKind::Closure { body, .. } => self.in_expr(body),
                _ => None,
            }
        }
        fn in_block<'a>(&self, b: &'a Block) -> Option<&'a Expr> {
            b.stmts.iter().find_map(|s| match s {
                Stmt::Let { init, .. } => self.in_expr(init),
                Stmt::LetElse {
                    init, else_block, ..
                } => self.in_expr(init).or_else(|| self.in_block(else_block)),
                Stmt::Expr { expr, .. } => self.in_expr(expr),
            })
        }
    }
    let finder = Finder { node };
    for item in &file.items {
        let found = match item {
            Item::Fn(f) => finder.in_block(&f.body),
            Item::Impl(im) => im.fns.iter().find_map(|f| finder.in_block(&f.body)),
            _ => None,
        };
        if let Some(body) = found {
            return Some((0, body));
        }
    }
    None
}

struct LoopFrame {
    /// Jump indices to patch to the loop's continue target.
    continues: Vec<usize>,
    /// Jump indices to patch to the end of the loop.
    breaks: Vec<usize>,
    /// `Some(pc)` once the continue target is known at frame creation
    /// (while/loop jump back to the start; for-loops patch later).
    continue_pc: Option<usize>,
}

struct FnEmitter<'e, 'a> {
    em: &'e mut Emitter<'a>,
    code: Vec<Instr>,
    spans: Vec<Span>,
    cur_span: Span,
    cap_base: u16,
    temp_top: u16,
    max_regs: u16,
    captured: std::collections::HashSet<LocalId>,
    loops: Vec<LoopFrame>,
}

impl<'e, 'a> FnEmitter<'e, 'a> {
    // ----------------------------------------------------------- helpers

    fn push(&mut self, i: Instr) -> usize {
        self.code.push(i);
        self.spans.push(self.cur_span);
        self.code.len() - 1
    }

    fn alloc_temp(&mut self) -> u16 {
        let r = self.temp_top;
        self.temp_top += 1;
        self.max_regs = self.max_regs.max(self.temp_top);
        r
    }

    fn alloc_window(&mut self, n: u16) -> u16 {
        let base = self.temp_top;
        self.temp_top += n;
        self.max_regs = self.max_regs.max(self.temp_top);
        base
    }

    fn temps_mark(&self) -> u16 {
        self.temp_top
    }

    fn temps_reset(&mut self, mark: u16) {
        self.temp_top = mark;
    }

    fn local_reg(&self, local: LocalId) -> u16 {
        local as u16
    }

    fn cap_reg(&self, slot: u16) -> u16 {
        self.cap_base + slot
    }

    /// Emit a placeholder jump; returns its index for patching.
    fn jump_placeholder(&mut self) -> usize {
        self.push(Instr::Jump { off: 0 })
    }

    fn jump_if_false_placeholder(&mut self, cond: u16) -> usize {
        self.push(Instr::JumpIfFalse { cond, off: 0 })
    }

    fn jump_if_true_placeholder(&mut self, cond: u16) -> usize {
        self.push(Instr::JumpIfTrue { cond, off: 0 })
    }

    fn patch_to_here(&mut self, idx: usize) {
        let target = self.code.len() as i64;
        let off = (target - (idx as i64 + 1)) as i32;
        match &mut self.code[idx] {
            Instr::Jump { off: o }
            | Instr::JumpIfFalse { off: o, .. }
            | Instr::JumpIfTrue { off: o, .. } => *o = off,
            _ => unreachable!("patching a non-jump"),
        }
    }

    fn jump_back_to(&mut self, target: usize) {
        let idx = self.code.len() as i64;
        let off = (target as i64 - (idx + 1)) as i32;
        self.push(Instr::Jump { off });
    }

    // ------------------------------------------------------------ blocks

    /// Emit a block; the tail expression value (if any) lands in `dst`.
    /// If the block has no value, `dst` (when given) is set to unit.
    fn emit_block(&mut self, block: &Block, dst: Option<u16>) {
        let outer_mark = self.temps_mark();
        let n = block.stmts.len();
        let mut produced = false;
        for (i, stmt) in block.stmts.iter().enumerate() {
            let last = i + 1 == n;
            let mark = self.temps_mark();
            match stmt {
                Stmt::Let { init, id, span, .. } => {
                    self.cur_span = *span;
                    let local = *self.em.res.decl_locals.get(id).expect("let stmt resolved");
                    self.emit_init_local(local, init);
                }
                Stmt::LetElse {
                    pat,
                    init,
                    else_block,
                    span,
                    ..
                } => {
                    self.cur_span = *span;
                    let v = self.alloc_temp();
                    self.emit_into(init, v);
                    let mut fails = Vec::new();
                    self.emit_pattern(pat, v, &mut fails);
                    let done = self.jump_placeholder();
                    for f in fails {
                        self.patch_to_here(f);
                    }
                    // Else block diverges; emit and (defensively) fault.
                    let scratch = self.alloc_temp();
                    self.emit_block(else_block, Some(scratch));
                    self.push(Instr::Fault {
                        code: FaultCode::UnreachableMatch,
                    });
                    self.patch_to_here(done);
                }
                Stmt::Expr { expr, terminated } => {
                    if last && !*terminated {
                        match dst {
                            Some(d) => self.emit_into(expr, d),
                            None => {
                                let scratch = self.alloc_temp();
                                self.emit_into(expr, scratch);
                            }
                        }
                        produced = true;
                    } else {
                        let scratch = self.alloc_temp();
                        self.emit_into(expr, scratch);
                    }
                }
            }
            self.temps_reset(mark);
        }
        if !produced && let Some(d) = dst {
            self.push(Instr::LoadUnit { dst: d });
        }
        self.temps_reset(outer_mark);
    }

    /// `let local = init` — store into the local's register, boxing into a
    /// cell when the local is captured.
    fn emit_init_local(&mut self, local: LocalId, init: &Expr) {
        let reg = self.local_reg(local);
        if self.captured.contains(&local) {
            let tmp = self.alloc_temp();
            self.emit_into(init, tmp);
            self.push(Instr::NewCell { dst: reg, src: tmp });
        } else {
            self.emit_into(init, reg);
        }
    }

    // ------------------------------------------------------- expressions

    /// Emit `e`, returning a register holding its value. Reads of plain
    /// locals return the local's register directly (no copy).
    fn emit_value(&mut self, e: &Expr) -> u16 {
        if !self.em.res.dyn_wraps.contains_key(&e.id)
            && let ExprKind::Path(_) = &e.kind
            && let Some(VarRes::Local(l)) = self.em.res.var_refs.get(&e.id)
            && !self.captured.contains(l)
        {
            return self.local_reg(*l);
        }
        let dst = self.alloc_temp();
        self.emit_into(e, dst);
        dst
    }

    fn emit_into(&mut self, e: &Expr, dst: u16) {
        let saved_span = self.cur_span;
        self.cur_span = e.span;
        self.emit_into_inner(e, dst);
        if let Some(&vt) = self.em.res.dyn_wraps.get(&e.id) {
            self.push(Instr::MakeDyn { dst, src: dst, vt });
        }
        self.cur_span = saved_span;
    }

    fn emit_into_inner(&mut self, e: &Expr, dst: u16) {
        match &e.kind {
            ExprKind::IntLit(n) => self.emit_int(*n, dst),
            // Already folded into base units by the checker.
            ExprKind::QuantityLit { .. } => match self.em.res.quantity_lits.get(&e.id) {
                Some(Factor::Int(n)) => self.emit_int(*n, dst),
                Some(Factor::Float(f)) => {
                    let k = self.em.intern_const(Const::Float(*f));
                    self.push(Instr::LoadConst { dst, k });
                }
                None => {
                    self.push(Instr::LoadUnit { dst });
                }
            },
            ExprKind::FloatLit(x) => {
                let k = self.em.intern_const(Const::Float(*x));
                self.push(Instr::LoadConst { dst, k });
            }
            ExprKind::BoolLit(b) => {
                self.push(Instr::LoadBool { dst, v: *b });
            }
            ExprKind::CharLit(c) => {
                let k = self.em.intern_const(Const::Char(*c));
                self.push(Instr::LoadConst { dst, k });
            }
            ExprKind::StrLit(s) => {
                let k = self.em.intern_const(Const::Str(s.as_str().into()));
                self.push(Instr::LoadConst { dst, k });
            }
            ExprKind::StrInterp(parts) => self.emit_str_interp(parts, dst),
            ExprKind::UnitLit => {
                self.push(Instr::LoadUnit { dst });
            }
            ExprKind::Error => {
                self.push(Instr::LoadUnit { dst });
            }
            ExprKind::Path(_) => self.emit_path(e, dst),
            ExprKind::Unary { expr, .. } => self.emit_unary(e, expr, dst),
            ExprKind::Binary { lhs, rhs, .. } => self.emit_binary(e, lhs, rhs, dst),
            ExprKind::Assign { target, value, op } => {
                self.emit_assign(e, target, value, op.is_some());
                self.push(Instr::LoadUnit { dst });
            }
            ExprKind::Call { callee, args } => self.emit_call(e, callee, args, dst),
            ExprKind::MethodCall { recv, args, .. } => self.emit_method_call(e, recv, args, dst),
            ExprKind::Field { obj, .. } => {
                // `d.ms` on a unit value is a constant divide, not a field
                // read — unit values are plain numbers at runtime.
                if let Some(&conv) = self.em.res.unit_convs.get(&e.id) {
                    self.emit_unit_conv(obj, conv, dst);
                    return;
                }
                let o = self.emit_value(obj);
                let idx = *self.em.res.fields.get(&e.id).unwrap_or(&0);
                self.push(Instr::GetField { dst, obj: o, idx });
            }
            ExprKind::Index { obj, idx } => {
                let kind = self.em.res.indexes.get(&e.id).cloned();
                match kind {
                    Some(IndexKind::List) => {
                        let o = self.emit_value(obj);
                        let i = self.emit_value(idx);
                        self.push(Instr::ListIndexGet {
                            dst,
                            list: o,
                            idx: i,
                        });
                    }
                    Some(IndexKind::Map) => {
                        let o = self.emit_value(obj);
                        let i = self.emit_value(idx);
                        self.push(Instr::MapIndexGet {
                            dst,
                            map: o,
                            key: i,
                        });
                    }
                    Some(IndexKind::UserGet { proto }) => {
                        let base = self.alloc_window(2);
                        self.emit_into(obj, base);
                        self.emit_into(idx, base + 1);
                        self.push(Instr::Call {
                            dst,
                            base,
                            nargs: 2,
                            target: CallTarget::Proto(proto),
                        });
                    }
                    None => {
                        self.push(Instr::LoadUnit { dst });
                    }
                }
            }
            ExprKind::StructLit { fields, .. } => self.emit_struct_lit(e, fields, dst),
            ExprKind::ListLit(items) => {
                let base = self.alloc_window(items.len() as u16);
                for (i, item) in items.iter().enumerate() {
                    self.emit_into(item, base + i as u16);
                }
                self.push(Instr::NewList {
                    dst,
                    base,
                    n: items.len() as u16,
                });
            }
            ExprKind::MapLit(entries) => {
                let base = self.alloc_window(entries.len() as u16 * 2);
                for (i, (k, v)) in entries.iter().enumerate() {
                    self.emit_into(k, base + (i * 2) as u16);
                    self.emit_into(v, base + (i * 2) as u16 + 1);
                }
                self.push(Instr::NewMap {
                    dst,
                    base,
                    n: entries.len() as u16,
                });
            }
            ExprKind::If { cond, then, else_ } => {
                let c = self.emit_value(cond);
                let to_else = self.jump_if_false_placeholder(c);
                self.emit_block(then, Some(dst));
                match else_ {
                    Some(else_expr) => {
                        let to_end = self.jump_placeholder();
                        self.patch_to_here(to_else);
                        self.emit_into(else_expr, dst);
                        self.patch_to_here(to_end);
                    }
                    None => {
                        let to_end = self.jump_placeholder();
                        self.patch_to_here(to_else);
                        self.push(Instr::LoadUnit { dst });
                        self.patch_to_here(to_end);
                    }
                }
            }
            ExprKind::IfLet {
                pat,
                scrutinee,
                then,
                else_,
            } => {
                let v = self.alloc_temp();
                self.emit_into(scrutinee, v);
                let mut fails = Vec::new();
                self.emit_pattern(pat, v, &mut fails);
                self.emit_block(then, Some(dst));
                let to_end = self.jump_placeholder();
                for f in fails {
                    self.patch_to_here(f);
                }
                match else_ {
                    Some(else_expr) => self.emit_into(else_expr, dst),
                    None => {
                        self.push(Instr::LoadUnit { dst });
                    }
                }
                self.patch_to_here(to_end);
            }
            ExprKind::Match { scrutinee, arms } => self.emit_match(scrutinee, arms, dst),
            ExprKind::While { cond, body } => {
                let start = self.code.len();
                self.loops.push(LoopFrame {
                    continues: vec![],
                    breaks: vec![],
                    continue_pc: Some(start),
                });
                let c = self.emit_value(cond);
                let to_end = self.jump_if_false_placeholder(c);
                let scratch_mark = self.temps_mark();
                self.emit_block(body, None);
                self.temps_reset(scratch_mark);
                self.jump_back_to(start);
                self.patch_to_here(to_end);
                let frame = self.loops.pop().unwrap();
                for b in frame.breaks {
                    self.patch_to_here(b);
                }
                self.push(Instr::LoadUnit { dst });
            }
            ExprKind::Loop { body } => {
                let start = self.code.len();
                self.loops.push(LoopFrame {
                    continues: vec![],
                    breaks: vec![],
                    continue_pc: Some(start),
                });
                let scratch_mark = self.temps_mark();
                self.emit_block(body, None);
                self.temps_reset(scratch_mark);
                self.jump_back_to(start);
                let frame = self.loops.pop().unwrap();
                for b in frame.breaks {
                    self.patch_to_here(b);
                }
                self.push(Instr::LoadUnit { dst });
            }
            ExprKind::For { iter, body, .. } => self.emit_for(e, iter, body, dst),
            ExprKind::Range { .. } => {
                // Only reachable on checker-rejected input.
                self.push(Instr::LoadUnit { dst });
            }
            ExprKind::Break => {
                let j = self.jump_placeholder();
                if let Some(frame) = self.loops.last_mut() {
                    frame.breaks.push(j);
                }
            }
            ExprKind::Continue => {
                if let Some(pc) = self.loops.last().and_then(|f| f.continue_pc) {
                    self.jump_back_to(pc);
                } else {
                    let j = self.jump_placeholder();
                    if let Some(frame) = self.loops.last_mut() {
                        frame.continues.push(j);
                    }
                }
            }
            ExprKind::Return(value) => match value {
                Some(v) => {
                    let r = self.emit_value(v);
                    self.push(Instr::Ret { src: r });
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
                    .closures
                    .get(&e.id)
                    .map(|c| c.proto)
                    .expect("closure resolved");
                self.em.ensure_proto(proto);
                self.push(Instr::MakeClosure { dst, proto });
            }
            ExprKind::Try(inner) => {
                let v = self.alloc_temp();
                self.emit_into(inner, v);
                let tag = self.alloc_temp();
                self.push(Instr::GetTag { dst: tag, obj: v });
                let propagate_tag = match self.em.res.try_kinds.get(&e.id) {
                    Some(TryKind::Result) => defs::TAG_ERR,
                    _ => defs::TAG_NONE,
                };
                let lit = self.alloc_temp();
                self.emit_int(propagate_tag as i64, lit);
                let cond = self.alloc_temp();
                self.push(Instr::EqI {
                    dst: cond,
                    a: tag,
                    b: lit,
                });
                let skip = self.jump_if_false_placeholder(cond);
                // Propagate the same None/Err value (PRD §3.5).
                self.push(Instr::Ret { src: v });
                self.patch_to_here(skip);
                self.push(Instr::GetField {
                    dst,
                    obj: v,
                    idx: 0,
                });
            }
        }
    }

    fn emit_int(&mut self, n: i64, dst: u16) {
        if let Ok(v) = i32::try_from(n) {
            self.push(Instr::LoadInt { dst, v });
        } else {
            let k = self.em.intern_const(Const::Int(n));
            self.push(Instr::LoadConst { dst, k });
        }
    }

    fn emit_path(&mut self, e: &Expr, dst: u16) {
        if let Some(res) = self.em.res.var_refs.get(&e.id) {
            match res {
                VarRes::Local(l) => {
                    let src = self.local_reg(*l);
                    if self.captured.contains(l) {
                        self.push(Instr::CellGet { dst, cell: src });
                    } else {
                        self.push(Instr::Move { dst, src });
                    }
                }
                VarRes::Capture(slot) => {
                    let cell = self.cap_reg(*slot);
                    self.push(Instr::CellGet { dst, cell });
                }
            }
            return;
        }
        match self.em.res.paths.get(&e.id) {
            Some(PathRes::FnValue(proto)) => {
                let proto = *proto;
                self.em.ensure_proto(proto);
                self.push(Instr::MakeClosure { dst, proto });
            }
            Some(PathRes::Const(c)) => match c {
                Const::Unit => {
                    self.push(Instr::LoadUnit { dst });
                }
                Const::Bool(b) => {
                    let v = *b;
                    self.push(Instr::LoadBool { dst, v });
                }
                other => {
                    let k = self.em.intern_const(other.clone());
                    self.push(Instr::LoadConst { dst, k });
                }
            },
            Some(PathRes::Variant { def, tag }) => {
                let (def, tag) = (def.0, *tag as u16);
                self.push(Instr::NewEnum {
                    dst,
                    def,
                    tag,
                    base: 0,
                    n: 0,
                });
            }
            None => {
                // Checker error path.
                self.push(Instr::LoadUnit { dst });
            }
        }
    }

    fn emit_unary(&mut self, e: &Expr, operand: &Expr, dst: u16) {
        let kind = self.em.res.un_ops.get(&e.id).copied();
        match kind {
            Some(UnOpKind::NegInt) => {
                let src = self.emit_value(operand);
                self.push(Instr::NegI { dst, src });
            }
            Some(UnOpKind::NegFloat) => {
                let src = self.emit_value(operand);
                self.push(Instr::NegF { dst, src });
            }
            Some(UnOpKind::Not) => {
                let src = self.emit_value(operand);
                self.push(Instr::Not { dst, src });
            }
            Some(UnOpKind::NegCall { proto }) => {
                let base = self.alloc_window(1);
                self.emit_into(operand, base);
                self.push(Instr::Call {
                    dst,
                    base,
                    nargs: 1,
                    target: CallTarget::Proto(proto),
                });
            }
            None => {
                self.push(Instr::LoadUnit { dst });
            }
        }
    }

    fn emit_binary(&mut self, e: &Expr, lhs: &Expr, rhs: &Expr, dst: u16) {
        use crate::ast::BinOp as B;
        let Some(kind) = self.em.res.bin_ops.get(&e.id).cloned() else {
            self.push(Instr::LoadUnit { dst });
            return;
        };
        match kind {
            BinOpKind::And => {
                self.emit_into(lhs, dst);
                let short = self.jump_if_false_placeholder(dst);
                self.emit_into(rhs, dst);
                self.patch_to_here(short);
            }
            BinOpKind::Or => {
                self.emit_into(lhs, dst);
                let short = self.jump_if_true_placeholder(dst);
                self.emit_into(rhs, dst);
                self.patch_to_here(short);
            }
            BinOpKind::IntArith(op) => {
                let a = self.emit_value(lhs);
                let b = self.emit_value(rhs);
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
                let a = self.emit_value(lhs);
                let b = self.emit_value(rhs);
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
                let a = self.emit_value(lhs);
                let b = self.emit_value(rhs);
                self.push(Instr::ConcatStr { dst, a, b });
            }
            BinOpKind::EqPrim { kind, negate } => {
                let a = self.emit_value(lhs);
                let b = self.emit_value(rhs);
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
                let a = self.emit_value(lhs);
                let b = self.emit_value(rhs);
                self.emit_cmp_prim(kind, op, a, b, dst);
            }
            BinOpKind::EqValue { negate } => {
                let base = self.alloc_window(2);
                self.emit_into(lhs, base);
                self.emit_into(rhs, base + 1);
                self.push(Instr::Call {
                    dst,
                    base,
                    nargs: 2,
                    target: CallTarget::Builtin(Builtin::ValueEq),
                });
                if negate {
                    self.push(Instr::Not { dst, src: dst });
                }
            }
            BinOpKind::EqCall { proto, negate } => {
                let base = self.alloc_window(2);
                self.emit_into(lhs, base);
                self.emit_into(rhs, base + 1);
                self.push(Instr::Call {
                    dst,
                    base,
                    nargs: 2,
                    target: CallTarget::Proto(proto),
                });
                if negate {
                    self.push(Instr::Not { dst, src: dst });
                }
            }
            BinOpKind::CmpValue { op } => {
                let base = self.alloc_window(2);
                self.emit_into(lhs, base);
                self.emit_into(rhs, base + 1);
                let c = self.alloc_temp();
                self.push(Instr::Call {
                    dst: c,
                    base,
                    nargs: 2,
                    target: CallTarget::Builtin(Builtin::ValueCmp),
                });
                let zero = self.alloc_temp();
                self.push(Instr::LoadInt { dst: zero, v: 0 });
                self.emit_cmp_prim(PrimKind::Int, op, c, zero, dst);
            }
            BinOpKind::CmpCall { proto, op } => {
                let base = self.alloc_window(2);
                self.emit_into(lhs, base);
                self.emit_into(rhs, base + 1);
                let c = self.alloc_temp();
                self.push(Instr::Call {
                    dst: c,
                    base,
                    nargs: 2,
                    target: CallTarget::Proto(proto),
                });
                let zero = self.alloc_temp();
                self.push(Instr::LoadInt { dst: zero, v: 0 });
                self.emit_cmp_prim(PrimKind::Int, op, c, zero, dst);
            }
            BinOpKind::ArithCall { proto } => {
                let base = self.alloc_window(2);
                self.emit_into(lhs, base);
                self.emit_into(rhs, base + 1);
                self.push(Instr::Call {
                    dst,
                    base,
                    nargs: 2,
                    target: CallTarget::Proto(proto),
                });
            }
        }
    }

    fn emit_cmp_prim(&mut self, kind: PrimKind, op: crate::ast::BinOp, a: u16, b: u16, dst: u16) {
        use crate::ast::BinOp as B;
        // Gt/Ge are emitted as swapped Lt/Le.
        let (x, y, le) = match op {
            B::Lt => (a, b, false),
            B::Le => (a, b, true),
            B::Gt => (b, a, false),
            _ => (b, a, true),
        };
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
            self.em.res.bin_ops.get(&e.id).cloned()
        } else {
            None
        };
        if compound && arith.is_none() {
            // Checker rejected the operator; evaluate for effects.
            let tmp = self.alloc_temp();
            self.emit_into(value, tmp);
            return;
        }
        match &target.kind {
            ExprKind::Path(_) => match self.em.res.var_refs.get(&target.id) {
                Some(VarRes::Local(l)) => {
                    let l = *l;
                    let reg = self.local_reg(l);
                    if self.captured.contains(&l) {
                        let cur = self.alloc_temp();
                        if let Some(kind) = &arith {
                            self.push(Instr::CellGet {
                                dst: cur,
                                cell: reg,
                            });
                            let tmp = self.alloc_temp();
                            self.emit_into(value, tmp);
                            self.emit_arith_kind(kind, cur, tmp, cur);
                        } else {
                            self.emit_into(value, cur);
                        }
                        self.push(Instr::CellSet {
                            cell: reg,
                            src: cur,
                        });
                    } else if let Some(kind) = &arith {
                        let tmp = self.alloc_temp();
                        self.emit_into(value, tmp);
                        self.emit_arith_kind(kind, reg, tmp, reg);
                    } else {
                        self.emit_into(value, reg);
                    }
                }
                Some(VarRes::Capture(slot)) => {
                    let cell = self.cap_reg(*slot);
                    let cur = self.alloc_temp();
                    if let Some(kind) = &arith {
                        self.push(Instr::CellGet { dst: cur, cell });
                        let tmp = self.alloc_temp();
                        self.emit_into(value, tmp);
                        self.emit_arith_kind(kind, cur, tmp, cur);
                    } else {
                        self.emit_into(value, cur);
                    }
                    self.push(Instr::CellSet { cell, src: cur });
                }
                None => {
                    let tmp = self.alloc_temp();
                    self.emit_into(value, tmp);
                }
            },
            ExprKind::Field { obj, .. } => {
                let o = self.emit_value(obj);
                let idx = *self.em.res.fields.get(&target.id).unwrap_or(&0);
                let tmp = self.alloc_temp();
                if let Some(kind) = &arith {
                    let cur = self.alloc_temp();
                    self.push(Instr::GetField {
                        dst: cur,
                        obj: o,
                        idx,
                    });
                    self.emit_into(value, tmp);
                    self.emit_arith_kind(kind, cur, tmp, cur);
                    self.push(Instr::SetField {
                        obj: o,
                        idx,
                        src: cur,
                    });
                } else {
                    self.emit_into(value, tmp);
                    self.push(Instr::SetField {
                        obj: o,
                        idx,
                        src: tmp,
                    });
                }
            }
            ExprKind::Index { obj, idx } => {
                let index_kind = self.em.res.indexes.get(&target.id).cloned();
                let o = self.emit_value(obj);
                let i = self.emit_value(idx);
                let tmp = self.alloc_temp();
                let src = if let Some(kind) = &arith {
                    let cur = self.alloc_temp();
                    match index_kind {
                        Some(IndexKind::Map) => {
                            self.push(Instr::MapIndexGet {
                                dst: cur,
                                map: o,
                                key: i,
                            });
                        }
                        _ => {
                            self.push(Instr::ListIndexGet {
                                dst: cur,
                                list: o,
                                idx: i,
                            });
                        }
                    }
                    self.emit_into(value, tmp);
                    self.emit_arith_kind(kind, cur, tmp, cur);
                    cur
                } else {
                    self.emit_into(value, tmp);
                    tmp
                };
                match index_kind {
                    Some(IndexKind::Map) => {
                        self.push(Instr::MapIndexSet {
                            map: o,
                            key: i,
                            src,
                        });
                    }
                    _ => {
                        self.push(Instr::ListIndexSet {
                            list: o,
                            idx: i,
                            src,
                        });
                    }
                }
            }
            _ => {
                // Checker rejected; evaluate for effects.
                let tmp = self.alloc_temp();
                self.emit_into(value, tmp);
            }
        }
    }

    /// Interpolated string: literal parts load constants, holes render
    /// through `Builtin::Str` (so custom Display impls — and their
    /// faults — behave exactly like `str(x)`), folded left-to-right with
    /// `ConcatStr`.
    fn emit_str_interp(&mut self, parts: &[crate::ast::InterpPart], dst: u16) {
        use crate::ast::InterpPart;
        let mut started = false;
        for part in parts {
            let piece = if started { self.alloc_temp() } else { dst };
            match part {
                InterpPart::Lit(s) => {
                    let k = self.em.intern_const(Const::Str(s.as_str().into()));
                    self.push(Instr::LoadConst { dst: piece, k });
                }
                InterpPart::Hole(h) => {
                    let base = self.alloc_window(1);
                    self.emit_display_into(h, base);
                    self.push(Instr::Call {
                        dst: piece,
                        base,
                        nargs: 1,
                        target: CallTarget::Builtin(Builtin::Str),
                    });
                }
            }
            if started {
                self.push(Instr::ConcatStr {
                    dst,
                    a: dst,
                    b: piece,
                });
            }
            started = true;
        }
        if !started {
            let k = self.em.intern_const(Const::Str("".into()));
            self.push(Instr::LoadConst { dst, k });
        }
    }

    /// Convert between a unit family and its backing number: one constant
    /// multiply (into base units) or divide (out of them). A factor of 1 —
    /// the base unit itself — is a plain move.
    fn emit_unit_conv(&mut self, operand: &Expr, conv: ConvKind, dst: u16) {
        let (factor, into_base) = match conv {
            ConvKind::In { factor } => (factor, true),
            ConvKind::Out { factor } => (factor, false),
        };
        let src = self.emit_value(operand);
        if factor.is_one() {
            self.push(Instr::Move { dst, src });
            return;
        }
        let k = self.alloc_temp();
        match factor {
            Factor::Int(n) => {
                self.emit_int(n, k);
                if into_base {
                    self.push(Instr::MulI { dst, a: src, b: k });
                } else {
                    self.push(Instr::DivI { dst, a: src, b: k });
                }
            }
            Factor::Float(f) => {
                let c = self.em.intern_const(Const::Float(f));
                self.push(Instr::LoadConst { dst: k, k: c });
                if into_base {
                    self.push(Instr::MulF { dst, a: src, b: k });
                } else {
                    self.push(Instr::DivF { dst, a: src, b: k });
                }
            }
        }
    }

    /// Emit `dst = a <op> b` for an arithmetic lowering the checker
    /// recorded (compound assignment read-modify-write).
    fn emit_arith_kind(&mut self, kind: &BinOpKind, a: u16, b: u16, dst: u16) {
        use crate::ast::BinOp as B;
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
                let base = self.alloc_window(2);
                self.push(Instr::Move { dst: base, src: a });
                self.push(Instr::Move {
                    dst: base + 1,
                    src: b,
                });
                self.push(Instr::Call {
                    dst,
                    base,
                    nargs: 2,
                    target: CallTarget::Proto(*proto),
                });
            }
            // Not an arithmetic lowering (checker error path).
            _ => {
                self.push(Instr::LoadUnit { dst });
            }
        }
    }

    fn emit_call(&mut self, e: &Expr, callee: &Expr, args: &[Expr], dst: u16) {
        let kind = self.em.res.calls.get(&e.id).cloned();
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
            // `Duration::ms(n)` — scale the one argument inline.
            Some(CallKind::UnitConv) => match (self.em.res.unit_convs.get(&e.id), args) {
                (Some(&conv), [arg]) => self.emit_unit_conv(arg, conv, dst),
                _ => {
                    self.push(Instr::LoadUnit { dst });
                }
            },
            Some(CallKind::Variant { def, tag }) => {
                let base = self.alloc_window(args.len() as u16);
                for (i, a) in args.iter().enumerate() {
                    self.emit_into(a, base + i as u16);
                }
                self.push(Instr::NewEnum {
                    dst,
                    def: def.0,
                    tag: tag as u16,
                    base,
                    n: args.len() as u16,
                });
            }
            Some(CallKind::Value) => {
                let f = self.emit_value(callee);
                let base = self.alloc_window(args.len() as u16);
                for (i, a) in args.iter().enumerate() {
                    self.emit_into(a, base + i as u16);
                }
                self.push(Instr::CallValue {
                    dst,
                    f,
                    base,
                    nargs: args.len() as u16,
                });
            }
            None => {
                self.push(Instr::LoadUnit { dst });
            }
        }
    }

    fn emit_args_call(&mut self, args: &[Expr], dst: u16, target: CallTarget) {
        let base = self.alloc_window(args.len() as u16);
        for (i, a) in args.iter().enumerate() {
            self.emit_into(a, base + i as u16);
        }
        self.push(Instr::Call {
            dst,
            base,
            nargs: args.len() as u16,
            target,
        });
    }

    /// Like `emit_args_call`, but renders unit-family arguments to text
    /// first. Unit values are plain numbers at runtime, so the static type
    /// is the only place their family survives — this is where `println(d)`
    /// becomes `1.5s` instead of `1500000000`.
    fn emit_display_args_call(&mut self, args: &[Expr], dst: u16, target: CallTarget) {
        let base = self.alloc_window(args.len() as u16);
        for (i, a) in args.iter().enumerate() {
            self.emit_display_into(a, base + i as u16);
        }
        self.push(Instr::Call {
            dst,
            base,
            nargs: args.len() as u16,
            target,
        });
    }

    /// Emit `e` into `dst`, replacing it with its rendered text when its
    /// static type is a unit family.
    fn emit_display_into(&mut self, e: &Expr, dst: u16) {
        self.emit_into(e, dst);
        let Some(def) = self.unit_family_of(e) else {
            return;
        };
        // A user `impl Display` for the family wins, mirroring how the VM
        // treats structs and enums.
        if let Some(&proto) = self.em.res.impl_maps.display.get(&def.0) {
            self.em.ensure_proto(proto);
            let base = self.alloc_window(1);
            self.push(Instr::Move {
                dst: base,
                src: dst,
            });
            self.push(Instr::Call {
                dst,
                base,
                nargs: 1,
                target: CallTarget::Proto(proto),
            });
            return;
        }
        let base = self.alloc_window(2);
        self.push(Instr::Move {
            dst: base,
            src: dst,
        });
        self.emit_int(def.0 as i64, base + 1);
        self.push(Instr::Call {
            dst,
            base,
            nargs: 2,
            target: CallTarget::Builtin(Builtin::FmtQuantity),
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

    fn emit_method_call(&mut self, e: &Expr, recv: &Expr, args: &[Expr], dst: u16) {
        let Some(res) = self.em.res.methods.get(&e.id).cloned() else {
            self.push(Instr::LoadUnit { dst });
            return;
        };
        let base = self.alloc_window(args.len() as u16 + 1);
        self.emit_into(recv, base);
        for (i, a) in args.iter().enumerate() {
            self.emit_into(a, base + 1 + i as u16);
        }
        let nargs = args.len() as u16 + 1;
        match res {
            MethodRes::Proto(proto) => {
                self.em.ensure_proto(proto);
                self.push(Instr::Call {
                    dst,
                    base,
                    nargs,
                    target: CallTarget::Proto(proto),
                });
            }
            MethodRes::Host(idx) => {
                self.push(Instr::Call {
                    dst,
                    base,
                    nargs,
                    target: CallTarget::Host(idx),
                });
            }
            MethodRes::Builtin(b) => {
                self.push(Instr::Call {
                    dst,
                    base,
                    nargs,
                    target: CallTarget::Builtin(b),
                });
            }
            MethodRes::Virtual { slot } => {
                self.push(Instr::CallVirtual {
                    dst,
                    base,
                    nargs,
                    slot,
                });
            }
        }
    }

    fn emit_struct_lit(&mut self, e: &Expr, fields: &[(Ident, Expr)], dst: u16) {
        let Some(res) = self.em.res.struct_lits.get(&e.id).cloned() else {
            self.push(Instr::LoadUnit { dst });
            return;
        };
        let order = self
            .em
            .res
            .field_orders
            .get(&e.id)
            .cloned()
            .unwrap_or_default();
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
        let base = self.alloc_window(n_fields as u16);
        // Evaluate in source order, placing each value at its declared slot.
        for (i, (_, value)) in fields.iter().enumerate() {
            match order.get(i) {
                Some(&idx) if (idx as usize) < n_fields => {
                    self.emit_into(value, base + idx);
                }
                _ => {
                    let scratch = self.alloc_temp();
                    self.emit_into(value, scratch);
                }
            }
        }
        match res {
            StructLitRes::Struct(def) => {
                self.push(Instr::NewStruct {
                    dst,
                    def: def.0,
                    base,
                    n: n_fields as u16,
                });
            }
            StructLitRes::Variant { def, tag } => {
                self.push(Instr::NewEnum {
                    dst,
                    def: def.0,
                    tag: tag as u16,
                    base,
                    n: n_fields as u16,
                });
            }
        }
    }

    // ------------------------------------------------------------- match

    fn emit_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], dst: u16) {
        let v = self.alloc_temp();
        self.emit_into(scrutinee, v);
        let mut ends = Vec::new();
        for arm in arms {
            let mark = self.temps_mark();
            let mut fails = Vec::new();
            self.emit_pattern(&arm.pat, v, &mut fails);
            if let Some(guard) = &arm.guard {
                let g = self.emit_value(guard);
                fails.push(self.jump_if_false_placeholder(g));
            }
            self.emit_into(&arm.body, dst);
            ends.push(self.jump_placeholder());
            for f in fails {
                self.patch_to_here(f);
            }
            self.temps_reset(mark);
        }
        // The checker proved exhaustiveness; reaching here is a bug.
        self.push(Instr::Fault {
            code: FaultCode::UnreachableMatch,
        });
        for end in ends {
            self.patch_to_here(end);
        }
    }

    fn emit_int_pattern_test(&mut self, n: i64, reg: u16, fails: &mut Vec<usize>) {
        let lit = self.alloc_temp();
        self.emit_int(n, lit);
        let c = self.alloc_temp();
        self.push(Instr::EqI {
            dst: c,
            a: reg,
            b: lit,
        });
        fails.push(self.jump_if_false_placeholder(c));
    }

    /// Emit tests for `pat` against the value in `reg`. Failure jumps are
    /// appended to `fails` (to be patched at the next alternative). Emits
    /// binding stores along the way.
    fn emit_pattern(&mut self, pat: &Pattern, reg: u16, fails: &mut Vec<usize>) {
        self.cur_span = pat.span;
        match &pat.kind {
            PatternKind::Wildcard | PatternKind::Error => {}
            PatternKind::Binding(_) => {
                if let Some(&(_, tag)) = self.em.res.pattern_variants.get(&pat.id) {
                    self.emit_tag_test(reg, tag, fails);
                } else if let Some(&local) = self.em.res.decl_locals.get(&pat.id) {
                    let dst = self.local_reg(local);
                    if self.captured.contains(&local) {
                        self.push(Instr::NewCell { dst, src: reg });
                    } else {
                        self.push(Instr::Move { dst, src: reg });
                    }
                }
            }
            PatternKind::IntLit(n) => {
                self.emit_int_pattern_test(*n, reg, fails);
            }
            // Folded to a base-unit constant by the checker; float-backed
            // families never reach here (they cannot be matched on).
            PatternKind::QuantityLit { .. } => {
                if let Some(&Factor::Int(n)) = self.em.res.quantity_lits.get(&pat.id) {
                    self.emit_int_pattern_test(n, reg, fails);
                }
            }
            PatternKind::BoolLit(b) => {
                if *b {
                    fails.push(self.jump_if_false_placeholder(reg));
                } else {
                    fails.push(self.jump_if_true_placeholder(reg));
                }
            }
            PatternKind::CharLit(c) => {
                let lit = self.alloc_temp();
                let k = self.em.intern_const(Const::Char(*c));
                self.push(Instr::LoadConst { dst: lit, k });
                let cond = self.alloc_temp();
                self.push(Instr::EqChar {
                    dst: cond,
                    a: reg,
                    b: lit,
                });
                fails.push(self.jump_if_false_placeholder(cond));
            }
            PatternKind::StrLit(s) => {
                let lit = self.alloc_temp();
                let k = self.em.intern_const(Const::Str(s.as_str().into()));
                self.push(Instr::LoadConst { dst: lit, k });
                let cond = self.alloc_temp();
                self.push(Instr::EqStr {
                    dst: cond,
                    a: reg,
                    b: lit,
                });
                fails.push(self.jump_if_false_placeholder(cond));
            }
            PatternKind::Variant { args, .. } => {
                let Some(&(_, tag)) = self.em.res.pattern_variants.get(&pat.id) else {
                    return;
                };
                self.emit_tag_test(reg, tag, fails);
                match args {
                    VariantPatArgs::Unit => {}
                    VariantPatArgs::Tuple(pats) => {
                        for (i, p) in pats.iter().enumerate() {
                            if pattern_is_trivial(p) {
                                // Still need bindings under trivial pats.
                                if matches!(p.kind, PatternKind::Wildcard) {
                                    continue;
                                }
                            }
                            let field = self.alloc_temp();
                            self.push(Instr::GetField {
                                dst: field,
                                obj: reg,
                                idx: i as u16,
                            });
                            self.emit_pattern(p, field, fails);
                        }
                    }
                    VariantPatArgs::Struct { fields, .. } => {
                        let order = self
                            .em
                            .res
                            .field_orders
                            .get(&pat.id)
                            .cloned()
                            .unwrap_or_default();
                        for (i, (_, p)) in fields.iter().enumerate() {
                            let Some(&idx) = order.get(i) else { continue };
                            if idx == u16::MAX || matches!(p.kind, PatternKind::Wildcard) {
                                continue;
                            }
                            let field = self.alloc_temp();
                            self.push(Instr::GetField {
                                dst: field,
                                obj: reg,
                                idx,
                            });
                            self.emit_pattern(p, field, fails);
                        }
                    }
                }
            }
            PatternKind::Struct { fields, .. } => {
                let order = self
                    .em
                    .res
                    .field_orders
                    .get(&pat.id)
                    .cloned()
                    .unwrap_or_default();
                for (i, (_, p)) in fields.iter().enumerate() {
                    let Some(&idx) = order.get(i) else { continue };
                    if idx == u16::MAX || matches!(p.kind, PatternKind::Wildcard) {
                        continue;
                    }
                    let field = self.alloc_temp();
                    self.push(Instr::GetField {
                        dst: field,
                        obj: reg,
                        idx,
                    });
                    self.emit_pattern(p, field, fails);
                }
            }
            PatternKind::Or(alts) => {
                // Succeed if any alternative matches (no bindings inside,
                // enforced by the checker).
                let mut successes = Vec::new();
                for (i, alt) in alts.iter().enumerate() {
                    let last = i + 1 == alts.len();
                    if last {
                        self.emit_pattern(alt, reg, fails);
                    } else {
                        let mut alt_fails = Vec::new();
                        self.emit_pattern(alt, reg, &mut alt_fails);
                        successes.push(self.jump_placeholder());
                        for f in alt_fails {
                            self.patch_to_here(f);
                        }
                    }
                }
                for s in successes {
                    self.patch_to_here(s);
                }
            }
        }
    }

    fn emit_tag_test(&mut self, reg: u16, tag: u32, fails: &mut Vec<usize>) {
        let t = self.alloc_temp();
        self.push(Instr::GetTag { dst: t, obj: reg });
        let lit = self.alloc_temp();
        self.emit_int(tag as i64, lit);
        let cond = self.alloc_temp();
        self.push(Instr::EqI {
            dst: cond,
            a: t,
            b: lit,
        });
        fails.push(self.jump_if_false_placeholder(cond));
    }

    // --------------------------------------------------------------- for

    fn emit_for(&mut self, e: &Expr, iter: &Expr, body: &Block, dst: u16) {
        let kind = self
            .em
            .res
            .for_kinds
            .get(&e.id)
            .copied()
            .unwrap_or(ForKind::List);
        let var_local = *self
            .em
            .res
            .decl_locals
            .get(&e.id)
            .expect("for var resolved");
        let var_reg = self.local_reg(var_local);
        let var_captured = self.captured.contains(&var_local);

        match kind {
            ForKind::RangeExclusive | ForKind::RangeInclusive => {
                let ExprKind::Range { lo, hi, .. } = &iter.kind else {
                    unreachable!("range for-kind without range iter")
                };
                let cursor = self.alloc_temp();
                self.emit_into(lo, cursor);
                let limit = self.alloc_temp();
                self.emit_into(hi, limit);
                let one = self.alloc_temp();
                self.push(Instr::LoadInt { dst: one, v: 1 });
                let start = self.code.len();
                let cond = self.alloc_temp();
                if matches!(kind, ForKind::RangeInclusive) {
                    self.push(Instr::LeI {
                        dst: cond,
                        a: cursor,
                        b: limit,
                    });
                } else {
                    self.push(Instr::LtI {
                        dst: cond,
                        a: cursor,
                        b: limit,
                    });
                }
                let to_end = self.jump_if_false_placeholder(cond);
                if var_captured {
                    self.push(Instr::NewCell {
                        dst: var_reg,
                        src: cursor,
                    });
                } else {
                    self.push(Instr::Move {
                        dst: var_reg,
                        src: cursor,
                    });
                }
                self.loops.push(LoopFrame {
                    continues: vec![],
                    breaks: vec![],
                    continue_pc: None,
                });
                let mark = self.temps_mark();
                self.emit_block(body, None);
                self.temps_reset(mark);
                let frame = self.loops.pop().unwrap();
                for c in frame.continues {
                    self.patch_to_here(c);
                }
                self.push(Instr::AddI {
                    dst: cursor,
                    a: cursor,
                    b: one,
                });
                self.jump_back_to(start);
                self.patch_to_here(to_end);
                for b in frame.breaks {
                    self.patch_to_here(b);
                }
            }
            ForKind::List | ForKind::MapKeys | ForKind::StrChars => {
                // Materialize the iterable (keys()/chars() create a list).
                let list = self.alloc_temp();
                match kind {
                    ForKind::List => self.emit_into(iter, list),
                    ForKind::MapKeys => {
                        let m = self.emit_value(iter);
                        let base = self.alloc_window(1);
                        self.push(Instr::Move { dst: base, src: m });
                        self.push(Instr::Call {
                            dst: list,
                            base,
                            nargs: 1,
                            target: CallTarget::Builtin(Builtin::MapKeys),
                        });
                    }
                    _ => {
                        let s = self.emit_value(iter);
                        let base = self.alloc_window(1);
                        self.push(Instr::Move { dst: base, src: s });
                        self.push(Instr::Call {
                            dst: list,
                            base,
                            nargs: 1,
                            target: CallTarget::Builtin(Builtin::StrChars),
                        });
                    }
                }
                let idx = self.alloc_temp();
                self.push(Instr::LoadInt { dst: idx, v: 0 });
                let one = self.alloc_temp();
                self.push(Instr::LoadInt { dst: one, v: 1 });
                let start = self.code.len();
                // Re-check the length each iteration: mutation during
                // iteration shrinks/extends the walk instead of faulting.
                let len = self.alloc_temp();
                let base = self.alloc_window(1);
                self.push(Instr::Move {
                    dst: base,
                    src: list,
                });
                self.push(Instr::Call {
                    dst: len,
                    base,
                    nargs: 1,
                    target: CallTarget::Builtin(Builtin::ListLen),
                });
                let cond = self.alloc_temp();
                self.push(Instr::LtI {
                    dst: cond,
                    a: idx,
                    b: len,
                });
                let to_end = self.jump_if_false_placeholder(cond);
                if var_captured {
                    let tmp = self.alloc_temp();
                    self.push(Instr::ListIndexGet {
                        dst: tmp,
                        list,
                        idx,
                    });
                    self.push(Instr::NewCell {
                        dst: var_reg,
                        src: tmp,
                    });
                } else {
                    self.push(Instr::ListIndexGet {
                        dst: var_reg,
                        list,
                        idx,
                    });
                }
                self.loops.push(LoopFrame {
                    continues: vec![],
                    breaks: vec![],
                    continue_pc: None,
                });
                let mark = self.temps_mark();
                self.emit_block(body, None);
                self.temps_reset(mark);
                let frame = self.loops.pop().unwrap();
                for c in frame.continues {
                    self.patch_to_here(c);
                }
                self.push(Instr::AddI {
                    dst: idx,
                    a: idx,
                    b: one,
                });
                self.jump_back_to(start);
                self.patch_to_here(to_end);
                for b in frame.breaks {
                    self.patch_to_here(b);
                }
            }
        }
        self.push(Instr::LoadUnit { dst });
    }
}

fn pattern_is_trivial(p: &Pattern) -> bool {
    matches!(p.kind, PatternKind::Wildcard)
}
