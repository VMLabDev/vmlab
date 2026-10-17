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
//! A GPU is named by its host PCI address — the same `gpu.address`
//! `passthrough` uses — and resolved through sysfs: the device's `drm/`
//! directory holds its render node, its `driver` link the kernel driver. The
//! sysfs root is a value so the resolution is testable on any host.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::process::ChildEnv;
use crate::config::model::{Gpu, GpuMode};

/// One host GPU with a render node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGpu {
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

    /// Resolve a PCI address to its render node and driver. An address with
    /// no device, or a device with no render node, is an error naming it.
    pub fn resolve(&self, address: &str) -> Result<HostGpu> {
        let dev = self.devices().join(address);
        if !dev.exists() {
            bail!(
                "gpu address {address}: no such PCI device (no {})",
                dev.display()
            );
        }
        match render_node(&dev) {
            Some(node) => Ok(HostGpu {
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
    pub fn render_gpus(&self) -> Vec<HostGpu> {
        let Ok(entries) = std::fs::read_dir(self.devices()) else {
            return Vec::new();
        };
        let mut gpus: Vec<HostGpu> = entries
            .flatten()
            .filter_map(|e| {
                let dev = e.path();
                Some(HostGpu {
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
pub fn emulator_env(vulkan: bool, gpu: Option<&HostGpu>, vulkan_icd: Option<&Path>) -> ChildEnv {
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

/// What a VM's `gpu {}` asks of the host GPU: the GPU it renders on, if one
/// was picked, and the emulator's environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderPlan {
    pub gpu: Option<HostGpu>,
    pub env: ChildEnv,
}

/// Decide a VM's [`RenderPlan`] before the emulator spawns (ADR-0003). Only
/// `virgl` and `vulkan` render on a host GPU — `passthrough`'s address is the
/// device itself, and no `gpu {}` renders nothing — so only they read the
/// lab's `address`, else `host_default` (the host config's `gpu_address`).
pub fn render_plan(
    gpu: Option<&Gpu>,
    host_default: Option<&str>,
    sysfs: &Sysfs,
) -> Result<RenderPlan> {
    let Some(gpu) = gpu.filter(|g| g.mode != GpuMode::Passthrough) else {
        return Ok(RenderPlan {
            gpu: None,
            env: emulator_env(false, None, None),
        });
    };
    let host = gpu
        .address
        .as_deref()
        .or(host_default)
        .map(|a| sysfs.resolve(a))
        .transpose()?;
    let env = emulator_env(
        gpu.mode == GpuMode::Vulkan,
        host.as_ref(),
        gpu.vulkan_icd.as_deref(),
    );
    Ok(RenderPlan { gpu: host, env })
}

/// Whether any of these `gpu {}` blocks renders on a host GPU nothing picked:
/// a `virgl`/`vulkan` with no `address`, and no host config default.
pub fn render_gpu_unpicked<'a>(
    gpus: impl IntoIterator<Item = &'a Gpu>,
    host_default: Option<&str>,
) -> bool {
    host_default.is_none()
        && gpus
            .into_iter()
            .any(|g| g.mode != GpuMode::Passthrough && g.address.is_none())
}

/// The warning `up` gives a lab with a `virgl`/`vulkan` GPU when nothing
/// picked a host GPU and the host has more than one to pick from: QEMU's
/// choice is then whichever node it finds first, and Venus sees every
/// Vulkan driver on the host. `None` with one GPU or none.
pub fn unpicked_warning(gpus: &[HostGpu]) -> Option<String> {
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
        "this host has {} render nodes and no GPU was picked for a virgl/vulkan gpu {{}}: {} — \
         set `address` in the lab's gpu {{}} or `gpu_address` in the host config (PRD §5.2)",
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

    fn gpu(mode: GpuMode, address: Option<&str>) -> Gpu {
        Gpu {
            mode,
            address: address.map(Into::into),
            vulkan_icd: None,
            span: Default::default(),
        }
    }

    /// The lab's address wins over the host config's default, which applies
    /// only when the lab names none.
    #[test]
    fn the_lab_address_wins_over_the_host_default() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let lab = gpu(GpuMode::Vulkan, Some("0000:72:00.0"));
        let plan = render_plan(Some(&lab), Some("0000:01:00.0"), &sysfs).unwrap();
        assert_eq!(plan.gpu.unwrap().address, "0000:72:00.0");
        let bare = gpu(GpuMode::Vulkan, None);
        let plan = render_plan(Some(&bare), Some("0000:01:00.0"), &sysfs).unwrap();
        assert_eq!(plan.gpu.unwrap().driver.as_deref(), Some("nvidia"));
        assert!(
            render_plan(Some(&bare), None, &sysfs)
                .unwrap()
                .gpu
                .is_none()
        );
    }

    /// `passthrough` hands its device to the guest, so neither its address
    /// nor the host default is read as a render GPU.
    #[test]
    fn passthrough_picks_no_render_gpu() {
        let sys = fake_host();
        let sysfs = Sysfs::new(sys.path());
        let pt = gpu(GpuMode::Passthrough, Some("0000:03:00.0"));
        let plan = render_plan(Some(&pt), Some("0000:72:00.0"), &sysfs).unwrap();
        assert!(plan.gpu.is_none() && plan.env.set.is_empty());
    }

    /// Only a paravirtual GPU with no address on the lab or the host is
    /// unpicked.
    #[test]
    fn a_render_gpu_is_unpicked_only_with_no_lab_or_host_address() {
        let bare = gpu(GpuMode::Vulkan, None);
        let picked = gpu(GpuMode::Virgl, Some("0000:72:00.0"));
        let pt = gpu(GpuMode::Passthrough, None);
        assert!(render_gpu_unpicked([&picked, &bare], None));
        assert!(!render_gpu_unpicked([&bare], Some("0000:72:00.0")));
        assert!(!render_gpu_unpicked([&picked, &pt], None));
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
