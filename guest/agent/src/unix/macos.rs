//! What only macOS answers: the virtio console the guest names after its
//! port, metrics from the Mach host and `getmntinfo`, `SystemVersion.plist`,
//! `shutdown(8)`, `settimeofday`, the console user's pasteboard, accounts
//! from the directory service, and the per-thread identity Darwin offers in
//! place of `setfsuid`.
//!
//! The policy behind each answer is in [`crate::darwin`], where the build
//! host's tests reach it; this module only asks the kernel.
//!
//! **What a macOS agent does not serve, and so never advertises** (PRD
//! §7.4): the workspace `watch` (no recursive backend — FSEvents would bring
//! the CoreServices framework into a binary that today links nothing but
//! libSystem), process `tree`s (no subreaper: an orphan goes to launchd,
//! where nothing can count it), and shared apps over waypipe or X11 (neither
//! is how a Mac draws a window).

use std::ffi::{CStr, CString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use vmlab_agent_proto::{DiskUsage, OsInfo, PORT_NAME, ShutdownMode};

use super::login::{Account, AccountDb, PasswdEntry};
use super::{CpuSample, LANG, Port};
use crate::darwin;

/// What the handshake calls this kernel.
pub(super) const NAME: &str = "macos";

/// Nothing beyond every Unix agent's set (see the module docs for why).
pub(super) const FEATURES: &[&str] = &[];

/// The `PATH` `path_helper` builds from `/etc/paths`, which is what a login
/// shell on macOS starts with, root's and everyone else's alike.
pub(super) const SUPATH: &str =
    "/usr/local/bin:/System/Cryptexes/App/usr/bin:/usr/bin:/bin:/usr/sbin:/sbin";
pub(super) const USER_PATH: &str = SUPATH;

/// root's home on macOS.
pub(super) const ROOT_HOME: &str = "/var/root";

/// zsh has been every account's shell since 10.15; bash 3.2 and sh remain.
pub(super) const SHELLS: &[&str] = &["/bin/zsh", "/bin/bash", "/bin/sh"];

/// The agent port: macOS's AppleVirtIO console names each virtio-serial
/// port's node after the port, and the `cu.` (callout) node is the one that
/// opens without waiting for carrier. It is a tty like any other, so its
/// line discipline comes off before the first frame.
pub(super) fn find_port() -> Option<Port> {
    let path = PathBuf::from(format!("/dev/cu.{PORT_NAME}"));
    path.exists().then(|| Port {
        note: Some(format!("serving on {}", path.display())),
        path,
        tty: true,
    })
}

/// Where macOS states its own name and version: present on every release
/// since 10.0, on the sealed system volume, readable by anyone.
const SYSTEM_VERSION_PLIST: &str = "/System/Library/CoreServices/SystemVersion.plist";

pub(super) fn os_info() -> OsInfo {
    let version = std::fs::read_to_string(SYSTEM_VERSION_PLIST)
        .ok()
        .and_then(|plist| darwin::parse_system_version(&plist));
    darwin::os_info(version, super::uname())
}

/// Bring the guest down through `shutdown(8)`, so launchd stops every
/// service in order; `reboot(2)` only where that could not even start.
pub(super) fn shutdown(mode: ShutdownMode) -> Result<(), String> {
    let argv = darwin::shutdown_argv(mode);
    if let Ok(status) = Command::new(argv[0]).args(&argv[1..]).status()
        && status.success()
    {
        return Ok(());
    }
    // <sys/reboot.h>: RB_AUTOBOOT restarts, RB_HALT stops (and on a Mac
    // powers off).
    const RB_AUTOBOOT: libc::c_int = 0;
    const RB_HALT: libc::c_int = 0x08;
    let howto = match mode {
        ShutdownMode::Reboot => RB_AUTOBOOT,
        ShutdownMode::Powerdown | ShutdownMode::Halt => RB_HALT,
    };
    // SAFETY: sync then reboot(2); nothing to clean up — the guest is going
    // down.
    unsafe { libc::sync() };
    // SAFETY: as above.
    match unsafe { ffi::reboot(howto) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error().to_string()),
    }
}

/// Set the system clock. `settimeofday` is macOS's own way to step it; the
/// RTC is left to the guest, as on Linux.
pub(super) fn set_clock(unix_ms: i64) -> Result<(), String> {
    let tv = libc::timeval {
        tv_sec: unix_ms.div_euclid(1000) as libc::time_t,
        tv_usec: (unix_ms.rem_euclid(1000) * 1000) as libc::suseconds_t,
    };
    // SAFETY: a plain syscall reading one initialised timeval; no timezone.
    match unsafe { libc::settimeofday(&tv, std::ptr::null()) } {
        0 => Ok(()),
        _ => Err(format!("settimeofday: {}", std::io::Error::last_os_error())),
    }
}

// ---- metrics sampling ------------------------------------------------------

/// Cumulative (busy, total) ticks across every CPU, from the Mach host.
pub fn cpu_sample() -> CpuSample {
    let mut ticks: darwin::CpuTicks = [0; 4];
    let mut count = ffi::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: `ticks` is a host_cpu_load_info (natural_t[CPU_STATE_MAX]) and
    // `count` says so in integer_t units.
    let kr = unsafe {
        ffi::host_statistics(
            ffi::host(),
            ffi::HOST_CPU_LOAD_INFO,
            ticks.as_mut_ptr().cast(),
            &mut count,
        )
    };
    match kr {
        ffi::KERN_SUCCESS => darwin::cpu_sample(ticks),
        _ => (0, 0),
    }
}

/// (used, total) bytes; "used" as [`darwin::memory_used`] counts it.
pub fn mem_sample() -> (u64, u64) {
    let total = sysctl_u64(c"hw.memsize").unwrap_or(0);
    let mut vm = ffi::VmStatistics64::default();
    let mut count = ffi::HOST_VM_INFO64_COUNT;
    // SAFETY: `vm` is the vm_statistics64 layout and `count` its size in
    // integer_t units; the kernel fills at most that much.
    let kr = unsafe {
        ffi::host_statistics64(
            ffi::host(),
            ffi::HOST_VM_INFO64,
            (&mut vm as *mut ffi::VmStatistics64).cast(),
            &mut count,
        )
    };
    if kr != ffi::KERN_SUCCESS {
        return (0, total);
    }
    // SAFETY: sysconf is always safe to call.
    let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
    let pages = darwin::PageCounts {
        wired: vm.wire_count.into(),
        internal: vm.internal_page_count.into(),
        purgeable: vm.purgeable_count.into(),
        compressed: vm.compressor_page_count.into(),
    };
    (darwin::memory_used(pages, page_size).min(total), total)
}

pub fn disk_sample() -> Vec<DiskUsage> {
    let mut mounts: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo points `mounts` at a buffer libc owns and reuses;
    // it is read here and nowhere else, before the next call.
    let n = unsafe { libc::getmntinfo(&mut mounts, libc::MNT_NOWAIT) };
    if n <= 0 || mounts.is_null() {
        return Vec::new();
    }
    // SAFETY: getmntinfo returned `n` initialised entries at `mounts`.
    let mounts = unsafe { std::slice::from_raw_parts(mounts, n as usize) };
    let text = |field: &[libc::c_char]| {
        // SAFETY: the kernel NUL-terminates both name fields.
        unsafe { CStr::from_ptr(field.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    let mut out = Vec::new();
    for m in mounts {
        let from = text(&m.f_mntfromname);
        let on = text(&m.f_mntonname);
        let flags = m.f_flags as libc::c_int;
        let local = flags & libc::MNT_LOCAL != 0;
        let nobrowse = flags & libc::MNT_DONTBROWSE != 0;
        let total = m.f_blocks * u64::from(m.f_bsize);
        if !darwin::reported_mount(&from, &on, local, nobrowse) || total == 0 {
            continue;
        }
        out.push(DiskUsage {
            mount: on,
            used: total.saturating_sub(m.f_bavail * u64::from(m.f_bsize)),
            total,
        });
    }
    out
}

fn sysctl_u64(name: &CStr) -> Option<u64> {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: a u64-sized out buffer whose size `len` states.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<u64>()).then_some(value)
}

// ---- clipboard -------------------------------------------------------------

/// The console user's pasteboard, reached through `pbcopy`/`pbpaste` inside
/// that user's session. The agent is a root daemon outside every login, so
/// this is the one way to the pasteboard a person at the screen is using —
/// and while nobody is logged in there is none, which the answer says.
pub(super) struct Clipboard;

impl Clipboard {
    /// Every macOS install has both tools; whether anyone is at the console
    /// is asked per request, since a person logs in and out under a running
    /// agent.
    pub(super) fn probe() -> Option<Clipboard> {
        (Path::new("/usr/bin/pbcopy").exists() && Path::new("/usr/bin/pbpaste").exists())
            .then_some(Clipboard)
    }

    pub(super) fn get(&self) -> Result<String, String> {
        let out = pasteboard(darwin::pasteboard_argv(console_user()?, "/usr/bin/pbpaste"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("pbpaste: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "pbpaste: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub(super) fn set(&self, text: &str) -> Result<(), String> {
        use std::io::Write;
        let mut child = pasteboard(darwin::pasteboard_argv(console_user()?, "/usr/bin/pbcopy"))
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("pbcopy: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| format!("pbcopy: {e}"))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("pbcopy: {e}"))?;
        match out.status.success() {
            true => Ok(()),
            false => Err(format!(
                "pbcopy: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        }
    }
}

/// `argv` with the one setting the tools read their text encoding from:
/// without a UTF-8 locale they read and write MacRoman.
fn pasteboard(argv: Vec<String>) -> Command {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).env("LANG", LANG);
    cmd
}

/// Why there is no pasteboard to reach: the console belongs to root, which
/// is loginwindow's, until somebody logs in.
const NOBODY_AT_THE_CONSOLE: &str =
    "nobody is logged in at the console, so there is no pasteboard to reach";

/// Whoever owns the console device: the logged-in user, or root while
/// loginwindow has the screen.
fn console_user() -> Result<u32, String> {
    let uid = std::fs::metadata("/dev/console")
        .map_err(|e| format!("/dev/console: {e}"))?
        .uid();
    match uid {
        0 => Err(NOBODY_AT_THE_CONSOLE.to_string()),
        uid => Ok(uid),
    }
}

// ---- what a login gets ------------------------------------------------------

/// macOS has no `/run/user`: the system volume is sealed read-only, and a
/// per-user process gets `$TMPDIR` from launchd instead.
pub(super) fn runtime_dir(_root: &Path, _account: &Account) -> Option<String> {
    None
}

/// The accounts macOS actually has: the directory service, through the libc
/// calls every macOS tool resolves a user with. `/etc/passwd` holds only a
/// few system accounts and none a person logs in as.
pub(super) struct DirectoryService;

impl AccountDb for DirectoryService {
    fn by_name(&self, name: &str) -> Option<PasswdEntry> {
        nix::unistd::User::from_name(name).ok()?.map(entry)
    }

    fn by_uid(&self, uid: u32) -> Option<PasswdEntry> {
        nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .ok()?
            .map(entry)
    }

    fn groups_of(&self, entry: &PasswdEntry) -> Vec<u32> {
        let Ok(name) = CString::new(entry.name.as_str()) else {
            return Vec::new();
        };
        let mut groups: Vec<libc::c_int> = vec![0; 64];
        loop {
            let mut n = groups.len() as libc::c_int;
            // SAFETY: `groups` holds `n` ints, which getgrouplist fills at
            // most, reporting how many it found in `n`.
            let rc = unsafe {
                libc::getgrouplist(
                    name.as_ptr(),
                    entry.gid as libc::c_int,
                    groups.as_mut_ptr(),
                    &mut n,
                )
            };
            if rc == 0 {
                groups.truncate(n.max(0) as usize);
                break;
            }
            if groups.len() >= 4096 {
                break;
            }
            groups.resize(groups.len() * 4, 0);
        }
        // `setgroups` takes at most NGROUPS_MAX (16) on macOS and refuses the
        // whole list beyond that; the primary group is added by the caller.
        let mut out: Vec<u32> = groups
            .into_iter()
            .map(|g| g as u32)
            .filter(|g| *g != entry.gid)
            .collect();
        out.dedup();
        out.truncate(15);
        out
    }

    fn group_by_name(&self, name: &str) -> Option<u32> {
        nix::unistd::Group::from_name(name)
            .ok()?
            .map(|g| g.gid.as_raw())
    }

    fn accounts_where(&self) -> &'static str {
        "this guest's directory service (dscl . -list /Users)"
    }

    fn groups_where(&self) -> &'static str {
        "this guest's directory service (dscl . -list /Groups)"
    }
}

fn entry(user: nix::unistd::User) -> PasswdEntry {
    PasswdEntry {
        name: user.name,
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
        home: user.dir.to_string_lossy().into_owned(),
        shell: user.shell.to_string_lossy().into_owned(),
    }
}

/// This thread's identity, worn until the value is dropped.
///
/// `pthread_setugid_np` is Darwin's per-thread credential — what its file
/// servers impersonate a client with — and, unlike Linux's `setfsuid`, it
/// carries the account's group memberships too: the kernel resolves them
/// for the assumed uid. It fails rather than lying, so a thread that could
/// not become the session refuses the work instead of doing it as root.
pub(super) struct ThreadIds(());

impl ThreadIds {
    pub(super) fn assume(uid: u32, gid: u32) -> std::io::Result<ThreadIds> {
        // SAFETY: affects only the calling thread's credential.
        match unsafe { ffi::pthread_setugid_np(uid, gid) } {
            0 => Ok(ThreadIds(())),
            _ => Err(std::io::Error::other(format!(
                "cannot read as uid {uid}: {}",
                std::io::Error::last_os_error()
            ))),
        }
    }
}

impl Drop for ThreadIds {
    fn drop(&mut self) {
        // SAFETY: as above; the KAUTH_*_NONE pair returns the thread to the
        // process's own credential.
        unsafe { ffi::pthread_setugid_np(ffi::KAUTH_UID_NONE, ffi::KAUTH_GID_NONE) };
    }
}

/// The libSystem calls the `libc` crate either lacks or marks deprecated in
/// favour of a crate this agent does not otherwise need.
mod ffi {
    use std::sync::OnceLock;

    pub const KERN_SUCCESS: i32 = 0;
    /// <mach/host_info.h>
    pub const HOST_CPU_LOAD_INFO: i32 = 3;
    pub const HOST_VM_INFO64: i32 = 4;
    /// `host_cpu_load_info`: CPU_STATE_MAX natural_t counters.
    pub const HOST_CPU_LOAD_INFO_COUNT: u32 = 4;
    /// `vm_statistics64` in integer_t units: the revision-1 layout every
    /// release since 10.9 fills.
    pub const HOST_VM_INFO64_COUNT: u32 =
        (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<i32>()) as u32;
    /// <sys/kauth.h>: "no assumed identity".
    pub const KAUTH_UID_NONE: libc::uid_t = !0 - 100;
    pub const KAUTH_GID_NONE: libc::gid_t = !0 - 100;

    /// <mach/vm_statistics.h> `struct vm_statistics64`, revision 1.
    #[repr(C)]
    #[derive(Default)]
    pub struct VmStatistics64 {
        pub free_count: u32,
        pub active_count: u32,
        pub inactive_count: u32,
        pub wire_count: u32,
        pub zero_fill_count: u64,
        pub reactivations: u64,
        pub pageins: u64,
        pub pageouts: u64,
        pub faults: u64,
        pub cow_faults: u64,
        pub lookups: u64,
        pub hits: u64,
        pub purges: u64,
        pub purgeable_count: u32,
        pub speculative_count: u32,
        pub decompressions: u64,
        pub compressions: u64,
        pub swapins: u64,
        pub swapouts: u64,
        pub compressor_page_count: u32,
        pub throttled_count: u32,
        pub external_page_count: u32,
        pub internal_page_count: u32,
        pub total_uncompressed_pages_in_compressor: u64,
    }

    unsafe extern "C" {
        fn mach_host_self() -> u32;
        pub fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
        pub fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
        pub fn pthread_setugid_np(uid: libc::uid_t, gid: libc::gid_t) -> libc::c_int;
        pub fn reboot(howto: libc::c_int) -> libc::c_int;
    }

    /// The host port, asked for once: every `mach_host_self` call adds a send
    /// right the caller would otherwise have to release.
    pub fn host() -> u32 {
        static HOST: OnceLock<u32> = OnceLock::new();
        // SAFETY: no arguments; returns this task's host name port.
        *HOST.get_or_init(|| unsafe { mach_host_self() })
    }
}
