//! vmlab as a library: the CLI binary (`src/main.rs`) and the web binary
//! (`src/web/main.rs`) both build on these modules. Only the surface the web
//! binary needs is `pub` (`cli`, `proto`, `paths`, plus `config`, `profiles`
//! and `template` for the visual lab editor and its catalog pickers); the
//! rest stays crate-internal and is reached via `crate::…` as before.

pub mod agent_asset;
pub mod cli;
pub mod config;
pub mod guest_asset;
mod hashing;
pub mod lab_init;
mod labd;
/// Host-side config-weave binary resolution, shared by labd and the web
/// binary's package endpoints (`labd` itself stays crate-internal).
pub mod weave_bin {
    pub use crate::labd::playbook::{
        ENV_BIN_DIR, GuestOs, default_bin_dir, resolve_bin_dir, weave_binary,
    };
}
mod lifecycle;
pub mod logs;
mod media;
mod net;
mod oci;
/// The OCI registry surface `vmlab-web` needs: looking up what a template's
/// registry publishes, to check a stored image against it.
pub mod oci_registry {
    pub use crate::oci::{Registry, with_version_tag};
}
pub mod paths;
pub mod profiles;
pub mod proto;
mod qemu;
pub use qemu::kvm_available;
/// The hardware-resolution surface `vmlab-web` needs: the designer shows the
/// values a machine inherits, and takes them from the resolver rather than
/// approximating the chain (ADR-0008).
pub mod hardware {
    pub use crate::qemu::{inherited_container, inherited_vm};
}
mod qmp;
mod scripting;
mod smb;
mod supervisor;
mod sync;
pub mod template;
mod viewer;
mod vision;
mod vnc;
