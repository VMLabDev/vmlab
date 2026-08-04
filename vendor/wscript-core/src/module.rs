#![allow(non_snake_case)] // macro-generated value bindings reuse type param names

//! Host module registration (PRD §6.1/6.2): `Module::new("term")`,
//! `m.fn_("print_at", |x: i64, y: i64, s: &str| ...)`,
//! `m.const_("MAX_PANES", 16i64)`, `m.ty::<Pane>().method(...)`.
//!
//! Registration is one closure per function with plain Rust signatures —
//! the machinery here captures the **type signature** for the checker, so
//! misusing a host API from a script is a compile-time error.
//!
//! Supported parameter types: anything `FromValue + ScriptType` (primitives,
//! `String`, `Vec<T>`, `HashMap<K, V>`, `Option<T>`, `Result<T, E>`, and
//! `#[derive(Script)]` data types), plus `&str` in any position up to four
//! parameters. Methods on `#[script(opaque)]` types take `&T` / `&mut T`
//! receivers. Return types are anything `IntoValue + ScriptType`.

use std::any::TypeId;
use std::rc::Rc;
use std::sync::Arc;

use crate::bytecode::Const;
use crate::defs::{DefId, DefTable};
use crate::host::{
    FromValue, HostCallable, HostCtx, HostError, IntoValue, ScriptOpaque, ScriptType, type_mismatch,
};
use crate::registry::{HostFnEntry, HostMethod, ModuleDef, Registry};
use crate::types::{FnSig, Type};
use crate::value::Value;

type SigFn = Box<dyn Fn(&mut DefTable) -> FnSig + Send + Sync>;
type RegisterFn = Box<dyn Fn(&mut DefTable) -> DefId + Send + Sync>;

struct StagedFn {
    name: String,
    sig: SigFn,
    imp: Arc<dyn HostCallable>,
    doc: Option<String>,
}

struct StagedMethod {
    register: RegisterFn,
    name: String,
    sig: SigFn,
    imp: Arc<dyn HostCallable>,
    doc: Option<String>,
}

/// A named collection of host functions, constants and types. Build one,
/// hand it to `Context::module(...)`; scripts then `use <name>`.
pub struct Module {
    name: String,
    doc: Option<String>,
    fns: Vec<StagedFn>,
    consts: Vec<(String, Type, Const)>,
    types: Vec<RegisterFn>,
    methods: Vec<StagedMethod>,
    /// Pending doc comment applied to the next registration.
    next_doc: Option<String>,
}

impl Module {
    pub fn new(name: impl Into<String>) -> Module {
        Module {
            name: name.into(),
            doc: None,
            fns: Vec::new(),
            consts: Vec::new(),
            types: Vec::new(),
            methods: Vec::new(),
            next_doc: None,
        }
    }

    /// Documentation for the module itself (shown by the LSP).
    pub fn doc(&mut self, text: impl Into<String>) -> &mut Self {
        self.doc = Some(text.into());
        self
    }

    /// Documentation attached to the *next* registered item.
    pub fn doc_next(&mut self, text: impl Into<String>) -> &mut Self {
        self.next_doc = Some(text.into());
        self
    }

    /// Register a host function under this module.
    pub fn fn_<M: 'static, F: HostFunction<M>>(
        &mut self,
        name: impl Into<String>,
        f: F,
    ) -> &mut Self {
        self.fns.push(StagedFn {
            name: name.into(),
            sig: Box::new(F::sig),
            imp: f.into_callable(),
            doc: self.next_doc.take(),
        });
        self
    }

    /// Register a constant.
    pub fn const_<C: IntoConst>(&mut self, name: impl Into<String>, value: C) -> &mut Self {
        let (ty, c) = value.into_const();
        self.consts.push((name.into(), ty, c));
        self
    }

    /// Register a `#[derive(Script)]` type under this module and get a
    /// builder for attaching methods (opaque types).
    pub fn ty<T: ScriptType + 'static>(&mut self) -> TypeBuilder<'_, T> {
        self.types.push(Box::new(|defs| nominal_id::<T>(defs)));
        TypeBuilder {
            module: self,
            _pd: std::marker::PhantomData,
        }
    }

    /// Consume the module into a registry (called by `Context::module`).
    pub fn merge_into(self, reg: &mut Registry) {
        let mut def = ModuleDef {
            name: self.name,
            fns: Vec::new(),
            consts: self.consts,
            types: Vec::new(),
            doc: self.doc,
        };
        for register in self.types {
            let id = register(&mut reg.defs);
            if !def.types.contains(&id) {
                def.types.push(id);
            }
        }
        for staged in self.fns {
            let sig = (staged.sig)(&mut reg.defs);
            let idx = reg.push_host_fn(HostFnEntry {
                sig: sig.clone(),
                imp: staged.imp,
            });
            def.fns.push((staged.name, sig, idx, staged.doc));
        }
        for m in self.methods {
            let type_def = (m.register)(&mut reg.defs);
            let sig = (m.sig)(&mut reg.defs);
            let idx = reg.push_host_fn(HostFnEntry {
                sig: sig.clone(),
                imp: m.imp,
            });
            reg.methods.entry(type_def).or_default().push(HostMethod {
                name: m.name,
                sig,
                host_idx: idx,
                doc: m.doc,
            });
        }
        reg.modules.push(def);
    }
}

/// Resolve a `ScriptType` to its nominal def id (registering it if new).
/// Panics if `T` maps to a structural type — that is a host programming
/// error, caught at registration time, never at script runtime.
fn nominal_id<T: ScriptType>(defs: &mut DefTable) -> DefId {
    match T::script_type(defs) {
        Type::Named(id) => id,
        other => panic!(
            "Module::ty/method requires a #[derive(Script)] nominal type; \
             {} maps to the structural type {}",
            std::any::type_name::<T>(),
            other.display(defs)
        ),
    }
}

/// Builder returned by [`Module::ty`]: attach methods to an opaque type.
pub struct TypeBuilder<'m, T> {
    module: &'m mut Module,
    _pd: std::marker::PhantomData<T>,
}

impl<'m, T: ScriptType + 'static> TypeBuilder<'m, T> {
    pub fn method<M: 'static, F: HostMethodFn<T, M>>(
        &mut self,
        name: impl Into<String>,
        f: F,
    ) -> &mut Self {
        self.module.methods.push(StagedMethod {
            register: Box::new(|defs| nominal_id::<T>(defs)),
            name: name.into(),
            sig: Box::new(F::sig),
            imp: f.into_callable(),
            doc: self.module.next_doc.take(),
        });
        self
    }
}

// --------------------------------------------------------------- consts

/// Types usable as registered module constants.
pub trait IntoConst {
    fn into_const(self) -> (Type, Const);
}

macro_rules! into_const {
    ($rust:ty, $ty:expr, $build:expr) => {
        impl IntoConst for $rust {
            fn into_const(self) -> (Type, Const) {
                ($ty, $build(self))
            }
        }
    };
}

into_const!(i64, Type::Int, Const::Int);
into_const!(i32, Type::Int, |v: i32| Const::Int(v as i64));
into_const!(f64, Type::Float, Const::Float);
into_const!(bool, Type::Bool, Const::Bool);
into_const!(char, Type::Char, Const::Char);
into_const!(&str, Type::Str, |v: &str| Const::Str(Arc::from(v)));
into_const!(String, Type::Str, |v: String| Const::Str(Arc::from(
    v.as_str()
)));

// ---------------------------------------------------- function adapters

/// A Rust closure registrable as a host function. `Marker` encodes the
/// parameter shape so owned-arg and `&str`-arg impls coexist.
pub trait HostFunction<Marker>: Send + Sync + 'static {
    fn sig(defs: &mut DefTable) -> FnSig;
    fn into_callable(self) -> Arc<dyn HostCallable>;
}

/// Marker: a parameter taken by value.
pub struct OwnArg<T>(std::marker::PhantomData<T>);
/// Marker: a leading `&mut dyn HostCtx` parameter (excluded from the
/// script-visible signature) — lets a host fn re-enter the VM for
/// script callbacks (`ScriptClosure`).
pub struct CtxArg;
/// Marker: a `&str` parameter.
pub struct StrArg;
/// Marker: `&T` opaque receiver.
pub struct RecvRef;
/// Marker: `&mut T` opaque receiver.
pub struct RecvMut;

fn str_of(v: &Value) -> Result<Rc<str>, HostError> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        other => Err(type_mismatch("string", other)),
    }
}

fn check_arity(args: &[Value], n: usize) -> Result<(), HostError> {
    if args.len() == n {
        Ok(())
    } else {
        Err(HostError::msg(format!(
            "host function expected {n} argument(s), got {}",
            args.len()
        )))
    }
}

macro_rules! impl_host_fn_owned {
    ($n:expr; $($A:ident : $idx:tt),*) => {
        impl<Func, Ret, $($A,)*> HostFunction<($(OwnArg<$A>,)*)> for Func
        where
            Func: Fn($($A),*) -> Ret + Send + Sync + 'static,
            Ret: IntoValue + ScriptType,
            $($A: FromValue + ScriptType + 'static,)*
        {
            fn sig(defs: &mut DefTable) -> FnSig {
                FnSig::new(
                    vec![$(<$A as ScriptType>::script_type(defs)),*],
                    <Ret as ScriptType>::script_type(defs),
                )
            }
            fn into_callable(self) -> Arc<dyn HostCallable> {
                Arc::new(
                    move |ctx: &mut dyn HostCtx, args: Vec<Value>| -> Result<Value, HostError> {
                        check_arity(&args, $n)?;
                        #[allow(unused_variables)]
                        let defs = ctx.defs();
                        $(let $A = <$A as FromValue>::from_value(args[$idx].clone(), defs)?;)*
                        let ret = (self)($($A),*);
                        ret.into_value(ctx.defs())
                    },
                )
            }
        }
    };
}

/// Host fns whose first Rust parameter is `&mut dyn HostCtx`: the ctx
/// gives access to `HostCtx::call_value` for invoking `ScriptClosure`
/// parameters; it does not appear in the script-visible signature.
macro_rules! impl_host_fn_ctx {
    ($n:expr; $($A:ident : $idx:tt),*) => {
        impl<Func, Ret, $($A,)*> HostFunction<(CtxArg, $(OwnArg<$A>,)*)> for Func
        where
            Func: Fn(&mut dyn HostCtx, $($A),*) -> Ret + Send + Sync + 'static,
            Ret: IntoValue + ScriptType,
            $($A: FromValue + ScriptType + 'static,)*
        {
            fn sig(defs: &mut DefTable) -> FnSig {
                FnSig::new(
                    vec![$(<$A as ScriptType>::script_type(defs)),*],
                    <Ret as ScriptType>::script_type(defs),
                )
            }
            fn into_callable(self) -> Arc<dyn HostCallable> {
                Arc::new(
                    move |ctx: &mut dyn HostCtx, args: Vec<Value>| -> Result<Value, HostError> {
                        check_arity(&args, $n)?;
                        // Conversions end their borrow of ctx before the
                        // closure takes it mutably.
                        $(let $A = {
                            #[allow(unused_variables)]
                            let defs = ctx.defs();
                            <$A as FromValue>::from_value(args[$idx].clone(), defs)?
                        };)*
                        let ret = (self)(ctx, $($A),*);
                        ret.into_value(ctx.defs())
                    },
                )
            }
        }
    };
}

impl_host_fn_ctx!(0;);
impl_host_fn_ctx!(1; A0: 0);
impl_host_fn_ctx!(2; A0: 0, A1: 1);
impl_host_fn_ctx!(3; A0: 0, A1: 1, A2: 2);
impl_host_fn_ctx!(4; A0: 0, A1: 1, A2: 2, A3: 3);

impl_host_fn_owned!(0;);
impl_host_fn_owned!(1; A0: 0);
impl_host_fn_owned!(2; A0: 0, A1: 1);
impl_host_fn_owned!(3; A0: 0, A1: 1, A2: 2);
impl_host_fn_owned!(4; A0: 0, A1: 1, A2: 2, A3: 3);
impl_host_fn_owned!(5; A0: 0, A1: 1, A2: 2, A3: 3, A4: 4);
impl_host_fn_owned!(6; A0: 0, A1: 1, A2: 2, A3: 3, A4: 4, A5: 5);
impl_host_fn_owned!(7; A0: 0, A1: 1, A2: 2, A3: 3, A4: 4, A5: 5, A6: 6);
impl_host_fn_owned!(8; A0: 0, A1: 1, A2: 2, A3: 3, A4: 4, A5: 5, A6: 6, A7: 7);

/// Mixed `&str`/owned parameter impls (arities 1-4). Each parameter spec
/// is `own NAME : idx` (NAME doubles as the generic parameter) or
/// `str NAME : idx` (NAME is the extraction temporary).
macro_rules! impl_host_fn_mixed {
    ($n:expr; [$($G:ident),*]; $(($kind:ident $name:ident : $idx:tt)),*) => {
        impl<Func, Ret, $($G,)*> HostFunction<($(mixed_marker!($kind $name),)*)> for Func
        where
            Func: Fn($(mixed_param!($kind $name)),*) -> Ret + Send + Sync + 'static,
            Ret: IntoValue + ScriptType,
            $($G: FromValue + ScriptType + 'static,)*
        {
            fn sig(defs: &mut DefTable) -> FnSig {
                FnSig::new(
                    vec![$(mixed_type!($kind $name, defs)),*],
                    <Ret as ScriptType>::script_type(defs),
                )
            }
            fn into_callable(self) -> Arc<dyn HostCallable> {
                Arc::new(
                    move |ctx: &mut dyn HostCtx, args: Vec<Value>| -> Result<Value, HostError> {
                        check_arity(&args, $n)?;
                        #[allow(unused_variables)]
                        let defs = ctx.defs();
                        $(mixed_extract!($kind $name : $idx, args, defs);)*
                        let ret = (self)($(mixed_use!($kind $name)),*);
                        ret.into_value(ctx.defs())
                    },
                )
            }
        }
    };
}

macro_rules! mixed_marker {
    (own $A:ident) => { OwnArg<$A> };
    (str $S:ident) => { StrArg };
}
macro_rules! mixed_param {
    (own $A:ident) => {
        $A
    };
    (str $S:ident) => {
        &str
    };
}
macro_rules! mixed_type {
    (own $A:ident, $defs:ident) => {
        <$A as ScriptType>::script_type($defs)
    };
    (str $S:ident, $defs:ident) => {
        Type::Str
    };
}
macro_rules! mixed_extract {
    (own $A:ident : $idx:tt, $args:ident, $defs:ident) => {
        let $A = <$A as FromValue>::from_value($args[$idx].clone(), $defs)?;
    };
    (str $S:ident : $idx:tt, $args:ident, $defs:ident) => {
        let $S = str_of(&$args[$idx])?;
    };
}
macro_rules! mixed_use {
    (own $A:ident) => {
        $A
    };
    (str $S:ident) => {
        &$S
    };
}

// Arity 1.
impl_host_fn_mixed!(1; []; (str s0: 0));
// Arity 2.
impl_host_fn_mixed!(2; [A1]; (str s0: 0), (own A1: 1));
impl_host_fn_mixed!(2; [A0]; (own A0: 0), (str s1: 1));
impl_host_fn_mixed!(2; []; (str s0: 0), (str s1: 1));
// Arity 3.
impl_host_fn_mixed!(3; [A1, A2]; (str s0: 0), (own A1: 1), (own A2: 2));
impl_host_fn_mixed!(3; [A0, A2]; (own A0: 0), (str s1: 1), (own A2: 2));
impl_host_fn_mixed!(3; [A0, A1]; (own A0: 0), (own A1: 1), (str s2: 2));
impl_host_fn_mixed!(3; [A2]; (str s0: 0), (str s1: 1), (own A2: 2));
impl_host_fn_mixed!(3; [A1]; (str s0: 0), (own A1: 1), (str s2: 2));
impl_host_fn_mixed!(3; [A0]; (own A0: 0), (str s1: 1), (str s2: 2));
impl_host_fn_mixed!(3; []; (str s0: 0), (str s1: 1), (str s2: 2));
// Arity 4.
impl_host_fn_mixed!(4; [A1, A2, A3]; (str s0: 0), (own A1: 1), (own A2: 2), (own A3: 3));
impl_host_fn_mixed!(4; [A0, A2, A3]; (own A0: 0), (str s1: 1), (own A2: 2), (own A3: 3));
impl_host_fn_mixed!(4; [A0, A1, A3]; (own A0: 0), (own A1: 1), (str s2: 2), (own A3: 3));
impl_host_fn_mixed!(4; [A0, A1, A2]; (own A0: 0), (own A1: 1), (own A2: 2), (str s3: 3));

// ------------------------------------------------------ method adapters

/// A Rust closure registrable as a method on an opaque type: first
/// parameter is `&T` or `&mut T`. Borrow conflicts at call time surface as
/// `Err`, never panics (PRD §6.5).
pub trait HostMethodFn<T, Marker>: Send + Sync + 'static {
    /// Signature *excluding* the receiver.
    fn sig(defs: &mut DefTable) -> FnSig;
    /// Callable taking the receiver handle as `args[0]`.
    fn into_callable(self) -> Arc<dyn HostCallable>;
}

pub fn with_opaque_ref<T: 'static, R>(v: &Value, f: impl FnOnce(&T) -> R) -> Result<R, HostError> {
    match v {
        Value::Opaque(cell) => {
            let guard = cell.cell.try_borrow().map_err(|_| {
                HostError::msg(
                    "aliasing violation: opaque value is already mutably borrowed at the \
                     host boundary",
                )
            })?;
            let t = guard
                .downcast_ref::<T>()
                .ok_or_else(|| HostError::msg("opaque handle holds a different Rust type"))?;
            Ok(f(t))
        }
        other => Err(type_mismatch("opaque handle", other)),
    }
}

pub fn with_opaque_mut<T: 'static, R>(
    v: &Value,
    f: impl FnOnce(&mut T) -> R,
) -> Result<R, HostError> {
    match v {
        Value::Opaque(cell) => {
            let mut guard = cell.cell.try_borrow_mut().map_err(|_| {
                HostError::msg(
                    "aliasing violation: opaque value is already borrowed at the host \
                     boundary",
                )
            })?;
            let t = guard
                .downcast_mut::<T>()
                .ok_or_else(|| HostError::msg("opaque handle holds a different Rust type"))?;
            Ok(f(t))
        }
        other => Err(type_mismatch("opaque handle", other)),
    }
}

macro_rules! impl_host_method {
    ($n:expr; $recv_marker:ident, $recv_ty:ty, $with:ident; $($A:ident : $idx:tt),*) => {
        impl<Func, Ret, T, $($A,)*> HostMethodFn<T, ($recv_marker, $(OwnArg<$A>,)*)> for Func
        where
            T: ScriptOpaque,
            Func: Fn($recv_ty, $($A),*) -> Ret + Send + Sync + 'static,
            Ret: IntoValue + ScriptType,
            $($A: FromValue + ScriptType + 'static,)*
        {
            fn sig(defs: &mut DefTable) -> FnSig {
                FnSig::new(
                    vec![$(<$A as ScriptType>::script_type(defs)),*],
                    <Ret as ScriptType>::script_type(defs),
                )
            }
            fn into_callable(self) -> Arc<dyn HostCallable> {
                Arc::new(
                    move |ctx: &mut dyn HostCtx, args: Vec<Value>| -> Result<Value, HostError> {
                        check_arity(&args, $n + 1)?;
                        #[allow(unused_variables)]
                        let defs = ctx.defs();
                        $(let $A = <$A as FromValue>::from_value(args[1 + $idx].clone(), defs)?;)*
                        let ret = $with::<T, Ret>(&args[0], |t| (self)(t, $($A),*))?;
                        ret.into_value(ctx.defs())
                    },
                )
            }
        }
    };
}

impl_host_method!(0; RecvRef, &T, with_opaque_ref;);
impl_host_method!(1; RecvRef, &T, with_opaque_ref; A0: 0);
impl_host_method!(2; RecvRef, &T, with_opaque_ref; A0: 0, A1: 1);
impl_host_method!(3; RecvRef, &T, with_opaque_ref; A0: 0, A1: 1, A2: 2);
impl_host_method!(4; RecvRef, &T, with_opaque_ref; A0: 0, A1: 1, A2: 2, A3: 3);
impl_host_method!(0; RecvMut, &mut T, with_opaque_mut;);
impl_host_method!(1; RecvMut, &mut T, with_opaque_mut; A0: 0);
impl_host_method!(2; RecvMut, &mut T, with_opaque_mut; A0: 0, A1: 1);
impl_host_method!(3; RecvMut, &mut T, with_opaque_mut; A0: 0, A1: 1, A2: 2);
impl_host_method!(4; RecvMut, &mut T, with_opaque_mut; A0: 0, A1: 1, A2: 2, A3: 3);

/// `&str` single-arg method variants (e.g. `|p: &mut Pane, title: &str|`).
macro_rules! impl_host_method_str1 {
    ($recv_marker:ident, $recv_ty:ty, $with:ident) => {
        impl<Func, Ret, T> HostMethodFn<T, ($recv_marker, StrArg)> for Func
        where
            T: ScriptOpaque,
            Func: Fn($recv_ty, &str) -> Ret + Send + Sync + 'static,
            Ret: IntoValue + ScriptType,
        {
            fn sig(defs: &mut DefTable) -> FnSig {
                FnSig::new(vec![Type::Str], <Ret as ScriptType>::script_type(defs))
            }
            fn into_callable(self) -> Arc<dyn HostCallable> {
                Arc::new(
                    move |ctx: &mut dyn HostCtx, args: Vec<Value>| -> Result<Value, HostError> {
                        check_arity(&args, 2)?;
                        let s = str_of(&args[1])?;
                        let ret = $with::<T, Ret>(&args[0], |t| (self)(t, &s))?;
                        ret.into_value(ctx.defs())
                    },
                )
            }
        }
    };
}

impl_host_method_str1!(RecvRef, &T, with_opaque_ref);
impl_host_method_str1!(RecvMut, &mut T, with_opaque_mut);

/// Insert (or find) the registered opaque def for `T` and wrap a host
/// value into a live handle — used by `#[derive(Script)]`'s generated
/// `IntoValue` for opaque types.
pub fn opaque_into_value<T: ScriptOpaque>(value: T, defs: &DefTable) -> Result<Value, HostError> {
    let Some(def) = defs.by_rust_type(TypeId::of::<T>()) else {
        return Err(HostError::msg(format!(
            "type {} is not registered in this context (register it with \
             Module::ty or Context::register_type)",
            std::any::type_name::<T>()
        )));
    };
    Ok(Value::Opaque(Rc::new(crate::value::OpaqueCell {
        def,
        cell: std::cell::RefCell::new(Box::new(value)),
    })))
}

/// Marker: data-type receiver (converted by value; methods are read-only
/// accessors — mutations to the receiver are not written back).
pub struct RecvData;

macro_rules! impl_host_method_data {
    ($n:expr; $($A:ident : $idx:tt),*) => {
        impl<Func, Ret, T, $($A,)*> HostMethodFn<T, (RecvData, $(OwnArg<$A>,)*)> for Func
        where
            T: FromValue + ScriptType + 'static,
            Func: Fn(&T, $($A),*) -> Ret + Send + Sync + 'static,
            Ret: IntoValue + ScriptType,
            $($A: FromValue + ScriptType + 'static,)*
        {
            fn sig(defs: &mut DefTable) -> FnSig {
                FnSig::new(
                    vec![$(<$A as ScriptType>::script_type(defs)),*],
                    <Ret as ScriptType>::script_type(defs),
                )
            }
            fn into_callable(self) -> Arc<dyn HostCallable> {
                Arc::new(
                    move |ctx: &mut dyn HostCtx, args: Vec<Value>| -> Result<Value, HostError> {
                        check_arity(&args, $n + 1)?;
                        let defs = ctx.defs();
                        let recv = <T as FromValue>::from_value(args[0].clone(), defs)?;
                        $(let $A = <$A as FromValue>::from_value(args[1 + $idx].clone(), defs)?;)*
                        let ret = (self)(&recv, $($A),*);
                        ret.into_value(ctx.defs())
                    },
                )
            }
        }
    };
}

impl_host_method_data!(0;);
impl_host_method_data!(1; A0: 0);
impl_host_method_data!(2; A0: 0, A1: 1);
impl_host_method_data!(3; A0: 0, A1: 1, A2: 2);

/// `&str` single-arg data-receiver method variant.
impl<Func, Ret, T> HostMethodFn<T, (RecvData, StrArg)> for Func
where
    T: FromValue + ScriptType + 'static,
    Func: Fn(&T, &str) -> Ret + Send + Sync + 'static,
    Ret: IntoValue + ScriptType,
{
    fn sig(defs: &mut DefTable) -> FnSig {
        FnSig::new(vec![Type::Str], <Ret as ScriptType>::script_type(defs))
    }
    fn into_callable(self) -> Arc<dyn HostCallable> {
        Arc::new(
            move |ctx: &mut dyn HostCtx, args: Vec<Value>| -> Result<Value, HostError> {
                check_arity(&args, 2)?;
                let defs = ctx.defs();
                let recv = <T as FromValue>::from_value(args[0].clone(), defs)?;
                let s = str_of(&args[1])?;
                let ret = (self)(&recv, &s);
                ret.into_value(ctx.defs())
            },
        )
    }
}
