//! Type schemes for the built-in methods of `string`, `List`, `Map`,
//! `Option`, `Result` and `weak` (PRD §3.6: the checker knows these
//! internally; users cannot define generic functions).
//!
//! Scheme parameter convention: the receiver's type parameters bind
//! `Param(0..n)` (List: 0 = element; Map: 0 = key, 1 = value; Result:
//! 0 = ok, 1 = err), and `fresh` additional parameters (e.g. `U` in `map`)
//! follow, instantiated with fresh inference variables per call site.

use wscript_core::bytecode::Builtin;
use wscript_core::types::{FnSig, Type};

use super::subst_params;

#[allow(clippy::enum_variant_names)]
pub enum SchemeConstraint {
    /// Element type must support `==`.
    EqElem,
    /// Element type must be orderable (`ord_able`: primitives, containers
    /// of orderables, or types with an `Ord` impl).
    OrdElem,
    /// Element type must be `string`.
    StrElem,
    /// Element type must be numeric (int or float) — `sum`.
    NumElem,
}

pub struct Scheme {
    pub params: Vec<Type>,
    pub ret: Type,
    /// Number of scheme-local type parameters beyond the receiver's.
    pub fresh: u32,
    pub builtin: Builtin,
    pub constraint: Option<SchemeConstraint>,
}

fn s(params: Vec<Type>, ret: Type, builtin: Builtin) -> Scheme {
    Scheme {
        params,
        ret,
        fresh: 0,
        builtin,
        constraint: None,
    }
}

fn p(n: u32) -> Type {
    Type::Param(n)
}

fn list(t: Type) -> Type {
    Type::List(Box::new(t))
}

fn opt(t: Type) -> Type {
    Type::Option(Box::new(t))
}

fn func(params: Vec<Type>, ret: Type) -> Type {
    Type::Fn(Box::new(FnSig::new(params, ret)))
}

/// Declares one receiver's method table: the lookup the checker calls and
/// the name list an editor enumerates are generated from the same arms, so
/// a method cannot be added to one and missed by the other.
///
/// That drift is what this exists to prevent: the list and map combinators
/// were added to the checker's tables and never reached the language
/// server's hand-copied ones, so fourteen methods typechecked but never
/// completed (issue #17).
macro_rules! method_table {
    (
        $lookup:ident / $names:ident;
        $( let $bind:ident = $bindval:expr; )*
        $( $name:literal => $scheme:expr, )*
    ) => {
        fn $lookup(name: &str) -> Option<Scheme> {
            #[allow(unused_imports)]
            use Builtin::*;
            $( let $bind = $bindval; )*
            Some(match name {
                $( $name => $scheme, )*
                _ => return None,
            })
        }

        const $names: &[&str] = &[ $($name),* ];
    };
}

/// Look up a builtin method on a non-nominal receiver type.
pub fn builtin_method(recv: &Type, name: &str) -> Option<Scheme> {
    method_table(recv).and_then(|(lookup, _)| lookup(name))
}

/// Every builtin method of `recv`, with the receiver's type arguments
/// substituted in — what completion lists. Scheme-local parameters stay
/// as `Type::Param`, which displays as `T`, `U`, …
pub fn builtin_methods(recv: &Type) -> Vec<(&'static str, FnSig)> {
    let Some((lookup, names)) = method_table(recv) else {
        return Vec::new();
    };
    names
        .iter()
        .filter_map(|name| {
            let scheme = lookup(name)?;
            // The same substitution `apply_scheme` performs, minus the
            // inference variables: nothing is being checked here, so the
            // scheme's own parameters stay parameters and display as such.
            let mut subst = receiver_args(recv);
            subst.extend((0..scheme.fresh).map(p));
            let params = scheme.params.iter().map(|t| subst_params(t, &subst));
            let ret = subst_params(&scheme.ret, &subst);
            Some((*name, FnSig::new(params.collect(), ret)))
        })
        .collect()
}

/// The type arguments a receiver binds to `Param(0..)`.
fn receiver_args(recv: &Type) -> Vec<Type> {
    match recv {
        Type::List(t) | Type::Option(t) | Type::Weak(t) => vec![(**t).clone()],
        Type::Map(k, v) | Type::Result(k, v) => vec![(**k).clone(), (**v).clone()],
        _ => vec![],
    }
}

/// A receiver's lookup and its name list, paired so no caller can reach
/// one without the other.
type MethodTable = (fn(&str) -> Option<Scheme>, &'static [&'static str]);

/// The table of builtin methods `recv` has, if it has one.
fn method_table(recv: &Type) -> Option<MethodTable> {
    Some(match recv {
        Type::Str => (str_method, STR_METHODS),
        Type::List(_) => (list_method, LIST_METHODS),
        Type::Map(..) => (map_method, MAP_METHODS),
        Type::Option(_) => (option_method, OPTION_METHODS),
        Type::Result(..) => (result_method, RESULT_METHODS),
        Type::Weak(_) => (weak_method, WEAK_METHODS),
        _ => return None,
    })
}

method_table! {
    str_method / STR_METHODS;
    let st = || Type::Str;
    // `len` counts chars (documented); `bytes_len` counts bytes.
    "len" => s(vec![], Type::Int, StrLen),
    "bytes_len" => s(vec![], Type::Int, StrBytesLen),
    "is_empty" => s(vec![], Type::Bool, StrIsEmpty),
    "split" => s(vec![st()], list(st()), StrSplit),
    "trim" => s(vec![], st(), StrTrim),
    "trim_start" => s(vec![], st(), StrTrimStart),
    "trim_end" => s(vec![], st(), StrTrimEnd),
    "to_upper" => s(vec![], st(), StrToUpper),
    "to_lower" => s(vec![], st(), StrToLower),
    "starts_with" => s(vec![st()], Type::Bool, StrStartsWith),
    "ends_with" => s(vec![st()], Type::Bool, StrEndsWith),
    "contains" => s(vec![st()], Type::Bool, StrContains),
    "find" => s(vec![st()], opt(Type::Int), StrFind),
    "replace" => s(vec![st(), st()], st(), StrReplace),
    "repeat" => s(vec![Type::Int], st(), StrRepeat),
    "pad_left" => s(vec![Type::Int, st()], st(), StrPadLeft),
    "pad_right" => s(vec![Type::Int, st()], st(), StrPadRight),
    "chars" => s(vec![], list(Type::Char), StrChars),
    "slice" => s(vec![Type::Int, Type::Int], st(), StrSlice),
    "parse_int" => s(vec![], opt(Type::Int), StrParseInt),
    "parse_float" => s(vec![], opt(Type::Float), StrParseFloat),
}

method_table! {
    list_method / LIST_METHODS;
    "len" => s(vec![], Type::Int, ListLen),
    "is_empty" => s(vec![], Type::Bool, ListIsEmpty),
    "push" => s(vec![p(0)], Type::Unit, ListPush),
    "pop" => s(vec![], opt(p(0)), ListPop),
    "get" => s(vec![Type::Int], opt(p(0)), ListGet),
    "set" => s(vec![Type::Int, p(0)], Type::Unit, ListSet),
    "insert" => s(vec![Type::Int, p(0)], Type::Unit, ListInsert),
    "remove" => s(vec![Type::Int], p(0), ListRemove),
    "clear" => s(vec![], Type::Unit, ListClear),
    "contains" => Scheme {
        constraint: Some(SchemeConstraint::EqElem),
        ..s(vec![p(0)], Type::Bool, ListContains)
    },
    "index_of" => Scheme {
        constraint: Some(SchemeConstraint::EqElem),
        ..s(vec![p(0)], opt(Type::Int), ListIndexOf)
    },
    "reverse" => s(vec![], Type::Unit, ListReverse),
    "sort" => Scheme {
        constraint: Some(SchemeConstraint::OrdElem),
        ..s(vec![], Type::Unit, ListSort)
    },
    "join" => Scheme {
        constraint: Some(SchemeConstraint::StrElem),
        ..s(vec![Type::Str], Type::Str, ListJoin)
    },
    "map" => Scheme {
        fresh: 1,
        ..s(vec![func(vec![p(0)], p(1))], list(p(1)), ListMap)
    },
    "filter" => s(vec![func(vec![p(0)], Type::Bool)], list(p(0)), ListFilter),
    "fold" => Scheme {
        fresh: 1,
        ..s(vec![p(1), func(vec![p(1), p(0)], p(1))], p(1), ListFold)
    },
    "first" => s(vec![], opt(p(0)), ListFirst),
    "last" => s(vec![], opt(p(0)), ListLast),
    "slice" => s(vec![Type::Int, Type::Int], list(p(0)), ListSlice),
    "concat" => s(vec![list(p(0))], list(p(0)), ListConcat),
    "clone" => s(vec![], list(p(0)), ListClone),
    "any" => s(vec![func(vec![p(0)], Type::Bool)], Type::Bool, ListAny),
    "all" => s(vec![func(vec![p(0)], Type::Bool)], Type::Bool, ListAll),
    "find" => s(vec![func(vec![p(0)], Type::Bool)], opt(p(0)), ListFind),
    "position" => s(
        vec![func(vec![p(0)], Type::Bool)],
        opt(Type::Int),
        ListPosition,
    ),
    "count" => s(vec![func(vec![p(0)], Type::Bool)], Type::Int, ListCount),
    // `sum`'s builtin is refined to ListSumFloat by the checker when
    // the element type resolves to float (see apply_scheme).
    "sum" => Scheme {
        constraint: Some(SchemeConstraint::NumElem),
        ..s(vec![], p(0), ListSumInt)
    },
    "min" => Scheme {
        constraint: Some(SchemeConstraint::OrdElem),
        ..s(vec![], opt(p(0)), ListMin)
    },
    "max" => Scheme {
        constraint: Some(SchemeConstraint::OrdElem),
        ..s(vec![], opt(p(0)), ListMax)
    },
    "sort_by" => s(
        vec![func(vec![p(0), p(0)], Type::Int)],
        Type::Unit,
        ListSortBy,
    ),
    "map_indexed" => Scheme {
        fresh: 1,
        ..s(
            vec![func(vec![Type::Int, p(0)], p(1))],
            list(p(1)),
            ListMapIndexed,
        )
    },
    "zip_with" => Scheme {
        fresh: 2,
        ..s(
            vec![list(p(1)), func(vec![p(0), p(1)], p(2))],
            list(p(2)),
            ListZipWith,
        )
    },
}

method_table! {
    map_method / MAP_METHODS;
    let map_ty = Type::Map(Box::new(p(0)), Box::new(p(1)));
    "len" => s(vec![], Type::Int, MapLen),
    "is_empty" => s(vec![], Type::Bool, MapIsEmpty),
    "insert" => s(vec![p(0), p(1)], Type::Unit, MapInsert),
    "remove" => s(vec![p(0)], opt(p(1)), MapRemove),
    "get" => s(vec![p(0)], opt(p(1)), MapGet),
    "contains_key" => s(vec![p(0)], Type::Bool, MapContainsKey),
    "keys" => s(vec![], list(p(0)), MapKeys),
    "values" => s(vec![], list(p(1)), MapValues),
    "clear" => s(vec![], Type::Unit, MapClear),
    "clone" => s(vec![], map_ty.clone(), MapClone),
    // Two-parameter (key, value) closures — the tuple-free surface.
    "each" => s(
        vec![func(vec![p(0), p(1)], Type::Unit)],
        Type::Unit,
        MapEach,
    ),
    "map" => Scheme {
        fresh: 1,
        ..s(
            vec![func(vec![p(0), p(1)], p(2))],
            list(p(2)),
            MapMapEntries,
        )
    },
    "filter" => s(vec![func(vec![p(0), p(1)], Type::Bool)], map_ty, MapFilter),
}

method_table! {
    option_method / OPTION_METHODS;
    "is_some" => s(vec![], Type::Bool, OptionIsSome),
    "is_none" => s(vec![], Type::Bool, OptionIsNone),
    "unwrap" => s(vec![], p(0), OptionUnwrap),
    "unwrap_or" => s(vec![p(0)], p(0), OptionUnwrapOr),
    "expect" => s(vec![Type::Str], p(0), OptionExpect),
}

method_table! {
    result_method / RESULT_METHODS;
    "is_ok" => s(vec![], Type::Bool, ResultIsOk),
    "is_err" => s(vec![], Type::Bool, ResultIsErr),
    "unwrap" => s(vec![], p(0), ResultUnwrap),
    "unwrap_or" => s(vec![p(0)], p(0), ResultUnwrapOr),
    "unwrap_err" => s(vec![], p(1), ResultUnwrapErr),
    "expect" => s(vec![Type::Str], p(0), ResultExpect),
}

method_table! {
    weak_method / WEAK_METHODS;
    "upgrade" => s(vec![], opt(p(0)), WeakUpgrade),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The enumeration and the lookup are one declaration; this pins the
    /// property a consumer relies on — nothing is listed that cannot be
    /// resolved, on every receiver that has a table.
    #[test]
    fn every_enumerated_method_resolves() {
        let receivers = [
            Type::Str,
            Type::List(Box::new(Type::Int)),
            Type::Map(Box::new(Type::Str), Box::new(Type::Int)),
            Type::Option(Box::new(Type::Int)),
            Type::Result(Box::new(Type::Int), Box::new(Type::Str)),
            Type::Weak(Box::new(Type::Int)),
        ];
        for recv in receivers {
            let listed = builtin_methods(&recv);
            assert!(!listed.is_empty(), "no methods listed for {recv:?}");
            for (name, _) in listed {
                assert!(
                    builtin_method(&recv, name).is_some(),
                    "`{name}` is listed on {recv:?} but does not resolve"
                );
            }
        }
        assert!(builtin_methods(&Type::Int).is_empty());
    }

    /// Receiver type arguments are substituted, so completion shows
    /// `push(int)` on a `List[int]` rather than `push(T)`.
    #[test]
    fn listed_signatures_bind_the_receivers_arguments() {
        let listed = builtin_methods(&Type::List(Box::new(Type::Int)));
        let push = listed.iter().find(|(n, _)| *n == "push").unwrap();
        assert_eq!(push.1.params, vec![Type::Int]);
        // A scheme-local parameter has nothing to bind to and stays one.
        let map = listed.iter().find(|(n, _)| *n == "map").unwrap();
        assert_eq!(map.1.ret, Type::List(Box::new(Type::Param(0))));
    }
}
