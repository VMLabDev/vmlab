//! vmlab as a library: the CLI binary (`src/main.rs`) builds on these modules.

pub mod agent_asset;
pub mod attach;
pub mod cli;
pub mod config;
pub mod dev;
pub mod guest_asset;
mod hashing;
pub mod lab_init;
mod labd;
mod lifecycle;
pub mod logs;
mod media;
mod net;
mod oci;
pub mod paths;
pub mod profiles;
pub mod proto;
mod qemu;
/// The lab status projection every surface renders (ADR-0004): produced by the
/// lab daemon, consumed unchanged by the CLI table.
pub mod status;
pub use qemu::kvm_available;
mod qmp;
mod scripting;
mod smb;
/// The managed `~/.ssh/config` block (§19.7): host-side, client-generated,
/// and the one artefact vmlab writes outside its own directories.
pub mod ssh_config;
mod supervisor;
mod sync;
pub mod template;
mod viewer;
mod vision;
mod vnc;
