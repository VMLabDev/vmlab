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

/// Look up a builtin method on a non-nominal receiver type.
pub fn builtin_method(recv: &Type, name: &str) -> Option<Scheme> {
    match recv {
        Type::Str => str_method(name),
        Type::List(_) => list_method(name),
        Type::Map(..) => map_method(name),
        Type::Option(_) => option_method(name),
        Type::Result(..) => result_method(name),
        Type::Weak(_) => weak_method(name),
        _ => None,
    }
}

fn str_method(name: &str) -> Option<Scheme> {
    use Builtin::*;
    let st = || Type::Str;
    Some(match name {
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
        _ => return None,
    })
}

fn list_method(name: &str) -> Option<Scheme> {
    use Builtin::*;
    Some(match name {
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
        _ => return None,
    })
}

fn map_method(name: &str) -> Option<Scheme> {
    use Builtin::*;
    let map_ty = Type::Map(Box::new(p(0)), Box::new(p(1)));
    Some(match name {
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
        _ => return None,
    })
}

fn option_method(name: &str) -> Option<Scheme> {
    use Builtin::*;
    Some(match name {
        "is_some" => s(vec![], Type::Bool, OptionIsSome),
        "is_none" => s(vec![], Type::Bool, OptionIsNone),
        "unwrap" => s(vec![], p(0), OptionUnwrap),
        "unwrap_or" => s(vec![p(0)], p(0), OptionUnwrapOr),
        "expect" => s(vec![Type::Str], p(0), OptionExpect),
        _ => return None,
    })
}

fn result_method(name: &str) -> Option<Scheme> {
    use Builtin::*;
    Some(match name {
        "is_ok" => s(vec![], Type::Bool, ResultIsOk),
        "is_err" => s(vec![], Type::Bool, ResultIsErr),
        "unwrap" => s(vec![], p(0), ResultUnwrap),
        "unwrap_or" => s(vec![p(0)], p(0), ResultUnwrapOr),
        "unwrap_err" => s(vec![], p(1), ResultUnwrapErr),
        "expect" => s(vec![Type::Str], p(0), ResultExpect),
        _ => return None,
    })
}

fn weak_method(name: &str) -> Option<Scheme> {
    Some(match name {
        "upgrade" => s(vec![], opt(p(0)), Builtin::WeakUpgrade),
        _ => return None,
    })
}
