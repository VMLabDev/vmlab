//! What only Linux answers: the virtio port found through `/sys`, metrics
//! from `/proc`, `os-release`, the init system that brings the guest down,
//! a clipboard through `wl-copy`/`xclip` when a display is reachable, and
//! the features only Linux serves — the workspace watch, process trees and
//! shared apps over waypipe and X11.

use std::fs;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use std::os::unix::fs::PermissionsExt;

use vmlab_agent_proto::{DiskUsage, OsInfo, PORT_NAME, ShutdownMode, features};

use super::login::Account;
use super::{CpuSample, Port};

/// What the handshake calls this kernel.
pub(super) const NAME: &str = "linux";

/// What a Linux agent serves beyond every Unix agent's set.
pub(super) const FEATURES: &[&str] = &[
    features::WATCH,
    features::TREE,
    // Whether `waypipe` is installed is the login's business, and the host
    // learns it from the open failing; X11 needs nothing.
    features::APP_WAYPIPE,
    features::APP_X11,
];

/// The `PATH` a login gets, from `login.defs`' own two answers: `ENV_SUPATH`
/// for uid 0 and `ENV_PATH` for everyone else. Taken from there rather than
/// invented because a login shell that cannot find `ip` — or that *can* find
/// it when a real login could not — is a difference a developer will hit and
/// not be able to explain.
///
/// `SUPATH` doubles as what the agent's own root sessions get, so root's
/// `PATH` is one string rather than two that have to be kept equal.
pub(super) const SUPATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
pub(super) const USER_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// root's home, where the agent's own terminal starts.
pub(super) const ROOT_HOME: &str = "/root";

/// The interactive shells tried, in order, for a terminal that names none.
pub(super) const SHELLS: &[&str] = &["/bin/bash", "/usr/bin/bash", "/bin/sh"];

/// The agent port: the virtio-serial port by name, or COM1 on a machine with
/// no virtio-serial controller but a UART (PRD §7.4).
pub(super) fn find_port() -> Option<Port> {
    let com1 = PathBuf::from("/dev/ttyS0");
    if !has_virtio_serial_controller() && com1.exists() {
        return Some(Port {
            note: Some(format!("no virtio-serial; serving on {}", com1.display())),
            path: com1,
            tty: true,
        });
    }
    find_virtio_port(PORT_NAME).map(|path| Port {
        path,
        tty: false,
        note: None,
    })
}

/// Resolve a virtio-serial port by its name property. Without udev there are
/// no /dev/virtio-ports/<name> symlinks, so scan /sys and fall back to the
/// symlink path for distros that do run udev.
fn find_virtio_port(name: &str) -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir("/sys/class/virtio-ports") {
        for entry in entries.flatten() {
            let port_name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
            if port_name.trim() == name {
                return Some(PathBuf::from("/dev").join(entry.file_name()));
            }
        }
    }
    let byname = PathBuf::from("/dev/virtio-ports").join(name);
    byname.exists().then_some(byname)
}

/// Whether this VM has a virtio-serial controller at all (PCI vendor 1af4,
/// device 1003 legacy or 1043 modern). Hardware, not timing: the profile
/// decided it before boot, so its absence is a fact, not a race — and the
/// signal that the agent channel is the 16550 on COM1 instead (PRD §7.4).
fn has_virtio_serial_controller() -> bool {
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return false;
    };
    entries.flatten().any(|e| {
        let vendor = fs::read_to_string(e.path().join("vendor")).unwrap_or_default();
        let device = fs::read_to_string(e.path().join("device")).unwrap_or_default();
        vendor.trim() == "0x1af4" && matches!(device.trim(), "0x1003" | "0x1043")
    })
}

/// Where shared-app sessions (PRD §20) put their guest end, as the program
/// sees it — inside a container these resolve under its root filesystem.
pub(super) struct AppPaths {
    /// The parent of every session's private directory.
    pub(super) run: String,
    /// The X socket directory every X client looks in.
    pub(super) x11_unix: String,
}

impl Default for AppPaths {
    fn default() -> AppPaths {
        AppPaths {
            run: "/run/vmlab-app".to_string(),
            x11_unix: "/tmp/.X11-unix".to_string(),
        }
    }
}

/// A session's private directory: under a parent anyone may pass through but
/// nobody else may list, replacing whatever an earlier agent left at the
/// name, mode 0700 and the session's own.
pub(super) fn make_app_dir(
    parent: &Path,
    dir: &Path,
    owner: Option<(u32, u32)>,
) -> std::io::Result<()> {
    if !parent.is_dir() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o711))?;
    }
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(dir, Some(uid), Some(gid))?;
    }
    Ok(())
}

/// Structured OS info from /etc/os-release + uname. Infallible: an initramfs
/// without os-release (container micro-VMs) still answers with the kernel's
/// view.
pub(super) fn os_info() -> OsInfo {
    let os_release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |key: &str| {
        os_release
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    };
    let uts = super::uname();
    OsInfo {
        id: field("ID").unwrap_or_else(|| "linux".into()),
        name: field("PRETTY_NAME")
            .or_else(|| field("NAME"))
            .unwrap_or_else(|| "Linux".into()),
        version: field("VERSION_ID").unwrap_or_default(),
        kernel: uts.0,
        arch: uts.1,
        hostname: uts.2,
    }
}

/// Bring the guest down: ask the init system so services stop cleanly, and
/// fall back to the raw reboot(2) syscall — which is the *only* path in a
/// container micro-VM's initramfs (no init system; the agent is cinit's
/// child and runs with CAP_SYS_BOOT).
pub(super) fn shutdown(mode: ShutdownMode) -> Result<(), String> {
    let argv: &[&str] = if Path::new("/run/systemd/system").exists() {
        match mode {
            ShutdownMode::Powerdown => &["systemctl", "poweroff"],
            ShutdownMode::Reboot => &["systemctl", "reboot"],
            ShutdownMode::Halt => &["systemctl", "halt"],
        }
    } else if Path::new("/sbin/openrc-shutdown").exists() {
        match mode {
            ShutdownMode::Powerdown => &["/sbin/openrc-shutdown", "-p", "now"],
            ShutdownMode::Reboot => &["/sbin/openrc-shutdown", "-r", "now"],
            ShutdownMode::Halt => &["/sbin/openrc-shutdown", "-H", "now"],
        }
    } else {
        &[]
    };
    if !argv.is_empty()
        && let Ok(status) = Command::new(argv[0]).args(&argv[1..]).status()
        && status.success()
    {
        return Ok(());
    }
    // SAFETY: sync then reboot(2); nothing to clean up — the guest is going
    // down.
    unsafe { libc::sync() };
    let cmd = match mode {
        ShutdownMode::Powerdown => libc::LINUX_REBOOT_CMD_POWER_OFF,
        ShutdownMode::Reboot => libc::LINUX_REBOOT_CMD_RESTART,
        ShutdownMode::Halt => libc::LINUX_REBOOT_CMD_HALT,
    };
    // SAFETY: see above.
    if unsafe { libc::reboot(cmd) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

// ---- metrics sampling ------------------------------------------------------

/// Cumulative (busy, total) jiffies from /proc/stat's aggregate cpu line.
pub fn cpu_sample() -> CpuSample {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let Some(line) = stat.lines().find(|l| l.starts_with("cpu ")) else {
        return (0, 0);
    };
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    let total: u64 = fields.iter().sum();
    let idle = fields.get(3).copied().unwrap_or(0) + fields.get(4).copied().unwrap_or(0);
    (total.saturating_sub(idle), total)
}

/// (used, total) bytes; "used" excludes reclaimable cache (MemAvailable).
pub fn mem_sample() -> (u64, u64) {
    let mut total = 0u64;
    let mut avail = 0u64;
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    for line in meminfo.lines() {
        let kb = |l: &str| {
            l.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                * 1024
        };
        if line.starts_with("MemTotal:") {
            total = kb(line);
        } else if line.starts_with("MemAvailable:") {
            avail = kb(line);
        }
    }
    (total.saturating_sub(avail), total)
}

pub fn disk_sample() -> Vec<DiskUsage> {
    let mut out = Vec::new();
    let mut seen_devices = Vec::new();
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    for line in mounts.lines() {
        let mut it = line.split_whitespace();
        let (Some(device), Some(mount)) = (it.next(), it.next()) else {
            continue;
        };
        // Real block-backed filesystems only; one entry per device (bind
        // mounts and btrfs subvolumes repeat the device).
        if !device.starts_with("/dev/") || seen_devices.contains(&device.to_string()) {
            continue;
        }
        let Ok(vfs) = nix::sys::statvfs::statvfs(mount) else {
            continue;
        };
        // c_ulong is 32 bits on the i686 build, so widen explicitly; on the
        // 64-bit builds the conversion is the identity clippy would remove.
        #[allow(clippy::useless_conversion)]
        let frsize: u64 = vfs.fragment_size().into();
        #[allow(clippy::useless_conversion)]
        let total = u64::from(vfs.blocks()) * frsize;
        if total == 0 {
            continue;
        }
        seen_devices.push(device.to_string());
        out.push(DiskUsage {
            mount: mount.to_string(),
            used: total - vfs.blocks_available() * frsize,
            total,
        });
    }
    out
}

// ---- clipboard (best-effort; headless guests never advertise it) ----------

/// A clipboard helper the agent's own environment can run.
pub(super) struct Clipboard {
    get: Vec<String>,
    set: Vec<String>,
}

impl Clipboard {
    /// A clipboard exists only when the agent's own environment can reach a
    /// display server (never true for the root service on server templates)
    /// and a helper tool is installed.
    pub(super) fn probe() -> Option<Clipboard> {
        let has = |bin: &str| {
            std::env::var_os("PATH")
                .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(bin).exists()))
        };
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && has("wl-copy") && has("wl-paste") {
            return Some(Clipboard {
                get: vec!["wl-paste".into(), "--no-newline".into()],
                set: vec!["wl-copy".into()],
            });
        }
        if std::env::var_os("DISPLAY").is_some() && has("xclip") {
            return Some(Clipboard {
                get: vec![
                    "xclip".into(),
                    "-selection".into(),
                    "clipboard".into(),
                    "-o".into(),
                ],
                set: vec!["xclip".into(), "-selection".into(), "clipboard".into()],
            });
        }
        None
    }

    pub(super) fn get(&self) -> Result<String, String> {
        let out = Command::new(&self.get[0])
            .args(&self.get[1..])
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("the clipboard tool failed: {e}"))?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub(super) fn set(&self, text: &str) -> Result<(), String> {
        self.run_set(text)
            .map_err(|e| format!("the clipboard tool failed: {e}"))
    }

    fn run_set(&self, text: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut child = Command::new(&self.set[0])
            .args(&self.set[1..])
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }
        drop(child.stdin.take());
        let status = child.wait()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "{} exited {status}",
                self.set[0]
            )));
        }
        Ok(())
    }
}

/// Set CLOCK_REALTIME. The system clock only — the hardware clock is left
/// to the guest's own sync, and nothing is shelled out to.
pub(super) fn set_clock(unix_ms: i64) -> Result<(), String> {
    let ts = libc::timespec {
        tv_sec: unix_ms.div_euclid(1000) as libc::time_t,
        tv_nsec: (unix_ms.rem_euclid(1000) * 1_000_000) as libc::c_long,
    };
    // SAFETY: a plain syscall reading one initialised timespec.
    match unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } {
        0 => Ok(()),
        _ => Err(format!(
            "clock_settime(CLOCK_REALTIME): {}",
            std::io::Error::last_os_error()
        )),
    }
}

// ---- what a login gets ------------------------------------------------------

/// Create the session's `XDG_RUNTIME_DIR` and hand back its guest-side path.
///
/// Rootless container tooling — the case §19.2 names — puts its state here
/// and fails in ways that read as a broken installation when it is absent.
/// logind owns this directory on a guest that has one and will mount its own
/// tmpfs over ours; doing it anyway is what covers the guests that have PAM
/// but no logind, and the `setuid` route, which has neither.
pub(super) fn runtime_dir(root: &Path, account: &Account) -> Option<String> {
    let inside = format!("/run/user/{}", account.uid);
    let path = root.join(inside.trim_start_matches('/'));
    fs::create_dir_all(&path).ok()?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
    let dir = fs::File::open(&path).ok()?;
    // Root-owned it would be useless to the session it exists for.
    nix::unistd::fchown(
        dir.as_fd(),
        Some(nix::unistd::Uid::from_raw(account.uid)),
        Some(nix::unistd::Gid::from_raw(account.gid)),
    )
    .ok()?;
    Some(inside)
}

/// This thread's filesystem identity, worn until the value is dropped.
///
/// `setfsuid`/`setfsgid` are Linux's per-thread filesystem identity.
/// Supplementary groups stay root's: they are not per-thread, so a file
/// reachable only through one is the one read this cannot mirror.
///
/// Raw syscalls because musl does not expose `setfsuid`/`setfsgid` at all,
/// and the agent is built against it.
pub(super) struct ThreadIds {
    previous_uid: u32,
    previous_gid: u32,
}

impl ThreadIds {
    /// Read as `uid`/`gid` on this thread. Neither call reports failure —
    /// both answer with the *previous* value — so this cannot fail either.
    pub(super) fn assume(uid: u32, gid: u32) -> std::io::Result<ThreadIds> {
        let (previous_uid, previous_gid) = set_fs_ids(uid, gid);
        Ok(ThreadIds {
            previous_uid,
            previous_gid,
        })
    }
}

impl Drop for ThreadIds {
    fn drop(&mut self) {
        set_fs_ids(self.previous_uid, self.previous_gid);
    }
}

/// Set this thread's filesystem uid/gid, returning the previous pair.
fn set_fs_ids(uid: u32, gid: u32) -> (u32, u32) {
    // SAFETY: two argument-free-by-value Linux syscalls that only ever affect
    // the calling thread.
    unsafe {
        let previous_gid = libc::syscall(libc::SYS_setfsgid, gid) as u32;
        let previous_uid = libc::syscall(libc::SYS_setfsuid, uid) as u32;
        (previous_uid, previous_gid)
    }
}
