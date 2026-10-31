//! Which hypervisor runs a machine (PRD §22, ADR-0021), and what the second
//! one cannot run yet.
//!
//! A machine names its hypervisor with `hypervisor = "qemu" | "vmlab"`; one
//! that names none takes the host config's default for its kind, which
//! starts at QEMU for both. `vmlab` is `vmlab-hv`, vmlab's own hypervisor,
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
    pub container: HypervisorKind,
    pub vm: HypervisorKind,
}

impl HypervisorDefaults {
    /// The hypervisor `machine` runs on: its own `hypervisor`, else this
    /// host's default for its kind.
    pub fn effective(&self, machine: MachineCfg<'_>) -> HypervisorKind {
        match machine {
            MachineCfg::Vm(vm) => vm.hypervisor.unwrap_or(self.vm),
            MachineCfg::Container(c) => c.hypervisor.unwrap_or(self.container),
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
        MachineCfg::Vm(vm) => vm_refusals(vm, host, hardware),
        MachineCfg::Container(c) => container_refusals(c, host),
    }
}

fn vm_refusals(vm: &Vm, host: &HostFacts, hardware: Option<&ResolvedVm>) -> Vec<String> {
    let name = &vm.name;
    let mut why = Vec::new();
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
    if vm.floppy.is_some()
        || vm
            .media
            .iter()
            .any(|m| m.kind == crate::config::model::MediaKind::Floppy)
    {
        why.push(phase(name, "a floppy drive", "4 (the legacy PC)"));
    }
    if let Some(h) = hardware {
        hardware_refusals(name, h, &mut why);
    }
    why
}

/// What the resolved hardware asks for that vmlab-hv cannot present yet.
fn hardware_refusals(name: &str, h: &ResolvedVm, why: &mut Vec<String>) {
    use crate::profiles::AgentTransport;
    // A macOS guest and the legacy PC are phases of their own: their
    // hardware differs everywhere, so the one reason says all of it.
    if h.profile.as_deref() == Some("macos") || h.apple_smc {
        why.push(phase(
            name,
            "a macOS guest",
            "macOS (its own phase, after 2)",
        ));
        return;
    }
    if !h.machine.starts_with("q35") {
        why.push(phase(
            name,
            &format!("machine = \"{}\" (i440fx)", h.machine),
            "4 (the legacy PC)",
        ));
        return;
    }
    match h.display_device.as_deref() {
        None | Some("VGA") | Some("virtio-vga") => {}
        Some(other) => why.push(format!(
            "vm \"{name}\": display device `{other}` — vmlab-hv presents std VGA and \
             virtio-vga only (PRD §22); set display = \"std\" or \"virtio-vga\", or use \
             hypervisor = \"qemu\""
        )),
    }
    if !h.nic_model.starts_with("virtio-net") {
        why.push(format!(
            "vm \"{name}\": nic_model `{}` — vmlab-hv presents virtio-net NICs only (PRD §22); \
             use hypervisor = \"qemu\"",
            h.nic_model
        ));
    }
    if h.agent_transport == AgentTransport::IsaSerial {
        why.push(phase(
            name,
            "agent_transport = \"isa-serial\"",
            "4 (the legacy PC)",
        ));
    }
    if h.input == crate::config::model::InputDevices::Usb {
        why.push(phase(name, "input = \"usb\" (a USB keyboard)", "after 2b"));
    }
    if let Some(model) = &h.cpu_model {
        why.push(format!(
            "vm \"{name}\": cpu_model = \"{model}\" — vmlab-hv runs the host CPU only \
             (PRD §22); remove it, or use hypervisor = \"qemu\""
        ));
    }
    // vmlab reads the host's TSC frequency through a paused QEMU, and
    // vmlab-hv has neither the probe that would replace it (HV phase-2 D10)
    // nor a frequency to pin in its machine description yet.
    if h.tsc.is_some() {
        why.push(phase(
            name,
            "a pinned TSC (`tsc`)",
            "after 2f (D10's host probe)",
        ));
    }
}

/// Why QEMU cannot run `vm` as declared — what only vmlab-hv brings, one
/// sentence per reason; empty when it can. Read by validation for every VM
/// that runs on QEMU, as [`vmlab_refusals`] is for every one that does not.
pub fn qemu_refusals(vm: &Vm) -> Vec<String> {
    let name = &vm.name;
    let mut why = Vec::new();
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
            container: HypervisorKind::Qemu,
            vm: HypervisorKind::Vmlab,
        };
        let pick = |name: &str| defaults.effective(file.lab.machine(name).unwrap());
        assert_eq!(pick("a"), HypervisorKind::Vmlab);
        assert_eq!(pick("b"), HypervisorKind::Qemu);
        assert_eq!(pick("c"), HypervisorKind::Qemu);
        assert_eq!(pick("d"), HypervisorKind::Vmlab);
        // With no host config, everything is QEMU.
        let none = HypervisorDefaults::default();
        assert_eq!(
            none.effective(file.lab.machine("b").unwrap()),
            HypervisorKind::Qemu
        );
        assert_eq!(
            none.effective(file.lab.machine("d").unwrap()),
            HypervisorKind::Qemu
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
        let file = lab(&format!("lab \"l\" {{ vm \"v\" {{ {body} }} }}"));
        let m = file.lab.machine("v").unwrap();
        let crate::config::model::MachineCfg::Vm(vm) = m else {
            unreachable!()
        };
        let hw =
            crate::qemu::resolve_vm(vm, None, &crate::profiles::ProfileSet::shipped().unwrap())
                .unwrap();
        vmlab_refusals(m, &kvm_host(), Some(&hw))
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

    /// Everything else names the phase that brings it.
    #[test]
    fn what_a_later_phase_brings_is_refused_naming_that_phase() {
        let base = r#"template = "scratch" arch = "x86_64" disk = 10GiB"#;
        let cases = [
            (
                r#"profile = "linux-generic" tsc = "host""#,
                "phase after 2f",
            ),
            (r#"profile = "macos""#, "phase macOS"),
            (r#"profile = "windows-legacy""#, "phase 4"),
        ];
        for (body, phase) in cases {
            let why = refused(&format!("{base} {body}"));
            assert!(
                why.iter()
                    .any(|w| w.contains(phase) && w.contains("vm \"v\"")),
                "{body}: expected {phase} in {why:?}"
            );
        }
        // macOS and the legacy PC are one reason each, not a list of their
        // hardware.
        assert_eq!(refused(&format!(r#"{base} profile = "macos""#)).len(), 1);
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
        let file = lab(r#"lab "l" { vm "v" { template = "x86_64/t" floppy = "./f" } }"#);
        let why = vmlab_refusals(file.lab.machine("v").unwrap(), &kvm_host(), None);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("phase 4"), "{}", why[0]);

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
}
