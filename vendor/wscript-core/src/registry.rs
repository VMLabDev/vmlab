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

/// A method on a host-registered type (`m.ty::<Pane>().method(...)`).
/// The receiver is not part of `sig`.
#[derive(Debug, Clone)]
pub struct HostMethod {
    pub name: String,
    pub sig: FnSig,
    pub host_idx: u32,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModuleDef {
    pub name: String,
    /// name → (signature, host fn index, doc)
    pub fns: Vec<(String, FnSig, u32, Option<String>)>,
    pub consts: Vec<(String, Type, Const)>,
    /// Types registered under this module (also importable via `use`).
    pub types: Vec<DefId>,
    pub doc: Option<String>,
}

/// All host registrations visible to a compilation. Shared (immutably)
/// between the checker and every VM spun from the owning `Context`.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    /// Builtins + host-registered defs. Script compilation clones this and
    /// appends script defs.
    pub defs: DefTable,
    pub modules: Vec<ModuleDef>,
    pub host_fns: Vec<HostFnEntry>,
    /// Methods of host-registered (usually opaque) types.
    pub methods: HashMap<DefId, Vec<HostMethod>>,
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
            type_aliases: HashMap::new(),
        }
    }

    pub fn module(&self, name: &str) -> Option<&ModuleDef> {
        self.modules.iter().find(|m| m.name == name)
    }

    pub fn push_host_fn(&mut self, entry: HostFnEntry) -> u32 {
        let idx = self.host_fns.len() as u32;
        self.host_fns.push(entry);
        idx
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
}

// A `Context` must be shareable across threads (PRD §4.3).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Registry>();
};
