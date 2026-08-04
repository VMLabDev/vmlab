//! Local type inference: a small unifier over `Type::Var`.
//!
//! Inference is *local* (PRD §3.3): annotations are mandatory on function
//! signatures, so unification variables only live within one top-level
//! function body (including its nested closures) and are fully resolved
//! when that function has been checked.

use wscript_core::types::{FnSig, Type};

#[derive(Default)]
pub struct Infer {
    /// Binding per variable; `None` = unbound.
    vars: Vec<Option<Type>>,
}

pub struct UnifyError {
    pub expected: Type,
    pub found: Type,
}

impl Infer {
    pub fn reset(&mut self) {
        self.vars.clear();
    }

    pub fn fresh(&mut self) -> Type {
        self.vars.push(None);
        Type::Var(self.vars.len() as u32 - 1)
    }

    /// Follow variable bindings one level (the binding itself may be a var).
    fn shallow(&self, mut t: Type) -> Type {
        while let Type::Var(v) = t {
            match &self.vars[v as usize] {
                Some(bound) => t = bound.clone(),
                None => return Type::Var(v),
            }
        }
        t
    }

    /// Fully substitute bound variables. Unbound variables survive (the
    /// caller reports "type annotations needed").
    pub fn resolve(&self, t: &Type) -> Type {
        match self.shallow(t.clone()) {
            Type::List(e) => Type::List(Box::new(self.resolve(&e))),
            Type::Map(k, v) => Type::Map(Box::new(self.resolve(&k)), Box::new(self.resolve(&v))),
            Type::Option(e) => Type::Option(Box::new(self.resolve(&e))),
            Type::Result(a, b) => {
                Type::Result(Box::new(self.resolve(&a)), Box::new(self.resolve(&b)))
            }
            Type::Weak(e) => Type::Weak(Box::new(self.resolve(&e))),
            Type::Fn(sig) => Type::Fn(Box::new(FnSig {
                params: sig.params.iter().map(|p| self.resolve(p)).collect(),
                ret: self.resolve(&sig.ret),
            })),
            other => other,
        }
    }

    pub fn contains_unbound(&self, t: &Type) -> bool {
        match self.shallow(t.clone()) {
            Type::Var(_) => true,
            Type::List(e) | Type::Option(e) | Type::Weak(e) => self.contains_unbound(&e),
            Type::Map(a, b) | Type::Result(a, b) => {
                self.contains_unbound(&a) || self.contains_unbound(&b)
            }
            Type::Fn(sig) => {
                sig.params.iter().any(|p| self.contains_unbound(p))
                    || self.contains_unbound(&sig.ret)
            }
            _ => false,
        }
    }

    fn occurs(&self, var: u32, t: &Type) -> bool {
        match self.shallow(t.clone()) {
            Type::Var(v) => v == var,
            Type::List(e) | Type::Option(e) | Type::Weak(e) => self.occurs(var, &e),
            Type::Map(a, b) | Type::Result(a, b) => self.occurs(var, &a) || self.occurs(var, &b),
            Type::Fn(sig) => {
                sig.params.iter().any(|p| self.occurs(var, p)) || self.occurs(var, &sig.ret)
            }
            _ => false,
        }
    }

    /// Unify `expected` with `found`. `Never` unifies with anything
    /// (divergence coerces to any type); `Error` unifies with anything
    /// (poison — the mismatch was already reported).
    pub fn unify(&mut self, expected: &Type, found: &Type) -> Result<(), UnifyError> {
        let a = self.shallow(expected.clone());
        let b = self.shallow(found.clone());
        match (&a, &b) {
            (Type::Error, _) | (_, Type::Error) => Ok(()),
            (Type::Never, _) | (_, Type::Never) => Ok(()),
            (Type::Var(v), _) => {
                if let Type::Var(w) = b
                    && w == *v
                {
                    return Ok(());
                }
                if self.occurs(*v, &b) {
                    return Err(UnifyError {
                        expected: a,
                        found: b,
                    });
                }
                self.vars[*v as usize] = Some(b);
                Ok(())
            }
            (_, Type::Var(w)) => {
                if self.occurs(*w, &a) {
                    return Err(UnifyError {
                        expected: a,
                        found: b,
                    });
                }
                self.vars[*w as usize] = Some(a);
                Ok(())
            }
            (Type::Int, Type::Int)
            | (Type::Float, Type::Float)
            | (Type::Bool, Type::Bool)
            | (Type::Char, Type::Char)
            | (Type::Unit, Type::Unit)
            | (Type::Str, Type::Str) => Ok(()),
            (Type::List(x), Type::List(y)) => self.unify(x, y),
            (Type::Option(x), Type::Option(y)) => self.unify(x, y),
            (Type::Weak(x), Type::Weak(y)) => self.unify(x, y),
            (Type::Map(k1, v1), Type::Map(k2, v2)) => {
                self.unify(k1, k2)?;
                self.unify(v1, v2)
            }
            (Type::Result(t1, e1), Type::Result(t2, e2)) => {
                self.unify(t1, t2)?;
                self.unify(e1, e2)
            }
            (Type::Fn(s1), Type::Fn(s2)) => {
                if s1.params.len() != s2.params.len() {
                    return Err(UnifyError {
                        expected: a.clone(),
                        found: b.clone(),
                    });
                }
                for (p1, p2) in s1.params.iter().zip(&s2.params) {
                    self.unify(p1, p2)?;
                }
                self.unify(&s1.ret, &s2.ret)
            }
            (Type::Named(d1), Type::Named(d2)) if d1 == d2 => Ok(()),
            (Type::Dyn(t1), Type::Dyn(t2)) if t1 == t2 => Ok(()),
            // Rigid type parameters (generic fn bodies): `T` unifies only
            // with itself.
            (Type::Param(p1), Type::Param(p2)) if p1 == p2 => Ok(()),
            _ => Err(UnifyError {
                expected: a,
                found: b,
            }),
        }
    }
}

/// Substitute `Type::Param(i)` with `args[i]` — instantiation of builtin
/// method schemes and of `Option`/`Result` variant payloads.
pub fn subst_params(t: &Type, args: &[Type]) -> Type {
    match t {
        Type::Param(i) => args.get(*i as usize).cloned().unwrap_or(Type::Error),
        Type::List(e) => Type::List(Box::new(subst_params(e, args))),
        Type::Map(k, v) => Type::Map(
            Box::new(subst_params(k, args)),
            Box::new(subst_params(v, args)),
        ),
        Type::Option(e) => Type::Option(Box::new(subst_params(e, args))),
        Type::Result(a, b) => Type::Result(
            Box::new(subst_params(a, args)),
            Box::new(subst_params(b, args)),
        ),
        Type::Weak(e) => Type::Weak(Box::new(subst_params(e, args))),
        Type::Fn(sig) => Type::Fn(Box::new(FnSig {
            params: sig.params.iter().map(|p| subst_params(p, args)).collect(),
            ret: subst_params(&sig.ret, args),
        })),
        other => other.clone(),
    }
}
