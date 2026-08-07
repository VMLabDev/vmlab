//! Host-level daemon configuration (PRD §9.4 pool override, §9.5 suffix,
//! §9.2 PSK, §8.1 watchdog threshold, §11 viewer, §6.4 chunk size), read
//! from `~/.config/vmlab/config.wcl`. Every field optional; defaults apply.

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
    pub oci_chunk_size: u64,
    /// Network fast-path tier selection (§9.1 substitutable backend).
    pub fastpath: crate::net::fastpath::FastpathMode,
    /// Directory holding config-weave guest binaries; `None` = env var /
    /// XDG default (see `labd::playbook::resolve_bin_dir`).
    pub config_weave_bin_dir: Option<std::path::PathBuf>,
    /// The file vmlab writes its managed SSH block into; `None` =
    /// `~/.ssh/config` (§19.7). A **location** knob with one code path behind
    /// it, never an on/off with two: the `ssh -G` check still runs, so a
    /// block redirected somewhere OpenSSH does not read warns honestly rather
    /// than pretending to work.
    pub ssh_config: Option<std::path::PathBuf>,
    /// The workspace syncer's per-file size guard (PRD §19.6). Host config
    /// rather than a `@dev` argument: the cap is about this developer's link
    /// to this guest, not about the lab everyone shares — and the refusal
    /// message names it, because "raise the cap" has to point somewhere.
    pub workspace_max_file: u64,
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
            ssh_config: None,
            workspace_max_file: DEFAULT_WORKSPACE_MAX_FILE,
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
            if let Some(v) = r
                .keyword("fastpath", crate::net::fastpath::FastpathMode::NAMES)
                .unspan()
            {
                cfg.fastpath = v;
            }
            cfg.config_weave_bin_dir = r.path("config_weave_bin_dir").unspan();
            cfg.ssh_config = r.path("ssh_config").unspan();
            if let Some(v) = r.size("oci_chunk_size").unspan() {
                cfg.oci_chunk_size = v;
            }
            if let Some(v) = r.size("workspace_max_file").unspan() {
                cfg.workspace_max_file = v;
            }
        }
        Ok(finish(name, source, issues, Some(cfg))?)
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
        assert_eq!(cfg.workspace_max_file, DEFAULT_WORKSPACE_MAX_FILE);
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
