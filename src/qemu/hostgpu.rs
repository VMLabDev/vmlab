//! The host GPU a paravirtual `gpu {}` renders on (PRD §5.2).
//!
//! `virgl` and `vulkan` render host-side, and on a host with more than one
//! GPU both halves of that rendering have to land on the same one: the
//! `egl-headless` display that scans the guest out for VNC, and the Vulkan
//! driver `virglrenderer`'s Venus contexts load. The first is a `rendernode=`
//! on the display; the second is a Vulkan loader filter in the emulator's own
//! environment, because `virglrenderer` forwards every physical device the
//! loader returns.
//!
//! Which GPU is a fact about one host, so a lab never says: the host config
//! declares its GPUs by name (`gpu "AMD1" { address = … }`), one of them the
//! default, and a lab's `gpu {}` names one with `host_gpu` or takes the
//! default ([`pick`]). The picked GPU's PCI address is resolved through
//! sysfs: the device's `drm/` directory holds its render node, its `driver`
//! link the kernel driver, its `power/` directory the runtime power state.
//! The sysfs root is a value so the resolution is testable on any host.
//!
//! Runtime power management is read here too, because it decides two
//! things before a VM starts: a GPU whose runtime state is `error` is
//! refused, since the driver has lost it and QEMU would abort on it some
//! seconds after the VM was reported up; and an NVIDIA GPU that can
//! runtime-suspend earns a warning and, under its `keepawake`, the
//! supervisor's helper that holds it awake ([`keepawake_wanted`],
//! ADR-0018).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::process::ChildEnv;
use super::vk_layer;
use crate::config::host::{HostConfig, HostGpu, KeepAwake};
use crate::config::hypervisor::HypervisorKind;
use crate::config::model::{Gpu, GpuMode};

/// One host GPU with a render node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderGpu {
    /// PCI address, `0000:72:00.0`.
    pub address: String,
    /// The render node, `/dev/dri/renderD129`.
    pub render_node: PathBuf,
    /// The kernel driver bound to the device, `amdgpu`; `None` when unbound.
    pub driver: Option<String>,
}

/// A host GPU's runtime power management, as sysfs and its driver report
/// it. Every field is `None` where the file is missing or unreadable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Power {
    /// `power/runtime_status`: `active`, `suspended`, `resuming`,
    /// `suspending`, `error` or `unsupported`.
    pub runtime_status: Option<String>,
    /// `power/control`: `auto` lets the device runtime-suspend, `on` keeps
    /// it powered.
    pub control: Option<String>,
    /// Whether the NVIDIA driver runs runtime D3 on the GPU, from the
    /// `Runtime D3 status:` line of `/proc/driver/nvidia/gpus/<addr>/power`.
    /// `None` for another driver.
    pub runtime_d3: Option<bool>,
}

impl Power {
    /// Whether the driver has lost the device: a resume failed and the
    /// kernel stopped trying (`runtime_status` `error`).
    pub fn failed(&self) -> bool {
        self.runtime_status.as_deref() == Some("error")
    }

    /// Whether the device can runtime-suspend while nothing uses it:
    /// `power/control` is `auto` and the driver does not say runtime D3 is
    /// off. The NVIDIA driver leaves `control` at `auto` on a desktop card
    /// whose runtime D3 it has disabled, so `control` alone overstates it.
    pub fn can_suspend(&self) -> bool {
        self.control.as_deref() == Some("auto") && self.runtime_d3 != Some(false)
    }
}

/// The sysfs tree host GPUs are read from: `/sys` on a real host, a fake tree
/// in tests. `proc` is where the NVIDIA driver's own `driver/nvidia/` files
/// are read from: `/proc` on a real host, `<root>/proc` in a fake tree.
#[derive(Debug, Clone)]
pub struct Sysfs {
    root: PathBuf,
    proc: PathBuf,
}

impl Sysfs {
    /// A fake tree at `root`, its `proc` under it.
    #[cfg(test)]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            proc: root.join("proc"),
            root,
        }
    }

    /// The host's own `/sys` and `/proc`.
    pub fn host() -> Self {
        Self {
            root: "/sys".into(),
            proc: "/proc".into(),
        }
    }

    /// The device's runtime power management.
    pub fn power(&self, address: &str) -> Power {
        let dev = self.devices().join(address);
        let read = |p: PathBuf| {
            std::fs::read_to_string(p)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let runtime_d3 = read(
            self.proc
                .join("driver/nvidia/gpus")
                .join(address)
                .join("power"),
        )
        .and_then(|text| nvidia_runtime_d3(&text));
        Power {
            runtime_status: read(dev.join("power/runtime_status")),
            control: read(dev.join("power/control")),
            runtime_d3,
        }
    }

    fn devices(&self) -> PathBuf {
        self.root.join("bus/pci/devices")
    }

    /// Whether a PCI device answers at this address — all `passthrough`
    /// asks of sysfs, since a card bound to `vfio-pci` has no render node.
    pub fn device(&self, address: &str) -> Result<()> {
        let dev = self.devices().join(address);
        if !dev.exists() {
            bail!(
                "gpu address {address}: no such PCI device (no {})",
                dev.display()
            );
        }
        Ok(())
    }

    /// The other functions of the PCI slot `address` sits in, each with the
    /// driver it is bound to (`None`: no driver), in function order — a
    /// graphics card's HDMI/DP audio function above all. Empty when
    /// `address` is not a PCI address.
    pub fn sibling_functions(&self, address: &str) -> Vec<(String, Option<String>)> {
        let Some((slot, own)) = address.rsplit_once('.') else {
            return Vec::new();
        };
        (0..8)
            .map(|function| format!("{slot}.{function}"))
            .filter(|sibling| sibling.rsplit_once('.').is_some_and(|(_, f)| f != own))
            .filter_map(|sibling| {
                let dev = self.devices().join(&sibling);
                dev.exists().then(|| {
                    let driver = driver(&dev);
                    (sibling, driver)
                })
            })
            .collect()
    }

    /// Resolve a PCI address to its render node and driver. An address with
    /// no device, a device with no render node, or one in a failed power
    /// state is an error naming it.
    pub fn resolve(&self, address: &str) -> Result<RenderGpu> {
        self.device(address)?;
        let dev = self.devices().join(address);
        if self.power(address).failed() {
            bail!(
                "gpu address {address}: the device is in a failed power state \
                 (power/runtime_status=error) — its driver lost it on a resume, \
                 and it needs a reboot before a VM can render on it"
            );
        }
        match render_node(&dev) {
            Some(node) => Ok(RenderGpu {
                address: address.to_string(),
                render_node: node,
                driver: driver(&dev),
            }),
            None => bail!(
                "gpu address {address}: the device has no DRM render node \
                 (no {}/drm/renderD*) — is its driver loaded?",
                dev.display()
            ),
        }
    }

    /// Every PCI device with a render node, ordered by render node.
    pub fn render_gpus(&self) -> Vec<RenderGpu> {
        let Ok(entries) = std::fs::read_dir(self.devices()) else {
            return Vec::new();
        };
        let mut gpus: Vec<RenderGpu> = entries
            .flatten()
            .filter_map(|e| {
                let dev = e.path();
                Some(RenderGpu {
                    address: e.file_name().to_string_lossy().into_owned(),
                    render_node: render_node(&dev)?,
                    driver: driver(&dev),
                })
            })
            .collect();
        gpus.sort_by(|a, b| a.render_node.cmp(&b.render_node));
        gpus
    }
}

/// `/dev/dri/renderDN` for the first `renderD*` under the device's `drm/`.
fn render_node(dev: &Path) -> Option<PathBuf> {
    let mut nodes: Vec<String> = std::fs::read_dir(dev.join("drm"))
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("renderD"))
        .collect();
    nodes.sort();
    nodes
        .into_iter()
        .next()
        .map(|n| Path::new("/dev/dri").join(n))
}

/// The NVIDIA driver's runtime D3 setting from its per-GPU `power` file:
/// `Runtime D3 status:  Enabled (fine-grained)` or `Disabled by default`.
fn nvidia_runtime_d3(text: &str) -> Option<bool> {
    let value = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Runtime D3 status:"))?
        .trim();
    if value.starts_with("Enabled") {
        Some(true)
    } else if value.starts_with("Disabled") {
        Some(false)
    } else {
        None
    }
}

/// Whether the supervisor should hold a GPU awake (ADR-0018): always under
/// `on`, never under `off`, and under `auto` only for an NVIDIA GPU that
/// can runtime-suspend — the driver whose runtime-D3 resume has lost GPUs
/// until reboot (Xid 119, a GSP that does not come back).
pub fn keepawake_wanted(mode: KeepAwake, gpu: &RenderGpu, power: &Power) -> bool {
    match mode {
        KeepAwake::On => true,
        KeepAwake::Off => false,
        KeepAwake::Auto => gpu.driver.as_deref() == Some("nvidia") && power.can_suspend(),
    }
}

/// The basename of the device's `driver` link.
fn driver(dev: &Path) -> Option<String> {
    let target = std::fs::read_link(dev.join("driver")).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// The Vulkan loader variables vmlab owns in the emulator's environment,
/// with the PCI-filter layer's own ([`vk_layer`]). Every one is removed
/// before anything is set, so nothing a supervisor was started with leaks
/// into a guest's renderer. `MESA_VK_DEVICE_SELECT` is in the list but never
/// set: Mesa's device-select layer breaks Venus
/// (`vkr: venus driver is required to call vkEnumeratePhysicalDevices first`).
pub const VULKAN_ENV_VARS: [&str; 8] = [
    "VK_DRIVER_FILES",
    "VK_ICD_FILENAMES",
    "VK_LOADER_DRIVERS_SELECT",
    "MESA_VK_DEVICE_SELECT",
    vk_layer::PATH_VAR,
    vk_layer::ADDRESS_VAR,
    vk_layer::DISABLE_VAR,
    vk_layer::HIDE_UNCACHED_VAR,
];

/// The emulator's environment for a VM (§5.2): the Vulkan loader
/// variables always cleared, and under `vulkan` a filter that leaves the
/// loader only the selected GPU's driver. An explicit `vulkan_icd` wins over
/// the driver mapping; a GPU whose driver maps to no ICD, or no GPU at all,
/// gets no filter. A GPU with an address under `vulkan` also gets the
/// PCI-filter layer from `layer_dir`, which hides every device but that
/// address's from the guest — the driver filter alone cannot tell two
/// identical cards apart.
pub fn emulator_env(
    vulkan: bool,
    gpu: Option<&RenderGpu>,
    vulkan_icd: Option<&Path>,
    layer_dir: Option<&Path>,
) -> ChildEnv {
    let mut env = ChildEnv {
        remove: VULKAN_ENV_VARS.iter().map(|v| v.to_string()).collect(),
        set: Vec::new(),
    };
    if !vulkan {
        return env;
    }
    if let Some(icd) = vulkan_icd {
        env.set
            .push(("VK_DRIVER_FILES".into(), icd.display().to_string()));
    } else if let Some(select) = gpu
        .and_then(|g| g.driver.as_deref())
        .and_then(loader_driver_select)
    {
        env.set
            .push(("VK_LOADER_DRIVERS_SELECT".into(), select.into()));
    }
    if let (Some(gpu), Some(dir)) = (gpu, layer_dir) {
        env.set
            .push((vk_layer::PATH_VAR.into(), dir.display().to_string()));
        env.set
            .push((vk_layer::ADDRESS_VAR.into(), gpu.address.clone()));
    }
    env
}

/// The host GPU a `gpu {}` resolves to on this host (§5.2), before sysfs is
/// asked about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picked<'h> {
    /// `None` when nothing resolves: no host GPU declared, or the name
    /// missing and no default to fall back on.
    pub gpu: Option<&'h HostGpu>,
    /// Set when the lab named a host GPU this host does not declare and a
    /// paravirtual mode fell back rather than refusing.
    pub missing: Option<&'h str>,
}

impl Picked<'_> {
    /// What to tell the user about a fallback, once.
    pub fn warning(&self) -> Option<String> {
        let wanted = self.missing?;
        Some(match self.gpu {
            Some(used) => format!(
                "host GPU `{wanted}` is not declared on this host; using the default `{}`",
                used.name
            ),
            None => format!(
                "host GPU `{wanted}` is not declared on this host, and it has no default; \
                 rendering on the GPU QEMU finds first"
            ),
        })
    }
}

/// The declared names, for a refusal that has to say what there is.
fn declared(host: &HostConfig) -> String {
    if host.gpus.is_empty() {
        return "none".into();
    }
    host.gpus
        .iter()
        .map(|g| format!("`{}`", g.name))
        .collect::<Vec<_>>()
        .join(", ")
}

const DECLARE_ONE: &str =
    "declare `gpu \"<name>\" { address = ... }` in ~/.config/vmlab/config.wcl";

/// Pick a `gpu {}`'s host GPU (PRD §5.2): the one `host_gpu` names, matched
/// case-insensitively, else the host's default. A name this host lacks
/// falls back to the default under `virgl`/`vulkan`, which only render
/// there; `passthrough` refuses, because handing the guest a different card
/// takes that card away from the host. `passthrough` with nothing to pick
/// refuses too.
pub fn pick<'h>(gpu: &'h Gpu, host: &'h HostConfig) -> Result<Picked<'h>> {
    let passthrough = gpu.mode == GpuMode::Passthrough;
    let picked = match gpu.host_gpu.as_deref() {
        Some(name) => match host.gpu(name) {
            Some(found) => Picked {
                gpu: Some(found),
                missing: None,
            },
            None if passthrough => bail!(
                "passthrough host GPU `{name}` is not declared on this host (declared: {}) — \
                 passthrough never falls back to another card, which would take it from the host; \
                 {DECLARE_ONE}",
                declared(host)
            ),
            None => Picked {
                gpu: host.default_gpu(),
                missing: Some(name),
            },
        },
        None => Picked {
            gpu: host.default_gpu(),
            missing: None,
        },
    };
    if passthrough && picked.gpu.is_none() {
        if host.gpus.is_empty() {
            bail!("passthrough needs a host GPU: {DECLARE_ONE}");
        }
        bail!(
            "passthrough needs a host GPU: this host declares {} and none is the default — \
             name one with `host_gpu` or mark one `default = true`",
            declared(host)
        );
    }
    Ok(picked)
}

/// What a VM's `gpu {}` asks of the host GPU: which declared GPU it got,
/// the device `passthrough` hands over or the render GPU a paravirtual mode
/// renders on, the emulator that runs it and that emulator's environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderPlan {
    /// The declared host GPU's name, for status.
    pub name: Option<String>,
    /// The PCI address `passthrough` hands the guest (`vfio-pci,host=`).
    pub passthrough: Option<String>,
    /// The other functions of that card vmlab-hv hands over with it, in
    /// function order ([`passthrough_functions`]). QEMU is handed none.
    pub passthrough_functions: Vec<String>,
    /// The GPU a `virgl`/`vulkan` guest renders on.
    pub gpu: Option<RenderGpu>,
    /// The host GPU's `qemu_binary`, checked: run in place of
    /// `qemu-system-<arch>` on PATH. `None` runs the stock emulator.
    pub qemu_binary: Option<PathBuf>,
    /// The directory the PCI-filter layer must be installed in before the
    /// emulator spawns ([`vk_layer::install`]); set when the environment
    /// names it.
    pub vk_layer: Option<PathBuf>,
    /// What the start should warn about, once each: a missing `host_gpu`
    /// the plan fell back from ([`Picked::warning`]), and on vmlab-hv the
    /// host GPU's QEMU-only keys it ignored and the render node it chose.
    pub warnings: Vec<String>,
    /// Whether the supervisor should hold [`Self::gpu`] awake before the
    /// emulator spawns ([`keepawake_wanted`]).
    pub keepawake: bool,
    pub env: ChildEnv,
}

impl RenderPlan {
    /// The `-L` directory a host GPU's own QEMU is handed (§5.2): the stock
    /// QEMU's data directory, so a minimal build finds the firmware and
    /// option ROMs it was never installed beside. QEMU still searches its
    /// own data directory after `-L`, so a full build loses nothing. `None`
    /// for the stock emulator, which needs no help, and when `PATH` holds
    /// no stock QEMU with a data directory.
    pub fn data_dir(&self, arch: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
        self.qemu_binary.as_ref()?;
        stock_data_dir(arch, path)
    }
}

/// The stock QEMU's data directory: `<prefix>/share/qemu` for the
/// `qemu-system-<arch>` the search path `path` finds (symlinks followed,
/// so a `/usr/local/bin` link into `/opt/qemu/bin` reads `/opt/qemu`), and
/// only when that directory exists.
pub fn stock_data_dir(arch: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let bin = super::emulator_binary(arch);
    let found = std::env::split_paths(path?)
        .map(|d| d.join(&bin))
        .find(|p| p.is_file())?;
    let real = found.canonicalize().unwrap_or(found);
    let dir = real.parent()?.parent()?.join("share/qemu");
    dir.is_dir().then_some(dir)
}

/// A host GPU's `qemu_binary` must be a runnable file, and one named
/// `qemu-system-<a>` must be for the VM's QEMU arch; a name of any other
/// form is taken as given. `arch` is `None` where the VM's arch is not
/// known yet (validation of a registry template), which skips that check.
pub fn check_qemu_binary(gpu: &str, binary: &Path, arch: Option<&str>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let runnable =
        std::fs::metadata(binary).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if !runnable {
        let why = if binary.exists() {
            "is not an executable file"
        } else {
            "does not exist"
        };
        bail!("host GPU `{gpu}`: qemu_binary {} {why}", binary.display());
    }
    if let Some(arch) = arch
        && let Some(theirs) = binary
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("qemu-system-"))
    {
        let ours = super::qemu_arch(arch);
        if theirs != ours {
            bail!(
                "host GPU `{gpu}`: qemu_binary {} emulates {theirs}, but this VM is {ours} \
                 (it would need qemu-system-{ours})",
                binary.display()
            );
        }
    }
    Ok(())
}

/// Lay a host GPU's `qemu_env` over the derived environment (§5.2): each
/// variable replaces any value vmlab derived for the same name, because an
/// explicit host setting beats a derived one. A name that is empty or
/// carries `=` is refused by the GPU's name.
pub fn apply_qemu_env(gpu: &str, env: &mut ChildEnv, extra: &[(String, String)]) -> Result<()> {
    for (key, value) in extra {
        if let Some(why) = crate::config::host::env_key_problem(key) {
            bail!("host GPU `{gpu}`: qemu_env has {why}: `{key}`");
        }
        env.set.retain(|(k, _)| k != key);
        env.set.push((key.clone(), value.clone()));
    }
    Ok(())
}

/// The emulator to spawn: a host GPU's `qemu_binary` (a
/// [`RenderPlan::qemu_binary`]), else `qemu-system-<arch>` from PATH.
pub fn emulator(qemu_binary: Option<&Path>, arch: &str) -> String {
    match qemu_binary {
        Some(path) => path.display().to_string(),
        None => super::emulator_binary(arch),
    }
}

/// The functions of a passed-through card that go to the guest with it, in
/// function order (PRD §5.2). A graphics card is more than one PCI function —
/// its HDMI/DP audio is function 1 — and a guest has sound from the card only
/// when that function arrives too.
///
/// With no `passthrough_functions` on the host GPU it is every other
/// function of the slot that is bound to `vfio-pci` already; one on another
/// driver stays with the host and is not an error, since nobody asked for it.
/// A declared list is exactly what goes, and each entry must be a function of
/// the same slot bound to `vfio-pci`: vmlab changes no binding, so an entry
/// that cannot go is refused by name. Either way the card itself must be
/// function 0, the only place a guest looks for a multi-function device.
fn passthrough_functions(decl: &HostGpu, sysfs: &Sysfs) -> Result<Vec<String>> {
    let primary_is_zero = decl.address.ends_with(".0");
    let siblings = sysfs.sibling_functions(&decl.address);
    let Some(asked) = &decl.passthrough_functions else {
        if !primary_is_zero {
            return Ok(Vec::new());
        }
        return Ok(siblings
            .into_iter()
            .filter(|(_, driver)| driver.as_deref() == Some("vfio-pci"))
            .map(|(address, _)| address)
            .collect());
    };
    let name = &decl.name;
    if !asked.is_empty() && !primary_is_zero {
        bail!(
            "host GPU `{name}`: `passthrough_functions` needs the GPU's `address` to be \
             function 0 of its card, and {} is not",
            decl.address
        );
    }
    let mut out: Vec<String> = Vec::new();
    for address in asked {
        if *address == decl.address {
            bail!(
                "host GPU `{name}`: `passthrough_functions` names {address}, the GPU's own \
                 `address` — list only the card's other functions"
            );
        }
        if out.contains(address) {
            bail!("host GPU `{name}`: `passthrough_functions` names {address} twice");
        }
        match siblings.iter().find(|(sibling, _)| sibling == address) {
            Some((_, Some(driver))) if driver == "vfio-pci" => out.push(address.clone()),
            Some((_, Some(driver))) => bail!(
                "host GPU `{name}`: `passthrough_functions` names {address}, which is bound \
                 to the host driver `{driver}`, not `vfio-pci` — vmlab never changes a driver \
                 binding: bind it to `vfio-pci`, or take it off the list"
            ),
            Some((_, None)) => bail!(
                "host GPU `{name}`: `passthrough_functions` names {address}, which is bound \
                 to no driver, not `vfio-pci` — vmlab never changes a driver binding: bind \
                 it to `vfio-pci`, or take it off the list"
            ),
            None => bail!(
                "host GPU `{name}`: `passthrough_functions` names {address}, which is not \
                 another function of the card at {} on this host",
                decl.address
            ),
        }
    }
    out.sort();
    Ok(out)
}

/// Decide a VM's [`RenderPlan`] before the emulator spawns (ADR-0003): pick
/// the host GPU, then ask sysfs about its address — a PCI device for
/// `passthrough`, a render node for `virgl`/`vulkan` — and name the GPU in
/// any refusal. Its `vulkan_icd` must exist; its `qemu_binary`, in every
/// mode, must run and suit `arch` (the VM's arch, when known); its
/// `qemu_env` goes on last. A `vulkan` GPU's PCI-filter layer is planned
/// under `data` ([`vk_layer::dir`]), not written, and its
/// `hide_uncached_memory` turns the layer's memory rule on.
///
/// On vmlab-hv (`hypervisor`, PRD §22, HV phase-2 D10) the render node, the
/// Vulkan environment, the filter layer and keep-awake are planned the same
/// way. `qemu_binary` and `qemu_env` are QEMU's and are ignored with a
/// warning each ([`qemu_env_ignored_warning`]): an entry written for QEMU,
/// `RENDER_SERVER_EXEC_PATH` above all, would otherwise replace the render
/// server of vmlab-hv's own bundle (card #447). vmlab-hv renders on a
/// node it is named, never one it finds, so a paravirtual GPU no declared
/// GPU resolves for renders on this host's first render node, and says so.
pub fn render_plan(
    gpu: Option<&Gpu>,
    arch: Option<&str>,
    host: &HostConfig,
    sysfs: &Sysfs,
    data: &Path,
    hypervisor: HypervisorKind,
) -> Result<RenderPlan> {
    let Some(gpu) = gpu else {
        return Ok(RenderPlan {
            env: emulator_env(false, None, None, None),
            ..Default::default()
        });
    };
    let picked = pick(gpu, host)?;
    let vmlab = hypervisor == HypervisorKind::Vmlab;
    let mut plan = RenderPlan {
        name: picked.gpu.map(|g| g.name.clone()),
        warnings: picked.warning().into_iter().collect(),
        ..Default::default()
    };
    let decl = picked.gpu;
    let context = || match decl {
        Some(g) => format!("host GPU `{}`", g.name),
        None => "host GPU".into(),
    };
    if gpu.mode == GpuMode::Passthrough {
        let decl = decl.expect("pick refuses passthrough with no host GPU");
        sysfs.device(&decl.address).with_context(context)?;
        plan.passthrough = Some(decl.address.clone());
        if vmlab {
            plan.passthrough_functions = passthrough_functions(decl, sysfs)?;
        } else if decl
            .passthrough_functions
            .as_ref()
            .is_some_and(|f| !f.is_empty())
        {
            plan.warnings.push(format!(
                "host GPU `{}`: `passthrough_functions` is ignored on QEMU, which is handed \
                 {} alone — it applies with hypervisor = \"vmlab\"",
                decl.name, decl.address
            ));
        }
        plan.env = emulator_env(false, None, None, None);
    } else {
        plan.gpu = decl
            .map(|g| sysfs.resolve(&g.address))
            .transpose()
            .with_context(context)?;
        if vmlab && plan.gpu.is_none() {
            let Some(first) = sysfs.render_gpus().into_iter().next() else {
                bail!(
                    "vmlab-hv renders `{}` on a host GPU's render node, and this host has \
                     none — {DECLARE_ONE}, or use hypervisor = \"qemu\"",
                    gpu.mode.as_str()
                );
            };
            plan.warnings.push(format!(
                "no declared host GPU resolves for this `gpu {{}}`; vmlab-hv renders on the \
                 first render node, {} ({})",
                first.render_node.display(),
                first.address
            ));
            plan.gpu = Some(first);
        }
        let vulkan = gpu.mode == GpuMode::Vulkan;
        let icd = decl
            .and_then(|g| g.vulkan_icd.as_deref())
            .filter(|_| vulkan);
        if let Some(icd) = icd
            && !icd.is_file()
        {
            bail!("{}: vulkan_icd {} does not exist", context(), icd.display());
        }
        if vulkan && plan.gpu.is_some() {
            plan.vk_layer = Some(vk_layer::dir(data));
        }
        if let (Some(d), Some(g)) = (decl, &plan.gpu) {
            plan.keepawake = keepawake_wanted(d.keepawake, g, &sysfs.power(&g.address));
        }
        plan.env = emulator_env(vulkan, plan.gpu.as_ref(), icd, plan.vk_layer.as_deref());
        // The memory rule is the layer's, so it goes only where the layer does.
        if plan.vk_layer.is_some() && decl.is_some_and(|g| g.hide_uncached_memory) {
            plan.env
                .set
                .push((vk_layer::HIDE_UNCACHED_VAR.into(), "1".into()));
        }
    }
    if let Some(decl) = decl
        && vmlab
    {
        // The executable and environment overrides are specific to QEMU.
        if let Some(binary) = &decl.qemu_binary {
            plan.warnings.push(format!(
                "host GPU `{}`: qemu_binary {} is QEMU's and does not apply on vmlab-hv; \
                 ignored",
                decl.name,
                binary.display()
            ));
        }
        plan.warnings.extend(qemu_env_ignored_warning(decl));
    } else if let Some(decl) = decl {
        if let Some(binary) = &decl.qemu_binary {
            check_qemu_binary(&decl.name, binary, arch)?;
            plan.qemu_binary = Some(binary.clone());
        }
        apply_qemu_env(&decl.name, &mut plan.env, &decl.qemu_env)?;
    }
    Ok(plan)
}

/// The variable virglrenderer reads for the Venus render server to start in
/// place of its build-time path. vmlab-hv's bundle launcher sets it to the
/// bundle's own server unless the process already carries one.
pub const RENDER_SERVER_VAR: &str = "RENDER_SERVER_EXEC_PATH";

/// The warning a host GPU's `qemu_env` earns on vmlab-hv, where it does not
/// apply: the keys it names, and for [`RENDER_SERVER_VAR`] that the VM runs
/// its bundle's render server instead. `None` for an empty `qemu_env`.
pub fn qemu_env_ignored_warning(decl: &HostGpu) -> Option<String> {
    if decl.qemu_env.is_empty() {
        return None;
    }
    let keys: Vec<&str> = decl.qemu_env.iter().map(|(k, _)| k.as_str()).collect();
    let server = if keys.contains(&RENDER_SERVER_VAR) {
        "; the VM runs the render server of its vmlab-hv bundle"
    } else {
        ""
    };
    Some(format!(
        "host GPU `{}`: qemu_env ({}) is QEMU's and does not apply on vmlab-hv; ignored{server}",
        decl.name,
        keys.join(", ")
    ))
}

/// `binary` as a spawn finds it: a path with a `/` as it is, a bare name
/// through `path` (the `PATH` value).
pub fn find_binary(binary: &Path, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if binary.components().count() > 1 {
        return Some(binary.to_path_buf());
    }
    std::env::split_paths(path?)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// The warning a `vulkan` VM on vmlab-hv earns when the Venus render server
/// it will start is not the one of its vmlab-hv bundle (card #325, card
/// #447: on NVIDIA an unpatched server presents at about 40 frames a
/// second). `binary` is the vmlab-hv a spawn runs ([`find_binary`]) and
/// `inherited` the [`RENDER_SERVER_VAR`] of the process that spawns it, which
/// the bundle's launcher keeps. A bundle is `bin/vmlab-hv`, a launcher
/// script, beside `libexec/virgl_render_server`. Three cases warn: an
/// inherited server that is not the bundle's, a vmlab-hv that is no bundle
/// launcher (virglrenderer then starts its build-time server), and an
/// inherited server with no bundle to compare it with.
pub fn render_server_warning(binary: &Path, inherited: Option<&str>) -> Option<String> {
    let real = binary.canonicalize().ok()?;
    let root = real.parent().and_then(Path::parent)?;
    let bundled = root.join("libexec/virgl_render_server");
    let launcher = real.parent().is_some_and(|dir| dir.ends_with("bin"))
        && root.join("libexec/vmlab-hv").is_file()
        && bundled.is_file();
    let same = |other: &str| {
        Path::new(other)
            .canonicalize()
            .is_ok_and(|other| bundled.canonicalize().is_ok_and(|b| b == other))
    };
    match (launcher, inherited.filter(|value| !value.is_empty())) {
        (true, None) => None,
        (true, Some(other)) if same(other) => None,
        (true, Some(other)) => Some(format!(
            "vmlab-hv starts the Venus render server {other} from {RENDER_SERVER_VAR} in the \
             environment, not its bundle's {}; an older or unpatched render server costs frame \
             rate (about 40 frames a second on NVIDIA) — unset {RENDER_SERVER_VAR} unless you \
             mean to test another server",
            bundled.display()
        )),
        (false, Some(other)) => Some(format!(
            "vmlab-hv at {} is not a bundle launcher, and starts the Venus render server \
             {other} from {RENDER_SERVER_VAR} in the environment; no bundle render server is \
             beside it to compare with",
            real.display()
        )),
        (false, None) => Some(format!(
            "vmlab-hv at {} is not a bundle launcher (bin/vmlab-hv beside \
             libexec/virgl_render_server), so virglrenderer starts its build-time Venus render \
             server, which may lack the bundle's patches — set host {{ vmlab_hv = … }} to a \
             bundle's bin/vmlab-hv",
            real.display()
        )),
    }
}

/// What these `vulkan`/`virgl` VMs on vmlab-hv earn short of an error, once
/// each: the `qemu_env` of the host GPU each picks, ignored
/// ([`qemu_env_ignored_warning`]), and for a `vulkan` one the render server
/// that is not its bundle's ([`render_server_warning`]). `validate` and `up`
/// print them; the start logs the same from its plan.
pub fn vmlab_hv_warnings<'a>(
    gpus: impl IntoIterator<Item = &'a Gpu>,
    host: &HostConfig,
    path: Option<&std::ffi::OsStr>,
    inherited: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut venus = false;
    for gpu in gpus {
        if gpu.mode == GpuMode::Passthrough {
            continue;
        }
        venus |= gpu.mode == GpuMode::Vulkan;
        if let Ok(picked) = pick(gpu, host)
            && let Some(w) = picked.gpu.and_then(qemu_env_ignored_warning)
            && !out.contains(&w)
        {
            out.push(w);
        }
    }
    if venus {
        let binary = host
            .vmlab_hv
            .clone()
            .unwrap_or_else(|| PathBuf::from("vmlab-hv"));
        out.extend(
            find_binary(&binary, path).and_then(|found| render_server_warning(&found, inherited)),
        );
    }
    out
}

/// Whether any of these `gpu {}` blocks renders on a host GPU nothing
/// picked: a `virgl`/`vulkan` for which [`pick`] finds no declared GPU.
pub fn render_gpu_unpicked<'a>(gpus: impl IntoIterator<Item = &'a Gpu>, host: &HostConfig) -> bool {
    gpus.into_iter()
        .filter(|g| g.mode != GpuMode::Passthrough)
        .any(|g| pick(g, host).is_ok_and(|p| p.gpu.is_none()))
}

/// Every warning these machines' `gpu {}` blocks earn on this host, once
/// each: a `host_gpu` this host lacks that a paravirtual mode falls back
/// from, an NVIDIA GPU a paravirtual mode renders on that can
/// runtime-suspend ([`runtime_pm_warning`]), and — when some paravirtual GPU
/// resolves to no declared GPU at all on a host with several render nodes —
/// [`unpicked_warning`]. Refusals are validation's and the start's, not
/// this.
pub fn warnings<'a>(
    gpus: impl IntoIterator<Item = (&'a str, &'a Gpu)>,
    host: &HostConfig,
    sysfs: &Sysfs,
) -> Vec<String> {
    let gpus: Vec<(&str, &Gpu)> = gpus.into_iter().collect();
    let mut out: Vec<String> = Vec::new();
    for (vm, gpu) in &gpus {
        if gpu.mode == GpuMode::Passthrough {
            continue;
        }
        let Ok(picked) = pick(gpu, host) else {
            continue;
        };
        if let Some(w) = picked.warning() {
            let w = format!("vm \"{vm}\": {w}");
            if !out.contains(&w) {
                out.push(w);
            }
        }
        if let Some(decl) = picked.gpu
            && let Ok(render) = sysfs.resolve(&decl.address)
            && let Some(w) = runtime_pm_warning(decl, &render, &sysfs.power(&decl.address))
            && !out.contains(&w)
        {
            out.push(w);
        }
    }
    if render_gpu_unpicked(gpus.iter().map(|(_, g)| *g), host)
        && let Some(w) = unpicked_warning(&sysfs.render_gpus())
    {
        out.push(w);
    }
    out
}

/// The warning a `virgl`/`vulkan` GPU earns when it is NVIDIA and can
/// runtime-suspend: between one lab's last emulator exiting and the next
/// one opening the render node, the driver powers the GPU down, and a
/// resume that fails has lost the GPU until reboot. Only NVIDIA, because
/// that resume (the GSP firmware booting again) is the failure seen; the
/// kernel default `power/control=auto` on every other driver would make it
/// noise. It names the host-side fix either way, and says whether vmlab's
/// keeper covers the gap meanwhile.
pub fn runtime_pm_warning(decl: &HostGpu, gpu: &RenderGpu, power: &Power) -> Option<String> {
    if gpu.driver.as_deref() != Some("nvidia") || !power.can_suspend() {
        return None;
    }
    let held = if keepawake_wanted(decl.keepawake, gpu, power) {
        "vmlab holds it awake while vmlabd runs (`keepawake`); to keep it awake across \
         supervisor restarts too"
    } else {
        "its `keepawake` is off, so nothing holds it; to keep it awake"
    };
    Some(format!(
        "host GPU `{}` ({}) can runtime-suspend (power/control=auto): a lab restart lets the \
         NVIDIA driver power it down, and a failed resume (Xid 119) has lost the GPU until \
         reboot. {held}, set `power/control` to `on` as root (a udev rule, for every boot: \
         ACTION==\"add|bind\", SUBSYSTEM==\"pci\", ATTR{{vendor}}==\"0x10de\", \
         ATTR{{class}}==\"0x030000\", ATTR{{power/control}}=\"on\") or turn runtime D3 off \
         (`options nvidia NVreg_DynamicPowerManagement=0x00` in /etc/modprobe.d/)",
        decl.name, decl.address
    ))
}

/// The warning a lab with a `virgl`/`vulkan` GPU gets when no host GPU
/// resolves and the host has more than one to pick from: QEMU's choice is
/// then whichever node it finds first, and Venus sees every Vulkan driver on
/// the host. `None` with one GPU or none.
pub fn unpicked_warning(gpus: &[RenderGpu]) -> Option<String> {
    if gpus.len() < 2 {
        return None;
    }
    let list: Vec<String> = gpus
        .iter()
        .map(|g| {
            format!(
                "{} ({}, {})",
                g.render_node.display(),
                g.address,
                g.driver.as_deref().unwrap_or("no driver")
            )
        })
        .collect();
    Some(format!(
        "this host has {} render nodes and no host GPU was picked for a virgl/vulkan gpu {{}}: {} — \
         declare `gpu \"<name>\" {{ address = ... }}` in ~/.config/vmlab/config.wcl \
         (a lone one is the default, else mark one `default = true`) (PRD §5.2)",
        gpus.len(),
        list.join(", ")
    ))
}

/// The `VK_LOADER_DRIVERS_SELECT` glob that keeps a kernel driver's Vulkan
/// ICD and drops every other: RADV for `amdgpu`, ANV for `i915`/`xe`, the
/// proprietary driver for `nvidia`.
pub fn loader_driver_select(kernel_driver: &str) -> Option<&'static str> {
    match kernel_driver {
        "amdgpu" => Some("*radeon*"),
        "i915" | "xe" => Some("*intel*"),
        "nvidia" => Some("*nvidia*"),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;

    /// Add a PCI device to a fake sysfs tree at `root`: a `drm/<node>` entry
    /// (when given) and a `driver` link to `drivers/<driver>`.
    pub fn fake_gpu(root: &Path, address: &str, node: Option<&str>, driver: &str) {
        let dev = root.join("bus/pci/devices").join(address);
        std::fs::create_dir_all(&dev).unwrap();
        if let Some(node) = node {
            std::fs::create_dir_all(dev.join("drm").join(node)).unwrap();
            std::fs::create_dir_all(dev.join("drm/card0")).unwrap();
        }
        std::os::unix::fs::symlink(
            format!("../../../bus/pci/drivers/{driver}"),
            dev.join("driver"),
        )
        .unwrap();
    }

    /// Give a fake device a `power/` directory: its `runtime_status` and
    /// `control`.
    pub fn fake_power(root: &Path, address: &str, runtime_status: &str, control: &str) {
        let power = root.join("bus/pci/devices").join(address).join("power");
        std::fs::create_dir_all(&power).unwrap();
        std::fs::write(power.join("runtime_status"), format!("{runtime_status}\n")).unwrap();
        std::fs::write(power.join("control"), format!("{control}\n")).unwrap();
    }

    /// Give a fake NVIDIA device the driver's own `power` file under
    /// `<root>/proc`, with this `Runtime D3 status:` value.
    pub fn fake_nvidia_runtime_d3(root: &Path, address: &str, status: &str) {
        let dir = root.join("proc/driver/nvidia/gpus").join(address);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("power"),
            format!(
                "Runtime D3 status:          {status}\n\
                 Tegra iGPU Rail-Gating:     Disabled\n\
                 Video Memory:               Active\n"
            ),
        )
        .unwrap();
    }

    /// This host's GPU set as a fake sysfs: two NVIDIA cards around an AMD
    /// iGPU, plus an Intel one.
    pub fn fake_host() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fake_gpu(dir.path(), "0000:01:00.0", Some("renderD128"), "nvidia");
        fake_gpu(dir.path(), "0000:72:00.0", Some("renderD129"), "amdgpu");
        fake_gpu(dir.path(), "0000:05:00.0", Some("renderD130"), "nvidia");
        fake_gpu(dir.path(), "0000:00:02.0", Some("renderD131"), "i915");
        fake_gpu(dir.path(), "0000:03:00.0", None, "vfio-pci");
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{fake_gpu, fake_host};
    use super::*;

    fn set(env: &ChildEnv) -> Vec<(&str, &str)> {
        env.set
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    /// An AMD address resolves to its render node and `amdgpu`, and a Vulkan
    /// guest on it gets the RADV filter and nothing else set.
    #[test]
    fn an_amdgpu_address_renders_on_its_node_with_the_radeon_filter() {
        let sys = fake_host();
        let gpu = Sysfs::new(sys.path()).resolve("0000:72:00.0").unwrap();
        assert_eq!(gpu.render_node, Path::new("/dev/dri/renderD129"));
        assert_eq!(gpu.driver.as_deref(), Some("amdgpu"));
        let env = emulator_env(true, Some(&gpu), None, None);
        assert_eq!(set(&env), [("VK_LOADER_DRIVERS_SELECT", "*radeon*")]);
    }

    /// An NVIDIA address gets the proprietary driver's filter.
    #[test]
    fn an_nvidia_address_renders_on_its_node_with_the_nvidia_filter() {
        let sys = fake_host();
        let gpu = Sysfs::new(sys.path()).resolve("0000:05:00.0").unwrap();
        assert_eq!(gpu.render_node, Path::new("/dev/dri/renderD130"));
        let env = emulator_env(true, Some(&gpu), None, None);
        assert_eq!(set(&env), [("VK_LOADER_DRIVERS_SELECT", "*nvidia*")]);
    }

    /// An Intel address (`i915` or `xe`) gets ANV's filter.
    #[test]
    fn an_intel_address_renders_on_its_node_with_the_intel_filter() {
        let sys = fake_host();
        let gpu = Sysfs::new(sys.path()).resolve("0000:00:02.0").unwrap();
        assert_eq!(gpu.render_node, Path::new("/dev/dri/renderD131"));
        let env = emulator_env(true, Some(&gpu), None, None);
        assert_eq!(set(&env), [("VK_LOADER_DRIVERS_SELECT", "*intel*")]);
        assert_eq!(loader_driver_select("xe"), Some("*intel*"));
    }

    /// An explicit ICD replaces the driver mapping with `VK_DRIVER_FILES`.
    #[test]
    fn a_vulkan_icd_overrides_the_driver_filter() {
        let sys = fake_host();
        let gpu = Sysfs::new(sys.path()).resolve("0000:72:00.0").unwrap();
        let icd = Path::new("/opt/icd/my_icd.json");
        let env = emulator_env(true, Some(&gpu), Some(icd), None);
        assert_eq!(set(&env), [("VK_DRIVER_FILES", "/opt/icd/my_icd.json")]);
    }

    /// Every Vulkan loader variable is cleared from every emulator, and
    /// `MESA_VK_DEVICE_SELECT` is never set — it breaks Venus.
    #[test]
    fn the_emulator_environment_always_clears_the_loader_variables() {
        let sys = fake_host();
        let gpu = Sysfs::new(sys.path()).resolve("0000:72:00.0").unwrap();
        for env in [
            emulator_env(false, None, None, None),
            emulator_env(false, Some(&gpu), None, None),
            emulator_env(true, None, None, None),
            emulator_env(true, Some(&gpu), None, None),
            emulator_env(true, Some(&gpu), None, Some(Path::new("/run/layer"))),
        ] {
            for var in VULKAN_ENV_VARS {
                assert!(env.remove.iter().any(|r| r == var), "{var} not cleared");
            }
            assert!(env.set.iter().all(|(k, _)| k != "MESA_VK_DEVICE_SELECT"));
        }
        assert!(emulator_env(false, Some(&gpu), None, None).set.is_empty());
        assert!(emulator_env(true, None, None, None).set.is_empty());
    }

    /// An address sysfs does not know, and a device with no render node, are
    /// both refused by name.
    #[test]
    fn an_address_with_no_device_or_no_render_node_is_refused_by_name() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let e = sysfs.resolve("0000:99:00.0").unwrap_err().to_string();
        assert!(
            e.contains("0000:99:00.0") && e.contains("no such PCI device"),
            "{e}"
        );
        let e = sysfs.resolve("0000:03:00.0").unwrap_err().to_string();
        assert!(
            e.contains("0000:03:00.0") && e.contains("no DRM render node"),
            "{e}"
        );
    }

    /// The data directory every plan here puts the layer under.
    const DATA: &str = "/home/u/.local/share/vmlab";

    /// [`super::render_plan`] for an x86_64 VM, the arch every test here
    /// but the arch checks runs.
    fn render_plan(gpu: Option<&Gpu>, host: &HostConfig, sysfs: &Sysfs) -> Result<RenderPlan> {
        super::render_plan(
            gpu,
            Some("x86_64"),
            host,
            sysfs,
            Path::new(DATA),
            HypervisorKind::Qemu,
        )
    }

    /// The layer variables a `vulkan` plan on `address` sets after the
    /// driver filter.
    fn layer_env(address: &str) -> [(String, String); 2] {
        [
            (
                vk_layer::PATH_VAR.into(),
                vk_layer::dir(Path::new(DATA)).display().to_string(),
            ),
            (vk_layer::ADDRESS_VAR.into(), address.into()),
        ]
    }

    /// `set` as owned pairs, to compare against a built list.
    fn owned(env: &ChildEnv) -> Vec<(String, String)> {
        env.set.clone()
    }

    fn gpu(mode: GpuMode, host_gpu: Option<&str>) -> Gpu {
        Gpu {
            vram: None,
            virtual_display: false,
            virtual_display_memory: None,
            mode,
            host_gpu: host_gpu.map(Into::into),
            span: Default::default(),
        }
    }

    fn decl(name: &str, address: &str, default: bool) -> HostGpu {
        HostGpu {
            name: name.into(),
            address: address.into(),
            vulkan_icd: None,
            default,
            qemu_binary: None,
            qemu_env: Vec::new(),
            keepawake: KeepAwake::Auto,
            hide_uncached_memory: false,
            passthrough_functions: None,
        }
    }

    /// This host as the brief declares it: `AMD1`, `Nvidia1` the default,
    /// `Nvidia2`.
    fn host() -> HostConfig {
        HostConfig {
            gpus: vec![
                decl("AMD1", "0000:72:00.0", false),
                decl("Nvidia1", "0000:01:00.0", true),
                decl("Nvidia2", "0000:05:00.0", false),
            ],
            ..Default::default()
        }
    }

    /// A declared name is found, case-insensitively, and renders on its
    /// own node with its own driver's filter.
    #[test]
    fn a_named_host_gpu_is_found_case_insensitively() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let plan = render_plan(Some(&gpu(GpuMode::Vulkan, Some("amd1"))), &host(), &sysfs).unwrap();
        assert_eq!(plan.name.as_deref(), Some("AMD1"));
        assert_eq!(plan.gpu.unwrap().address, "0000:72:00.0");
        assert_eq!(set(&plan.env)[0], ("VK_LOADER_DRIVERS_SELECT", "*radeon*"));
        assert_eq!(owned(&plan.env)[1..], layer_env("0000:72:00.0"));
        assert!(plan.warnings.is_empty());
    }

    /// No `host_gpu` takes the default: the marked one, else a lone one.
    #[test]
    fn an_unset_host_gpu_takes_the_default() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let bare = gpu(GpuMode::Virgl, None);
        let plan = render_plan(Some(&bare), &host(), &sysfs).unwrap();
        assert_eq!(plan.name.as_deref(), Some("Nvidia1"));
        assert_eq!(
            plan.gpu.unwrap().render_node,
            Path::new("/dev/dri/renderD128")
        );
        let lone = HostConfig {
            gpus: vec![decl("Intel1", "0000:00:02.0", false)],
            ..Default::default()
        };
        let plan = render_plan(Some(&bare), &lone, &sysfs).unwrap();
        assert_eq!(plan.name.as_deref(), Some("Intel1"));
    }

    /// A name this host lacks falls back to the default under a paravirtual
    /// mode, with a warning naming both.
    #[test]
    fn a_missing_paravirt_host_gpu_falls_back_to_the_default_with_a_warning() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let plan = render_plan(Some(&gpu(GpuMode::Vulkan, Some("Nope"))), &host(), &sysfs).unwrap();
        assert_eq!(plan.name.as_deref(), Some("Nvidia1"));
        assert_eq!(
            plan.warnings.first().map(String::as_str),
            Some("host GPU `Nope` is not declared on this host; using the default `Nvidia1`")
        );
        let w = warnings(
            [("a", &gpu(GpuMode::Vulkan, Some("Nope")))],
            &host(),
            &sysfs,
        );
        assert_eq!(
            w,
            ["vm \"a\": host GPU `Nope` is not declared on this host; using the default `Nvidia1`"]
        );
    }

    /// `passthrough` never falls back: a missing name is refused with what
    /// the host does declare, and nothing to pick is refused too.
    #[test]
    fn a_missing_passthrough_host_gpu_is_refused() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let e = render_plan(
            Some(&gpu(GpuMode::Passthrough, Some("Nope"))),
            &host(),
            &sysfs,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("`Nope` is not declared"), "{e}");
        assert!(e.contains("`AMD1`, `Nvidia1`, `Nvidia2`"), "{e}");
        let e = render_plan(
            Some(&gpu(GpuMode::Passthrough, None)),
            &HostConfig::default(),
            &sysfs,
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.starts_with("passthrough needs a host GPU: declare `gpu \"<name>\""),
            "{e}"
        );
    }

    /// On vmlab-hv a passed-through card takes its other functions with it
    /// (card #590): every one already bound to `vfio-pci`, in function
    /// order, and none on another driver, which nobody asked for. QEMU is
    /// handed the one address.
    #[test]
    fn passthrough_takes_the_cards_vfio_bound_functions_on_vmlab_hv() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        fake_gpu(sys.path(), "0000:03:00.2", None, "vfio-pci");
        fake_gpu(sys.path(), "0000:03:00.1", None, "vfio-pci");
        fake_gpu(sys.path(), "0000:03:00.3", None, "xhci_hcd");
        // Another slot's function is not this card's.
        fake_gpu(sys.path(), "0000:03:01.1", None, "vfio-pci");
        let mut h = host();
        h.gpus.push(decl("Vfio1", "0000:03:00.0", false));
        let pass = gpu(GpuMode::Passthrough, Some("Vfio1"));
        let on = |h: &HostConfig, hypervisor| {
            super::render_plan(
                Some(&pass),
                Some("x86_64"),
                h,
                &sysfs,
                Path::new(DATA),
                hypervisor,
            )
        };
        let plan = on(&h, HypervisorKind::Vmlab).unwrap();
        assert_eq!(plan.passthrough.as_deref(), Some("0000:03:00.0"));
        assert_eq!(plan.passthrough_functions, ["0000:03:00.1", "0000:03:00.2"]);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        let qemu = on(&h, HypervisorKind::Qemu).unwrap();
        assert!(qemu.passthrough_functions.is_empty() && qemu.warnings.is_empty());

        // A card declared by a function other than 0 goes alone.
        h.gpus.last_mut().unwrap().address = "0000:03:00.1".into();
        let plan = on(&h, HypervisorKind::Vmlab).unwrap();
        assert!(plan.passthrough_functions.is_empty());
    }

    /// `passthrough_functions` is exactly what goes: an empty list sends the
    /// card alone, and an entry that cannot go — on a host driver, on none,
    /// in another slot, absent, the card itself, or named twice — is refused
    /// by name, because the user asked for it. On QEMU the list is ignored
    /// with a warning.
    #[test]
    fn declared_passthrough_functions_go_or_are_refused_by_name() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        fake_gpu(sys.path(), "0000:03:00.1", None, "vfio-pci");
        fake_gpu(sys.path(), "0000:03:00.2", None, "vfio-pci");
        fake_gpu(sys.path(), "0000:03:00.3", None, "xhci_hcd");
        std::fs::create_dir_all(sys.path().join("bus/pci/devices/0000:03:00.4")).unwrap();
        fake_gpu(sys.path(), "0000:03:01.0", None, "vfio-pci");
        let pass = gpu(GpuMode::Passthrough, Some("Vfio1"));
        let on = |functions: &[&str], address: &str, hypervisor| {
            let mut h = host();
            h.gpus.push(HostGpu {
                passthrough_functions: Some(functions.iter().map(|f| f.to_string()).collect()),
                ..decl("Vfio1", address, false)
            });
            super::render_plan(
                Some(&pass),
                Some("x86_64"),
                &h,
                &sysfs,
                Path::new(DATA),
                hypervisor,
            )
        };
        let hv = |functions: &[&str]| on(functions, "0000:03:00.0", HypervisorKind::Vmlab);
        assert!(hv(&[]).unwrap().passthrough_functions.is_empty());
        assert_eq!(
            hv(&["0000:03:00.2"]).unwrap().passthrough_functions,
            ["0000:03:00.2"]
        );
        assert_eq!(
            hv(&["0000:03:00.2", "0000:03:00.1"])
                .unwrap()
                .passthrough_functions,
            ["0000:03:00.1", "0000:03:00.2"]
        );
        let refused = |functions: &[&str]| format!("{:#}", hv(functions).unwrap_err());
        let e = refused(&["0000:03:00.3"]);
        assert!(
            e.starts_with("host GPU `Vfio1`: `passthrough_functions` names 0000:03:00.3")
                && e.contains("bound to the host driver `xhci_hcd`, not `vfio-pci`")
                && e.contains("never changes a driver binding"),
            "{e}"
        );
        let e = refused(&["0000:03:00.4"]);
        assert!(
            e.contains("0000:03:00.4, which is bound to no driver"),
            "{e}"
        );
        for other in ["0000:03:01.0", "0000:03:00.7", "03:00.1"] {
            let e = refused(&[other]);
            assert!(
                e.contains("is not another function of the card at 0000:03:00.0"),
                "{e}"
            );
        }
        let e = refused(&["0000:03:00.0"]);
        assert!(e.contains("the GPU's own `address`"), "{e}");
        let e = refused(&["0000:03:00.1", "0000:03:00.1"]);
        assert!(e.contains("names 0000:03:00.1 twice"), "{e}");
        let e = format!(
            "{:#}",
            on(&["0000:03:00.0"], "0000:03:00.1", HypervisorKind::Vmlab).unwrap_err()
        );
        assert!(e.contains("to be function 0 of its card"), "{e}");

        let qemu = on(&["0000:03:00.3"], "0000:03:00.0", HypervisorKind::Qemu).unwrap();
        assert!(qemu.passthrough_functions.is_empty());
        assert_eq!(qemu.warnings.len(), 1, "{:?}", qemu.warnings);
        assert!(
            qemu.warnings[0].contains("`passthrough_functions` is ignored on QEMU"),
            "{:?}",
            qemu.warnings
        );
    }

    /// A passthrough host GPU hands over its address and renders nothing;
    /// a card bound to `vfio-pci` has no render node and needs none.
    #[test]
    fn passthrough_hands_over_the_address_and_renders_nothing() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let mut h = host();
        h.gpus.push(decl("Vfio1", "0000:03:00.0", false));
        let plan =
            render_plan(Some(&gpu(GpuMode::Passthrough, Some("vfio1"))), &h, &sysfs).unwrap();
        assert_eq!(plan.passthrough.as_deref(), Some("0000:03:00.0"));
        assert!(plan.gpu.is_none() && plan.env.set.is_empty());
        h.gpus.push(decl("Gone", "0000:99:00.0", false));
        let e = format!(
            "{:#}",
            render_plan(Some(&gpu(GpuMode::Passthrough, Some("Gone"))), &h, &sysfs).unwrap_err()
        );
        assert!(
            e.starts_with("host GPU `Gone`: ") && e.contains("no such PCI device"),
            "{e}"
        );
    }

    /// With no host GPU declared a paravirtual mode renders where QEMU puts
    /// it, filter-free but still stripped of inherited loader variables.
    #[test]
    fn no_declared_host_gpu_leaves_paravirt_to_qemu() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let plan = render_plan(
            Some(&gpu(GpuMode::Vulkan, None)),
            &HostConfig::default(),
            &sysfs,
        )
        .unwrap();
        assert!(plan.name.is_none() && plan.gpu.is_none());
        assert!(plan.env.set.is_empty());
        assert_eq!(plan.env.remove.len(), VULKAN_ENV_VARS.len());
        let plan = render_plan(
            Some(&gpu(GpuMode::Vulkan, Some("Nope"))),
            &HostConfig::default(),
            &sysfs,
        )
        .unwrap();
        assert!(plan.gpu.is_none());
        assert!(plan.warnings[0].contains("it has no default"));
    }

    /// A host GPU's `vulkan_icd` replaces the driver filter under `vulkan`
    /// only, and one that does not exist is refused by the GPU's name.
    #[test]
    fn a_host_gpus_vulkan_icd_feeds_the_loader_and_must_exist() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let icd = sys.path().join("nvidia_icd.json");
        std::fs::write(&icd, "{}").unwrap();
        let mut h = host();
        h.gpus[2].vulkan_icd = Some(icd.clone());
        let plan = render_plan(Some(&gpu(GpuMode::Vulkan, Some("Nvidia2"))), &h, &sysfs).unwrap();
        assert_eq!(
            set(&plan.env)[0],
            ("VK_DRIVER_FILES", icd.to_str().unwrap())
        );
        assert_eq!(owned(&plan.env)[1..], layer_env("0000:05:00.0"));
        let plan = render_plan(Some(&gpu(GpuMode::Virgl, Some("Nvidia2"))), &h, &sysfs).unwrap();
        assert!(plan.env.set.is_empty());
        h.gpus[2].vulkan_icd = Some("/nonexistent/icd.json".into());
        let e = render_plan(Some(&gpu(GpuMode::Vulkan, Some("Nvidia2"))), &h, &sysfs)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("host GPU `Nvidia2`") && e.contains("does not exist"),
            "{e}"
        );
    }

    /// A paravirtual host GPU whose device has no render node is refused
    /// lazily, at resolution, by name.
    #[test]
    fn a_paravirt_host_gpu_with_no_render_node_is_refused_by_name() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let h = HostConfig {
            gpus: vec![decl("Vfio1", "0000:03:00.0", true)],
            ..Default::default()
        };
        let e = format!(
            "{:#}",
            render_plan(Some(&gpu(GpuMode::Virgl, None)), &h, &sysfs).unwrap_err()
        );
        assert!(
            e.contains("host GPU `Vfio1`") && e.contains("no DRM render node"),
            "{e}"
        );
    }

    /// Only a paravirtual GPU that resolves to no declared host GPU is
    /// unpicked.
    #[test]
    fn a_render_gpu_is_unpicked_only_when_nothing_resolves() {
        let bare = gpu(GpuMode::Vulkan, None);
        let pt = gpu(GpuMode::Passthrough, None);
        assert!(render_gpu_unpicked([&bare], &HostConfig::default()));
        assert!(!render_gpu_unpicked([&bare], &host()));
        assert!(!render_gpu_unpicked([&pt], &HostConfig::default()));
        let no_default = HostConfig {
            gpus: vec![
                decl("A", "0000:72:00.0", false),
                decl("B", "0000:01:00.0", false),
            ],
            ..Default::default()
        };
        assert!(render_gpu_unpicked([&bare], &no_default));
        assert!(!render_gpu_unpicked(
            [&gpu(GpuMode::Vulkan, Some("b"))],
            &no_default
        ));
    }

    /// The multi-GPU warning names every node with its address and driver,
    /// and a single-GPU host gets none.
    #[test]
    fn the_unpicked_warning_names_every_node() {
        let sys = fake_host();
        let gpus = Sysfs::new(sys.path()).render_gpus();
        let w = unpicked_warning(&gpus).unwrap();
        assert!(
            w.contains("/dev/dri/renderD129 (0000:72:00.0, amdgpu)"),
            "{w}"
        );
        assert!(
            w.contains("/dev/dri/renderD128 (0000:01:00.0, nvidia)"),
            "{w}"
        );
        assert!(w.contains("declare `gpu \"<name>\""), "{w}");
        assert!(unpicked_warning(&gpus[..1]).is_none());
    }

    /// The host's render GPUs list in render-node order, skipping devices
    /// without one.
    #[test]
    fn render_gpus_lists_every_render_node_in_order() {
        let sys = fake_host();
        let gpus = Sysfs::new(sys.path()).render_gpus();
        let got: Vec<(&str, &str)> = gpus
            .iter()
            .map(|g| (g.address.as_str(), g.driver.as_deref().unwrap()))
            .collect();
        assert_eq!(
            got,
            [
                ("0000:01:00.0", "nvidia"),
                ("0000:72:00.0", "amdgpu"),
                ("0000:05:00.0", "nvidia"),
                ("0000:00:02.0", "i915"),
            ]
        );
    }

    /// An executable file with `name` under `dir`, standing in for a QEMU.
    fn fake_qemu(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let bin = dir.join(name);
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// This host with `Nvidia1` carrying its own QEMU and environment.
    fn host_with_qemu(binary: &Path, env: &[(&str, &str)]) -> HostConfig {
        let mut h = host();
        h.gpus[1].qemu_binary = Some(binary.to_path_buf());
        h.gpus[1].qemu_env = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        h
    }

    /// Every VM on a host GPU with a `qemu_binary` runs it — passthrough
    /// included — and a VM on another GPU runs the stock emulator.
    #[test]
    fn a_host_gpus_qemu_binary_runs_its_vms_in_every_mode() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let bin = fake_qemu(&sys.path().join("opt/bin"), "qemu-system-x86_64");
        let mut h = host_with_qemu(&bin, &[]);
        h.gpus[1].address = "0000:03:00.0".into();
        let pt = render_plan(Some(&gpu(GpuMode::Passthrough, None)), &h, &sysfs).unwrap();
        assert_eq!(pt.qemu_binary.as_deref(), Some(bin.as_path()));
        assert_eq!(
            emulator(pt.qemu_binary.as_deref(), "x86_64"),
            bin.display().to_string()
        );
        let h = host_with_qemu(&bin, &[]);
        for mode in [GpuMode::Virgl, GpuMode::Vulkan] {
            let plan = render_plan(Some(&gpu(mode, None)), &h, &sysfs).unwrap();
            assert_eq!(plan.qemu_binary.as_deref(), Some(bin.as_path()));
        }
        let amd = render_plan(Some(&gpu(GpuMode::Vulkan, Some("AMD1"))), &h, &sysfs).unwrap();
        assert_eq!(amd.qemu_binary, None);
        assert_eq!(
            emulator(amd.qemu_binary.as_deref(), "x86_64"),
            "qemu-system-x86_64"
        );
        assert_eq!(render_plan(None, &h, &sysfs).unwrap().qemu_binary, None);
    }

    /// A `qemu_binary` named for another arch is refused, naming both; a
    /// name of another form is taken as given; an unknown VM arch skips
    /// the check.
    #[test]
    fn a_qemu_binary_for_another_arch_is_refused_naming_both() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let arm = fake_qemu(&sys.path().join("opt/bin"), "qemu-system-aarch64");
        let h = host_with_qemu(&arm, &[]);
        let e = render_plan(Some(&gpu(GpuMode::Vulkan, None)), &h, &sysfs)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("host GPU `Nvidia1`")
                && e.contains("emulates aarch64, but this VM is x86_64"),
            "{e}"
        );
        assert!(
            super::render_plan(
                Some(&gpu(GpuMode::Vulkan, None)),
                None,
                &h,
                &sysfs,
                Path::new(DATA),
                HypervisorKind::Qemu,
            )
            .is_ok()
        );
        let odd = fake_qemu(&sys.path().join("opt/bin"), "qemu-vmlab");
        let h = host_with_qemu(&odd, &[]);
        let plan = render_plan(Some(&gpu(GpuMode::Vulkan, None)), &h, &sysfs).unwrap();
        assert_eq!(plan.qemu_binary.as_deref(), Some(odd.as_path()));
        // `x86` is QEMU's `x86_64`.
        let bin = fake_qemu(&sys.path().join("opt/bin"), "qemu-system-x86_64");
        let h = host_with_qemu(&bin, &[]);
        assert!(
            super::render_plan(
                Some(&gpu(GpuMode::Vulkan, None)),
                Some("x86"),
                &h,
                &sysfs,
                Path::new(DATA),
                HypervisorKind::Qemu,
            )
            .is_ok()
        );
    }

    /// vmlab-hv takes the derived Vulkan environment and nothing of the host
    /// GPU's QEMU-only keys: `qemu_binary` and `qemu_env` are ignored, each
    /// with a warning, so an entry written for QEMU cannot replace the
    /// bundle's render server (card #447). QEMU still gets both.
    #[test]
    fn on_vmlab_hv_qemu_env_is_ignored_and_a_render_node_is_always_named() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let gone = sys.path().join("nope/qemu-system-x86_64");
        let h = host_with_qemu(
            &gone,
            &[
                ("RENDER_SERVER_EXEC_PATH", "/x"),
                ("VK_LOADER_DRIVERS_SELECT", "*custom*"),
            ],
        );
        let on = |gpu: &Gpu, h: &HostConfig| {
            super::render_plan(
                Some(gpu),
                Some("x86_64"),
                h,
                &sysfs,
                Path::new(DATA),
                HypervisorKind::Vmlab,
            )
        };
        let plan = on(&gpu(GpuMode::Vulkan, None), &h).unwrap();
        assert_eq!(plan.qemu_binary, None);
        assert!(
            plan.env
                .set
                .iter()
                .all(|(k, v)| k != "RENDER_SERVER_EXEC_PATH" && v != "*custom*"),
            "{:?}",
            plan.env
        );
        assert_eq!(plan.warnings.len(), 2, "{:?}", plan.warnings);
        assert!(plan.warnings[0].contains("qemu_binary") && plan.warnings[0].contains("ignored"));
        assert_eq!(
            plan.warnings[1],
            "host GPU `Nvidia1`: qemu_env (RENDER_SERVER_EXEC_PATH, VK_LOADER_DRIVERS_SELECT) is \
             QEMU's and does not apply on vmlab-hv; ignored; the VM runs the render server of \
             its vmlab-hv bundle"
        );
        // The same host GPU on QEMU keeps its environment.
        let exe = fake_qemu(&sys.path().join("q"), "qemu-system-x86_64");
        let qemu_host = host_with_qemu(&exe, &[("RENDER_SERVER_EXEC_PATH", "/x")]);
        let on_qemu = render_plan(Some(&gpu(GpuMode::Vulkan, None)), &qemu_host, &sysfs).unwrap();
        assert!(
            on_qemu
                .env
                .set
                .contains(&("RENDER_SERVER_EXEC_PATH".into(), "/x".into()))
        );
        assert!(on_qemu.warnings.is_empty(), "{:?}", on_qemu.warnings);
        let qemu = render_plan(Some(&gpu(GpuMode::Vulkan, None)), &host(), &sysfs).unwrap();
        let vmlab = on(&gpu(GpuMode::Vulkan, None), &host()).unwrap();
        assert_eq!(
            (vmlab.gpu, vmlab.env, vmlab.vk_layer),
            (qemu.gpu, qemu.env, qemu.vk_layer)
        );

        let none = HostConfig::default();
        let plan = on(&gpu(GpuMode::Virgl, None), &none).unwrap();
        let first = sysfs.render_gpus().into_iter().next().unwrap();
        assert_eq!(plan.gpu.as_ref(), Some(&first));
        assert!(
            plan.warnings[0].contains("first render node"),
            "{:?}",
            plan.warnings
        );
    }

    /// A bundle as `build/hv.py` stages it: the launcher, the binary and the
    /// render server.
    fn bundle(root: &Path) -> PathBuf {
        fake_qemu(&root.join("libexec"), "vmlab-hv");
        fake_qemu(&root.join("libexec"), "virgl_render_server");
        fake_qemu(&root.join("bin"), "vmlab-hv")
    }

    /// The render server a vmlab-hv VM starts is its bundle's, or the start
    /// says which one it is instead.
    #[test]
    fn a_render_server_that_is_not_the_bundles_warns() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = bundle(&dir.path().join("bundle"));
        let bundled = dir.path().join("bundle/libexec/virgl_render_server");
        assert_eq!(render_server_warning(&launcher, None), None);
        assert_eq!(render_server_warning(&launcher, Some("")), None);
        assert_eq!(
            render_server_warning(&launcher, Some(bundled.to_str().unwrap())),
            None
        );
        // A symlink to the launcher, as an install puts on PATH, is the bundle.
        let link = dir.path().join("vmlab-hv");
        std::os::unix::fs::symlink(&launcher, &link).unwrap();
        assert_eq!(render_server_warning(&link, None), None);

        let old = fake_qemu(&dir.path().join("old/libexec"), "virgl_render_server");
        let w = render_server_warning(&launcher, Some(old.to_str().unwrap())).unwrap();
        assert!(
            w.contains(old.to_str().unwrap())
                && w.contains("not its bundle's")
                && w.contains(bundled.canonicalize().unwrap().to_str().unwrap()),
            "{w}"
        );

        // The binary without its launcher: virglrenderer's build-time server.
        let bare = dir.path().join("bundle/libexec/vmlab-hv");
        let w = render_server_warning(&bare, None).unwrap();
        assert!(
            w.contains("build-time") && w.contains("not a bundle launcher"),
            "{w}"
        );
        let cargo = fake_qemu(&dir.path().join("target/release"), "vmlab-hv");
        assert!(
            render_server_warning(&cargo, None)
                .unwrap()
                .contains("build-time")
        );
        let w = render_server_warning(&cargo, Some("/opt/virgl_render_server")).unwrap();
        assert!(w.contains("/opt/virgl_render_server"), "{w}");
        // A vmlab-hv that is not there is the start's refusal, not a warning.
        assert_eq!(render_server_warning(&dir.path().join("nope"), None), None);
    }

    /// `validate` and `up` say both things of a vmlab-hv VM before it
    /// starts: the ignored `qemu_env` of the GPU it picks, once, and a
    /// render server that is not the bundle's for a `vulkan` one only.
    #[test]
    fn vmlab_hv_warnings_name_the_ignored_qemu_env_and_the_render_server() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = bundle(&dir.path().join("bundle"));
        let mut h = host_with_qemu(Path::new("/q"), &[("RENDER_SERVER_EXEC_PATH", "/old")]);
        h.vmlab_hv = Some(launcher.clone());
        let vulkan = gpu(GpuMode::Vulkan, Some("Nvidia1"));
        let virgl = gpu(GpuMode::Virgl, Some("Nvidia1"));
        let amd = gpu(GpuMode::Vulkan, Some("AMD1"));
        let pass = gpu(GpuMode::Passthrough, Some("Nvidia1"));

        let w = vmlab_hv_warnings([&vulkan, &virgl], &h, None, None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("qemu_env (RENDER_SERVER_EXEC_PATH)"), "{w:?}");
        assert!(vmlab_hv_warnings([&amd, &pass], &h, None, None).is_empty());

        let w = vmlab_hv_warnings([&amd], &h, None, Some("/old"));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("/old") && w[0].contains("not its bundle's"),
            "{w:?}"
        );
        // `virgl` starts no render server.
        let w = vmlab_hv_warnings([&gpu(GpuMode::Virgl, Some("AMD1"))], &h, None, Some("/old"));
        assert!(w.is_empty(), "{w:?}");

        // No `vmlab_hv` in the host config: the one on PATH.
        h.vmlab_hv = None;
        let path = std::env::join_paths([dir.path().join("bundle/libexec")]).unwrap();
        let w = vmlab_hv_warnings([&amd], &h, Some(&path), None);
        assert!(w[0].contains("build-time"), "{w:?}");
        let path = std::env::join_paths([dir.path().join("bundle/bin")]).unwrap();
        assert!(vmlab_hv_warnings([&amd], &h, Some(&path), None).is_empty());
    }

    /// A `qemu_binary` that is missing, or is there but cannot run, is
    /// refused naming the host GPU and the path.
    #[test]
    fn a_missing_or_non_executable_qemu_binary_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let gone = sys.path().join("nope/qemu-system-x86_64");
        let e = render_plan(
            Some(&gpu(GpuMode::Vulkan, None)),
            &host_with_qemu(&gone, &[]),
            &sysfs,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            e,
            format!(
                "host GPU `Nvidia1`: qemu_binary {} does not exist",
                gone.display()
            )
        );
        let flat = fake_qemu(&sys.path().join("opt/bin"), "qemu-system-x86_64");
        std::fs::set_permissions(&flat, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = render_plan(
            Some(&gpu(GpuMode::Vulkan, None)),
            &host_with_qemu(&flat, &[]),
            &sysfs,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            e,
            format!(
                "host GPU `Nvidia1`: qemu_binary {} is not an executable file",
                flat.display()
            )
        );
        let dir = sys.path().join("opt");
        let e = render_plan(
            Some(&gpu(GpuMode::Vulkan, None)),
            &host_with_qemu(&dir, &[]),
            &sysfs,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("is not an executable file"), "{e}");
    }

    /// `-L` is the stock QEMU's `<prefix>/share/qemu`, handed only to a
    /// host GPU's own QEMU and only when the directory exists.
    #[test]
    fn data_dir_is_the_stock_prefix_and_only_for_a_host_gpus_qemu() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let prefix = sys.path().join("usr");
        fake_qemu(&prefix.join("bin"), "qemu-system-x86_64");
        let path = std::env::join_paths([sys.path().join("empty"), prefix.join("bin")]).unwrap();
        let path = Some(path.as_os_str());
        let own = fake_qemu(&sys.path().join("opt/bin"), "qemu-system-x86_64");
        let h = host_with_qemu(&own, &[]);
        let plan = render_plan(Some(&gpu(GpuMode::Vulkan, None)), &h, &sysfs).unwrap();
        // No share/qemu under the stock prefix: nothing to hand over.
        assert_eq!(plan.data_dir("x86_64", path), None);
        std::fs::create_dir_all(prefix.join("share/qemu")).unwrap();
        assert_eq!(
            plan.data_dir("x86_64", path),
            Some(prefix.join("share/qemu").canonicalize().unwrap())
        );
        // The stock emulator finds its own data; no stock QEMU, no `-L`.
        let amd = render_plan(Some(&gpu(GpuMode::Vulkan, Some("AMD1"))), &h, &sysfs).unwrap();
        assert_eq!(amd.data_dir("x86_64", path), None);
        assert_eq!(plan.data_dir("aarch64", path), None);
        assert_eq!(plan.data_dir("x86_64", None), None);
    }

    /// A stock QEMU reached through a symlink reads the prefix it lives in.
    #[test]
    fn the_stock_data_dir_follows_a_symlinked_emulator() {
        let tmp = tempfile::tempdir().unwrap();
        let real = fake_qemu(&tmp.path().join("opt/qemu/bin"), "qemu-system-x86_64");
        std::fs::create_dir_all(tmp.path().join("opt/qemu/share/qemu")).unwrap();
        std::fs::create_dir_all(tmp.path().join("local/bin")).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("local/bin/qemu-system-x86_64")).unwrap();
        let path = tmp.path().join("local/bin");
        assert_eq!(
            stock_data_dir("x86_64", Some(path.as_os_str())),
            Some(
                tmp.path()
                    .join("opt/qemu/share/qemu")
                    .canonicalize()
                    .unwrap()
            )
        );
    }

    /// `qemu_env` lands after the derived loader filter and wins over it:
    /// an explicit host setting beats a derived one.
    #[test]
    fn qemu_env_is_applied_after_the_derived_filter_and_wins() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let bin = fake_qemu(&sys.path().join("opt/bin"), "qemu-system-x86_64");
        let h = host_with_qemu(&bin, &[("VKR_DEBUG", "validate")]);
        let plan = render_plan(Some(&gpu(GpuMode::Vulkan, None)), &h, &sysfs).unwrap();
        let [path, address] = layer_env("0000:01:00.0");
        assert_eq!(
            owned(&plan.env),
            [
                ("VK_LOADER_DRIVERS_SELECT".into(), "*nvidia*".into()),
                path.clone(),
                address.clone(),
                ("VKR_DEBUG".into(), "validate".into())
            ]
        );
        let h = host_with_qemu(&bin, &[("VK_LOADER_DRIVERS_SELECT", "*nvidia_icd*")]);
        let plan = render_plan(Some(&gpu(GpuMode::Vulkan, None)), &h, &sysfs).unwrap();
        assert_eq!(
            owned(&plan.env),
            [
                path.clone(),
                address,
                ("VK_LOADER_DRIVERS_SELECT".into(), "*nvidia_icd*".into())
            ]
        );
        // The layer's own variables obey the same rule.
        let h = host_with_qemu(
            &bin,
            &[
                (vk_layer::ADDRESS_VAR, "0000:05:00.0"),
                (vk_layer::DISABLE_VAR, "1"),
            ],
        );
        let plan = render_plan(Some(&gpu(GpuMode::Vulkan, None)), &h, &sysfs).unwrap();
        assert_eq!(
            owned(&plan.env),
            [
                ("VK_LOADER_DRIVERS_SELECT".into(), "*nvidia*".into()),
                path,
                (vk_layer::ADDRESS_VAR.into(), "0000:05:00.0".into()),
                (vk_layer::DISABLE_VAR.into(), "1".into())
            ]
        );
        // The inherited loader variables are still stripped first.
        assert_eq!(plan.env.remove.len(), VULKAN_ENV_VARS.len());
        // Passthrough runs under it too.
        let mut h = host_with_qemu(&bin, &[("VKR_DEBUG", "validate")]);
        h.gpus[1].address = "0000:03:00.0".into();
        let pt = render_plan(Some(&gpu(GpuMode::Passthrough, None)), &h, &sysfs).unwrap();
        assert_eq!(set(&pt.env), [("VKR_DEBUG", "validate")]);
    }

    /// A `qemu_env` name that is empty or carries `=` is refused by the
    /// host GPU's name, however the declaration was made.
    #[test]
    fn a_bad_qemu_env_key_is_refused_by_name() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let bin = fake_qemu(&sys.path().join("opt/bin"), "qemu-system-x86_64");
        for (key, why) in [
            ("", "an empty variable name"),
            ("A=B", "a variable name containing `=`"),
        ] {
            let h = host_with_qemu(&bin, &[(key, "x")]);
            let e = render_plan(Some(&gpu(GpuMode::Vulkan, None)), &h, &sysfs)
                .unwrap_err()
                .to_string();
            assert_eq!(
                e,
                format!("host GPU `Nvidia1`: qemu_env has {why}: `{key}`")
            );
        }
    }

    /// Every `vulkan` guest on a picked host GPU gets the PCI-filter layer
    /// keyed to that GPU's address — the two identical NVIDIA cards each
    /// their own, the AMD iGPU its own — and the plan names the directory to
    /// install it in.
    #[test]
    fn a_vulkan_host_gpu_gets_the_pci_filter_layer_on_its_own_address() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        for (name, address, select) in [
            ("Nvidia1", "0000:01:00.0", "*nvidia*"),
            ("Nvidia2", "0000:05:00.0", "*nvidia*"),
            ("AMD1", "0000:72:00.0", "*radeon*"),
        ] {
            let plan =
                render_plan(Some(&gpu(GpuMode::Vulkan, Some(name))), &host(), &sysfs).unwrap();
            let mut want = vec![("VK_LOADER_DRIVERS_SELECT".to_string(), select.to_string())];
            want.extend(layer_env(address));
            assert_eq!(owned(&plan.env), want, "{name}");
            assert_eq!(
                plan.vk_layer.as_deref(),
                Some(vk_layer::dir(Path::new(DATA)).as_path())
            );
        }
    }

    /// A host GPU's `hide_uncached_memory` turns the layer's memory rule on
    /// for a `vulkan` guest on that GPU and on no other; the GPU's
    /// `qemu_env` still has the last word. `virgl` on the same GPU loads no
    /// layer and so gets no switch.
    #[test]
    fn hide_uncached_memory_sets_the_layers_switch_for_that_gpu_only() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let mut h = host();
        let nvidia2 = h.gpus.iter().position(|g| g.name == "Nvidia2").unwrap();
        h.gpus[nvidia2].hide_uncached_memory = true;
        let switch = (vk_layer::HIDE_UNCACHED_VAR.to_string(), "1".to_string());
        let plan = |mode, name| render_plan(Some(&gpu(mode, Some(name))), &h, &sysfs).unwrap();
        let on = plan(GpuMode::Vulkan, "Nvidia2");
        assert_eq!(owned(&on.env).last(), Some(&switch));
        for other in [
            plan(GpuMode::Vulkan, "Nvidia1"),
            plan(GpuMode::Vulkan, "AMD1"),
        ] {
            assert!(!owned(&other.env).contains(&switch));
        }
        assert!(plan(GpuMode::Virgl, "Nvidia2").env.set.is_empty());
        h.gpus[nvidia2].qemu_env = vec![(vk_layer::HIDE_UNCACHED_VAR.into(), "0".into())];
        let off = render_plan(Some(&gpu(GpuMode::Vulkan, Some("Nvidia2"))), &h, &sysfs).unwrap();
        let set: Vec<_> = owned(&off.env)
            .into_iter()
            .filter(|(k, _)| k == vk_layer::HIDE_UNCACHED_VAR)
            .collect();
        assert_eq!(
            set,
            [(vk_layer::HIDE_UNCACHED_VAR.to_string(), "0".to_string())]
        );
    }

    /// `virgl`, `passthrough`, no `gpu {}` and a `vulkan` guest with no host
    /// GPU resolved get no layer, though its variables are still stripped
    /// from what the emulator inherits.
    #[test]
    fn only_a_vulkan_guest_with_a_resolved_host_gpu_gets_the_layer() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let mut h = host();
        h.gpus.push(decl("Vfio1", "0000:03:00.0", false));
        let plans = [
            render_plan(Some(&gpu(GpuMode::Virgl, Some("Nvidia2"))), &h, &sysfs),
            render_plan(Some(&gpu(GpuMode::Passthrough, Some("Vfio1"))), &h, &sysfs),
            render_plan(None, &h, &sysfs),
            render_plan(
                Some(&gpu(GpuMode::Vulkan, None)),
                &HostConfig::default(),
                &sysfs,
            ),
        ];
        for plan in plans {
            let plan = plan.unwrap();
            assert_eq!(plan.vk_layer, None);
            assert!(plan.env.set.is_empty(), "{:?}", plan.env.set);
            for var in [
                vk_layer::PATH_VAR,
                vk_layer::ADDRESS_VAR,
                vk_layer::DISABLE_VAR,
                vk_layer::HIDE_UNCACHED_VAR,
            ] {
                assert!(
                    plan.env.remove.iter().any(|r| r == var),
                    "{var} not cleared"
                );
            }
        }
    }

    /// A host GPU the driver has lost on a resume (`runtime_status` is
    /// `error`) is refused by the start and by validation, naming the GPU
    /// and its address, rather than handed to an emulator that would abort
    /// on it after the VM was reported up.
    #[test]
    fn a_host_gpu_in_a_failed_power_state_is_refused_by_name() {
        let sys = fake_host();
        super::testing::fake_power(sys.path(), "0000:01:00.0", "error", "auto");
        let sysfs = Sysfs::new(sys.path());
        for mode in [GpuMode::Vulkan, GpuMode::Virgl] {
            let e = format!(
                "{:#}",
                render_plan(Some(&gpu(mode, None)), &host(), &sysfs).unwrap_err()
            );
            assert!(
                e.starts_with("host GPU `Nvidia1`: gpu address 0000:01:00.0: ")
                    && e.contains("failed power state (power/runtime_status=error)")
                    && e.contains("needs a reboot"),
                "{e}"
            );
        }
        // The other card, and a GPU whose state reads anything else, render.
        render_plan(
            Some(&gpu(GpuMode::Vulkan, Some("Nvidia2"))),
            &host(),
            &sysfs,
        )
        .unwrap();
        for status in ["active", "suspended", "resuming", "unsupported"] {
            super::testing::fake_power(sys.path(), "0000:01:00.0", status, "auto");
            render_plan(Some(&gpu(GpuMode::Vulkan, None)), &host(), &sysfs).unwrap();
        }
    }

    /// Power is read from `power/` and, for NVIDIA, the driver's own file;
    /// a device can suspend under `control=auto` unless the driver says
    /// runtime D3 is disabled, and never under `control=on`.
    #[test]
    fn power_reads_sysfs_and_the_nvidia_runtime_d3_line() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        assert_eq!(sysfs.power("0000:01:00.0"), Power::default());
        super::testing::fake_power(sys.path(), "0000:01:00.0", "suspended", "auto");
        let p = sysfs.power("0000:01:00.0");
        assert_eq!(p.runtime_status.as_deref(), Some("suspended"));
        assert_eq!(p.control.as_deref(), Some("auto"));
        assert_eq!(p.runtime_d3, None);
        assert!(p.can_suspend() && !p.failed());
        for (line, d3, can) in [
            ("Enabled (fine-grained)", Some(true), true),
            ("Disabled by default", Some(false), false),
            ("Not supported", None, true),
        ] {
            super::testing::fake_nvidia_runtime_d3(sys.path(), "0000:01:00.0", line);
            let p = sysfs.power("0000:01:00.0");
            assert_eq!(p.runtime_d3, d3, "{line}");
            assert_eq!(p.can_suspend(), can, "{line}");
        }
        super::testing::fake_power(sys.path(), "0000:05:00.0", "active", "on");
        assert!(!sysfs.power("0000:05:00.0").can_suspend());
    }

    /// `on` and `off` are what they say on any driver; `auto` holds only an
    /// NVIDIA GPU that can runtime-suspend.
    #[test]
    fn keepawake_auto_is_an_nvidia_gpu_that_can_suspend() {
        let render = |driver: &str| RenderGpu {
            address: "0000:01:00.0".into(),
            render_node: "/dev/dri/renderD128".into(),
            driver: Some(driver.into()),
        };
        let power = |control: &str, d3: Option<bool>| Power {
            runtime_status: Some("active".into()),
            control: Some(control.into()),
            runtime_d3: d3,
        };
        let nv = render("nvidia");
        let amd = render("amdgpu");
        assert!(keepawake_wanted(KeepAwake::Auto, &nv, &power("auto", None)));
        assert!(keepawake_wanted(
            KeepAwake::Auto,
            &nv,
            &power("auto", Some(true))
        ));
        assert!(!keepawake_wanted(
            KeepAwake::Auto,
            &nv,
            &power("auto", Some(false))
        ));
        assert!(!keepawake_wanted(KeepAwake::Auto, &nv, &power("on", None)));
        assert!(!keepawake_wanted(
            KeepAwake::Auto,
            &amd,
            &power("auto", None)
        ));
        assert!(keepawake_wanted(KeepAwake::On, &amd, &power("on", None)));
        assert!(!keepawake_wanted(
            KeepAwake::Off,
            &nv,
            &power("auto", Some(true))
        ));
    }

    /// The plan carries the keep-awake decision for the GPU it renders on,
    /// and passthrough never wants one.
    #[test]
    fn the_render_plan_says_whether_to_keep_its_gpu_awake() {
        let sys = fake_host();
        super::testing::fake_power(sys.path(), "0000:01:00.0", "active", "auto");
        super::testing::fake_power(sys.path(), "0000:05:00.0", "active", "auto");
        super::testing::fake_nvidia_runtime_d3(sys.path(), "0000:05:00.0", "Disabled by default");
        super::testing::fake_power(sys.path(), "0000:72:00.0", "active", "auto");
        let sysfs = Sysfs::new(sys.path());
        let wants = |mode, name: &str, h: &HostConfig| {
            render_plan(Some(&gpu(mode, Some(name))), h, &sysfs)
                .unwrap()
                .keepawake
        };
        let h = host();
        assert!(wants(GpuMode::Vulkan, "Nvidia1", &h));
        assert!(wants(GpuMode::Virgl, "Nvidia1", &h));
        assert!(!wants(GpuMode::Vulkan, "Nvidia2", &h));
        assert!(!wants(GpuMode::Vulkan, "AMD1", &h));
        let mut h = host();
        h.gpus[0].keepawake = KeepAwake::On;
        h.gpus[1].keepawake = KeepAwake::Off;
        assert!(wants(GpuMode::Vulkan, "AMD1", &h));
        assert!(!wants(GpuMode::Vulkan, "Nvidia1", &h));
        h.gpus.push(HostGpu {
            keepawake: KeepAwake::On,
            ..decl("Vfio1", "0000:03:00.0", false)
        });
        assert!(!wants(GpuMode::Passthrough, "Vfio1", &h));
    }

    /// An NVIDIA GPU a paravirtual guest renders on that can runtime-suspend
    /// earns one warning naming the GPU, its address and both host-side
    /// fixes; it says whether the keeper covers the gap. A GPU whose runtime
    /// D3 is disabled, one at `control=on`, another driver, and passthrough
    /// earn none.
    #[test]
    fn a_suspendable_nvidia_gpu_earns_one_runtime_pm_warning() {
        let sys = fake_host();
        super::testing::fake_power(sys.path(), "0000:01:00.0", "suspended", "auto");
        super::testing::fake_power(sys.path(), "0000:05:00.0", "active", "auto");
        super::testing::fake_nvidia_runtime_d3(sys.path(), "0000:05:00.0", "Disabled by default");
        super::testing::fake_power(sys.path(), "0000:72:00.0", "active", "auto");
        let sysfs = Sysfs::new(sys.path());
        let vulkan = gpu(GpuMode::Vulkan, Some("Nvidia1"));
        let virgl = gpu(GpuMode::Virgl, None);
        let w = warnings([("a", &vulkan), ("b", &virgl)], &host(), &sysfs);
        assert_eq!(w.len(), 1, "{w:?}");
        let w = &w[0];
        assert!(
            w.starts_with(
                "host GPU `Nvidia1` (0000:01:00.0) can runtime-suspend (power/control=auto)"
            ),
            "{w}"
        );
        assert!(w.contains("vmlab holds it awake while vmlabd runs"), "{w}");
        assert!(w.contains("ATTR{power/control}=\"on\""), "{w}");
        assert!(
            w.contains("options nvidia NVreg_DynamicPowerManagement=0x00"),
            "{w}"
        );

        let mut off = host();
        off.gpus[1].keepawake = KeepAwake::Off;
        let w = warnings([("a", &vulkan)], &off, &sysfs);
        assert!(
            w[0].contains("its `keepawake` is off, so nothing holds it"),
            "{w:?}"
        );

        let quiet = [
            gpu(GpuMode::Vulkan, Some("Nvidia2")),
            gpu(GpuMode::Vulkan, Some("AMD1")),
        ];
        assert!(warnings(quiet.iter().map(|g| ("a", g)), &host(), &sysfs).is_empty());
        super::testing::fake_power(sys.path(), "0000:01:00.0", "active", "on");
        assert!(warnings([("a", &vulkan)], &host(), &sysfs).is_empty());
        let mut pt = host();
        pt.gpus[1].address = "0000:03:00.0".into();
        super::testing::fake_power(sys.path(), "0000:03:00.0", "suspended", "auto");
        let pass = gpu(GpuMode::Passthrough, Some("Nvidia1"));
        assert!(warnings([("a", &pass)], &pt, &sysfs).is_empty());
    }
}
