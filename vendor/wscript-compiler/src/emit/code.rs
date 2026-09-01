//! What one function body is emitted *into*: a register file, a label
//! table and an instruction stream.
//!
//! Both halves used to be bare fields on the emitter, maintained by
//! convention. Registers were three `u16`s where a temp was allocated by
//! bumping a counter and released at eight of the seventy-eight sites that
//! bumped it, so inside one expression the frame only grew. Branches were
//! raw indices into `code`: an emitter pushed `Jump { off: 0 }`, kept the
//! index, and patched it later — which meant the order of "push the
//! fallthrough jump" and "patch the failures" was load-bearing and
//! invisible, and a patch of a non-jump was caught, if at all, by
//! `unreachable!` at runtime.
//!
//! Here both are types. A temp is a [`Scratch`] handed to a scope and
//! released when that scope ends ([`RegAlloc::mark`] /
//! [`RegAlloc::release`], driven by `FnEmitter::with_scratch` and friends);
//! a branch target is a [`Label`], created before it is known where it
//! points, and resolved by [`CodeBuf::finish`], which refuses to finish a
//! body holding a label that was never bound.

use wscript_core::bytecode::Instr;
use wscript_core::span::Span;

use crate::check::LocalId;

// ---------------------------------------------------------------- registers

/// A register of the frame being emitted.
///
/// The frame layout (see the module docs of the emitter) is:
/// `[0 .. n_locals)` locals, then `[n_locals .. n_locals + n_captures)`
/// capture cells, then temps.
///
/// This is a marker, not a capability: the inner `u16` is public because
/// every `Instr` field is a bare `u16`, so a register has to be unwrapped
/// at each instruction the emitter pushes. What it buys is that a register
/// reads as one in a signature — `emit_into(&Expr, Reg)`, `Window::at(i)
/// -> Reg` — where a field index, a capture slot and an argument count are
/// all also `u16`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reg(pub u16);

/// A temp register owned by the scope that allocated it: free to write to,
/// and invalid once that scope releases.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Scratch(Reg);

impl Scratch {
    pub fn reg(self) -> Reg {
        self.0
    }
}

/// A contiguous run of `n` registers — a call's argument window, or the
/// field values of a constructor.
///
/// The VM reads `base .. base + n` as one block, so the run has to be
/// unbroken. That holds by construction here: a window is handed to a
/// scope, and every allocation made inside that scope comes from above it.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    base: Reg,
    n: u16,
}

impl Window {
    pub fn base(self) -> Reg {
        self.base
    }

    /// Slot `i` of the window.
    pub fn at(self, i: u16) -> Reg {
        assert!(i < self.n, "window slot {i} of {}", self.n);
        Reg(self.base.0 + i)
    }

    pub fn len(self) -> u16 {
        self.n
    }
}

/// Where a value landed, and whether the emitter may write over it.
///
/// Named `ValueReg` rather than the more natural "operand" because
/// **Operand** is taken: in this same crate it is the descriptor the
/// checker's operator ladders decide over (`check/ops.rs`), and CONTEXT.md
/// defines it that way.
///
/// `emit_value` returns a *local's own register* for a plain path read
/// rather than copying it into a temp. The register it returns is
/// therefore sometimes scratch the caller owns and sometimes a local it
/// must not clobber — a distinction that used to live in the reader's head
/// at every call site, and which ~20 of them resolved from context.
///
/// Reading is always allowed ([`ValueReg::reg`]). Writing is not: there is
/// no accessor for it, so a caller that wants to write has to match
/// `Owned` and take the [`Scratch`] out — which is exactly the case where
/// writing is sound. Every caller today only reads.
///
/// This records provenance; it does not make a bad write unrepresentable.
/// It cannot: a local is a perfectly legal destination when it *is* the
/// destination (`emit_init_local` writes one), so "may I write here?" is a
/// question about liveness, not about the register's type. What the type
/// removes is the *ambiguity* — the caller is told which kind it got
/// instead of inferring it from the shape of the expression it passed.
#[derive(Clone, Copy, Debug)]
pub enum ValueReg {
    /// A temp allocated for this value.
    Owned(Scratch),
    /// A register the frame owns — a local, or a capture cell.
    Borrowed(Reg),
}

impl ValueReg {
    /// The register to read the value from.
    pub fn reg(self) -> Reg {
        match self {
            ValueReg::Owned(s) => s.reg(),
            ValueReg::Borrowed(r) => r,
        }
    }
}

/// A point in the temp stack to release back to. See [`RegAlloc::mark`].
#[derive(Clone, Copy, Debug)]
pub struct Mark(u16);

/// The frame's register file: fixed slots for locals and capture cells,
/// with temps allocated stack-style above them.
pub struct RegAlloc {
    /// First capture-cell register (`n_locals`).
    cap_base: u16,
    /// First free temp.
    top: u16,
    /// High-water mark: the frame size the VM must allocate for this
    /// function. Every register operand the emitter produces is below it —
    /// the contract `wscript_core::verify` checks.
    high: u16,
}

impl RegAlloc {
    pub fn new(n_locals: u16, n_caps: u16) -> RegAlloc {
        let base = n_locals + n_caps;
        RegAlloc {
            cap_base: n_locals,
            top: base,
            high: base,
        }
    }

    /// A local's register. `LocalId` *is* the frame slot (ADR-0001).
    pub fn local(&self, local: LocalId) -> Reg {
        Reg(local as u16)
    }

    /// The register a closure's capture cell was loaded into by the
    /// prologue.
    pub fn capture(&self, slot: u16) -> Reg {
        Reg(self.cap_base + slot)
    }

    pub fn mark(&self) -> Mark {
        Mark(self.top)
    }

    /// Release every temp allocated since `mark`.
    pub fn release(&mut self, mark: Mark) {
        debug_assert!(
            mark.0 <= self.top,
            "release to {} from a top of {}",
            mark.0,
            self.top
        );
        self.top = mark.0;
    }

    pub fn scratch(&mut self) -> Scratch {
        Scratch(Reg(self.take(1)))
    }

    pub fn window(&mut self, n: u16) -> Window {
        Window {
            base: Reg(self.take(n)),
            n,
        }
    }

    /// The frame size for the emitted proto. At least one register: a
    /// function with no locals still returns something.
    pub fn frame_size(&self) -> u16 {
        self.high.max(1)
    }

    fn take(&mut self, n: u16) -> u16 {
        let base = self.top;
        self.top += n;
        self.high = self.high.max(self.top);
        base
    }
}

// ------------------------------------------------------------------- labels

/// A branch target: created before it is known where it points, bound once
/// that is known, and resolved to an offset by [`CodeBuf::finish`].
///
/// A label may be jumped to any number of times, from before or after the
/// point it is bound, and may be bound only once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Label(u32);

/// The instruction stream for one function body, its spans, and the labels
/// branching through it.
pub struct CodeBuf {
    code: Vec<Instr>,
    spans: Vec<Span>,
    /// The span attributed to the next instruction pushed.
    span: Span,
    /// Position per label; `None` until bound.
    bound: Vec<Option<u32>>,
    /// `(jump instruction, its target)` — resolved by [`CodeBuf::finish`].
    fixups: Vec<(u32, Label)>,
}

impl CodeBuf {
    pub fn new(span: Span) -> CodeBuf {
        CodeBuf {
            code: Vec::new(),
            spans: Vec::new(),
            span,
            bound: Vec::new(),
            fixups: Vec::new(),
        }
    }

    pub fn push(&mut self, i: Instr) {
        self.code.push(i);
        self.spans.push(self.span);
    }

    pub fn span(&self) -> Span {
        self.span
    }

    pub fn set_span(&mut self, span: Span) {
        self.span = span;
    }

    /// A fresh, unbound label.
    pub fn label(&mut self) -> Label {
        self.bound.push(None);
        Label(self.bound.len() as u32 - 1)
    }

    /// Bind `label` to the next instruction pushed.
    pub fn bind(&mut self, label: Label) {
        let at = self.code.len() as u32;
        let slot = &mut self.bound[label.0 as usize];
        assert!(
            slot.is_none(),
            "label {} bound twice ({:?} and {at})",
            label.0,
            slot
        );
        *slot = Some(at);
    }

    /// A label bound here — the target of a backward jump.
    pub fn label_here(&mut self) -> Label {
        let l = self.label();
        self.bind(l);
        l
    }

    pub fn jump(&mut self, label: Label) {
        self.fixup(label);
        self.push(Instr::Jump { off: 0 });
    }

    pub fn jump_if_false(&mut self, cond: Reg, label: Label) {
        self.fixup(label);
        self.push(Instr::JumpIfFalse {
            cond: cond.0,
            off: 0,
        });
    }

    pub fn jump_if_true(&mut self, cond: Reg, label: Label) {
        self.fixup(label);
        self.push(Instr::JumpIfTrue {
            cond: cond.0,
            off: 0,
        });
    }

    /// Resolve every jump and hand over the body.
    ///
    /// Panics if a label was created and never bound. That is a compiler
    /// bug of the same class as the `unreachable!("patching a non-jump")`
    /// this replaces — the emitter cannot produce a meaningful jump for a
    /// target it never decided, and emitting one anyway is how control
    /// silently falls into the wrong block. `name` is the function being
    /// emitted, so the panic says which one.
    pub fn finish(mut self, name: &str) -> (Vec<Instr>, Vec<Span>) {
        for (i, slot) in self.bound.iter().enumerate() {
            assert!(
                slot.is_some(),
                "emitting `{name}`: label {i} was never bound"
            );
        }
        for &(at, label) in &self.fixups {
            let target = self.bound[label.0 as usize].expect("every label bound") as i64;
            let off = (target - (at as i64 + 1)) as i32;
            match &mut self.code[at as usize] {
                Instr::Jump { off: o }
                | Instr::JumpIfFalse { off: o, .. }
                | Instr::JumpIfTrue { off: o, .. } => *o = off,
                // Only the jump emitters above record a fixup.
                other => unreachable!("fixup on {other:?}"),
            }
        }
        (self.code, self.spans)
    }

    /// Record that the instruction about to be pushed targets `label`.
    fn fixup(&mut self, label: Label) {
        self.fixups.push((self.code.len() as u32, label));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf() -> CodeBuf {
        CodeBuf::new(Span::DUMMY)
    }

    /// Offsets are relative to the *next* instruction, forwards and back.
    #[test]
    fn labels_resolve_in_both_directions() {
        let mut c = buf();
        let top = c.label_here();
        let end = c.label();
        c.jump_if_false(Reg(0), end); // pc 0 -> 3
        c.push(Instr::Nop); // pc 1
        c.jump(top); // pc 2 -> 0
        c.bind(end);
        c.push(Instr::RetUnit); // pc 3
        let (code, spans) = c.finish("f");
        assert_eq!(
            code,
            [
                Instr::JumpIfFalse { cond: 0, off: 2 },
                Instr::Nop,
                Instr::Jump { off: -3 },
                Instr::RetUnit,
            ]
        );
        assert_eq!(spans.len(), code.len());
    }

    /// Several jumps may share one target — every `break` in a loop body
    /// does.
    #[test]
    fn one_label_serves_many_jumps() {
        let mut c = buf();
        let end = c.label();
        c.jump(end);
        c.jump(end);
        c.bind(end);
        c.push(Instr::RetUnit);
        let (code, _) = c.finish("f");
        assert_eq!(code[0], Instr::Jump { off: 1 });
        assert_eq!(code[1], Instr::Jump { off: 0 });
    }

    /// The check the type exists for: a target the emitter forgot to
    /// decide is a program whose control flow is wrong, and it is caught
    /// where it happened rather than by the VM later.
    #[test]
    #[should_panic(expected = "label 0 was never bound")]
    fn finishing_with_an_unbound_label_panics() {
        let mut c = buf();
        let end = c.label();
        c.jump(end);
        c.finish("f");
    }

    #[test]
    #[should_panic(expected = "bound twice")]
    fn binding_a_label_twice_panics() {
        let mut c = buf();
        let l = c.label();
        c.bind(l);
        c.push(Instr::Nop);
        c.bind(l);
    }

    /// Temps are released, not merely grown: a sibling scope reuses the
    /// registers its predecessor gave back, while the high-water mark
    /// remembers the deepest the frame ever got.
    #[test]
    fn releasing_a_scope_reuses_its_registers() {
        let mut r = RegAlloc::new(2, 1);
        let mark = r.mark();
        let a = r.scratch();
        let w = r.window(2);
        assert_eq!(a.reg(), Reg(3));
        assert_eq!((w.at(0), w.at(1)), (Reg(4), Reg(5)));
        assert_eq!(r.frame_size(), 6);

        r.release(mark);
        assert_eq!(r.scratch().reg(), Reg(3), "the temp stack unwound");
        assert_eq!(r.frame_size(), 6, "but the frame is sized for the peak");
    }

    #[test]
    fn locals_and_capture_cells_sit_below_the_temps() {
        let mut r = RegAlloc::new(2, 2);
        assert_eq!(r.local(1), Reg(1));
        assert_eq!(r.capture(0), Reg(2));
        assert_eq!(r.capture(1), Reg(3));
        assert_eq!(r.scratch().reg(), Reg(4), "temps start above the cells");
    }

    /// An empty window reserves nothing; the next allocation starts where
    /// it did.
    #[test]
    fn an_empty_window_reserves_nothing() {
        let mut r = RegAlloc::new(1, 0);
        let w = r.window(0);
        assert_eq!(w.len(), 0);
        assert_eq!(w.base(), Reg(1));
        assert_eq!(r.scratch().reg(), Reg(1));
    }

    #[test]
    #[should_panic(expected = "window slot 2 of 2")]
    fn indexing_past_a_window_panics() {
        let mut r = RegAlloc::new(0, 0);
        r.window(2).at(2);
    }
}
