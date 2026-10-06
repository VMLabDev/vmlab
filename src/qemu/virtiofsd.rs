//! virtiofsd process management: locate the daemon and spawn one instance
//! per shared directory (PRD §18 volumes; §7.5 shares in a later phase).
//!
//! virtiofsd is a vhost-user back-end — QEMU connects to its socket and the
//! guest mounts the export natively (`mount -t virtiofs <tag>`). Online
//! snapshots keep working because the daemon is run with `--migration-mode`:
//! its FUSE session state travels through QEMU's migration stream, which is
//! exactly what `snapshot-save` captures (validated against QEMU 11 /
//! virtiofsd 1.13 — save with dirty state, online load, and a
//! restore-much-later into a fresh QEMU + virtiofsd all round-trip).
//!
//! Distros ship virtiofsd releases older than those flags (Ubuntu 24.04 has
//! 1.10.0), so what a binary accepts is probed from its `--help` once per
//! binary ([`probe`]) rather than assumed. `--print-capabilities` is no help
//! here: it reports the vhost-user device type only.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use super::process::Proc;

/// Where distros put virtiofsd when it is not on PATH (it ships as an
/// internal helper: Arch/CachyOS `/usr/lib`, Fedora/RHEL `/usr/libexec`,
/// Debian/Ubuntu `/usr/lib/qemu`).
const KNOWN_LOCATIONS: &[&str] = &[
    "/usr/lib/virtiofsd",
    "/usr/libexec/virtiofsd",
    "/usr/lib/qemu/virtiofsd",
];

/// Locate the virtiofsd binary: `$VMLAB_VIRTIOFSD` override, then PATH,
/// then the known install locations. `None` means volumes fall back to CIFS.
pub fn binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VMLAB_VIRTIOFSD").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|d| d.join("virtiofsd"))
            .find(|c| c.is_file())
    {
        return Some(found);
    }
    KNOWN_LOCATIONS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// The first virtiofsd release with `--migration-mode` and friends — the
/// least an online snapshot of a machine with virtiofs devices needs.
pub const MIGRATION_SINCE: &str = "1.11.0";

/// The first virtiofsd release with `--readonly`.
pub const READONLY_SINCE: &str = "1.13.0";

/// The flags every spawn passes; a binary missing one cannot serve a share at
/// all (QEMU's retired C virtiofsd, for one, has no `--shared-dir`).
const REQUIRED_FLAGS: &[&str] = &["--socket-path", "--shared-dir", "--cache", "--log-level"];

/// The migration flags [`spawn`] passes when the binary has them.
const MIGRATION_FLAGS: &[&str] = &[
    "--migration-mode",
    "--migration-verify-handles",
    "--migration-on-error",
];

/// A virtiofsd this host can run, and what it accepts beyond the basics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Virtiofsd {
    pub path: PathBuf,
    /// As `--version` reports it (`1.10.0`); `None` if it said nothing usable.
    pub version: Option<String>,
    /// Has `--migration-mode`: its session state can ride a snapshot.
    pub migration: bool,
    /// Has `--readonly`.
    pub readonly: bool,
}

impl Virtiofsd {
    /// `virtiofsd 1.10.0 (/usr/libexec/virtiofsd)`, for messages.
    pub fn describe(&self) -> String {
        describe(&self.path, self.version.as_deref())
    }

    /// Why an online snapshot of `machine`, whose virtiofs devices this
    /// daemon serves, cannot be taken or loaded — `None` when it can.
    pub fn snapshot_refusal(&self, machine: &str) -> Option<String> {
        (!self.migration).then(|| {
            format!(
                "{machine}: online snapshots of a machine with virtiofs devices need virtiofsd \
                 {MIGRATION_SINCE} or later to carry their state, and this host has {} — use an \
                 offline snapshot (the machine stopped), or install a newer virtiofsd (or point \
                 VMLAB_VIRTIOFSD at one) and restart the machine",
                self.describe()
            )
        })
    }

    /// Why a read-only share cannot ride this daemon — `None` when it can.
    pub fn readonly_refusal(&self, what: &str) -> Option<String> {
        (!self.readonly).then(|| {
            format!(
                "{what} is read-only, and {} has no --readonly (added in virtiofsd \
                 {READONLY_SINCE}) — install a newer virtiofsd (or point VMLAB_VIRTIOFSD at \
                 one), or set transport = \"smb\"",
                self.describe()
            )
        })
    }
}

/// What looking for a virtiofsd found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// No binary at all.
    Missing,
    /// A binary vmlab cannot drive; `why` says what it lacks.
    Unusable {
        path: PathBuf,
        version: Option<String>,
        why: String,
    },
    Found(Virtiofsd),
}

impl Probe {
    /// The usable daemon, if there is one.
    pub fn found(self) -> Option<Virtiofsd> {
        match self {
            Probe::Found(v) => Some(v),
            _ => None,
        }
    }

    /// Why no virtiofsd can serve a share, for an error; `None` when one can.
    pub fn unusable_reason(&self) -> Option<String> {
        match self {
            Probe::Found(_) => None,
            Probe::Missing => Some(format!(
                "no virtiofsd was found on this host (install virtiofsd {READONLY_SINCE} or \
                 later, or set VMLAB_VIRTIOFSD)"
            )),
            Probe::Unusable { path, version, why } => Some(format!(
                "{} cannot serve a share: {why} (install virtiofsd {READONLY_SINCE} or later, \
                 or point VMLAB_VIRTIOFSD at one)",
                describe(path, version.as_deref())
            )),
        }
    }
}

fn describe(path: &Path, version: Option<&str>) -> String {
    match version {
        Some(v) => format!("virtiofsd {v} ({})", path.display()),
        None => format!("virtiofsd of unknown version ({})", path.display()),
    }
}

/// The host's usable virtiofsd, if any.
pub fn found() -> Option<Virtiofsd> {
    probe().found()
}

/// Locate the virtiofsd and work out what it accepts.
///
/// The answer is cached per binary — path, size and mtime — so the lab
/// daemon asks the binary once rather than at every share placement, and
/// still notices a virtiofsd upgraded underneath it.
pub fn probe() -> Probe {
    type Key = (PathBuf, u64, Option<SystemTime>);
    static CACHE: Mutex<Option<HashMap<Key, Probe>>> = Mutex::new(None);

    let Some(path) = binary() else {
        return Probe::Missing;
    };
    let meta = std::fs::metadata(&path).ok();
    let key = (
        path.clone(),
        meta.as_ref().map_or(0, |m| m.len()),
        meta.and_then(|m| m.modified().ok()),
    );
    if let Some(hit) = CACHE
        .lock()
        .expect("virtiofsd probe cache")
        .get_or_insert_with(HashMap::new)
        .get(&key)
    {
        return hit.clone();
    }
    let probe = run_probe(&path);
    CACHE
        .lock()
        .expect("virtiofsd probe cache")
        .get_or_insert_with(HashMap::new)
        .insert(key, probe.clone());
    probe
}

/// Ask the binary itself: `--version`, then `--help`.
fn run_probe(path: &Path) -> Probe {
    let run = |arg: &str| {
        std::process::Command::new(path)
            .arg(arg)
            .stdin(std::process::Stdio::null())
            .output()
    };
    let version = run("--version")
        .ok()
        .and_then(|o| parse_version(&String::from_utf8_lossy(&o.stdout)));
    match run("--help") {
        Ok(out) => {
            // clap prints help to stdout; older daemons used stderr.
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            classify(path, version, &text)
        }
        Err(e) => Probe::Unusable {
            path: path.to_path_buf(),
            version,
            why: format!("running it with --help failed: {e}"),
        },
    }
}

/// `virtiofsd 1.10.0` → `1.10.0`.
fn parse_version(out: &str) -> Option<String> {
    let line = out.lines().find(|l| !l.trim().is_empty())?;
    let v = line.split_whitespace().last()?;
    v.starts_with(|c: char| c.is_ascii_digit())
        .then(|| v.to_string())
}

/// Every long option a `--help` text lists: the `--name` tokens that open an
/// option line (`  -h, --help`, `      --cache <CACHE>`,
/// `      --inode-file-handles=<…>`).
fn help_flags(help: &str) -> BTreeSet<String> {
    let mut flags = BTreeSet::new();
    for line in help.lines() {
        let line = line.trim_start();
        if !line.starts_with('-') {
            continue;
        }
        for token in line.split([' ', ',']) {
            if let Some(name) = token.strip_prefix("--") {
                let name: String = name
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                if !name.is_empty() {
                    flags.insert(format!("--{name}"));
                }
            } else if !token.is_empty() && !token.starts_with('-') {
                break;
            }
        }
    }
    flags
}

/// What a binary's `--help` says it can do.
fn classify(path: &Path, version: Option<String>, help: &str) -> Probe {
    let flags = help_flags(help);
    let lacking: Vec<&str> = REQUIRED_FLAGS
        .iter()
        .copied()
        .filter(|f| !flags.contains(*f))
        .collect();
    if !lacking.is_empty() {
        return Probe::Unusable {
            path: path.to_path_buf(),
            version,
            why: format!("its --help lists no {}", lacking.join(", ")),
        };
    }
    Probe::Found(Virtiofsd {
        path: path.to_path_buf(),
        version,
        migration: MIGRATION_FLAGS.iter().all(|f| flags.contains(*f)),
        readonly: flags.contains("--readonly"),
    })
}

/// virtio-fs mount tags are limited to 36 bytes on the device. Share names
/// almost always fit; a longer one keeps a recognisable prefix plus an
/// FNV-1a hash suffix so distinct names never collapse to the same tag.
pub fn mount_tag(name: &str) -> String {
    const MAX: usize = 36;
    if name.len() <= MAX {
        return name.to_string();
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    let mut end = MAX - 9; // room for '-' + 8 hex digits
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}-{:08x}", &name[..end], h as u32)
}

/// Spawn one virtiofsd exporting `shared_dir` on `socket` and wait for the
/// socket to appear (QEMU's vhost-user chardev connects at startup, so the
/// daemon must be listening before the VM spawns). The stale socket from a
/// previous run is removed first — virtiofsd refuses to reuse it.
///
/// Migration flags, passed when the binary has them ([`Probe`]): `find-paths`
/// re-opens inodes by path on restore (the export root is the same directory
/// on the same host — snapshots, not real migration),
/// `--migration-verify-handles` guards against files swapped underneath
/// between save and restore, and `guest-error` surfaces any non-transferable
/// inode as an EIO to the guest instead of failing the whole snapshot job.
/// Without them the daemon still serves the share; online snapshots of the
/// machine are refused instead ([`Virtiofsd::snapshot_refusal`]).
///
/// A read-only export on a daemon with no `--readonly` is an error rather
/// than a writable export.
pub async fn spawn(
    name: &str,
    socket: &Path,
    shared_dir: &Path,
    readonly: bool,
    log_path: &Path,
) -> Result<Arc<Proc>> {
    let vfsd = match probe() {
        Probe::Found(v) => v,
        other => bail!("{}", other.unusable_reason().unwrap_or_default()),
    };
    if readonly && let Some(why) = vfsd.readonly_refusal(&format!("{name}'s share")) {
        bail!("{why}");
    }
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let mut args = vec![
        "--socket-path".to_string(),
        socket.display().to_string(),
        "--shared-dir".to_string(),
        shared_dir.display().to_string(),
        "--cache".to_string(),
        "auto".to_string(),
        "--log-level".to_string(),
        "warn".to_string(),
    ];
    if vfsd.migration {
        args.extend(
            [
                "--migration-mode",
                "find-paths",
                "--migration-verify-handles",
                "--migration-on-error",
                "guest-error",
            ]
            .map(String::from),
        );
    }
    if readonly {
        args.push("--readonly".to_string());
    }
    let proc = Proc::spawn(
        &format!("virtiofsd:{name}"),
        &vfsd.path.display().to_string(),
        &args,
        log_path,
    )
    .await?;
    // The socket appears as soon as the daemon is up (well under a second);
    // a missing shared dir or bad flag shows up as an early exit instead.
    for _ in 0..50 {
        if socket.exists() {
            return Ok(proc);
        }
        if !proc.is_running() {
            bail!(
                "virtiofsd for {name} exited at startup ({}) — see {}",
                proc.exit_status().unwrap_or_default(),
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    proc.kill().await;
    bail!(
        "virtiofsd for {name} never created {} — see {}",
        socket.display(),
        log_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // `binary()` reads the environment, which tests must not mutate
    // (edition 2024); the lookup-order logic is simple enough that only the
    // spawn contract is exercised, and only when a virtiofsd exists.

    /// Option lines from Ubuntu 24.04's virtiofsd 1.10.0 `--help`, with a
    /// description line of the kind that must not read as an option.
    const HELP_1_10: &str = "Launch a virtiofsd backend.

Usage: virtiofsd [OPTIONS]

Options:
      --shared-dir <SHARED_DIR>
          Shared directory path
      --socket-path <SOCKET_PATH>
          vhost-user socket path
      --inode-file-handles=<INODE_FILE_HANDLES>
          - never: Never use file handles, always use O_PATH file descriptors.
      --cache <CACHE>
          The caching policy the file system should use (auto, always, never, metadata)
      --print-capabilities
      --log-level <LOG_LEVEL>
  -o <COMPAT_OPTIONS>
  -h, --help
  -V, --version
";

    /// The lines virtiofsd 1.14.0 adds over [`HELP_1_10`] that vmlab uses.
    const HELP_1_14_EXTRA: &str = "      --readonly
      --migration-mode <MIGRATION_MODE>
          Defines how to perform migration, i.e. how to represent the internal state to the destination
      --migration-on-error <MIGRATION_ON_ERROR>
      --migration-verify-handles
      --migration-confirm-paths
";

    /// QEMU's retired C virtiofsd: no `--shared-dir`, `-o source=` instead.
    const HELP_C: &str = "usage: virtiofsd [options]

    -h   --help                print help
    -V   --version             print version
    --print-capabilities       Print vhost-user.json
    --socket-path=PATH         path for the vhost-user socket
    -o source=PATH             shared directory tree
";

    #[test]
    fn help_flags_reads_option_lines_only() {
        let flags = help_flags(HELP_1_10);
        for f in [
            "--shared-dir",
            "--socket-path",
            "--inode-file-handles",
            "--cache",
            "--log-level",
            "--help",
            "--version",
        ] {
            assert!(flags.contains(f), "{f} in {flags:?}");
        }
        assert!(!flags.iter().any(|f| f.contains("never")), "{flags:?}");
        assert!(help_flags(HELP_C).contains("--socket-path"));
    }

    #[test]
    fn a_virtiofsd_before_1_11_serves_shares_but_not_snapshots() {
        let path = Path::new("/usr/libexec/virtiofsd");
        let Probe::Found(v) = classify(path, Some("1.10.0".into()), HELP_1_10) else {
            panic!("1.10.0 can serve a share");
        };
        assert!(!v.migration && !v.readonly, "{v:?}");
        let why = v.snapshot_refusal("web").expect("refused");
        assert!(why.contains("virtiofsd 1.11.0 or later"), "{why}");
        assert!(
            why.contains("virtiofsd 1.10.0 (/usr/libexec/virtiofsd)"),
            "{why}"
        );
        let why = v.readonly_refusal("share \"ro\"").expect("refused");
        assert!(why.contains("virtiofsd 1.13.0"), "{why}");
    }

    #[test]
    fn a_current_virtiofsd_has_everything_vmlab_asks_for() {
        let help = format!("{HELP_1_10}{HELP_1_14_EXTRA}");
        let Probe::Found(v) = classify(Path::new("/usr/lib/virtiofsd"), None, &help) else {
            panic!("1.14.0 can serve a share");
        };
        assert!(v.migration && v.readonly, "{v:?}");
        assert_eq!(v.snapshot_refusal("web"), None);
        assert_eq!(v.readonly_refusal("share"), None);
    }

    #[test]
    fn the_c_virtiofsd_cannot_serve_a_share() {
        let probe = classify(Path::new("/usr/libexec/virtiofsd"), None, HELP_C);
        assert!(matches!(probe, Probe::Unusable { .. }), "{probe:?}");
        let why = probe.unusable_reason().unwrap();
        assert!(why.contains("--shared-dir"), "{why}");
        assert!(why.contains("virtiofsd 1.13.0 or later"), "{why}");
        assert!(Probe::Missing.unusable_reason().is_some());
    }

    #[test]
    fn version_is_the_last_word_of_the_first_line() {
        assert_eq!(
            parse_version("virtiofsd 1.10.0\n").as_deref(),
            Some("1.10.0")
        );
        assert_eq!(parse_version("\nvirtiofsd 1.14.0"), Some("1.14.0".into()));
        assert_eq!(parse_version("error: unknown flag"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn mount_tags_fit_the_device_limit() {
        assert_eq!(mount_tag("mnt_src"), "mnt_src");
        assert_eq!(mount_tag(&"a".repeat(36)), "a".repeat(36));
        let long = "very_long_share_name_that_exceeds_the_virtio_limit";
        let tag = mount_tag(long);
        assert!(tag.len() <= 36, "{tag}");
        assert!(tag.starts_with("very_long_share_name_that_e"), "{tag}");
        // Distinct long names get distinct tags.
        assert_ne!(tag, mount_tag(&format!("{long}_2")));
        // Multibyte input truncates on a char boundary without panicking.
        let multi = mount_tag(&"ü".repeat(40));
        assert!(multi.len() <= 36, "{multi}");
    }

    #[tokio::test]
    async fn spawn_creates_socket_and_kill_reaps() {
        if found().is_none() {
            eprintln!("virtiofsd not installed — skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        let sock = tmp.path().join("vfs.sock");
        let proc = spawn(
            "t",
            &sock,
            &shared,
            false,
            &tmp.path().join("virtiofsd.log"),
        )
        .await
        .unwrap();
        assert!(sock.exists());
        assert!(proc.is_running());
        proc.kill().await;
        proc.wait_exit(Duration::from_secs(5)).await.unwrap();
    }

    #[tokio::test]
    async fn spawn_fails_fast_on_missing_shared_dir() {
        if found().is_none() {
            eprintln!("virtiofsd not installed — skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let Err(err) = spawn(
            "t",
            &tmp.path().join("vfs.sock"),
            &tmp.path().join("does-not-exist"),
            false,
            &tmp.path().join("virtiofsd.log"),
        )
        .await
        else {
            panic!("spawn with a missing shared dir must fail");
        };
        assert!(err.to_string().contains("virtiofsd for t"), "{err}");
    }
}
