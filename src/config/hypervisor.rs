//! Which hypervisor runs a machine (PRD §22, ADR-0021), and what the second
//! one cannot run yet.
//!
//! A machine names its hypervisor with `hypervisor = "qemu" | "vmlab"`; one
//! that names none takes an explicit host default, its existing hypervisor,
//! or automatic selection for new VMs and containers.
//! `vmlab` is `vmlab-hv`, vmlab's own hypervisor,
//! which grows phase by phase. [`vmlab_refusals`] is **the** list of what it
//! cannot run yet — validation reads it, and so does the lab daemon before
//! it builds a machine — so a phase that lands shrinks one function.

use serde::{Deserialize, Serialize};

use super::model::{AccelMode, Container, GpuMode, MachineCfg, Vm};
use crate::qemu::ResolvedVm;

/// A hypervisor a machine can run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HypervisorKind {
    /// QEMU, driven over QMP: riscv64 guests, and an aarch64 guest that
    /// vmlab-hv refuses or a setting puts here to troubleshoot. It runs no
    /// x86 guest (PRD §22).
    #[default]
    Qemu,
    /// `vmlab-hv`, driven over its own control protocol.
    Vmlab,
}

impl HypervisorKind {
    /// The spelling of each value, once, for the extractors.
    pub const NAMES: &'static [(&'static str, Self)] =
        &[("qemu", Self::Qemu), ("vmlab", Self::Vmlab)];

    /// The schema spelling, which every surface that reports one uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qemu => "qemu",
            Self::Vmlab => "vmlab",
        }
    }
}

impl std::fmt::Display for HypervisorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The host config's default hypervisor for each machine kind (`host {
/// hypervisor { … } }`). Keyed by kind because the cutover is per phase: a
/// phase that passes its gate flips the default for its machines alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HypervisorDefaults {
    pub container: Option<HypervisorKind>,
    /// `None` selects vmlab-hv for every x86 VM and for an aarch64 VM it can
    /// run, and QEMU for any other.
    pub vm: Option<HypervisorKind>,
}

/// Which rung of the selection ladder named a machine's hypervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSource {
    /// The machine's own `hypervisor`.
    Machine,
    /// The host config's `hypervisor {}` default for its kind.
    HostDefault,
    /// The identity persisted with the machine's disks.
    Existing,
    /// An existing QEMU machine moving to vmlab-hv at its next start: every
    /// x86 one, and an aarch64 one vmlab-hv refuses nothing.
    QemuMigration,
    /// Automatic selection: vmlab-hv for an x86 guest, else vmlab-hv unless
    /// [`vmlab_refusals`] names a reason.
    Automatic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HypervisorSelection {
    pub kind: HypervisorKind,
    pub source: SelectionSource,
    pub fallback_reason: Option<String>,
}

impl HypervisorDefaults {
    /// Explicit settings win over persisted identity. Only automatic
    /// selection of a guest that is not x86 falls back to QEMU — an aarch64
    /// guest when [`vmlab_refusals`] names a reason, a riscv64 guest always;
    /// an x86 guest always selects vmlab-hv unless a setting names QEMU,
    /// which [`qemu_x86_refusals`] then refuses.
    pub fn select(
        &self,
        machine: MachineCfg<'_>,
        host: &HostFacts,
        hardware: Option<&ResolvedVm>,
        existing: Option<HypervisorKind>,
    ) -> HypervisorSelection {
        let (own, default) = match machine {
            MachineCfg::Vm(vm) => (vm.hypervisor, self.vm),
            MachineCfg::Container(c) => (c.hypervisor, self.container),
        };
        let explicit = own.or(default);
        let arch = guest_arch(machine, host, hardware);
        if explicit.is_none() && existing == Some(HypervisorKind::Qemu) {
            // An x86 machine moves unconditionally: QEMU runs none. An
            // aarch64 one moves when vmlab-hv runs everything it asks for,
            // and otherwise stays on QEMU, saying why (PRD §22).
            let emulated = matches!(machine, MachineCfg::Vm(_)) && emulated_guest(&arch);
            let why = if emulated {
                vmlab_refusals(machine, host, hardware)
            } else {
                Vec::new()
            };
            if x86_guest(&arch) || (emulated && why.is_empty()) {
                return HypervisorSelection {
                    kind: HypervisorKind::Vmlab,
                    source: SelectionSource::QemuMigration,
                    fallback_reason: None,
                };
            }
            if !why.is_empty() {
                return HypervisorSelection {
                    kind: HypervisorKind::Qemu,
                    source: SelectionSource::Existing,
                    fallback_reason: Some(format!(
                        "{}: keeping existing QEMU machine: {}",
                        machine.name(),
                        why.join("; ")
                    )),
                };
            }
        }
        if let Some(kind) = explicit.or(existing) {
            return HypervisorSelection {
                kind,
                source: match (own, default) {
                    (Some(_), _) => SelectionSource::Machine,
                    (None, Some(_)) => SelectionSource::HostDefault,
                    (None, None) => SelectionSource::Existing,
                },
                fallback_reason: (explicit.is_none() && kind == HypervisorKind::Qemu)
                    .then(|| format!("{}: keeping existing QEMU machine until destroy or an explicit hypervisor setting", machine.name())),
            };
        }
        // An x86 guest has no fallback: vmlab-hv is its only hypervisor, so
        // what vmlab-hv refuses is an error, said by validation and again by
        // the daemon. Another architecture falls back to QEMU.
        let why = if x86_guest(&arch) {
            Vec::new()
        } else {
            vmlab_refusals(machine, host, hardware)
        };
        HypervisorSelection {
            kind: if why.is_empty() {
                HypervisorKind::Vmlab
            } else {
                HypervisorKind::Qemu
            },
            source: SelectionSource::Automatic,
            fallback_reason: (!why.is_empty()).then(|| why.join("; ")),
        }
    }
}

/// What the refusals need to know about the host doing the running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    /// The host's architecture, which a container always runs.
    pub arch: String,
    /// `/dev/kvm` opens read/write.
    pub kvm: bool,
}

impl HostFacts {
    /// This host, probed.
    pub fn probe() -> Self {
        Self {
            arch: std::env::consts::ARCH.to_string(),
            kvm: crate::qemu::kvm_available(),
        }
    }
}

/// The host architecture `vmlab-hv` runs on: x86_64, where it runs x86
/// guests under KVM and aarch64 guests on its emulator (phase 3d).
const VMLAB_HOST_ARCH: &str = "x86_64";

/// The vCPUs the emulated `virt` machine's GICv2 takes at most.
const VIRT_MAX_VCPUS: u32 = 8;

/// The least memory vmlab-hv builds a `virt` machine with.
const VIRT_MIN_MEMORY: u64 = 64 << 20;

/// Whether vmlab-hv runs a guest of `arch` on its own emulator rather than
/// under KVM: aarch64, since phase 3d (PRD §22).
pub fn emulated_guest(arch: &str) -> bool {
    crate::qemu::qemu_arch(arch) == "aarch64"
}

// Flip each flag only once `hv/` delivers that sub-phase (its acceptance and
// hwref tests pass in-tree), one edit per sub-phase.
const PC_MACHINE_CORE_DELIVERED: bool = true; // 4a
const PC_NETWORKING_DELIVERED: bool = true; // 4b
const PC_FLOPPY_UHCI_DELIVERED: bool = true; // 4c
const PC_CIRRUS_DELIVERED: bool = true; // 4d

#[derive(Debug, Clone, Copy, Default)]
struct LegacyPcDelivery {
    machine_core: bool,
    networking: bool,
    floppy_uhci: bool,
    cirrus: bool,
}

const LEGACY_PC_DELIVERED: LegacyPcDelivery = LegacyPcDelivery {
    machine_core: PC_MACHINE_CORE_DELIVERED,
    networking: PC_NETWORKING_DELIVERED,
    floppy_uhci: PC_FLOPPY_UHCI_DELIVERED,
    cirrus: PC_CIRRUS_DELIVERED,
};

fn legacy_pc(machine: &str) -> bool {
    machine == "pc" || machine.starts_with("pc,") || machine.starts_with("pc-i440fx-")
}

/// Why `vmlab-hv` cannot run `machine` on `host` — one sentence per reason,
/// each naming the machine, the reason and the vmlab-hv phase that brings it
/// (or what to do instead); empty when it can.
///
/// Phase 1 runs container micro-VMs, and phases 2a to 2f a modern x86_64 VM:
/// under KVM, the q35 machine on SeaBIOS or OVMF (secure boot included), std
/// VGA or virtio-vga — 2D, or accelerated by a host GPU under `virgl` and
/// `vulkan` — or a host GPU passed through, virtio or AHCI disks, AHCI
/// CD-ROMs, virtio-net NICs, the USB tablet and `usb = true`'s hot-plug
/// xHCI, a TPM, the Hyper-V enlightenments, virtio-fs shares served in
/// process and an HDA sound card — Linux and Windows 10, 11 and Server.
/// `hardware` is the VM's resolved hardware where the caller has
/// it — the lab daemon always does; validation does when the template is in
/// the store — because most of what decides a VM (its firmware, display,
/// machine type) comes from the template and profile rather than the lab
/// file. Without it only the declaration's own reasons are listed, and the
/// daemon finishes the job before it builds the machine.
pub fn vmlab_refusals(
    machine: MachineCfg<'_>,
    host: &HostFacts,
    hardware: Option<&ResolvedVm>,
) -> Vec<String> {
    match machine {
        MachineCfg::Vm(vm) => vm_refusals(vm, host, hardware, LEGACY_PC_DELIVERED),
        MachineCfg::Container(c) => container_refusals(c, host),
    }
}

fn vm_refusals(
    vm: &Vm,
    host: &HostFacts,
    hardware: Option<&ResolvedVm>,
    delivered: LegacyPcDelivery,
) -> Vec<String> {
    let name = &vm.name;
    let mut why = Vec::new();
    let arch = hardware
        .map(|h| crate::qemu::qemu_arch(&h.arch).to_string())
        .or_else(|| crate::qemu::resolve::vm_arch(vm).map(|a| crate::qemu::qemu_arch(&a).into()));
    let emulated = arch.as_deref().is_some_and(emulated_guest);
    if host.arch != VMLAB_HOST_ARCH {
        why.push(
            if x86_guest(&guest_arch(MachineCfg::Vm(vm), host, hardware)) {
                format!(
                    "vm \"{name}\": KVM on an x86_64 host is required — x86 guests run on \
                 vmlab-hv only, and this host is {} (PRD §22)",
                    host.arch
                )
            } else if emulated {
                format!(
                    "vm \"{name}\": vmlab-hv emulates an aarch64 guest on an x86_64 host only, \
                     and this host is {} (PRD §22) — use hypervisor = \"qemu\"",
                    host.arch
                )
            } else {
                arch_refusal("vm host", name, &host.arch)
            },
        );
    }
    if !vm.qemu_args.is_empty() {
        why.push(if emulated {
            format!(
                "vm \"{name}\": `qemu_args` is QEMU's own argv, and vmlab-hv has none (PRD §22, \
                 HV-D16) — remove it and declare the hardware with the vm block's own keys, \
                 or set hypervisor = \"qemu\" to troubleshoot on QEMU"
            )
        } else {
            qemu_args_refusal(name)
        });
    }
    if emulated {
        if vm.accel == Some(AccelMode::Kvm) {
            why.push(format!(
                "vm \"{name}\": accel = \"kvm\" — vmlab-hv runs an aarch64 guest on its own \
                 emulator, not under KVM (PRD §22); remove `accel`"
            ));
        }
    } else if vm.accel == Some(AccelMode::Tcg) {
        why.push(format!(
            "vm \"{name}\": accel = \"tcg\" — x86 guests run on vmlab-hv under KVM only, \
             and it has no emulation until phase 3 (PRD §22); remove `accel`"
        ));
    }
    if let Some(arch) = &arch
        && arch != VMLAB_HOST_ARCH
        && !emulated
    {
        why.push(arch_refusal("vm", name, arch));
    } else if !emulated && !host.kvm && vm.accel != Some(AccelMode::Tcg) {
        why.push(kvm_required("vm", name));
    }
    let has_floppy = vm.floppy.is_some()
        || vm
            .media
            .iter()
            .any(|m| m.kind == crate::config::model::MediaKind::Floppy);
    if emulated {
        if has_floppy {
            why.push(format!(
                "vm \"{name}\": a floppy drive is x86 hardware, and vmlab-hv's aarch64 `virt` \
                 machine has none (PRD §22); attach the files on a CD-ROM"
            ));
        }
        if let Some(h) = hardware {
            virt_hardware_refusals(name, h, &mut why);
        }
        return why;
    }
    why.extend(vm_hardware_refusals(
        name,
        hardware,
        !vm.nics.is_empty(),
        has_floppy,
        delivered,
    ));
    why
}

/// What an aarch64 guest's resolved hardware asks for that vmlab-hv's
/// emulated `virt` machine (phase 3d) does not present: a GICv2 machine of
/// one to eight vCPUs on AAVMF, with virtio-blk disks and CD-ROMs, virtio-net
/// NICs and the agent's virtio-serial port. It has no display, input, USB,
/// TPM, sound or virtio-fs yet. A display device the profile names is left
/// out rather than refused: the guest runs headless, on its serial console.
fn virt_hardware_refusals(name: &str, h: &ResolvedVm, why: &mut Vec<String>) {
    use crate::profiles::AgentTransport;
    let mut refuse = |what: &str, fix: &str| {
        why.push(format!(
            "vm \"{name}\": {what} — vmlab-hv's emulated aarch64 `virt` machine does not \
             present it yet (PRD §22); {fix}"
        ));
    };
    if h.machine != "virt" {
        refuse(
            &format!("machine = \"{}\"", h.machine),
            "use the `virt` machine",
        );
    }
    if h.firmware != Some(crate::profiles::FirmwareKind::Ovmf) {
        refuse("a firmware other than UEFI (AAVMF)", "use UEFI firmware");
    }
    if !(1..=VIRT_MAX_VCPUS).contains(&h.cpus) {
        refuse(
            &format!("{} vCPUs (its GICv2 takes 1 to {VIRT_MAX_VCPUS})", h.cpus),
            &format!("set cpus to at most {VIRT_MAX_VCPUS}"),
        );
    }
    if h.memory < VIRT_MIN_MEMORY {
        refuse(
            &format!("{} MiB of memory", h.memory >> 20),
            "give it at least 64MiB",
        );
    }
    if h.gpu.is_some() {
        refuse("a `gpu {}` block", "remove it");
    }
    if h.usb {
        refuse(
            "`usb = true` (an xHCI for passthrough)",
            "set `usb = false`",
        );
    }
    if h.tpm {
        refuse("a TPM", "set `tpm = false`");
    }
    if h.audio {
        refuse("a sound card (`audio`)", "remove `audio`");
    }
    if h.cpu_model.is_some() {
        refuse("a `cpu_model`", "remove it; the emulated CPU is Armv8.0-A");
    }
    if !h.nic_model.starts_with("virtio-net") {
        refuse(
            &format!("nic_model `{}`", h.nic_model),
            "use a virtio-net NIC",
        );
    }
    if h.agent_transport == AgentTransport::IsaSerial {
        refuse(
            "agent_transport = \"isa-serial\"",
            "use agent_transport = \"virtio-serial\"",
        );
    }
    if h.apple_smc {
        refuse("an Apple SMC", "remove `apple_smc`");
    }
}

/// The resolved launch's hardware refusals, including attachments not carried
/// by [`ResolvedVm`]. The adapter checks these before it spawns, using the
/// same delivery gates as configuration validation and the lab daemon.
pub(crate) fn vmlab_vm_launch_refusals(
    hardware: &ResolvedVm,
    has_nics: bool,
    has_floppy: bool,
) -> Vec<String> {
    if emulated_guest(&hardware.arch) {
        let mut why = Vec::new();
        if has_floppy {
            why.push(format!(
                "vm \"{}\": a floppy drive is x86 hardware, and vmlab-hv's aarch64 `virt` \
                 machine has none (PRD §22); attach the files on a CD-ROM",
                hardware.name
            ));
        }
        virt_hardware_refusals(&hardware.name, hardware, &mut why);
        return why;
    }
    vm_hardware_refusals(
        &hardware.name,
        Some(hardware),
        has_nics,
        has_floppy,
        LEGACY_PC_DELIVERED,
    )
}

fn vm_hardware_refusals(
    name: &str,
    hardware: Option<&ResolvedVm>,
    has_nics: bool,
    has_floppy: bool,
    delivered: LegacyPcDelivery,
) -> Vec<String> {
    let mut why = Vec::new();
    if has_floppy {
        if !delivered.floppy_uhci {
            why.push(phase(name, "a floppy drive", "4c (floppy and UHCI)"));
        } else if hardware.is_some_and(|h| !legacy_pc(&h.machine)) {
            why.push(format!(
                "vm \"{name}\": a floppy drive needs vmlab-hv's pc machine (PRD §22); \
                 use a profile on the pc machine, or attach the files on a CD-ROM"
            ));
        }
    }
    if let Some(h) = hardware {
        hardware_refusals(name, h, has_nics, delivered, &mut why);
    }
    why
}

/// The macos profile's QEMU CPU string: vmlab-hv's `Skylake-Client-v4` is
/// this model with these edits, pinned from a capture (HV macOS phase).
pub const MACOS_CPU_MODEL: &str =
    "Skylake-Client-v4,-hle,-rtm,kvm=on,vendor=GenuineIntel,vmware-cpuid-freq=on";

/// Whether `h` is a macOS guest: the macos profile, or an Apple SMC.
pub fn is_macos(h: &ResolvedVm) -> bool {
    h.profile.as_deref() == Some("macos") || h.apple_smc
}

/// What the resolved hardware asks for that vmlab-hv cannot present yet.
fn hardware_refusals(
    name: &str,
    h: &ResolvedVm,
    has_nics: bool,
    delivered: LegacyPcDelivery,
    why: &mut Vec<String>,
) {
    use crate::profiles::AgentTransport;
    // A macOS guest brings the hardware vmlab-hv's macOS phase delivered:
    // VMware SVGA, a USB keyboard and the one CPU model it pins. The rest of
    // the list below still applies to it.
    let macos = is_macos(h);
    if macos && h.cpu_model.as_deref() != Some(MACOS_CPU_MODEL) {
        why.push(format!(
            "vm \"{name}\": a macOS guest runs on vmlab-hv with the macos profile's cpu_model only, \
             not `{}` (PRD §22); remove `cpu_model`",
            h.cpu_model.as_deref().unwrap_or("the host CPU")
        ));
    }
    let pc = legacy_pc(&h.machine);
    if pc {
        if h.usb {
            why.push(format!(
                "vm \"{name}\": `usb = true` requires the q35 xHCI on vmlab-hv (PRD §5.2.1); \
                 set `usb = false` on the pc machine"
            ));
        }
        if !delivered.machine_core {
            why.push(phase(
                name,
                &format!("machine = \"{}\" (i440fx and IDE)", h.machine),
                "4a (the pc machine core)",
            ));
        }
        // QEMU adds PIIX3 UHCI and its tablet to every pc argv, even when
        // the lab declares neither a floppy nor USB passthrough.
        if !delivered.floppy_uhci {
            why.push(phase(name, "the pc UHCI tablet", "4c (floppy and UHCI)"));
        }
    } else if !h.machine.starts_with("q35") {
        why.push(format!(
            "vm \"{name}\": machine = \"{}\" is not a q35 or pc machine \
             vmlab-hv supports (PRD §22); use a profile on q35 or pc",
            h.machine
        ));
    }
    match h.display_device.as_deref() {
        None | Some("VGA") | Some("virtio-vga") => {}
        Some("vmware-svga") if macos => {}
        Some("cirrus-vga") if pc => {
            if !delivered.cirrus {
                why.push(phase(name, "the Cirrus VGA display", "4d (Cirrus)"));
            }
        }
        Some(other) => why.push(format!(
            "vm \"{name}\": display device `{other}` — vmlab-hv presents std VGA and \
             virtio-vga only (PRD §22); set display = \"std\" or \"virtio-vga\""
        )),
    }
    match h.nic_model.as_str() {
        // vmlab-hv's pc presents the NICs its captures show, e1000 and PCnet.
        model if model.starts_with("virtio-net") && pc && has_nics => why.push(format!(
            "vm \"{name}\": nic_model `{model}` on the pc machine — vmlab-hv's pc presents \
             e1000 and PCnet NICs only (PRD §22); use a pc profile with one of them"
        )),
        model if model.starts_with("virtio-net") => {}
        "e1000" | "pcnet" if pc => {
            if has_nics && !delivered.networking {
                why.push(phase(
                    name,
                    &format!("nic_model = \"{}\"", h.nic_model),
                    "4b (e1000 and PCnet)",
                ));
            }
        }
        _ => why.push(format!(
            "vm \"{name}\": nic_model `{}` — vmlab-hv presents virtio-net NICs only (PRD §22) \
             on q35, and e1000 or PCnet on pc; use a profile with one of them",
            h.nic_model
        )),
    }
    if h.agent_transport == AgentTransport::IsaSerial && !delivered.machine_core {
        why.push(phase(
            name,
            "agent_transport = \"isa-serial\" on pc",
            "4a (the pc machine core)",
        ));
    } else if h.agent_transport == AgentTransport::IsaSerial && !pc {
        why.push(format!(
            "vm \"{name}\": agent_transport = \"isa-serial\" needs vmlab-hv's pc machine \
             (PRD §22); use the pc machine, or agent_transport = \"virtio-serial\""
        ));
    }
    if h.input == crate::config::model::InputDevices::Usb && !macos {
        why.push(phase(name, "input = \"usb\" (a USB keyboard)", "after 2b"));
    }
    if let Some(model) = h.cpu_model.as_ref().filter(|_| !macos) {
        if pc && !delivered.machine_core {
            why.push(phase(
                name,
                &format!("cpu_model = \"{model}\" on pc"),
                "4a (the pc machine core)",
            ));
        } else if !pc {
            why.push(format!(
                "vm \"{name}\": cpu_model = \"{model}\" — vmlab-hv runs the host CPU only \
                 on q35 (PRD §22); remove it"
            ));
        }
    }
    // vmlab-hv pins the TSC (invtsc and tsc_khz) on q35 only; its pc
    // machine refuses both, as it does the other macOS-era CPU features.
    if pc && h.tsc.is_some() {
        why.push(format!(
            "vm \"{name}\": a pinned TSC (`tsc = \"host\"`) on the pc machine — vmlab-hv \
             pins the TSC on q35 only (PRD §22); remove `tsc`"
        ));
    }
}

/// Why QEMU cannot run `vm` as declared — what only vmlab-hv brings, one
/// sentence per reason; empty when it can. Read by validation for every VM
/// that runs on QEMU, as [`vmlab_refusals`] is for every one that does not.
pub fn qemu_refusals(vm: &Vm) -> Vec<String> {
    let name = &vm.name;
    let mut why = Vec::new();
    if vm.gpu.as_ref().is_some_and(|gpu| gpu.virtual_display) {
        why.push(format!("vm \"{name}\": gpu virtual_display needs hypervisor = \"vmlab\"; QEMU does not support the shared-memory virtual monitor"));
    }
    // vmlab gives QEMU no audio device (hv-audio D7): audio is vmlab-hv's.
    for (key, on) in [("audio", vm.audio), ("microphone", vm.microphone)] {
        if on {
            why.push(format!(
                "vm \"{name}\": {key} = true needs hypervisor = \"vmlab\" — vmlab gives a QEMU \
                 machine no sound card (PRD §11, §22); set hypervisor = \"vmlab\", or drop `{key}`"
            ));
        }
    }
    why
}

/// Whether `arch` is an x86 guest, which runs on vmlab-hv only.
pub fn x86_guest(arch: &str) -> bool {
    crate::qemu::qemu_arch(arch) == VMLAB_HOST_ARCH
}

/// The architecture `machine` runs: a VM's resolved or declared one, else
/// the host's, which a container always runs.
fn guest_arch(machine: MachineCfg<'_>, host: &HostFacts, hardware: Option<&ResolvedVm>) -> String {
    match machine {
        MachineCfg::Vm(vm) => hardware
            .map(|h| h.arch.clone())
            .or_else(|| crate::qemu::resolve::vm_arch(vm))
            .unwrap_or_else(|| host.arch.clone()),
        MachineCfg::Container(_) => host.arch.clone(),
    }
}

/// Why `machine` may not run as `selection` puts it on QEMU — one sentence
/// per reason, each naming the replacement; empty when it may. QEMU runs no
/// x86 or x86_64 guest (HV-D29, MEM-40): vmlab-hv is their only hypervisor,
/// so an x86 machine that a setting puts on QEMU, or that declares
/// `qemu_args`, is an error. Other architectures stay on QEMU.
pub fn qemu_x86_refusals(
    machine: MachineCfg<'_>,
    selection: &HypervisorSelection,
    host: &HostFacts,
    hardware: Option<&ResolvedVm>,
) -> Vec<String> {
    if !x86_guest(&guest_arch(machine, host, hardware)) {
        return Vec::new();
    }
    let (kind, name) = match machine {
        MachineCfg::Vm(vm) => ("vm", &vm.name),
        MachineCfg::Container(c) => ("container", &c.name),
    };
    // On vmlab-hv, `qemu_args` is one of [`vmlab_refusals`]'s.
    if selection.kind != HypervisorKind::Qemu {
        return Vec::new();
    }
    let mut why = vec![match selection.source {
        SelectionSource::HostDefault => format!(
            "{kind} \"{name}\": the host config sets `hypervisor {{ {kind} = \"qemu\" }}`, \
                 and QEMU no longer runs x86 guests (PRD §22) — remove that default or set it \
                 to \"vmlab\", which runs them"
        ),
        _ => format!(
            "{kind} \"{name}\": hypervisor = \"qemu\" no longer runs x86 guests (PRD §22) — \
                 set hypervisor = \"vmlab\", or remove the line; vmlab-hv runs every x86 machine"
        ),
    }];
    if let MachineCfg::Vm(vm) = machine
        && !vm.qemu_args.is_empty()
    {
        why.push(qemu_args_refusal(name));
    }
    why
}

/// Why an online snapshot of a running vmlab-hv VM with `hardware` cannot be
/// taken or loaded — `None` when it can. An accelerated GPU's renderer state
/// and a passed-through device's are not vmlab-hv's to save (HV phase-2 D9);
/// offline snapshots are unaffected. USB devices are refused while attached,
/// on either hypervisor, by [`crate::usb::snapshot_refusal`].
pub fn vmlab_online_snapshot_refusal(hardware: &ResolvedVm) -> Option<String> {
    let gpu = hardware.gpu.as_ref()?;
    let what = match gpu.mode {
        GpuMode::Passthrough => "a host GPU passed through (`gpu { mode = \"passthrough\" }`)",
        GpuMode::Virgl | GpuMode::Vulkan => "an accelerated GPU (`gpu {}`)",
    };
    Some(format!(
        "\"{}\" runs on vmlab-hv with {what}, whose state vmlab-hv cannot save (PRD §22) — \
         stop the machine and take an offline snapshot instead",
        hardware.name
    ))
}

/// A VM refused for something a later vmlab-hv phase brings.
fn phase(name: &str, what: &str, phase: &str) -> String {
    format!(
        "vm \"{name}\": {what} — vmlab-hv runs it from phase {phase} (PRD §22), and QEMU \
         no longer runs x86 guests, so this machine cannot start until then"
    )
}

fn container_refusals(c: &Container, host: &HostFacts) -> Vec<String> {
    let name = &c.name;
    let mut why = Vec::new();
    // A container runs the host's own architecture.
    if host.arch != VMLAB_HOST_ARCH {
        why.push(arch_refusal("container", name, &host.arch));
    } else if !host.kvm {
        why.push(kvm_required("container", name));
    }
    why
}

/// `qemu_args` is QEMU's argv; no other hypervisor has one to append to, and
/// none ever will (HV-D16). QEMU runs no x86 guest, so on one it is an error.
fn qemu_args_refusal(name: &str) -> String {
    format!(
        "vm \"{name}\": `qemu_args` is QEMU's own argv, and QEMU no longer runs x86 guests \
         (PRD §22, HV-D16) — remove it and declare the hardware with the vm block's own \
         keys (`cpu_model`, `display`, `firmware`, `input`, …) or a profile"
    )
}

/// An x86 guest on a host without usable KVM (MEM-40): vmlab-hv runs under
/// KVM only, and QEMU's emulation no longer runs x86 guests.
fn kvm_required(kind: &str, name: &str) -> String {
    format!(
        "{kind} \"{name}\": KVM is required — x86 guests run on vmlab-hv under KVM only, \
         and /dev/kvm is not usable on this host (PRD §22); enable virtualization in the \
         firmware, load the kvm module, and give this user read/write access to /dev/kvm"
    )
}

fn arch_refusal(kind: &str, name: &str, arch: &str) -> String {
    format!(
        "{kind} \"{name}\": hypervisor = \"vmlab\" runs x86_64 and aarch64 guests only so far, \
         and this is {arch} (PRD §22) — use hypervisor = \"qemu\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kvm_host() -> HostFacts {
        HostFacts {
            arch: "x86_64".into(),
            kvm: true,
        }
    }

    fn lab(src: &str) -> crate::config::LabFile {
        let src = format!("import <vmlab.wcl>\n{src}");
        crate::config::load_lab_source(&src, "t.wcl", std::path::Path::new("/nonexistent"))
            .expect("parses")
    }

    #[test]
    fn a_machine_takes_its_own_hypervisor_over_the_hosts_default() {
        let file = lab(r#"lab "l" {
  container "a" { image = "alpine" hypervisor = "vmlab" }
  container "b" { image = "alpine" }
  vm "c" { template = "x86_64/t" hypervisor = "qemu" }
  vm "d" { template = "x86_64/t" }
}"#);
        let defaults = HypervisorDefaults {
            container: Some(HypervisorKind::Qemu),
            vm: Some(HypervisorKind::Vmlab),
        };
        let pick = |name: &str| {
            defaults
                .select(file.lab.machine(name).unwrap(), &kvm_host(), None, None)
                .kind
        };
        assert_eq!(pick("a"), HypervisorKind::Vmlab);
        assert_eq!(pick("b"), HypervisorKind::Qemu);
        assert_eq!(pick("c"), HypervisorKind::Qemu);
        assert_eq!(pick("d"), HypervisorKind::Vmlab);
        // Supported new containers and VMs select vmlab-hv.
        let none = HypervisorDefaults::default();
        assert_eq!(
            none.select(file.lab.machine("b").unwrap(), &kvm_host(), None, None)
                .kind,
            HypervisorKind::Vmlab
        );
        assert_eq!(
            none.select(file.lab.machine("d").unwrap(), &kvm_host(), None, None)
                .kind,
            HypervisorKind::Vmlab
        );
    }

    #[test]
    fn automatic_vm_selection_migrates_existing_x86_qemu_and_respects_explicit_choices() {
        let file = lab(r#"lab "l" {
          vm "modern" { template = "scratch" arch = "x86_64" profile = "linux-modern" disk = 1GiB }
          vm "args" { template = "x86_64/t" qemu_args = ["-no-hpet"] }
          vm "explicit" { template = "x86_64/t" hypervisor = "vmlab" qemu_args = ["-no-hpet"] }
        }"#);
        let defaults = HypervisorDefaults::default();
        let args = file.lab.machine("args").unwrap();
        // An x86 guest no longer falls back: `qemu_args` is refused instead.
        let picked = defaults.select(args, &kvm_host(), None, None);
        assert_eq!(picked.kind, HypervisorKind::Vmlab);
        assert!(picked.fallback_reason.is_none());
        assert!(
            vmlab_refusals(args, &kvm_host(), None)
                .iter()
                .any(|w| w.contains("`qemu_args`"))
        );
        let explicit = defaults.select(
            file.lab.machine("explicit").unwrap(),
            &kvm_host(),
            None,
            None,
        );
        assert_eq!(explicit.kind, HypervisorKind::Vmlab);
        assert!(explicit.fallback_reason.is_none());
        let modern = file.lab.machine("modern").unwrap();
        let hardware = crate::qemu::resolve_vm(
            &file.lab.vms[0],
            None,
            &crate::profiles::ProfileSet::shipped().unwrap(),
        )
        .unwrap();
        assert_eq!(
            defaults
                .select(modern, &kvm_host(), Some(&hardware), None)
                .kind,
            HypervisorKind::Vmlab
        );
        let migrated = defaults.select(
            modern,
            &kvm_host(),
            Some(&hardware),
            Some(HypervisorKind::Qemu),
        );
        assert_eq!(migrated.kind, HypervisorKind::Vmlab);
        assert_eq!(migrated.source, SelectionSource::QemuMigration);
        assert!(migrated.fallback_reason.is_none());
        for kind in [HypervisorKind::Qemu, HypervisorKind::Vmlab] {
            let host = HypervisorDefaults {
                vm: Some(kind),
                ..defaults
            };
            assert_eq!(
                host.select(
                    modern,
                    &kvm_host(),
                    Some(&hardware),
                    Some(HypervisorKind::Vmlab)
                )
                .kind,
                kind
            );
        }
        let no_kvm = HostFacts {
            kvm: false,
            ..kvm_host()
        };
        let picked = defaults.select(modern, &no_kvm, Some(&hardware), None);
        assert_eq!(picked.kind, HypervisorKind::Vmlab);
        assert!(picked.fallback_reason.is_none());
        let why = vmlab_refusals(modern, &no_kvm, Some(&hardware));
        assert!(why.iter().any(|w| w.contains("KVM is required")), "{why:?}");
    }

    #[test]
    fn automatic_selection_of_an_x86_guest_never_falls_back_to_qemu() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let setting = r#"profile = "linux-modern" accel = "tcg""#;
        let file = lab(&format!(
            r#"lab "l" {{ vm "v" {{ template = "scratch" arch = "x86_64" disk = 1GiB {setting} }} }}"#
        ));
        let vm = &file.lab.vms[0];
        let hw = crate::qemu::resolve_vm(vm, None, &profiles).unwrap();
        let picked =
            HypervisorDefaults::default().select(MachineCfg::Vm(vm), &kvm_host(), Some(&hw), None);
        assert_eq!(picked.kind, HypervisorKind::Vmlab, "{setting}");
        assert_eq!(picked.fallback_reason, None, "{setting}");
        assert!(
            !vmlab_refusals(MachineCfg::Vm(vm), &kvm_host(), Some(&hw)).is_empty(),
            "{setting}"
        );
    }

    #[test]
    fn automatic_selection_supports_pinned_tsc_on_modern_vms() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        for profile in ["linux-modern", "windows-11"] {
            let file = lab(&format!(
                r#"lab "l" {{ vm "v" {{ template = "scratch" arch = "x86_64" disk = 1GiB profile = "{profile}" tsc = "host" }} }}"#
            ));
            let vm = &file.lab.vms[0];
            let hardware = crate::qemu::resolve_vm(vm, None, &profiles).unwrap();
            assert!(hardware.tsc.is_some());
            let picked = HypervisorDefaults::default().select(
                MachineCfg::Vm(vm),
                &kvm_host(),
                Some(&hardware),
                None,
            );
            assert_eq!(picked.kind, HypervisorKind::Vmlab, "{profile}");
            assert!(picked.fallback_reason.is_none(), "{profile}: {picked:?}");
        }
    }

    #[test]
    fn automatic_container_selection_migrates_existing_x86_qemu_and_reports_refusals() {
        let file = lab(r#"lab "l" {
          container "c" { image = "alpine" }
          container "q" { image = "alpine" hypervisor = "qemu" }
        }"#);
        let defaults = HypervisorDefaults::default();
        let machine = file.lab.machine("c").unwrap();
        let picked = defaults.select(machine, &kvm_host(), None, None);
        assert_eq!(picked.kind, HypervisorKind::Vmlab);
        assert!(picked.fallback_reason.is_none());
        for kind in [HypervisorKind::Qemu, HypervisorKind::Vmlab] {
            assert_eq!(
                defaults.select(machine, &kvm_host(), None, Some(kind)).kind,
                HypervisorKind::Vmlab
            );
            let host = HypervisorDefaults {
                container: Some(kind),
                ..defaults
            };
            assert_eq!(
                host.select(machine, &kvm_host(), None, Some(HypervisorKind::Vmlab))
                    .kind,
                kind
            );
        }
        assert_eq!(
            defaults
                .select(
                    file.lab.machine("q").unwrap(),
                    &kvm_host(),
                    None,
                    Some(HypervisorKind::Vmlab)
                )
                .kind,
            HypervisorKind::Qemu
        );
        // Without KVM an x86 container is refused, not moved to QEMU.
        let no_kvm = HostFacts {
            kvm: false,
            ..kvm_host()
        };
        let picked = defaults.select(machine, &no_kvm, None, None);
        assert_eq!(picked.kind, HypervisorKind::Vmlab);
        assert!(picked.fallback_reason.is_none());
        // Another architecture's container stays on QEMU.
        let arm = HostFacts {
            arch: "aarch64".into(),
            ..kvm_host()
        };
        let picked = defaults.select(machine, &arm, None, None);
        assert_eq!(picked.kind, HypervisorKind::Qemu);
        assert_eq!(
            picked.fallback_reason,
            Some(vmlab_refusals(machine, &arm, None).join("; "))
        );
    }

    #[test]
    fn a_container_on_an_x86_64_kvm_host_is_refused_nothing() {
        let file = lab(r#"lab "l" { container "a" { image = "alpine" } }"#);
        assert!(vmlab_refusals(file.lab.machine("a").unwrap(), &kvm_host(), None).is_empty());
    }

    #[test]
    fn a_container_without_kvm_is_refused_naming_kvm() {
        let file = lab(r#"lab "l" { container "a" { image = "alpine" } }"#);
        let host = HostFacts {
            kvm: false,
            ..kvm_host()
        };
        let why = vmlab_refusals(file.lab.machine("a").unwrap(), &host, None);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("container \"a\""), "{}", why[0]);
        assert!(why[0].contains("KVM is required"), "{}", why[0]);
        assert!(why[0].contains("/dev/kvm"), "{}", why[0]);
    }

    #[test]
    fn a_container_on_another_arch_is_refused_naming_it() {
        let file = lab(r#"lab "l" { container "a" { image = "alpine" } }"#);
        let host = HostFacts {
            arch: "aarch64".into(),
            kvm: true,
        };
        let why = vmlab_refusals(file.lab.machine("a").unwrap(), &host, None);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("x86_64 only"), "{}", why[0]);
        assert!(why[0].contains("aarch64"), "{}", why[0]);
    }

    /// A lab with one VM `v`, its body `body`, and that VM's refusals with
    /// its hardware resolved against the shipped profiles.
    fn refused(body: &str) -> Vec<String> {
        refused_with(body, LEGACY_PC_DELIVERED)
    }

    fn refused_with(body: &str, delivered: LegacyPcDelivery) -> Vec<String> {
        let file = lab(&format!("lab \"l\" {{ vm \"v\" {{ {body} }} }}"));
        let m = file.lab.machine("v").unwrap();
        let crate::config::model::MachineCfg::Vm(vm) = m else {
            unreachable!()
        };
        let hw =
            crate::qemu::resolve_vm(vm, None, &crate::profiles::ProfileSet::shipped().unwrap())
                .unwrap();
        vm_refusals(vm, &kvm_host(), Some(&hw), delivered)
    }

    /// Phases 2a to 2f: a modern VM on q35 — SeaBIOS or OVMF, secure boot,
    /// a TPM, Hyper-V, Linux or Windows, a GPU of any mode, virtio-fs
    /// shares, USB passthrough and a sound card — is refused nothing.
    #[test]
    fn a_modern_vm_on_q35_is_refused_nothing() {
        let base = r#"template = "scratch" arch = "x86_64" disk = 10GiB"#;
        for body in [
            format!(r#"{base} profile = "linux-modern" firmware = "seabios""#),
            format!(r#"{base} profile = "linux-modern""#),
            format!(r#"{base} profile = "linux-modern" secure_boot = true tpm = true"#),
            format!(r#"{base} profile = "windows-10""#),
            format!(r#"{base} profile = "windows-11""#),
            format!(r#"{base} profile = "windows-server""#),
            format!(r#"{base} profile = "linux-generic""#),
            format!(r#"{base} profile = "linux-generic" display = "virtio-vga""#),
            format!(r#"{base} profile = "linux-generic" nested = true"#),
            format!(r#"{base} profile = "linux-generic" tsc = "host""#),
            format!(r#"{base} profile = "windows-11" tsc = "host""#),
            format!(r#"{base} profile = "linux-generic" gpu {{ mode = "virgl" }}"#),
            format!(r#"{base} profile = "linux-modern" gpu {{ mode = "vulkan" }}"#),
            format!(r#"{base} profile = "linux-modern" gpu {{ mode = "passthrough" }}"#),
            format!(r#"{base} profile = "linux-generic" usb = true"#),
            format!(
                r#"{base} profile = "linux-generic" share "s" {{ host = "." guest = "/s" transport = "virtiofs" }}"#
            ),
            format!(r#"{base} profile = "windows-11" audio = true microphone = true"#),
        ] {
            let why = refused(&body);
            assert!(why.is_empty(), "{body}: {why:?}");
        }
    }

    #[test]
    fn usb_passthrough_on_the_pc_machine_is_refused_by_name() {
        let why = refused(
            r#"template = "scratch" arch = "x86_64" disk = 10GiB profile = "windows-xp" usb = true"#,
        );
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("usb = true"), "{why:?}");
    }

    #[test]
    fn firmware_resolution_is_supported_by_fw_cfg() {
        let why = refused(
            r#"template = "scratch" arch = "x86_64" profile = "linux-modern" disk = 10GiB firmware_width = 1024 firmware_height = 768"#,
        );
        assert!(why.is_empty(), "{why:?}");
    }

    /// Everything else names the phase that brings it.
    #[test]
    fn what_a_later_phase_brings_is_refused_naming_that_phase() {
        let base = r#"template = "scratch" arch = "x86_64" disk = 10GiB"#;
        let cases = [(r#"profile = "linux-modern" accel = "tcg""#, "phase 3")];
        for (body, phase) in cases {
            let why = refused(&format!("{base} {body}"));
            assert!(
                why.iter()
                    .any(|w| w.contains(phase) && w.contains("vm \"v\"")),
                "{body}: expected {phase} in {why:?}"
            );
        }
    }

    /// The macOS phase is delivered: the macos profile's hardware, its SVGA,
    /// USB keyboard and pinned CPU model included, runs on vmlab-hv. Another
    /// CPU model for it, or those devices on any other guest, still do not.
    #[test]
    fn a_macos_guest_is_admitted_with_its_own_hardware_only() {
        let base = r#"template = "scratch" arch = "x86_64" disk = 10GiB"#;
        let why = refused(&format!(r#"{base} profile = "macos""#));
        assert!(why.is_empty(), "{why:?}");
        let why = refused(&format!(
            r#"{base} profile = "macos" cpu_model = "Haswell""#
        ));
        assert!(
            why.len() == 1 && why[0].contains("macos profile's cpu_model only"),
            "{why:?}"
        );
        for other in [
            r#"profile = "linux-modern" display = "vmware""#,
            r#"profile = "linux-modern" input = "usb""#,
            r#"profile = "linux-modern" cpu_model = "Skylake-Client-v4""#,
        ] {
            assert_eq!(refused(&format!("{base} {other}")).len(), 1, "{other}");
        }
    }

    #[test]
    fn legacy_pc_refusals_shrink_at_each_subphase() {
        let body = r#"template = "scratch" arch = "x86" disk = 2GiB
            profile = "windows-9x" cpu_model = "pentium3" floppy = "./f.img"
            nic { nat = true }"#;
        let mut delivered = LegacyPcDelivery::default();
        let before = refused_with(body, delivered);
        for phase in ["4a", "4b", "4c", "4d"] {
            assert!(before.iter().any(|why| why.contains(phase)), "{before:?}");
        }
        assert!(
            before.iter().any(|why| why.contains("isa-serial")),
            "{before:?}"
        );
        assert!(
            before.iter().any(|why| why.contains("pentium3")),
            "{before:?}"
        );

        delivered.machine_core = true;
        let core = refused_with(body, delivered);
        assert!(!core.iter().any(|why| why.contains("4a")), "{core:?}");
        assert!(core.iter().any(|why| why.contains("4b")), "{core:?}");
        assert!(core.iter().any(|why| why.contains("4c")), "{core:?}");
        assert!(core.iter().any(|why| why.contains("4d")), "{core:?}");

        delivered.networking = true;
        let network = refused_with(body, delivered);
        assert!(!network.iter().any(|why| why.contains("4b")), "{network:?}");
        assert!(network.iter().any(|why| why.contains("4c")), "{network:?}");
        assert!(network.iter().any(|why| why.contains("4d")), "{network:?}");

        delivered.floppy_uhci = true;
        let floppy = refused_with(body, delivered);
        assert_eq!(floppy.len(), 1, "{floppy:?}");
        assert!(floppy[0].contains("4d"), "{floppy:?}");

        delivered.cirrus = true;
        assert!(refused_with(body, delivered).is_empty());
    }

    #[test]
    fn adapter_launches_and_configuration_use_the_same_hardware_refusals() {
        for profile in [
            "windows-legacy",
            "windows-xp",
            "windows-9x",
            "templeos",
            "linux-generic",
        ] {
            let file = lab(&format!(
                r#"lab "l" {{ vm "v" {{ template = "scratch" arch = "x86_64" disk = 2GiB
                    profile = "{profile}" floppy = "./f.img" nic {{ nat = true }} }} }}"#
            ));
            let machine = file.lab.machine("v").unwrap();
            let MachineCfg::Vm(vm) = machine else {
                unreachable!()
            };
            let hw =
                crate::qemu::resolve_vm(vm, None, &crate::profiles::ProfileSet::shipped().unwrap())
                    .unwrap();
            let cfg = vmlab_refusals(machine, &kvm_host(), Some(&hw));
            let launch = vmlab_vm_launch_refusals(&hw, true, true);
            assert_eq!(launch, cfg, "{profile}");
            // 4c is delivered: only a floppy off the pc machine is refused.
            assert_eq!(
                launch
                    .iter()
                    .filter(|why| why.contains("a floppy drive"))
                    .count(),
                usize::from(profile == "linux-generic"),
                "{profile}: {launch:?}"
            );
        }
    }

    #[test]
    fn adapter_launch_refusals_use_the_actual_attachment_presence() {
        let resolve = |profile: &str| {
            let file = lab(&format!(
                r#"lab "l" {{
                    vm "v" {{ template = "scratch" arch = "x86_64" disk = 2GiB
                        profile = "{profile}" }}
                }}"#
            ));
            let MachineCfg::Vm(vm) = file.lab.machine("v").unwrap() else {
                unreachable!()
            };
            crate::qemu::resolve_vm(vm, None, &crate::profiles::ProfileSet::shipped().unwrap())
                .unwrap()
        };
        // On pc, NICs (4b) and a floppy (4c) are delivered either way.
        let pc = resolve("windows-legacy");
        assert!(vmlab_vm_launch_refusals(&pc, false, false).is_empty());
        let attached = vmlab_vm_launch_refusals(&pc, true, true);
        assert!(attached.is_empty(), "{attached:?}");
        // On q35 the floppy is refused only when one is attached.
        let q35 = resolve("linux-generic");
        assert!(vmlab_vm_launch_refusals(&q35, true, false).is_empty());
        let floppy = vmlab_vm_launch_refusals(&q35, true, true);
        assert!(
            floppy.len() == 1 && floppy[0].contains("a floppy drive"),
            "{floppy:?}"
        );
    }

    /// 4a to 4d are in `hv/`: a networked legacy VM, Cirrus and floppy
    /// included, is refused nothing and runs on vmlab-hv.
    #[test]
    fn the_delivered_sub_phases_are_4a_to_4d() {
        const {
            assert!(LEGACY_PC_DELIVERED.machine_core && LEGACY_PC_DELIVERED.networking);
            assert!(LEGACY_PC_DELIVERED.floppy_uhci && LEGACY_PC_DELIVERED.cirrus);
        }
        for profile in ["windows-9x", "windows-legacy"] {
            let why = refused(&format!(
                r#"template = "scratch" arch = "x86_64" disk = 2GiB
                    profile = "{profile}" floppy = "answer.img" nic {{ nat = true }}"#
            ));
            assert!(why.is_empty(), "{profile}: {why:?}");
        }
    }

    fn all_legacy_pc_delivered() -> LegacyPcDelivery {
        LegacyPcDelivery {
            machine_core: true,
            networking: true,
            floppy_uhci: true,
            cirrus: true,
        }
    }

    #[test]
    fn each_legacy_profile_is_allowed_only_after_its_devices_arrive() {
        let base = r#"template = "scratch" arch = "x86_64" disk = 2GiB"#;
        for profile in ["windows-legacy", "windows-xp", "windows-9x", "templeos"] {
            let body = format!(r#"{base} profile = "{profile}" nic {{ nat = true }}"#);
            assert!(!refused_with(&body, LegacyPcDelivery::default()).is_empty());
            let why = refused_with(&body, all_legacy_pc_delivered());
            assert!(why.is_empty(), "{profile}: {why:?}");
        }
    }

    #[test]
    fn a_pc_without_a_nic_does_not_need_phase_4b() {
        let body = r#"template = "scratch" arch = "x86_64" disk = 2GiB
            profile = "windows-legacy""#;
        let delivered = LegacyPcDelivery {
            networking: false,
            ..all_legacy_pc_delivered()
        };
        assert!(refused_with(body, delivered).is_empty());
        let with_nic = format!("{body} nic {{ nat = true }}");
        let why = refused_with(&with_nic, delivered);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("4b"), "{why:?}");
    }

    #[test]
    fn a_pc_always_needs_its_uhci_tablet_even_without_a_floppy() {
        let body = r#"template = "scratch" arch = "x86_64" disk = 2GiB
            profile = "templeos""#;
        let delivered = LegacyPcDelivery {
            floppy_uhci: false,
            ..all_legacy_pc_delivered()
        };
        let why = refused_with(body, delivered);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("4c") && why[0].contains("UHCI"), "{why:?}");
    }

    #[test]
    fn phase_4_does_not_enable_unsupported_q35_hardware() {
        let base = r#"template = "scratch" arch = "x86_64" disk = 10GiB
            profile = "linux-generic""#;
        for extra in ["", r#"cpu_model = "pentium3""#, r#"display = "cirrus-vga""#] {
            let body = format!("{base} {extra}");
            assert_eq!(
                refused_with(&body, LegacyPcDelivery::default()),
                refused_with(&body, all_legacy_pc_delivered()),
                "{body}"
            );
        }
        let floppy = refused_with(
            &format!(r#"{base} floppy = "./f.img""#),
            all_legacy_pc_delivered(),
        );
        assert_eq!(floppy.len(), 1, "{floppy:?}");
        assert!(floppy[0].contains("pc machine"), "{floppy:?}");
    }

    #[test]
    fn phase_4_does_not_enable_unknown_pc_devices_or_pinned_tsc() {
        let file = lab(r#"lab "l" {
            vm "v" { template = "scratch" arch = "x86_64" disk = 2GiB
                profile = "windows-legacy" }
        }"#);
        let MachineCfg::Vm(vm) = file.lab.machine("v").unwrap() else {
            unreachable!()
        };
        let mut hw =
            crate::qemu::resolve_vm(vm, None, &crate::profiles::ProfileSet::shipped().unwrap())
                .unwrap();
        hw.nic_model = "rtl8139".into();
        hw.display_device = Some("unknown-vga".into());
        let mut why = Vec::new();
        hardware_refusals(&vm.name, &hw, false, all_legacy_pc_delivered(), &mut why);
        assert_eq!(why.len(), 2, "{why:?}");
        assert!(why.iter().any(|why| why.contains("rtl8139")), "{why:?}");
        assert!(why.iter().any(|why| why.contains("unknown-vga")), "{why:?}");

        let pinned = refused_with(
            r#"template = "scratch" arch = "x86_64" disk = 2GiB
                profile = "windows-legacy" tsc = "host""#,
            all_legacy_pc_delivered(),
        );
        assert_eq!(pinned.len(), 1, "{pinned:?}");
        assert!(pinned[0].contains("pinned TSC"), "{pinned:?}");
    }

    /// What no phase brings says so, and what to do.
    #[test]
    fn qemu_args_tcg_and_another_arch_are_refused() {
        let file = lab(r#"lab "l" {
  vm "busy" {
    template = "x86_64/t"
    accel = "tcg"
    qemu_args = ["-device", "foo"]
  }
  vm "rv" { template = "riscv64/t" }
}"#);
        let busy = vmlab_refusals(file.lab.machine("busy").unwrap(), &kvm_host(), None);
        assert_eq!(busy.len(), 2, "{busy:?}");
        assert!(
            busy.iter()
                .any(|w| w.contains("`qemu_args`") && w.contains("HV-D16")),
            "{busy:?}"
        );
        assert!(
            busy.iter()
                .any(|w| w.contains("accel = \"tcg\"") && w.contains("phase 3")),
            "{busy:?}"
        );
        let rv = vmlab_refusals(file.lab.machine("rv").unwrap(), &kvm_host(), None);
        assert_eq!(rv.len(), 1, "{rv:?}");
        assert!(rv[0].contains("riscv64"), "{rv:?}");
    }

    /// Phase 3d: an aarch64 VM runs on vmlab-hv's emulator, which needs no
    /// KVM and takes `accel = "tcg"`; `accel = "kvm"` and `qemu_args` are
    /// refused, naming QEMU as the troubleshooting route for the latter.
    #[test]
    fn an_aarch64_vm_runs_emulated_on_vmlab_hv() {
        let file = lab(r#"lab "l" {
  vm "plain" { template = "aarch64/t" }
  vm "tcg" { template = "aarch64/t" accel = "tcg" }
  vm "kvm" { template = "aarch64/t" accel = "kvm" }
  vm "args" { template = "aarch64/t" qemu_args = ["-no-hpet"] }
}"#);
        let no_kvm = HostFacts {
            kvm: false,
            ..kvm_host()
        };
        for host in [kvm_host(), no_kvm] {
            for name in ["plain", "tcg"] {
                let why = vmlab_refusals(file.lab.machine(name).unwrap(), &host, None);
                assert!(why.is_empty(), "{name}: {why:?}");
            }
        }
        let kvm = vmlab_refusals(file.lab.machine("kvm").unwrap(), &kvm_host(), None);
        assert!(
            kvm.len() == 1 && kvm[0].contains("accel = \"kvm\"") && kvm[0].contains("emulator"),
            "{kvm:?}"
        );
        let args = vmlab_refusals(file.lab.machine("args").unwrap(), &kvm_host(), None);
        assert!(
            args.len() == 1
                && args[0].contains("`qemu_args`")
                && args[0].contains("hypervisor = \"qemu\""),
            "{args:?}"
        );
        let arm_host = HostFacts {
            arch: "aarch64".into(),
            kvm: true,
        };
        let why = vmlab_refusals(file.lab.machine("plain").unwrap(), &arm_host, None);
        assert!(
            why.len() == 1 && why[0].contains("x86_64 host only"),
            "{why:?}"
        );
    }

    /// Automatic selection puts an aarch64 VM on vmlab-hv, and on QEMU only
    /// with the reason vmlab-hv refused it; an existing QEMU aarch64 VM moves
    /// at its next start unless vmlab-hv refuses it, when it stays, saying
    /// why; an explicit setting still wins.
    #[test]
    fn automatic_selection_puts_aarch64_on_vmlab_hv_and_migrates_existing_qemu() {
        let file = lab(r#"lab "l" {
  vm "arm" { template = "aarch64/t" }
  vm "args" { template = "aarch64/t" qemu_args = ["-no-hpet"] }
  vm "pinned" { template = "aarch64/t" hypervisor = "qemu" }
}"#);
        let none = HypervisorDefaults::default();
        let arm = file.lab.machine("arm").unwrap();
        let picked = none.select(arm, &kvm_host(), None, None);
        assert_eq!(
            (picked.kind, picked.source, picked.fallback_reason),
            (HypervisorKind::Vmlab, SelectionSource::Automatic, None)
        );
        let moved = none.select(arm, &kvm_host(), None, Some(HypervisorKind::Qemu));
        assert_eq!(
            (moved.kind, moved.source),
            (HypervisorKind::Vmlab, SelectionSource::QemuMigration)
        );
        let args = file.lab.machine("args").unwrap();
        let fallback = none.select(args, &kvm_host(), None, None);
        assert_eq!(fallback.kind, HypervisorKind::Qemu);
        assert!(
            fallback
                .fallback_reason
                .as_deref()
                .is_some_and(|w| w.contains("`qemu_args`")),
            "{fallback:?}"
        );
        let kept = none.select(args, &kvm_host(), None, Some(HypervisorKind::Qemu));
        assert_eq!(
            (kept.kind, kept.source),
            (HypervisorKind::Qemu, SelectionSource::Existing)
        );
        assert!(
            kept.fallback_reason
                .as_deref()
                .is_some_and(|w| w.contains("keeping existing QEMU") && w.contains("`qemu_args`")),
            "{kept:?}"
        );
        let pinned = file.lab.machine("pinned").unwrap();
        for existing in [
            None,
            Some(HypervisorKind::Qemu),
            Some(HypervisorKind::Vmlab),
        ] {
            let picked = none.select(pinned, &kvm_host(), None, existing);
            assert_eq!(
                (picked.kind, picked.source),
                (HypervisorKind::Qemu, SelectionSource::Machine)
            );
            assert!(qemu_x86_refusals(pinned, &picked, &kvm_host(), None).is_empty());
        }
        let by_host = HypervisorDefaults {
            vm: Some(HypervisorKind::Qemu),
            container: None,
        };
        assert_eq!(
            by_host
                .select(arm, &kvm_host(), None, Some(HypervisorKind::Qemu))
                .kind,
            HypervisorKind::Qemu
        );
    }

    /// The emulated `virt` machine runs a modern aarch64 Linux VM headless
    /// (its virtio GPU is left out) and refuses, by name, the hardware it
    /// does not present yet.
    #[test]
    fn an_aarch64_vm_is_refused_only_what_virt_lacks() {
        let base = r#"template = "scratch" arch = "aarch64" profile = "linux-modern" disk = 10GiB"#;
        for body in [
            base.to_string(),
            format!("{base} cpus = 8 secure_boot = true"),
            format!(r#"{base} accel = "tcg""#),
            format!(r#"{base} display = "virtio-gpu""#),
        ] {
            let why = refused(&body);
            assert!(why.is_empty(), "{body}: {why:?}");
        }
        for (extra, word) in [
            ("cpus = 9", "GICv2"),
            ("usb = true", "usb = true"),
            ("tpm = true", "TPM"),
            ("audio = true", "sound card"),
            (r#"cpu_model = "cortex-a72""#, "cpu_model"),
            (r#"gpu { mode = "virgl" }"#, "gpu {}"),
        ] {
            let why = refused(&format!("{base} {extra}"));
            assert!(
                why.len() == 1 && why[0].contains(word) && why[0].contains("`virt`"),
                "{extra}: {why:?}"
            );
        }
    }

    /// Without its hardware — a template validation cannot see — a VM is
    /// refused only what its declaration says; the daemon checks the rest.
    #[test]
    fn without_hardware_only_the_declaration_is_checked() {
        let file = lab(r#"lab "l" { vm "v" { template = "x86_64/t" accel = "tcg" } }"#);
        let why = vmlab_refusals(file.lab.machine("v").unwrap(), &kvm_host(), None);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("phase 3"), "{}", why[0]);
        // A floppy is delivered (4c); only its machine, which the
        // template decides, can still refuse it.
        let floppy = lab(r#"lab "l" { vm "v" { template = "x86_64/t" floppy = "./f" } }"#);
        assert!(vmlab_refusals(floppy.lab.machine("v").unwrap(), &kvm_host(), None).is_empty());

        let plain = lab(r#"lab "l" { vm "v" { template = "x86_64/t" } }"#);
        assert!(vmlab_refusals(plain.lab.machine("v").unwrap(), &kvm_host(), None).is_empty());
    }

    /// Audio is vmlab-hv's alone: on QEMU each key is its own refusal,
    /// naming the hypervisor that brings it.
    #[test]
    fn audio_on_qemu_is_refused_naming_vmlab() {
        let file = lab(r#"lab "l" {
  vm "loud" { template = "x86_64/t" audio = true microphone = true }
  vm "quiet" { template = "x86_64/t" }
}"#);
        let vm = |n: &str| match file.lab.machine(n).unwrap() {
            crate::config::model::MachineCfg::Vm(vm) => vm,
            _ => unreachable!(),
        };
        let why = qemu_refusals(vm("loud"));
        assert_eq!(why.len(), 2, "{why:?}");
        assert!(why[0].contains("audio = true") && why[0].contains("hypervisor = \"vmlab\""));
        assert!(why[1].contains("microphone = true"), "{why:?}");
        assert!(qemu_refusals(vm("quiet")).is_empty());
    }

    /// D9: an accelerated or passed-through GPU blocks online snapshots on
    /// vmlab-hv, naming which; no GPU blocks nothing.
    #[test]
    fn a_gpu_refuses_online_snapshots_on_vmlab_hv() {
        let hw = |body: &str| {
            let file = lab(&format!(
                r#"lab "l" {{ vm "v" {{ template = "scratch" arch = "x86_64" disk = 10GiB profile = "linux-modern" {body} }} }}"#
            ));
            let crate::config::model::MachineCfg::Vm(vm) = file.lab.machine("v").unwrap() else {
                unreachable!()
            };
            crate::qemu::resolve_vm(vm, None, &crate::profiles::ProfileSet::shipped().unwrap())
                .unwrap()
        };
        assert_eq!(vmlab_online_snapshot_refusal(&hw("")), None);
        let virgl = vmlab_online_snapshot_refusal(&hw(r#"gpu { mode = "virgl" }"#)).unwrap();
        assert!(
            virgl.contains("accelerated GPU") && virgl.contains("offline"),
            "{virgl}"
        );
        let vfio = vmlab_online_snapshot_refusal(&hw(r#"gpu { mode = "passthrough" }"#)).unwrap();
        assert!(vfio.contains("passed through"), "{vfio}");
    }

    #[test]
    fn a_vm_without_kvm_is_refused_naming_kvm() {
        let file = lab(r#"lab "l" { vm "v" { template = "x86_64/t" } }"#);
        let host = HostFacts {
            kvm: false,
            ..kvm_host()
        };
        let why = vmlab_refusals(file.lab.machine("v").unwrap(), &host, None);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("KVM is required"), "{}", why[0]);
        assert!(why[0].contains("/dev/kvm"), "{}", why[0]);
        assert!(!why[0].contains("hypervisor = \"qemu\""), "{}", why[0]);
    }

    /// An x86 VM on a host of another architecture has no KVM for it.
    #[test]
    fn an_x86_vm_on_another_host_arch_needs_an_x86_64_kvm_host() {
        let file = lab(r#"lab "l" { vm "v" { template = "x86_64/t" } }"#);
        let host = HostFacts {
            arch: "aarch64".into(),
            kvm: true,
        };
        let machine = file.lab.machine("v").unwrap();
        let why = vmlab_refusals(machine, &host, None);
        assert!(
            why.iter()
                .any(|w| w.contains("KVM on an x86_64 host is required")),
            "{why:?}"
        );
        let picked = HypervisorDefaults::default().select(machine, &host, None, None);
        assert_eq!(picked.kind, HypervisorKind::Vmlab);
    }

    // -- QEMU runs no x86 guest ---------------------------------------------

    fn qemu_refused(
        file: &crate::config::LabFile,
        name: &str,
        defaults: HypervisorDefaults,
        host: &HostFacts,
    ) -> Vec<String> {
        let machine = file.lab.machine(name).unwrap();
        let selection = defaults.select(machine, host, None, None);
        qemu_x86_refusals(machine, &selection, host, None)
    }

    /// Each setting that puts an x86 machine on QEMU is an error naming the
    /// replacement, and so is `qemu_args` on an x86 VM.
    #[test]
    fn every_setting_that_puts_an_x86_machine_on_qemu_is_an_error() {
        let file = lab(r#"lab "l" {
  vm "explicit" { template = "x86_64/t" hypervisor = "qemu" }
  vm "plain" { template = "x86_64/t" }
  vm "args" { template = "x86_64/t" hypervisor = "qemu" qemu_args = ["-no-hpet"] }
  container "c" { image = "alpine" }
}"#);
        let none = HypervisorDefaults::default();
        let host = kvm_host();

        let why = qemu_refused(&file, "explicit", none, &host);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("vm \"explicit\""), "{why:?}");
        assert!(why[0].contains("set hypervisor = \"vmlab\""), "{why:?}");

        let by_host = HypervisorDefaults {
            vm: Some(HypervisorKind::Qemu),
            container: Some(HypervisorKind::Qemu),
        };
        let why = qemu_refused(&file, "plain", by_host, &host);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("hypervisor { vm = \"qemu\" }"), "{why:?}");
        assert!(why[0].contains("remove that default"), "{why:?}");
        let why = qemu_refused(&file, "c", by_host, &host);
        assert!(why[0].contains("{ container = \"qemu\" }"), "{why:?}");

        assert!(qemu_refused(&file, "plain", none, &host).is_empty());
        assert!(qemu_refused(&file, "c", none, &host).is_empty());

        let why = qemu_refused(&file, "args", none, &host);
        assert_eq!(why.len(), 2, "{why:?}");
        assert!(
            why[1].contains("`qemu_args`") && why[1].contains("remove it"),
            "{why:?}"
        );
    }

    /// QEMU keeps the other architectures, `qemu_args` and settings alike.
    #[test]
    fn other_architectures_may_run_on_qemu() {
        let file = lab(r#"lab "l" {
  vm "arm" { template = "aarch64/t" hypervisor = "qemu" qemu_args = ["-no-hpet"] }
  vm "rv" { template = "riscv64/t" }
  container "c" { image = "alpine" }
}"#);
        let none = HypervisorDefaults::default();
        assert!(qemu_refused(&file, "arm", none, &kvm_host()).is_empty());
        assert!(qemu_refused(&file, "rv", none, &kvm_host()).is_empty());
        let arm_host = HostFacts {
            arch: "aarch64".into(),
            kvm: true,
        };
        let by_host = HypervisorDefaults {
            vm: None,
            container: Some(HypervisorKind::Qemu),
        };
        assert!(qemu_refused(&file, "c", by_host, &arm_host).is_empty());
        let rv = file.lab.machine("rv").unwrap();
        assert_eq!(
            none.select(rv, &kvm_host(), None, None).kind,
            HypervisorKind::Qemu
        );
        // riscv64 stays on QEMU however it was run before (§22).
        assert_eq!(
            none.select(rv, &kvm_host(), None, Some(HypervisorKind::Qemu))
                .kind,
            HypervisorKind::Qemu
        );
    }
}
