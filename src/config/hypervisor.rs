//! Which hypervisor runs a machine (PRD §22, ADR-0021), and what the second
//! one cannot run yet.
//!
//! A machine names its hypervisor with `hypervisor = "qemu" | "vmlab"`; one
//! that names none takes an explicit host default, its existing hypervisor,
//! or automatic VM selection. Containers still default to QEMU.
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
    pub container: HypervisorKind,
    /// `None` selects vmlab-hv for supported new VMs, QEMU otherwise.
    pub vm: Option<HypervisorKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HypervisorSelection {
    pub kind: HypervisorKind,
    pub fallback_reason: Option<String>,
}

impl HypervisorDefaults {
    /// Explicit settings win over persisted identity. Only automatic VM
    /// selection falls back; an explicit vmlab selection is still validated.
    pub fn select(
        &self,
        machine: MachineCfg<'_>,
        host: &HostFacts,
        hardware: Option<&ResolvedVm>,
        existing: Option<HypervisorKind>,
    ) -> HypervisorSelection {
        let explicit = match machine {
            MachineCfg::Vm(vm) => vm.hypervisor.or(self.vm),
            MachineCfg::Container(c) => Some(c.hypervisor.unwrap_or(self.container)),
        };
        if let Some(kind) = explicit.or(existing) {
            return HypervisorSelection {
                kind,
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

/// The macos profile's QEMU CPU string: vmlab-hv's `Skylake-Client-v4` is
/// this model with these edits, pinned from a capture (HV macOS phase).
pub const MACOS_CPU_MODEL: &str =
    "Skylake-Client-v4,-hle,-rtm,kvm=on,vendor=GenuineIntel,vmware-cpuid-freq=on";

/// Whether `h` is a macOS guest: the macos profile, or an Apple SMC.
pub fn is_macos(h: &ResolvedVm) -> bool {
    h.profile.as_deref() == Some("macos") || h.apple_smc
}

/// What the resolved hardware asks for that vmlab-hv cannot present yet.
fn hardware_refusals(name: &str, h: &ResolvedVm, why: &mut Vec<String>) {
    use crate::profiles::AgentTransport;
    // A macOS guest brings the hardware vmlab-hv's macOS phase delivered:
    // VMware SVGA, a USB keyboard and the one CPU model it pins. The rest of
    // the list below still applies to it.
    let macos = is_macos(h);
    if macos && h.cpu_model.as_deref() != Some(MACOS_CPU_MODEL) {
        why.push(format!(
            "vm \"{name}\": a macOS guest runs on vmlab-hv with the macos profile's cpu_model only, \
             not `{}` (PRD §22); remove `cpu_model`, or use hypervisor = \"qemu\"",
            h.cpu_model.as_deref().unwrap_or("the host CPU")
        ));
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
        Some("vmware-svga") if macos => {}
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
    if h.input == crate::config::model::InputDevices::Usb && !macos {
        why.push(phase(name, "input = \"usb\" (a USB keyboard)", "after 2b"));
    }
    if let Some(model) = h.cpu_model.as_ref().filter(|_| !macos) {
        why.push(format!(
            "vm \"{name}\": cpu_model = \"{model}\" — vmlab-hv runs the host CPU only \
             (PRD §22); remove it, or use hypervisor = \"qemu\""
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
        // Containers keep QEMU; supported new VMs select vmlab-hv.
        let none = HypervisorDefaults::default();
        assert_eq!(
            none.select(file.lab.machine("b").unwrap(), &kvm_host(), None, None)
                .kind,
            HypervisorKind::Qemu
        );
        assert_eq!(
            none.select(file.lab.machine("d").unwrap(), &kvm_host(), None, None)
                .kind,
            HypervisorKind::Vmlab
        );
    }

    #[test]
    fn automatic_vm_selection_falls_back_and_preserves_explicit_and_existing_choices() {
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
        for kind in [HypervisorKind::Qemu, HypervisorKind::Vmlab] {
            assert_eq!(
                defaults
                    .select(modern, &kvm_host(), Some(&hardware), Some(kind))
                    .kind,
                kind
            );
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
            r#"profile = "windows-legacy""#,
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
        let cases = [(r#"profile = "windows-legacy""#, "phase 4")];
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
        let why = refused(&format!(r#"{base} profile = "macos" cpu_model = "Haswell""#));
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
