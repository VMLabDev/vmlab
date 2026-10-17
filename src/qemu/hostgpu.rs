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
//! link the kernel driver. The sysfs root is a value so the resolution is
//! testable on any host.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::process::ChildEnv;
use crate::config::host::{HostConfig, HostGpu};
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

/// The sysfs tree host GPUs are read from: `/sys` on a real host, a fake tree
/// in tests.
#[derive(Debug, Clone)]
pub struct Sysfs {
    root: PathBuf,
}

impl Sysfs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The host's own `/sys`.
    pub fn host() -> Self {
        Self::new("/sys")
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

    /// Resolve a PCI address to its render node and driver. An address with
    /// no device, or a device with no render node, is an error naming it.
    pub fn resolve(&self, address: &str) -> Result<RenderGpu> {
        self.device(address)?;
        let dev = self.devices().join(address);
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

/// The basename of the device's `driver` link.
fn driver(dev: &Path) -> Option<String> {
    let target = std::fs::read_link(dev.join("driver")).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// The Vulkan loader variables vmlab owns in the emulator's environment.
/// Every one is removed before anything is set, so nothing a supervisor was
/// started with leaks into a guest's renderer. `MESA_VK_DEVICE_SELECT` is in
/// the list but never set: Mesa's device-select layer breaks Venus
/// (`vkr: venus driver is required to call vkEnumeratePhysicalDevices first`).
pub const VULKAN_ENV_VARS: [&str; 4] = [
    "VK_DRIVER_FILES",
    "VK_ICD_FILENAMES",
    "VK_LOADER_DRIVERS_SELECT",
    "MESA_VK_DEVICE_SELECT",
];

/// The emulator's environment for a VM (§5.2): the Vulkan loader
/// variables always cleared, and under `vulkan` a filter that leaves the
/// loader only the selected GPU's driver. An explicit `vulkan_icd` wins over
/// the driver mapping; a GPU whose driver maps to no ICD, or no GPU at all,
/// gets no filter.
pub fn emulator_env(vulkan: bool, gpu: Option<&RenderGpu>, vulkan_icd: Option<&Path>) -> ChildEnv {
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
/// renders on, and the emulator's environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderPlan {
    /// The declared host GPU's name, for status.
    pub name: Option<String>,
    /// The PCI address `passthrough` hands the guest (`vfio-pci,host=`).
    pub passthrough: Option<String>,
    /// The GPU a `virgl`/`vulkan` guest renders on.
    pub gpu: Option<RenderGpu>,
    /// A missing `host_gpu` the plan fell back from ([`Picked::warning`]).
    pub warning: Option<String>,
    pub env: ChildEnv,
}

/// Decide a VM's [`RenderPlan`] before the emulator spawns (ADR-0003): pick
/// the host GPU, then ask sysfs about its address — a PCI device for
/// `passthrough`, a render node for `virgl`/`vulkan` — and name the GPU in
/// any refusal. Its `vulkan_icd` must exist.
pub fn render_plan(gpu: Option<&Gpu>, host: &HostConfig, sysfs: &Sysfs) -> Result<RenderPlan> {
    let Some(gpu) = gpu else {
        return Ok(RenderPlan {
            env: emulator_env(false, None, None),
            ..Default::default()
        });
    };
    let picked = pick(gpu, host)?;
    let mut plan = RenderPlan {
        name: picked.gpu.map(|g| g.name.clone()),
        warning: picked.warning(),
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
        plan.env = emulator_env(false, None, None);
        return Ok(plan);
    }
    plan.gpu = decl
        .map(|g| sysfs.resolve(&g.address))
        .transpose()
        .with_context(context)?;
    let vulkan = gpu.mode == GpuMode::Vulkan;
    let icd = decl
        .and_then(|g| g.vulkan_icd.as_deref())
        .filter(|_| vulkan);
    if let Some(icd) = icd
        && !icd.is_file()
    {
        bail!("{}: vulkan_icd {} does not exist", context(), icd.display());
    }
    plan.env = emulator_env(vulkan, plan.gpu.as_ref(), icd);
    Ok(plan)
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
/// from, and — when some paravirtual GPU resolves to no declared GPU at all
/// on a host with several render nodes — [`unpicked_warning`]. Refusals are
/// validation's and the start's, not this.
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
        if let Some(w) = pick(gpu, host).ok().and_then(|p| p.warning()) {
            let w = format!("vm \"{vm}\": {w}");
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    if render_gpu_unpicked(gpus.iter().map(|(_, g)| *g), host)
        && let Some(w) = unpicked_warning(&sysfs.render_gpus())
    {
        out.push(w);
    }
    out
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
    use super::testing::fake_host;
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
        let env = emulator_env(true, Some(&gpu), None);
        assert_eq!(set(&env), [("VK_LOADER_DRIVERS_SELECT", "*radeon*")]);
    }

    /// An NVIDIA address gets the proprietary driver's filter.
    #[test]
    fn an_nvidia_address_renders_on_its_node_with_the_nvidia_filter() {
        let sys = fake_host();
        let gpu = Sysfs::new(sys.path()).resolve("0000:05:00.0").unwrap();
        assert_eq!(gpu.render_node, Path::new("/dev/dri/renderD130"));
        let env = emulator_env(true, Some(&gpu), None);
        assert_eq!(set(&env), [("VK_LOADER_DRIVERS_SELECT", "*nvidia*")]);
    }

    /// An Intel address (`i915` or `xe`) gets ANV's filter.
    #[test]
    fn an_intel_address_renders_on_its_node_with_the_intel_filter() {
        let sys = fake_host();
        let gpu = Sysfs::new(sys.path()).resolve("0000:00:02.0").unwrap();
        assert_eq!(gpu.render_node, Path::new("/dev/dri/renderD131"));
        let env = emulator_env(true, Some(&gpu), None);
        assert_eq!(set(&env), [("VK_LOADER_DRIVERS_SELECT", "*intel*")]);
        assert_eq!(loader_driver_select("xe"), Some("*intel*"));
    }

    /// An explicit ICD replaces the driver mapping with `VK_DRIVER_FILES`.
    #[test]
    fn a_vulkan_icd_overrides_the_driver_filter() {
        let sys = fake_host();
        let gpu = Sysfs::new(sys.path()).resolve("0000:72:00.0").unwrap();
        let icd = Path::new("/opt/icd/my_icd.json");
        let env = emulator_env(true, Some(&gpu), Some(icd));
        assert_eq!(set(&env), [("VK_DRIVER_FILES", "/opt/icd/my_icd.json")]);
    }

    /// Every Vulkan loader variable is cleared from every emulator, and
    /// `MESA_VK_DEVICE_SELECT` is never set — it breaks Venus.
    #[test]
    fn the_emulator_environment_always_clears_the_loader_variables() {
        let sys = fake_host();
        let gpu = Sysfs::new(sys.path()).resolve("0000:72:00.0").unwrap();
        for env in [
            emulator_env(false, None, None),
            emulator_env(false, Some(&gpu), None),
            emulator_env(true, None, None),
            emulator_env(true, Some(&gpu), None),
        ] {
            for var in VULKAN_ENV_VARS {
                assert!(env.remove.iter().any(|r| r == var), "{var} not cleared");
            }
            assert!(env.set.iter().all(|(k, _)| k != "MESA_VK_DEVICE_SELECT"));
        }
        assert!(emulator_env(false, Some(&gpu), None).set.is_empty());
        assert!(emulator_env(true, None, None).set.is_empty());
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

    fn gpu(mode: GpuMode, host_gpu: Option<&str>) -> Gpu {
        Gpu {
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
        assert_eq!(set(&plan.env), [("VK_LOADER_DRIVERS_SELECT", "*radeon*")]);
        assert_eq!(plan.warning, None);
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
            plan.warning.as_deref(),
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
        assert!(plan.warning.unwrap().contains("it has no default"));
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
        assert_eq!(set(&plan.env), [("VK_DRIVER_FILES", icd.to_str().unwrap())]);
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
}
