//! The runtime value model (PRD §4, §5.2).
//!
//! Primitives live inline in registers; everything else is an `Rc`-managed
//! reference type with free aliased mutation (interior `RefCell`s). Values
//! are deliberately `!Send` — one VM per thread (PRD §4.3).

use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::{Rc, Weak};

use crate::bytecode::Const;
use crate::defs::{DefId, DefKind, DefTable, VariantKind};

/// Maximum structural nesting depth for the recursive value operations
/// (equality, ordering, deep clone, display). Values are freely aliasable
/// `Rc<RefCell<...>>` graphs, so scripts can build cyclic or arbitrarily
/// deep data; the VM's structural ops fault past this depth instead of
/// overflowing the native stack, and the infallible renderer truncates.
///
/// Sized so the deepest walk fits comfortably in a 2 MiB thread stack
/// even in debug builds (the ops' native frames are several KiB there),
/// mirroring Lua's C-call limit of the same order. 200 nesting levels is
/// far beyond any sane script data.
pub const MAX_VALUE_DEPTH: usize = 200;

/// Output cap for the infallible [`Value::display`] renderer. Required in
/// addition to the depth cap: a branching cycle (`a = [l, l]; l = [a, a]`)
/// would otherwise do O(2^MAX_VALUE_DEPTH) work before every path hits the
/// depth limit. Rendering stops with a `…` marker at this many bytes.
pub const MAX_DISPLAY_BYTES: usize = 64 * 1024;

/// Render a float the way scripts see it: whole values keep one decimal
/// place (`2.0`, not `2`) so they stay visibly distinct from ints.
pub fn format_float(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// A runtime value.
#[derive(Debug, Clone)]
pub enum Value {
    Unit,
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    /// Immutable string; `Rc` makes clones cheap (PRD §3.2).
    Str(Rc<str>),
    List(Rc<RefCell<Vec<Value>>>),
    /// Ordered map for deterministic iteration. Key types are restricted by
    /// the checker to `int`, `bool`, `char`, `string`.
    Map(Rc<RefCell<BTreeMap<Key, Value>>>),
    Struct(Rc<StructInstance>),
    Enum(Rc<EnumInstance>),
    Closure(Rc<Closure>),
    /// Opaque host handle (`#[script(opaque)]`).
    Opaque(Rc<OpaqueCell>),
    /// A concrete value coerced to `dyn Trait`, carrying its vtable id.
    Dyn(Rc<DynObj>),
    /// `weak[T]`.
    WeakRef(WeakValue),
    /// Internal: a mutable box for closure-captured locals. Never observable
    /// as a script type; only `CellGet`/`CellSet`/`MakeClosure` touch it.
    Cell(Rc<RefCell<Value>>),
}

/// Map key — the hashable/orderable subset of values.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    Int(i64),
    Bool(bool),
    Char(char),
    Str(Rc<str>),
}

impl Key {
    pub fn from_value(v: &Value) -> Option<Key> {
        match v {
            Value::Int(n) => Some(Key::Int(*n)),
            Value::Bool(b) => Some(Key::Bool(*b)),
            Value::Char(c) => Some(Key::Char(*c)),
            Value::Str(s) => Some(Key::Str(s.clone())),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Key::Int(n) => Value::Int(*n),
            Key::Bool(b) => Value::Bool(*b),
            Key::Char(c) => Value::Char(*c),
            Key::Str(s) => Value::Str(s.clone()),
        }
    }
}

#[derive(Debug)]
pub struct StructInstance {
    pub def: DefId,
    pub fields: RefCell<Vec<Value>>,
}

#[derive(Debug)]
pub struct EnumInstance {
    pub def: DefId,
    pub tag: u32,
    pub fields: RefCell<Vec<Value>>,
}

#[derive(Debug)]
pub struct Closure {
    pub proto: u32,
    pub captures: Vec<Rc<RefCell<Value>>>,
}

/// A live host value held by handle. Borrow conflicts at the host boundary
/// surface as `Err`, never panics (PRD §6.5) — hence `try_borrow` at every
/// access site.
pub struct OpaqueCell {
    pub def: DefId,
    pub cell: RefCell<Box<dyn Any>>,
}

impl fmt::Debug for OpaqueCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OpaqueCell(def={:?})", self.def)
    }
}

#[derive(Debug)]
pub struct DynObj {
    pub vtable: u32,
    pub inner: Value,
}

/// The weak counterpart of each reference-type payload.
#[derive(Debug, Clone)]
pub enum WeakValue {
    List(Weak<RefCell<Vec<Value>>>),
    Map(Weak<RefCell<BTreeMap<Key, Value>>>),
    Struct(Weak<StructInstance>),
    Enum(Weak<EnumInstance>),
    Closure(Weak<Closure>),
    Opaque(Weak<OpaqueCell>),
    Dyn(Weak<DynObj>),
}

impl WeakValue {
    pub fn upgrade(&self) -> Option<Value> {
        match self {
            WeakValue::List(w) => w.upgrade().map(Value::List),
            WeakValue::Map(w) => w.upgrade().map(Value::Map),
            WeakValue::Struct(w) => w.upgrade().map(Value::Struct),
            WeakValue::Enum(w) => w.upgrade().map(Value::Enum),
            WeakValue::Closure(w) => w.upgrade().map(Value::Closure),
            WeakValue::Opaque(w) => w.upgrade().map(Value::Opaque),
            WeakValue::Dyn(w) => w.upgrade().map(Value::Dyn),
        }
    }
}

impl Value {
    pub fn from_const(c: &Const) -> Value {
        match c {
            Const::Unit => Value::Unit,
            Const::Int(n) => Value::Int(*n),
            Const::Float(f) => Value::Float(*f),
            Const::Bool(b) => Value::Bool(*b),
            Const::Char(c) => Value::Char(*c),
            Const::Str(s) => Value::Str(Rc::from(&**s)),
        }
    }

    pub fn new_struct(def: DefId, fields: Vec<Value>) -> Value {
        Value::Struct(Rc::new(StructInstance {
            def,
            fields: RefCell::new(fields),
        }))
    }

    pub fn new_enum(def: DefId, tag: u32, fields: Vec<Value>) -> Value {
        Value::Enum(Rc::new(EnumInstance {
            def,
            tag,
            fields: RefCell::new(fields),
        }))
    }

    pub fn new_list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(RefCell::new(items)))
    }

    pub fn new_map(entries: BTreeMap<Key, Value>) -> Value {
        Value::Map(Rc::new(RefCell::new(entries)))
    }

    /// `weak(x)`: downgrade. Returns `None` for non-reference values (the
    /// checker rejects those; this is the runtime backstop).
    pub fn downgrade(&self) -> Option<WeakValue> {
        match self {
            Value::List(rc) => Some(WeakValue::List(Rc::downgrade(rc))),
            Value::Map(rc) => Some(WeakValue::Map(Rc::downgrade(rc))),
            Value::Struct(rc) => Some(WeakValue::Struct(Rc::downgrade(rc))),
            Value::Enum(rc) => Some(WeakValue::Enum(Rc::downgrade(rc))),
            Value::Closure(rc) => Some(WeakValue::Closure(Rc::downgrade(rc))),
            Value::Opaque(rc) => Some(WeakValue::Opaque(Rc::downgrade(rc))),
            Value::Dyn(rc) => Some(WeakValue::Dyn(Rc::downgrade(rc))),
            _ => None,
        }
    }

    /// `same(a, b)`: reference identity for reference types; value equality
    /// for primitives (documented behaviour of the builtin).
    pub fn same(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Char(a), Value::Char(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => Rc::ptr_eq(a, b),
            (Value::List(a), Value::List(b)) => Rc::ptr_eq(a, b),
            (Value::Map(a), Value::Map(b)) => Rc::ptr_eq(a, b),
            (Value::Struct(a), Value::Struct(b)) => Rc::ptr_eq(a, b),
            (Value::Enum(a), Value::Enum(b)) => Rc::ptr_eq(a, b),
            (Value::Closure(a), Value::Closure(b)) => Rc::ptr_eq(a, b),
            (Value::Opaque(a), Value::Opaque(b)) => Rc::ptr_eq(a, b),
            (Value::Dyn(a), Value::Dyn(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Human-readable rendering used by `print`, `str()` and derived
    /// `Display` (debug-ish for structs/enums, PRD §3.8). Top-level strings
    /// render bare; strings nested in structures render quoted.
    ///
    /// Infallible by design (host boundary + REPL), so cyclic or oversized
    /// values are *truncated* with a `…` marker rather than faulting: past
    /// [`MAX_VALUE_DEPTH`] nesting levels or [`MAX_DISPLAY_BYTES`] of
    /// output. The VM-side renderer (`print`/`str()` in scripts) faults on
    /// too-deep values instead.
    pub fn display(&self, defs: &DefTable) -> String {
        let mut out = String::new();
        self.fmt_into(defs, &mut out, false, 0);
        out
    }

    fn fmt_into(&self, defs: &DefTable, out: &mut String, nested: bool, depth: usize) {
        use std::fmt::Write;
        if depth >= MAX_VALUE_DEPTH || out.len() >= MAX_DISPLAY_BYTES {
            if !out.ends_with('…') {
                out.push('…');
            }
            return;
        }
        match self {
            Value::Unit => out.push_str("()"),
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(f) => out.push_str(&format_float(*f)),
            Value::Bool(b) => {
                let _ = write!(out, "{b}");
            }
            Value::Char(c) => {
                if nested {
                    let _ = write!(out, "{c:?}");
                } else {
                    out.push(*c);
                }
            }
            Value::Str(s) => {
                if nested {
                    let _ = write!(out, "{s:?}");
                } else {
                    out.push_str(s);
                }
            }
            Value::List(items) => {
                out.push('[');
                for (i, v) in items.borrow().iter().enumerate() {
                    if out.len() >= MAX_DISPLAY_BYTES {
                        if !out.ends_with('…') {
                            out.push('…');
                        }
                        break;
                    }
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.fmt_into(defs, out, true, depth + 1);
                }
                out.push(']');
            }
            Value::Map(entries) => {
                out.push_str("#{");
                for (i, (k, v)) in entries.borrow().iter().enumerate() {
                    if out.len() >= MAX_DISPLAY_BYTES {
                        if !out.ends_with('…') {
                            out.push('…');
                        }
                        break;
                    }
                    if i > 0 {
                        out.push_str(", ");
                    }
                    k.to_value().fmt_into(defs, out, true, depth + 1);
                    out.push_str(": ");
                    v.fmt_into(defs, out, true, depth + 1);
                }
                out.push('}');
            }
            Value::Struct(s) => {
                let name = defs.name_of(s.def).to_string();
                out.push_str(&name);
                if let Some(DefKind::Struct(sd)) = defs.defs.get(s.def.index())
                    && sd.opaque
                {
                    out.push_str(" { <opaque> }");
                    return;
                }
                out.push_str(" { ");
                let field_names: Vec<String> = defs
                    .as_struct(s.def)
                    .map(|sd| sd.fields.iter().map(|(n, _)| n.clone()).collect())
                    .unwrap_or_default();
                for (i, v) in s.fields.borrow().iter().enumerate() {
                    if out.len() >= MAX_DISPLAY_BYTES {
                        if !out.ends_with('…') {
                            out.push('…');
                        }
                        break;
                    }
                    if i > 0 {
                        out.push_str(", ");
                    }
                    if let Some(n) = field_names.get(i) {
                        out.push_str(n);
                        out.push_str(": ");
                    }
                    v.fmt_into(defs, out, true, depth + 1);
                }
                out.push_str(" }");
            }
            Value::Enum(e) => {
                let (variant, kind, names) = match defs.as_enum(e.def) {
                    Some(ed) => {
                        let v = &ed.variants[e.tag as usize];
                        (
                            v.name.clone(),
                            v.kind,
                            v.fields.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                        )
                    }
                    None => (format!("<tag {}>", e.tag), VariantKind::Tuple, vec![]),
                };
                // Builtin Option/Result render without the enum name prefix
                // (matches how scripts write them: `Some(1)`, not
                // `Option::Some(1)`).
                let enum_name = defs.name_of(e.def);
                if enum_name != "Option" && enum_name != "Result" {
                    out.push_str(enum_name);
                    out.push_str("::");
                }
                out.push_str(&variant);
                let fields = e.fields.borrow();
                match kind {
                    VariantKind::Unit => {}
                    VariantKind::Tuple => {
                        out.push('(');
                        for (i, v) in fields.iter().enumerate() {
                            if out.len() >= MAX_DISPLAY_BYTES {
                                if !out.ends_with('…') {
                                    out.push('…');
                                }
                                break;
                            }
                            if i > 0 {
                                out.push_str(", ");
                            }
                            v.fmt_into(defs, out, true, depth + 1);
                        }
                        out.push(')');
                    }
                    VariantKind::Struct => {
                        out.push_str(" { ");
                        for (i, v) in fields.iter().enumerate() {
                            if out.len() >= MAX_DISPLAY_BYTES {
                                if !out.ends_with('…') {
                                    out.push('…');
                                }
                                break;
                            }
                            if i > 0 {
                                out.push_str(", ");
                            }
                            if let Some(n) = names.get(i) {
                                out.push_str(n);
                                out.push_str(": ");
                            }
                            v.fmt_into(defs, out, true, depth + 1);
                        }
                        out.push_str(" }");
                    }
                }
            }
            Value::Closure(c) => {
                let _ = write!(out, "<fn #{}>", c.proto);
            }
            Value::Opaque(o) => {
                let _ = write!(out, "<{}>", defs.name_of(o.def));
            }
            Value::Dyn(d) => d.inner.fmt_into(defs, out, nested, depth + 1),
            Value::WeakRef(_) => out.push_str("<weak>"),
            Value::Cell(c) => c.borrow().fmt_into(defs, out, nested, depth + 1),
        }
    }

    /// Name of the value's runtime shape, for fault messages.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Value::Unit => "unit",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Char(_) => "char",
            Value::Str(_) => "string",
            Value::List(_) => "List",
            Value::Map(_) => "Map",
            Value::Struct(_) => "struct",
            Value::Enum(_) => "enum",
            Value::Closure(_) => "fn",
            Value::Opaque(_) => "opaque",
            Value::Dyn(_) => "dyn",
            Value::WeakRef(_) => "weak",
            Value::Cell(_) => "cell",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> DefTable {
        DefTable::default()
    }

    /// `l = [l]` — a self-containing list.
    fn cyclic_list() -> Value {
        let l = Value::new_list(vec![]);
        if let Value::List(rc) = &l {
            rc.borrow_mut().push(l.clone());
        }
        l
    }

    #[test]
    fn display_primitives() {
        let d = defs();
        assert_eq!(Value::Int(42).display(&d), "42");
        assert_eq!(Value::Float(1.0).display(&d), "1.0");
        assert_eq!(Value::Float(1.5).display(&d), "1.5");
        assert_eq!(Value::Bool(true).display(&d), "true");
        assert_eq!(Value::Unit.display(&d), "()");
        // Top-level strings render bare; nested ones quoted.
        assert_eq!(Value::Str(Rc::from("hi")).display(&d), "hi");
        let list = Value::new_list(vec![Value::Str(Rc::from("hi")), Value::Char('a')]);
        assert_eq!(list.display(&d), r#"["hi", 'a']"#);
    }

    #[test]
    fn display_cyclic_list_truncates_finitely() {
        let out = cyclic_list().display(&defs());
        assert!(out.contains('…'), "missing truncation marker: {out}");
        // Depth cap × small per-level overhead, plus the size-cap slack.
        assert!(out.len() <= MAX_DISPLAY_BYTES + 4 * MAX_VALUE_DEPTH);
    }

    #[test]
    fn display_branching_cycle_bounded() {
        // a = [l, l]; l = [a, a] — exponential paths without the size cap.
        let a = Value::new_list(vec![]);
        let l = Value::new_list(vec![a.clone(), a.clone()]);
        if let Value::List(rc) = &a {
            rc.borrow_mut().push(l.clone());
            rc.borrow_mut().push(l.clone());
        }
        let start = std::time::Instant::now();
        let out = a.display(&defs());
        assert!(out.contains('…'));
        assert!(out.len() <= MAX_DISPLAY_BYTES + 4 * MAX_VALUE_DEPTH);
        // The size cap bounds total work; without it this would never
        // finish. Generous wall-clock guard against regressions.
        assert!(start.elapsed().as_secs() < 10);
    }

    #[test]
    fn key_from_value_roundtrip() {
        for v in [
            Value::Int(7),
            Value::Bool(false),
            Value::Char('x'),
            Value::Str(Rc::from("k")),
        ] {
            let k = Key::from_value(&v).unwrap();
            assert!(k.to_value().same(&v));
        }
        assert!(Key::from_value(&Value::new_list(vec![])).is_none());
    }

    #[test]
    fn same_is_identity_for_refs_value_for_prims() {
        let a = Value::new_list(vec![Value::Int(1)]);
        let alias = a.clone();
        let twin = Value::new_list(vec![Value::Int(1)]);
        assert!(a.same(&alias));
        assert!(!a.same(&twin));
        assert!(Value::Int(1).same(&Value::Int(1)));
    }

    #[test]
    fn weak_upgrade_lifecycle() {
        let strong = Value::new_list(vec![Value::Int(1)]);
        let weak = strong.downgrade().unwrap();
        assert!(weak.upgrade().is_some());
        drop(strong);
        assert!(weak.upgrade().is_none());
    }
}
