//! vmlab as a library: the CLI binary (`src/main.rs`) builds on these modules.

pub mod agent_asset;
pub mod appshare;
pub mod cli;
pub mod config;
pub mod dev;
#[allow(dead_code)] // Consumers land with #174-#177; drop once every item has a caller.
mod freeze;
/// The bodies of the cargo-fuzz targets under `fuzz/`; absent from every
/// build but cargo-fuzz's.
#[cfg(fuzzing)]
pub mod fuzzing;
pub mod guest_asset;
mod hashing;
mod hvdisplay;
mod instance;
pub mod lab_init;
mod labd;
mod lifecycle;
pub mod logs;
mod media;
mod net;
/// `vmlab-fastpath`'s `main` (F21): the eBPF fast path's privileged helper.
#[cfg(feature = "ebpf")]
pub use net::fastpath::helper::main as fastpath_helper_main;
mod nocow;
mod oci;
pub mod paths;
pub mod profiles;
pub mod proto;
mod qemu;
pub mod record;
/// The lab status projection every surface renders (ADR-0004): produced by the
/// lab daemon, consumed unchanged by the CLI table.
pub mod status;
pub use qemu::kvm_available;
mod qmp;
mod scripting;
mod smb;
mod staging;
mod supervisor;
mod sync;
pub mod template;
mod usb;
pub mod version;
mod viewer;
mod vision;
mod vnc;
