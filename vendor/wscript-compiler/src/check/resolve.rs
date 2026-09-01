//! Lowering a syntactic [`TypeExpr`] to a semantic [`Type`].
//!
//! One implementation, two callers: the checker lowers the types a script
//! writes, and the `.wscripti` loader (`crate::wscripti`) lowers the types
//! a host declares. The scopes differ — only a script has generic type
//! parameters in scope — but what counts as a well-formed type must not,
//! or an interface can declare something no script could ever write. It
//! did: the loader's hand-copied resolver mapped `TypeExprKind::Error` to
//! `unit`, admitted `Map[float, int]` and `weak[int]`, and accepted type
//! constructors at the wrong arity.

use wscript_core::defs::{DefId, DefTable};
use wscript_core::diag::Diagnostic;
use wscript_core::span::Span;
use wscript_core::types::{FnSig, Type};

use crate::ast::{TypeExpr, TypeExprKind};

/// What a [`resolve_type`] call resolves names against.
pub(crate) trait TypeScope {
    /// A named type (struct, enum, trait or unit family) in scope.
    fn type_named(&self, name: &str) -> Option<DefId>;

    /// The definition table those ids index, for rendering names.
    fn defs(&self) -> &DefTable;

    fn report(&mut self, d: Diagnostic);

    /// An in-scope generic type parameter, by position in the enclosing
    /// function's parameter list. Host declarations are monomorphic, so
    /// the interface loader takes the default.
    fn type_param(&self, _name: &str) -> Option<u32> {
        None
    }
}

fn error(scope: &mut impl TypeScope, code: &'static str, span: Span, msg: impl Into<String>) {
    scope.report(Diagnostic::error(code, span, msg));
}

fn error_help(
    scope: &mut impl TypeScope,
    code: &'static str,
    span: Span,
    msg: impl Into<String>,
    help: impl Into<String>,
) {
    scope.report(Diagnostic::error(code, span, msg).with_help(help));
}

/// The types `weak[T]` is meaningful for: everything with an identity of
/// its own to point at. Primitives and strings are copied or immutable,
/// so a weak reference to one could never be observed to break.
pub(crate) fn is_reference_type(t: &Type) -> bool {
    matches!(
        t,
        Type::List(_)
            | Type::Map(..)
            | Type::Named(_)
            | Type::Fn(_)
            | Type::Dyn(_)
            | Type::Option(_)
            | Type::Result(..)
            | Type::Error
    )
}

/// The primitive a bare type name spells, if it spells one.
///
/// Separate from [`resolve_type`] because the editor's index has to answer
/// the same question from a name with no `TypeExpr` and no scope to report
/// into — and answering it with a second copy of this list is how the two
/// definitions of a `.wscripti` type drifted in the first place.
pub(crate) fn primitive_named(name: &str) -> Option<Type> {
    Some(match name {
        "int" => Type::Int,
        "float" => Type::Float,
        "bool" => Type::Bool,
        "char" => Type::Char,
        "unit" => Type::Unit,
        "string" => Type::Str,
        _ => return None,
    })
}

pub(crate) fn resolve_type(scope: &mut impl TypeScope, t: &TypeExpr) -> Type {
    match &t.kind {
        TypeExprKind::Unit => Type::Unit,
        TypeExprKind::Error => Type::Error,
        TypeExprKind::Name(ident) => {
            if let Some(primitive) = primitive_named(&ident.name) {
                return primitive;
            }
            match ident.name.as_str() {
                "List" | "Map" | "Option" | "Result" | "weak" => {
                    let name = ident.name.clone();
                    let arity = match name.as_str() {
                        "Map" | "Result" => 2,
                        _ => 1,
                    };
                    error_help(
                        scope,
                        "E0210",
                        t.span,
                        format!("`{name}` requires type arguments"),
                        format!(
                            "write `{name}[{}]`",
                            (0..arity).map(|_| "T").collect::<Vec<_>>().join(", ")
                        ),
                    );
                    Type::Error
                }
                other => {
                    // In-scope generic type parameters resolve first
                    // (shadowing an existing type name is an error at the
                    // declaration, so no ambiguity survives here).
                    if let Some(i) = scope.type_param(other) {
                        return Type::Param(i);
                    }
                    match scope.type_named(other) {
                        Some(id) if scope.defs().as_trait(id).is_some() => {
                            error_help(
                                scope,
                                "E0211",
                                ident.span,
                                format!("trait `{other}` cannot be used as a type directly"),
                                format!("use `dyn {other}` for a dynamically dispatched value"),
                            );
                            Type::Error
                        }
                        Some(id) => Type::Named(id),
                        None => {
                            error(
                                scope,
                                "E0212",
                                ident.span,
                                format!("unknown type `{other}`"),
                            );
                            Type::Error
                        }
                    }
                }
            }
        }
        TypeExprKind::App(ident, args) => {
            let mut arg_tys: Vec<Type> = args.iter().map(|a| resolve_type(scope, a)).collect();
            fn expect(
                scope: &mut impl TypeScope,
                t: &TypeExpr,
                name: &str,
                n: usize,
                arg_tys: &mut Vec<Type>,
            ) {
                if arg_tys.len() != n {
                    let msg = format!(
                        "`{name}` takes {n} type argument{}, found {}",
                        if n == 1 { "" } else { "s" },
                        arg_tys.len()
                    );
                    error(scope, "E0210", t.span, msg);
                    arg_tys.resize(n, Type::Error);
                }
            }
            match ident.name.as_str() {
                "List" => {
                    expect(scope, t, "List", 1, &mut arg_tys);
                    Type::List(Box::new(arg_tys.remove(0)))
                }
                "Option" => {
                    expect(scope, t, "Option", 1, &mut arg_tys);
                    Type::Option(Box::new(arg_tys.remove(0)))
                }
                "weak" => {
                    expect(scope, t, "weak", 1, &mut arg_tys);
                    let inner = arg_tys.remove(0);
                    if !is_reference_type(&inner) {
                        let msg = format!(
                            "`weak[{}]` is invalid: weak references only apply to \
                             reference types",
                            inner.display(scope.defs())
                        );
                        error_help(
                            scope,
                            "E0213",
                            t.span,
                            msg,
                            "structs, enums, List, Map and functions can be weakly \
                             referenced; primitives and strings cannot",
                        );
                    }
                    Type::Weak(Box::new(inner))
                }
                "Map" => {
                    expect(scope, t, "Map", 2, &mut arg_tys);
                    let v = arg_tys.remove(1);
                    let k = arg_tys.remove(0);
                    if !matches!(
                        k,
                        Type::Int | Type::Bool | Type::Char | Type::Str | Type::Error
                    ) {
                        let span = args.first().map(|a| a.span).unwrap_or(t.span);
                        let msg = format!("`{}` cannot be a map key", k.display(scope.defs()));
                        error_help(
                            scope,
                            "E0214",
                            span,
                            msg,
                            "map keys must be int, bool, char, or string",
                        );
                    }
                    Type::Map(Box::new(k), Box::new(v))
                }
                "Result" => {
                    expect(scope, t, "Result", 2, &mut arg_tys);
                    let e = arg_tys.remove(1);
                    let ok = arg_tys.remove(0);
                    Type::Result(Box::new(ok), Box::new(e))
                }
                other => {
                    let msg = format!("`{other}` does not take type arguments");
                    let help = if scope.type_param(other).is_some() {
                        "type parameters do not take type arguments".to_string()
                    } else {
                        "user-defined generic *types* are not supported yet; generic \
                         functions are — declare type parameters on the function: \
                         `fn f[T](x: T)`"
                            .to_string()
                    };
                    error_help(scope, "E0215", t.span, msg, help);
                    Type::Error
                }
            }
        }
        TypeExprKind::Fn(params, ret) => {
            let params: Vec<Type> = params.iter().map(|p| resolve_type(scope, p)).collect();
            let ret = match ret {
                Some(r) => resolve_type(scope, r),
                None => Type::Unit,
            };
            Type::Fn(Box::new(FnSig::new(params, ret)))
        }
        TypeExprKind::Dyn(ident) => match scope.type_named(&ident.name) {
            Some(id) if scope.defs().as_trait(id).is_some() => {
                if scope.defs().as_trait(id).is_some_and(|t| t.operator) {
                    let msg = format!("operator trait `{}` cannot be used as `dyn`", ident.name);
                    error(scope, "E0211", ident.span, msg);
                    return Type::Error;
                }
                Type::Dyn(id)
            }
            Some(_) => {
                let msg = format!("`{}` is not a trait", ident.name);
                error(scope, "E0211", ident.span, msg);
                Type::Error
            }
            None => {
                let msg = format!("unknown trait `{}`", ident.name);
                error(scope, "E0212", ident.span, msg);
                Type::Error
            }
        },
    }
}
