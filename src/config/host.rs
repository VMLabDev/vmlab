//! Host-level daemon configuration (PRD §9.4 pool override, §9.5 suffix,
//! §9.2 PSK, §8.1 watchdog threshold, §11 viewer, §6.4 chunk size, §20.4
//! shared-app clients), read from `~/.config/vmlab/config.wcl`. Every field
//! optional; defaults apply.

use std::path::Path;

use anyhow::{Result, anyhow};
use wcl_lang::{Document, Environment, Registry, disk_loader};

use super::block::{Reader, Unspan, finish};

pub const HOST_SCHEMA_WCL: &str = include_str!("host_schema.wcl");

/// The workspace size guard's default cap (PRD §19.6). Sized to catch the
/// 4 GB `.vhdx` nobody wrote an ignore rule for while leaving room for the
/// large-but-legitimate files a source tree does hold, because the guard's
/// job is to refuse *unwanted* work — a build burst is wanted work that
/// happens to be large, and that warns rather than refuses.
pub const DEFAULT_WORKSPACE_MAX_FILE: u64 = 256 << 20;

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub subnet_pool: ipnet::Ipv4Net,
    pub dns_suffix: String,
    pub dns_upstream: Option<String>,
    pub disk_low_percent: u8,
    pub psk: Option<String>,
    /// TCP listen port for inbound cross-host segment trunks (§9.2).
    pub trunk_port: u16,
    pub viewer: Option<String>,
    /// The host's waypipe client for shared apps (PRD §20.4); `None` =
    /// `waypipe` on `PATH`.
    pub waypipe: Option<std::path::PathBuf>,
    /// The FreeRDP client for shared apps from Windows guests (PRD §20.4);
    /// `None` = `xfreerdp3`, then `xfreerdp`, on `PATH`.
    pub freerdp: Option<std::path::PathBuf>,
    /// The ffmpeg console recordings are encoded with (PRD §11.1); `None` =
    /// `ffmpeg` on `PATH`. Read by the lab daemon when a recording starts.
    pub ffmpeg: Option<std::path::PathBuf>,
    pub oci_chunk_size: u64,
    /// Network fast-path tier selection (§9.1 substitutable backend).
    pub fastpath: crate::net::fastpath::FastpathMode,
    /// Directory holding config-weave guest binaries; `None` = env var /
    /// XDG default (see `labd::playbook::resolve_bin_dir`).
    pub config_weave_bin_dir: Option<std::path::PathBuf>,
    /// The workspace syncer's per-file size guard (PRD §19.6). Host config
    /// rather than a `@dev` argument: the cap is about this developer's link
    /// to this guest, not about the lab everyone shares — and the refusal
    /// message names it, because "raise the cap" has to point somewhere.
    pub workspace_max_file: u64,
    /// UEFI firmware override (PRD §5.2): `<dir>/<arch>/`, laid out like the
    /// bundled `firmware/` set. `None` = vmlab's bundled firmware, then the
    /// host's (see `qemu::firmware`).
    pub firmware_dir: Option<std::path::PathBuf>,
    /// The host GPUs (PRD §5.2), in declaration order: every host-specific
    /// GPU fact lives here, named, so a lab file names a GPU rather than a
    /// PCI address and stays portable between hosts.
    pub gpus: Vec<HostGpu>,
}

/// One declared host GPU (PRD §5.2): `gpu "<name>" { address = … }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGpu {
    /// The label; a lab's `host_gpu` matches it case-insensitively.
    pub name: String,
    /// Host PCI address, `0000:72:00.0`.
    pub address: String,
    /// Vulkan ICD JSON overriding the driver-derived loader filter.
    pub vulkan_icd: Option<std::path::PathBuf>,
    /// Marked `default = true`. [`HostConfig::default_gpu`] is the one to
    /// ask: a lone block is the default without it.
    pub default: bool,
    /// The QEMU every VM on this GPU runs in place of `qemu-system-<arch>`
    /// on PATH — a patched build the GPU's driver needs. Absolute; whether
    /// it exists, runs and suits the VM's arch is asked at resolution
    /// (`qemu::hostgpu::render_plan`), since a host config is read on
    /// machines that never start a VM.
    pub qemu_binary: Option<std::path::PathBuf>,
    /// Variables set in the QEMU process's own environment, in key order,
    /// after vmlab's derived Vulkan loader variables — so these win.
    pub qemu_env: Vec<(String, String)>,
    /// Whether the supervisor holds this GPU out of runtime suspend while
    /// VMs render on it (ADR-0018).
    pub keepawake: KeepAwake,
}

/// A host GPU's `keepawake` (PRD §5.2, ADR-0018): whether the supervisor
/// runs a helper that keeps the GPU out of runtime suspend from the first
/// `virgl`/`vulkan` VM started on it until the supervisor stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeepAwake {
    /// Only an NVIDIA GPU that can runtime-suspend: `power/control` is
    /// `auto` and the driver does not report runtime D3 disabled.
    #[default]
    Auto,
    /// Always, on any driver with EGL.
    On,
    /// Never.
    Off,
}

impl KeepAwake {
    /// The spelling of each value, once, for the extractor.
    pub const NAMES: &'static [(&'static str, Self)] =
        &[("auto", Self::Auto), ("on", Self::On), ("off", Self::Off)];
}

/// Why `key` cannot name an environment variable: empty, or carrying the
/// `=` that ends a name in `environ`. `None` for a usable name.
pub fn env_key_problem(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        Some("an empty variable name")
    } else if key.contains('=') {
        Some("a variable name containing `=`")
    } else {
        None
    }
}

/// What `gpu_address` says now that it is gone (PRD §5.2).
pub const GPU_ADDRESS_MOVED: &str = "host `gpu_address` was replaced by named host GPUs: \
     declare `gpu \"<name>\" { address = \"0000:01:00.0\" }` beside `host {}` in \
     ~/.config/vmlab/config.wcl — a lone block is the default, or mark one `default = true` (PRD §5.2)";

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            subnet_pool: "10.213.0.0/16".parse().expect("valid default pool"),
            dns_suffix: "vmlab.internal".to_string(),
            dns_upstream: None,
            disk_low_percent: 10,
            psk: None,
            trunk_port: 13947,
            viewer: None,
            waypipe: None,
            freerdp: None,
            ffmpeg: None,
            oci_chunk_size: crate::oci::chunking::DEFAULT_CHUNK_SIZE,
            fastpath: crate::net::fastpath::FastpathMode::Auto,
            config_weave_bin_dir: None,
            workspace_max_file: DEFAULT_WORKSPACE_MAX_FILE,
            firmware_dir: None,
            gpus: Vec::new(),
        }
    }
}

impl HostConfig {
    /// Load from the XDG config dir; absent file = all defaults.
    pub fn load_default() -> Result<Self> {
        let path = crate::paths::config_dir().join("config.wcl");
        if !path.is_file() {
            return Ok(Self::default());
        }
        let source = std::fs::read_to_string(&path)?;
        Self::parse(&source, &path.display().to_string())
    }

    pub fn parse(source: &str, name: &str) -> Result<Self> {
        if !source.contains("import <vmlab-host.wcl>") {
            return Err(anyhow!(
                "host config {name} is missing `import <vmlab-host.wcl>` at the top"
            ));
        }
        let mut registry = Registry::new();
        registry.register("vmlab-host.wcl", HOST_SCHEMA_WCL);
        let doc = Document::open_at_with_loader(
            source,
            name,
            None,
            &Environment::new(),
            registry.loader(disk_loader()),
        )
        .map_err(|e| anyhow!("parse error in {name}: {e}"))?;
        let mut issues = super::schema_issues(&doc);
        let mut cfg = Self::default();
        let mut moved = Vec::new();
        if let Some(block) = doc.blocks().find(|b| b.kind() == "host") {
            let mut r = Reader::new(&block, &mut issues);
            // Every field is an override: absent (or malformed, which is
            // reported) leaves the default in place.
            if let Some(v) = r.parse_as("subnet_pool", "CIDR").unspan() {
                cfg.subnet_pool = v;
            }
            if let Some(v) = r.string("dns_suffix").unspan() {
                cfg.dns_suffix = v;
            }
            cfg.dns_upstream = r.string("dns_upstream").unspan();
            if let Some(v) = r.int_in("disk_low_percent", 0, 100).unspan() {
                cfg.disk_low_percent = v;
            }
            cfg.psk = r.string("psk").unspan();
            if let Some(v) = r.port("trunk_port").unspan() {
                cfg.trunk_port = v;
            }
            cfg.viewer = r.string("viewer").unspan();
            cfg.waypipe = r.path("waypipe").unspan();
            cfg.ffmpeg = r.path("ffmpeg").unspan();
            cfg.freerdp = r.path("freerdp").unspan();
            if let Some(v) = r
                .keyword("fastpath", crate::net::fastpath::FastpathMode::NAMES)
                .unspan()
            {
                cfg.fastpath = v;
            }
            cfg.config_weave_bin_dir = r.path("config_weave_bin_dir").unspan();
            cfg.firmware_dir = r.path("firmware_dir").unspan();
            if let Some(span) = r.field_span("gpu_address") {
                moved.push(super::Issue::at(span, GPU_ADDRESS_MOVED));
            }
            if let Some(v) = r.size("oci_chunk_size").unspan() {
                cfg.oci_chunk_size = v;
            }
            if let Some(v) = r.size("workspace_max_file").unspan() {
                cfg.workspace_max_file = v;
            }
        }
        for block in doc.blocks().filter(|b| b.kind() == "gpu") {
            let mut r = Reader::new(&block, &mut issues);
            let (Some(name), Some(address)) = (r.label(), r.required_string("address")) else {
                continue;
            };
            let qemu_binary = r.path("qemu_binary");
            if let Some(path) = &qemu_binary
                && !path.value.is_absolute()
            {
                r.issue_at(
                    path.span,
                    format!(
                        "host gpu \"{name}\": `qemu_binary` must be an absolute path, got `{}`",
                        path.value.display()
                    ),
                );
            }
            let mut qemu_env = r.string_map("qemu_env");
            qemu_env.retain(|(key, _)| match env_key_problem(key) {
                Some(why) => {
                    r.issue(format!(
                        "host gpu \"{name}\": `qemu_env` has {why}: `{key}`"
                    ));
                    false
                }
                None => true,
            });
            let gpu = HostGpu {
                name,
                address: address.value,
                vulkan_icd: r.path("vulkan_icd").unspan(),
                default: r.bool("default").unspan().unwrap_or(false),
                qemu_binary: qemu_binary.unspan(),
                qemu_env,
                keepawake: r
                    .keyword("keepawake", KeepAwake::NAMES)
                    .unspan()
                    .unwrap_or_default(),
            };
            if let Some(prev) = cfg
                .gpus
                .iter()
                .find(|g| g.name.eq_ignore_ascii_case(&gpu.name))
            {
                r.issue(format!(
                    "host gpu \"{}\" is declared twice (as \"{}\" too); names match case-insensitively",
                    gpu.name, prev.name
                ));
                continue;
            }
            if gpu.default
                && let Some(prev) = cfg.gpus.iter().find(|g| g.default)
            {
                r.issue(format!(
                    "host gpu \"{}\" is marked `default = true`, and so is \"{}\": at most one may be",
                    gpu.name, prev.name
                ));
            }
            cfg.gpus.push(gpu);
        }
        super::supersede(&mut issues, moved);
        Ok(finish(name, source, issues, Some(cfg))?)
    }

    /// The host GPU named `name`, matched case-insensitively.
    pub fn gpu(&self, name: &str) -> Option<&HostGpu> {
        self.gpus.iter().find(|g| g.name.eq_ignore_ascii_case(name))
    }

    /// The host GPU a lab gets when it names none, or names one this host
    /// lacks: the one marked `default = true`, else a lone declared GPU.
    pub fn default_gpu(&self) -> Option<&HostGpu> {
        self.gpus
            .iter()
            .find(|g| g.default)
            .or(match self.gpus.as_slice() {
                [only] => Some(only),
                _ => None,
            })
    }

    /// Where a segment's DNS forwards what it cannot answer (PRD §9.5):
    /// `dns_upstream` when set, else the host's own resolver. `None` leaves
    /// unmatched names NXDOMAIN.
    pub fn upstream_resolver(&self) -> Option<std::net::SocketAddr> {
        self.dns_upstream
            .as_deref()
            .and_then(parse_upstream)
            .or_else(host_resolver)
    }
}

fn parse_upstream(s: &str) -> Option<std::net::SocketAddr> {
    if let Ok(sa) = s.parse() {
        return Some(sa);
    }
    s.parse::<std::net::IpAddr>()
        .ok()
        .map(|ip| std::net::SocketAddr::new(ip, 53))
}

/// The host's own resolver, read from /etc/resolv.conf (PRD §9.5: upstream
/// defaults to the host's resolver).
fn host_resolver() -> Option<std::net::SocketAddr> {
    let content = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("nameserver")
            && let Ok(ip) = rest.trim().parse::<std::net::IpAddr>()
        {
            // A loopback systemd-resolved stub still works — it's the
            // host's resolver, reachable from the daemon's host sockets.
            return Some(std::net::SocketAddr::new(ip, 53));
        }
    }
    None
}

/// Percentage of free space on the filesystem holding `path`.
pub fn free_space_percent(path: &Path) -> Result<u8> {
    let stat = nix::sys::statvfs::statvfs(path)?;
    let total = stat.blocks() as u64;
    if total == 0 {
        return Ok(100);
    }
    let avail = stat.blocks_available() as u64;
    Ok(((avail * 100) / total) as u8)
}

/// Periodic free-space watchdog (PRD §8.1): emits via `alert` when the
/// filesystem holding `path` drops below `threshold_percent` free —
/// edge-triggered, re-arming once space recovers. Exits when `cancel`
/// fires (daemon shutdown).
pub fn spawn_disk_watchdog(
    path: std::path::PathBuf,
    threshold_percent: u8,
    period: std::time::Duration,
    cancel: tokio_util::sync::CancellationToken,
    alert: impl Fn(u8) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut alerted = false;
        loop {
            if let Ok(free) = free_space_percent(&path) {
                if free < threshold_percent && !alerted {
                    alerted = true;
                    alert(free);
                } else if free >= threshold_percent {
                    alerted = false;
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(period) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_absent() {
        let cfg = HostConfig::default();
        assert_eq!(cfg.subnet_pool.to_string(), "10.213.0.0/16");
        assert_eq!(cfg.dns_suffix, "vmlab.internal");
        assert_eq!(cfg.disk_low_percent, 10);
        assert_eq!(cfg.oci_chunk_size, 512 << 20);
        assert_eq!(cfg.workspace_max_file, DEFAULT_WORKSPACE_MAX_FILE);
        assert_eq!(cfg.firmware_dir, None);
        assert!(cfg.gpus.is_empty() && cfg.default_gpu().is_none());
    }

    #[test]
    fn parses_overrides() {
        let cfg = HostConfig::parse(
            r#"import <vmlab-host.wcl>
host {
  subnet_pool      = "10.99.0.0/16"
  dns_suffix       = "lab.local"
  disk_low_percent = 5
  psk              = "sekrit"
  trunk_port       = 13948
  oci_chunk_size   = 128MiB
  fastpath         = "sockmap"
  workspace_max_file = 2GiB
  firmware_dir     = "/opt/edk2"
  waypipe          = "/opt/waypipe/bin/waypipe"
  ffmpeg           = "/opt/ffmpeg/bin/ffmpeg"
  freerdp          = "/usr/bin/sdl-freerdp3"
}
"#,
            "<test>",
        )
        .unwrap();
        assert_eq!(cfg.subnet_pool.to_string(), "10.99.0.0/16");
        assert_eq!(cfg.dns_suffix, "lab.local");
        assert_eq!(cfg.disk_low_percent, 5);
        assert_eq!(cfg.psk.as_deref(), Some("sekrit"));
        assert_eq!(cfg.trunk_port, 13948);
        assert_eq!(cfg.oci_chunk_size, 128 << 20);
        assert_eq!(cfg.fastpath, crate::net::fastpath::FastpathMode::Sockmap);
        assert_eq!(cfg.workspace_max_file, 2 << 30);
        assert_eq!(cfg.firmware_dir.as_deref(), Some(Path::new("/opt/edk2")));
        assert_eq!(
            cfg.waypipe.as_deref(),
            Some(Path::new("/opt/waypipe/bin/waypipe"))
        );
        assert_eq!(
            cfg.ffmpeg.as_deref(),
            Some(Path::new("/opt/ffmpeg/bin/ffmpeg"))
        );
        assert_eq!(
            cfg.freerdp.as_deref(),
            Some(Path::new("/usr/bin/sdl-freerdp3"))
        );
    }

    /// Host GPUs are named by their label, in declaration order, and the one
    /// marked `default = true` is the default.
    #[test]
    fn parses_named_host_gpus() {
        let cfg = HostConfig::parse(
            r#"import <vmlab-host.wcl>
gpu "AMD1"    { address = "0000:72:00.0" }
gpu "Nvidia1" { address = "0000:01:00.0"  default = true }
gpu "Nvidia2" { address = "0000:05:00.0"  vulkan_icd = "/usr/share/vulkan/icd.d/nvidia_icd.json" }
"#,
            "<test>",
        )
        .unwrap();
        let names: Vec<&str> = cfg.gpus.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["AMD1", "Nvidia1", "Nvidia2"]);
        assert_eq!(cfg.gpus[0].address, "0000:72:00.0");
        assert_eq!(
            cfg.gpus[2].vulkan_icd.as_deref(),
            Some(Path::new("/usr/share/vulkan/icd.d/nvidia_icd.json"))
        );
        assert_eq!(cfg.default_gpu().unwrap().name, "Nvidia1");
        assert_eq!(cfg.gpu("nvidia2").unwrap().name, "Nvidia2");
        assert!(cfg.gpu("Nope").is_none());
    }

    /// A host GPU carries its own QEMU and the variables that QEMU runs
    /// under; a GPU that names neither has neither.
    #[test]
    fn parses_a_host_gpus_qemu_binary_and_env() {
        let cfg = HostConfig::parse(
            r#"import <vmlab-host.wcl>
gpu "AMD1" { address = "0000:72:00.0" }
gpu "Nvidia1" {
  address     = "0000:01:00.0"
  default     = true
  qemu_binary = "/opt/qemu-vmlab/bin/qemu-system-x86_64"
  qemu_env    = { VKR_DEBUG: "validate", RENDER_SERVER_EXEC_PATH: "/opt/virgl/virgl_render_server" }
}
"#,
            "<test>",
        )
        .unwrap();
        let nv = cfg.gpu("Nvidia1").unwrap();
        assert_eq!(
            nv.qemu_binary.as_deref(),
            Some(Path::new("/opt/qemu-vmlab/bin/qemu-system-x86_64"))
        );
        assert_eq!(
            nv.qemu_env,
            [
                (
                    "RENDER_SERVER_EXEC_PATH".to_string(),
                    "/opt/virgl/virgl_render_server".to_string()
                ),
                ("VKR_DEBUG".into(), "validate".into()),
            ]
        );
        let amd = cfg.gpu("AMD1").unwrap();
        assert!(amd.qemu_binary.is_none() && amd.qemu_env.is_empty());
    }

    /// `keepawake` reads `auto`, `on` or `off`, defaults to `auto`, and
    /// refuses anything else by name.
    #[test]
    fn parses_a_host_gpus_keepawake() {
        let cfg = HostConfig::parse(
            r#"import <vmlab-host.wcl>
gpu "AMD1"    { address = "0000:72:00.0"  keepawake = "on" }
gpu "Nvidia1" { address = "0000:01:00.0"  default = true }
gpu "Nvidia2" { address = "0000:05:00.0"  keepawake = "off" }
"#,
            "<test>",
        )
        .unwrap();
        let modes: Vec<KeepAwake> = cfg.gpus.iter().map(|g| g.keepawake).collect();
        assert_eq!(modes, [KeepAwake::On, KeepAwake::Auto, KeepAwake::Off]);
        let err = host_err("gpu \"Nvidia1\" { address = \"0000:01:00.0\"  keepawake = \"yes\" }\n");
        assert!(
            err.contains("`keepawake` must be one of auto, on, off, got `yes`"),
            "{err}"
        );
    }

    /// A relative `qemu_binary` would resolve against whatever directory
    /// the daemon runs in, and a `qemu_env` value must be a string.
    #[test]
    fn rejects_a_relative_qemu_binary_and_a_non_string_env_value() {
        let err = host_err(
            "gpu \"Nvidia1\" { address = \"0000:01:00.0\"  qemu_binary = \"bin/qemu\" }\n",
        );
        assert!(
            err.contains(
                "host gpu \"Nvidia1\": `qemu_binary` must be an absolute path, got `bin/qemu`"
            ),
            "{err}"
        );
        let err = host_err("gpu \"Nvidia1\" { address = \"0000:01:00.0\"  qemu_env = { N: 3 } }\n");
        assert!(err.contains("`qemu_env.N` must be a string"), "{err}");
        let err = host_err("gpu \"Nvidia1\" { address = \"0000:01:00.0\"  qemu_env = \"A=1\" }\n");
        assert!(
            err.contains("`qemu_env` must be a record of strings"),
            "{err}"
        );
    }

    /// An environment name may be neither empty nor carry `=`, which ends
    /// a name in `environ`.
    #[test]
    fn an_empty_or_equals_bearing_env_key_is_refused() {
        assert_eq!(env_key_problem(""), Some("an empty variable name"));
        assert_eq!(
            env_key_problem("A=B"),
            Some("a variable name containing `=`")
        );
        assert_eq!(env_key_problem("VKR_DEBUG"), None);
    }

    /// One declared GPU is the default without saying so; two unmarked
    /// ones leave no default.
    #[test]
    fn a_lone_host_gpu_is_the_default() {
        let one = HostConfig::parse(
            "import <vmlab-host.wcl>\ngpu \"Intel1\" { address = \"0000:00:02.0\" }\n",
            "<t>",
        )
        .unwrap();
        assert_eq!(one.default_gpu().unwrap().name, "Intel1");
        let two = HostConfig::parse(
            "import <vmlab-host.wcl>\n\
             gpu \"A\" { address = \"0000:00:02.0\" }\n\
             gpu \"B\" { address = \"0000:01:00.0\" }\n",
            "<t>",
        )
        .unwrap();
        assert!(two.default_gpu().is_none());
    }

    /// Names match case-insensitively, so two that differ only in case are
    /// one name declared twice; and at most one GPU is the default.
    #[test]
    fn rejects_duplicate_names_and_two_defaults() {
        let err = host_err(
            "gpu \"nvidia1\" { address = \"0000:01:00.0\" }\n\
             gpu \"Nvidia1\" { address = \"0000:05:00.0\" }\n",
        );
        assert!(err.starts_with("1 error(s)"), "{err}");
        assert!(
            err.contains(
                "config.wcl:3:1: host gpu \"Nvidia1\" is declared twice (as \"nvidia1\" too)"
            ),
            "{err}"
        );
        let err = host_err(
            "gpu \"A\" { address = \"0000:01:00.0\"  default = true }\n\
             gpu \"B\" { address = \"0000:05:00.0\"  default = true }\n",
        );
        assert!(
            err.contains("host gpu \"B\" is marked `default = true`, and so is \"A\""),
            "{err}"
        );
        let err = host_err("gpu \"A\" { default = true }\n");
        assert!(err.contains("address"), "{err}");
    }

    /// v0.24's `gpu_address` is refused with where it went, not the
    /// schema's bare unknown-field complaint.
    #[test]
    fn gpu_address_is_refused_with_the_migration() {
        let err = host_err("host { gpu_address = \"0000:72:00.0\" }\n");
        assert!(err.starts_with("1 error(s)"), "{err}");
        assert!(
            err.contains(&format!("config.wcl:2:8: {GPU_ADDRESS_MOVED}")),
            "{err}"
        );
    }

    /// The diagnostics a host-config author now gets — same wording as a
    /// lab file, anchored at the line (ADR-0006).
    fn host_err(body: &str) -> String {
        let source = format!("import <vmlab-host.wcl>\n{body}");
        let err = HostConfig::parse(&source, "config.wcl").expect_err("should be rejected");
        format!("{err:#}")
    }

    #[test]
    fn rejects_bad_values() {
        assert_eq!(
            host_err("host { disk_low_percent = 200 }\n"),
            "1 error(s) in config.wcl\n  \
             config.wcl:2:8: `disk_low_percent` must be between 0 and 100, got 200"
        );
        assert!(
            host_err("host { trunk_port = 0 }\n")
                .contains("`trunk_port` must be between 1 and 65535, got 0")
        );
        assert!(
            host_err("host { trunk_port = 70000 }\n")
                .contains("`trunk_port` must be between 1 and 65535, got 70000")
        );
        assert!(
            host_err("host { fastpath = \"fast\" }\n")
                .contains("`fastpath` must be one of auto, off, sockmap, afxdp, got `fast`")
        );
        assert!(
            host_err("host { subnet_pool = \"10.213\" }\n").contains("malformed CIDR `10.213`")
        );
        assert!(
            host_err("host { dns_suffix = 3 }\n")
                .contains("`dns_suffix` must be a string, got an integer")
        );
        assert!(HostConfig::parse("host { }\n", "<t>").is_err());
    }

    #[test]
    fn every_mistake_in_a_host_config_is_reported_at_once() {
        let err = host_err("host {\n  trunk_port = 0\n  fastpath = \"fast\"\n}\n");
        assert!(err.starts_with("2 error(s) in config.wcl"), "{err}");
        assert!(
            err.contains("config.wcl:3:3: `trunk_port` must be"),
            "{err}"
        );
        assert!(err.contains("config.wcl:4:3: `fastpath` must be"), "{err}");
    }

    /// A field the host schema does not name is the schema's to reject, not
    /// the extractor's — but it must still reach the user, positioned.
    #[test]
    fn an_unknown_field_is_rejected_by_the_schema() {
        let err = host_err("host { bogus = 1 }\n");
        assert!(err.contains("bogus"), "{err}");
        assert!(err.contains("config.wcl:2:"), "unpositioned: {err}");
    }

    /// Spans survive as spans, not just as rendered text (ADR-0006, story 19).
    #[test]
    fn a_host_config_issue_carries_a_span_a_surface_can_use() {
        let source = "import <vmlab-host.wcl>\nhost { trunk_port = 0 }\n";
        let err = HostConfig::parse(source, "config.wcl").unwrap_err();
        let diag = err
            .downcast_ref::<crate::config::block::IssueError>()
            .expect("host config errors carry their issues")
            .diagnostic();
        let span = diag.issues[0].span.expect("positioned");
        assert_eq!(
            &source[span.offset()..span.offset() + span.len()],
            "trunk_port = 0"
        );
    }

    /// The one spelling of each fastpath mode: what the extractor accepts is
    /// what `FastpathMode::parse` accepts, by construction.
    #[test]
    fn fastpath_accepts_exactly_the_modes_parse_knows() {
        for (name, mode) in crate::net::fastpath::FastpathMode::NAMES {
            let cfg = HostConfig::parse(
                &format!("import <vmlab-host.wcl>\nhost {{ fastpath = \"{name}\" }}\n"),
                "<t>",
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(cfg.fastpath, *mode);
        }
    }

    #[test]
    fn free_space_works() {
        let pct = free_space_percent(Path::new("/")).unwrap();
        assert!(pct <= 100);
    }

    #[tokio::test]
    async fn watchdog_edge_triggers() {
        // Threshold 101% can't be satisfied → alert exactly once per arm.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = spawn_disk_watchdog(
            std::env::temp_dir(),
            101,
            std::time::Duration::from_millis(10),
            cancel.clone(),
            move |free| {
                let _ = tx.send(free);
            },
        );
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap();
        assert!(first.is_some());
        // No second alert while still below threshold.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err());
        // Cancellation stops the loop (joinable, not aborted).
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("watchdog exits on cancel")
            .unwrap();
    }
}
