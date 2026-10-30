//! Build the QEMU argv for one VM (PRD §3, §5.2). Pure function of the
//! resolved hardware + runtime paths, so it is exhaustively unit-testable.
//! `qemu_args` from the lab file are appended verbatim, last, so they win.

use std::path::{Path, PathBuf};

use anyhow::Result;
use vmlab_agent_proto::PORT_NAME as AGENT_PORT_NAME;

use super::resolve::ResolvedVm;
use crate::config::model::{AccelMode, GpuMode, InputDevices, MacAddr};
use crate::profiles::{AgentTransport, DiskBus, FirmwareKind};

/// Per-VM runtime paths and attachments supplied by the lab daemon.
#[derive(Debug, Clone, Default)]
pub struct VmPaths {
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
    /// The host render node a `virgl`/`vulkan` GPU's `egl-headless` display
    /// renders on (`hostgpu::Sysfs::resolve`). Resolved by the lab daemon from
    /// sysfs; `None` leaves QEMU's choice.
    pub gpu_render_node: Option<PathBuf>,
    /// The host PCI address a `passthrough` GPU hands the guest: the
    /// declared host GPU the lab daemon picked (`hostgpu::render_plan`).
    pub gpu_passthrough: Option<String>,
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

/// The Hyper-V enlightenments `hyperv = true` adds to the KVM `-cpu` string
/// (§5.2): Windows reads its clock from the reference TSC page instead of an
/// emulated timer, and takes fewer exits for interrupts, timers, TLB flushes
/// and IPIs. `+invtsc` is deliberately absent — QEMU refuses to migrate a
/// guest that has it, and an online snapshot is a migration (§7.3).
pub const HYPERV_FLAGS: &str = "hv-relaxed,hv-vapic,hv-spinlocks=0x1fff,hv-vpindex,hv-runtime,\
                                hv-time,hv-synic,hv-stimer,hv-frequencies,hv-reset,hv-tlbflush,\
                                hv-ipi";

/// Added to [`HYPERV_FLAGS`] where the host's KVM virtualises the APIC
/// (§5.2). Hyper-V's SynIC has Windows acknowledge its timer interrupts by
/// AutoEOI, which KVM cannot do in hardware, so it turns APICv/AVIC off for
/// the whole VM; `hv-avic` tells Windows not to use AutoEOI and leaves them
/// on, so an end-of-interrupt stops being an exit. A Windows 11 24H2 or
/// Server 2025 guest takes 512 clock interrupts a second on every vCPU even
/// idle, so that is about one exit in three gone. Without APICv/AVIC the
/// guest would pay for an EOI AutoEOI used to save, so it is added only
/// where they are on.
pub const HYPERV_APICV_FLAG: &str = "hv-avic";

/// What AVIC also needs on an AMD host, set wherever [`HYPERV_APICV_FLAG`]
/// is: KVM inhibits AVIC while its in-kernel PIT re-injects missed ticks,
/// QEMU's default. A guest with the enlightenments keeps time from the
/// reference TSC page and has no use for PIT ticks it missed.
pub const HYPERV_APICV_PIT: &str = "kvm-pit.lost_tick_policy=discard";

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

/// The `-cpu` model under KVM. `host` passes the host CPU through, VMX/SVM
/// included, so an x86 guest that did not ask for `nested = true` has both
/// masked: the switch decides what the guest sees, not whatever the host
/// happens to allow (§5.2). Masking the other vendor's flag too is harmless,
/// and spares the builder asking which vendor the host is. Other arches pass
/// the host CPU through unchanged. A resolved `hyperv` adds
/// [`HYPERV_FLAGS`] on x86, with `nested` or without, and
/// [`HYPERV_APICV_FLAG`] where the host virtualises the APIC.
fn kvm_cpu_model(vm: &ResolvedVm, apic_virtualization: bool) -> String {
    let x86 = qemu_arch(&vm.arch) == "x86_64";
    let host = if vm.nested || !x86 {
        "host"
    } else {
        "host,vmx=off,svm=off"
    };
    match hyperv_apicv(vm, Accel::Kvm, apic_virtualization) {
        Some(true) => format!("{host},{HYPERV_FLAGS},{HYPERV_APICV_FLAG}"),
        Some(false) => format!("{host},{HYPERV_FLAGS}"),
        None => host.to_string(),
    }
}

/// Whether the enlightenments apply under `accel`, and if so whether
/// [`HYPERV_APICV_FLAG`] rides with them: `None` without them.
fn hyperv_apicv(vm: &ResolvedVm, accel: Accel, apic_virtualization: bool) -> Option<bool> {
    (vm.hyperv && hyperv_skipped(vm, accel).is_none()).then_some(apic_virtualization)
}

/// The OSK string the Apple SMC hands a macOS guest that asks for it
/// (§5.2). Public knowledge, the same in every Mac; the guest refuses to boot
/// without it, which is the only reason `apple_smc` exists.
pub const APPLE_SMC_OSK: &str = "<apple-smc-osk-redacted>";

/// The qdev id of the xHCI controller `input = "usb"` puts the keyboard and
/// tablet on (§5.2).
pub const INPUT_XHCI_ID: &str = "xhci";

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

pub fn emulator_binary(arch: &str) -> String {
    format!("qemu-system-{}", qemu_arch(arch))
}

/// Which IDE/AHCI bus and unit the n-th disk-or-CD-ROM takes.
///
/// Every attachment is addressed, never auto-placed. QEMU places an
/// unaddressed `ide-hd` on the secondary master (ide.1 unit 0) — the middle of
/// the four legacy slots — and its CD-ROM auto-placement does not advance past
/// the first bus, so the two together collide as soon as a guest carries a disk
/// and more than one ISO. That is every legacy Windows build: install media,
/// vmlab's bootstrap ISO, and (once the answer file grows drivers) an unattend
/// ISO too, which is exactly the four slots a legacy `pc` has.
///
/// q35's AHCI ports hold one unit each, so there the slot IS the bus.
fn ide_slot(slot: usize, machine: &MachineKind) -> (usize, usize) {
    match machine {
        MachineKind::Q35 => (slot, 0),
        // Two units to a bus, primary first: (0,0) (0,1) (1,0) (1,1).
        _ => (slot / 2, slot % 2),
    }
}

/// The machine shape the argv is being built for, as far as device addressing
/// cares: AHCI ports on q35, IDE buses on `pc`, neither on `virt`.
enum MachineKind {
    Q35,
    LegacyPc,
    Virt,
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
    if let Some(dir) = &paths.qemu_data_dir {
        arg(&mut a, "L", dir.display().to_string());
    }
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
    // A named model (§5.2) replaces the accelerator default under either
    // accelerator: it is the CPU the guest was declared to see. QEMU owns the
    // catalogue, so an unknown name is refused by QEMU at start.
    let (accel_name, default_cpu) = match accel {
        Accel::Kvm => ("kvm", kvm_cpu_model(vm, paths.apic_virtualization)),
        Accel::Tcg => ("tcg", "max".to_string()),
    };
    arg(&mut a, "accel", accel_name.into());
    let mut cpu = vm.cpu_model.clone().unwrap_or(default_cpu);
    // `tsc = "host"` (§5.2): an invariant TSC, pinned at the host's
    // frequency. `+invtsc` alone makes QEMU refuse every migration — and an
    // online snapshot is one (§7.3) — unless the frequency is explicit, so
    // the two only ever go together.
    if vm.tsc.is_some() && tsc_skipped(vm, accel).is_none() {
        let hz = paths.tsc_hz.ok_or_else(|| {
            anyhow::anyhow!("tsc = \"host\" needs the host TSC frequency (PRD §5.2)")
        })?;
        cpu.push_str(&format!(",+invtsc,tsc-frequency={hz}"));
    }
    arg(&mut a, "cpu", cpu);
    arg(&mut a, "smp", vm.cpus.to_string());
    arg(&mut a, "m", format!("{}M", vm.memory >> 20));
    // A fixed clock starts the RTC at the declared instant, and a guest that
    // reads the RTC as local time (the profile's `rtc`) has it start at the
    // host's (§7.3); otherwise QEMU's default, the host's UTC time, applies.
    if let Some(base) = &vm.rtc_base {
        arg(&mut a, "rtc", format!("base={base}"));
    }
    // AVIC stays on only without PIT tick re-injection (§5.2).
    if hyperv_apicv(vm, accel, paths.apic_virtualization) == Some(true) {
        arg(&mut a, "global", HYPERV_APICV_PIT.into());
    }
    // The Apple SMC (§5.2): macOS reads the OSK from it and refuses to boot
    // where there is none, and looks for an SMBIOS baseboard (type 2) table,
    // which QEMU only emits when asked.
    if vm.apple_smc {
        arg(
            &mut a,
            "device",
            format!("isa-applesmc,osk={APPLE_SMC_OSK}"),
        );
        arg(&mut a, "smbios", "type=2".into());
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
        match vm.agent_transport {
            // `-serial` claims ISA index 0 (COM1) before any `-device` is
            // placed, so where the agent needs COM1 the log is placed
            // explicitly on COM2 instead.
            AgentTransport::IsaSerial => {
                arg(
                    &mut a,
                    "chardev",
                    format!("file,id=serlog,path={}", log.display()),
                );
                arg(&mut a, "device", "isa-serial,chardev=serlog,index=1".into());
            }
            _ => arg(&mut a, "serial", format!("file:{}", log.display())),
        }
    }

    // UEFI firmware: CODE read-only pflash + per-VM writable VARS.
    if vm.firmware == Some(FirmwareKind::Ovmf) {
        let code = paths
            .firmware_code
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OVMF requires a firmware CODE path"))?;
        arg(
            &mut a,
            "drive",
            format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
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

    // Guest agent channel (§7.4): the vmlab-agent port (interactive
    // terminals / exec / files / net-info / shutdown — guest/agent-proto).
    // QEMU listens, the daemon connects — on the same socket whichever
    // guest-facing device carries it: the virtio-serial port a modern guest
    // drives, or a 16550 on COM1 for a guest with no virtio drivers, where
    // the legacy agent speaks the same protocol.
    if vm.agent_transport.has_channel() {
        arg(
            &mut a,
            "chardev",
            format!(
                "socket,id=vagent0,path={},server=on,wait=off",
                paths.agent_sock.display()
            ),
        );
    }
    match vm.agent_transport {
        AgentTransport::VirtioSerial => {
            arg(&mut a, "device", "virtio-serial-pci".into());
            arg(
                &mut a,
                "device",
                format!("virtserialport,chardev=vagent0,name={}", AGENT_PORT_NAME),
            );
        }
        AgentTransport::IsaSerial => {
            arg(
                &mut a,
                "device",
                "isa-serial,chardev=vagent0,index=0".into(),
            );
        }
        AgentTransport::None => {}
    }

    // Primary + extra disks: explicit blockdev node names (disk0, disk1, …)
    // so QMP snapshot commands can address them (§7.3).
    // SeaBIOS honours an explicit boot order; OVMF/UEFI boots removable media
    // on its own. Without it, SeaBIOS tries the (blank) disk and any floppy
    // before the install CD and stalls. So under SeaBIOS we give CD-ROMs the
    // lowest bootindex (install media boots first), disks the next, and leave
    // the answer-file floppy unindexed (never a boot candidate).
    let seabios = vm.firmware != Some(FirmwareKind::Ovmf);
    let machine_kind = if vm.machine.starts_with("virt") {
        MachineKind::Virt
    } else if vm.machine.starts_with("q35") {
        MachineKind::Q35
    } else {
        MachineKind::LegacyPc
    };
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
            // Addressed, never auto-placed: QEMU puts an unaddressed ide-hd on
            // the secondary master (ide.1 unit 0), in the middle of the four
            // slots, so every CD-ROM placed after it collides with something
            // nothing asked for. Disks take the slots from the front; the
            // CD-ROMs below carry on from where they stop.
            DiskBus::Ide | DiskBus::Sata => {
                let (bus_idx, unit) = ide_slot(disk_index, &machine_kind);
                a.push(format!(
                    "ide-hd,drive={node},bus=ide.{bus_idx},unit={unit}{boot}"
                ))
            }
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
        // The install CD-ROM gets a qdev id: QMP `eject` addresses a drive
        // by it, a `-blockdev` drive having no backend name to give.
        let id = if i == 0 && paths.install_cdrom {
            format!(",id={INSTALL_CDROM_ID}")
        } else {
            String::new()
        };
        if virt {
            a.push(format!("virtio-blk-pci,drive=cd{i}{boot}{id}"));
        } else {
            let (bus, unit) = ide_slot(sata_disks + i, &machine_kind);
            a.push(format!(
                "ide-cd,drive=cd{i},bus=ide.{bus},unit={unit}{boot}{id}"
            ));
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
                // One display adapter, the GL one. x86 machines take the
                // VGA-compatible form, which keeps firmware and the boot
                // console on it and stops q35/pc adding their default std
                // VGA; `virt` has no legacy VGA, so the pure device plus an
                // explicit `-vga none`. A second adapter in the guest leaves
                // Venus enumerating no device at all.
                let dev = match machine_kind {
                    MachineKind::Virt => "virtio-gpu-gl",
                    MachineKind::Q35 | MachineKind::LegacyPc => "virtio-vga-gl",
                };
                // QEMU refuses Venus without blob resources and a hostmem
                // window.
                let opts = if gpu.mode == GpuMode::Vulkan {
                    ",venus=true,blob=true,hostmem=4G"
                } else {
                    ""
                };
                arg(&mut a, "device", format!("{dev}{opts}"));
                if matches!(machine_kind, MachineKind::Virt) {
                    arg(&mut a, "vga", "none".into());
                }
                // The scanout has to come from the GPU Venus renders on, or
                // the console is black.
                let display = match &paths.gpu_render_node {
                    Some(node) => format!("egl-headless,rendernode={}", node.display()),
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
    // The `virt` machine has no default USB host controller, so `-usb` is a
    // no-op there and usb-tablet has nothing to plug into — add an explicit
    // xHCI controller. x86 q35/pc have a default controller via `-usb`.
    //
    // `usb = true` (§5.2.1) wants an xHCI with a known id for host devices
    // to hot-plug onto. `virt` already has one, so it only gains the id; x86
    // gains a second controller beside `-usb`'s. QEMU then places the
    // unaddressed tablet on the xHCI (verified on q35, QEMU 11.1) — one more
    // reason the flag is opt-in: the guest's USB topology changes with it.
    //
    // `input = "usb"` (§5.2) adds a USB keyboard beside the tablet, for a
    // guest that reads no PS/2 keyboard (macOS, and OpenCore before it). On
    // x86 both go on an xHCI of their own in place of `-usb`'s controller.
    // Neither is bound to the display: QMP `send-key` names no console, and
    // QEMU hands such an event to the newest *unbound* keyboard — the USB
    // one, which takes over from PS/2 when it is created — so binding it
    // would leave scripted keys on the PS/2 keyboard the guest ignores.
    let usbx = format!("qemu-xhci,id={}", crate::usb::CONTROLLER_ID);
    let usb_input = vm.input == InputDevices::Usb;
    if virt {
        let xhci = if vm.usb { usbx } else { "qemu-xhci".into() };
        arg(&mut a, "device", xhci);
        if usb_input {
            arg(&mut a, "device", "usb-kbd".into());
        }
        arg(&mut a, "device", "usb-tablet".into());
    } else if usb_input {
        arg(&mut a, "device", format!("qemu-xhci,id={INPUT_XHCI_ID}"));
        arg(&mut a, "device", format!("usb-kbd,bus={INPUT_XHCI_ID}.0"));
        arg(
            &mut a,
            "device",
            format!("usb-tablet,bus={INPUT_XHCI_ID}.0"),
        );
        if vm.usb {
            arg(&mut a, "device", usbx);
        }
    } else {
        a.push("-usb".into());
        if vm.usb {
            arg(&mut a, "device", usbx);
        }
        arg(&mut a, "device", "usb-tablet".into());
    }

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
            firmware_code: Some("/usr/share/edk2/x64/OVMF_CODE.4m.fd".into()),
            ovmf_vars: Some("/lab/.vmlab/t/OVMF_VARS.fd".into()),
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
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("memory-backend"), "{s}");
        assert!(!s.contains("vhost-user-fs"), "{s}");

        p.virtiofs_shares = vec![("mnt_src".into(), PathBuf::from("/run/l/t/vfs0.sock"))];
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("-machine q35,memory-backend=mem0"), "{s}");
        // 4 GiB: the linux-modern profile's memory, through the resolver.
        assert!(
            s.contains("memory-backend-memfd,id=mem0,size=4096M,share=on"),
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
        p.tpm_sock = Some("/run/l/t/tpm.sock".into());
        let vm = resolved("windows-11", "x86_64");
        let args = build_args("lab1", &vm, &p, Accel::Kvm).unwrap();
        let s = joined(&args);
        assert!(s.contains("-machine q35"));
        assert!(s.contains("-accel kvm"));
        assert!(s.contains("-cpu host"));
        // 4 vCPU / 8 GiB come from the windows-11 profile via the resolver.
        assert!(s.contains("-smp 4"), "{s}");
        assert!(s.contains("-m 8192M"), "{s}");
        // The CODE image is whatever the daemon discovered and injected —
        // which build the host ships is firmware.rs's business, not this
        // builder's, so these tests pass on a host with no edk2 at all.
        assert!(
            s.contains("if=pflash,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd"),
            "{s}"
        );
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

    /// Guests with no agent transport at all get no agent socket, no
    /// virtio-serial bus and no COM device — nothing for an agent to answer
    /// over. The serial log keeps its `-serial` shorthand on COM1.
    #[test]
    fn no_agent_transport_emits_no_agent_device() {
        let mut vm = resolved("windows-legacy", "x86_64");
        vm.agent_transport = AgentTransport::None;
        let mut p = paths();
        p.serial_log = Some("/logs/l/t/serial.log".into());
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("virtserialport"), "{s}");
        assert!(!s.contains("virtio-serial-pci"), "{s}");
        assert!(!s.contains("vmlab.agent.0"), "{s}");
        assert!(!s.contains("vagent0"), "{s}");
        assert!(!s.contains("isa-serial"), "{s}");
        assert!(s.contains("-serial file:/logs/l/t/serial.log"), "{s}");
    }

    /// The legacy agent's channel (§7.4): the same host socket a modern
    /// guest's virtio port lands on, carried by a 16550 on COM1 — and the
    /// serial log moved to COM2 so the two never contend for index 0.
    #[test]
    fn isa_serial_transport_puts_the_agent_on_com1_and_the_log_on_com2() {
        let vm = resolved("windows-xp", "x86_64");
        assert_eq!(vm.agent_transport, AgentTransport::IsaSerial);
        let mut p = paths();
        p.serial_log = Some("/logs/l/t/serial.log".into());
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(
            s.contains("socket,id=vagent0,path=/run/l/t/agent.sock,server=on,wait=off"),
            "{s}"
        );
        assert!(s.contains("isa-serial,chardev=vagent0,index=0"), "{s}");
        assert!(
            s.contains("file,id=serlog,path=/logs/l/t/serial.log"),
            "{s}"
        );
        assert!(s.contains("isa-serial,chardev=serlog,index=1"), "{s}");
        assert!(!s.contains("-serial file:"), "{s}");
        assert!(!s.contains("virtserialport"), "{s}");
        assert!(!s.contains("virtio-serial-pci"), "{s}");
    }

    /// `windows-legacy` (Vista/7/2008) keeps its virtio-serial channel: that
    /// era has virtio-win drivers, and its agent blocker is ConPTY, not the
    /// transport (decision Q1).
    #[test]
    fn windows_legacy_keeps_the_virtio_serial_channel() {
        let vm = resolved("windows-legacy", "x86_64");
        let s = joined(&build_args("l", &vm, &paths(), Accel::Kvm).unwrap());
        assert!(s.contains("-machine pc"));
        assert!(s.contains("ide-hd,drive=disk0"));
        assert!(
            s.contains("virtserialport,chardev=vagent0,name=vmlab.agent.0"),
            "{s}"
        );
        assert!(!s.contains("isa-serial"), "{s}");
    }

    #[test]
    fn tap_backend_emits_fd_netdev() {
        let vm = resolved("linux-modern", "x86_64");
        let mut p = paths();
        p.nics[0].backend = NicBackend::Tap { child_fd: 10 };
        p.nics[0].mtu = Some(9000);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("-netdev tap,id=net0,fd=10"), "{s}");
        assert!(!s.contains("addr.type=unix"), "{s}");
        // The virtio device line is backend-agnostic (host_mtu still works).
        assert!(s.contains("netdev=net0,mac=52:54:00:00:00:01,host_mtu=9000"));
    }

    /// `nested` is a real switch under KVM on x86: without it the guest
    /// sees neither VMX nor SVM, with it the host CPU passes through whole.
    #[test]
    fn nested_decides_whether_kvm_guests_see_vmx_svm() {
        let mut vm = resolved("linux-modern", "x86_64");
        assert!(!vm.nested);
        let args = build_args("l", &vm, &paths(), Accel::Kvm).unwrap();
        let cpu = args.iter().position(|a| a == "-cpu").unwrap();
        assert_eq!(args[cpu + 1], "host,vmx=off,svm=off");

        vm.nested = true;
        let args = build_args("l", &vm, &paths(), Accel::Kvm).unwrap();
        let cpu = args.iter().position(|a| a == "-cpu").unwrap();
        assert_eq!(args[cpu + 1], "host");

        // TCG keeps `max` either way.
        for nested in [false, true] {
            vm.nested = nested;
            let s = joined(&build_args("l", &vm, &paths(), Accel::Tcg).unwrap());
            assert!(s.contains("-cpu max "), "{s}");
        }
    }

    /// The flag comes from the vm block through the resolver.
    #[test]
    fn nested_resolves_from_the_vm_block() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let v = super::super::resolve::testing::vm(
            "vm \"t\" { template = \"scratch\" arch = \"x86_64\" profile = \"linux-modern\" disk = 1GiB nested = true }",
        );
        let vm = super::super::resolve::resolve_vm(&v, None, &profiles).unwrap();
        let s = joined(&build_args("l", &vm, &paths(), Accel::Kvm).unwrap());
        assert!(s.contains("-cpu host "), "{s}");
    }

    /// A fixed clock starts the RTC at the declared instant; any other clock
    /// leaves QEMU's default, the host's UTC time (§7.3).
    #[test]
    fn a_fixed_clock_sets_the_rtc_base() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let v = super::super::resolve::testing::vm(
            "vm \"t\" { template = \"scratch\" arch = \"x86_64\" profile = \"linux-generic\" disk = 1GiB clock = \"2020-01-01T00:00:00Z\" }",
        );
        let vm = super::super::resolve::resolve_vm(&v, None, &profiles).unwrap();
        let args = build_args("l", &vm, &paths(), Accel::Kvm).unwrap();
        let rtc = args.iter().position(|a| a == "-rtc").expect("-rtc");
        assert_eq!(args[rtc + 1], "base=2020-01-01T00:00:00");

        let host = resolved("linux-generic", "x86_64");
        let args = build_args("l", &host, &paths(), Accel::Kvm).unwrap();
        assert!(!args.iter().any(|a| a == "-rtc"), "{args:?}");
    }

    /// A profile whose guest reads the RTC as local time (#166) starts it at
    /// the host's: `base=localtime` for the host's clock, and a fixed instant
    /// as the host's wall clock at it. A Windows profile on the Rust agent
    /// keeps QEMU's UTC default — its agent tells the guest so.
    #[test]
    fn a_local_rtc_profile_starts_the_rtc_at_local_time() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let rtc_of = |vm: &super::super::resolve::ResolvedVm| {
            let args = build_args("l", vm, &paths(), Accel::Kvm).unwrap();
            args.iter()
                .position(|a| a == "-rtc")
                .map(|i| args[i + 1].clone())
        };
        for profile in ["windows-xp", "windows-9x"] {
            let host = resolved(profile, "x86_64");
            assert_eq!(
                rtc_of(&host).as_deref(),
                Some("base=localtime"),
                "{profile}"
            );
            let v = super::super::resolve::testing::vm(&format!(
                "vm \"t\" {{ template = \"scratch\" arch = \"x86_64\" profile = \"{profile}\" disk = 1GiB clock = \"2020-01-01T00:00:00Z\" }}"
            ));
            let fixed = super::super::resolve::resolve_vm(&v, None, &profiles).unwrap();
            let at = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z").unwrap();
            let local = at
                .with_timezone(&chrono::Local)
                .naive_local()
                .format("%Y-%m-%dT%H:%M:%S");
            assert_eq!(rtc_of(&fixed), Some(format!("base={local}")), "{profile}");
        }
        for profile in ["windows-11", "windows-server", "windows-legacy"] {
            assert_eq!(rtc_of(&resolved(profile, "x86_64")), None, "{profile}");
        }
    }

    /// A resolved `cpu_model` replaces the accelerator default under both
    /// KVM and TCG — the guest sees the CPU it was declared, not the host's
    /// (Windows 9x Setup on a modern host). Unset, both defaults stand.
    #[test]
    fn a_cpu_model_replaces_the_default_under_kvm_and_tcg() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let v = super::super::resolve::testing::vm(
            "vm \"t\" { template = \"scratch\" arch = \"x86\" profile = \"windows-9x\" disk = 1GiB cpu_model = \"pentium3\" }",
        );
        let vm = super::super::resolve::resolve_vm(&v, None, &profiles).unwrap();
        for accel in [Accel::Kvm, Accel::Tcg] {
            let args = build_args("l", &vm, &paths(), accel).unwrap();
            let cpus: Vec<&String> = args
                .iter()
                .enumerate()
                .filter(|(_, a)| *a == "-cpu")
                .map(|(i, _)| &args[i + 1])
                .collect();
            assert_eq!(cpus, ["pentium3"], "{accel:?}");
        }

        let unset = resolved("windows-9x", "x86");
        assert_eq!(unset.cpu_model, None);
        let kvm = build_args("l", &unset, &paths(), Accel::Kvm).unwrap();
        let cpu = kvm.iter().position(|a| a == "-cpu").unwrap();
        assert_eq!(kvm[cpu + 1], "host,vmx=off,svm=off");
        let tcg = build_args("l", &unset, &paths(), Accel::Tcg).unwrap();
        let cpu = tcg.iter().position(|a| a == "-cpu").unwrap();
        assert_eq!(tcg[cpu + 1], "max");
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
                "vm \"t\" {{ template = \"scratch\" arch = \"x86\" profile = \"windows-9x\" disk = 1GiB {accel} }}"
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

    /// A requested TCG emits `-accel tcg` with TCG's own `-cpu max` default,
    /// unless a `cpu_model` names another (Windows 95 on an AMD host).
    #[test]
    fn a_requested_tcg_builds_a_tcg_argv() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let build = |extra: &str| {
            let v = super::super::resolve::testing::vm(&format!(
                "vm \"t\" {{ template = \"scratch\" arch = \"x86\" profile = \"windows-9x\" disk = 1GiB accel = \"tcg\" {extra} }}"
            ));
            let vm = super::super::resolve::resolve_vm(&v, None, &profiles).unwrap();
            let plan = plan_accel(&vm, true).unwrap();
            joined(&build_args("l", &vm, &paths(), plan.accel()).unwrap())
        };
        let s = build("");
        assert!(s.contains("-accel tcg -cpu max "), "{s}");
        let s = build("cpu_model = \"pentium3\"");
        assert!(s.contains("-accel tcg -cpu pentium3 "), "{s}");
    }

    /// Every combination `hyperv` meets (§5.2): the flags ride the KVM
    /// `-cpu host` string on x86, with `nested` or without; TCG keeps `max`,
    /// a `cpu_model` keeps the model, a non-x86 guest keeps `host`, and each
    /// of those says why. `hyperv = false` (or unset) never adds them.
    #[test]
    fn hyperv_adds_the_enlightenments_under_kvm_on_the_host_cpu_only() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let resolve = |profile: &str, arch: &str, extra: &str| {
            let v = super::super::resolve::testing::vm(&format!(
                "vm \"t\" {{ template = \"scratch\" arch = \"{arch}\" profile = \"{profile}\" \
                 disk = 1GiB {extra} }}"
            ));
            super::super::resolve::resolve_vm(&v, None, &profiles).unwrap()
        };
        let cpu = |vm: &super::super::resolve::ResolvedVm, accel: Accel| {
            let args = build_args("l", vm, &paths(), accel).unwrap();
            let i = args.iter().position(|a| a == "-cpu").unwrap();
            args[i + 1].clone()
        };
        let flags = "hv-relaxed,hv-vapic,hv-spinlocks=0x1fff,hv-vpindex,hv-runtime,hv-time,\
                     hv-synic,hv-stimer,hv-frequencies,hv-reset,hv-tlbflush,hv-ipi";
        assert_eq!(HYPERV_FLAGS, flags);
        assert!(!HYPERV_FLAGS.contains("invtsc"));

        // The shipped profiles that set it, under KVM.
        for profile in [
            "windows-11",
            "windows-10",
            "windows-server",
            "windows-legacy",
        ] {
            for arch in ["x86_64", "x86"] {
                let vm = resolve(profile, arch, "");
                assert!(vm.hyperv, "{profile}");
                assert_eq!(
                    cpu(&vm, Accel::Kvm),
                    format!("host,vmx=off,svm=off,{flags}"),
                    "{profile} {arch}"
                );
                assert_eq!(hyperv_skipped(&vm, Accel::Kvm), None, "{profile}");
                // Under TCG: `max`, and the reason.
                assert_eq!(cpu(&vm, Accel::Tcg), "max", "{profile}");
                assert!(
                    hyperv_skipped(&vm, Accel::Tcg).is_some_and(|w| w.contains("TCG")),
                    "{profile}"
                );
            }
        }
        // With `nested`: the whole host CPU, plus the flags.
        let nested = resolve("windows-11", "x86_64", "nested = true");
        assert_eq!(cpu(&nested, Accel::Kvm), format!("host,{flags}"));
        assert_eq!(cpu(&nested, Accel::Tcg), "max");
        // A named model replaces the host CPU, flags and all, either way.
        let model = resolve("windows-11", "x86_64", "cpu_model = \"Skylake-Client\"");
        assert!(model.hyperv);
        assert_eq!(cpu(&model, Accel::Kvm), "Skylake-Client");
        assert_eq!(cpu(&model, Accel::Tcg), "Skylake-Client");
        assert!(hyperv_skipped(&model, Accel::Kvm).is_some_and(|w| w.contains("cpu_model")));
        // Opted out on the vm block: the old string, nothing to say.
        for extra in ["hyperv = false", "hyperv = false nested = true"] {
            let off = resolve("windows-server", "x86_64", extra);
            assert!(!off.hyperv);
            assert!(!cpu(&off, Accel::Kvm).contains("hv-"), "{extra}");
            assert_eq!(hyperv_skipped(&off, Accel::Kvm), None);
            assert_eq!(hyperv_skipped(&off, Accel::Tcg), None);
        }
        // Profiles that do not set it: no flags, nothing to say.
        for profile in ["windows-xp", "windows-9x", "linux-modern"] {
            let vm = resolve(profile, "x86_64", "");
            assert_eq!(cpu(&vm, Accel::Kvm), "host,vmx=off,svm=off", "{profile}");
            assert_eq!(hyperv_skipped(&vm, Accel::Tcg), None, "{profile}");
        }
        // Opted in where the profile does not: on.
        let xp = resolve("windows-xp", "x86_64", "hyperv = true");
        assert_eq!(
            cpu(&xp, Accel::Kvm),
            format!("host,vmx=off,svm=off,{flags}")
        );
        // Not x86: the host CPU unchanged, and the reason.
        let arm = resolve("linux-modern", "aarch64", "hyperv = true");
        assert_eq!(cpu(&arm, Accel::Kvm), "host");
        assert!(hyperv_skipped(&arm, Accel::Kvm).is_some_and(|w| w.contains("x86")));
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

    /// Non-x86 KVM has no VMX/SVM to mask.
    #[test]
    fn nested_masking_is_x86_only() {
        let vm = resolved("linux-modern", "aarch64");
        let args = build_args("l", &vm, &paths(), Accel::Kvm).unwrap();
        let cpu = args.iter().position(|a| a == "-cpu").unwrap();
        assert_eq!(args[cpu + 1], "host");
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

    /// With the host's APIC virtualised, an enlightened guest gains `hv-avic`
    /// and the PIT stops re-injecting, together and only together (§5.2);
    /// the host fact changes nothing for a guest the enlightenments skip.
    #[test]
    fn apic_virtualization_adds_hv_avic_and_drops_pit_reinjection_for_hyperv_only() {
        let profiles = crate::profiles::ProfileSet::shipped().unwrap();
        let resolve = |profile: &str, arch: &str, extra: &str| {
            let v = super::super::resolve::testing::vm(&format!(
                "vm \"t\" {{ template = \"scratch\" arch = \"{arch}\" profile = \"{profile}\" \
                 disk = 1GiB {extra} }}"
            ));
            super::super::resolve::resolve_vm(&v, None, &profiles).unwrap()
        };
        let argv = |vm: &super::super::resolve::ResolvedVm, accel: Accel, apicv: bool| {
            let p = VmPaths {
                apic_virtualization: apicv,
                ..paths()
            };
            build_args("l", vm, &p, accel).unwrap()
        };
        let cpu = |args: &[String]| {
            let i = args.iter().position(|a| a == "-cpu").unwrap();
            args[i + 1].clone()
        };
        let pit = |args: &[String]| {
            args.windows(2)
                .filter(|w| w[0] == "-global" && w[1] == HYPERV_APICV_PIT)
                .count()
        };
        assert_eq!(HYPERV_APICV_FLAG, "hv-avic");
        assert_eq!(HYPERV_APICV_PIT, "kvm-pit.lost_tick_policy=discard");

        for profile in ["windows-11", "windows-server", "windows-legacy"] {
            let vm = resolve(profile, "x86_64", "");
            let on = argv(&vm, Accel::Kvm, true);
            assert_eq!(
                cpu(&on),
                format!("host,vmx=off,svm=off,{HYPERV_FLAGS},hv-avic"),
                "{profile}"
            );
            assert_eq!(pit(&on), 1, "{profile}");
            // Without APICv/AVIC: the flags alone, the PIT left as it was.
            let off = argv(&vm, Accel::Kvm, false);
            assert_eq!(
                cpu(&off),
                format!("host,vmx=off,svm=off,{HYPERV_FLAGS}"),
                "{profile}"
            );
            assert_eq!(pit(&off), 0, "{profile}");
            // Under TCG the enlightenments are skipped, and so is this.
            let tcg = argv(&vm, Accel::Tcg, true);
            assert_eq!(cpu(&tcg), "max");
            assert_eq!(pit(&tcg), 0);
        }
        // `nested` keeps it; a named model, `hyperv = false`, a profile
        // without the enlightenments and a non-x86 guest get neither.
        let nested = argv(
            &resolve("windows-11", "x86_64", "nested = true"),
            Accel::Kvm,
            true,
        );
        assert_eq!(cpu(&nested), format!("host,{HYPERV_FLAGS},hv-avic"));
        assert_eq!(pit(&nested), 1);
        for (profile, arch, extra) in [
            ("windows-11", "x86_64", "cpu_model = \"Skylake-Client\""),
            ("windows-server", "x86_64", "hyperv = false"),
            ("windows-xp", "x86_64", ""),
            ("linux-modern", "x86_64", ""),
            ("linux-modern", "aarch64", "hyperv = true"),
        ] {
            let args = argv(&resolve(profile, arch, extra), Accel::Kvm, true);
            assert!(!cpu(&args).contains("hv-avic"), "{profile} {extra}");
            assert_eq!(pit(&args), 0, "{profile} {extra}");
        }
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

    #[test]
    fn no_nics_means_nic_none() {
        let vm = resolved("linux-modern", "x86_64");
        let mut p = paths();
        p.nics.clear();
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
        p.nics[0].mtu = Some(9000);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("host_mtu=9000"), "{s}");

        // 1500 is the default — no host_mtu emitted even on virtio.
        let mut p = paths();
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

    /// `vulkan` on q35 is the VGA-compatible Venus device with blob
    /// resources, so q35 adds no std VGA beside it.
    #[test]
    fn vulkan_on_q35_is_one_vga_venus_device_with_blob() {
        let s = joined(
            &build_args("l", &resolved_gpu("vulkan", "x86_64"), &paths(), Accel::Kvm).unwrap(),
        );
        assert!(s.contains("-machine q35"), "{s}");
        assert!(
            s.contains("-device virtio-vga-gl,venus=true,blob=true,hostmem=4G"),
            "{s}"
        );
        assert!(!s.contains("virtio-gpu-gl"), "{s}");
        assert!(!s.contains("-vga none"), "{s}");
        assert!(s.contains("-display egl-headless"), "{s}");
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
    }

    /// `virgl` gets the same one-adapter treatment, without the Venus
    /// options.
    #[test]
    fn virgl_is_one_gl_adapter_on_q35_and_virt() {
        let s = joined(
            &build_args("l", &resolved_gpu("virgl", "x86_64"), &paths(), Accel::Kvm).unwrap(),
        );
        assert!(s.contains("-device virtio-vga-gl "), "{s}");
        assert!(!s.contains("venus"), "{s}");
        assert!(!s.contains("-vga none"), "{s}");
        let s = joined(
            &build_args("l", &resolved_gpu("virgl", "aarch64"), &paths(), Accel::Tcg).unwrap(),
        );
        assert!(s.contains("-device virtio-gpu-gl "), "{s}");
        assert!(s.contains("-vga none"), "{s}");
    }

    /// `passthrough` hands the guest the declared host GPU's address the
    /// lab daemon picked, and with none picked refuses rather than guess.
    #[test]
    fn passthrough_hands_over_the_picked_host_gpu() {
        let vm = resolved_gpu("passthrough", "x86_64");
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
        let vm = resolved_gpu("vulkan", "x86_64");
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
                joined(&build_args("l", &resolved_gpu(mode, "x86_64"), &p, Accel::Kvm).unwrap());
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
        let vm = resolved("linux-modern", "x86_64");
        let p = paths();
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

    /// The `macos` profile's argv (§5.2) is Phase 0's working command line,
    /// with vmlab's own ids, paths and plumbing around it: the Skylake model
    /// with the VMware leaf and an invariant TSC pinned at the frequency the
    /// lab daemon handed in, the Apple SMC and an SMBIOS type 2 table, OVMF
    /// without secure boot, AHCI disks, VMware SVGA, a USB keyboard and
    /// tablet on their own xHCI, virtio-net and the agent's virtio-serial
    /// port, and no TPM.
    #[test]
    fn macos_shape_is_phase_0s_command_line() {
        let vm = resolved("macos", "x86_64");
        let mut p = paths();
        p.extra_disks = vec![("oc".into(), "/lab/.vmlab/t/disk-oc.qcow2".into())];
        p.serial_log = Some("/logs/l/t/serial.log".into());
        p.tsc_hz = Some(4_291_956_000);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert_eq!(
            s,
            "-name vmlab:l/t -machine q35 -accel kvm \
             -cpu Skylake-Client-v4,-hle,-rtm,kvm=on,vendor=GenuineIntel,vmware-cpuid-freq=on,\
             +invtsc,tsc-frequency=4291956000 \
             -smp 4 -m 8192M \
             -device isa-applesmc,osk=<apple-smc-osk-redacted> \
             -smbios type=2 \
             -display none -vnc unix:/run/l/t/vnc.sock,share=ignore \
             -qmp unix:/run/l/t/qmp.sock,server=on,wait=off -monitor none \
             -serial file:/logs/l/t/serial.log \
             -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd \
             -drive if=pflash,format=raw,file=/lab/.vmlab/t/OVMF_VARS.fd \
             -chardev socket,id=vagent0,path=/run/l/t/agent.sock,server=on,wait=off \
             -device virtio-serial-pci \
             -device virtserialport,chardev=vagent0,name=vmlab.agent.0 \
             -blockdev driver=qcow2,node-name=disk0,file.driver=file,file.filename=/lab/.vmlab/t/disk0.qcow2 \
             -device ide-hd,drive=disk0,bus=ide.0,unit=0 \
             -blockdev driver=qcow2,node-name=disk1,file.driver=file,file.filename=/lab/.vmlab/t/disk-oc.qcow2 \
             -device ide-hd,drive=disk1,bus=ide.1,unit=0 \
             -netdev stream,id=net0,server=off,addr.type=unix,addr.path=/run/l/t/nic0.sock \
             -device virtio-net-pci,netdev=net0,mac=52:54:00:00:00:01 \
             -device vmware-svga \
             -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0 \
             -S"
        );
        // Unaddressed PCI devices on q35 land on pcie.0 — where QEMU keeps a
        // virtio device transitional, the mode AppleVirtIO drives.
        assert!(!s.contains("pcie-root-port"), "{s}");
    }

    /// `tsc = "host"` needs the frequency the lab daemon reads; under TCG,
    /// where there is no KVM to read it from, it is left off and said.
    #[test]
    fn a_pinned_tsc_needs_kvm_and_a_frequency() {
        let vm = resolved("macos", "x86_64");
        let mut p = paths();
        let err = build_args("l", &vm, &p, Accel::Kvm).unwrap_err();
        assert!(err.to_string().contains("host TSC frequency"), "{err}");
        p.tsc_hz = Some(3_000_000_000);
        let s = joined(&build_args("l", &vm, &p, Accel::Tcg).unwrap());
        assert!(!s.contains("invtsc") && !s.contains("tsc-frequency"), "{s}");
        let said = tsc_not_applied(&vm, Accel::Tcg).unwrap();
        assert!(said.contains("tsc = \"host\" is not applied"), "{said}");
        assert_eq!(tsc_not_applied(&vm, Accel::Kvm), None);
        assert_eq!(
            tsc_not_applied(&resolved("linux-modern", "x86_64"), Accel::Tcg),
            None
        );
        // On the host CPU too: the pin follows the accelerator default.
        let mut host = resolved("linux-modern", "x86_64");
        host.tsc = Some(crate::config::model::TscMode::Host);
        let s = joined(&build_args("l", &host, &p, Accel::Kvm).unwrap());
        assert!(
            s.contains("-cpu host,vmx=off,svm=off,+invtsc,tsc-frequency=3000000000"),
            "{s}"
        );
    }

    /// `input = "usb"` beside `usb = true`: the input xHCI and the
    /// passthrough one are separate controllers, and `-usb` is not used.
    #[test]
    fn usb_input_and_usb_passthrough_are_two_controllers() {
        let mut vm = resolved("macos", "x86_64");
        vm.usb = true;
        let mut p = paths();
        p.tsc_hz = Some(1);
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("-device qemu-xhci,id=xhci "), "{s}");
        assert!(s.contains("-device qemu-xhci,id=usbx"), "{s}");
        assert!(!s.contains("-usb "), "{s}");
        // Without `apple_smc`, no SMC and no SMBIOS table.
        vm.apple_smc = false;
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("applesmc") && !s.contains("-smbios"), "{s}");
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

    /// `usb = true` (§5.2.1): an xHCI with the fixed id host devices
    /// hot-plug onto. q35 gains it beside `-usb`; `virt` names the xHCI it
    /// already has rather than adding a second.
    #[test]
    fn usb_passthrough_controller() {
        let p = paths();
        let mut vm = resolved("linux-modern", "x86_64");
        assert!(!vm.usb, "off by default");
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("qemu-xhci"), "no controller unasked: {s}");
        assert!(s.contains("-usb "), "{s}");

        vm.usb = true;
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("-device qemu-xhci,id=usbx"), "{s}");
        assert!(s.contains("-usb "), "the default controllers stay: {s}");

        let mut vm = resolved("linux-modern", "aarch64");
        let s = joined(&build_args("l", &vm, &p, Accel::Tcg).unwrap());
        assert!(s.contains("-device qemu-xhci "), "{s}");
        assert!(!s.contains("id=usbx"), "{s}");
        vm.usb = true;
        let s = joined(&build_args("l", &vm, &p, Accel::Tcg).unwrap());
        assert_eq!(
            s.matches("qemu-xhci").count(),
            1,
            "one controller on virt: {s}"
        );
        assert!(s.contains("-device qemu-xhci,id=usbx"), "{s}");
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
        p.cdroms = vec![
            "/isos/installer.iso".into(),
            "/lab/.vmlab/media/cidata.iso".into(),
        ];
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("ide-cd,drive=cd0,bus=ide.0"), "{s}");
        assert!(s.contains("ide-cd,drive=cd1,bus=ide.1"), "{s}");
    }

    /// A legacy `pc` guest fills its four IDE slots in order: the disk, then
    /// every CD-ROM. Nothing is auto-placed — QEMU drops an unaddressed
    /// `ide-hd` on the secondary master, mid-list, and its CD auto-placement
    /// will not advance past the first bus, so the pair collide with "IDE unit
    /// N is in use" and refuse the whole command line.
    #[test]
    fn legacy_pc_fills_its_four_ide_slots_in_order() {
        let vm = resolved("windows-legacy", "x86_64");
        assert!(!vm.machine.starts_with("q35"), "profile must be legacy pc");
        let mut p = paths();
        // The Option 3 shape: install media, unattend+drivers, bootstrap ISO.
        p.cdroms = vec![
            "/isos/installer.iso".into(),
            "/lab/.vmlab/media/unattend.iso".into(),
            "/lab/.vmlab/media/vmlab.iso".into(),
        ];
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(s.contains("ide-hd,drive=disk0,bus=ide.0,unit=0"), "{s}");
        assert!(s.contains("ide-cd,drive=cd0,bus=ide.0,unit=1"), "{s}");
        assert!(s.contains("ide-cd,drive=cd1,bus=ide.1,unit=0"), "{s}");
        assert!(s.contains("ide-cd,drive=cd2,bus=ide.1,unit=1"), "{s}");
    }

    /// Only the install CD-ROM carries the qdev id `eject_cdrom()` addresses;
    /// built media and the bootstrap ISO stay unnamed, and a VM whose first
    /// CD-ROM is built media names none.
    #[test]
    fn only_the_install_cdrom_carries_the_eject_id() {
        let vm = resolved("windows-legacy", "x86_64");
        let mut p = paths();
        p.cdroms = vec![
            "/isos/installer.iso".into(),
            "/lab/.vmlab/media/vmlab.iso".into(),
        ];
        p.install_cdrom = true;
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(
            s.contains("ide-cd,drive=cd0,bus=ide.0,unit=1,bootindex=0,id=install-cd"),
            "{s}"
        );
        assert_eq!(s.matches("id=install-cd").count(), 1, "{s}");

        p.install_cdrom = false;
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        assert!(!s.contains("id=install-cd"), "{s}");
    }

    /// The disk is addressed on q35 too, where a slot is a whole AHCI port.
    #[test]
    fn q35_addresses_the_disk_and_cdroms_by_port() {
        let vm = resolved("windows-server", "x86_64");
        let mut p = paths();
        p.cdroms = vec!["/isos/installer.iso".into()];
        let s = joined(&build_args("l", &vm, &p, Accel::Kvm).unwrap());
        // windows-server is virtio-disk, so the CD-ROM starts at port 0.
        assert!(s.contains("ide-cd,drive=cd0,bus=ide.0,unit=0"), "{s}");
    }
}
