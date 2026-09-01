//! The checker's body-checking state: lexical scopes, function frames,
//! loops, and the type parameters currently in scope.
//!
//! This is a plain data structure — it holds no reference to the checker
//! and touches nothing but its own state, so it is unit-testable on its
//! own. That matters most for capture threading ([`Env::lookup`]), which
//! is the subtlest logic in the checker and was previously observable only
//! by running a script whose output happened to depend on it.
//!
//! Callers enter and leave through the scoped methods on `Checker`
//! (`in_scope`, `in_fn`, `in_loop`, `with_type_params`) rather than
//! calling the push/pop pairs here directly: six manually-balanced stacks
//! with unwritten ordering rules is what this module exists to remove.

use std::collections::{HashMap, HashSet};

use wscript_core::span::Span;
use wscript_core::types::Type;

use super::{BoundKind, CapSrc, LocalId, VarRes};
use crate::ast::Ident;

#[derive(Clone)]
pub(crate) struct Binding {
    pub local: LocalId,
    pub ty: Type,
    /// Definition span (for the LSP's goto-definition).
    pub span: Span,
}

struct Scope {
    bindings: HashMap<String, Binding>,
    /// Index into `fns` of the owning function.
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

/// What a finished function frame contributes back to its `FnInfo`.
pub(crate) struct FnFrame {
    pub n_locals: u32,
    pub captured: HashSet<LocalId>,
    pub captures: Vec<CapSrc>,
}

#[derive(Default)]
pub(crate) struct Env {
    scopes: Vec<Scope>,
    fns: Vec<FnState>,
    /// Rigid type parameters in scope, innermost last. A stack rather than
    /// a single field because they are pushed in two unrelated situations:
    /// while resolving a generic fn's *signature* (no frame yet) and while
    /// checking its *body* (inside a frame). As one mutable field, a
    /// missed clear resolved a name to the wrong function's parameter —
    /// a silent miscompile with no diagnostic.
    type_params: Vec<Vec<(String, Option<BoundKind>)>>,
}

impl Env {
    // ------------------------------------------------------------ frames

    pub(crate) fn push_fn(&mut self, ret: Type) {
        self.fns.push(FnState {
            ret,
            n_locals: 0,
            captured: HashSet::new(),
            captures: Vec::new(),
            capture_map: HashMap::new(),
            loops: Vec::new(),
        });
    }

    pub(crate) fn pop_fn(&mut self) -> FnFrame {
        let state = self.fns.pop().expect("pop_fn without a frame");
        FnFrame {
            n_locals: state.n_locals,
            captured: state.captured,
            captures: state.captures,
        }
    }

    pub(crate) fn current_ret(&self) -> Type {
        self.fns
            .last()
            .map(|s| s.ret.clone())
            .unwrap_or(Type::Error)
    }

    // ------------------------------------------------------------ scopes

    pub(crate) fn push_scope(&mut self) {
        let fn_depth = self
            .fns
            .len()
            .checked_sub(1)
            .expect("push_scope requires an enclosing function frame");
        self.scopes.push(Scope {
            bindings: HashMap::new(),
            fn_depth,
        });
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop().expect("pop_scope without a scope");
    }

    /// Declare a local in the innermost scope.
    ///
    /// Slots are allocated monotonically per function and never reused
    /// across sibling scopes — see ADR-0001. The emitter identifies a
    /// `LocalId` with a register index, and capture tracking assumes one
    /// binding per id.
    pub(crate) fn declare(&mut self, name: &Ident, ty: Type) -> LocalId {
        let state = self.fns.last_mut().expect("declare without a frame");
        let local = state.n_locals;
        state.n_locals += 1;
        self.scopes
            .last_mut()
            .expect("declare without a scope")
            .bindings
            .insert(
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
    /// closures.
    pub(crate) fn lookup(&mut self, name: &str) -> Option<(VarRes, Type)> {
        let current_depth = self.fns.len().checked_sub(1)?;
        // Innermost scope first.
        let (owner_depth, binding) = self.scopes.iter().rev().find_map(|scope| {
            scope
                .bindings
                .get(name)
                .map(|b| (scope.fn_depth, b.clone()))
        })?;
        if owner_depth == current_depth {
            return Some((VarRes::Local(binding.local), binding.ty));
        }
        // Captured: mark the local in its owner and thread capture slots
        // through every closure between owner and current.
        self.fns[owner_depth].captured.insert(binding.local);
        let mut src = CapSrc::Local(binding.local);
        let mut slot = 0u16;
        for depth in (owner_depth + 1)..=current_depth {
            let key = (owner_depth, binding.local);
            let state = &mut self.fns[depth];
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

    /// Span of a local's definition (for the LSP's goto-definition).
    pub(crate) fn lookup_span(&self, name: &str) -> Option<Span> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name).map(|b| b.span))
    }

    // ------------------------------------------------------------- loops

    pub(crate) fn enter_loop(&mut self) {
        if let Some(state) = self.fns.last_mut() {
            state.loops.push(LoopCtx { has_break: false });
        }
    }

    /// Returns whether the loop contained a `break`.
    pub(crate) fn exit_loop(&mut self) -> bool {
        self.fns
            .last_mut()
            .and_then(|s| s.loops.pop())
            .map(|l| l.has_break)
            .unwrap_or(false)
    }

    /// Records a `break` against the innermost loop; `false` when there is
    /// none (the caller reports E0221).
    pub(crate) fn mark_break(&mut self) -> bool {
        match self.fns.last_mut().and_then(|s| s.loops.last_mut()) {
            Some(l) => {
                l.has_break = true;
                true
            }
            None => false,
        }
    }

    pub(crate) fn inside_loop(&self) -> bool {
        self.fns.last().is_some_and(|s| !s.loops.is_empty())
    }

    // --------------------------------------------------- type parameters

    pub(crate) fn push_type_params(&mut self, params: Vec<(String, Option<BoundKind>)>) {
        self.type_params.push(params);
    }

    pub(crate) fn pop_type_params(&mut self) {
        self.type_params.pop();
    }

    /// The rigid type parameters currently in scope. `Type::Param(i)`
    /// indexes this slice.
    pub(crate) fn type_params(&self) -> &[(String, Option<BoundKind>)] {
        self.type_params.last().map(Vec::as_slice).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(name: &str) -> Ident {
        Ident {
            name: name.to_string(),
            span: Span::DUMMY,
        }
    }

    /// A frame plus a scope, the state every body-checking method assumes.
    fn env() -> Env {
        let mut env = Env::default();
        env.push_fn(Type::Unit);
        env.push_scope();
        env
    }

    #[test]
    fn locals_are_numbered_monotonically_and_never_reused() {
        let mut env = env();
        assert_eq!(env.declare(&ident("a"), Type::Int), 0);
        env.push_scope();
        assert_eq!(env.declare(&ident("b"), Type::Int), 1);
        env.pop_scope();
        // A sibling scope gets a fresh slot, not b's — see ADR-0001.
        env.push_scope();
        assert_eq!(env.declare(&ident("c"), Type::Int), 2);
    }

    #[test]
    fn inner_scope_shadows_outer() {
        let mut env = env();
        env.declare(&ident("x"), Type::Int);
        env.push_scope();
        env.declare(&ident("x"), Type::Str);
        assert!(matches!(
            env.lookup("x"),
            Some((VarRes::Local(1), Type::Str))
        ));
        env.pop_scope();
        assert!(matches!(
            env.lookup("x"),
            Some((VarRes::Local(0), Type::Int))
        ));
    }

    #[test]
    fn a_name_out_of_scope_does_not_resolve() {
        let mut env = env();
        env.push_scope();
        env.declare(&ident("inner"), Type::Int);
        env.pop_scope();
        assert!(env.lookup("inner").is_none());
    }

    #[test]
    fn one_level_capture_marks_the_owner_and_allocates_a_slot() {
        let mut env = env();
        env.declare(&ident("x"), Type::Int);
        env.push_fn(Type::Int); // a closure
        env.push_scope();

        assert!(matches!(
            env.lookup("x"),
            Some((VarRes::Capture(0), Type::Int))
        ));

        let closure = env.pop_fn();
        assert_eq!(closure.captures.len(), 1);
        assert!(matches!(closure.captures[0], CapSrc::Local(0)));
        let outer = env.pop_fn();
        assert!(outer.captured.contains(&0), "owner must box the local");
    }

    #[test]
    fn repeated_capture_of_one_local_reuses_its_slot() {
        let mut env = env();
        env.declare(&ident("x"), Type::Int);
        env.push_fn(Type::Int);
        env.push_scope();

        assert!(matches!(env.lookup("x"), Some((VarRes::Capture(0), _))));
        assert!(matches!(env.lookup("x"), Some((VarRes::Capture(0), _))));

        assert_eq!(env.pop_fn().captures.len(), 1, "slot must be deduped");
    }

    #[test]
    fn two_locals_captured_get_distinct_slots() {
        let mut env = env();
        env.declare(&ident("x"), Type::Int);
        env.declare(&ident("y"), Type::Str);
        env.push_fn(Type::Int);
        env.push_scope();

        assert!(matches!(env.lookup("x"), Some((VarRes::Capture(0), _))));
        assert!(matches!(env.lookup("y"), Some((VarRes::Capture(1), _))));
        assert_eq!(env.pop_fn().captures.len(), 2);
    }

    /// The threading loop: a local of the outermost function referenced
    /// from two closures deep must be captured by the middle closure too,
    /// and the inner one must source it from that slot rather than from
    /// the original local.
    #[test]
    fn nested_capture_threads_through_the_middle_closure() {
        let mut env = env();
        env.declare(&ident("x"), Type::Int);
        env.push_fn(Type::Int); // middle
        env.push_scope();
        env.push_fn(Type::Int); // inner
        env.push_scope();

        assert!(matches!(env.lookup("x"), Some((VarRes::Capture(0), _))));

        let inner = env.pop_fn();
        assert!(
            matches!(inner.captures[0], CapSrc::Capture(0)),
            "inner must read the middle closure's slot, not the local"
        );
        env.pop_scope();
        let middle = env.pop_fn();
        assert!(
            matches!(middle.captures[0], CapSrc::Local(0)),
            "middle must capture the original local"
        );
        env.pop_scope();
        assert!(env.pop_fn().captured.contains(&0));
    }

    #[test]
    fn a_local_of_the_current_function_is_not_a_capture() {
        let mut env = env();
        env.push_fn(Type::Int);
        env.push_scope();
        env.declare(&ident("own"), Type::Int);
        assert!(matches!(env.lookup("own"), Some((VarRes::Local(0), _))));
        assert!(env.pop_fn().captures.is_empty());
    }

    #[test]
    fn loops_track_break_per_frame() {
        let mut env = env();
        assert!(!env.inside_loop());
        assert!(!env.mark_break(), "break outside a loop is rejected");

        env.enter_loop();
        assert!(env.inside_loop());
        assert!(env.mark_break());
        assert!(env.exit_loop(), "loop contained a break");

        env.enter_loop();
        assert!(!env.exit_loop(), "loop without a break");
        assert!(!env.inside_loop());
    }

    #[test]
    fn a_closure_does_not_inherit_the_enclosing_loop() {
        let mut env = env();
        env.enter_loop();
        env.push_fn(Type::Int);
        assert!(!env.inside_loop(), "`break` must not cross a closure");
        env.pop_fn();
        assert!(env.inside_loop());
    }

    #[test]
    fn type_params_nest_and_restore() {
        let mut env = Env::default();
        assert!(env.type_params().is_empty());
        env.push_type_params(vec![("T".into(), None)]);
        assert_eq!(env.type_params().len(), 1);
        env.push_type_params(vec![("A".into(), None), ("B".into(), None)]);
        assert_eq!(env.type_params().len(), 2);
        env.pop_type_params();
        assert_eq!(env.type_params()[0].0, "T");
        env.pop_type_params();
        assert!(env.type_params().is_empty());
    }

    #[test]
    fn ret_type_follows_the_innermost_frame() {
        let mut env = Env::default();
        assert_eq!(env.current_ret(), Type::Error, "no frame");
        env.push_fn(Type::Int);
        assert_eq!(env.current_ret(), Type::Int);
        env.push_fn(Type::Str);
        assert_eq!(env.current_ret(), Type::Str);
        env.pop_fn();
        assert_eq!(env.current_ret(), Type::Int);
    }
}
