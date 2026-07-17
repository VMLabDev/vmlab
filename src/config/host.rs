//! Host-level daemon configuration (PRD §9.4 pool override, §9.5 suffix,
//! §9.2 PSK, §8.1 watchdog threshold, §11 viewer, §6.4 chunk size), read
//! from `~/.config/vmlab/config.wcl`. Every field optional; defaults apply.

use std::path::Path;

use anyhow::{Result, anyhow};
use wcl_lang::{Document, Environment, Registry, Value, disk_loader};

pub const HOST_SCHEMA_WCL: &str = include_str!("host_schema.wcl");

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
    pub oci_chunk_size: u64,
    /// Network fast-path tier selection (§9.1 substitutable backend).
    pub fastpath: crate::net::fastpath::FastpathMode,
    /// Directory holding config-weave guest binaries; `None` = env var /
    /// XDG default (see `labd::playbook::resolve_bin_dir`).
    pub config_weave_bin_dir: Option<std::path::PathBuf>,
}

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
            oci_chunk_size: crate::oci::chunking::DEFAULT_CHUNK_SIZE,
            fastpath: crate::net::fastpath::FastpathMode::Auto,
            config_weave_bin_dir: None,
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
        let errors = doc.schema_errors();
        if !errors.is_empty() {
            let msgs: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            return Err(anyhow!("schema errors in {name}: {}", msgs.join("; ")));
        }

        let mut cfg = Self::default();
        let Some(block) = doc.blocks().find(|b| b.kind() == "host") else {
            return Ok(cfg);
        };
        let get_str = |field: &str| -> Result<Option<String>> {
            match block.field(field) {
                None => Ok(None),
                Some(f) => match f.value() {
                    Ok(Value::Utf8(s)) => Ok(Some(s.clone())),
                    Ok(Value::None) => Ok(None),
                    Ok(other) => Err(anyhow!(
                        "host config: `{field}` must be a string, got {other:?}"
                    )),
                    Err(e) => Err(anyhow!("host config: cannot evaluate `{field}`: {e}")),
                },
            }
        };
        if let Some(pool) = get_str("subnet_pool")? {
            cfg.subnet_pool = pool
                .parse()
                .map_err(|_| anyhow!("host config: malformed subnet_pool `{pool}`"))?;
        }
        if let Some(s) = get_str("dns_suffix")? {
            cfg.dns_suffix = s;
        }
        cfg.dns_upstream = get_str("dns_upstream")?;
        if let Some(f) = block.field("disk_low_percent") {
            match f.value() {
                Ok(Value::I64(n)) if (0..=100).contains(n) => cfg.disk_low_percent = *n as u8,
                Ok(Value::None) => {}
                Ok(other) => {
                    return Err(anyhow!(
                        "host config: disk_low_percent must be 0..=100, got {other:?}"
                    ));
                }
                Err(e) => return Err(anyhow!("host config: {e}")),
            }
        }
        cfg.psk = get_str("psk")?;
        if let Some(f) = block.field("trunk_port") {
            match f.value() {
                Ok(Value::I64(n)) if (1..=65535).contains(n) => cfg.trunk_port = *n as u16,
                Ok(Value::None) => {}
                Ok(other) => {
                    return Err(anyhow!(
                        "host config: trunk_port must be 1..=65535, got {other:?}"
                    ));
                }
                Err(e) => return Err(anyhow!("host config: {e}")),
            }
        }
        cfg.viewer = get_str("viewer")?;
        if let Some(s) = get_str("fastpath")? {
            cfg.fastpath = crate::net::fastpath::FastpathMode::parse(&s).ok_or_else(|| {
                anyhow!("host config: fastpath must be auto|off|sockmap|afxdp, got `{s}`")
            })?;
        }
        cfg.config_weave_bin_dir = get_str("config_weave_bin_dir")?.map(std::path::PathBuf::from);
        if let Some(f) = block.field("oci_chunk_size") {
            match f.value() {
                Ok(Value::I64(n)) if *n >= 0 => cfg.oci_chunk_size = *n as u64,
                Ok(Value::None) => {}
                Ok(other) => {
                    return Err(anyhow!(
                        "host config: oci_chunk_size must be a non-negative byte size, got {other:?}"
                    ));
                }
                Err(e) => return Err(anyhow!("host config: {e}")),
            }
        }
        Ok(cfg)
    }
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
    }

    #[test]
    fn rejects_bad_values() {
        assert!(
            HostConfig::parse(
                "import <vmlab-host.wcl>\nhost { disk_low_percent = 200 }\n",
                "<t>"
            )
            .is_err()
        );
        assert!(
            HostConfig::parse("import <vmlab-host.wcl>\nhost { trunk_port = 0 }\n", "<t>").is_err()
        );
        assert!(
            HostConfig::parse(
                "import <vmlab-host.wcl>\nhost { trunk_port = 70000 }\n",
                "<t>"
            )
            .is_err()
        );
        assert!(
            HostConfig::parse(
                "import <vmlab-host.wcl>\nhost { fastpath = \"fast\" }\n",
                "<t>"
            )
            .is_err()
        );
        assert!(HostConfig::parse("host { }\n", "<t>").is_err());
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
