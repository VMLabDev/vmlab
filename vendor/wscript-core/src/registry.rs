//! The registration registry: everything a `Context` knows before compiling
//! a script — host modules, function signatures, constants, registered
//! types and their methods. The type checker reads signatures from here;
//! the VM reads implementations (PRD §2's key invariant).

use std::collections::HashMap;
use std::sync::Arc;

use crate::bytecode::Const;
use crate::defs::{DefId, DefTable};
use crate::host::HostCallable;
use crate::types::{FnSig, Type};

#[derive(Clone)]
pub struct HostFnEntry {
    pub sig: FnSig,
    pub imp: Arc<dyn HostCallable>,
}

impl std::fmt::Debug for HostFnEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HostFnEntry({:?})", self.sig)
    }
}

/// A host function as *declared*: what the checker, the interface emitter
/// and the editor need to talk about it. Module functions
/// (`m.fn_(...)`, in [`ModuleDef::fns`]) and methods on a registered type
/// (`m.ty::<Pane>().method(...)`, in [`Registry::methods`]) declare the
/// same things, so they share this type; a method's receiver is not part
/// of `sig` and not one of the `params`.
///
/// Parameter names live here rather than in [`FnSig`] because `FnSig` is
/// part of type identity (it derives `Eq`/`Hash` and is embedded in
/// `Type::Fn`): `fn(int) -> int` must not become a different type for
/// being written with a different parameter name. Names are documentation
/// — the interface emitter and the LSP show them, the checker ignores them.
#[derive(Debug, Clone)]
pub struct HostFnDecl {
    pub name: String,
    pub sig: FnSig,
    /// Index into [`Registry::host_fns`], which holds the implementation.
    pub host_idx: u32,
    pub doc: Option<String>,
    /// Declared parameter names, positionally matching `sig.params`.
    /// Empty when the host declared none — consumers then fall back to
    /// positional placeholders rather than inventing a name.
    pub params: Vec<String>,
}

impl HostFnDecl {
    /// Declared parameter names, or `None` when the host declared none.
    pub fn param_names(&self) -> Option<&[String]> {
        // `params` is either empty or one name per parameter: registration
        // asserts it (`Module::merge_into`) and the `.wscripti` loader
        // reads both from the same declaration. So the only case here is
        // "nothing was declared".
        (!self.params.is_empty()).then_some(&self.params[..])
    }
}

/// The placeholder standing in for parameter `i` where the host declared
/// no name. Deliberately synthetic: `a0` reads as "nothing was declared",
/// where a plausible-looking invented name would read as fact.
pub fn positional_param_name(i: usize) -> String {
    format!("a{i}")
}

#[derive(Debug, Clone, Default)]
pub struct ModuleDef {
    pub name: String,
    pub fns: Vec<HostFnDecl>,
    pub consts: Vec<(String, Type, Const)>,
    /// Types registered under this module (also importable via `use`).
    pub types: Vec<DefId>,
    pub doc: Option<String>,
}

/// Which of the two ways a host registration is addressed from a script:
/// `math::atan2` or `value.get(k)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    /// A module function — `owner::name`.
    Fn,
    /// A method on a registered type — `owner.name`.
    Method,
}

/// Where a host index was declared. The reverse of [`Registry::host_fns`]:
/// the checker resolves a call to a `host_idx`, and hover has to get back
/// from that number to the module or type that declared it.
#[derive(Debug, Clone, Copy)]
enum HostSite {
    /// `modules[module].fns[idx]`
    ModuleFn { module: usize, idx: usize },
    /// `methods[&def][idx]`
    Method { def: DefId, idx: usize },
    /// An implementation whose declaration has not landed yet.
    /// [`Registry::push_host_fn`] mints this, and the `push_module` /
    /// `push_method` that follows overwrites it — registration is two
    /// steps because the index has to exist before the declaration can
    /// record it. One left standing at the end means a registration that
    /// nothing can name; [`Registry::undeclared_host_fns`] is how a test
    /// says so.
    Undeclared,
}

/// One host registration, located: the declaration plus who owns it.
/// Borrowed from the registry, so resolving a `host_idx` copies nothing.
#[derive(Debug, Clone, Copy)]
pub struct HostRef<'a> {
    pub kind: HostKind,
    /// Module name for a function, type name for a method.
    pub owner: &'a str,
    pub decl: &'a HostFnDecl,
}

impl HostRef<'_> {
    /// `math::atan2` for a module function, `Value.get` for a method.
    pub fn qualified_name(&self) -> String {
        let sep = match self.kind {
            HostKind::Fn => "::",
            HostKind::Method => ".",
        };
        format!("{}{sep}{}", self.owner, self.decl.name)
    }
}

/// All host registrations visible to a compilation. Shared (immutably)
/// between the checker and every VM spun from the owning `Context`.
///
/// Declarations are read through [`Registry::modules`] and
/// [`Registry::methods_of`] and written through [`Registry::push_module`]
/// and [`Registry::push_method`]. The write path is the only one because
/// it maintains the reverse index [`Registry::host_ref`] answers from: a
/// declaration pushed past it would typecheck and then be invisible to
/// every editor feature.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    /// Builtins + host-registered defs. Script compilation clones this and
    /// appends script defs.
    pub defs: DefTable,
    pub host_fns: Vec<HostFnEntry>,
    modules: Vec<ModuleDef>,
    /// Methods of host-registered (usually opaque) types.
    methods: HashMap<DefId, Vec<HostFnDecl>>,
    /// Declaration site of each `host_fns` index, parallel to it.
    host_sites: Vec<HostSite>,
    /// Historical or compatibility names for a registered host type. Aliases
    /// resolve to the same nominal definition, so values and methods are
    /// indistinguishable from those written with the canonical name.
    pub type_aliases: HashMap<String, DefId>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry {
            defs: DefTable::with_builtins(),
            modules: Vec::new(),
            host_fns: Vec::new(),
            methods: HashMap::new(),
            host_sites: Vec::new(),
            type_aliases: HashMap::new(),
        }
    }

    /// Register another script-visible name for an existing nominal type.
    pub fn alias_type(&mut self, alias: impl Into<String>, target: DefId) -> Result<(), String> {
        let alias = alias.into();
        if target.index() >= self.defs.defs.len() {
            return Err(format!("cannot alias unknown type definition {}", target.0));
        }
        let canonical_taken = self
            .defs
            .defs
            .iter()
            .enumerate()
            .any(|(i, _)| self.defs.name_of(DefId(i as u32)) == alias);
        if canonical_taken || self.type_aliases.contains_key(&alias) {
            return Err(format!(
                "cannot alias host type as `{alias}`: the name is already taken"
            ));
        }
        self.type_aliases.insert(alias, target);
        Ok(())
    }

    /// Every registered module, in registration order.
    pub fn modules(&self) -> &[ModuleDef] {
        &self.modules
    }

    pub fn module(&self, name: &str) -> Option<&ModuleDef> {
        self.modules.iter().find(|m| m.name == name)
    }

    /// The methods registered on a host type — empty for a type with none,
    /// so a caller iterates without first asking whether any exist.
    pub fn methods_of(&self, def: DefId) -> &[HostFnDecl] {
        self.methods.get(&def).map_or(&[], Vec::as_slice)
    }

    pub fn push_host_fn(&mut self, entry: HostFnEntry) -> u32 {
        let idx = self.host_fns.len() as u32;
        self.host_fns.push(entry);
        self.host_sites.push(HostSite::Undeclared);
        idx
    }

    /// Add a fully-built module, recording where each of its functions was
    /// declared.
    pub fn push_module(&mut self, def: ModuleDef) {
        let module = self.modules.len();
        for (idx, f) in def.fns.iter().enumerate() {
            self.set_site(f.host_idx, HostSite::ModuleFn { module, idx });
        }
        self.modules.push(def);
    }

    /// Add one method declaration to a registered type.
    pub fn push_method(&mut self, def: DefId, decl: HostFnDecl) {
        let methods = self.methods.entry(def).or_default();
        let idx = methods.len();
        let host_idx = decl.host_idx;
        methods.push(decl);
        self.set_site(host_idx, HostSite::Method { def, idx });
    }

    /// The declaration behind a `host_idx` — two indexings, no scan.
    pub fn host_ref(&self, host_idx: u32) -> Option<HostRef<'_>> {
        match *self.host_sites.get(host_idx as usize)? {
            HostSite::ModuleFn { module, idx } => {
                let m = self.modules.get(module)?;
                Some(HostRef {
                    kind: HostKind::Fn,
                    owner: &m.name,
                    decl: m.fns.get(idx)?,
                })
            }
            HostSite::Method { def, idx } => Some(HostRef {
                kind: HostKind::Method,
                owner: self.defs.name_of(def),
                decl: self.methods.get(&def)?.get(idx)?,
            }),
            HostSite::Undeclared => None,
        }
    }

    /// Host indices no declaration claimed — always empty once
    /// registration is finished.
    pub fn undeclared_host_fns(&self) -> Vec<u32> {
        self.host_sites
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, HostSite::Undeclared))
            .map(|(i, _)| i as u32)
            .collect()
    }

    fn set_site(&mut self, host_idx: u32, site: HostSite) {
        if let Some(slot) = self.host_sites.get_mut(host_idx as usize) {
            *slot = site;
        }
    }
}

// A `Context` must be shareable across threads (PRD §4.3).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Registry>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{ScriptOpaque, ScriptType, register_host_struct};
    use crate::module::Module;

    /// An opaque handle type, registered the way `#[derive(Script)]`
    /// registers one; the derive macro lives downstream of this crate.
    struct Table;

    impl ScriptType for Table {
        fn script_type(defs: &mut DefTable) -> Type {
            Type::Named(register_host_struct(
                defs,
                "Table",
                std::any::TypeId::of::<Table>(),
                true,
                |_| vec![],
            ))
        }
    }

    impl ScriptOpaque for Table {}

    /// Every registration reaches the reverse index. `Undeclared` is the
    /// hole this guards: a registration path that mints a `host_idx` and
    /// pushes its declaration past `push_module`/`push_method` would
    /// typecheck and then be invisible to hover.
    #[test]
    fn every_registration_resolves_to_its_declaration() {
        let mut m = Module::new("geometry");
        m.fn_named("atan2", ["y", "x"], |y: f64, x: f64| y.atan2(x));
        m.fn_("sqrt", |x: f64| x.sqrt());
        m.ty::<Table>().method("width", |_t: &Table| 1i64);
        let mut reg = Registry::new();
        m.merge_into(&mut reg);

        assert_eq!(reg.host_fns.len(), 3);
        for idx in 0..reg.host_fns.len() as u32 {
            let host = reg
                .host_ref(idx)
                .unwrap_or_else(|| panic!("host index {idx} has no declaration"));
            assert_eq!(host.decl.host_idx, idx);
        }
        let names: Vec<String> = (0..3)
            .map(|i| reg.host_ref(i).unwrap().qualified_name())
            .collect();
        assert_eq!(names, ["geometry::atan2", "geometry::sqrt", "Table.width"]);
    }

    /// A method's owner is its type, and a function's is its module — the
    /// distinction hover renders as `.` versus `::`.
    #[test]
    fn a_methods_owner_is_its_type() {
        let mut m = Module::new("ui");
        m.ty::<Table>().method("width", |_t: &Table| 1i64);
        let mut reg = Registry::new();
        m.merge_into(&mut reg);
        let host = reg.host_ref(0).unwrap();
        assert_eq!(host.kind, HostKind::Method);
        assert_eq!(host.owner, "Table");
    }

    /// `push_host_fn` on its own leaves an index nothing can name. The
    /// two-step shape is unavoidable — the index has to exist before a
    /// declaration can record it — so this is the state
    /// `undeclared_host_fns` exists to report.
    #[test]
    fn an_implementation_with_no_declaration_is_reported() {
        let mut reg = Registry::new();
        struct Unreachable;
        impl HostCallable for Unreachable {
            fn call(
                &self,
                _ctx: &mut dyn crate::host::HostCtx,
                _args: Vec<crate::value::Value>,
            ) -> Result<crate::value::Value, crate::host::HostError> {
                unreachable!("never called: this registration has no declaration")
            }
        }
        let idx = reg.push_host_fn(HostFnEntry {
            sig: FnSig::new(vec![], Type::Unit),
            imp: Arc::new(Unreachable),
        });
        assert!(reg.host_ref(idx).is_none());
        assert_eq!(reg.undeclared_host_fns(), vec![idx]);
    }

    /// An index no registration minted resolves to nothing rather than
    /// panicking or naming an unrelated declaration.
    #[test]
    fn an_unknown_host_index_resolves_to_nothing() {
        let reg = Registry::new();
        assert!(reg.host_ref(0).is_none());
        assert!(reg.host_ref(u32::MAX).is_none());
    }
}
