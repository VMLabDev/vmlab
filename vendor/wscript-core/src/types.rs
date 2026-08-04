use crate::defs::{DefId, DefTable};

/// The wscript type representation, shared by the checker, host registration,
/// and `.wscripti` generation.
///
/// User types are monomorphic (PRD §3.6); the only generic types are the
/// compiler-special-cased built-ins, which get dedicated variants here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Int,
    Float,
    Bool,
    Char,
    Unit,
    Str,
    List(Box<Type>),
    Map(Box<Type>, Box<Type>),
    Option(Box<Type>),
    Result(Box<Type>, Box<Type>),
    Weak(Box<Type>),
    /// Function value type: `fn(int, string) -> bool`.
    Fn(Box<FnSig>),
    /// A nominal struct or enum (script-defined or host-registered).
    Named(DefId),
    /// `dyn Trait` — dynamic dispatch through a vtable (PRD §3.7).
    Dyn(DefId),
    /// Type parameter of a built-in method scheme (e.g. `T` in `List[T].map`).
    /// Instantiated with fresh inference variables at each call site.
    Param(u32),
    /// Checker-internal inference variable. Never appears in checked output.
    Var(u32),
    /// The type of diverging expressions (`return`, `break`); unifies with
    /// anything.
    Never,
    /// Poison type produced after a reported error, to suppress cascades.
    Error,
}

impl Type {
    pub fn is_error(&self) -> bool {
        matches!(self, Type::Error)
    }

    /// Is this one of the inline value types (stored directly in registers)?
    pub fn is_primitive(&self) -> bool {
        matches!(
            self,
            Type::Int | Type::Float | Type::Bool | Type::Char | Type::Unit
        )
    }

    /// Render the type for diagnostics and `.wscripti` files.
    pub fn display(&self, defs: &DefTable) -> String {
        match self {
            Type::Int => "int".into(),
            Type::Float => "float".into(),
            Type::Bool => "bool".into(),
            Type::Char => "char".into(),
            Type::Unit => "unit".into(),
            Type::Str => "string".into(),
            Type::List(t) => format!("List[{}]", t.display(defs)),
            Type::Map(k, v) => format!("Map[{}, {}]", k.display(defs), v.display(defs)),
            Type::Option(t) => format!("Option[{}]", t.display(defs)),
            Type::Result(t, e) => format!("Result[{}, {}]", t.display(defs), e.display(defs)),
            Type::Weak(t) => format!("weak[{}]", t.display(defs)),
            Type::Fn(sig) => {
                let params: Vec<String> = sig.params.iter().map(|p| p.display(defs)).collect();
                if sig.ret == Type::Unit {
                    format!("fn({})", params.join(", "))
                } else {
                    format!("fn({}) -> {}", params.join(", "), sig.ret.display(defs))
                }
            }
            Type::Named(id) => defs.name_of(*id).to_string(),
            Type::Dyn(id) => format!("dyn {}", defs.trait_name(*id)),
            Type::Param(n) => {
                // Scheme parameters render as T, U, V… in diagnostics.
                let letters = ['T', 'U', 'V', 'W'];
                letters
                    .get(*n as usize)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| format!("T{n}"))
            }
            Type::Var(n) => format!("_{n}"),
            Type::Never => "!".into(),
            Type::Error => "{error}".into(),
        }
    }
}

/// A function signature: parameter types and return type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FnSig {
    pub params: Vec<Type>,
    pub ret: Type,
}

impl FnSig {
    pub fn new(params: Vec<Type>, ret: Type) -> FnSig {
        FnSig { params, ret }
    }
}
