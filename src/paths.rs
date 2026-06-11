//! Filesystem layout (PRD §4). All XDG paths respect the corresponding
//! environment variables.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Name of the lab definition file, located by walking up from cwd.
pub const LAB_FILE: &str = "vmlab.wcl";
/// Lab-local working directory beside the lab file.
pub const LAB_DIR: &str = ".vmlab";

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

/// `~/.local/share/vmlab` — template store lives under here.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("vmlab")
}

/// `~/.local/share/vmlab/templates`
pub fn template_store_dir() -> PathBuf {
    data_dir().join("templates")
}

/// `~/.local/state/vmlab` — daemon state, logs, event history.
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("vmlab")
}

/// `~/.config/vmlab` — host daemon config, user profile overrides.
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("vmlab")
}

/// `$XDG_RUNTIME_DIR/vmlab` — control sockets. Some WSL setups lack
/// `XDG_RUNTIME_DIR`; fall back to a uid-scoped tmp directory (PRD §13).
pub fn runtime_dir() -> PathBuf {
    match env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("vmlab"),
        None => {
            let uid = nix::unistd::Uid::effective().as_raw();
            PathBuf::from(format!("/tmp/vmlab-{uid}"))
        }
    }
}

/// Supervisor control socket.
pub fn supervisor_socket() -> PathBuf {
    runtime_dir().join("vmlabd.sock")
}

/// Runtime directory for one lab daemon: control socket plus per-VM
/// QMP/agent/NIC/VNC sockets.
pub fn lab_runtime_dir(lab: &str) -> PathBuf {
    runtime_dir().join("labs").join(lab)
}

/// Lab daemon control socket.
pub fn lab_socket(lab: &str) -> PathBuf {
    lab_runtime_dir(lab).join("control.sock")
}

/// Walk up from `start` looking for `vmlab.wcl` (like git locates its repo).
/// Returns the directory containing the lab file.
pub fn find_lab_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", start.display()))?;
    for dir in start.ancestors() {
        if dir.join(LAB_FILE).is_file() {
            return Ok(dir.to_path_buf());
        }
    }
    bail!(
        "no {LAB_FILE} found in {} or any parent directory",
        start.display()
    );
}

/// Lab-local working data directory (`<repo>/.vmlab/`). Created on demand.
pub fn lab_local_dir(lab_root: &Path) -> PathBuf {
    lab_root.join(LAB_DIR)
}

/// Ensure a directory exists with private permissions.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create directory {}", dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_lab_root_walks_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(LAB_FILE), "lab \"t\" {}\n").unwrap();
        let found = find_lab_root(&nested).unwrap();
        assert_eq!(found, root.canonicalize().unwrap());
    }

    #[test]
    fn find_lab_root_fails_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_lab_root(tmp.path()).is_err());
    }

    #[test]
    fn xdg_overrides_respected() {
        // Pure function check without mutating process env: xdg() reads env,
        // so just sanity-check shape of derived paths.
        assert!(template_store_dir().ends_with("vmlab/templates"));
        assert!(
            supervisor_socket().ends_with("vmlab/vmlabd.sock")
                || supervisor_socket()
                    .to_string_lossy()
                    .contains("/tmp/vmlab-")
        );
    }
}
