//! Container micro-VMs (PRD §18), which only Linux runs: cinit starts the
//! agent with `--container <config>`, and every session then lives inside
//! the workload's namespaces and root filesystem — a terminal through the
//! [`Jail`] its child enters, an exec through the `--nsexec` trampoline, a
//! re-exec of this binary that does the same before it execs.

use std::ffi::CString;
use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use nix::unistd::{ForkResult, Pid, chdir, chroot, fork};

use super::linux::AppPaths;
use super::login::{self, Credentials, Logins, Session};
use super::{UnixPlatform, UnixSpawner};

/// Everything a container session spawn needs, prepared once at startup.
pub(super) struct ContainerCtx {
    pub(super) rootfs: String,
    /// Namespace fds of the workload holder — entering keeps working even
    /// after that process dies. `None` in idle mode (no namespaces; the
    /// prepared rootfs is the whole container).
    pub(super) setns: Option<NsHandles>,
    pub(super) setns_pid: Option<u32>,
    pub(super) env: Vec<(String, String)>,
    pub(super) workdir: String,
    /// §19.2's container floor: the user cinit resolved, which every session
    /// with no declared `login {}` lands as.
    pub(super) floor: Option<Session>,
}

impl ContainerCtx {
    /// What a terminal's child enters before its shell starts, prepared
    /// before the fork so the child allocates nothing.
    pub(super) fn jail(&self) -> std::io::Result<Jail<'_>> {
        Ok(Jail {
            ns: self.setns.as_ref(),
            root: CString::new(self.rootfs.as_str()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in rootfs path")
            })?,
        })
    }
}

/// A container, as the child of a terminal's fork enters it: the workload's
/// namespaces, if it has any, then its root filesystem.
pub(super) struct Jail<'a> {
    ns: Option<&'a NsHandles>,
    root: CString,
}

impl Jail<'_> {
    /// Join the namespaces. Entering a PID namespace only applies to later
    /// children, so this forks once more: the outer child supervises the
    /// actual shell, mirrors its status and never returns; the inner one
    /// returns `true` and carries on. `close` are the PTY fds the
    /// supervisor must not keep open.
    ///
    /// # Safety
    /// Post-fork, pre-exec: async-signal-safe calls only.
    pub(super) unsafe fn enter(&self, close: [libc::c_int; 2]) -> bool {
        let Some(ns) = self.ns else {
            return true;
        };
        if ns.enter().is_err() {
            return false;
        }
        // SAFETY: the caller's post-fork contract; the supervisor only
        // closes fds and waits.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                // SAFETY: closing our copies of the PTY fds.
                unsafe {
                    libc::close(close[0]);
                    libc::close(close[1]);
                }
                mirror_child_exit(child);
            }
            Ok(ForkResult::Child) => true,
            Err(_) => false,
        }
    }

    /// Make the container's root filesystem this process's root.
    ///
    /// # Safety
    /// Post-fork, pre-exec: async-signal-safe calls only.
    pub(super) unsafe fn chroot(&self) -> bool {
        chroot(self.root.as_c_str()).is_ok()
    }
}

/// Open namespace handles (same shape as cinit's `Namespaces`).
pub(super) struct NsHandles {
    pid: File,
    mount: File,
}

impl NsHandles {
    fn open(pid: u32) -> std::io::Result<NsHandles> {
        Ok(NsHandles {
            pid: File::open(format!("/proc/{pid}/ns/pid"))?,
            mount: File::open(format!("/proc/{pid}/ns/mnt"))?,
        })
    }

    /// Enter the mount namespace immediately. Entering a PID namespace only
    /// affects subsequently-created children, so callers must fork once
    /// more (async-signal-safe: raw setns syscalls on open fds).
    fn enter(&self) -> nix::Result<()> {
        use std::os::fd::AsFd;
        nix::sched::setns(self.mount.as_fd(), nix::sched::CloneFlags::CLONE_NEWNS)?;
        nix::sched::setns(self.pid.as_fd(), nix::sched::CloneFlags::CLONE_NEWPID)
    }
}

/// Container-mode platform from cinit's config file.
pub fn new_platform(config_path: &str) -> UnixPlatform {
    let cfg: vmlab_agent_proto::ContainerConfig = std::fs::read_to_string(config_path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| {
            eprintln!("vmlab-agent: FATAL: bad container config {config_path}: {e}");
            std::process::exit(1);
        });
    let setns = cfg.setns_pid.map(|pid| {
        NsHandles::open(pid).unwrap_or_else(|e| {
            eprintln!("vmlab-agent: FATAL: cannot open namespaces of pid {pid}: {e}");
            std::process::exit(1);
        })
    });
    let logins = Arc::new(Logins::for_container(&cfg.rootfs));
    login::start_sweeper(logins.clone());
    let container = Arc::new(ContainerCtx {
        floor: cfg.user.as_deref().and_then(|user| logins.floor(user)),
        rootfs: cfg.rootfs,
        setns,
        setns_pid: cfg.setns_pid,
        env: cfg.env,
        workdir: cfg.workdir.unwrap_or_else(|| "/".to_string()),
    });
    UnixPlatform {
        clipboard: None,
        container: Some(container.clone()),
        spawner: UnixSpawner {
            container: Some(container),
            logins,
        },
        app_paths: AppPaths::default(),
    }
}

/// Where a container session starts, when that is somewhere other than the
/// container's own working directory.
///
/// A **declared** login lands in its own `$HOME`, the way §19.2 asks every
/// session to: `login "dev"` on a container is a person saying "attach me as
/// `dev`", and `/app` is where the *workload* runs. The floor is the workload,
/// so it keeps the workload's directory.
pub(super) fn container_home(ctx: &ContainerCtx, session: Option<&Session>) -> Option<String> {
    let session = session?;
    (session.declared && session.account.home != "/")
        .then(|| session.account.home.clone())
        .filter(|home| Path::new(&format!("{}{home}", ctx.rootfs)).is_dir())
}

/// The trampoline command line `nsexec_main` parses back out: the holder
/// pid (0 in idle mode), the rootfs, the working directory, the session's
/// ids (`-` for none), then the caller's argv after a `--` separator.
pub(super) fn nsexec_argv(
    ctx: &ContainerCtx,
    argv: Vec<String>,
    cwd: Option<String>,
    credentials: Option<&Credentials>,
) -> Vec<String> {
    let mut wrapped = vec![
        "/proc/self/exe".to_string(),
        "--nsexec".to_string(),
        ctx.setns_pid.unwrap_or(0).to_string(),
        ctx.rootfs.clone(),
        cwd.unwrap_or_else(|| ctx.workdir.clone()),
        credentials.map_or_else(|| "-".to_string(), format_credentials),
        "--".to_string(),
    ];
    wrapped.extend(argv);
    wrapped
}

/// `uid:gid:g1,g2,…` — the trampoline's own contract with itself, so the
/// re-exec that crosses the namespaces knows who to become on the far side.
fn format_credentials(c: &Credentials) -> String {
    let groups: Vec<String> = c.groups.iter().map(u32::to_string).collect();
    format!("{}:{}:{}", c.uid, c.gid, groups.join(","))
}

pub(super) fn parse_credentials(field: &str) -> Option<Credentials> {
    if field == "-" {
        return None;
    }
    let mut parts = field.split(':');
    let uid = parts.next()?.parse().ok()?;
    let gid = parts.next()?.parse().ok()?;
    let groups = parts
        .next()
        .unwrap_or_default()
        .split(',')
        .filter_map(|g| g.parse().ok())
        .collect();
    Some(Credentials { uid, gid, groups })
}

/// Container shell (paths as the post-chroot child sees them): prefer the
/// static BusyBox cinit injects so troubleshooting commands are consistent
/// across images, then the image's own /bin/sh (see cinit's mounts).
pub(super) fn container_shell(rootfs: &str) -> Option<Vec<String>> {
    let executable = |inside: &str| {
        let full = format!("{rootfs}{inside}");
        Path::new(&full)
            .metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if executable(vmlab_agent_proto::BUSYBOX_FALLBACK) {
        return Some(vec![
            vmlab_agent_proto::BUSYBOX_FALLBACK.to_string(),
            "sh".to_string(),
        ]);
    }
    executable("/bin/sh").then(|| vec!["/bin/sh".to_string()])
}

/// Container environment: the spec env verbatim plus a terminal type and
/// the injected BusyBox applets at the **end** of PATH (§18), and — for a
/// session that is somebody — the login variables cinit's own `build_env`
/// does not know about (§19.2).
///
/// The shim is a fallback, not a toolbox: it exists so a distroless image
/// still has a shell and the basics for `exec` and `shell`. Ahead of the
/// image's own directories it shadowed them — `mount` in a `debian:12`
/// container resolved to BusyBox's, which reads no `/etc/fstab` options on a
/// remount, instead of util-linux's — so the image's tools win and BusyBox
/// answers only for what the image lacks.
pub(super) fn container_env(
    ctx: &ContainerCtx,
    session: Option<&Session>,
) -> Vec<(String, String)> {
    let mut env = ctx.env.clone();
    if !env.iter().any(|(k, _)| k == "TERM") {
        env.push(("TERM".into(), "xterm-256color".into()));
    }
    let busybox_bin = vmlab_agent_proto::BUSYBOX_BIN_DIR;
    if let Some((_, path)) = env.iter_mut().find(|(k, _)| k == "PATH") {
        if !path.split(':').any(|part| part == busybox_bin) {
            *path = format!("{path}:{busybox_bin}");
        }
    } else {
        env.push((
            "PATH".into(),
            format!("{}:{busybox_bin}", super::os::SUPATH),
        ));
    }
    // A **declared** login overrides what the container says about who it is:
    // cinit's `build_env` put the *workload* user's `HOME` there, and a
    // session attached as `dev` that reports `HOME=/app` is exactly the
    // half-login §19.2 forbids. The floor is the workload, so for it the
    // container's own values stand. `PATH` is neither's — it was assembled
    // above, with the shim on the end.
    if let Some(session) = session {
        for (key, value) in session.env() {
            if key == "PATH" {
                continue;
            }
            match env.iter_mut().find(|(k, _)| *k == key) {
                Some(existing) if session.declared => existing.1 = value,
                Some(_) => {}
                None => env.push((key, value)),
            }
        }
    }
    env
}

/// Wait for a namespace child and exit with an equivalent status. Runs only
/// in the small supervisor created after `setns` (same as cinit's).
fn mirror_child_exit(child: Pid) -> ! {
    let mut status = 0;
    loop {
        // SAFETY: plain waitpid loop; _exit only.
        let rc = unsafe { libc::waitpid(child.as_raw(), &mut status, 0) };
        if rc >= 0 {
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                128 + libc::WTERMSIG(status)
            } else {
                continue;
            };
            // SAFETY: terminating the supervisor.
            unsafe { libc::_exit(code) }
        }
        if nix::errno::Errno::last() != nix::errno::Errno::EINTR {
            // SAFETY: terminating the supervisor.
            unsafe { libc::_exit(127) }
        }
    }
}

/// `vmlab-agent --nsexec <pid|0> <rootfs> <workdir> <ids|-> -- argv…` — the
/// exec trampoline for container mode: join the container's namespaces, fork
/// so the PID namespace applies, chroot, chdir, drop to the session's ids,
/// exec. Environment (already the merged container env) passes through
/// untouched; stdio are the pipes std::process wired up in the parent agent.
pub fn nsexec_main(args: &[String]) -> ! {
    let usage = || -> ! {
        eprintln!("vmlab-agent: bad --nsexec invocation");
        std::process::exit(127);
    };
    let (Some(pid), Some(rootfs), Some(workdir), Some(ids)) =
        (args.first(), args.get(1), args.get(2), args.get(3))
    else {
        usage();
    };
    if args.get(4).map(String::as_str) != Some("--") || args.len() < 6 {
        usage();
    }
    let argv = &args[5..];
    let credentials = parse_credentials(ids);

    let pid: u32 = pid.parse().unwrap_or(0);
    if pid != 0 {
        let Ok(ns) = NsHandles::open(pid) else {
            eprintln!("vmlab-agent: nsexec: cannot open namespaces of pid {pid}");
            std::process::exit(127);
        };
        if ns.enter().is_err() {
            eprintln!("vmlab-agent: nsexec: setns failed");
            std::process::exit(127);
        }
        // The PID namespace applies to children: fork and mirror.
        // SAFETY: single-threaded trampoline process.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => mirror_child_exit(child),
            Ok(ForkResult::Child) => {}
            Err(_) => std::process::exit(127),
        }
    }
    if chroot(rootfs.as_str()).is_err() {
        eprintln!("vmlab-agent: nsexec: chroot {rootfs} failed");
        std::process::exit(127);
    }
    if chdir(workdir.as_str()).is_err() {
        let _ = chdir("/");
    }
    let c_argv: Vec<CString> = argv
        .iter()
        .filter_map(|a| CString::new(a.as_str()).ok())
        .collect();
    if c_argv.len() != argv.len() {
        std::process::exit(127);
    }
    // Last, as in the terminal child: everything above needs root, and the
    // PATH search below should see what the session itself can reach.
    if let Some(credentials) = &credentials {
        // SAFETY: single-threaded trampoline about to exec.
        if !unsafe { credentials.apply() } {
            eprintln!("vmlab-agent: nsexec: cannot become uid {}", credentials.uid);
            std::process::exit(127);
        }
    }
    // execvp: resolves argv[0] via the inherited PATH inside the chroot.
    let _ = nix::unistd::execvp(&c_argv[0], &c_argv);
    eprintln!("vmlab-agent: nsexec: exec {} failed", argv[0]);
    std::process::exit(127);
}
