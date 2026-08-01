//! Hardware inheritance (PRD §5.2): VM block > template > profile. The
//! profile's defaults are the floor; `scratch` VMs have no template layer
//! (§6.5).

use crate::config::model::{Firmware, Gpu, TemplateRef, Vm};
use crate::profiles::{DiskBus, FirmwareKind, InputTransport, Machine, Profile, ProfileSet};
use crate::template::TemplateMeta;

/// Fully resolved hardware for one VM — input to the cmdline builder.
#[derive(Debug, Clone)]
pub struct ResolvedVm {
    pub name: String,
    /// Effective profile name (vm.profile > template.profile), if any —
    /// consumers like SMB mount-command selection key off it.
    pub profile: Option<String>,
    pub arch: String,
    pub cpus: u32,
    /// Bytes.
    pub memory: u64,
    pub machine: String,
    pub firmware: Option<FirmwareKind>,
    pub secure_boot: bool,
    pub tpm: bool,
    pub disk_bus: DiskBus,
    pub nic_model: String,
    /// VGA/display device QEMU name (None = profile said nothing and the
    /// gpu block supplies the display device instead).
    pub display_device: Option<String>,
    pub agent_channel: bool,
    /// How scripted input reaches the guest (QMP vs VNC).
    pub input_transport: InputTransport,
    /// The guest mounts virtiofs natively (profile capability) — with a
    /// host virtiofsd, `transport = "auto"` shares attach as vhost-user-fs
    /// devices instead of SMB (§7.5).
    pub virtiofs: bool,
    /// The `nested` flag from config. No cmdline consumer: `-cpu host`
    /// already exposes VMX/SVM (see cmdline.rs §5.2), so nested virt needs
    /// no extra QEMU argument; carried for a future non-host CPU model.
    pub nested: bool,
    pub gpu: Option<Gpu>,
    pub qemu_args: Vec<String>,
}

/// Which layer of the §5.2 precedence chain supplied a resolved value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Vm,
    Template,
    Profile,
    /// Nothing declared it — vmlab's own fallback applied.
    Default,
}

/// Firmware and secure boot, each tagged with the layer it came from. The
/// two resolve together because they only mean anything together: secure
/// boot is a property of the UEFI build, and a caller reporting a conflict
/// between them has to name where each side was inherited from — either may
/// have come from a layer the lab author never looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwareChoice {
    pub firmware: Option<FirmwareKind>,
    pub firmware_layer: Layer,
    pub secure_boot: bool,
    pub secure_boot_layer: Layer,
}

impl FirmwareChoice {
    /// Secure boot was asked for but the resolved firmware cannot deliver
    /// it. SeaBIOS has no secure boot at all, and unset firmware means
    /// SeaBIOS on x86 — the QEMU default the cmdline builder relies on.
    pub fn secure_boot_unsupported(&self) -> bool {
        self.secure_boot && self.firmware != Some(FirmwareKind::Ovmf)
    }

    /// Why the combination cannot work, naming the layer each side came
    /// from — either may have been inherited from somewhere the lab author
    /// never looked. One wording, whether validation reports it against a
    /// source span or the resolver refuses to boot on it.
    pub fn conflict_message(&self, machine: &str, profile_name: Option<&str>) -> String {
        let from_secure_boot = self.secure_boot_layer.label(profile_name);
        match self.firmware {
            Some(_) => format!(
                "vm \"{machine}\": secure_boot = true (from {from_secure_boot}) but firmware = \
                 \"seabios\" (from {}) — secure boot needs UEFI, so it would be ignored silently \
                 (PRD §5.2)",
                self.firmware_layer.label(profile_name),
            ),
            None => format!(
                "vm \"{machine}\": secure_boot = true (from {from_secure_boot}) but no firmware \
                 is set, so the VM boots SeaBIOS (the QEMU default on x86) — secure boot needs \
                 UEFI, set `firmware = \"ovmf\"` (PRD §5.2)"
            ),
        }
    }
}

impl Layer {
    /// How this layer reads in an error message.
    pub fn label(self, profile_name: Option<&str>) -> String {
        match self {
            Layer::Vm => "the vm block".to_string(),
            Layer::Template => "the template".to_string(),
            Layer::Profile => match profile_name {
                Some(name) => format!("profile \"{name}\""),
                None => "the profile".to_string(),
            },
            Layer::Default => "vmlab's default".to_string(),
        }
    }
}

/// First layer that declared a value, and which layer that was.
fn pick<T>(vm: Option<T>, template: Option<T>, profile: Option<T>) -> Option<(T, Layer)> {
    vm.map(|v| (v, Layer::Vm))
        .or_else(|| template.map(|v| (v, Layer::Template)))
        .or_else(|| profile.map(|v| (v, Layer::Profile)))
}

/// The effective profile name: the VM's own, else the one its template
/// recorded (§5.2).
pub fn effective_profile_name(lab_vm: &Vm, template: Option<&TemplateMeta>) -> Option<String> {
    lab_vm
        .profile
        .clone()
        .or_else(|| template.and_then(|t| t.profile.clone()))
}

/// The arch a VM runs at: a store template names it in its reference,
/// every other kind has to declare it (§6.4/§6.5). `None` is a validation
/// error elsewhere.
pub fn vm_arch(lab_vm: &Vm) -> Option<String> {
    match &lab_vm.template {
        TemplateRef::Scratch | TemplateRef::Registry { .. } => lab_vm.arch.clone(),
        TemplateRef::Store { arch, .. } => Some(arch.clone()),
    }
}

/// The floor for a VM that names no profile: `custom`'s "assume nothing"
/// (§5.3), except that the agent channel is vmlab's own, not the guest's.
pub fn default_profile() -> Profile {
    Profile {
        agent_channel: true,
        ..Profile::default()
    }
}

/// Resolve firmware and secure boot for one VM. Split out of
/// [`resolve_vm`] so validation can reach the same chain — and the layers
/// behind it — without a template store or a full resolve (§5.1).
pub fn resolve_firmware(
    lab_vm: &Vm,
    template: Option<&TemplateMeta>,
    profile: &Profile,
    arch: &str,
) -> FirmwareChoice {
    let (firmware, firmware_layer) = match pick(
        lab_vm.firmware.map(firmware_kind),
        template.and_then(|t| t.firmware.as_deref().and_then(meta_firmware)),
        profile.firmware,
    ) {
        Some((f, layer)) => (Some(f), layer),
        // The `virt` machine (non-x86) has no SeaBIOS fallback, so a guest
        // that named no firmware would be unbootable — default it to UEFI.
        None => (
            (arch != "x86_64").then_some(FirmwareKind::Ovmf),
            Layer::Default,
        ),
    };
    let (secure_boot, secure_boot_layer) = pick(
        lab_vm.secure_boot,
        template.and_then(|t| t.secure_boot),
        profile.secure_boot,
    )
    .unwrap_or((false, Layer::Default));
    FirmwareChoice {
        firmware,
        firmware_layer,
        secure_boot,
        secure_boot_layer,
    }
}

fn firmware_kind(f: Firmware) -> FirmwareKind {
    match f {
        Firmware::Ovmf => FirmwareKind::Ovmf,
        Firmware::Seabios => FirmwareKind::Seabios,
    }
}

fn meta_firmware(s: &str) -> Option<FirmwareKind> {
    match s {
        "ovmf" => Some(FirmwareKind::Ovmf),
        "seabios" => Some(FirmwareKind::Seabios),
        _ => None,
    }
}

fn display_device_name(d: &str, arch: &str) -> String {
    let x86 = arch == "x86_64" || arch == "x86";
    match d {
        "qxl" => "qxl-vga".to_string(),
        // VGA-compatible virtio GPU: a real WDDM/DRM device the in-guest virtio
        // driver binds, while still exposing a legacy VGA framebuffer so the VNC
        // console / build OCR work before that driver loads. Preferred default
        // for guests that have a virtio GPU driver (Windows' modern shell
        // fail-fasts on the Basic Display Adapter). The non-x86 `virt` machine
        // has no legacy VGA, so virtio-vga is unavailable there — fall back to
        // the pure virtio GPU, which the same driver binds.
        "virtio-vga" if !x86 => "virtio-gpu-pci".to_string(),
        "virtio-vga" => "virtio-vga".to_string(),
        // Pure virtio GPU, no VGA compatibility.
        "virtio-gpu" => "virtio-gpu-pci".to_string(),
        "std" => "VGA".to_string(),
        other => other.to_string(), // power users may name a QEMU device directly
    }
}

/// Resolve a VM's effective hardware. `template` is the store metadata for
/// its backing template (None for scratch). The effective profile comes
/// from vm.profile > template.profile; an unknown name is a validation
/// error long before this runs.
pub fn resolve_vm(
    lab_vm: &Vm,
    template: Option<&TemplateMeta>,
    profiles: &ProfileSet,
) -> anyhow::Result<ResolvedVm> {
    let profile_name = effective_profile_name(lab_vm, template);
    let default_profile = default_profile();
    let profile = match &profile_name {
        Some(name) => profiles
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown profile \"{name}\""))?,
        None => &default_profile,
    };

    let arch = vm_arch(lab_vm)
        .ok_or_else(|| anyhow::anyhow!("vm \"{}\" needs an explicit arch", lab_vm.name))?;

    let machine = if arch == "x86_64" || arch == "x86" {
        // `x86` is a display-only alias that runs on the x86_64 emulator, so it
        // uses the same i440fx/q35 machines from the profile.
        profile
            .machine
            .unwrap_or(Machine::Q35)
            .qemu_name()
            .to_string()
    } else if arch == "riscv64" {
        // RISC-V Linux guests currently boot via device-tree; ACPI consumer
        // support is still WIP, so pin acpi=off on the generic virt platform.
        "virt,acpi=off".to_string()
    } else {
        // Other non-x86 system emulators use the generic virtual platform.
        "virt".to_string()
    };

    let firmware_choice = resolve_firmware(lab_vm, template, profile, &arch);
    // Validation rejects this combination against the source span, but it
    // cannot always see the template layer — a registry template is not
    // pulled at validate time. By the time a machine resolves, every layer
    // is in hand: refuse rather than boot a VM whose secure boot would be
    // dropped on the floor (§5.2).
    if firmware_choice.secure_boot_unsupported() {
        anyhow::bail!(firmware_choice.conflict_message(&lab_vm.name, profile_name.as_deref()));
    }

    let display_device = lab_vm
        .display
        .clone()
        .or_else(|| template.and_then(|t| t.display.clone()))
        .or_else(|| profile.display.clone())
        .map(|d| display_device_name(&d, &arch));

    Ok(ResolvedVm {
        name: lab_vm.name.clone(),
        profile: profile_name,
        arch,
        cpus: lab_vm
            .cpus
            .or(template.and_then(|t| t.cpus))
            .or(profile.cpus)
            .unwrap_or(2),
        memory: lab_vm
            .memory
            .or(template.and_then(|t| t.memory))
            .or(profile.memory)
            .unwrap_or(2 << 30),
        machine,
        firmware: firmware_choice.firmware,
        secure_boot: firmware_choice.secure_boot,
        tpm: lab_vm
            .tpm
            .or(template.and_then(|t| t.tpm))
            .or(profile.tpm)
            .unwrap_or(false),
        disk_bus: profile.disk_bus.unwrap_or(DiskBus::Virtio),
        nic_model: profile
            .nic_model
            .clone()
            .unwrap_or_else(|| "virtio-net-pci".to_string()),
        display_device,
        agent_channel: profile.agent_channel,
        input_transport: profile.input_transport,
        virtiofs: profile.virtiofs,
        nested: lab_vm.nested,
        gpu: lab_vm.gpu.clone(),
        qemu_args: lab_vm.qemu_args.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_lab_source;
    use std::path::Path;

    fn vm(src: &str) -> Vm {
        let full = format!("import <vmlab.wcl>\nlab \"t\" {{\n{src}\n}}\n");
        let lf = load_lab_source(&full, "<test>", Path::new("/tmp")).unwrap();
        lf.lab.vms.into_iter().next().unwrap()
    }

    fn meta() -> TemplateMeta {
        TemplateMeta {
            name: "win".into(),
            arch: "x86_64".into(),
            version: "1".into(),
            profile: Some("windows-11".into()),
            cpus: Some(8),
            memory: Some(16 << 30),
            disk: None,
            firmware: None,
            tpm: None,
            secure_boot: None,
            display: None,
            created: chrono::Utc::now(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
            agent_version: None,
        }
    }

    #[test]
    fn precedence_vm_over_template_over_profile() {
        let profiles = ProfileSet::shipped().unwrap();
        let v = vm("vm \"a\" { template = \"x86_64/win\" cpus = 2 }");
        let m = meta();
        let r = resolve_vm(&v, Some(&m), &profiles).unwrap();
        // cpus: VM block wins.
        assert_eq!(r.cpus, 2);
        // memory: template wins over the windows-11 profile's 8G.
        assert_eq!(r.memory, 16 << 30);
        // tpm/secure_boot: profile floor (windows-11 → true).
        assert!(r.tpm);
        assert!(r.secure_boot);
        assert_eq!(r.machine, "q35");
        assert_eq!(r.firmware, Some(FirmwareKind::Ovmf));
    }

    #[test]
    fn scratch_uses_profile_floor() {
        let profiles = ProfileSet::shipped().unwrap();
        let v = vm(
            "vm \"a\" { template = \"scratch\" arch = \"x86_64\" profile = \"windows-legacy\" disk = 10GiB }",
        );
        let r = resolve_vm(&v, None, &profiles).unwrap();
        assert_eq!(r.machine, "pc");
        assert_eq!(r.firmware, Some(FirmwareKind::Seabios));
        assert_eq!(r.disk_bus, DiskBus::Ide);
        assert_eq!(r.nic_model, "e1000");
        assert!(!r.tpm);
    }

    #[test]
    fn aarch64_uses_virt_machine() {
        let profiles = ProfileSet::shipped().unwrap();
        let v = vm(
            "vm \"a\" { template = \"scratch\" arch = \"aarch64\" profile = \"linux-modern\" disk = 10GiB }",
        );
        let r = resolve_vm(&v, None, &profiles).unwrap();
        assert_eq!(r.machine, "virt");
    }

    #[test]
    fn riscv64_uses_virt_machine() {
        let profiles = ProfileSet::shipped().unwrap();
        let v = vm(
            "vm \"a\" { template = \"scratch\" arch = \"riscv64\" profile = \"linux-modern\" disk = 10GiB }",
        );
        let r = resolve_vm(&v, None, &profiles).unwrap();
        assert_eq!(r.machine, "virt,acpi=off");
    }

    #[test]
    fn firmware_choice_reports_the_layer_each_value_came_from() {
        let profiles = ProfileSet::shipped().unwrap();
        let legacy = profiles.get("windows-legacy").unwrap();

        // Profile floor on both sides.
        let v = vm("vm \"a\" { template = \"x86_64/t\" }");
        let c = resolve_firmware(&v, None, legacy, "x86_64");
        assert_eq!(c.firmware, Some(FirmwareKind::Seabios));
        assert_eq!(c.firmware_layer, Layer::Profile);
        assert!(!c.secure_boot);
        assert_eq!(c.secure_boot_layer, Layer::Profile);
        assert!(!c.secure_boot_unsupported());

        // VM block over template over profile.
        let v = vm("vm \"a\" { template = \"x86_64/t\" secure_boot = true }");
        let mut m = meta();
        m.firmware = Some("ovmf".into());
        let c = resolve_firmware(&v, Some(&m), legacy, "x86_64");
        assert_eq!(c.firmware, Some(FirmwareKind::Ovmf));
        assert_eq!(c.firmware_layer, Layer::Template);
        assert!(c.secure_boot);
        assert_eq!(c.secure_boot_layer, Layer::Vm);
        assert!(!c.secure_boot_unsupported());
    }

    #[test]
    fn secure_boot_is_unsupported_without_uefi() {
        let profiles = ProfileSet::shipped().unwrap();
        let legacy = profiles.get("windows-legacy").unwrap();
        let custom = profiles.get("custom").unwrap();

        let v = vm("vm \"a\" { template = \"x86_64/t\" secure_boot = true }");
        assert!(resolve_firmware(&v, None, legacy, "x86_64").secure_boot_unsupported());

        // Nothing named a firmware and x86 has no UEFI default: SeaBIOS.
        let c = resolve_firmware(&v, None, custom, "x86_64");
        assert_eq!(c.firmware, None);
        assert_eq!(c.firmware_layer, Layer::Default);
        assert!(c.secure_boot_unsupported());

        // Non-x86 defaults to UEFI, so the same VM is fine there.
        assert!(!resolve_firmware(&v, None, custom, "aarch64").secure_boot_unsupported());
    }

    /// Validation cannot see a registry template's hardware — it is not
    /// pulled at validate time — so the resolver is the backstop: by the
    /// time a machine resolves, every layer is in hand.
    #[test]
    fn resolve_refuses_secure_boot_without_uefi() {
        let profiles = ProfileSet::shipped().unwrap();
        let v = vm(
            "vm \"a\" { template = \"ghcr.io/acme/win:1\" arch = \"x86_64\" secure_boot = true }",
        );
        let mut m = meta();
        m.profile = Some("windows-legacy".into());
        let err = resolve_vm(&v, Some(&m), &profiles)
            .expect_err("secure boot on SeaBIOS must not resolve")
            .to_string();
        assert!(err.contains("secure boot needs UEFI"), "{err}");
        assert!(err.contains("profile \"windows-legacy\""), "{err}");

        // The same VM on a UEFI profile resolves as before.
        m.profile = Some("windows-11".into());
        let r = resolve_vm(&v, Some(&m), &profiles).expect("UEFI secure boot resolves");
        assert!(r.secure_boot);
        assert_eq!(r.firmware, Some(FirmwareKind::Ovmf));
    }

    #[test]
    fn vm_firmware_override_beats_profile() {
        let profiles = ProfileSet::shipped().unwrap();
        let v = vm(
            "vm \"a\" { template = \"scratch\" arch = \"x86_64\" profile = \"windows-11\" disk = 10GiB firmware = \"seabios\" secure_boot = false }",
        );
        let r = resolve_vm(&v, None, &profiles).unwrap();
        assert_eq!(r.firmware, Some(FirmwareKind::Seabios));
        assert!(!r.secure_boot);
    }
}
