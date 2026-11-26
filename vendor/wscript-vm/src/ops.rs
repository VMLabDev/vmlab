//! Structural value operations: equality, ordering, deep clone, display.
//!
//! These descend values structurally; when they reach a struct/enum whose
//! type has a *custom* (non-derived) `Eq`/`Ord`/`Display` impl, they call
//! it through `CompiledUnit::impls`. Derived impls are pure structure.
//!
//! Two invariants govern every arm here:
//!
//! 1. **Depth is bounded.** Values are freely aliasable `Rc<RefCell<...>>`
//!    graphs, so scripts can build cyclic (or just very deep) data. Every
//!    recursive walk threads a depth counter and faults past
//!    [`MAX_VALUE_DEPTH`] — a trappable [`RuntimeError`], never a native
//!    stack overflow. The `Rc::ptr_eq` fast paths keep `x == x` true for
//!    cyclic values without recursing.
//!
//! 2. **No `RefCell` borrow is held across script re-entry.** `value_eq`,
//!    `value_cmp` and `fmt_value` can call custom impls, which run script
//!    code that may mutate the very collection being walked — so they use
//!    per-element access (borrow, clone one element, drop the borrow)
//!    instead of holding a borrow or snapshotting whole collections.
//!    `deep_clone` never runs script code (Clone is derive-only, PRD §3.8)
//!    and may hold shared borrows across its own recursion.

use std::cmp::Ordering;
use std::rc::Rc;

use wscript_core::defs::{DefId, DefKind, VariantKind};
use wscript_core::value::{Key, MAX_VALUE_DEPTH, Value};

use crate::{RuntimeError, Vm};

/// Shared fault message prefix for all depth-limit faults (asserted by
/// script tests).
const DEPTH_FAULT: &str = "value nesting too deep (cyclic value?)";

/// Clone one element pair out of two lists/field-vectors under a scoped
/// borrow. `Ok(None)` = both ended; `Err(())` = lengths diverged (a custom
/// impl mutated an operand mid-walk — callers treat that as unequal).
fn pair_at(
    x: &std::cell::RefCell<Vec<Value>>,
    y: &std::cell::RefCell<Vec<Value>>,
    i: usize,
) -> Result<Option<(Value, Value)>, ()> {
    let (xb, yb) = (x.borrow(), y.borrow());
    match (xb.get(i), yb.get(i)) {
        (Some(a), Some(b)) => Ok(Some((a.clone(), b.clone()))),
        (None, None) => Ok(None),
        _ => Err(()),
    }
}

impl Vm {
    pub(crate) fn value_eq(&mut self, a: &Value, b: &Value) -> Result<bool, RuntimeError> {
        self.value_eq_at(a, b, 0)
    }

    fn value_eq_at(&mut self, a: &Value, b: &Value, depth: usize) -> Result<bool, RuntimeError> {
        if depth >= MAX_VALUE_DEPTH {
            return Err(self.fault(DEPTH_FAULT));
        }
        self.charge_structural()?;
        Ok(match (a, b) {
            (Value::Unit, Value::Unit) => true,
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Char(x), Value::Char(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::List(x), Value::List(y)) => {
                if Rc::ptr_eq(x, y) {
                    return Ok(true);
                }
                if x.borrow().len() != y.borrow().len() {
                    return Ok(false);
                }
                let mut i = 0;
                loop {
                    let Ok(pair) = pair_at(x, y, i) else {
                        return Ok(false);
                    };
                    let Some((xv, yv)) = pair else { break };
                    if !self.value_eq_at(&xv, &yv, depth + 1)? {
                        return Ok(false);
                    }
                    i += 1;
                }
                true
            }
            (Value::Map(x), Value::Map(y)) => {
                if Rc::ptr_eq(x, y) {
                    return Ok(true);
                }
                if x.borrow().len() != y.borrow().len() {
                    return Ok(false);
                }
                // Keys are cheap (prims / Rc<str>); values are fetched one
                // at a time so no borrow is held across recursion.
                let keys: Vec<Key> = x.borrow().keys().cloned().collect();
                for k in &keys {
                    let pair = {
                        let (xb, yb) = (x.borrow(), y.borrow());
                        match (xb.get(k), yb.get(k)) {
                            (Some(xv), Some(yv)) => (xv.clone(), yv.clone()),
                            _ => return Ok(false),
                        }
                    };
                    if !self.value_eq_at(&pair.0, &pair.1, depth + 1)? {
                        return Ok(false);
                    }
                }
                true
            }
            (Value::Struct(x), Value::Struct(y)) => {
                if x.def != y.def {
                    return Ok(false);
                }
                if Rc::ptr_eq(x, y) {
                    return Ok(true);
                }
                if let Some(&proto) = self.unit_impls().eq.get(&x.def.0) {
                    let r = self.call_proto_nested(
                        proto,
                        vec![Value::Struct(x.clone()), Value::Struct(y.clone())],
                    )?;
                    return match r {
                        Value::Bool(b) => Ok(b),
                        _ => Err(self.fault("custom Eq impl did not return bool")),
                    };
                }
                let mut i = 0;
                loop {
                    let Ok(pair) = pair_at(&x.fields, &y.fields, i) else {
                        return Ok(false);
                    };
                    let Some((xv, yv)) = pair else { break };
                    if !self.value_eq_at(&xv, &yv, depth + 1)? {
                        return Ok(false);
                    }
                    i += 1;
                }
                true
            }
            (Value::Enum(x), Value::Enum(y)) => {
                if x.def != y.def || x.tag != y.tag {
                    return Ok(false);
                }
                if Rc::ptr_eq(x, y) {
                    return Ok(true);
                }
                if let Some(&proto) = self.unit_impls().eq.get(&x.def.0) {
                    let r = self.call_proto_nested(
                        proto,
                        vec![Value::Enum(x.clone()), Value::Enum(y.clone())],
                    )?;
                    return match r {
                        Value::Bool(b) => Ok(b),
                        _ => Err(self.fault("custom Eq impl did not return bool")),
                    };
                }
                let mut i = 0;
                loop {
                    let Ok(pair) = pair_at(&x.fields, &y.fields, i) else {
                        return Ok(false);
                    };
                    let Some((xv, yv)) = pair else { break };
                    if !self.value_eq_at(&xv, &yv, depth + 1)? {
                        return Ok(false);
                    }
                    i += 1;
                }
                true
            }
            (Value::Dyn(x), Value::Dyn(y)) => {
                let (ix, iy) = (x.inner.clone(), y.inner.clone());
                self.value_eq_at(&ix, &iy, depth + 1)?
            }
            (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
            (Value::Opaque(x), Value::Opaque(y)) => Rc::ptr_eq(x, y),
            _ => false,
        })
    }

    /// Three-way comparison: -1 / 0 / 1.
    pub(crate) fn value_cmp(&mut self, a: &Value, b: &Value) -> Result<i64, RuntimeError> {
        self.value_cmp_at(a, b, 0)
    }

    fn value_cmp_at(&mut self, a: &Value, b: &Value, depth: usize) -> Result<i64, RuntimeError> {
        if depth >= MAX_VALUE_DEPTH {
            return Err(self.fault(DEPTH_FAULT));
        }
        self.charge_structural()?;
        let ord = match (a, b) {
            (Value::Int(x), Value::Int(y)) => x.cmp(y),
            (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            (Value::Char(x), Value::Char(y)) => x.cmp(y),
            (Value::Str(x), Value::Str(y)) => x.cmp(y),
            (Value::Unit, Value::Unit) => Ordering::Equal,
            (Value::List(x), Value::List(y)) => {
                if Rc::ptr_eq(x, y) {
                    return Ok(0);
                }
                let mut i = 0;
                loop {
                    // Lexicographic; lengths re-read per element so a
                    // mutation mid-compare gives an unspecified result,
                    // never a panic.
                    let pair = {
                        let (xb, yb) = (x.borrow(), y.borrow());
                        match (xb.get(i), yb.get(i)) {
                            (Some(xv), Some(yv)) => (xv.clone(), yv.clone()),
                            (None, None) => return Ok(0),
                            (None, Some(_)) => return Ok(-1),
                            (Some(_), None) => return Ok(1),
                        }
                    };
                    let c = self.value_cmp_at(&pair.0, &pair.1, depth + 1)?;
                    if c != 0 {
                        return Ok(c);
                    }
                    i += 1;
                }
            }
            (Value::Struct(x), Value::Struct(y)) if x.def == y.def => {
                if let Some(&proto) = self.unit_impls().cmp.get(&x.def.0) {
                    let r = self.call_proto_nested(
                        proto,
                        vec![Value::Struct(x.clone()), Value::Struct(y.clone())],
                    )?;
                    return match r {
                        Value::Int(n) => Ok(n.signum()),
                        _ => Err(self.fault("custom Ord impl did not return int")),
                    };
                }
                // Derived path only: identity implies equality under a
                // total structural order (a custom impl must still run).
                if Rc::ptr_eq(x, y) {
                    return Ok(0);
                }
                let mut i = 0;
                loop {
                    let Ok(pair) = pair_at(&x.fields, &y.fields, i) else {
                        return Ok(0);
                    };
                    let Some((xv, yv)) = pair else {
                        return Ok(0);
                    };
                    let c = self.value_cmp_at(&xv, &yv, depth + 1)?;
                    if c != 0 {
                        return Ok(c);
                    }
                    i += 1;
                }
            }
            (Value::Enum(x), Value::Enum(y)) if x.def == y.def => {
                if let Some(&proto) = self.unit_impls().cmp.get(&x.def.0) {
                    let r = self.call_proto_nested(
                        proto,
                        vec![Value::Enum(x.clone()), Value::Enum(y.clone())],
                    )?;
                    return match r {
                        Value::Int(n) => Ok(n.signum()),
                        _ => Err(self.fault("custom Ord impl did not return int")),
                    };
                }
                match x.tag.cmp(&y.tag) {
                    Ordering::Equal => {
                        if Rc::ptr_eq(x, y) {
                            return Ok(0);
                        }
                        let mut i = 0;
                        loop {
                            let Ok(pair) = pair_at(&x.fields, &y.fields, i) else {
                                return Ok(0);
                            };
                            let Some((xv, yv)) = pair else {
                                return Ok(0);
                            };
                            let c = self.value_cmp_at(&xv, &yv, depth + 1)?;
                            if c != 0 {
                                return Ok(c);
                            }
                            i += 1;
                        }
                    }
                    other => other,
                }
            }
            _ => {
                return Err(self.fault(format!(
                    "cannot order {} and {}",
                    a.kind_name(),
                    b.kind_name()
                )));
            }
        };
        Ok(match ord {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        })
    }

    /// Deep clone (derive `Clone`, PRD §3.8). Structs/enums/lists/maps are
    /// recreated recursively; strings are immutable (shared); functions,
    /// opaque handles, weak refs and dyn values copy the reference.
    pub(crate) fn deep_clone(&mut self, v: &Value) -> Result<Value, RuntimeError> {
        self.deep_clone_at(v, 0)
    }

    // INVARIANT: deep_clone must never call script code (Clone is
    // derive-only). That is what makes the held borrows below sound —
    // nothing can mutate the collection mid-walk. If custom Clone impls
    // are ever added, this must switch to per-element access like
    // value_eq_at.
    fn deep_clone_at(&mut self, v: &Value, depth: usize) -> Result<Value, RuntimeError> {
        if depth >= MAX_VALUE_DEPTH {
            return Err(self.fault(DEPTH_FAULT));
        }
        self.charge_structural()?;
        Ok(match v {
            Value::List(items) => {
                let items = items.borrow();
                let mut out = Vec::with_capacity(items.len());
                for x in items.iter() {
                    out.push(self.deep_clone_at(x, depth + 1)?);
                }
                drop(items);
                Value::new_list(out)
            }
            Value::Map(entries) => {
                let entries = entries.borrow();
                let mut out = std::collections::BTreeMap::new();
                for (k, x) in entries.iter() {
                    out.insert(k.clone(), self.deep_clone_at(x, depth + 1)?);
                }
                drop(entries);
                Value::new_map(out)
            }
            Value::Struct(s) => {
                let fields = s.fields.borrow();
                let mut out = Vec::with_capacity(fields.len());
                for x in fields.iter() {
                    out.push(self.deep_clone_at(x, depth + 1)?);
                }
                drop(fields);
                Value::new_struct(s.def, out)
            }
            Value::Enum(e) => {
                let fields = e.fields.borrow();
                let mut out = Vec::with_capacity(fields.len());
                for x in fields.iter() {
                    out.push(self.deep_clone_at(x, depth + 1)?);
                }
                drop(fields);
                Value::new_enum(e.def, e.tag, out)
            }
            other => other.clone(),
        })
    }

    /// Render a unit-family value: the raw base-unit count paired with the
    /// largest unit that names it cleanly (`1500000000` in a nanosecond-based
    /// `Duration` → `1.5s`).
    ///
    /// Int-backed families only use a unit that divides the value exactly,
    /// so the rendering always round-trips; float-backed ones take the
    /// largest unit the value reaches.
    pub(crate) fn fmt_quantity(&mut self, v: &Value, def: DefId) -> Result<String, RuntimeError> {
        match self.unit_defs().as_unit(def).and_then(|u| u.render(v)) {
            Some(s) => Ok(s),
            // Not a unit family, or not its backing primitive: the emitter
            // only asks for this where the static type said otherwise, so
            // fall back rather than fault.
            None => self.display_value(v),
        }
    }

    /// Display with custom `Display` impls (used by print/str/fmt).
    pub(crate) fn display_value(&mut self, v: &Value) -> Result<String, RuntimeError> {
        let mut out = String::new();
        self.fmt_value(v, &mut out, false, 0)?;
        Ok(out)
    }

    fn fmt_value(
        &mut self,
        v: &Value,
        out: &mut String,
        nested: bool,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        if depth >= MAX_VALUE_DEPTH {
            return Err(self.fault(DEPTH_FAULT));
        }
        self.charge_structural()?;
        // Custom Display impls take priority for nominal types.
        let custom = match v {
            Value::Struct(s) => self.unit_impls().display.get(&s.def.0).copied(),
            Value::Enum(e) => self.unit_impls().display.get(&e.def.0).copied(),
            Value::Dyn(d) => {
                let inner = d.inner.clone();
                return self.fmt_value(&inner, out, nested, depth + 1);
            }
            _ => None,
        };
        if let Some(proto) = custom {
            let r = self.call_proto_nested(proto, vec![v.clone()])?;
            return match r {
                Value::Str(s) => {
                    out.push_str(&s);
                    Ok(())
                }
                _ => Err(self.fault("custom Display impl did not return string")),
            };
        }
        match v {
            Value::List(items) => {
                out.push('[');
                let mut i = 0;
                loop {
                    // Per-element access: a custom Display on an element
                    // may mutate the list being printed.
                    let item = { items.borrow().get(i).cloned() };
                    let Some(x) = item else { break };
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.fmt_value(&x, out, true, depth + 1)?;
                    i += 1;
                }
                out.push(']');
                Ok(())
            }
            Value::Map(entries) => {
                out.push_str("#{");
                let keys: Vec<Key> = entries.borrow().keys().cloned().collect();
                let mut first = true;
                for k in &keys {
                    let item = { entries.borrow().get(k).cloned() };
                    let Some(x) = item else { continue };
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    self.fmt_value(&k.to_value(), out, true, depth + 1)?;
                    out.push_str(": ");
                    self.fmt_value(&x, out, true, depth + 1)?;
                }
                out.push('}');
                Ok(())
            }
            Value::Struct(s) => {
                let (name, field_names) = {
                    let defs = self.unit_defs();
                    let name = defs.name_of(s.def).to_string();
                    let names: Vec<String> = defs
                        .as_struct(s.def)
                        .map(|sd| sd.fields.iter().map(|(n, _)| n.clone()).collect())
                        .unwrap_or_default();
                    (name, names)
                };
                let opaque = matches!(
                    self.unit_defs().get(s.def),
                    DefKind::Struct(sd) if sd.opaque
                );
                out.push_str(&name);
                if opaque {
                    out.push_str(" { <opaque> }");
                    return Ok(());
                }
                out.push_str(" { ");
                let mut i = 0;
                loop {
                    let item = { s.fields.borrow().get(i).cloned() };
                    let Some(x) = item else { break };
                    if i > 0 {
                        out.push_str(", ");
                    }
                    if let Some(n) = field_names.get(i) {
                        out.push_str(n);
                        out.push_str(": ");
                    }
                    self.fmt_value(&x, out, true, depth + 1)?;
                    i += 1;
                }
                out.push_str(" }");
                Ok(())
            }
            Value::Enum(e) => {
                let (enum_name, vname, kind, field_names) = {
                    let defs = self.unit_defs();
                    let enum_name = defs.name_of(e.def).to_string();
                    let (vname, kind, names) = defs
                        .as_enum(e.def)
                        .and_then(|ed| ed.variants.get(e.tag as usize))
                        .map(|vd| {
                            (
                                vd.name.clone(),
                                vd.kind,
                                vd.fields.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                            )
                        })
                        .unwrap_or((format!("<tag {}>", e.tag), VariantKind::Tuple, vec![]));
                    (enum_name, vname, kind, names)
                };
                if enum_name != "Option" && enum_name != "Result" {
                    out.push_str(&enum_name);
                    out.push_str("::");
                }
                out.push_str(&vname);
                match kind {
                    VariantKind::Unit => {}
                    VariantKind::Tuple => {
                        out.push('(');
                        let mut i = 0;
                        loop {
                            let item = { e.fields.borrow().get(i).cloned() };
                            let Some(x) = item else { break };
                            if i > 0 {
                                out.push_str(", ");
                            }
                            self.fmt_value(&x, out, true, depth + 1)?;
                            i += 1;
                        }
                        out.push(')');
                    }
                    VariantKind::Struct => {
                        out.push_str(" { ");
                        let mut i = 0;
                        loop {
                            let item = { e.fields.borrow().get(i).cloned() };
                            let Some(x) = item else { break };
                            if i > 0 {
                                out.push_str(", ");
                            }
                            if let Some(n) = field_names.get(i) {
                                out.push_str(n);
                                out.push_str(": ");
                            }
                            self.fmt_value(&x, out, true, depth + 1)?;
                            i += 1;
                        }
                        out.push_str(" }");
                    }
                }
                Ok(())
            }
            other => {
                out.push_str(&other_display(other, self, nested));
                Ok(())
            }
        }
    }
}

fn other_display(v: &Value, vm: &Vm, nested: bool) -> String {
    // Primitives and remaining kinds: reuse the structural renderer.
    let defs = vm.unit_defs();
    let mut s = v.display(defs);
    if nested && matches!(v, Value::Str(_) | Value::Char(_)) {
        s = match v {
            Value::Str(x) => format!("{x:?}"),
            Value::Char(c) => format!("{c:?}"),
            _ => s,
        };
    }
    s
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use wscript_core::registry::Registry;
    use wscript_core::value::{MAX_VALUE_DEPTH, Value};

    use crate::{Vm, VmConfig};

    /// A bare VM. No unit needs loading: `unit_defs`/`unit_impls` fall
    /// back to the registry, so structural ops are safe on a fresh `Vm`.
    fn test_vm() -> Vm {
        Vm::new(&Registry::new(), VmConfig::default())
    }

    /// A VM plus values produced by running script functions — the way to
    /// obtain struct instances / custom-impl types for direct op tests.
    fn vm_with(src: &str) -> (Vm, wscript_core::bytecode::CompiledUnit) {
        let registry = Registry::new();
        let compiled = wscript_compiler::compile(src, &registry).unwrap_or_else(|d| {
            panic!("test script failed to compile: {d:?}");
        });
        let mut vm = Vm::new(&registry, VmConfig::default());
        vm.load(&compiled.unit);
        (vm, compiled.unit)
    }

    /// `l = [l]` — a self-containing list, built host-side (no compiler).
    fn cyclic_list() -> Value {
        let l = Value::new_list(vec![]);
        if let Value::List(rc) = &l {
            rc.borrow_mut().push(l.clone());
        }
        l
    }

    fn nested_list(depth: usize) -> Value {
        let mut v = Value::new_list(vec![Value::Int(1)]);
        for _ in 0..depth {
            v = Value::new_list(vec![v]);
        }
        v
    }

    // ------------------------------------------------ cycle / depth

    #[test]
    fn eq_distinct_cyclic_lists_fault() {
        let mut vm = test_vm();
        let (a, b) = (cyclic_list(), cyclic_list());
        let err = vm.value_eq(&a, &b).unwrap_err();
        assert!(err.message.contains("nesting too deep"), "{}", err.message);
    }

    #[test]
    fn eq_same_cyclic_value_is_true() {
        let mut vm = test_vm();
        let a = cyclic_list();
        assert!(vm.value_eq(&a, &a.clone()).unwrap());
    }

    #[test]
    fn cmp_cyclic_lists_fault() {
        let mut vm = test_vm();
        let (a, b) = (cyclic_list(), cyclic_list());
        let err = vm.value_cmp(&a, &b).unwrap_err();
        assert!(err.message.contains("nesting too deep"), "{}", err.message);
    }

    #[test]
    fn cmp_same_cyclic_list_is_equal() {
        let mut vm = test_vm();
        let a = cyclic_list();
        assert_eq!(vm.value_cmp(&a, &a.clone()).unwrap(), 0);
    }

    #[test]
    fn deep_clone_cyclic_list_faults() {
        let mut vm = test_vm();
        let err = vm.deep_clone(&cyclic_list()).unwrap_err();
        assert!(err.message.contains("nesting too deep"), "{}", err.message);
    }

    #[test]
    fn display_cyclic_list_faults() {
        let mut vm = test_vm();
        let err = vm.display_value(&cyclic_list()).unwrap_err();
        assert!(err.message.contains("nesting too deep"), "{}", err.message);
    }

    #[test]
    fn deep_acyclic_data_faults_not_aborts() {
        // Depth chosen well past MAX_VALUE_DEPTH but shallow enough that
        // *dropping* the value doesn't overflow: Value's Rc drop recurses
        // (a separate, known limitation — the ops guard walks, not drops).
        let mut vm = test_vm();
        let v = nested_list(2_000);
        assert!(vm.value_eq(&v, &nested_list(2_000)).is_err());
        assert!(vm.value_cmp(&v, &nested_list(2_000)).is_err());
        assert!(vm.deep_clone(&v).is_err());
        assert!(vm.display_value(&v).is_err());
    }

    #[test]
    fn nesting_just_under_limit_ok() {
        let mut vm = test_vm();
        // Each list level consumes one depth unit; leave slack for the
        // innermost Int comparison.
        let d = MAX_VALUE_DEPTH - 2;
        let (a, b) = (nested_list(d), nested_list(d));
        assert!(vm.value_eq(&a, &b).unwrap());
        assert_eq!(vm.value_cmp(&a, &b).unwrap(), 0);
        assert!(vm.deep_clone(&a).is_ok());
        assert!(vm.display_value(&a).is_ok());
    }

    // ------------------------------------- clone-elimination regressions

    #[test]
    fn eq_nested_lists_by_value() {
        let mut vm = test_vm();
        let mk = |tail: i64| {
            Value::new_list(vec![
                Value::Int(1),
                Value::new_list(vec![Value::Int(2), Value::Int(tail)]),
            ])
        };
        assert!(vm.value_eq(&mk(3), &mk(3)).unwrap());
        assert!(!vm.value_eq(&mk(3), &mk(4)).unwrap());
        let short = Value::new_list(vec![Value::Int(1)]);
        assert!(!vm.value_eq(&mk(3), &short).unwrap());
    }

    #[test]
    fn eq_maps_by_value() {
        use std::collections::BTreeMap;
        use wscript_core::value::Key;
        let mut vm = test_vm();
        let mk = |k: &str, v: i64| {
            let mut m = BTreeMap::new();
            m.insert(Key::Str(Rc::from(k)), Value::Int(v));
            Value::new_map(m)
        };
        assert!(vm.value_eq(&mk("a", 1), &mk("a", 1)).unwrap());
        assert!(!vm.value_eq(&mk("a", 1), &mk("b", 1)).unwrap());
        assert!(!vm.value_eq(&mk("a", 1), &mk("a", 2)).unwrap());
        let empty = Value::new_map(BTreeMap::new());
        assert!(vm.value_eq(&empty, &empty.clone()).unwrap());
        assert!(!vm.value_eq(&mk("a", 1), &empty).unwrap());
    }

    #[test]
    fn cmp_lists_lexicographic() {
        let mut vm = test_vm();
        let l = |xs: &[i64]| Value::new_list(xs.iter().map(|&n| Value::Int(n)).collect());
        assert_eq!(vm.value_cmp(&l(&[1, 2]), &l(&[1, 3])).unwrap(), -1);
        assert_eq!(vm.value_cmp(&l(&[1]), &l(&[1, 0])).unwrap(), -1);
        assert_eq!(vm.value_cmp(&l(&[1, 0]), &l(&[1])).unwrap(), 1);
        assert_eq!(vm.value_cmp(&l(&[1, 2]), &l(&[1, 2])).unwrap(), 0);
        let err = vm
            .value_cmp(&Value::Int(1), &Value::Str(Rc::from("x")))
            .unwrap_err();
        assert!(err.message.contains("cannot order"), "{}", err.message);
    }

    #[test]
    fn deep_clone_is_deep_and_disjoint() {
        let mut vm = test_vm();
        let inner = Value::new_list(vec![Value::Int(1), Value::Int(2)]);
        let outer = Value::new_list(vec![inner.clone()]);
        let cloned = vm.deep_clone(&outer).unwrap();
        if let Value::List(items) = &inner {
            items.borrow_mut().push(Value::Int(3));
        }
        // The clone must not see the mutation, and must not alias.
        assert!(!vm.value_eq(&outer, &cloned).unwrap());
        assert!(!outer.same(&cloned));
        let (Value::List(o), Value::List(c)) = (&outer, &cloned) else {
            unreachable!()
        };
        assert!(!o.borrow()[0].same(&c.borrow()[0]));
    }

    #[test]
    fn deep_clone_shares_strings() {
        let mut vm = test_vm();
        let s = Value::Str(Rc::from("shared"));
        let list = Value::new_list(vec![s.clone()]);
        let cloned = vm.deep_clone(&list).unwrap();
        let Value::List(c) = &cloned else {
            unreachable!()
        };
        // Strings are immutable — the clone shares the Rc.
        assert!(s.same(&c.borrow()[0]));
    }

    #[test]
    fn fuel_charged_by_structural_eq() {
        let mut vm = test_vm();
        let big = |n: i64| Value::new_list((0..10_000).map(|i| Value::Int(i + n)).collect());
        vm.set_fuel(Some(50));
        let err = vm.value_eq(&big(0), &big(0)).unwrap_err();
        assert_eq!(err.message, "fuel exhausted");
        assert_eq!(vm.fuel(), Some(0));
        // Unmetered: the same comparison completes.
        vm.set_fuel(None);
        assert!(vm.value_eq(&big(0), &big(0)).unwrap());
    }

    // ------------------------- mutation during custom impls (no panics)

    #[test]
    fn mutation_during_custom_eq_no_panic() {
        // The custom Eq impl pops from the very list being compared —
        // the regression the snapshot removal must survive without a
        // RefCell panic. The result is unspecified; not panicking is
        // the contract.
        let src = r#"
struct S { owner: List[S] }
impl Eq for S {
    fn eq(self, other: S) -> bool {
        self.owner.pop()
        true
    }
}
fn mk() -> List[S] {
    let l: List[S] = []
    l.push(S { owner: l })
    l.push(S { owner: l })
    l
}
"#;
        let (mut vm, unit) = vm_with(src);
        let a = vm.call_name(&unit, "mk", vec![]).unwrap();
        let b = vm.call_name(&unit, "mk", vec![]).unwrap();
        let _ = vm.value_eq(&a, &b); // Ok or Err — must not panic.
    }

    #[test]
    fn mutation_during_custom_display_no_panic() {
        let src = r#"
struct D { owner: List[D] }
impl Display for D {
    fn fmt(self) -> string {
        self.owner.pop()
        "d"
    }
}
fn mk() -> List[D] {
    let l: List[D] = []
    l.push(D { owner: l })
    l.push(D { owner: l })
    l
}
"#;
        let (mut vm, unit) = vm_with(src);
        let l = vm.call_name(&unit, "mk", vec![]).unwrap();
        let _ = vm.display_value(&l); // Ok or Err — must not panic.
    }

    #[test]
    fn custom_eq_still_called_for_distinct_values() {
        let src = r#"
struct P { x: int }
impl Eq for P {
    fn eq(self, other: P) -> bool { self.x == other.x }
}
fn mk(x: int) -> P { P { x: x } }
"#;
        let (mut vm, unit) = vm_with(src);
        let a = vm.call_name(&unit, "mk", vec![Value::Int(1)]).unwrap();
        let b = vm.call_name(&unit, "mk", vec![Value::Int(1)]).unwrap();
        let c = vm.call_name(&unit, "mk", vec![Value::Int(2)]).unwrap();
        assert!(vm.value_eq(&a, &b).unwrap());
        assert!(!vm.value_eq(&a, &c).unwrap());
    }
}
