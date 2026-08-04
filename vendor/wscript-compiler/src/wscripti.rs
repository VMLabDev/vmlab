//! `.wscripti` interface consumption (PRD §9.1): parse an interface file
//! (with the normal parser — the grammar is a strict subset) and register
//! its declarations into a [`Registry`] so `wscript check` and the LSP can
//! typecheck scripts against host APIs they cannot link against.
//!
//! Functions get stub implementations that fault if a VM ever calls them
//! — interface registries are for checking, not running.

use std::collections::HashMap;
use std::sync::Arc;

use wscript_core::bytecode::Const;
use wscript_core::defs::{DefId, DefKind, EnumDef, StructDef, VariantDef, VariantKind};
use wscript_core::diag::Diagnostic;
use wscript_core::host::{HostCallable, HostCtx, HostError};
use wscript_core::registry::{HostFnEntry, HostMethod, ModuleDef, Registry};
use wscript_core::span::Span;
use wscript_core::types::{FnSig, Type};
use wscript_core::value::Value;

use crate::ast::*;

/// Source location of a declaration inside a `.wscripti` file — the LSP's
/// goto-definition target for host symbols (PRD §9, feature 3).
#[derive(Debug, Clone, Default)]
pub struct WscriptiIndex {
    /// `module::fn` and `module::const` → span.
    pub module_items: HashMap<(String, String), Span>,
    /// type name → span; (type, method) → span.
    pub types: HashMap<String, Span>,
    pub methods: HashMap<(String, String), Span>,
}

struct StubFn {
    name: String,
}

impl HostCallable for StubFn {
    fn call(&self, _ctx: &mut dyn HostCtx, _args: Vec<Value>) -> Result<Value, HostError> {
        Err(HostError::msg(format!(
            "`{}` comes from a .wscripti interface file and has no implementation \
             (interfaces are for checking, not running)",
            self.name
        )))
    }
}

/// Load one interface file's declarations into `reg`. Declarations whose
/// names already exist are skipped (the live registration wins — e.g. the
/// CLI registers the real stdlib and also reads std.wscripti via wscript.toml).
/// Returns diagnostics (parse errors and unsupported forms) and the
/// definition index for the LSP.
pub fn load(source: &str, reg: &mut Registry) -> (Vec<Diagnostic>, WscriptiIndex) {
    let parsed = crate::parse(source);
    let mut diags = parsed.diags;
    let mut index = WscriptiIndex::default();
    let mut loader = Loader {
        reg,
        diags: &mut diags,
        index: &mut index,
    };
    // Types first (signatures reference them), then modules.
    for item in &parsed.file.items {
        loader.load_type(item);
    }
    for item in &parsed.file.items {
        match item {
            Item::Mod(m) => loader.load_module(m),
            Item::Impl(im) => loader.load_impl(im),
            // Unit families are a script-level construct: at the host
            // boundary their values are just the backing number.
            Item::Units(u) => {
                let span = u.name.span;
                loader.error(
                    span,
                    "`units` declarations are not allowed in an interface file",
                );
            }
            _ => {}
        }
    }
    (diags, index)
}

struct Loader<'a> {
    reg: &'a mut Registry,
    diags: &'a mut Vec<Diagnostic>,
    index: &'a mut WscriptiIndex,
}

impl<'a> Loader<'a> {
    fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.diags.push(
            Diagnostic::error("E0271", span, msg).with_help(
                "this declaration in the .wscripti interface file could not be registered",
            ),
        );
    }

    fn name_taken(&self, name: &str) -> bool {
        self.reg.defs.defs.iter().any(|d| match d {
            DefKind::Struct(s) => s.name == name,
            DefKind::Enum(e) => e.name == name,
            DefKind::Trait(t) => t.name == name,
            DefKind::Unit(u) => u.name == name,
        })
    }

    fn load_type(&mut self, item: &Item) {
        match item {
            Item::Struct(s) => {
                self.index.types.insert(s.name.name.clone(), s.name.span);
                if self.name_taken(&s.name.name) {
                    return;
                }
                let id = self.reg.defs.push(DefKind::Struct(StructDef {
                    name: s.name.name.clone(),
                    fields: vec![],
                    opaque: s.opaque,
                    host: true,
                    rust_type: None,
                }));
                let fields: Vec<(String, Type)> = s
                    .fields
                    .iter()
                    .map(|f| (f.name.name.clone(), self.resolve_type(&f.ty)))
                    .collect();
                if let DefKind::Struct(sd) = &mut self.reg.defs.defs[id.index()] {
                    sd.fields = fields;
                }
            }
            Item::Enum(e) => {
                self.index.types.insert(e.name.name.clone(), e.name.span);
                if self.name_taken(&e.name.name) {
                    return;
                }
                let id = self.reg.defs.push(DefKind::Enum(EnumDef {
                    name: e.name.name.clone(),
                    variants: vec![],
                    host: true,
                    rust_type: None,
                }));
                let variants: Vec<VariantDef> = e
                    .variants
                    .iter()
                    .map(|v| {
                        let (kind, fields) = match &v.body {
                            VariantBody::Unit => (VariantKind::Unit, vec![]),
                            VariantBody::Tuple(tys) => (
                                VariantKind::Tuple,
                                tys.iter()
                                    .enumerate()
                                    .map(|(i, t)| (i.to_string(), self.resolve_type(t)))
                                    .collect(),
                            ),
                            VariantBody::Struct(fs) => (
                                VariantKind::Struct,
                                fs.iter()
                                    .map(|f| (f.name.name.clone(), self.resolve_type(&f.ty)))
                                    .collect(),
                            ),
                        };
                        VariantDef {
                            name: v.name.name.clone(),
                            kind,
                            fields,
                        }
                    })
                    .collect();
                if let DefKind::Enum(ed) = &mut self.reg.defs.defs[id.index()] {
                    ed.variants = variants;
                }
            }
            _ => {}
        }
    }

    fn load_module(&mut self, m: &ModDecl) {
        if self.reg.modules.iter().any(|x| x.name == m.name.name) {
            // Live registration wins; still index for goto-definition.
            for item in &m.items {
                if let Item::Fn(f) = item {
                    self.index
                        .module_items
                        .insert((m.name.name.clone(), f.name.name.clone()), f.name.span);
                } else if let Item::Const(c) = item {
                    self.index
                        .module_items
                        .insert((m.name.name.clone(), c.name.name.clone()), c.name.span);
                }
            }
            return;
        }
        let mut def = ModuleDef {
            name: m.name.name.clone(),
            fns: Vec::new(),
            consts: Vec::new(),
            types: Vec::new(),
            doc: m.doc.clone(),
        };
        for item in &m.items {
            match item {
                Item::Fn(f) => {
                    if !f.type_params.is_empty() {
                        self.diags.push(
                            Diagnostic::error(
                                "E0254",
                                f.name.span,
                                "generic host functions are not supported",
                            )
                            .with_help(
                                "host functions are monomorphic Rust closures; declare \
                                 concrete overloads instead",
                            ),
                        );
                        continue;
                    }
                    self.index
                        .module_items
                        .insert((m.name.name.clone(), f.name.name.clone()), f.name.span);
                    let sig = self.fn_sig(f);
                    let idx = self.reg.push_host_fn(HostFnEntry {
                        sig: sig.clone(),
                        imp: Arc::new(StubFn {
                            name: format!("{}::{}", m.name.name, f.name.name),
                        }),
                    });
                    def.fns.push((f.name.name.clone(), sig, idx, f.doc.clone()));
                }
                Item::Const(c) => {
                    self.index
                        .module_items
                        .insert((m.name.name.clone(), c.name.name.clone()), c.name.span);
                    let ty = self.resolve_type(&c.ty);
                    let placeholder = match ty {
                        Type::Int => Const::Int(0),
                        Type::Float => Const::Float(0.0),
                        Type::Bool => Const::Bool(false),
                        Type::Char => Const::Char('\0'),
                        Type::Str => Const::Str(Arc::from("")),
                        _ => Const::Unit,
                    };
                    def.consts.push((c.name.name.clone(), ty, placeholder));
                }
                other => {
                    let span = item_span(other);
                    self.error(span, "only fn and const declarations belong in mod blocks");
                }
            }
        }
        self.reg.modules.push(def);
    }

    fn load_impl(&mut self, im: &ImplDecl) {
        if im.trait_name.is_some() {
            self.error(im.span, "trait impls do not appear in interface files");
            return;
        }
        let Some(def_id) = self.find_type(&im.ty_name.name) else {
            self.error(
                im.ty_name.span,
                format!("unknown type `{}` in interface impl", im.ty_name.name),
            );
            return;
        };
        let already = self
            .reg
            .methods
            .get(&def_id)
            .map(|ms| !ms.is_empty())
            .unwrap_or(false);
        for f in &im.fns {
            self.index
                .methods
                .insert((im.ty_name.name.clone(), f.name.name.clone()), f.name.span);
            if already {
                continue; // live registration wins
            }
            // Drop the `self` receiver from the registered signature.
            let params: Vec<Type> = f
                .params
                .iter()
                .filter(|p| !p.is_self)
                .map(|p| match &p.ty {
                    Some(t) => self.resolve_type(t),
                    None => Type::Error,
                })
                .collect();
            let ret = f
                .ret
                .as_ref()
                .map(|t| self.resolve_type(t))
                .unwrap_or(Type::Unit);
            let sig = FnSig::new(params, ret);
            let idx = self.reg.push_host_fn(HostFnEntry {
                sig: sig.clone(),
                imp: Arc::new(StubFn {
                    name: format!("{}::{}", im.ty_name.name, f.name.name),
                }),
            });
            self.reg
                .methods
                .entry(def_id)
                .or_default()
                .push(HostMethod {
                    name: f.name.name.clone(),
                    sig,
                    host_idx: idx,
                    doc: f.doc.clone(),
                });
        }
    }

    fn find_type(&self, name: &str) -> Option<DefId> {
        self.reg
            .defs
            .defs
            .iter()
            .position(|d| match d {
                DefKind::Struct(s) => s.name == name,
                DefKind::Enum(e) => e.name == name,
                DefKind::Trait(_) | DefKind::Unit(_) => false,
            })
            .map(|i| DefId(i as u32))
    }

    fn fn_sig(&mut self, f: &FnDecl) -> FnSig {
        let params: Vec<Type> = f
            .params
            .iter()
            .filter(|p| !p.is_self)
            .map(|p| match &p.ty {
                Some(t) => self.resolve_type(t),
                None => Type::Error,
            })
            .collect();
        let ret = f
            .ret
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(Type::Unit);
        FnSig::new(params, ret)
    }

    /// Lightweight type resolution against the registry's def table
    /// (mirrors the checker's resolve_type for the interface subset).
    fn resolve_type(&mut self, t: &TypeExpr) -> Type {
        match &t.kind {
            TypeExprKind::Unit | TypeExprKind::Error => Type::Unit,
            TypeExprKind::Name(ident) => match ident.name.as_str() {
                "int" => Type::Int,
                "float" => Type::Float,
                "bool" => Type::Bool,
                "char" => Type::Char,
                "unit" => Type::Unit,
                "string" => Type::Str,
                other => match self.find_type(other) {
                    Some(id) => Type::Named(id),
                    None => {
                        self.error(ident.span, format!("unknown type `{other}` in interface"));
                        Type::Error
                    }
                },
            },
            TypeExprKind::App(ident, args) => {
                let mut resolved: Vec<Type> = args.iter().map(|a| self.resolve_type(a)).collect();
                resolved.resize(2, Type::Error);
                let b = Box::new(resolved.remove(1));
                let a = Box::new(resolved.remove(0));
                match ident.name.as_str() {
                    "List" => Type::List(a),
                    "Option" => Type::Option(a),
                    "weak" => Type::Weak(a),
                    "Map" => Type::Map(a, b),
                    "Result" => Type::Result(a, b),
                    other => {
                        self.error(ident.span, format!("`{other}` cannot take type arguments"));
                        Type::Error
                    }
                }
            }
            TypeExprKind::Fn(params, ret) => {
                let params: Vec<Type> = params.iter().map(|p| self.resolve_type(p)).collect();
                let ret = ret
                    .as_ref()
                    .map(|r| self.resolve_type(r))
                    .unwrap_or(Type::Unit);
                Type::Fn(Box::new(FnSig::new(params, ret)))
            }
            TypeExprKind::Dyn(ident) => {
                self.error(
                    ident.span,
                    "dyn types do not appear in interface files (host traits are v2)",
                );
                Type::Error
            }
        }
    }
}

fn item_span(item: &Item) -> Span {
    match item {
        Item::Use(u) => u.span,
        Item::Fn(f) => f.span,
        Item::Struct(s) => s.span,
        Item::Enum(e) => e.span,
        Item::Trait(t) => t.span,
        Item::Impl(i) => i.span,
        Item::Mod(m) => m.span,
        Item::Const(c) => c.span,
        Item::Units(u) => u.span,
    }
}
