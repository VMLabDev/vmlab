//! A bytecode verifier: every operand of every instruction is in range.
//!
//! The VM trusts its input — instruction operands are the compiler's own
//! output and are not re-validated per dispatch, so a register index past
//! the end of a frame is a panic rather than a fault (see the
//! `wscript-vm` module docs). That contract was prose. This turns it into
//! a check the emitter's own output is held to: `n_regs` is the frame the
//! VM allocates, so "every register operand is below it" is exactly the
//! obligation the emitter's high-water mark exists to discharge.
//!
//! Verification is *not* on the run path. It is a compiler-side assertion,
//! run over the script corpus by the test suite; an embedder running
//! `compile` pays nothing for it.
//!
//! What it does not cover: `CaptureSrc::Reg` names a register of the
//! *enclosing* frame, `CallVirtual::slot` a vtable chosen at runtime by
//! the receiver, and `CallTarget::Host` a table the host supplies — none
//! is decidable from one proto in isolation.

use std::fmt;

use crate::bytecode::{CallTarget, CompiledUnit, FnProto, Instr};

/// One out-of-range operand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyError {
    /// Index into `CompiledUnit::protos`.
    pub proto: usize,
    pub proto_name: String,
    /// Instruction index, or `None` for a whole-proto claim.
    pub pc: Option<usize>,
    pub message: String,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "proto #{} `{}`", self.proto, self.proto_name)?;
        if let Some(pc) = self.pc {
            write!(f, " pc {pc}")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// Check every proto in `unit`. Returns every violation found, not just the
/// first — one bad emitter path usually produces a family of them, and
/// seeing the family is what names the path.
pub fn verify(unit: &CompiledUnit) -> Result<(), Vec<VerifyError>> {
    let mut errs = Vec::new();
    for (idx, proto) in unit.protos.iter().enumerate() {
        let mut v = Verifier {
            unit,
            proto,
            idx,
            errs: &mut errs,
        };
        v.run();
    }
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

/// [`verify`], with the errors already rendered — for a test or an
/// assertion that only needs to print them.
pub fn verify_report(unit: &CompiledUnit) -> Result<(), String> {
    verify(unit).map_err(|errs| {
        errs.iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

struct Verifier<'a> {
    unit: &'a CompiledUnit,
    proto: &'a FnProto,
    idx: usize,
    errs: &'a mut Vec<VerifyError>,
}

impl Verifier<'_> {
    fn run(&mut self) {
        if self.proto.code.len() != self.proto.spans.len() {
            let (c, s) = (self.proto.code.len(), self.proto.spans.len());
            self.whole(format!("{c} instructions but {s} spans"));
        }
        if self.proto.code.is_empty() {
            self.whole("empty body: control would run off the end".into());
        }
        if self.proto.n_params > self.proto.n_regs {
            let (p, n) = (self.proto.n_params, self.proto.n_regs);
            self.whole(format!("{p} params do not fit in {n} registers"));
        }
        for pc in 0..self.proto.code.len() {
            self.instr(pc);
        }
    }

    /// Every register operand of one instruction.
    ///
    /// Exhaustive by construction: no `_` arm, so a new instruction cannot
    /// be added to the format without deciding what verifies it.
    fn instr(&mut self, pc: usize) {
        use Instr::*;
        match self.proto.code[pc] {
            // ---- constants & moves ----
            LoadConst { dst, k } => {
                self.reg(pc, "dst", dst);
                self.konst(pc, k);
            }
            LoadUnit { dst } | LoadBool { dst, .. } | LoadInt { dst, .. } => {
                self.reg(pc, "dst", dst)
            }
            Move { dst, src }
            | NegI { dst, src }
            | NegF { dst, src }
            | Not { dst, src }
            | NewCell { dst, src } => {
                self.reg(pc, "dst", dst);
                self.reg(pc, "src", src);
            }

            // ---- binary operators (all three operands are registers) ----
            AddI { dst, a, b }
            | SubI { dst, a, b }
            | MulI { dst, a, b }
            | DivI { dst, a, b }
            | RemI { dst, a, b }
            | AddF { dst, a, b }
            | SubF { dst, a, b }
            | MulF { dst, a, b }
            | DivF { dst, a, b }
            | RemF { dst, a, b }
            | ConcatStr { dst, a, b }
            | EqI { dst, a, b }
            | EqF { dst, a, b }
            | EqBool { dst, a, b }
            | EqChar { dst, a, b }
            | EqStr { dst, a, b }
            | LtI { dst, a, b }
            | LeI { dst, a, b }
            | LtF { dst, a, b }
            | LeF { dst, a, b }
            | LtChar { dst, a, b }
            | LeChar { dst, a, b }
            | LtStr { dst, a, b }
            | LeStr { dst, a, b } => {
                self.reg(pc, "dst", dst);
                self.reg(pc, "a", a);
                self.reg(pc, "b", b);
            }

            // ---- control flow ----
            Jump { off } => self.jump(pc, off),
            JumpIfFalse { cond, off } | JumpIfTrue { cond, off } => {
                self.reg(pc, "cond", cond);
                self.jump(pc, off);
            }

            // ---- calls ----
            Call {
                dst,
                base,
                nargs,
                target,
            } => {
                self.reg(pc, "dst", dst);
                self.window(pc, "args", base, nargs);
                if let CallTarget::Proto(p) = target {
                    self.proto_idx(pc, p);
                }
            }
            CallValue {
                dst,
                f,
                base,
                nargs,
            } => {
                self.reg(pc, "dst", dst);
                self.reg(pc, "f", f);
                self.window(pc, "args", base, nargs);
            }
            CallVirtual {
                dst, base, nargs, ..
            } => {
                self.reg(pc, "dst", dst);
                // The receiver is argument 0, so the window is never empty.
                self.window(pc, "args", base, nargs.max(1));
            }
            Ret { src } => self.reg(pc, "src", src),
            RetUnit => {}

            // ---- structs & enums ----
            NewStruct { dst, def, base, n } => {
                self.reg(pc, "dst", dst);
                self.window(pc, "fields", base, n);
                self.def(pc, def);
            }
            GetField { dst, obj, .. } => {
                self.reg(pc, "dst", dst);
                self.reg(pc, "obj", obj);
            }
            SetField { obj, src, .. } => {
                self.reg(pc, "obj", obj);
                self.reg(pc, "src", src);
            }
            NewEnum {
                dst, def, base, n, ..
            } => {
                self.reg(pc, "dst", dst);
                self.window(pc, "payload", base, n);
                self.def(pc, def);
            }
            GetTag { dst, obj } => {
                self.reg(pc, "dst", dst);
                self.reg(pc, "obj", obj);
            }

            // ---- containers ----
            NewList { dst, base, n } => {
                self.reg(pc, "dst", dst);
                self.window(pc, "items", base, n);
            }
            // `n` counts key/value *pairs*.
            NewMap { dst, base, n } => {
                self.reg(pc, "dst", dst);
                self.window(pc, "entries", base, n.saturating_mul(2));
            }
            ListIndexGet { dst, list, idx } => {
                self.reg(pc, "dst", dst);
                self.reg(pc, "list", list);
                self.reg(pc, "idx", idx);
            }
            ListIndexSet { list, idx, src } => {
                self.reg(pc, "list", list);
                self.reg(pc, "idx", idx);
                self.reg(pc, "src", src);
            }
            MapIndexGet { dst, map, key } => {
                self.reg(pc, "dst", dst);
                self.reg(pc, "map", map);
                self.reg(pc, "key", key);
            }
            MapIndexSet { map, key, src } => {
                self.reg(pc, "map", map);
                self.reg(pc, "key", key);
                self.reg(pc, "src", src);
            }

            // ---- closures & capture cells ----
            CellGet { dst, cell } => {
                self.reg(pc, "dst", dst);
                self.reg(pc, "cell", cell);
            }
            CellSet { cell, src } => {
                self.reg(pc, "cell", cell);
                self.reg(pc, "src", src);
            }
            MakeClosure { dst, proto } => {
                self.reg(pc, "dst", dst);
                self.proto_idx(pc, proto);
            }
            // A closure's capture vector is `FnProto::captures` of the proto
            // it was made from — this one.
            LoadCapture { dst, slot } => {
                self.reg(pc, "dst", dst);
                let n = self.proto.captures.len();
                if slot as usize >= n {
                    self.at(pc, format!("capture slot {slot} of {n}"));
                }
            }

            // ---- traits ----
            MakeDyn { dst, src, vt } => {
                self.reg(pc, "dst", dst);
                self.reg(pc, "src", src);
                let n = self.unit.vtables.len();
                if vt as usize >= n {
                    self.at(pc, format!("vtable {vt} of {n}"));
                }
            }

            // ---- misc ----
            Fault { .. } | Nop => {}
        }
    }

    fn reg(&mut self, pc: usize, what: &str, r: u16) {
        if r >= self.proto.n_regs {
            let n = self.proto.n_regs;
            self.at(pc, format!("{what} register {r} of {n}"));
        }
    }

    /// A contiguous run of `n` registers at `base`. An empty window names no
    /// register, so its base is allowed to sit one past the end — that is
    /// where the allocator's top is when nothing was reserved.
    fn window(&mut self, pc: usize, what: &str, base: u16, n: u16) {
        let n_regs = self.proto.n_regs;
        let end = base as u32 + n as u32;
        if end > n_regs as u32 {
            self.at(
                pc,
                format!("{what} window {base}..{end} exceeds {n_regs} registers"),
            );
        }
    }

    /// Jump offsets are relative to the *next* instruction.
    fn jump(&mut self, pc: usize, off: i32) {
        let target = pc as i64 + 1 + off as i64;
        let len = self.proto.code.len() as i64;
        if target < 0 || target >= len {
            self.at(pc, format!("jump to {target}, outside 0..{len}"));
        }
    }

    fn konst(&mut self, pc: usize, k: u32) {
        let n = self.unit.consts.len();
        if k as usize >= n {
            self.at(pc, format!("constant {k} of {n}"));
        }
    }

    fn proto_idx(&mut self, pc: usize, p: u32) {
        let n = self.unit.protos.len();
        if p as usize >= n {
            self.at(pc, format!("proto {p} of {n}"));
        }
    }

    fn def(&mut self, pc: usize, def: u32) {
        let n = self.unit.defs.len();
        if def as usize >= n {
            self.at(pc, format!("def {def} of {n}"));
        }
    }

    fn at(&mut self, pc: usize, message: String) {
        self.errs.push(VerifyError {
            proto: self.idx,
            proto_name: self.proto.name.clone(),
            pc: Some(pc),
            message,
        });
    }

    fn whole(&mut self, message: String) {
        self.errs.push(VerifyError {
            proto: self.idx,
            proto_name: self.proto.name.clone(),
            pc: None,
            message,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Const, FnProto};
    use crate::defs::DefTable;
    use crate::span::Span;

    /// A one-proto unit around `code`, with a frame of `n_regs`.
    fn unit(n_regs: u16, code: Vec<Instr>) -> CompiledUnit {
        CompiledUnit {
            id: 0,
            protos: vec![FnProto {
                name: "main".into(),
                n_params: 0,
                n_regs,
                spans: vec![Span::DUMMY; code.len()],
                code,
                captures: vec![],
            }],
            consts: vec![Const::Int(1)],
            defs: DefTable::with_builtins(),
            vtables: vec![],
            impls: Default::default(),
            exports: Default::default(),
            generic_fns: vec![],
            source_map: Default::default(),
        }
    }

    fn errors(u: &CompiledUnit) -> Vec<String> {
        match verify(u) {
            Ok(()) => vec![],
            Err(errs) => errs.iter().map(|e| e.message.clone()).collect(),
        }
    }

    #[test]
    fn a_well_formed_proto_verifies() {
        let u = unit(
            2,
            vec![
                Instr::LoadInt { dst: 0, v: 1 },
                Instr::Move { dst: 1, src: 0 },
                Instr::Ret { src: 1 },
            ],
        );
        assert_eq!(verify(&u), Ok(()));
    }

    /// The frame-sizing contract: `n_regs` is what the VM allocates, so a
    /// register at or past it reads off the end of the frame.
    #[test]
    fn a_register_past_the_frame_is_caught() {
        let u = unit(2, vec![Instr::Move { dst: 2, src: 0 }, Instr::RetUnit]);
        assert_eq!(errors(&u), ["dst register 2 of 2"]);
    }

    /// An argument window is a *run* of registers: the last one has to fit,
    /// not merely the base.
    #[test]
    fn a_window_running_past_the_frame_is_caught() {
        let u = unit(
            3,
            vec![
                Instr::NewList {
                    dst: 0,
                    base: 1,
                    n: 3,
                },
                Instr::RetUnit,
            ],
        );
        assert_eq!(errors(&u), ["items window 1..4 exceeds 3 registers"]);
    }

    /// A map's `n` counts pairs, so a two-entry literal reserves four
    /// registers — the arithmetic the check has to mirror.
    #[test]
    fn a_map_window_counts_two_registers_per_entry() {
        let u = unit(
            4,
            vec![
                Instr::NewMap {
                    dst: 0,
                    base: 1,
                    n: 2,
                },
                Instr::RetUnit,
            ],
        );
        assert_eq!(errors(&u), ["entries window 1..5 exceeds 4 registers"]);
    }

    /// An empty window reserves nothing, so its base legitimately sits at
    /// the allocator's top — one past the last register.
    #[test]
    fn an_empty_window_may_sit_at_the_top_of_the_frame() {
        let u = unit(
            1,
            vec![
                Instr::NewList {
                    dst: 0,
                    base: 1,
                    n: 0,
                },
                Instr::RetUnit,
            ],
        );
        assert_eq!(verify(&u), Ok(()));
    }

    #[test]
    fn a_jump_off_either_end_is_caught() {
        let past = unit(1, vec![Instr::Jump { off: 5 }, Instr::RetUnit]);
        assert_eq!(errors(&past), ["jump to 6, outside 0..2"]);
        let before = unit(1, vec![Instr::RetUnit, Instr::Jump { off: -9 }]);
        assert_eq!(errors(&before), ["jump to -7, outside 0..2"]);
    }

    /// Falling off the end is a jump to `code.len()`: in range for the
    /// arithmetic, out of range for the dispatch loop.
    #[test]
    fn a_jump_to_one_past_the_last_instruction_is_caught() {
        let u = unit(1, vec![Instr::Jump { off: 1 }, Instr::RetUnit]);
        assert_eq!(errors(&u), ["jump to 2, outside 0..2"]);
    }

    #[test]
    fn out_of_range_table_indices_are_caught() {
        let u = unit(
            1,
            vec![
                Instr::LoadConst { dst: 0, k: 7 },
                Instr::MakeClosure { dst: 0, proto: 3 },
                Instr::MakeDyn {
                    dst: 0,
                    src: 0,
                    vt: 0,
                },
                Instr::LoadCapture { dst: 0, slot: 0 },
                Instr::RetUnit,
            ],
        );
        assert_eq!(
            errors(&u),
            [
                "constant 7 of 1",
                "proto 3 of 1",
                "vtable 0 of 0",
                "capture slot 0 of 0",
            ]
        );
    }

    #[test]
    fn a_proto_whose_shape_is_wrong_is_caught() {
        let mut u = unit(1, vec![Instr::RetUnit]);
        u.protos[0].spans.clear();
        u.protos[0].n_params = 4;
        assert_eq!(
            errors(&u),
            [
                "1 instructions but 0 spans",
                "4 params do not fit in 1 registers"
            ]
        );
    }

    #[test]
    fn an_empty_body_is_caught() {
        let u = unit(1, vec![]);
        assert_eq!(errors(&u), ["empty body: control would run off the end"]);
    }
}
