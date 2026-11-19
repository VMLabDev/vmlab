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
    /// QEMU, driven over QMP: every machine vmlab has ever run.
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
    /// `None` selects vmlab-hv for supported new VMs, QEMU otherwise.
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
    /// An existing x86 QEMU machine moving to vmlab-hv at its next start.
    QemuMigration,
    /// Automatic selection: vmlab-hv unless [`vmlab_refusals`] names a reason.
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
    /// selection falls back; an explicit vmlab selection is still validated.
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
        if explicit.is_none()
            && existing == Some(HypervisorKind::Qemu)
            && x86_guest(&guest_arch(machine, host, hardware))
        {
            return HypervisorSelection {
                kind: HypervisorKind::Vmlab,
                source: SelectionSource::QemuMigration,
                fallback_reason: None,
            };
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
        let why = vmlab_refusals(machine, host, hardware);
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

/// The one architecture `vmlab-hv` runs today.
const VMLAB_ARCH: &str = "x86_64";

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
    if host.arch != VMLAB_ARCH {
        why.push(arch_refusal("vm host", name, &host.arch));
    }
    if !vm.qemu_args.is_empty() {
        why.push(qemu_args_refusal("vm", name));
    }
    if vm.accel == Some(AccelMode::Tcg) {
        why.push(format!(
            "vm \"{name}\": accel = \"tcg\" — vmlab-hv runs under KVM only until phase 3 \
             (PRD §22); remove `accel`, or use hypervisor = \"qemu\""
        ));
    }
    let arch = hardware
        .map(|h| crate::qemu::qemu_arch(&h.arch).to_string())
        .or_else(|| crate::qemu::resolve::vm_arch(vm).map(|a| crate::qemu::qemu_arch(&a).into()));
    if let Some(arch) = &arch
        && arch != VMLAB_ARCH
    {
        why.push(arch_refusal("vm", name, arch));
    } else if !host.kvm && vm.accel != Some(AccelMode::Tcg) {
        why.push(format!(
            "vm \"{name}\": hypervisor = \"vmlab\" needs KVM, and /dev/kvm is not usable \
             on this host — vmlab-hv has no emulation until phase 3 (PRD §22); use \
             hypervisor = \"qemu\", which falls back to TCG"
        ));
    }
    let has_floppy = vm.floppy.is_some()
        || vm
            .media
            .iter()
            .any(|m| m.kind == crate::config::model::MediaKind::Floppy);
    why.extend(vm_hardware_refusals(
        name,
        hardware,
        !vm.nics.is_empty(),
        has_floppy,
        delivered,
    ));
    why
}

/// The resolved launch's hardware refusals, including attachments not carried
/// by [`ResolvedVm`]. The adapter checks these before it spawns, using the
/// same delivery gates as configuration validation and the lab daemon.
pub(crate) fn vmlab_vm_launch_refusals(
    hardware: &ResolvedVm,
    has_nics: bool,
    has_floppy: bool,
) -> Vec<String> {
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
                 use hypervisor = \"qemu\" for a floppy on another machine"
            ));
        }
    }
    if let Some(h) = hardware {
        hardware_refusals(name, h, has_nics, delivered, &mut why);
    }
    why
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
    // macOS has a separate gate for its complete hardware configuration.
    if h.profile.as_deref() == Some("macos") || h.apple_smc {
        why.push(phase(
            name,
            "a macOS guest",
            "macOS (its own phase, after 2)",
        ));
        return;
    }
    let pc = legacy_pc(&h.machine);
    if pc {
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
             vmlab-hv supports (PRD §22); use hypervisor = \"qemu\"",
            h.machine
        ));
    }
    match h.display_device.as_deref() {
        None | Some("VGA") | Some("virtio-vga") => {}
        Some("cirrus-vga") if pc => {
            if !delivered.cirrus {
                why.push(phase(name, "the Cirrus VGA display", "4d (Cirrus)"));
            }
        }
        Some(other) => why.push(format!(
            "vm \"{name}\": display device `{other}` — vmlab-hv presents std VGA and \
             virtio-vga only (PRD §22); set display = \"std\" or \"virtio-vga\", or use \
             hypervisor = \"qemu\""
        )),
    }
    match h.nic_model.as_str() {
        // vmlab-hv's pc presents the NICs its captures show, e1000 and PCnet.
        model if model.starts_with("virtio-net") && pc && has_nics => why.push(format!(
            "vm \"{name}\": nic_model `{model}` on the pc machine — vmlab-hv's pc presents \
             e1000 and PCnet NICs only (PRD §22); set nic_model = \"e1000\" or \"pcnet\", or \
             use hypervisor = \"qemu\""
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
            "vm \"{name}\": nic_model `{}` — vmlab-hv presents virtio-net NICs only (PRD §22); \
             use hypervisor = \"qemu\"",
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
             (PRD §22); use hypervisor = \"qemu\" on another machine"
        ));
    }
    if h.input == crate::config::model::InputDevices::Usb {
        why.push(phase(name, "input = \"usb\" (a USB keyboard)", "after 2b"));
    }
    if let Some(model) = &h.cpu_model {
        if pc && !delivered.machine_core {
            why.push(phase(
                name,
                &format!("cpu_model = \"{model}\" on pc"),
                "4a (the pc machine core)",
            ));
        } else if !pc {
            why.push(format!(
                "vm \"{name}\": cpu_model = \"{model}\" — vmlab-hv runs the host CPU only \
                 on q35 (PRD §22); remove it, or use hypervisor = \"qemu\""
            ));
        }
    }
    // vmlab-hv pins the TSC (invtsc and tsc_khz) on q35 only; its pc
    // machine refuses both, as it does the other macOS-era CPU features.
    if pc && h.tsc.is_some() {
        why.push(format!(
            "vm \"{name}\": a pinned TSC (`tsc = \"host\"`) on the pc machine — vmlab-hv \
             pins the TSC on q35 only (PRD §22); remove `tsc`, or use hypervisor = \"qemu\""
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

/// Why an x86 machine is on QEMU, as the removal warning names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QemuReason {
    /// The machine sets `hypervisor = "qemu"`.
    Explicit,
    /// The host config's `hypervisor {}` default for its kind is `qemu`.
    HostDefault,
    /// Its disks were made under QEMU, and a machine keeps its hypervisor.
    Existing,
    /// It declares `qemu_args`, which only QEMU takes.
    QemuArgs,
    /// vmlab-hv refuses it as declared, so automatic selection fell back.
    Refused,
}

/// One x86 machine that runs on QEMU, which a later release stops running
/// (the warning release before the removal): why it is there and what to do
/// about it, and the online snapshots that go with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QemuRemoval {
    pub reason: QemuReason,
    /// The whole warning: the machine, the reason and the fix.
    pub warning: String,
    /// Online snapshots taken under QEMU. They stay on disk, but restoring
    /// one by name is refused once QEMU no longer runs x86 guests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub online_snapshots: Vec<String>,
}

impl QemuRemoval {
    /// What a surface prints: the warning, then the snapshots it costs.
    pub fn lines(&self, machine: &str) -> Vec<String> {
        let mut lines = vec![self.warning.clone()];
        if !self.online_snapshots.is_empty() {
            lines.push(format!(
                "\"{machine}\": online snapshot(s) {} were taken under QEMU and cannot be \
                 restored once QEMU support for x86 guests is removed — to keep one, restore \
                 it, shut the machine down and take an offline snapshot, which either \
                 hypervisor restores",
                self.online_snapshots
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines
    }
}

/// Whether `arch` is an x86 guest, the only kind the removal touches.
fn x86_guest(arch: &str) -> bool {
    crate::qemu::qemu_arch(arch) == VMLAB_ARCH
}

fn guest_arch(machine: MachineCfg<'_>, host: &HostFacts, hardware: Option<&ResolvedVm>) -> String {
    match machine {
        MachineCfg::Vm(vm) => hardware
            .map(|h| h.arch.clone())
            .or_else(|| crate::qemu::resolve::vm_arch(vm))
            .unwrap_or_else(|| host.arch.clone()),
        MachineCfg::Container(_) => host.arch.clone(),
    }
}

/// The removal warning for `machine` under `selection` — `None` unless it is
/// an x86 or x86_64 guest that runs on QEMU. `hardware` as in
/// [`vmlab_refusals`]: without it a VM whose template is not local reports
/// only what its declaration shows.
pub fn qemu_removal(
    machine: MachineCfg<'_>,
    selection: &HypervisorSelection,
    host: &HostFacts,
    hardware: Option<&ResolvedVm>,
) -> Option<QemuRemoval> {
    if selection.kind != HypervisorKind::Qemu {
        return None;
    }
    if !x86_guest(&guest_arch(machine, host, hardware)) {
        return None;
    }
    let (kind, name) = match machine {
        MachineCfg::Vm(vm) => ("vm", &vm.name),
        MachineCfg::Container(c) => ("container", &c.name),
    };
    let blockers = vmlab_refusals(machine, host, hardware);
    let qemu_args = matches!(machine, MachineCfg::Vm(vm) if !vm.qemu_args.is_empty());
    let reason = match selection.source {
        SelectionSource::Machine => QemuReason::Explicit,
        SelectionSource::HostDefault => QemuReason::HostDefault,
        SelectionSource::Existing => QemuReason::Existing,
        SelectionSource::QemuMigration => return None,
        SelectionSource::Automatic if qemu_args => QemuReason::QemuArgs,
        SelectionSource::Automatic => QemuReason::Refused,
    };
    let (because, fix) = match reason {
        QemuReason::Explicit => (
            "it sets hypervisor = \"qemu\"".to_string(),
            format!(
                "set hypervisor = \"vmlab\" on it (removing the line alone leaves a machine \
                 that already has disks on QEMU until `vmlab {kind} destroy {name}`)"
            ),
        ),
        QemuReason::HostDefault => (
            format!("the host config sets `hypervisor {{ {kind} = \"qemu\" }}`"),
            format!(
                "remove that default from the host config; a machine that already has disks \
                 then needs hypervisor = \"vmlab\" as well, or `vmlab {kind} destroy {name}`"
            ),
        ),
        QemuReason::Existing => (
            "its disks were made under QEMU and a machine keeps its hypervisor".to_string(),
            format!(
                "set hypervisor = \"vmlab\" on it, or run `vmlab {kind} destroy {name}` so \
                 the next `up` builds it on vmlab-hv"
            ),
        ),
        QemuReason::QemuArgs => (
            "it declares `qemu_args`, which only QEMU takes".to_string(),
            "remove `qemu_args`".to_string(),
        ),
        QemuReason::Refused => (
            "vmlab-hv cannot run it as declared".to_string(),
            "change what is named below so that vmlab-hv runs it".to_string(),
        ),
    };
    let mut warning = format!(
        "{kind} \"{name}\" runs on QEMU because {because}. QEMU support for x86 guests is \
         going away in a later vmlab release, and this machine will then not start. Fix: {fix}."
    );
    // Not for `qemu_args` alone, which the reason above already names.
    if !(reason == QemuReason::QemuArgs && blockers.len() == 1) && !blockers.is_empty() {
        warning.push_str(&format!(
            " vmlab-hv refuses it today, and `hypervisor = \"qemu\"` stops being the way out \
             for an x86 guest: {}",
            blockers.join("; ")
        ));
    }
    Some(QemuRemoval {
        reason,
        warning,
        online_snapshots: Vec::new(),
    })
}

/// The host half of the removal warning: x86 guests run under QEMU's
/// emulation (TCG) on a host without usable KVM, and vmlab-hv has none. Said
/// once per lab that has an x86 machine on QEMU.
pub fn kvm_removal_warning(host: &HostFacts) -> Option<String> {
    if host.arch != VMLAB_ARCH {
        return Some(format!(
            "this host is {} and runs x86 guests under QEMU's emulation (TCG). x86 guests \
             will need KVM on an x86_64 host once QEMU support for x86 guests is removed in \
             a later vmlab release",
            host.arch
        ));
    }
    (!host.kvm).then(|| {
        "/dev/kvm is not usable on this host, so x86 guests run under QEMU's emulation \
         (TCG). x86 guests will need KVM once QEMU support for x86 guests is removed in a \
         later vmlab release — enable virtualization in the firmware, load the kvm module, \
         and give this user access to /dev/kvm"
            .to_string()
    })
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
        "vm \"{name}\": {what} — vmlab-hv runs it from phase {phase} (PRD §22); use \
         hypervisor = \"qemu\" until then"
    )
}

fn container_refusals(c: &Container, host: &HostFacts) -> Vec<String> {
    let name = &c.name;
    let mut why = Vec::new();
    // A container runs the host's own architecture.
    if host.arch != VMLAB_ARCH {
        why.push(arch_refusal("container", name, &host.arch));
    } else if !host.kvm {
        why.push(format!(
            "container \"{name}\": hypervisor = \"vmlab\" needs KVM, and /dev/kvm is not usable \
             on this host — vmlab-hv has no emulation fallback yet (PRD §22); use \
             hypervisor = \"qemu\", which falls back to TCG"
        ));
    }
    why
}

/// `qemu_args` is QEMU's argv; no other hypervisor has one to append to, and
/// none ever will (HV-D16).
fn qemu_args_refusal(kind: &str, name: &str) -> String {
    format!(
        "{kind} \"{name}\": `qemu_args` is QEMU's own argv and means nothing to \
         hypervisor = \"vmlab\", in any phase (PRD §22, HV-D16) — drop it, or run this \
         machine on \"qemu\""
    )
}

fn arch_refusal(kind: &str, name: &str, arch: &str) -> String {
    format!(
        "{kind} \"{name}\": hypervisor = \"vmlab\" runs x86_64 only so far, and this is \
         {arch} (PRD §22) — use hypervisor = \"qemu\""
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
        let picked = defaults.select(args, &kvm_host(), None, None);
        assert_eq!(picked.kind, HypervisorKind::Qemu);
        assert!(picked.fallback_reason.unwrap().contains("`qemu_args`"));
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
        assert_eq!(picked.kind, HypervisorKind::Qemu);
        assert!(picked.fallback_reason.unwrap().contains("/dev/kvm"));
    }

    #[test]
    fn automatic_selection_uses_the_resolved_refusal_list() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        for setting in [
            r#"profile = "macos""#,
            r#"profile = "linux-modern" accel = "tcg""#,
        ] {
            let file = lab(&format!(
                r#"lab "l" {{ vm "v" {{ template = "scratch" arch = "x86_64" disk = 1GiB {setting} }} }}"#
            ));
            let vm = &file.lab.vms[0];
            let hw = crate::qemu::resolve_vm(vm, None, &profiles).unwrap();
            let picked = HypervisorDefaults::default().select(
                MachineCfg::Vm(vm),
                &kvm_host(),
                Some(&hw),
                None,
            );
            assert_eq!(picked.kind, HypervisorKind::Qemu, "{setting}");
            assert_eq!(
                picked.fallback_reason,
                Some(vmlab_refusals(MachineCfg::Vm(vm), &kvm_host(), Some(&hw)).join("; "))
            );
        }
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
        for host in [
            HostFacts {
                kvm: false,
                ..kvm_host()
            },
            HostFacts {
                arch: "aarch64".into(),
                ..kvm_host()
            },
        ] {
            let picked = defaults.select(machine, &host, None, None);
            assert_eq!(picked.kind, HypervisorKind::Qemu);
            assert_eq!(
                picked.fallback_reason,
                Some(vmlab_refusals(machine, &host, None).join("; "))
            );
        }
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
        let cases = [
            (r#"profile = "macos""#, "phase macOS"),
            (r#"profile = "linux-modern" accel = "tcg""#, "phase 3"),
        ];
        for (body, phase) in cases {
            let why = refused(&format!("{base} {body}"));
            assert!(
                why.iter()
                    .any(|w| w.contains(phase) && w.contains("vm \"v\"")),
                "{body}: expected {phase} in {why:?}"
            );
        }
        // macOS retains its separate hardware gate.
        assert_eq!(refused(&format!(r#"{base} profile = "macos""#)).len(), 1);
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
    template = "aarch64/t"
    accel = "tcg"
    qemu_args = ["-device", "foo"]
  }
}"#);
        let busy = vmlab_refusals(file.lab.machine("busy").unwrap(), &kvm_host(), None);
        assert_eq!(busy.len(), 3, "{busy:?}");
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
        assert!(busy.iter().any(|w| w.contains("aarch64")), "{busy:?}");
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
        assert!(why[0].contains("/dev/kvm"), "{}", why[0]);
    }

    // -- the QEMU removal warning --------------------------------------------

    fn removal(
        file: &crate::config::LabFile,
        name: &str,
        defaults: HypervisorDefaults,
        host: &HostFacts,
        existing: Option<HypervisorKind>,
    ) -> Option<QemuRemoval> {
        let machine = file.lab.machine(name).unwrap();
        let selection = defaults.select(machine, host, None, existing);
        qemu_removal(machine, &selection, host, None)
    }

    /// Each rung of the ladder that lands an x86 machine on QEMU is its own
    /// reason, and each warning names the machine, the removal and a fix.
    #[test]
    fn every_x86_machine_on_qemu_is_warned_naming_its_reason_and_fix() {
        let file = lab(r#"lab "l" {
  vm "explicit" { template = "x86_64/t" hypervisor = "qemu" }
  vm "plain" { template = "x86_64/t" }
  vm "args" { template = "x86_64/t" qemu_args = ["-no-hpet"] }
  vm "tcg" { template = "x86_64/t" accel = "tcg" }
  container "c" { image = "alpine" }
}"#);
        let none = HypervisorDefaults::default();
        let host = kvm_host();
        let qemu = Some(HypervisorKind::Qemu);

        let w = removal(&file, "explicit", none, &host, None).unwrap();
        assert_eq!(w.reason, QemuReason::Explicit);
        assert!(w.warning.contains("vm \"explicit\" runs on QEMU"), "{w:?}");
        assert!(w.warning.contains("it sets hypervisor = \"qemu\""), "{w:?}");
        assert!(w.warning.contains("going away"), "{w:?}");
        assert!(
            w.warning.contains("Fix: set hypervisor = \"vmlab\""),
            "{w:?}"
        );
        // vmlab-hv would run it, so nothing else is named.
        assert!(!w.warning.contains("refuses it today"), "{w:?}");

        let by_host = HypervisorDefaults {
            vm: qemu,
            container: qemu,
        };
        let w = removal(&file, "plain", by_host, &host, None).unwrap();
        assert_eq!(w.reason, QemuReason::HostDefault);
        assert!(w.warning.contains("hypervisor { vm = \"qemu\" }"), "{w:?}");
        assert!(w.warning.contains("from the host config"), "{w:?}");
        let w = removal(&file, "c", by_host, &host, None).unwrap();
        assert_eq!(w.reason, QemuReason::HostDefault);
        assert!(w.warning.contains("container \"c\" runs on QEMU"), "{w:?}");
        assert!(w.warning.contains("{ container = \"qemu\" }"), "{w:?}");

        assert!(removal(&file, "plain", none, &host, qemu).is_none());

        let w = removal(&file, "args", none, &host, None).unwrap();
        assert_eq!(w.reason, QemuReason::QemuArgs);
        assert!(w.warning.contains("Fix: remove `qemu_args`"), "{w:?}");
        assert!(!w.warning.contains("refuses it today"), "{w:?}");

        let w = removal(&file, "tcg", none, &host, None).unwrap();
        assert_eq!(w.reason, QemuReason::Refused);
        assert!(w.warning.contains("vmlab-hv cannot run it"), "{w:?}");
        assert!(w.warning.contains("accel = \"tcg\""), "{w:?}");
    }

    /// An explicit setting that also hides a refusal says both: moving the
    /// machine takes more than changing `hypervisor`.
    #[test]
    fn an_explicit_qemu_machine_names_what_vmlab_hv_refuses_too() {
        let file = lab(r#"lab "l" {
  vm "v" { template = "x86_64/t" hypervisor = "qemu" qemu_args = ["-no-hpet"] }
}"#);
        let w = removal(&file, "v", HypervisorDefaults::default(), &kvm_host(), None).unwrap();
        assert_eq!(w.reason, QemuReason::Explicit);
        assert!(w.warning.contains("refuses it today"), "{w:?}");
        assert!(w.warning.contains("`qemu_args`"), "{w:?}");
    }

    /// The removal is x86's: another architecture on QEMU, and any machine
    /// on vmlab-hv, are told nothing.
    #[test]
    fn other_architectures_and_vmlab_hv_machines_are_not_warned() {
        let file = lab(r#"lab "l" {
  vm "arm" { template = "aarch64/t" }
  vm "hv" { template = "x86_64/t" }
  container "c" { image = "alpine" }
}"#);
        let none = HypervisorDefaults::default();
        assert_eq!(removal(&file, "arm", none, &kvm_host(), None), None);
        assert_eq!(removal(&file, "hv", none, &kvm_host(), None), None);
        assert_eq!(removal(&file, "c", none, &kvm_host(), None), None);
        // A container runs the host's architecture, whatever QEMU it is on.
        let arm_host = HostFacts {
            arch: "aarch64".into(),
            kvm: true,
        };
        assert_eq!(removal(&file, "c", none, &arm_host, None), None);
    }

    /// A host without usable KVM is told x86 guests will need it, and its
    /// x86 machines are warned with KVM as what vmlab-hv refuses.
    #[test]
    fn a_host_without_kvm_is_warned_that_x86_guests_will_need_it() {
        assert_eq!(kvm_removal_warning(&kvm_host()), None);
        let no_kvm = HostFacts {
            arch: "x86_64".into(),
            kvm: false,
        };
        let w = kvm_removal_warning(&no_kvm).unwrap();
        assert!(w.contains("/dev/kvm"), "{w}");
        assert!(w.contains("will need KVM"), "{w}");
        let arm = HostFacts {
            arch: "aarch64".into(),
            kvm: true,
        };
        let w = kvm_removal_warning(&arm).unwrap();
        assert!(w.contains("aarch64"), "{w}");
        assert!(w.contains("will need KVM"), "{w}");

        let file = lab(r#"lab "l" { vm "v" { template = "x86_64/t" } }"#);
        let w = removal(&file, "v", HypervisorDefaults::default(), &no_kvm, None).unwrap();
        assert_eq!(w.reason, QemuReason::Refused);
        assert!(w.warning.contains("/dev/kvm"), "{w:?}");
    }

    /// The online snapshots are a line of their own, and none is none.
    #[test]
    fn the_warning_lists_the_online_snapshots_the_removal_costs() {
        let mut w = QemuRemoval {
            reason: QemuReason::Existing,
            warning: "vm \"v\" runs on QEMU".into(),
            online_snapshots: Vec::new(),
        };
        assert_eq!(w.lines("v"), ["vm \"v\" runs on QEMU"]);
        let json = serde_json::to_value(&w).unwrap();
        assert_eq!(json["reason"], "existing");
        assert!(json.get("online_snapshots").is_none(), "{json}");
        w.online_snapshots = vec!["warm".into(), "hot".into()];
        let lines = w.lines("v");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].contains("\"warm\", \"hot\""), "{lines:?}");
        assert!(lines[1].contains("cannot be restored"), "{lines:?}");
        assert!(lines[1].contains("offline snapshot"), "{lines:?}");
        let json = serde_json::to_value(&w).unwrap();
        assert_eq!(json["online_snapshots"][1], "hot");
    }
}
