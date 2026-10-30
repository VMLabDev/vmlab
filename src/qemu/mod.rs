//! QEMU integration: hardware resolution, command-line construction,
//! firmware lookup, process management (PRD §3, §5.2).

pub mod cmdline;
pub mod container;
pub mod firmware;
pub mod hostgpu;
pub mod keepawake;
pub mod process;
pub mod resolve;
pub mod virtiofsd;
pub mod vk_layer;

pub use cmdline::{
    Accel, AccelPlan, NicBackend, NicSpec, VmPaths, build_args, emulator_binary,
    host_nested_problem, hyperv_not_applied, kvm_available, kvm_usable, pick_accel, plan_accel,
    qemu_arch,
};
pub use process::Proc;
pub use resolve::{ResolvedContainer, ResolvedVm, resolve_container, resolve_vm};
