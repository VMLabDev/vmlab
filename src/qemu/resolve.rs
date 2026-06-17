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
    pub nested: bool,
    pub gpu: Option<Gpu>,
    pub qemu_args: Vec<String>,
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

fn display_device_name(d: &str) -> String {
    match d {
        "qxl" => "qxl-vga".to_string(),
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
    let profile_name = lab_vm
        .profile
        .clone()
        .or_else(|| template.and_then(|t| t.profile.clone()));
    let default_profile = Profile {
        agent_channel: true,
        ..Profile::default()
    };
    let profile = match &profile_name {
        Some(name) => profiles
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown profile \"{name}\""))?,
        None => &default_profile,
    };

    let arch = match &lab_vm.template {
        TemplateRef::Scratch | TemplateRef::Registry { .. } => lab_vm
            .arch
            .clone()
            .ok_or_else(|| anyhow::anyhow!("vm \"{}\" needs an explicit arch", lab_vm.name))?,
        TemplateRef::Store { arch, .. } => arch.clone(),
    };

    let machine = if arch == "x86_64" {
        profile
            .machine
            .unwrap_or(Machine::Q35)
            .qemu_name()
            .to_string()
    } else {
        // Non-x86 system emulators use the generic virtual platform.
        "virt".to_string()
    };

    let firmware = lab_vm
        .firmware
        .map(firmware_kind)
        .or_else(|| template.and_then(|t| t.firmware.as_deref().and_then(meta_firmware)))
        .or(profile.firmware);

    let display_device = lab_vm
        .display
        .clone()
        .or_else(|| template.and_then(|t| t.display.clone()))
        .or_else(|| profile.display.clone())
        .map(|d| display_device_name(&d));

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
        firmware,
        secure_boot: lab_vm
            .secure_boot
            .or(template.and_then(|t| t.secure_boot))
            .or(profile.secure_boot)
            .unwrap_or(false),
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
            sha256: None,
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
            "vm \"a\" { template = \"scratch\" arch = \"x86_64\" profile = \"windows-legacy\" disk = \"10G\" }",
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
            "vm \"a\" { template = \"scratch\" arch = \"aarch64\" profile = \"linux-modern\" disk = \"10G\" }",
        );
        let r = resolve_vm(&v, None, &profiles).unwrap();
        assert_eq!(r.machine, "virt");
    }

    #[test]
    fn vm_firmware_override_beats_profile() {
        let profiles = ProfileSet::shipped().unwrap();
        let v = vm(
            "vm \"a\" { template = \"scratch\" arch = \"x86_64\" profile = \"windows-11\" disk = \"10G\" firmware = \"seabios\" secure_boot = false }",
        );
        let r = resolve_vm(&v, None, &profiles).unwrap();
        assert_eq!(r.firmware, Some(FirmwareKind::Seabios));
        assert!(!r.secure_boot);
    }
}
