//! QEMU integration: hardware resolution, command-line construction,
//! firmware lookup, process management (PRD §3, §5.2).

pub mod cmdline;
pub mod container;
pub mod firmware;
pub mod process;
pub mod resolve;
pub mod virtiofsd;

pub use cmdline::{
    Accel, NicBackend, NicSpec, VmPaths, build_args, emulator_binary, kvm_available, pick_accel,
};
pub use process::Proc;
pub use resolve::{ResolvedVm, resolve_vm};
