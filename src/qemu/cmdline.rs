//! Build the QEMU argv for one VM (PRD §3, §5.2). Pure function of the
//! resolved hardware + runtime paths, so it is exhaustively unit-testable.
//! `qemu_args` from the lab file are appended verbatim, last, so they win.
//! QEMU runs aarch64 and riscv64 guests on the `virt` machine; an x86 guest
//! runs on vmlab-hv only (§22), so no q35 or pc argv is composed here.

use std::path::{Path, PathBuf};

use anyhow::Result;
use vmlab_agent_proto::PORT_NAME as AGENT_PORT_NAME;

use super::resolve::ResolvedVm;
use crate::config::model::{AccelMode, GpuMode, InputDevices, MacAddr};
use crate::profiles::{AgentTransport, FirmwareKind};

/// One virtio-fs share a hypervisor serves in process
/// ([`VmPaths::hypervisor_shares`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HypervisorShare {
    /// The mount tag the guest mounts by.
    pub tag: String,
    /// The host directory exported.
    pub host: PathBuf,
    pub readonly: bool,
}

/// Per-VM runtime paths and attachments supplied by the lab daemon.
#[derive(Debug, Clone, Default)]
pub struct VmPaths {
    /// The host's Apple SMC key for a machine with `apple_smc` (§5.2), which
    /// vmlab-hv's description carries; its `Debug` is redacted.
    pub apple_smc_osk: Option<crate::config::host::AppleSmcOsk>,
    pub qmp_sock: PathBuf,
    /// vmlab-agent channel (`vmlab.agent.0`): interactive terminals,
    /// streaming exec, file transfer, metrics — see `guest/agent-proto`.
    pub agent_sock: PathBuf,
    pub vnc_sock: PathBuf,
    /// vmlab-hv's native frame socket (§22, phase 2b), beside `vnc_sock`.
    /// QEMU has none and ignores it.
    pub display_sock: PathBuf,
    /// Primary disk qcow2 (linked clone or blank).
    pub primary_disk: PathBuf,
    /// Additional disks in declaration order: (name, path).
    pub extra_disks: Vec<(String, PathBuf)>,
    /// CD-ROM attachments (paths to ISOs, including built media).
    pub cdroms: Vec<PathBuf>,
    /// `cdroms[0]` is the machine's *install* CD-ROM — the VM's `cdrom =`,
    /// which a template build's `source "iso"` renders to — rather than built
    /// media. Its device then carries the qdev id [`INSTALL_CDROM_ID`], the
    /// one drive `eject_cdrom()` addresses.
    pub install_cdrom: bool,
    /// Floppy attachment.
    pub floppy: Option<PathBuf>,
    /// NIC attachments in declaration order.
    pub nics: Vec<NicSpec>,
    /// Read-only UEFI CODE image for this VM's arch and secure-boot setting
    /// (`firmware::UefiFirmware::code`). Discovery probes the host, so the
    /// lab daemon does it once when it assembles these paths — this builder
    /// stays a pure function of what it is handed (ADR-0008).
    pub firmware_code: Option<PathBuf>,
    /// Reproducible SeaBIOS selected for q35 or legacy PC hardware.
    pub seabios_code: Option<PathBuf>,
    /// The selected firmware accepts connection hints without rewriting BootOrder.
    pub firmware_boot_hints: bool,
    pub firmware_resolution: Option<(u32, u32)>,
    /// The reproducible PCI ROM for the resolved NIC model.
    pub nic_rom: Option<PathBuf>,
    /// Writable OVMF VARS copy for this VM (created by the lab daemon from
    /// the template in `firmware::UefiFirmware::vars_template`).
    pub ovmf_vars: Option<PathBuf>,
    /// SeaBIOS JPEG splash, staged by the daemon for x86 QEMU guests only.
    pub bootsplash: Option<PathBuf>,
    /// swtpm control socket (when tpm enabled).
    pub tpm_sock: Option<PathBuf>,
    /// Serial console log file.
    pub serial_log: Option<PathBuf>,
    /// virtiofs shares in declaration order: (mount tag, virtiofsd socket).
    /// The daemons must be listening before QEMU spawns; non-empty switches
    /// the RAM to a shared memory-backend-memfd (§7.5).
    pub virtiofs_shares: Vec<(String, PathBuf)>,
    /// virtio-fs shares a hypervisor serves itself, in process, from the
    /// host directory (vmlab-hv, §22 phase 2e): no virtiofsd, no socket.
    /// QEMU has no such device and is never handed any.
    pub hypervisor_shares: Vec<HypervisorShare>,
    /// The host render node a `virgl`/`vulkan` GPU's `egl-headless` display
    /// renders on (`hostgpu::Sysfs::resolve`). Resolved by the lab daemon from
    /// sysfs; `None` leaves QEMU's choice.
    pub gpu_render_node: Option<PathBuf>,
    /// The host PCI address a `passthrough` GPU hands the guest: the
    /// declared host GPU the lab daemon picked (`hostgpu::render_plan`).
    pub gpu_passthrough: Option<String>,
    /// The other functions of that card that go with it on vmlab-hv, in
    /// function order ([`super::hostgpu::RenderPlan::passthrough_functions`]).
    /// This argv hands over the one address above.
    pub gpu_passthrough_functions: Vec<String>,
    /// The TSC frequency, in Hz, a `tsc = "host"` VM is pinned at under KVM
    /// (§5.2): the host's, read from KVM by the lab daemon at the VM's first
    /// start and kept (`tsc::probe_host_hz`). Required there; read nowhere
    /// else.
    pub tsc_hz: Option<u64>,
    /// A data directory searched ahead of QEMU's own (`-L`): the stock
    /// QEMU's, handed to a host GPU's `qemu_binary` so a minimal build finds
    /// its firmware and ROMs (`hostgpu::RenderPlan::data_dir`).
    pub qemu_data_dir: Option<PathBuf>,
    /// The host's KVM virtualises the APIC — Intel APICv or AMD AVIC
    /// ([`host_apic_virtualization`]). A host fact the lab daemon reads once,
    /// like the firmware, so the builder stays pure: it decides whether a
    /// `hyperv` guest gets [`HYPERV_APICV_FLAG`] (§5.2).
    pub apic_virtualization: bool,
}

/// A path as it stands inside a QEMU option value: QEMU splits the value at
/// each `,` and reads `,,` as one literal comma. A path QEMU takes as a whole
/// argument (`-bios`, `-readconfig`, `-L`, `-serial file:`) is not escaped.
pub fn opt_path(path: &Path) -> String {
    path.display().to_string().replace(',', ",,")
}

/// The qdev id of a VM's install CD-ROM device (see
/// [`VmPaths::install_cdrom`]). Only that drive carries an id, so nothing
/// addressed by it can reach built media or the VMLAB bootstrap ISO.
pub const INSTALL_CDROM_ID: &str = "install-cd";

/// One NIC attachment for the argv builder.
#[derive(Debug, Clone)]
pub struct NicSpec {
    pub mac: MacAddr,
    /// Segment MTU; drives `host_mtu=` on virtio NICs so the guest can use
    /// a jumbo segment. `None`/1500 leaves the link at the default.
    pub mtu: Option<u16>,
    pub backend: NicBackend,
}

/// The network backend carried by the typed VM launch.
#[derive(Debug, Clone)]
pub enum NicBackend {
    /// Stream-socket netdev into the userspace switch (§9.1); the daemon
    /// listens, QEMU connects.
    Stream { sock: PathBuf },
    /// Pre-opened tap fd inherited at this descriptor number (the afxdp
    /// fast path; the daemon maps the fd in at spawn).
    Tap { child_fd: i32 },
    /// Descriptors retained by vmlab and sent to vmlab-hv over its control socket.
    /// The entries name `LaunchSpec::fds` mappings, not inherited descriptors.
    TapQueues {
        tap_fds: Vec<i32>,
        vhost_fds: Vec<i32>,
    },
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
/// A machine that declares no `accel` gets this; a VM that may declare one
/// goes through [`plan_accel`].
pub fn pick_accel(arch: &str) -> Accel {
    if kvm_usable(arch) {
        Accel::Kvm
    } else {
        Accel::Tcg
    }
}

/// Whether KVM can run a guest of `arch` here: /dev/kvm opens read/write
/// and the guest arch is the host's.
pub fn kvm_usable(arch: &str) -> bool {
    kvm_available() && qemu_arch(arch) == std::env::consts::ARCH
}

/// The accelerator a VM launches under, decided before the emulator
/// spawns (ADR-0003) — so a refusal happens before anything is started,
/// and the caller's only branch is whether to warn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelPlan {
    /// KVM: declared, or vmlab's choice where it is usable.
    Kvm,
    /// TCG because the VM declared `accel = "tcg"` — nothing to warn about.
    Tcg,
    /// TCG because KVM is unusable and no layer declared an `accel`: the
    /// slow fallback, which the caller warns about.
    TcgFallback,
}

impl AccelPlan {
    pub fn accel(self) -> Accel {
        match self {
            AccelPlan::Kvm => Accel::Kvm,
            AccelPlan::Tcg | AccelPlan::TcgFallback => Accel::Tcg,
        }
    }
}

/// Decide a VM's accelerator from its resolved `accel` (§5.2) and whether
/// KVM is usable for its arch ([`kvm_usable`], injected so the decision is
/// testable on any host). Unset keeps vmlab's choice with the fallback;
/// `tcg` always emulates; `kvm` refuses rather than fall back.
pub fn plan_accel(vm: &ResolvedVm, kvm_usable: bool) -> Result<AccelPlan> {
    match &vm.accel {
        None if kvm_usable => Ok(AccelPlan::Kvm),
        None => Ok(AccelPlan::TcgFallback),
        Some(req) => match req.mode {
            AccelMode::Tcg => Ok(AccelPlan::Tcg),
            AccelMode::Kvm if kvm_usable => Ok(AccelPlan::Kvm),
            AccelMode::Kvm => anyhow::bail!(
                "vm \"{}\": accel = \"kvm\" (from {}) but KVM is not usable for a {} guest \
                 on this {} host — /dev/kvm must open read/write and the guest arch must be the \
                 host's; drop the `accel` to allow the TCG fallback, or set accel = \"tcg\" \
                 (PRD §5.2)",
                vm.name,
                req.from,
                vm.arch,
                std::env::consts::ARCH,
            ),
        },
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

/// Why a `nested = true` guest will not see VMX/SVM on this host, with the
/// fix — or None when there is nothing to say. The loaded x86 KVM module's
/// `nested` parameter is the whole question: off, `-cpu host` has no VMX/SVM
/// to pass through however the VM is declared (§5.2). No module loaded means
/// no x86 KVM at all, which the TCG fallback warning already covers.
pub fn host_nested_problem() -> Option<String> {
    host_nested_problem_in(Path::new("/sys/module"))
}

fn host_nested_problem_in(sys_module: &Path) -> Option<String> {
    for (module, feature) in [("kvm_intel", "VMX"), ("kvm_amd", "SVM")] {
        let param = sys_module.join(module).join("parameters/nested");
        let Ok(value) = std::fs::read_to_string(&param) else {
            continue;
        };
        let value = value.trim();
        // `Y`/`N` from kvm_intel, `1`/`0` from kvm_amd.
        if matches!(value, "Y" | "y" | "1") {
            return None;
        }
        return Some(format!(
            "nested = true, but the host's {module} has nested virtualisation off \
             ({} = {value}), so the guest sees no {feature} — enable it with \
             `echo \"options {module} nested=1\" | sudo tee /etc/modprobe.d/kvm-nested.conf` \
             and reload the module with no VM running \
             (`sudo modprobe -r {module} && sudo modprobe {module}`)",
            param.display(),
        ));
    }
    None
}

/// Why a macOS guest faults on this host, with the fix — or None when there
/// is nothing to say (§5.2). XNU reads Intel MSRs that KVM does not emulate,
/// and KVM injects a #GP for each unless `ignore_msrs` is on; on an AMD host
/// that is every boot. No KVM module loaded means no KVM at all, which the
/// TCG fallback warning already covers.
pub fn host_msrs_problem() -> Option<String> {
    host_msrs_problem_in(Path::new("/sys/module"))
}

fn host_msrs_problem_in(sys_module: &Path) -> Option<String> {
    let param = sys_module.join("kvm/parameters/ignore_msrs");
    let value = std::fs::read_to_string(&param).ok()?;
    let value = value.trim();
    if matches!(value, "Y" | "y" | "1") {
        return None;
    }
    Some(format!(
        "a macOS guest needs KVM to ignore the MSRs it does not emulate, and this host's \
         kvm has ignore_msrs off ({} = {value}), so the guest faults during boot — set it with \
         `echo \"options kvm ignore_msrs=1 report_ignored_msrs=0\" | sudo tee \
         /etc/modprobe.d/kvm-msrs.conf` and, until the next boot, \
         `echo 1 | sudo tee {}` (PRD §5.2)",
        param.display(),
        param.display(),
    ))
}

/// Whether the host's KVM virtualises the APIC: `kvm_intel`'s
/// `enable_apicv` or `kvm_amd`'s `avic`. Read where the runtime paths are
/// assembled and handed to the builder as
/// [`VmPaths::apic_virtualization`].
pub fn host_apic_virtualization() -> bool {
    host_apic_virtualization_in(Path::new("/sys/module"))
}

fn host_apic_virtualization_in(sys_module: &Path) -> bool {
    [("kvm_intel", "enable_apicv"), ("kvm_amd", "avic")]
        .iter()
        .any(|(module, param)| {
            std::fs::read_to_string(sys_module.join(module).join("parameters").join(param))
                .is_ok_and(|v| matches!(v.trim(), "Y" | "y" | "1"))
        })
}

/// Why a VM that resolved `hyperv = true` boots without the enlightenments
/// under `accel` — `None` when it gets them, or never asked. They are KVM's
/// (TCG does not implement them), x86's, and an addition to the host CPU, so
/// a named `cpu_model`, which decides the CPU alone, goes without (§5.2).
fn hyperv_skipped(vm: &ResolvedVm, accel: Accel) -> Option<&'static str> {
    if !vm.hyperv {
        None
    } else if qemu_arch(&vm.arch) != "x86_64" {
        Some("the enlightenments are x86 only")
    } else if accel == Accel::Tcg {
        Some("it runs under TCG, and the enlightenments are KVM's")
    } else if vm.cpu_model.is_some() {
        Some("a `cpu_model` names the CPU, and the enlightenments are added to the host CPU only")
    } else {
        None
    }
}

/// The line `up` prints for a VM whose own block set `hyperv = true` and
/// that boots without the enlightenments under `accel` (§5.2). `declared` is
/// the vm block's `hyperv`: one inherited from a template or a profile says
/// nothing, because a profile default is not something the lab author asked
/// for.
pub fn hyperv_not_applied(declared: Option<bool>, vm: &ResolvedVm, accel: Accel) -> Option<String> {
    if declared != Some(true) {
        return None;
    }
    hyperv_skipped(vm, accel).map(|why| {
        format!(
            "vm \"{}\": hyperv = true is not applied — {why}; it boots without the Hyper-V \
             enlightenments (PRD §5.2)",
            vm.name
        )
    })
}

/// Why a VM that resolved `tsc = "host"` boots without a pinned TSC under
/// `accel` — `None` when it gets one, or never asked. The frequency is read
/// from KVM, and `tsc-frequency` is an x86 CPU property (§5.2).
fn tsc_skipped(vm: &ResolvedVm, accel: Accel) -> Option<&'static str> {
    if vm.tsc.is_none() {
        None
    } else if accel == Accel::Tcg {
        Some("it runs under TCG, and the frequency is read from KVM")
    } else {
        None
    }
}

/// The line `up` prints for a VM that resolved `tsc = "host"` and boots
/// without a pinned TSC under `accel` (§5.2). Unlike `hyperv`, said for an
/// inherited one too: a guest that needs an invariant TSC — macOS — does not
/// boot without one, and its profile is where the setting usually comes
/// from.
pub fn tsc_not_applied(vm: &ResolvedVm, accel: Accel) -> Option<String> {
    tsc_skipped(vm, accel).map(|why| {
        format!(
            "vm \"{}\": tsc = \"host\" is not applied — {why}; it boots with QEMU's default \
             TSC, which a macOS guest does not boot with (PRD §5.2)",
            vm.name
        )
    })
}

/// The line `up` prints for a VM whose secure boot rests on a variable store
/// the guest's own kernel can rewrite (§5.2) — `None` for a VM without
/// secure boot, and for an x86_64 one. vmlab-hv opens an x86_64 store so
/// that only SMM writes it. QEMU's `virt` machine, which runs every other
/// arch, gets the store as plain NOR flash ([`build_args`]) and has no
/// secure world to keep it in, so nothing above the guest kernel guards PK,
/// KEK, db and dbx. Said for an inherited `secure_boot` too: the profile or
/// the template is where it usually comes from.
pub fn secure_boot_store_unprotected(vm: &ResolvedVm) -> Option<String> {
    let exposed = vm.secure_boot
        && vm.firmware == Some(FirmwareKind::Ovmf)
        && qemu_arch(&vm.arch) != "x86_64";
    exposed.then(|| {
        format!(
            "vm \"{}\": secure_boot = true verifies what the firmware boots, but on {} the \
             UEFI variable store is flash the guest's own kernel can write — code running in \
             the guest kernel can replace its secure-boot keys or turn secure boot off, and \
             the change survives a reset; only an x86_64 guest's store is protected from its \
             OS (PRD §5.2)",
            vm.name, vm.arch
        )
    })
}

pub fn emulator_binary(arch: &str) -> String {
    format!("qemu-system-{}", qemu_arch(arch))
}

/// Build the full argv (excluding argv[0], which is `emulator_binary`).
///
/// QEMU runs the guests vmlab-hv does not: aarch64 and riscv64, on the
/// `virt` machine. An x86 or x86_64 guest is refused here — vmlab-hv is its
/// only hypervisor (PRD §22) — so no q35 or pc argv is composed.
pub fn build_args(
    lab: &str,
    vm: &ResolvedVm,
    paths: &VmPaths,
    accel: Accel,
) -> Result<Vec<String>> {
    if qemu_arch(&vm.arch) == "x86_64" {
        anyhow::bail!(
            "vm \"{}\": QEMU no longer runs x86 guests — vmlab-hv runs them (PRD §22); set \
             hypervisor = \"vmlab\", or remove the setting that names QEMU",
            vm.name
        );
    }
    if vm
        .gpu
        .as_ref()
        .is_some_and(|gpu| gpu.virtual_display || gpu.virtual_display_memory.is_some())
    {
        anyhow::bail!(
            "vm \"{}\": gpu virtual_display requires hypervisor = \"vmlab\"; QEMU does not support it",
            vm.name
        );
    }
    if vm.agent_transport == AgentTransport::IsaSerial {
        anyhow::bail!(
            "vm \"{}\": agent_transport = \"isa-serial\" is the legacy x86 tier, which QEMU \
             no longer runs (PRD §22)",
            vm.name
        );
    }
    fn arg(a: &mut Vec<String>, s: &str, v: String) {
        a.push(format!("-{s}"));
        a.push(v);
    }
    let mut a: Vec<String> = Vec::new();

    arg(&mut a, "name", format!("vmlab:{lab}/{}", vm.name));
    if let Some(dir) = &paths.qemu_data_dir {
        arg(&mut a, "L", dir.display().to_string());
    }
    // vhost-user-fs shares need the guest RAM in shared memory. Only VMs
    // that carry one get the memfd backend: it renames the RAM block in
    // internal snapshots, so the plain `-m` shape stays untouched for every
    // other VM (§7.5).
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
    // A named model (§5.2) replaces the accelerator default under either
    // accelerator: it is the CPU the guest was declared to see. QEMU owns the
    // catalogue, so an unknown name is refused by QEMU at start. Under KVM
    // the host CPU passes through unchanged.
    let (accel_name, default_cpu) = match accel {
        Accel::Kvm => ("kvm", "host"),
        Accel::Tcg => ("tcg", "max"),
    };
    arg(&mut a, "accel", accel_name.into());
    let cpu = vm.cpu_model.clone().unwrap_or_else(|| default_cpu.into());
    arg(&mut a, "cpu", cpu);
    arg(&mut a, "smp", vm.cpus.to_string());
    arg(&mut a, "m", format!("{}M", vm.memory >> 20));
    // A fixed clock starts the RTC at the declared instant, and a guest that
    // reads the RTC as local time (the profile's `rtc`) has it start at the
    // host's (§7.3); otherwise QEMU's default, the host's UTC time, applies.
    if let Some(base) = &vm.rtc_base {
        arg(&mut a, "rtc", format!("base={base}"));
    }

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
        format!("unix:{},share=ignore", opt_path(&paths.vnc_sock)),
    );

    arg(
        &mut a,
        "qmp",
        format!("unix:{},server=on,wait=off", opt_path(&paths.qmp_sock)),
    );
    a.push("-monitor".into());
    a.push("none".into());

    if let Some(log) = &paths.serial_log {
        arg(&mut a, "serial", format!("file:{}", log.display()));
    }

    // UEFI firmware: CODE read-only pflash + per-VM writable VARS. `virt` has
    // no secure world here, so the VARS flash is writable by the guest OS —
    // under secure boot too, which [`secure_boot_store_unprotected`] says.
    if vm.firmware == Some(FirmwareKind::Ovmf) {
        let code = paths
            .firmware_code
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OVMF requires a firmware CODE path"))?;
        arg(
            &mut a,
            "drive",
            format!("if=pflash,format=raw,readonly=on,file={}", opt_path(code)),
        );
        let vars = paths
            .ovmf_vars
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OVMF requires a per-VM VARS path"))?;
        arg(
            &mut a,
            "drive",
            format!("if=pflash,format=raw,file={}", opt_path(vars)),
        );
    }

    // Guest agent channel (§7.4): the vmlab-agent port (interactive
    // terminals / exec / files / net-info / shutdown — guest/agent-proto).
    // QEMU listens, the daemon connects.
    if vm.agent_transport == AgentTransport::VirtioSerial {
        arg(
            &mut a,
            "chardev",
            format!(
                "socket,id=vagent0,path={},server=on,wait=off",
                opt_path(&paths.agent_sock)
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
    // so QMP snapshot commands can address them (§7.3). `virt` has no IDE or
    // AHCI bus, so every disk is virtio-blk whatever the profile's bus.
    // SeaBIOS boots install media before disks; under UEFI the guest's own
    // boot order decides.
    let indexed_boot = vm.firmware != Some(FirmwareKind::Ovmf);
    let mut disk_index = 0usize;
    let mut add_disk = |a: &mut Vec<String>, path: &Path| {
        let node = format!("disk{disk_index}");
        a.push("-blockdev".into());
        a.push(format!(
            "driver=qcow2,node-name={node},file.driver=file,file.filename={}",
            opt_path(path)
        ));
        a.push("-device".into());
        let boot = if indexed_boot {
            format!(",bootindex={}", 50 + disk_index)
        } else {
            String::new()
        };
        a.push(format!("virtio-blk-pci,drive={node}{boot}"));
        disk_index += 1;
    };
    add_disk(&mut a, &paths.primary_disk);
    for (_, path) in &paths.extra_disks {
        add_disk(&mut a, path);
    }

    // CD-ROMs: read-only virtio-blk, `virt` having no IDE/AHCI bus —
    // cloud-init's NoCloud datasource finds the CIDATA seed by filesystem
    // label on any block device, regardless of bus.
    for (i, iso) in paths.cdroms.iter().enumerate() {
        a.push("-blockdev".into());
        a.push(format!(
            "driver=raw,node-name=cd{i},read-only=on,file.driver=file,file.filename={}",
            opt_path(iso)
        ));
        a.push("-device".into());
        let boot = if indexed_boot {
            format!(",bootindex={i}")
        } else {
            String::new()
        };
        // The install CD-ROM gets a qdev id: QMP `eject` addresses a drive
        // by it, a `-blockdev` drive having no backend name to give.
        let id = if i == 0 && paths.install_cdrom {
            format!(",id={INSTALL_CDROM_ID}")
        } else {
            String::new()
        };
        a.push(format!("virtio-blk-pci,drive=cd{i}{boot}{id}"));
    }

    if paths.floppy.is_some() {
        anyhow::bail!(
            "vm \"{}\": a floppy drive is x86 hardware, and QEMU runs no x86 guest (PRD §22)",
            vm.name
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
                    opt_path(sock)
                ),
            ),
            NicBackend::Tap { child_fd } => {
                arg(&mut a, "netdev", format!("tap,id=net{i},fd={child_fd}"));
            }
            NicBackend::TapQueues { .. } => {
                return Err(anyhow::anyhow!(
                    "control-socket TAP queues require vmlab-hv"
                ));
            }
        }
        // virtio-net negotiates a jumbo MTU with the guest via `host_mtu`;
        // other models don't support it reliably, so they stay at the default.
        let host_mtu = nic
            .mtu
            .filter(|m| *m != 1500 && vm.nic_model.starts_with("virtio-net"))
            .map(|m| format!(",host_mtu={m}"))
            .unwrap_or_default();
        let rom = paths
            .nic_rom
            .as_ref()
            .map(|path| format!(",romfile={}", opt_path(path)))
            .unwrap_or_default();
        arg(
            &mut a,
            "device",
            format!(
                "{},netdev=net{i},mac={}{host_mtu}{rom}",
                vm.nic_model, nic.mac
            ),
        );
    }
    if paths.nics.is_empty() {
        // No nic blocks = no network hardware at all (§5.2).
        a.push("-nic".into());
        a.push("none".into());
    }

    // TPM via swtpm (§5.3), the sysbus TIS `virt` takes.
    if let Some(tpm_sock) = &paths.tpm_sock {
        arg(
            &mut a,
            "chardev",
            format!("socket,id=chrtpm,path={}", opt_path(tpm_sock)),
        );
        arg(&mut a, "tpmdev", "emulator,id=tpm0,chardev=chrtpm".into());
        arg(&mut a, "device", "tpm-tis-device,tpmdev=tpm0".into());
    }

    // Display device / GPU modes (§5.2). Paravirt GL renders host-side with
    // the framebuffer scraped for VNC (egl-headless path).
    match &vm.gpu {
        Some(gpu) => match gpu.mode {
            GpuMode::Passthrough => {
                // The lab daemon's host GPU pick refuses passthrough with
                // nothing to pick; keep a real error in case a path skips it.
                let addr = paths.gpu_passthrough.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("gpu passthrough needs a host GPU (PRD §5.2)")
                })?;
                arg(&mut a, "device", format!("vfio-pci,host={addr}"));
                if let Some(d) = &vm.display_device {
                    arg(&mut a, "device", d.clone());
                }
            }
            GpuMode::Virgl | GpuMode::Vulkan => {
                // One display adapter, the GL one: `virt` has no legacy VGA,
                // so the pure device plus an explicit `-vga none`. A second
                // adapter in the guest leaves Venus enumerating no device at
                // all. QEMU refuses Venus without blob resources and a
                // hostmem window.
                let size = gpu.vram_bytes();
                let hostmem = if size >= 1 << 30 {
                    format!("{}G", size >> 30)
                } else {
                    format!("{}M", size >> 20)
                };
                let opts = if gpu.mode == GpuMode::Vulkan {
                    format!(",venus=true,blob=true,hostmem={hostmem}")
                } else if gpu.vram.is_some() {
                    format!(",blob=true,hostmem={hostmem}")
                } else {
                    String::new()
                };
                arg(&mut a, "device", format!("virtio-gpu-gl{opts}"));
                arg(&mut a, "vga", "none".into());
                // The scanout has to come from the GPU Venus renders on, or
                // the console is black.
                let display = match &paths.gpu_render_node {
                    Some(node) => format!("egl-headless,rendernode={}", opt_path(node)),
                    None => "egl-headless".into(),
                };
                arg(&mut a, "display", display);
            }
        },
        None => {
            if let Some(d) = &vm.display_device {
                arg(&mut a, "device", d.clone());
            }
        }
    }

    // USB tablet: absolute pointer events for screen automation (§10.3).
    // The `virt` machine has no default USB host controller, so it gets an
    // explicit xHCI — with the known id `usb = true` (§5.2.1) gives host
    // devices to hot-plug onto. `input = "usb"` (§5.2) adds a USB keyboard
    // beside the tablet.
    let xhci = if vm.usb {
        format!("qemu-xhci,id={}", crate::usb::CONTROLLER_ID)
    } else {
        "qemu-xhci".into()
    };
    arg(&mut a, "device", xhci);
    if vm.input == InputDevices::Usb {
        arg(&mut a, "device", "usb-kbd".into());
    }
    arg(&mut a, "device", "usb-tablet".into());

    // virtiofs shares: one vhost-user-fs device per export, tag = share
    // name — the guest agent mounts by tag once the VM is ready (§7.5).
    for (i, (tag, sock)) in paths.virtiofs_shares.iter().enumerate() {
        arg(
            &mut a,
            "chardev",
            format!("socket,id=vfs{i},path={}", opt_path(sock)),
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

#[cfg(test)]
mod tests {
    use super::*;
    // The real resolver, on real declarations: an argv test that hand-built
    // its resolved hardware would be asserting against a copy of the
    // precedence chain rather than against the chain (ADR-0008).
    use super::super::resolve::testing::resolved_gpu_vm as resolved_gpu;
    use super::super::resolve::testing::resolved_vm as resolved;

    /// Runtime paths the lab daemon would hand the builder, including the
    /// firmware it discovered on the host. Both firmware paths are always
    /// present here; `build_args` only reads them under OVMF, and no test
    /// depends on this build host shipping edk2.
    fn paths() -> VmPaths {
        VmPaths {
            qmp_sock: "/run/l/t/qmp.sock".into(),
            agent_sock: "/run/l/t/agent.sock".into(),
            vnc_sock: "/run/l/t/vnc.sock".into(),
            primary_disk: "/lab/.vmlab/t/disk0.qcow2".into(),
            firmware_code: Some("/usr/share/AAVMF/AAVMF_CODE.fd".into()),
            ovmf_vars: Some("/lab/.vmlab/t/AAVMF_VARS.fd".into()),
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

    /// The resolver's output, named so a signature does not read as a
    /// struct literal to `resolution_is_never_mirrored`.
    type Resolved = ResolvedVm;

    fn arm() -> Resolved {
        resolved("linux-modern", "aarch64")
    }

    /// QEMU runs no x86 guest (§22): every x86 profile, 32-bit included, is
    /// refused before any argv is composed, naming vmlab-hv.
    #[test]
    fn an_x86_guest_is_refused() {
        for (profile, arch) in [
            ("linux-modern", "x86_64"),
            ("windows-11", "x86_64"),
            ("windows-legacy", "x86_64"),
            ("windows-9x", "x86"),
            ("macos", "x86_64"),
        ] {
            let vm = resolved(profile, arch);
            let e = build_args("l", &vm, &paths(), Accel::Kvm)
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("QEMU no longer runs x86 guests") && e.contains("vmlab-hv"),
                "{profile}: {e}"
            );
        }
    }

    #[test]
    fn an_explicit_nic_rom_is_passed_to_qemu() {
        let mut p = paths();
        p.nic_rom = Some("/fw/virtio-net.rom".into());
        let args = joined(&build_args("l", &arm(), &p, Accel::Kvm).unwrap());
        assert!(args.contains("romfile=/fw/virtio-net.rom"), "{args}");
    }

    /// QEMU splits an option value at each `,` and reads `,,` as a literal
    /// comma, so a path inside an option value must have its commas doubled.
    /// `-serial file:` takes a path whole, so its path stays as it is.
    #[test]
    fn a_comma_in_a_path_is_doubled_in_every_option() {
        let dir = "/lab/with,comma";
        let at = |name: &str| PathBuf::from(format!("{dir}/{name}"));
        let mut p = paths();
        p.qmp_sock = at("qmp.sock");
        p.agent_sock = at("agent.sock");
        p.vnc_sock = at("vnc.sock");
        p.primary_disk = at("disk0.qcow2");
        p.extra_disks = vec![("data".into(), at("data.qcow2"))];
        p.cdroms = vec![at("install.iso"), at("seed.iso")];
        p.firmware_code = Some(at("AAVMF_CODE.fd"));
        p.ovmf_vars = Some(at("AAVMF_VARS.fd"));
        p.nic_rom = Some(at("virtio-net.rom"));
        p.tpm_sock = Some(at("swtpm.sock"));
        p.serial_log = Some(at("serial.log"));
        p.virtiofs_shares = vec![("src".into(), at("vfs0.sock"))];
        p.nics[0].backend = NicBackend::Stream {
            sock: at("nic0.sock"),
        };
        let mut vm = arm();
        vm.firmware = Some(FirmwareKind::Ovmf);
        vm.tpm = true;
        let args = build_args("lab", &vm, &p, Accel::Tcg).unwrap();
        let mut seen = 0;
        for value in &args {
            if !value.contains("with,") {
                continue;
            }
            seen += 1;
            if value.starts_with("file:") {
                assert!(!value.contains("with,,comma"), "{value}");
            } else {
                assert!(value.contains("with,,comma"), "{value}");
            }
        }
        // The disks, the CD-ROMs, both firmware files, the QMP, VNC, agent,
        // NIC, TPM and virtiofs sockets, the ROM and the serial log.
        assert!(seen >= 14, "{seen}: {args:?}");
    }

    /// virtiofs shares attach as vhost-user-fs devices and flip the RAM to
    /// a shared memfd backend; share-less VMs keep plain `-m` (RAM block
    /// name stability for existing snapshots).
    #[test]
    fn virtiofs_shares_shape() {
        let vm = arm();
        let mut p = paths();
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("memory-backend"), "{s}");
        assert!(!s.contains("vhost-user-fs"), "{s}");

        p.virtiofs_shares = vec![("mnt_src".into(), PathBuf::from("/run/l/t/vfs0.sock"))];
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(
            s.contains(&format!("-machine {},memory-backend=mem0", vm.machine)),
            "{s}"
        );
        assert!(
            s.contains(&format!(
                "memory-backend-memfd,id=mem0,size={}M,share=on",
                vm.memory >> 20
            )),
            "{s}"
        );
        assert!(s.contains("socket,id=vfs0,path=/run/l/t/vfs0.sock"), "{s}");
        assert!(
            s.contains("vhost-user-fs-pci,chardev=vfs0,tag=mnt_src"),
            "{s}"
        );
    }

    /// Guests with no agent transport at all get no agent socket and no
    /// virtio-serial bus; the serial log keeps its `-serial` shorthand.
    #[test]
    fn no_agent_transport_emits_no_agent_device() {
        let mut vm = arm();
        vm.agent_transport = AgentTransport::None;
        let mut p = paths();
        p.serial_log = Some("/logs/l/t/serial.log".into());
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("virtserialport"), "{s}");
        assert!(!s.contains("virtio-serial-pci"), "{s}");
        assert!(!s.contains("vagent0"), "{s}");
        assert!(s.contains("-serial file:/logs/l/t/serial.log"), "{s}");
    }

    /// The legacy agent tier rides a 16550 on COM1, which is x86's.
    #[test]
    fn the_isa_serial_transport_is_refused() {
        let mut vm = arm();
        vm.agent_transport = AgentTransport::IsaSerial;
        let e = build_args("l", &vm, &paths(), Accel::Kvm)
            .unwrap_err()
            .to_string();
        assert!(e.contains("isa-serial"), "{e}");
    }

    #[test]
    fn a_floppy_is_refused() {
        let mut p = paths();
        p.floppy = Some("/lab/.vmlab/media/drivers.img".into());
        let e = build_args("l", &arm(), &p, Accel::Kvm)
            .unwrap_err()
            .to_string();
        assert!(e.contains("floppy"), "{e}");
    }

    #[test]
    fn tap_backend_emits_fd_netdev() {
        let mut p = paths();
        p.nics[0].backend = NicBackend::Tap { child_fd: 10 };
        p.nics[0].mtu = Some(9000);
        let s = joined(&build_args("l", &arm(), &p, Accel::Kvm).unwrap());
        assert!(s.contains("-netdev tap,id=net0,fd=10"), "{s}");
        assert!(!s.contains("addr.type=unix"), "{s}");
        // The virtio device line is backend-agnostic (host_mtu still works).
        assert!(s.contains("netdev=net0,mac=52:54:00:00:00:01,host_mtu=9000"));
    }

    /// KVM passes the host CPU through unchanged, `nested` or not; TCG
    /// keeps `max`; a `cpu_model` replaces either default.
    #[test]
    fn the_cpu_is_the_host_under_kvm_max_under_tcg_or_the_declared_model() {
        let cpu = |vm: &ResolvedVm, accel: Accel| {
            let args = build_args("l", vm, &paths(), accel).unwrap();
            let i = args.iter().position(|a| a == "-cpu").unwrap();
            args[i + 1].clone()
        };
        let mut vm = arm();
        for nested in [false, true] {
            vm.nested = nested;
            assert_eq!(cpu(&vm, Accel::Kvm), "host");
            assert_eq!(cpu(&vm, Accel::Tcg), "max");
        }
        // The enlightenments are x86's: the host CPU stays as it was.
        vm.hyperv = true;
        assert_eq!(cpu(&vm, Accel::Kvm), "host");
        assert!(hyperv_skipped(&vm, Accel::Kvm).is_some_and(|w| w.contains("x86")));
        vm.cpu_model = Some("cortex-a72".into());
        assert_eq!(cpu(&vm, Accel::Kvm), "cortex-a72");
        assert_eq!(cpu(&vm, Accel::Tcg), "cortex-a72");
    }

    /// A fixed clock starts the RTC at the declared instant; any other clock
    /// leaves QEMU's default, the host's UTC time (§7.3).
    #[test]
    fn a_fixed_clock_sets_the_rtc_base() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let v = super::super::resolve::testing::vm(
            "vm \"t\" { template = \"scratch\" arch = \"aarch64\" profile = \"linux-modern\" disk = 1GiB clock = \"2020-01-01T00:00:00Z\" }",
        );
        let vm = super::super::resolve::resolve_vm(&v, None, &profiles).unwrap();
        let args = build_args("l", &vm, &paths(), Accel::Kvm).unwrap();
        let rtc = args.iter().position(|a| a == "-rtc").expect("-rtc");
        assert_eq!(args[rtc + 1], "base=2020-01-01T00:00:00");

        let args = build_args("l", &arm(), &paths(), Accel::Kvm).unwrap();
        assert!(!args.iter().any(|a| a == "-rtc"), "{args:?}");
    }

    /// Unset `accel` keeps vmlab's choice: KVM where usable, else the TCG
    /// fallback the caller warns about. `tcg` always emulates — with or
    /// without KVM, and never as a fallback. `kvm` refuses, naming the
    /// setting and its layer, rather than falling back.
    #[test]
    fn plan_accel_follows_the_declared_setting() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let with = |accel: &str| {
            let v = super::super::resolve::testing::vm(&format!(
                "vm \"t\" {{ template = \"scratch\" arch = \"aarch64\" profile = \"linux-modern\" disk = 1GiB {accel} }}"
            ));
            super::super::resolve::resolve_vm(&v, None, &profiles).unwrap()
        };
        let unset = with("");
        assert_eq!(plan_accel(&unset, true).unwrap(), AccelPlan::Kvm);
        assert_eq!(plan_accel(&unset, false).unwrap(), AccelPlan::TcgFallback);

        let tcg = with("accel = \"tcg\"");
        assert_eq!(plan_accel(&tcg, true).unwrap(), AccelPlan::Tcg);
        assert_eq!(plan_accel(&tcg, false).unwrap(), AccelPlan::Tcg);

        let kvm = with("accel = \"kvm\"");
        assert_eq!(plan_accel(&kvm, true).unwrap(), AccelPlan::Kvm);
        let err = plan_accel(&kvm, false).unwrap_err().to_string();
        assert!(
            err.contains("vm \"t\": accel = \"kvm\" (from the vm block) but KVM is not usable"),
            "{err}"
        );

        assert_eq!(AccelPlan::Kvm.accel(), Accel::Kvm);
        assert_eq!(AccelPlan::Tcg.accel(), Accel::Tcg);
        assert_eq!(AccelPlan::TcgFallback.accel(), Accel::Tcg);
    }

    /// `up` speaks only for a `hyperv = true` the vm block itself set and
    /// the launch drops; a profile's default that does not apply is silent.
    #[test]
    fn hyperv_not_applied_speaks_for_the_vm_block_only() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let resolve = |extra: &str| {
            let v = super::super::resolve::testing::vm(&format!(
                "vm \"w\" {{ template = \"scratch\" arch = \"x86_64\" profile = \"windows-11\" \
                 disk = 1GiB {extra} }}"
            ));
            let vm = super::super::resolve::resolve_vm(&v, None, &profiles).unwrap();
            (v.hyperv, vm)
        };
        // From the profile: silent under TCG and with a model.
        let (declared, inherited) = resolve("");
        assert_eq!(declared, None);
        assert_eq!(hyperv_not_applied(declared, &inherited, Accel::Tcg), None);
        let (declared, model) = resolve("cpu_model = \"Skylake-Client\"");
        assert_eq!(hyperv_not_applied(declared, &model, Accel::Kvm), None);
        // From the vm block: applied under KVM, said under TCG or a model.
        let (declared, own) = resolve("hyperv = true");
        assert_eq!(hyperv_not_applied(declared, &own, Accel::Kvm), None);
        let line = hyperv_not_applied(declared, &own, Accel::Tcg).unwrap();
        assert!(
            line.starts_with("vm \"w\": hyperv = true is not applied"),
            "{line}"
        );
        assert!(line.contains("TCG"), "{line}");
        let (declared, own_model) = resolve("hyperv = true cpu_model = \"Skylake-Client\"");
        let line = hyperv_not_applied(declared, &own_model, Accel::Kvm).unwrap();
        assert!(line.contains("cpu_model"), "{line}");
        // Turned off on the vm block: nothing to say.
        let (declared, off) = resolve("hyperv = false");
        assert_eq!(hyperv_not_applied(declared, &off, Accel::Tcg), None);
    }

    /// `tsc = "host"` is said when TCG leaves it off.
    #[test]
    fn a_pinned_tsc_is_said_under_tcg() {
        let vm = resolved("macos", "x86_64");
        let said = tsc_not_applied(&vm, Accel::Tcg).unwrap();
        assert!(said.contains("tsc = \"host\" is not applied"), "{said}");
        assert_eq!(tsc_not_applied(&vm, Accel::Kvm), None);
        assert_eq!(
            tsc_not_applied(&resolved("linux-modern", "x86_64"), Accel::Tcg),
            None
        );
    }

    #[test]
    fn host_nested_off_is_named_with_its_fix() {
        let sys = tempfile::tempdir().unwrap();
        assert_eq!(
            host_nested_problem_in(sys.path()),
            None,
            "no module: nothing to say"
        );

        let params = sys.path().join("kvm_amd/parameters");
        std::fs::create_dir_all(&params).unwrap();
        std::fs::write(params.join("nested"), "1\n").unwrap();
        assert_eq!(host_nested_problem_in(sys.path()), None);

        std::fs::write(params.join("nested"), "0\n").unwrap();
        let msg = host_nested_problem_in(sys.path()).unwrap();
        assert!(
            msg.contains("kvm_amd") && msg.contains("= 0") && msg.contains("SVM"),
            "{msg}"
        );
        assert!(msg.contains("options kvm_amd nested=1"), "{msg}");

        let sys = tempfile::tempdir().unwrap();
        let params = sys.path().join("kvm_intel/parameters");
        std::fs::create_dir_all(&params).unwrap();
        std::fs::write(params.join("nested"), "N\n").unwrap();
        let msg = host_nested_problem_in(sys.path()).unwrap();
        assert!(msg.contains("kvm_intel") && msg.contains("VMX"), "{msg}");
        std::fs::write(params.join("nested"), "Y\n").unwrap();
        assert_eq!(host_nested_problem_in(sys.path()), None);
    }

    #[test]
    fn host_apic_virtualization_reads_either_vendors_parameter() {
        let sys = tempfile::tempdir().unwrap();
        assert!(!host_apic_virtualization_in(sys.path()), "no module");

        let amd = sys.path().join("kvm_amd/parameters");
        std::fs::create_dir_all(&amd).unwrap();
        std::fs::write(amd.join("avic"), "N\n").unwrap();
        assert!(!host_apic_virtualization_in(sys.path()));
        std::fs::write(amd.join("avic"), "Y\n").unwrap();
        assert!(host_apic_virtualization_in(sys.path()));

        let sys = tempfile::tempdir().unwrap();
        let intel = sys.path().join("kvm_intel/parameters");
        std::fs::create_dir_all(&intel).unwrap();
        std::fs::write(intel.join("enable_apicv"), "N\n").unwrap();
        assert!(!host_apic_virtualization_in(sys.path()));
        std::fs::write(intel.join("enable_apicv"), "Y\n").unwrap();
        assert!(host_apic_virtualization_in(sys.path()));
    }

    /// The ignore_msrs check reads the kvm module's parameter, and names the
    /// fix when it is off.
    #[test]
    fn host_msrs_off_is_named_with_its_fix() {
        let dir = tempfile::tempdir().unwrap();
        let params = dir.path().join("kvm/parameters");
        assert_eq!(host_msrs_problem_in(dir.path()), None, "no kvm module");
        std::fs::create_dir_all(&params).unwrap();
        std::fs::write(params.join("ignore_msrs"), "Y\n").unwrap();
        assert_eq!(host_msrs_problem_in(dir.path()), None);
        std::fs::write(params.join("ignore_msrs"), "N\n").unwrap();
        let said = host_msrs_problem_in(dir.path()).unwrap();
        assert!(said.contains("ignore_msrs = N"), "{said}");
        assert!(
            said.contains("options kvm ignore_msrs=1 report_ignored_msrs=0"),
            "{said}"
        );
        assert!(said.contains("/etc/modprobe.d/"), "{said}");
    }

    #[test]
    fn no_nics_means_nic_none() {
        let mut p = paths();
        p.nics.clear();
        let s = joined(&build_args("l", &arm(), &p, Accel::Tcg).unwrap());
        assert!(s.contains("-nic none"));
        assert!(s.contains("-accel tcg"));
        assert!(s.contains("-cpu max"));
    }

    #[test]
    fn jumbo_mtu_sets_host_mtu_on_virtio_only() {
        let mut vm = arm();
        assert!(vm.nic_model.starts_with("virtio-net"));
        let mut p = paths();
        p.nics[0].mtu = Some(9000);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("host_mtu=9000"), "{s}");

        // 1500 is the default — no host_mtu emitted even on virtio.
        p.nics[0].mtu = Some(1500);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("host_mtu"), "{s}");

        // Another model never gets host_mtu, even on a jumbo segment.
        vm.nic_model = "e1000".into();
        p.nics[0].mtu = Some(9000);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("host_mtu"), "{s}");
    }

    #[test]
    fn qemu_args_go_last() {
        let mut vm = arm();
        vm.qemu_args = vec!["-device".into(), "weird-thing".into()];
        let args = build_args("l", &vm, &paths(), Accel::Kvm).unwrap();
        assert_eq!(
            args[args.len() - 2..],
            ["-device".to_string(), "weird-thing".to_string()]
        );
    }

    #[test]
    fn gpu_vram_reaches_qemu_hostmem() {
        for mode in ["virgl", "vulkan"] {
            for (size, expected) in [(256 << 20, "256M"), (16 << 30, "16G"), (256 << 30, "256G")] {
                let mut vm = resolved_gpu(mode, "aarch64");
                vm.gpu.as_mut().unwrap().vram = Some(size);
                let s = joined(&build_args("l", &vm, &paths(), Accel::Kvm).unwrap());
                assert!(s.contains(&format!("blob=true,hostmem={expected}")), "{s}");
                assert_eq!(s.contains("venus=true"), mode == "vulkan");
            }
        }
    }

    /// `vulkan` on `virt` has no VGA form: the pure device and `-vga none`.
    #[test]
    fn vulkan_on_virt_is_the_pure_device_with_no_vga() {
        let s = joined(
            &build_args(
                "l",
                &resolved_gpu("vulkan", "aarch64"),
                &paths(),
                Accel::Tcg,
            )
            .unwrap(),
        );
        assert!(s.contains("-machine virt"), "{s}");
        assert!(
            s.contains("-device virtio-gpu-gl,venus=true,blob=true,hostmem=4G"),
            "{s}"
        );
        assert!(s.contains("-vga none"), "{s}");
        assert!(s.contains("-display egl-headless"), "{s}");
    }

    /// `virgl` gets the same one-adapter treatment, without the Venus
    /// options.
    #[test]
    fn virgl_is_one_gl_adapter_on_virt() {
        let s = joined(
            &build_args("l", &resolved_gpu("virgl", "aarch64"), &paths(), Accel::Tcg).unwrap(),
        );
        assert!(s.contains("-device virtio-gpu-gl "), "{s}");
        assert!(!s.contains("venus"), "{s}");
        assert!(s.contains("-vga none"), "{s}");
    }

    /// `passthrough` hands the guest the declared host GPU's address the
    /// lab daemon picked, and with none picked refuses rather than guess.
    #[test]
    fn passthrough_hands_over_the_picked_host_gpu() {
        let vm = resolved_gpu("passthrough", "aarch64");
        let mut p = paths();
        let e = build_args("l", &vm, &p, Accel::Kvm)
            .unwrap_err()
            .to_string();
        assert!(e.contains("needs a host GPU"), "{e}");
        p.gpu_passthrough = Some("0000:01:00.0".into());
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("-device vfio-pci,host=0000:01:00.0"), "{s}");
    }

    /// A data directory the lab daemon resolved becomes `-L`; without one
    /// the argv names none.
    #[test]
    fn a_qemu_data_dir_becomes_dash_l() {
        let vm = resolved_gpu("vulkan", "aarch64");
        let mut p = paths();
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("-L "), "{s}");
        p.qemu_data_dir = Some("/usr/share/qemu".into());
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains(" -L /usr/share/qemu "), "{s}");
    }

    /// A resolved host GPU puts its render node on the `egl-headless`
    /// display, so the scanout comes from the GPU that renders.
    #[test]
    fn a_resolved_render_node_lands_on_the_egl_display() {
        let mut p = paths();
        p.gpu_render_node = Some("/dev/dri/renderD129".into());
        for mode in ["virgl", "vulkan"] {
            let s =
                joined(&build_args("l", &resolved_gpu(mode, "aarch64"), &p, Accel::Kvm).unwrap());
            assert!(
                s.contains("-display egl-headless,rendernode=/dev/dri/renderD129"),
                "{s}"
            );
        }
    }

    /// QEMU is always headless with VNC on a socket; a `gui = true` window
    /// is a separate viewer the CLI launches (§11), never QEMU's own GTK.
    #[test]
    fn always_headless_with_vnc() {
        let s = joined(&build_args("l", &arm(), &paths(), Accel::Kvm).unwrap());
        assert!(s.contains("-display none"), "{s}");
        assert!(!s.contains("gtk"), "{s}");
        assert!(s.contains("-vnc unix:"), "{s}");
    }

    /// The aarch64 `virt` machine has no IDE/AHCI or default USB controller,
    /// so CD-ROMs ride virtio-blk, USB needs an explicit xHCI, and UEFI is
    /// mandatory (no SeaBIOS fallback). swtpm uses the MMIO tpm-tis-device.
    #[test]
    fn aarch64_virt_shape() {
        let mut vm = arm();
        vm.tpm = true;
        let mut p = paths();
        p.tpm_sock = Some("/run/l/t/tpm.sock".into());
        p.cdroms = vec!["/lab/.vmlab/media/cidata.iso".into()];
        let s = joined(&build_args("lab1", &vm, &p, Accel::Tcg).unwrap());
        assert!(s.contains("-machine virt"), "{s}");
        assert!(s.contains("if=pflash,format=raw,readonly=on"), "UEFI: {s}");
        assert!(
            s.contains("virtio-blk-pci,drive=cd0"),
            "CD-ROM must ride virtio on virt: {s}"
        );
        assert!(!s.contains("ide-"), "no IDE on virt: {s}");
        assert!(s.contains("qemu-xhci"), "explicit USB controller: {s}");
        assert!(!s.contains("-usb "), "no bare -usb on virt: {s}");
        assert!(s.contains("usb-tablet"), "{s}");
        assert!(s.contains("virtio-gpu-pci"), "{s}");
        assert!(s.contains("tpm-tis-device,tpmdev=tpm0"), "{s}");
        assert!(s.contains("virtio-blk-pci,drive=disk0"), "{s}");
        assert!(
            s.contains("virtserialport,chardev=vagent0,name=vmlab.agent.0"),
            "{s}"
        );
        assert!(s.ends_with("-S"), "{s}");
    }

    /// riscv64 shares the aarch64 `virt` path (virtio CD-ROMs, explicit
    /// xHCI, MMIO swtpm, mandatory UEFI) but pins `acpi=off` on the machine.
    #[test]
    fn riscv64_virt_shape() {
        let mut vm = resolved("linux-modern", "riscv64");
        vm.tpm = true;
        let mut p = paths();
        p.tpm_sock = Some("/run/l/t/tpm.sock".into());
        p.cdroms = vec!["/lab/.vmlab/media/cidata.iso".into()];
        let s = joined(&build_args("lab1", &vm, &p, Accel::Tcg).unwrap());
        assert!(s.contains("-machine virt,acpi=off"), "{s}");
        assert!(s.contains("if=pflash,format=raw,readonly=on"), "UEFI: {s}");
        assert!(
            s.contains("virtio-blk-pci,drive=cd0"),
            "CD-ROM must ride virtio on virt: {s}"
        );
        assert!(!s.contains("ide-"), "no IDE on virt: {s}");
        assert!(s.contains("qemu-xhci"), "explicit USB controller: {s}");
        assert!(!s.contains("-usb "), "no bare -usb on virt: {s}");
        assert!(s.contains("usb-tablet"), "{s}");
        assert!(s.contains("tpm-tis-device,tpmdev=tpm0"), "{s}");
        assert!(s.contains("virtio-blk-pci,drive=disk0"), "{s}");
    }

    /// `usb = true` (§5.2.1): `virt` names the xHCI it already has rather
    /// than adding a second, and `input = "usb"` adds a keyboard to it.
    #[test]
    fn usb_passthrough_controller_and_usb_input() {
        let p = paths();
        let mut vm = arm();
        assert!(!vm.usb, "off by default");
        let s = joined(&build_args("l", &vm, &p, Accel::Tcg).unwrap());
        assert!(s.contains("-device qemu-xhci "), "{s}");
        assert!(!s.contains("id=usbx"), "{s}");
        assert!(!s.contains("usb-kbd"), "{s}");
        vm.usb = true;
        vm.input = InputDevices::Usb;
        let s = joined(&build_args("l", &vm, &p, Accel::Tcg).unwrap());
        assert_eq!(
            s.matches("qemu-xhci").count(),
            1,
            "one controller on virt: {s}"
        );
        assert!(s.contains("-device qemu-xhci,id=usbx"), "{s}");
        assert!(s.contains("-device usb-kbd -device usb-tablet"), "{s}");
    }

    /// Disks and CD-ROMs are boot-indexed under SeaBIOS only; under UEFI the
    /// guest's own boot order decides. The disks are virtio-blk whatever the
    /// profile's bus.
    #[test]
    fn disks_are_virtio_and_boot_indexed_outside_uefi() {
        let mut vm = arm();
        let mut p = paths();
        p.extra_disks = vec![("data".into(), "/lab/.vmlab/t/disk-data.qcow2".into())];
        p.cdroms = vec!["/isos/installer.iso".into()];
        let s = joined(&build_args("l", &vm, &p, Accel::Tcg).unwrap());
        assert!(!s.contains("bootindex="), "{s}");
        vm.firmware = None;
        vm.disk_bus = crate::profiles::DiskBus::Sata;
        let s = joined(&build_args("l", &vm, &p, Accel::Tcg).unwrap());
        assert!(s.contains("virtio-blk-pci,drive=disk0,bootindex=50"), "{s}");
        assert!(s.contains("virtio-blk-pci,drive=disk1,bootindex=51"), "{s}");
        assert!(s.contains("virtio-blk-pci,drive=cd0,bootindex=0"), "{s}");
    }

    /// Only the install CD-ROM carries the qdev id `eject_cdrom()` addresses;
    /// built media and the bootstrap ISO stay unnamed, and a VM whose first
    /// CD-ROM is built media names none.
    #[test]
    fn only_the_install_cdrom_carries_the_eject_id() {
        let vm = arm();
        let mut p = paths();
        p.cdroms = vec![
            "/isos/installer.iso".into(),
            "/lab/.vmlab/media/vmlab.iso".into(),
        ];
        p.install_cdrom = true;
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("virtio-blk-pci,drive=cd0,id=install-cd"), "{s}");
        assert_eq!(s.matches("id=install-cd").count(), 1, "{s}");

        p.install_cdrom = false;
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("id=install-cd"), "{s}");
    }
}
