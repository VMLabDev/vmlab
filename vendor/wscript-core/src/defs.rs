use crate::types::{FnSig, Type};

/// Index into a [`DefTable`]. One id space covers structs, enums and traits;
/// `Type::Named` must point at a struct/enum entry, `Type::Dyn` at a trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefId(pub u32);

impl DefId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Reserved ids, present in every table (see [`DefTable::with_builtins`]).
pub const DEF_OPTION: DefId = DefId(0);
pub const DEF_RESULT: DefId = DefId(1);
pub const TRAIT_ADD: DefId = DefId(2);
pub const TRAIT_SUB: DefId = DefId(3);
pub const TRAIT_MUL: DefId = DefId(4);
pub const TRAIT_DIV: DefId = DefId(5);
pub const TRAIT_REM: DefId = DefId(6);
pub const TRAIT_NEG: DefId = DefId(7);
pub const TRAIT_EQ: DefId = DefId(8);
pub const TRAIT_ORD: DefId = DefId(9);
pub const TRAIT_DISPLAY: DefId = DefId(10);
pub const TRAIT_INDEX: DefId = DefId(11);
pub const FIRST_FREE_DEF: u32 = 12;

/// Tag values for the builtin enums (fixed; the VM and `?` lowering rely on
/// them).
pub const TAG_NONE: u32 = 0;
pub const TAG_SOME: u32 = 1;
pub const TAG_OK: u32 = 0;
pub const TAG_ERR: u32 = 1;

#[derive(Debug, Clone)]
pub enum DefKind {
    Struct(StructDef),
    Enum(EnumDef),
    Trait(TraitDef),
    /// A unit family: a nominal type backed by `int` or `float` whose values
    /// are stored normalised to the base unit (PRD §3.10).
    Unit(UnitDef),
}

/// One unit's conversion factor, in the family's backing type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Factor {
    Int(i64),
    Float(f64),
}

impl Factor {
    pub fn is_one(self) -> bool {
        match self {
            Factor::Int(n) => n == 1,
            Factor::Float(f) => f == 1.0,
        }
    }

    /// Render a factor for diagnostics.
    pub fn display(self) -> String {
        match self {
            Factor::Int(n) => n.to_string(),
            Factor::Float(f) => crate::value::format_float(f),
        }
    }
}

/// A unit family declared with `units Name: int { ... }`.
///
/// Values of the family are erased to the backing primitive at runtime; the
/// table survives into the `CompiledUnit` so the VM can render them.
#[derive(Debug, Clone)]
pub struct UnitDef {
    pub name: String,
    /// `Type::Int` or `Type::Float`.
    pub base: Type,
    /// Index into `units` of the unit whose factor is 1.
    pub base_unit: usize,
    /// Declaration order: `(unit name, factor in base units)`.
    pub units: Vec<(String, Factor)>,
}

impl UnitDef {
    pub fn is_float(&self) -> bool {
        self.base == Type::Float
    }

    pub fn factor_of(&self, unit: &str) -> Option<Factor> {
        self.units.iter().find(|(n, _)| n == unit).map(|(_, f)| *f)
    }

    pub fn base_name(&self) -> &str {
        &self.units[self.base_unit].0
    }

    /// Units ordered largest factor first — the search order used when
    /// rendering a raw value.
    pub fn descending(&self) -> Vec<&(String, Factor)> {
        let mut v: Vec<&(String, Factor)> = self.units.iter().collect();
        v.sort_by(|a, b| match (a.1, b.1) {
            (Factor::Int(x), Factor::Int(y)) => y.cmp(&x),
            (Factor::Float(x), Factor::Float(y)) => {
                y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal)
            }
            _ => std::cmp::Ordering::Equal,
        });
        v
    }

    /// Render a stored base-unit count with the largest unit that names it
    /// cleanly: for `int` the largest unit dividing it exactly (so the text
    /// always round-trips), for `float` the largest unit it reaches. Zero,
    /// and anything no unit fits, falls back to the base unit.
    ///
    /// `None` if the value is not the family's backing primitive.
    pub fn render(&self, v: &crate::value::Value) -> Option<String> {
        use crate::value::{Value, format_float};
        let base = self.base_name();
        match v {
            Value::Int(n) => Some(
                self.descending()
                    .into_iter()
                    .find_map(|(name, f)| match f {
                        Factor::Int(d) if *d != 0 && *n != 0 && n % d == 0 => {
                            Some(format!("{}{name}", n / d))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| format!("{n}{base}")),
            ),
            Value::Float(x) => Some(
                self.descending()
                    .into_iter()
                    .find_map(|(name, f)| match f {
                        Factor::Float(d) if *d > 0.0 && x.abs() >= *d => {
                            Some(format!("{}{name}", format_float(x / d)))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| format!("{}{base}", format_float(*x))),
            ),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<(String, Type)>,
    /// Host handle type (`#[script(opaque)]`): no field access, methods only.
    pub opaque: bool,
    /// Registered by the host rather than declared in script.
    pub host: bool,
    /// For host-registered types: the Rust `TypeId`, so conversions can
    /// locate the def for their type in any context.
    pub rust_type: Option<std::any::TypeId>,
}

#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<VariantDef>,
    pub host: bool,
    pub rust_type: Option<std::any::TypeId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantKind {
    Unit,
    Tuple,
    Struct,
}

#[derive(Debug, Clone)]
pub struct VariantDef {
    pub name: String,
    pub kind: VariantKind,
    /// Payload fields. Tuple variants have `"0"`, `"1"`… as names.
    pub fields: Vec<(String, Type)>,
}

#[derive(Debug, Clone)]
pub struct TraitDef {
    pub name: String,
    /// Method name → signature (excluding the implicit `self` receiver).
    pub methods: Vec<(String, FnSig)>,
    /// One of the builtin operator traits (Add … Index). Their method
    /// signatures are shape-checked specially (`Self` is `Type::Param(0)`).
    pub operator: bool,
}

/// Table of all nominal type and trait definitions visible to a compilation:
/// builtins, host-registered defs, then script defs appended in order.
#[derive(Debug, Clone)]
pub struct DefTable {
    pub defs: Vec<DefKind>,
}

impl Default for DefTable {
    fn default() -> Self {
        DefTable::with_builtins()
    }
}

impl DefTable {
    /// A table pre-seeded with `Option`, `Result` and the operator traits at
    /// their reserved ids.
    pub fn with_builtins() -> DefTable {
        let p0 = || Type::Param(0);
        let p1 = || Type::Param(1);
        let mut defs = Vec::with_capacity(FIRST_FREE_DEF as usize);
        defs.push(DefKind::Enum(EnumDef {
            name: "Option".into(),
            variants: vec![
                VariantDef {
                    name: "None".into(),
                    kind: VariantKind::Unit,
                    fields: vec![],
                },
                VariantDef {
                    name: "Some".into(),
                    kind: VariantKind::Tuple,
                    fields: vec![("0".into(), p0())],
                },
            ],
            host: false,
            rust_type: None,
        }));
        defs.push(DefKind::Enum(EnumDef {
            name: "Result".into(),
            variants: vec![
                VariantDef {
                    name: "Ok".into(),
                    kind: VariantKind::Tuple,
                    fields: vec![("0".into(), p0())],
                },
                VariantDef {
                    name: "Err".into(),
                    kind: VariantKind::Tuple,
                    fields: vec![("0".into(), p1())],
                },
            ],
            host: false,
            rust_type: None,
        }));
        // Operator traits (PRD §3.7). `Self` is Param(0) in these signatures;
        // impls provide concrete types which the checker shape-checks.
        let binop = |name: &str, method: &str| {
            DefKind::Trait(TraitDef {
                name: name.into(),
                methods: vec![(method.into(), FnSig::new(vec![p0()], p0()))],
                operator: true,
            })
        };
        defs.push(binop("Add", "add"));
        defs.push(binop("Sub", "sub"));
        defs.push(binop("Mul", "mul"));
        defs.push(binop("Div", "div"));
        defs.push(binop("Rem", "rem"));
        defs.push(DefKind::Trait(TraitDef {
            name: "Neg".into(),
            methods: vec![("neg".into(), FnSig::new(vec![], p0()))],
            operator: true,
        }));
        defs.push(DefKind::Trait(TraitDef {
            name: "Eq".into(),
            methods: vec![("eq".into(), FnSig::new(vec![p0()], Type::Bool))],
            operator: true,
        }));
        defs.push(DefKind::Trait(TraitDef {
            name: "Ord".into(),
            // cmp returns -1 / 0 / 1
            methods: vec![("cmp".into(), FnSig::new(vec![p0()], Type::Int))],
            operator: true,
        }));
        defs.push(DefKind::Trait(TraitDef {
            name: "Display".into(),
            methods: vec![("fmt".into(), FnSig::new(vec![], Type::Str))],
            operator: true,
        }));
        defs.push(DefKind::Trait(TraitDef {
            name: "Index".into(),
            // Impls declare their own concrete index (Param 1) and output
            // (Param 2) types; only the one-parameter shape is fixed.
            methods: vec![("index".into(), FnSig::new(vec![p1()], Type::Param(2)))],
            operator: true,
        }));
        debug_assert_eq!(defs.len(), FIRST_FREE_DEF as usize);
        DefTable { defs }
    }

    pub fn push(&mut self, def: DefKind) -> DefId {
        let id = DefId(self.defs.len() as u32);
        self.defs.push(def);
        id
    }

    pub fn get(&self, id: DefId) -> &DefKind {
        &self.defs[id.index()]
    }

    pub fn name_of(&self, id: DefId) -> &str {
        match self.defs.get(id.index()) {
            Some(DefKind::Struct(s)) => &s.name,
            Some(DefKind::Enum(e)) => &e.name,
            Some(DefKind::Trait(t)) => &t.name,
            Some(DefKind::Unit(u)) => &u.name,
            // Defensive: values can outlive the table that defined them
            // (e.g. REPL lines); never panic while rendering.
            None => "<unknown type>",
        }
    }

    pub fn trait_name(&self, id: DefId) -> &str {
        self.name_of(id)
    }

    pub fn as_struct(&self, id: DefId) -> Option<&StructDef> {
        match &self.defs[id.index()] {
            DefKind::Struct(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_enum(&self, id: DefId) -> Option<&EnumDef> {
        match &self.defs[id.index()] {
            DefKind::Enum(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_trait(&self, id: DefId) -> Option<&TraitDef> {
        match &self.defs[id.index()] {
            DefKind::Trait(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_unit(&self, id: DefId) -> Option<&UnitDef> {
        match self.defs.get(id.index()) {
            Some(DefKind::Unit(u)) => Some(u),
            _ => None,
        }
    }

    /// Is this def a unit family? Values of one are erased to their backing
    /// primitive at runtime, so most places that special-case `Type::Named`
    /// need to ask.
    pub fn is_quantity(&self, id: DefId) -> bool {
        matches!(self.defs.get(id.index()), Some(DefKind::Unit(_)))
    }

    /// The backing primitive of a unit family, or the type unchanged for
    /// anything else. Recurses through containers — used to lower script
    /// signatures for the host boundary, where units are invisible.
    pub fn erase_units(&self, t: &Type) -> Type {
        match t {
            Type::Named(id) => match self.as_unit(*id) {
                Some(u) => u.base.clone(),
                None => t.clone(),
            },
            Type::List(e) => Type::List(Box::new(self.erase_units(e))),
            Type::Map(k, v) => {
                Type::Map(Box::new(self.erase_units(k)), Box::new(self.erase_units(v)))
            }
            Type::Option(e) => Type::Option(Box::new(self.erase_units(e))),
            Type::Result(o, e) => {
                Type::Result(Box::new(self.erase_units(o)), Box::new(self.erase_units(e)))
            }
            Type::Weak(e) => Type::Weak(Box::new(self.erase_units(e))),
            Type::Fn(sig) => Type::Fn(Box::new(FnSig::new(
                sig.params.iter().map(|p| self.erase_units(p)).collect(),
                self.erase_units(&sig.ret),
            ))),
            _ => t.clone(),
        }
    }

    /// Find a host-registered def by its Rust `TypeId`.
    pub fn by_rust_type(&self, ty: std::any::TypeId) -> Option<DefId> {
        self.defs
            .iter()
            .position(|d| match d {
                DefKind::Struct(s) => s.rust_type == Some(ty),
                DefKind::Enum(e) => e.rust_type == Some(ty),
                DefKind::Trait(_) | DefKind::Unit(_) => false,
            })
            .map(|i| DefId(i as u32))
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}
