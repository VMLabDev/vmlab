//! Tiny error plumbing (no anyhow in the guest — keep the binary lean), the
//! poweroff path every exit funnels through, and the in-root path walk every
//! image path is resolved through.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::Duration;

/// String-typed errors: PID 1 only ever prints them to the console and powers
/// off, so a rich error type buys nothing.
pub type Result<T> = std::result::Result<T, String>;

/// `.ctx("what")` — attach context the way anyhow's `.context()` would.
pub trait Ctx<T> {
    fn ctx(self, what: &str) -> Result<T>;
}

impl<T, E: std::fmt::Display> Ctx<T> for std::result::Result<T, E> {
    fn ctx(self, what: &str) -> Result<T> {
        self.map_err(|e| format!("{what}: {e}"))
    }
}

/// Flush, sync disks, give the console a beat to drain, and power off.
/// Never returns. Used for both the normal exit path and fatal errors (the
/// host treats a missing `exited` event as a crash).
pub fn power_off() -> ! {
    unsafe { libc::sync() };
    thread::sleep(Duration::from_millis(200));
    let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF);
    // reboot(2) only fails if we are somehow not privileged; spin so the
    // signature stays `!`.
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// Links followed in one resolution before it is a loop: Linux's own limit.
pub const MAX_LINKS: usize = 40;

/// `path`, as the container names it, on cinit's side of `root` (security
/// review F56).
///
/// The image is untrusted, and cinit touches it from outside the chroot: a
/// path that only had the rootfs put in front of it would let an absolute
/// symlink in the image (`/etc/resolv.conf -> /proc/sysrq-trigger`) or a
/// `..` name a file of the micro-VM itself. This walks the path one
/// component at a time, as `openat2(RESOLVE_IN_ROOT)` does: a symlink's
/// target is resolved against `root`, `..` stops at `root`, and more than
/// [`MAX_LINKS`] links is a loop. Every component is followed, the last
/// included; a component that does not exist yet is taken as named. The
/// result names no symlink the kernel could follow out of `root`.
///
/// Prior art: the agent's `unix/in_root.rs`.
pub fn in_root(root: &str, path: &str) -> Result<PathBuf> {
    let root = Path::new(root);
    let mut pending = components(Path::new(path));
    pending.reverse();
    let mut inside: Vec<OsString> = Vec::new();
    let mut links = 0;
    while let Some(component) = pending.pop() {
        if component == ".." {
            // The container's root is its own parent.
            inside.pop();
            continue;
        }
        let candidate = joined(root, &inside).join(&component);
        let Ok(target) = std::fs::read_link(&candidate) else {
            inside.push(component);
            continue;
        };
        links += 1;
        if links > MAX_LINKS {
            return Err(format!("{path}: too many levels of symbolic links"));
        }
        if target.is_absolute() {
            inside.clear();
        }
        // The target's components take this one's place, ahead of the rest.
        let mut expanded = components(&target);
        expanded.reverse();
        pending.extend(expanded);
    }
    Ok(joined(root, &inside))
}

/// The named components of `path`: no root, no `.`, `..` kept for the walk.
fn components(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

fn joined(root: &Path, inside: &[OsString]) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in inside {
        path.push(component);
    }
    path
}

/// Read the image file `path` (resolved by [`in_root`]). Only a regular file
/// is read: a link onto a device node or a FIFO is refused instead of
/// blocking PID 1 or reading a device.
pub fn read_in_root(root: &str, path: &str) -> Result<String> {
    let full = in_root(root, path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&full)
        .ctx(&format!("open {path}"))?;
    if !file.metadata().ctx(&format!("stat {path}"))?.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    let mut out = String::new();
    file.read_to_string(&mut out).ctx(&format!("read {path}"))?;
    Ok(out)
}

/// Write `contents` to the image file `path` (resolved by [`in_root`]),
/// creating it when absent. Only a regular file is written: a link onto a
/// device node, a FIFO or a directory is refused before anything is
/// truncated.
pub fn write_in_root(root: &str, path: &str, contents: &str) -> Result<()> {
    let full = in_root(root, path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&full)
        .ctx(&format!("open {path}"))?;
    if !file.metadata().ctx(&format!("stat {path}"))?.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    file.set_len(0).ctx(&format!("truncate {path}"))?;
    file.write_all(contents.as_bytes())
        .ctx(&format!("write {path}"))
}

/// Copy the file `source` of the micro-VM to the image file `path` (resolved
/// by [`in_root`]) with the permission bits `mode`, creating it when absent.
/// Only a regular file is written, as in [`write_in_root`]: a FIFO there
/// would block PID 1 in `open`, and a device node would take the bytes.
pub fn copy_in_root(root: &str, source: &str, path: &str, mode: u32) -> Result<()> {
    let mut from = std::fs::File::open(source).ctx(&format!("open {source}"))?;
    let full = in_root(root, path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&full)
        .ctx(&format!("open {path}"))?;
    if !file.metadata().ctx(&format!("stat {path}"))?.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    file.set_len(0).ctx(&format!("truncate {path}"))?;
    std::io::copy(&mut from, &mut file).ctx(&format!("write {path}"))?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .ctx(&format!("chmod {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// A micro-VM with a container root inside it: every name exists both
    /// `inner` inside the root and `OUTER` beside it.
    fn fixture() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let vm = dir.path();
        let root = vm.join("rootfs");
        for base in [vm, root.as_path()] {
            fs::create_dir_all(base.join("etc")).unwrap();
            fs::create_dir_all(base.join("run")).unwrap();
        }
        fs::write(vm.join("etc/sentinel"), "OUTER").unwrap();
        fs::write(root.join("etc/sentinel"), "inner").unwrap();
        let root = root.to_str().unwrap().to_string();
        (dir, root)
    }

    #[test]
    fn absolute_links_and_dotdot_stay_inside_the_root() {
        let (_dir, root) = fixture();
        let r = Path::new(&root);
        symlink("/etc/sentinel", r.join("abs")).unwrap();
        symlink("/etc", r.join("etcdir")).unwrap();
        symlink("../../../../../etc/sentinel", r.join("etc/climb")).unwrap();
        let read = |p: &str| fs::read_to_string(in_root(&root, p).unwrap()).unwrap();
        assert_eq!(read("/abs"), "inner", "absolute link");
        assert_eq!(read("/etcdir/sentinel"), "inner", "absolute dir link");
        assert_eq!(read("/etc/climb"), "inner", "relative link past /");
        assert_eq!(read("/../../etc/sentinel"), "inner", "`..` past the root");
        assert_eq!(read("etc/../../etc/./sentinel"), "inner", "relative path");
        assert_eq!(in_root(&root, "/").unwrap(), r);
        // A missing component is taken as named, inside the root.
        assert_eq!(
            in_root(&root, "/etcdir/new/file").unwrap(),
            r.join("etc/new/file")
        );
    }

    #[test]
    fn a_link_loop_is_refused() {
        let (_dir, root) = fixture();
        let r = Path::new(&root);
        symlink("/b", r.join("a")).unwrap();
        symlink("a", r.join("b")).unwrap();
        let err = in_root(&root, "/a/file").unwrap_err();
        assert!(err.contains("too many levels"), "{err}");
    }

    /// The review's case: `resolv.conf -> /run/...` is written inside the
    /// root, never beside it.
    #[test]
    fn write_through_an_absolute_link_lands_inside_the_root() {
        let (dir, root) = fixture();
        let r = Path::new(&root);
        symlink("/run/resolv.conf", r.join("etc/resolv.conf")).unwrap();
        write_in_root(&root, "/etc/resolv.conf", "nameserver 10.0.0.1\n").unwrap();
        assert_eq!(
            fs::read_to_string(r.join("run/resolv.conf")).unwrap(),
            "nameserver 10.0.0.1\n"
        );
        assert!(!dir.path().join("run/resolv.conf").exists());
        // Rewriting truncates.
        write_in_root(&root, "/etc/resolv.conf", "x\n").unwrap();
        assert_eq!(
            fs::read_to_string(r.join("run/resolv.conf")).unwrap(),
            "x\n"
        );
        // A link onto a directory is refused, not written through.
        symlink("/run", r.join("etc/hosts")).unwrap();
        assert!(write_in_root(&root, "/etc/hosts", "x").is_err());
        // A link whose parent does not exist in the root fails (the caller
        // warns) rather than escaping.
        symlink("/nowhere/x", r.join("etc/hostname")).unwrap();
        assert!(write_in_root(&root, "/etc/hostname", "x").is_err());
    }

    #[test]
    fn copy_writes_a_regular_file_with_its_mode_and_nothing_else() {
        let (dir, root) = fixture();
        let r = Path::new(&root);
        let source = dir.path().join("source");
        let source = source.to_str().unwrap();
        fs::write(source, "new bytes").unwrap();
        // An existing file is replaced whole, and takes the mode.
        fs::write(r.join("run/tool"), "older and longer bytes").unwrap();
        copy_in_root(&root, source, "/run/tool", 0o755).unwrap();
        assert_eq!(fs::read_to_string(r.join("run/tool")).unwrap(), "new bytes");
        let mode = fs::metadata(r.join("run/tool")).unwrap().permissions();
        assert_eq!(mode.mode() & 0o777, 0o755);
        // A directory and a link onto one are refused.
        assert!(copy_in_root(&root, source, "/etc", 0o755).is_err());
        symlink("/run", r.join("etc/tool")).unwrap();
        assert!(copy_in_root(&root, source, "/etc/tool", 0o755).is_err());
    }

    #[test]
    fn read_through_an_absolute_link_reads_inside_the_root() {
        let (_dir, root) = fixture();
        let r = Path::new(&root);
        symlink("/etc/sentinel", r.join("etc/passwd")).unwrap();
        assert_eq!(read_in_root(&root, "/etc/passwd").unwrap(), "inner");
        assert!(read_in_root(&root, "/etc").is_err(), "a directory");
        assert!(read_in_root(&root, "/missing").is_err());
    }
}
