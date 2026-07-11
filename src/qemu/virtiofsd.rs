//! virtiofsd process management: locate the daemon and spawn one instance
//! per shared directory (PRD §18 volumes; §7.5 shares in a later phase).
//!
//! virtiofsd is a vhost-user back-end — QEMU connects to its socket and the
//! guest mounts the export natively (`mount -t virtiofs <tag>`). Snapshots
//! keep working because the daemon is always run with `--migration-mode`:
//! its FUSE session state travels through QEMU's migration stream, which is
//! exactly what `snapshot-save` captures (validated against QEMU 11 /
//! virtiofsd 1.13 — save with dirty state, online load, and a
//! restore-much-later into a fresh QEMU + virtiofsd all round-trip).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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

/// Is a virtiofsd available on this host?
pub fn available() -> bool {
    binary().is_some()
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
/// Migration flags: `find-paths` re-opens inodes by path on restore (the
/// export root is the same directory on the same host — snapshots, not real
/// migration), `--migration-verify-handles` guards against files swapped
/// underneath between save and restore, and `guest-error` surfaces any
/// non-transferable inode as an EIO to the guest instead of failing the
/// whole snapshot job.
pub async fn spawn(
    name: &str,
    socket: &Path,
    shared_dir: &Path,
    readonly: bool,
    log_path: &Path,
) -> Result<Arc<Proc>> {
    let Some(bin) = binary() else {
        bail!("no virtiofsd binary found (set VMLAB_VIRTIOFSD or install one)");
    };
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
        "--migration-mode".to_string(),
        "find-paths".to_string(),
        "--migration-verify-handles".to_string(),
        "--migration-on-error".to_string(),
        "guest-error".to_string(),
        "--log-level".to_string(),
        "warn".to_string(),
    ];
    if readonly {
        args.push("--readonly".to_string());
    }
    let proc = Proc::spawn(
        &format!("virtiofsd:{name}"),
        &bin.display().to_string(),
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
        if !available() {
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
        if !available() {
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
