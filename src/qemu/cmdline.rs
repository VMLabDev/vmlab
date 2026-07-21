//! Build the QEMU argv for one VM (PRD §3, §5.2). Pure function of the
//! resolved hardware + runtime paths, so it is exhaustively unit-testable.
//! `qemu_args` from the lab file are appended verbatim, last, so they win.

use std::path::{Path, PathBuf};

use anyhow::Result;
use vmlab_agent_proto::PORT_NAME as AGENT_PORT_NAME;

use super::firmware;
use super::resolve::ResolvedVm;
use crate::config::model::{GpuMode, MacAddr};
use crate::profiles::{DiskBus, FirmwareKind};

/// Per-VM runtime paths and attachments supplied by the lab daemon.
#[derive(Debug, Clone, Default)]
pub struct VmPaths {
    pub qmp_sock: PathBuf,
    /// vmlab-agent channel (`vmlab.agent.0`): interactive terminals,
    /// streaming exec, file transfer, metrics — see `guest/agent-proto`.
    pub agent_sock: PathBuf,
    pub vnc_sock: PathBuf,
    /// Primary disk qcow2 (linked clone or blank).
    pub primary_disk: PathBuf,
    /// Additional disks in declaration order: (name, path).
    pub extra_disks: Vec<(String, PathBuf)>,
    /// CD-ROM attachments (paths to ISOs, including built media).
    pub cdroms: Vec<PathBuf>,
    /// Floppy attachment.
    pub floppy: Option<PathBuf>,
    /// NIC attachments in declaration order.
    pub nics: Vec<NicSpec>,
    /// Writable OVMF VARS copy for this VM (created by the lab daemon from
    /// the template in `firmware::UefiFirmware::vars_template`).
    pub ovmf_vars: Option<PathBuf>,
    /// swtpm control socket (when tpm enabled).
    pub tpm_sock: Option<PathBuf>,
    /// Serial console log file.
    pub serial_log: Option<PathBuf>,
    /// virtiofs shares in declaration order: (mount tag, virtiofsd socket).
    /// The daemons must be listening before QEMU spawns; non-empty switches
    /// the RAM to a shared memory-backend-memfd (§7.5).
    pub virtiofs_shares: Vec<(String, PathBuf)>,
}

/// One NIC attachment for the argv builder.
#[derive(Debug, Clone)]
pub struct NicSpec {
    pub mac: MacAddr,
    /// Segment MTU; drives `host_mtu=` on virtio NICs so the guest can use
    /// a jumbo segment. `None`/1500 leaves the link at the default.
    pub mtu: Option<u16>,
    pub backend: NicBackend,
}

/// How QEMU reaches the segment for one NIC.
#[derive(Debug, Clone)]
pub enum NicBackend {
    /// Stream-socket netdev into the userspace switch (§9.1); the daemon
    /// listens, QEMU connects.
    Stream { sock: PathBuf },
    /// Pre-opened tap fd inherited at this descriptor number (the afxdp
    /// fast path; the daemon maps the fd in at spawn).
    Tap { child_fd: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    Kvm,
    Tcg,
}

/// The QEMU system emulator arch for a template arch. `x86` (32-bit Windows
/// and friends) is a display-only alias: it runs on the x86_64 emulator/machine
/// (a 64-bit QEMU runs 32-bit guests), so it shows as `x86` in the store/list
/// but behaves like x86_64 everywhere below.
pub fn qemu_arch(arch: &str) -> &str {
    if arch == "x86" { "x86_64" } else { arch }
}

/// Pick the accelerator: KVM when /dev/kvm is usable and the target arch is
/// the host arch; TCG otherwise — slow but functional, warn loudly (PRD §14).
pub fn pick_accel(arch: &str) -> Accel {
    let host = std::env::consts::ARCH;
    if kvm_available() && qemu_arch(arch) == host {
        Accel::Kvm
    } else {
        Accel::Tcg
    }
}

/// Whether this process can open KVM with the read/write access QEMU needs.
/// This catches container/device permission failures that a path check misses.
pub fn kvm_available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(Path::new("/dev/kvm"))
        .is_ok()
}

pub fn emulator_binary(arch: &str) -> String {
    format!("qemu-system-{}", qemu_arch(arch))
}

/// Build the full argv (excluding argv[0], which is `emulator_binary`).
pub fn build_args(
    lab: &str,
    vm: &ResolvedVm,
    paths: &VmPaths,
    accel: Accel,
) -> Result<Vec<String>> {
    fn arg(a: &mut Vec<String>, s: &str, v: String) {
        a.push(format!("-{s}"));
        a.push(v);
    }
    let mut a: Vec<String> = Vec::new();

    arg(&mut a, "name", format!("vmlab:{lab}/{}", vm.name));
    // vhost-user-fs shares need the guest RAM in shared memory. Only VMs
    // that carry one get the memfd backend: it renames the RAM block in
    // internal snapshots (pc.ram → mem0), so the plain `-m` shape stays
    // untouched for every other VM (§7.5).
    if paths.virtiofs_shares.is_empty() {
        arg(&mut a, "machine", vm.machine.clone());
    } else {
        arg(
            &mut a,
            "machine",
            format!("{},memory-backend=mem0", vm.machine),
        );
        arg(
            &mut a,
            "object",
            format!(
                "memory-backend-memfd,id=mem0,size={}M,share=on",
                vm.memory >> 20
            ),
        );
    }
    match accel {
        Accel::Kvm => {
            arg(&mut a, "accel", "kvm".into());
            // `host` exposes everything incl. VMX/SVM for nested (§5.2).
            arg(&mut a, "cpu", "host".into());
        }
        Accel::Tcg => {
            arg(&mut a, "accel", "tcg".into());
            arg(&mut a, "cpu", "max".into());
        }
    }
    arg(&mut a, "smp", vm.cpus.to_string());
    arg(&mut a, "m", format!("{}M", vm.memory >> 20));

    // Always headless, with a VNC display on a unix socket (§11). A
    // `gui = true` window is a *separate* VNC viewer the CLI launches on
    // `up` (see cli::console) — never QEMU's own GTK window, whose close
    // would quit QEMU and kill the VM. Decoupling the window from the VM
    // lets the user close the viewer and reattach with `vmlab console`.
    arg(&mut a, "display", "none".into());
    // share=ignore so several clients coexist: a human viewer
    // (`vmlab console`), screenshots, and the VNC input transport (§10.3)
    // can all attach at once, and none can kick the others. QEMU's default
    // (allow-exclusive) lets a non-shared viewer drop everyone else;
    // force-shared swings the other way and *drops* any client that asks
    // for exclusive — which gtk-vnc viewers (gvncviewer/remote-viewer) do by
    // default, so they'd connect and immediately get disconnected ("waiting
    // for display…"). `ignore` simply ignores the shared flag and keeps
    // every client attached.
    arg(
        &mut a,
        "vnc",
        format!("unix:{},share=ignore", paths.vnc_sock.display()),
    );

    arg(
        &mut a,
        "qmp",
        format!("unix:{},server=on,wait=off", paths.qmp_sock.display()),
    );
    a.push("-monitor".into());
    a.push("none".into());

    if let Some(log) = &paths.serial_log {
        arg(&mut a, "serial", format!("file:{}", log.display()));
    }

    // UEFI firmware: CODE read-only pflash + per-VM writable VARS.
    if vm.firmware == Some(FirmwareKind::Ovmf) {
        let fw = match qemu_arch(vm.arch.as_str()) {
            "x86_64" => firmware::ovmf_x86_64(vm.secure_boot)?,
            "aarch64" => firmware::uefi_aarch64()?,
            "riscv64" => firmware::uefi_riscv64()?,
            other => anyhow::bail!("no UEFI firmware lookup for arch {other}"),
        };
        arg(
            &mut a,
            "drive",
            format!(
                "if=pflash,format=raw,readonly=on,file={}",
                fw.code.display()
            ),
        );
        let vars = paths
            .ovmf_vars
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OVMF requires a per-VM VARS path"))?;
        arg(
            &mut a,
            "drive",
            format!("if=pflash,format=raw,file={}", vars.display()),
        );
        if vm.secure_boot && qemu_arch(vm.arch.as_str()) == "x86_64" {
            // SMM is required for the secboot build to enforce anything.
            a.push("-global".into());
            a.push("driver=cfi.pflash01,property=secure,value=on".into());
        }
    }
    // SeaBIOS is the QEMU default on x86 — nothing to add.

    // Guest agent virtio-serial channel (§7.4): the vmlab-agent port
    // (interactive terminals / exec / files / net-info / shutdown —
    // guest/agent-proto). QEMU listens, the daemon connects.
    if vm.agent_channel {
        arg(
            &mut a,
            "chardev",
            format!(
                "socket,id=vagent0,path={},server=on,wait=off",
                paths.agent_sock.display()
            ),
        );
        arg(&mut a, "device", "virtio-serial-pci".into());
        arg(
            &mut a,
            "device",
            format!("virtserialport,chardev=vagent0,name={}", AGENT_PORT_NAME),
        );
    }

    // Primary + extra disks: explicit blockdev node names (disk0, disk1, …)
    // so QMP snapshot commands can address them (§7.3).
    // SeaBIOS honours an explicit boot order; OVMF/UEFI boots removable media
    // on its own. Without it, SeaBIOS tries the (blank) disk and any floppy
    // before the install CD and stalls. So under SeaBIOS we give CD-ROMs the
    // lowest bootindex (install media boots first), disks the next, and leave
    // the answer-file floppy unindexed (never a boot candidate).
    let seabios = vm.firmware != Some(FirmwareKind::Ovmf);
    let mut disk_index = 0usize;
    let mut add_disk = |a: &mut Vec<String>, path: &Path, bus: DiskBus| {
        let node = format!("disk{disk_index}");
        a.push("-blockdev".into());
        a.push(format!(
            "driver=qcow2,node-name={node},file.driver=file,file.filename={}",
            path.display()
        ));
        a.push("-device".into());
        let boot = if seabios {
            format!(",bootindex={}", 50 + disk_index)
        } else {
            String::new()
        };
        match bus {
            DiskBus::Virtio => a.push(format!("virtio-blk-pci,drive={node}{boot}")),
            DiskBus::Ide | DiskBus::Sata => a.push(format!("ide-hd,drive={node}{boot}")),
        }
        disk_index += 1;
    };
    add_disk(&mut a, &paths.primary_disk, vm.disk_bus);
    for (_, path) in &paths.extra_disks {
        add_disk(&mut a, path, vm.disk_bus);
    }

    // CD-ROMs. The `virt` machine (aarch64 et al.) has no IDE/AHCI bus at
    // all, so attach ISOs as read-only virtio-blk there — cloud-init's
    // NoCloud datasource finds the CIDATA seed by filesystem label on any
    // block device, regardless of bus. On x86 they ride IDE/SATA, which is
    // universally bootable. q35's AHCI ports hold a single unit each and
    // QEMU's auto-placement does not advance past the first port, so address
    // ports explicitly there, skipping any ports the IDE/SATA disks above
    // already claimed. Legacy `pc` IDE buses take two units and auto-place.
    let virt = vm.machine.starts_with("virt");
    let sata_disks = match vm.disk_bus {
        DiskBus::Virtio => 0,
        DiskBus::Ide | DiskBus::Sata => disk_index,
    };
    let ahci = vm.machine.starts_with("q35");
    for (i, iso) in paths.cdroms.iter().enumerate() {
        a.push("-blockdev".into());
        a.push(format!(
            "driver=raw,node-name=cd{i},read-only=on,file.driver=file,file.filename={}",
            iso.display()
        ));
        a.push("-device".into());
        let boot = if seabios {
            format!(",bootindex={i}")
        } else {
            String::new()
        };
        if virt {
            a.push(format!("virtio-blk-pci,drive=cd{i}{boot}"));
        } else if ahci {
            a.push(format!(
                "ide-cd,drive=cd{i},bus=ide.{}{boot}",
                sata_disks + i
            ));
        } else {
            a.push(format!("ide-cd,drive=cd{i}{boot}"));
        }
    }

    if let Some(floppy) = &paths.floppy {
        arg(
            &mut a,
            "drive",
            format!("if=floppy,format=raw,file={}", floppy.display()),
        );
    }

    // NICs (§9.1): stream-socket netdevs into the segment switch (daemon
    // listens, QEMU connects), or pre-opened tap fds on the afxdp fast path.
    for (i, nic) in paths.nics.iter().enumerate() {
        match &nic.backend {
            NicBackend::Stream { sock } => arg(
                &mut a,
                "netdev",
                format!(
                    "stream,id=net{i},server=off,addr.type=unix,addr.path={}",
                    sock.display()
                ),
            ),
            NicBackend::Tap { child_fd } => {
                arg(&mut a, "netdev", format!("tap,id=net{i},fd={child_fd}"));
            }
        }
        // virtio-net negotiates a jumbo MTU with the guest via `host_mtu`;
        // e1000/pcnet don't support it reliably, so they stay at the default.
        let host_mtu = nic
            .mtu
            .filter(|m| *m != 1500 && vm.nic_model.starts_with("virtio-net"))
            .map(|m| format!(",host_mtu={m}"))
            .unwrap_or_default();
        arg(
            &mut a,
            "device",
            format!("{},netdev=net{i},mac={}{host_mtu}", vm.nic_model, nic.mac),
        );
    }
    if paths.nics.is_empty() {
        // No nic blocks = no network hardware at all (§5.2).
        a.push("-nic".into());
        a.push("none".into());
    }

    // TPM via swtpm (§5.3).
    if let Some(tpm_sock) = &paths.tpm_sock {
        arg(
            &mut a,
            "chardev",
            format!("socket,id=chrtpm,path={}", tpm_sock.display()),
        );
        arg(&mut a, "tpmdev", "emulator,id=tpm0,chardev=chrtpm".into());
        let dev = if qemu_arch(vm.arch.as_str()) == "x86_64" {
            "tpm-tis"
        } else {
            "tpm-tis-device"
        };
        arg(&mut a, "device", format!("{dev},tpmdev=tpm0"));
    }

    // Display device / GPU modes (§5.2). Paravirt GL renders host-side with
    // the framebuffer scraped for VNC (egl-headless path).
    match &vm.gpu {
        Some(gpu) => match gpu.mode {
            GpuMode::Passthrough => {
                // §5.1 validation rejects a passthrough gpu with no address;
                // keep a real error in case a config path skips validation.
                let addr = gpu.address.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("gpu passthrough requires an `address` (PRD §5.2)")
                })?;
                arg(&mut a, "device", format!("vfio-pci,host={addr}"));
                if let Some(d) = &vm.display_device {
                    arg(&mut a, "device", d.clone());
                }
            }
            GpuMode::Virgl => {
                arg(&mut a, "device", "virtio-gpu-gl".into());
                arg(&mut a, "display", "egl-headless".into());
            }
            GpuMode::Vulkan => {
                arg(
                    &mut a,
                    "device",
                    "virtio-gpu-gl,venus=true,hostmem=4G".into(),
                );
                arg(&mut a, "display", "egl-headless".into());
            }
        },
        None => {
            if let Some(d) = &vm.display_device {
                arg(&mut a, "device", d.clone());
            }
        }
    }

    // USB tablet: absolute pointer events for screen automation (§10.3).
    // The `virt` machine has no default USB host controller, so `-usb` is a
    // no-op there and usb-tablet has nothing to plug into — add an explicit
    // xHCI controller. x86 q35/pc have a default controller via `-usb`.
    if virt {
        arg(&mut a, "device", "qemu-xhci".into());
    } else {
        a.push("-usb".into());
    }
    arg(&mut a, "device", "usb-tablet".into());

    // virtiofs shares: one vhost-user-fs device per export, tag = share
    // name — the guest agent mounts by tag once the VM is ready (§7.5).
    for (i, (tag, sock)) in paths.virtiofs_shares.iter().enumerate() {
        arg(
            &mut a,
            "chardev",
            format!("socket,id=vfs{i},path={}", sock.display()),
        );
        arg(
            &mut a,
            "device",
            format!("vhost-user-fs-pci,chardev=vfs{i},tag={tag}"),
        );
    }

    // Don't start the guest CPU until the daemon says go — lets the switch
    // ports and QMP attach race-free.
    a.push("-S".into());

    // Escape hatch, verbatim, last so it wins (§5.2).
    a.extend(vm.qemu_args.iter().cloned());

    Ok(a)
}

/// Whether this process can put a window on screen.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::ProfileSet;

    fn resolved(profile: &str, arch: &str) -> ResolvedVm {
        let profiles = ProfileSet::shipped().unwrap();
        let p = profiles.get(profile).unwrap();
        ResolvedVm {
            name: "t".into(),
            profile: Some(profile.into()),
            arch: arch.into(),
            cpus: 2,
            memory: 2 << 30,
            machine: if arch == "x86_64" {
                p.machine
                    .map(|m| m.qemu_name().to_string())
                    .unwrap_or("q35".into())
            } else if arch == "riscv64" {
                "virt,acpi=off".into()
            } else {
                "virt".into()
            },
            firmware: p.firmware,
            secure_boot: p.secure_boot.unwrap_or(false),
            tpm: p.tpm.unwrap_or(false),
            disk_bus: p.disk_bus.unwrap_or(crate::profiles::DiskBus::Virtio),
            nic_model: p.nic_model.clone().unwrap_or("virtio-net-pci".into()),
            // Mirror of resolve::display_device_name (arch-aware: virtio-vga
            // downgrades to virtio-gpu-pci off x86).
            display_device: p.display.clone().map(|d| {
                let x86 = arch == "x86_64" || arch == "x86";
                match d.as_str() {
                    "qxl" => "qxl-vga".to_string(),
                    "virtio-vga" if !x86 => "virtio-gpu-pci".to_string(),
                    "virtio-vga" => "virtio-vga".to_string(),
                    "virtio-gpu" => "virtio-gpu-pci".to_string(),
                    "std" => "VGA".to_string(),
                    o => o.to_string(),
                }
            }),
            agent_channel: true,
            input_transport: crate::profiles::InputTransport::Qmp,
            virtiofs: p.virtiofs,
            nested: false,
            gpu: None,
            qemu_args: vec![],
        }
    }

    fn paths() -> VmPaths {
        VmPaths {
            qmp_sock: "/run/l/t/qmp.sock".into(),
            agent_sock: "/run/l/t/agent.sock".into(),
            vnc_sock: "/run/l/t/vnc.sock".into(),
            primary_disk: "/lab/.vmlab/t/disk0.qcow2".into(),
            nics: vec![NicSpec {
                mac: "52:54:00:00:00:01".parse().unwrap(),
                mtu: None,
                backend: NicBackend::Stream {
                    sock: PathBuf::from("/run/l/t/nic0.sock"),
                },
            }],
            ..Default::default()
        }
    }

    fn joined(args: &[String]) -> String {
        args.join(" ")
    }

    /// virtiofs shares attach as vhost-user-fs devices and flip the RAM to
    /// a shared memfd backend; share-less VMs keep plain `-m` (RAM block
    /// name stability for existing snapshots).
    #[test]
    fn virtiofs_shares_shape() {
        let vm = resolved("linux-modern", "x86_64");
        let mut p = paths();
        p.ovmf_vars = Some("/v".into());
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("memory-backend"), "{s}");
        assert!(!s.contains("vhost-user-fs"), "{s}");

        p.virtiofs_shares = vec![("mnt_src".into(), PathBuf::from("/run/l/t/vfs0.sock"))];
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("-machine q35,memory-backend=mem0"), "{s}");
        assert!(
            s.contains("memory-backend-memfd,id=mem0,size=2048M,share=on"),
            "{s}"
        );
        assert!(s.contains("socket,id=vfs0,path=/run/l/t/vfs0.sock"), "{s}");
        assert!(
            s.contains("vhost-user-fs-pci,chardev=vfs0,tag=mnt_src"),
            "{s}"
        );
    }

    #[test]
    fn windows11_shape() {
        let mut p = paths();
        p.ovmf_vars = Some("/lab/.vmlab/t/OVMF_VARS.fd".into());
        p.tpm_sock = Some("/run/l/t/tpm.sock".into());
        let vm = resolved("windows-11", "x86_64");
        let args = build_args("lab1", &vm, &p, Accel::Kvm).unwrap();
        let s = joined(&args);
        assert!(s.contains("-machine q35"));
        assert!(s.contains("-accel kvm"));
        assert!(s.contains("-cpu host"));
        assert!(s.contains("if=pflash,format=raw,readonly=on"));
        assert!(s.contains("secboot"), "secure boot firmware expected: {s}");
        assert!(s.contains("driver=cfi.pflash01,property=secure,value=on"));
        assert!(s.contains("virtio-blk-pci,drive=disk0"));
        assert!(s.contains("tpm-tis,tpmdev=tpm0"));
        assert!(s.contains("virtio-vga"));
        assert!(
            s.contains("socket,id=vagent0,path=/run/l/t/agent.sock"),
            "{s}"
        );
        assert!(
            s.contains("virtserialport,chardev=vagent0,name=vmlab.agent.0"),
            "{s}"
        );
        assert!(s.contains("netdev=net0,mac=52:54:00:00:00:01"));
        assert!(s.contains("-vnc unix:/run/l/t/vnc.sock"));
        assert!(s.contains("usb-tablet"));
        assert!(args.last().unwrap() != "-S" || s.ends_with("-S"));
    }

    #[test]
    fn legacy_shape() {
        let vm = resolved("windows-legacy", "x86_64");
        let args = build_args("lab1", &vm, &paths(), Accel::Kvm).unwrap();
        let s = joined(&args);
        assert!(s.contains("-machine pc"));
        assert!(!s.contains("pflash"), "SeaBIOS must not add pflash: {s}");
        assert!(s.contains("ide-hd,drive=disk0"));
        assert!(s.contains("e1000,netdev=net0"));
        assert!(s.contains("-device VGA"));
        assert!(!s.contains("tpm"));
    }

    /// Vintage guests (`agent_channel = false`) get no virtio-serial bus at
    /// all — no vmlab-agent port.
    #[test]
    fn no_agent_channel_emits_no_virtio_serial() {
        let mut vm = resolved("windows-legacy", "x86_64");
        vm.agent_channel = false;
        let s = joined(&build_args("l", &vm, &paths(), Accel::Kvm).unwrap());
        assert!(!s.contains("virtserialport"), "{s}");
        assert!(!s.contains("virtio-serial-pci"), "{s}");
        assert!(!s.contains("vmlab.agent.0"), "{s}");
    }

    #[test]
    fn tap_backend_emits_fd_netdev() {
        let vm = resolved("linux-modern", "x86_64");
        let mut p = paths();
        p.ovmf_vars = Some("/v".into());
        p.nics[0].backend = NicBackend::Tap { child_fd: 10 };
        p.nics[0].mtu = Some(9000);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("-netdev tap,id=net0,fd=10"), "{s}");
        assert!(!s.contains("addr.type=unix"), "{s}");
        // The virtio device line is backend-agnostic (host_mtu still works).
        assert!(s.contains("netdev=net0,mac=52:54:00:00:00:01,host_mtu=9000"));
    }

    #[test]
    fn no_nics_means_nic_none() {
        let vm = resolved("linux-modern", "x86_64");
        let mut p = paths();
        p.nics.clear();
        p.ovmf_vars = Some("/v".into());
        let s = joined(&build_args("l", &vm, &p, Accel::Tcg).unwrap());
        assert!(s.contains("-nic none"));
        assert!(s.contains("-accel tcg"));
        assert!(s.contains("-cpu max"));
    }

    #[test]
    fn jumbo_mtu_sets_host_mtu_on_virtio_only() {
        // virtio-net guest on a jumbo segment gets host_mtu=.
        let vm = resolved("linux-modern", "x86_64");
        assert!(vm.nic_model.starts_with("virtio-net"));
        let mut p = paths();
        p.ovmf_vars = Some("/v".into());
        p.nics[0].mtu = Some(9000);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("host_mtu=9000"), "{s}");

        // 1500 is the default — no host_mtu emitted even on virtio.
        let mut p = paths();
        p.ovmf_vars = Some("/v".into());
        p.nics[0].mtu = Some(1500);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("host_mtu"), "{s}");

        // Legacy e1000 NICs never get host_mtu, even on a jumbo segment.
        let vm = resolved("windows-legacy", "x86_64");
        assert!(!vm.nic_model.starts_with("virtio-net"));
        let mut p = paths();
        p.nics[0].mtu = Some(9000);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("host_mtu"), "{s}");
    }

    #[test]
    fn qemu_args_go_last() {
        let mut vm = resolved("linux-generic", "x86_64");
        vm.qemu_args = vec!["-device".into(), "weird-thing".into()];
        let args = build_args("l", &vm, &paths(), Accel::Kvm).unwrap();
        assert_eq!(
            args[args.len() - 2..],
            ["-device".to_string(), "weird-thing".to_string()]
        );
    }

    #[test]
    fn cdroms_and_floppy_attach() {
        let vm = resolved("windows-legacy", "x86_64");
        let mut p = paths();
        p.cdroms = vec![
            "/isos/win.iso".into(),
            "/lab/.vmlab/media/unattend.iso".into(),
        ];
        p.floppy = Some("/lab/.vmlab/media/drivers.img".into());
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("ide-cd,drive=cd0"));
        assert!(s.contains("ide-cd,drive=cd1"));
        assert!(s.contains("if=floppy,format=raw,file=/lab/.vmlab/media/drivers.img"));
    }

    /// QEMU is always headless with VNC on a socket; a `gui = true` window
    /// is a separate viewer the CLI launches (§11), never QEMU's own GTK.
    #[test]
    fn always_headless_with_vnc() {
        let vm = resolved("linux-modern", "x86_64");
        let mut p = paths();
        p.ovmf_vars = Some("/v".into());
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("-display none"), "{s}");
        assert!(!s.contains("gtk"), "{s}");
        assert!(s.contains("-vnc unix:"), "{s}");
    }

    /// The aarch64 `virt` machine has no IDE/AHCI or default USB controller,
    /// so CD-ROMs ride virtio-blk, USB needs an explicit xHCI, and UEFI is
    /// mandatory (no SeaBIOS fallback). swtpm uses the MMIO tpm-tis-device.
    #[test]
    fn aarch64_virt_shape() {
        let mut vm = resolved("linux-modern", "aarch64");
        vm.tpm = true;
        let mut p = paths();
        p.ovmf_vars = Some("/lab/.vmlab/t/AAVMF_VARS.fd".into());
        p.tpm_sock = Some("/run/l/t/tpm.sock".into());
        p.cdroms = vec!["/lab/.vmlab/media/cidata.iso".into()];
        let s = joined(&build_args("lab1", &vm, &p, Accel::Tcg).unwrap());
        assert!(s.contains("-machine virt"), "{s}");
        assert!(s.contains("if=pflash,format=raw,readonly=on"), "UEFI: {s}");
        assert!(
            s.contains("virtio-blk-pci,drive=cd0"),
            "CD-ROM must ride virtio on virt: {s}"
        );
        assert!(!s.contains("ide-cd"), "no IDE on virt: {s}");
        assert!(s.contains("qemu-xhci"), "explicit USB controller: {s}");
        assert!(!s.contains("-usb "), "no bare -usb on virt: {s}");
        assert!(s.contains("usb-tablet"), "{s}");
        assert!(s.contains("virtio-gpu-pci"), "{s}");
        assert!(s.contains("tpm-tis-device,tpmdev=tpm0"), "{s}");
        assert!(s.contains("virtio-blk-pci,drive=disk0"), "{s}");
    }

    /// riscv64 shares the aarch64 `virt` path (virtio CD-ROMs, explicit
    /// xHCI, MMIO swtpm, mandatory UEFI) but pins `acpi=off` on the machine.
    #[test]
    fn riscv64_virt_shape() {
        let mut vm = resolved("linux-modern", "riscv64");
        vm.tpm = true;
        let mut p = paths();
        p.ovmf_vars = Some("/lab/.vmlab/t/RISCV_VIRT_VARS.fd".into());
        p.tpm_sock = Some("/run/l/t/tpm.sock".into());
        p.cdroms = vec!["/lab/.vmlab/media/cidata.iso".into()];
        let s = joined(&build_args("lab1", &vm, &p, Accel::Tcg).unwrap());
        assert!(s.contains("-machine virt,acpi=off"), "{s}");
        assert!(s.contains("if=pflash,format=raw,readonly=on"), "UEFI: {s}");
        assert!(
            s.contains("virtio-blk-pci,drive=cd0"),
            "CD-ROM must ride virtio on virt: {s}"
        );
        assert!(!s.contains("ide-cd"), "no IDE on virt: {s}");
        assert!(s.contains("qemu-xhci"), "explicit USB controller: {s}");
        assert!(!s.contains("-usb "), "no bare -usb on virt: {s}");
        assert!(s.contains("usb-tablet"), "{s}");
        assert!(s.contains("tpm-tis-device,tpmdev=tpm0"), "{s}");
        assert!(s.contains("virtio-blk-pci,drive=disk0"), "{s}");
    }

    /// Two CD-ROMs on q35 must land on distinct AHCI ports (a port holds a
    /// single unit; auto-placement would stack them on ide.0 and fail).
    #[test]
    fn q35_cdroms_get_their_own_ahci_ports() {
        let vm = resolved("linux-modern", "x86_64");
        let mut p = paths();
        p.ovmf_vars = Some("/v".into());
        p.cdroms = vec![
            "/isos/installer.iso".into(),
            "/lab/.vmlab/media/cidata.iso".into(),
        ];
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("ide-cd,drive=cd0,bus=ide.0"), "{s}");
        assert!(s.contains("ide-cd,drive=cd1,bus=ide.1"), "{s}");
    }
}
