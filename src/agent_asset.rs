//! Locate the vmlab-agent guest binaries — the in-guest terminal/exec/file
//! agent the template build bakes into every image (built per guest target
//! by `guest/build-agent.sh`, shipped as `agent/<os>-<arch>/vmlab-agent`
//! plus `VERSION`).
//!
//! Lookup order matches [`crate::guest_asset`]:
//!  1. `$VMLAB_GUEST_ASSET_DIR/agent/<os>-<arch>/` — explicit override (dev
//!     builds point it at `guest/dist/`).
//!  2. `/usr/share/vmlab/guest/agent/<os>-<arch>/` — system-wide install.
//!  3. `~/.local/share/vmlab/guest/agent/<os>-<arch>/` — the per-user data
//!     dir ([`crate::paths::data_dir`]).

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

const VERSION_FILE: &str = "VERSION";

/// Guest OS flavour of the agent binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOs {
    Linux,
    Windows,
}

impl AgentOs {
    pub fn key(self) -> &'static str {
        match self {
            AgentOs::Linux => "linux",
            AgentOs::Windows => "windows",
        }
    }

    fn binary(self) -> &'static str {
        match self {
            AgentOs::Linux => "vmlab-agent",
            AgentOs::Windows => "vmlab-agent.exe",
        }
    }
}

/// A resolved agent binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAsset {
    pub path: PathBuf,
    /// Content of the optional VERSION file (`"unknown"` when absent).
    pub version: String,
}

impl AgentAsset {
    /// The binary bytes (small — static, opt-level=z).
    pub fn read(&self) -> Result<Vec<u8>> {
        fs::read(&self.path).with_context(|| format!("reading {}", self.path.display()))
    }
}

/// Find the agent binary for a guest `os` + `arch` (e.g. `x86_64`), or fail
/// with every searched path listed.
pub fn ensure_agent_asset(os: AgentOs, arch: &str) -> Result<AgentAsset> {
    find_in(&candidate_dirs(), os, arch)
}

/// The base directories searched, in priority order (same roots as the
/// micro-VM boot asset).
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = env::var_os("VMLAB_GUEST_ASSET_DIR").filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    dirs.push(PathBuf::from("/usr/share/vmlab/guest"));
    dirs.push(crate::paths::data_dir().join("guest"));
    dirs
}

fn find_in(dirs: &[PathBuf], os: AgentOs, arch: &str) -> Result<AgentAsset> {
    let key = format!("{}-{arch}", os.key());
    let mut searched = Vec::new();
    for base in dirs {
        let dir = base.join("agent").join(&key);
        let path = dir.join(os.binary());
        if path.is_file() {
            let version = fs::read_to_string(dir.join(VERSION_FILE))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            return Ok(AgentAsset { path, version });
        }
        searched.push(dir);
    }
    let searched = searched
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "no vmlab-agent binary for {key}; searched: {searched}. Build one with \
         `guest/build-agent.sh {key}` and install it into one of those directories \
         (or point VMLAB_GUEST_ASSET_DIR at guest/dist)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_asset(dir: &std::path::Path, key: &str, binary: &str, version: Option<&str>) {
        let d = dir.join("agent").join(key);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(binary), b"binary").unwrap();
        if let Some(v) = version {
            fs::write(d.join(VERSION_FILE), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn finds_linux_and_windows_flavours() {
        let tmp = tempfile::tempdir().unwrap();
        write_asset(tmp.path(), "linux-x86_64", "vmlab-agent", Some("agent=abc"));
        write_asset(tmp.path(), "windows-x86_64", "vmlab-agent.exe", None);
        let dirs = vec![tmp.path().to_path_buf()];

        let linux = find_in(&dirs, AgentOs::Linux, "x86_64").unwrap();
        assert!(linux.path.ends_with("agent/linux-x86_64/vmlab-agent"));
        assert_eq!(linux.version, "agent=abc");
        assert_eq!(linux.read().unwrap(), b"binary");

        let win = find_in(&dirs, AgentOs::Windows, "x86_64").unwrap();
        assert!(win.path.ends_with("agent/windows-x86_64/vmlab-agent.exe"));
        assert_eq!(win.version, "unknown");
    }

    #[test]
    fn priority_order_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let hi = tmp.path().join("hi");
        let lo = tmp.path().join("lo");
        write_asset(&hi, "linux-aarch64", "vmlab-agent", Some("hi"));
        write_asset(&lo, "linux-aarch64", "vmlab-agent", Some("lo"));
        let got = find_in(&[hi, lo], AgentOs::Linux, "aarch64").unwrap();
        assert_eq!(got.version, "hi");
    }

    #[test]
    fn missing_asset_error_lists_paths_and_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let err = find_in(&[tmp.path().to_path_buf()], AgentOs::Windows, "x86_64").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("windows-x86_64"), "{msg}");
        assert!(msg.contains("build-agent.sh windows-x86_64"), "{msg}");
        assert!(
            msg.contains(&tmp.path().join("agent").display().to_string()),
            "{msg}"
        );
    }
}
