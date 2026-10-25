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

/// The user's home directory. `/` when `HOME` is unset, which is what every
/// XDG fallback below already assumes.
pub fn home() -> PathBuf {
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

/// `~/.local/share/config-weave/bin` — where config-weave's `just install`
/// drops its guest binaries; the default lookup dir for playbook runs.
pub fn config_weave_bin_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("config-weave/bin")
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

/// The longest path a unix socket can bind: `sun_path` is 108 bytes and
/// holds the terminating NUL.
const SUN_PATH_MAX: usize = 107;

/// Longest socket file name bound in a machine's runtime directory:
/// `app-<u32>-conns.sock` (the shared-app connection socket, §20.3).
const MACHINE_SOCKET_NAME_MAX: usize = "app-4294967295-conns.sock".len();

/// What a machine runtime directory adds below its lab's runtime directory at
/// worst: `/containers/` (the longer kind directory), a hashed machine
/// component (see [`fitted`]) and the longest socket name.
const MACHINE_DIR_RESERVE: usize =
    "/containers/".len() + HASHED_COMPONENT_LEN + 1 + MACHINE_SOCKET_NAME_MAX;

/// `_` plus 12 hex characters.
const HASHED_COMPONENT_LEN: usize = 13;

/// `parent/name` when `reserve` more bytes below it still fit in a socket
/// path, else `parent/_<12 hex of name>`.
///
/// Lab and machine names are DNS labels of up to 63 characters each, so the
/// plain path does not always fit in `sun_path`: QEMU then fails to bind its
/// QMP or VNC socket. A DNS label cannot start with `_`, so a hashed component
/// never collides with a real name. With both components hashed every socket
/// fits as long as the runtime base ([`runtime_dir`]) is at most 37 bytes;
/// `/run/user/<uid>/vmlab` is at most 26.
fn fitted(parent: &Path, name: &str, reserve: usize) -> PathBuf {
    let plain = parent.join(name);
    if plain.as_os_str().len() + reserve <= SUN_PATH_MAX {
        return plain;
    }
    let digest = Sha256::digest(name.as_bytes());
    parent.join(format!("_{}", hex::encode(&digest[..6])))
}

/// Runtime directory for one lab daemon: control socket plus the machine
/// runtime directories ([`machine_runtime_dir`]).
pub fn lab_runtime_dir(lab: &str) -> PathBuf {
    lab_runtime_dir_in(&runtime_dir(), lab)
}

/// [`lab_runtime_dir`] against an explicit runtime base, for tests.
pub fn lab_runtime_dir_in(runtime: &Path, lab: &str) -> PathBuf {
    fitted(&runtime.join("labs"), lab, MACHINE_DIR_RESERVE)
}

/// Which kind of machine a runtime directory belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineKind {
    Vm,
    Container,
}

impl MachineKind {
    fn dir(self) -> &'static str {
        match self {
            MachineKind::Vm => "vms",
            MachineKind::Container => "containers",
        }
    }
}

/// Runtime directory for one machine's sockets (QMP, agent, VNC, NICs,
/// virtiofsd, terminals, shared apps):
/// `<lab runtime>/{vms,containers}/<machine>`, with the machine component
/// hashed when its longest socket would not fit in `sun_path`.
pub fn machine_runtime_dir(lab: &str, kind: MachineKind, machine: &str) -> PathBuf {
    machine_runtime_dir_in(&runtime_dir(), lab, kind, machine)
}

/// [`machine_runtime_dir`] against an explicit runtime base, for tests.
pub fn machine_runtime_dir_in(
    runtime: &Path,
    lab: &str,
    kind: MachineKind,
    machine: &str,
) -> PathBuf {
    let parent = lab_runtime_dir_in(runtime, lab).join(kind.dir());
    fitted(&parent, machine, 1 + MACHINE_SOCKET_NAME_MAX)
}

/// Lab daemon control socket.
pub fn lab_socket(lab: &str) -> PathBuf {
    lab_runtime_dir(lab).join("control.sock")
}

/// Where one lab's `smbd` keeps the directories it binds unix sockets in —
/// `private dir` (`msg.sock/<pid>`) and `ncalrpc dir` (`np/<pipe>`) — with
/// its volatile lock and pid directories.
///
/// A unix socket path is capped at 108 bytes (`sun_path`), so these cannot
/// live under the lab's own `.vmlab/smb`: a lab in a deep directory left smbd
/// dying at start with `messaging_dgm_ref failed: File name too long`. Keyed
/// by a short hash of the lab's SMB state directory rather than by any name
/// the user chose, so the length is fixed by construction:
/// `<runtime>/smb/<12 hex>`.
pub fn smb_runtime_dir(smb_state_dir: &Path) -> PathBuf {
    smb_runtime_dir_in(&runtime_dir(), smb_state_dir)
}

/// [`smb_runtime_dir`] against an explicit runtime base, for tests.
pub fn smb_runtime_dir_in(runtime: &Path, smb_state_dir: &Path) -> PathBuf {
    runtime.join("smb").join(short_hash(smb_state_dir))
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

/// Ensure a directory exists.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create directory {}", dir.display()))?;
    Ok(())
}

/// Ensure a directory exists, owned by us and reachable only by us (0700).
///
/// Used for everything under [`runtime_dir`]: the control sockets there are a
/// full-privilege interface — a client that can connect runs scripts in the
/// lab, reads and writes guest files, and makes the daemon write to any path
/// the user can. With `XDG_RUNTIME_DIR` set the directory is already private,
/// but the WSL/Docker fallback is `/tmp/vmlab-<uid>`, where the default mode
/// would leave the sockets (and the per-VM VNC sockets beside them) reachable
/// by every local user. So: create private, tighten an existing directory, and
/// refuse one owned by somebody else — a pre-created `/tmp` squat.
///
/// Deliberately *not* applied to the data/state/template directories: those are
/// legitimately shared or bind-mounted in some deployments, and they hold no
/// control interface.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

        // Created private in one step, so there is no window where a fresh
        // `/tmp/vmlab-<uid>` is world-traversable.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("cannot create directory {}", dir.display()))?;

        let meta = std::fs::metadata(dir)
            .with_context(|| format!("cannot stat directory {}", dir.display()))?;
        let us = nix::unistd::Uid::effective().as_raw();
        if meta.uid() != us {
            bail!(
                "{} is owned by uid {}, not {us} — refusing to put control sockets there",
                dir.display(),
                meta.uid()
            );
        }
        if meta.permissions().mode() & 0o777 != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("cannot restrict {} to 0700", dir.display()))?;
        }
    }
    #[cfg(not(unix))]
    ensure_dir(dir)?;
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
/// `__labd` subcommands). For the CLI this is `self_exe()`; for any other
/// executable it's the `vmlab` binary next to the current one, falling back
/// to `vmlab` on `PATH`.
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

    /// The longest socket any machine binds, for the given names and base.
    fn longest_machine_socket(base: &Path, lab: &str, kind: MachineKind, m: &str) -> PathBuf {
        machine_runtime_dir_in(base, lab, kind, m).join("app-4294967295-conns.sock")
    }

    #[test]
    fn machine_sockets_fit_sun_path_for_the_longest_names() {
        let label = "a".repeat(63);
        for base in [
            Path::new("/run/user/1000/vmlab"),
            Path::new("/run/user/4294967294/vmlab"),
            Path::new("/tmp/thirty-seven-bytes-of-base/vmlab"),
        ] {
            for kind in [MachineKind::Vm, MachineKind::Container] {
                let sock = longest_machine_socket(base, &label, kind, &label);
                assert!(
                    sock.as_os_str().len() <= SUN_PATH_MAX,
                    "{} is {} bytes",
                    sock.display(),
                    sock.as_os_str().len()
                );
            }
            let control = lab_runtime_dir_in(base, &label).join("control.sock");
            assert!(control.as_os_str().len() <= SUN_PATH_MAX);
        }
    }

    #[test]
    fn short_names_keep_readable_runtime_paths() {
        let base = Path::new("/run/user/1000/vmlab");
        assert_eq!(
            machine_runtime_dir_in(base, "demo", MachineKind::Vm, "web01"),
            Path::new("/run/user/1000/vmlab/labs/demo/vms/web01")
        );
        assert_eq!(
            machine_runtime_dir_in(base, "demo", MachineKind::Container, "cache"),
            Path::new("/run/user/1000/vmlab/labs/demo/containers/cache")
        );
    }

    #[test]
    fn hashed_components_are_stable_distinct_and_never_dns_labels() {
        let base = Path::new("/run/user/1000/vmlab");
        let a = "a".repeat(63);
        let b = "b".repeat(63);
        let ra = lab_runtime_dir_in(base, &a);
        assert_eq!(ra, lab_runtime_dir_in(base, &a));
        assert_ne!(ra, lab_runtime_dir_in(base, &b));
        let last = ra.file_name().unwrap().to_str().unwrap();
        assert!(
            last.starts_with('_') && last.len() == HASHED_COMPONENT_LEN,
            "{last}"
        );
    }

    #[test]
    fn smb_runtime_dir_is_bounded_whatever_the_lab_path() {
        // A lab root of 200 characters still yields socket directories far
        // inside sun_path's 108 bytes: nothing of the lab path survives.
        let deep = PathBuf::from(format!("/{}/.vmlab/smb", "d".repeat(200)));
        let base = Path::new("/run/user/1000/vmlab");
        let run = smb_runtime_dir_in(base, &deep);
        // The longest socket smbd binds there: msg.sock/<pid>, pid_max 2^22.
        let msg = run.join("msg.sock").join("4194304");
        assert!(msg.as_os_str().len() < 64, "{}", msg.display());
        let ncalrpc = run.join("ncalrpc").join("np").join("lsa_ds");
        assert!(ncalrpc.as_os_str().len() < 64, "{}", ncalrpc.display());
        // The same length for every lab; distinct labs, distinct directories.
        let other = smb_runtime_dir_in(base, Path::new("/l/.vmlab/smb"));
        assert_eq!(run.as_os_str().len(), other.as_os_str().len());
        assert_ne!(run, other);
        assert_eq!(run, smb_runtime_dir_in(base, &deep));
    }

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
