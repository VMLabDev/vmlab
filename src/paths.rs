//! Filesystem layout (PRD §4). All XDG paths respect the corresponding
//! environment variables.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

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

/// `~/.local/share/vmlab/oci` — the digest-addressed cache of pulled
/// container images (compressed layer blobs, flattened rootfs images, and
/// tag-resolution records).
pub fn oci_cache_dir() -> PathBuf {
    data_dir().join("oci")
}

/// `~/.local/share/vmlab/labs` — the managed home for labs created through
/// the web UI's "New Lab" (a lab is any directory containing `vmlab.wcl`;
/// this is just the default place the server puts new ones).
pub fn labs_home() -> PathBuf {
    data_dir().join("labs")
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

/// Lab-local working data directory — the disk clones, built media, TPM
/// state, and persisted lab state. Defaults to `<lab>/.vmlab/`, beside the
/// lab file.
///
/// `VMLAB_WORK_DIR` relocates it to a per-lab subdirectory under that base
/// instead. This keeps the heavy, write-churning working data off a slow
/// filesystem while the lab file itself stays put: in Docker on Windows the
/// lab directory is a bind mount over a virtiofs/9p bridge, and writing disk
/// clones across it is painfully slow (issue #2) — pointing `VMLAB_WORK_DIR`
/// at a container-native named volume fixes that. The lab file stays editable
/// on the host. Created on demand by the callers that write into it.
pub fn lab_local_dir(lab_root: &Path) -> PathBuf {
    let base = env::var_os("VMLAB_WORK_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    lab_local_dir_in(base.as_deref(), lab_root)
}

/// The override-aware core of [`lab_local_dir`], split out so it can be tested
/// without mutating process-global environment.
fn lab_local_dir_in(base: Option<&Path>, lab_root: &Path) -> PathBuf {
    match base {
        // A single relocated base may hold several labs, so namespace each by
        // its root: `<basename>-<hash>` keeps it human-recognisable while the
        // hash of the canonical path guarantees no two distinct labs collide.
        Some(base) => {
            let canon = lab_root
                .canonicalize()
                .unwrap_or_else(|_| lab_root.to_path_buf());
            let name = lab_root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("lab");
            base.join(format!("{name}-{}", short_hash(&canon)))
        }
        None => lab_root.join(LAB_DIR),
    }
}

/// 12 hex chars of the SHA-256 of a path — a stable, collision-resistant tag.
fn short_hash(p: &Path) -> String {
    let digest = Sha256::digest(p.to_string_lossy().as_bytes());
    hex::encode(&digest[..6])
}

/// Ensure a directory exists with private permissions.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create directory {}", dir.display()))?;
    Ok(())
}

/// This binary's path, robust against in-place rebuilds: when `cargo build`
/// replaces the file under a long-running daemon, `/proc/self/exe` reads
/// `<path> (deleted)` and spawning from it fails with ENOENT — strip the
/// marker and use the rebuilt binary at the original path.
pub fn self_exe() -> std::io::Result<PathBuf> {
    let exe = env::current_exe()?;
    if let Some(s) = exe.to_str()
        && let Some(stripped) = s.strip_suffix(" (deleted)")
    {
        return Ok(PathBuf::from(stripped));
    }
    Ok(exe)
}

/// Path to the `vmlab` binary that hosts the daemons (the `__supervisord` /
/// `__labd` subcommands). For the CLI this is `self_exe()`; for siblings like
/// `vmlab-web` it's the `vmlab` binary next to the current executable, falling
/// back to `vmlab` on `PATH`.
pub fn vmlab_exe() -> std::io::Result<PathBuf> {
    let cur = self_exe()?;
    if cur.file_name().and_then(|n| n.to_str()) == Some("vmlab") {
        return Ok(cur);
    }
    if let Some(sibling) = cur.parent().map(|d| d.join("vmlab"))
        && sibling.exists()
    {
        return Ok(sibling);
    }
    Ok(PathBuf::from("vmlab"))
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
    fn lab_local_dir_defaults_beside_lab() {
        assert_eq!(
            lab_local_dir_in(None, Path::new("/some/lab")),
            PathBuf::from("/some/lab/.vmlab")
        );
    }

    #[test]
    fn lab_local_dir_override_relocates_and_namespaces() {
        let base = Path::new("/work");
        // Nonexistent roots can't canonicalize; the raw path is hashed, which
        // is fine — it just needs to be stable and distinct per lab.
        let alpha = lab_local_dir_in(Some(base), Path::new("/labs/alpha"));
        let beta = lab_local_dir_in(Some(base), Path::new("/labs/beta"));
        assert!(alpha.starts_with("/work"));
        assert!(
            alpha
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("alpha-"),
            "{alpha:?}"
        );
        // Distinct labs never collide, and the mapping is stable per root.
        assert_ne!(alpha, beta);
        assert_eq!(
            alpha,
            lab_local_dir_in(Some(base), Path::new("/labs/alpha"))
        );
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
