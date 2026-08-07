//! Linux platform half: virtio-port discovery, the PTY terminal (a
//! namespace-free descendant of `guest/cinit/src/tty.rs`), metrics sampling
//! from /proc, and a best-effort clipboard when a display session is
//! actually reachable (in practice: never on the headless server templates,
//! and the feature is then simply not advertised).
//!
//! Who a session runs as lives next door in [`login`]: PRD §19.2's declared
//! logins, and the container floor — the user cinit resolved, which every
//! session inherits when no `login {}` is declared.

pub mod login;

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, chdir, chroot, execve, fork, setsid};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;

use vmlab_agent_proto::{
    AgentMsg, DiskUsage, NetInterface, OsInfo, PORT_NAME, ShutdownMode, features,
};

use crate::logon::Held;
use crate::mux::Mux;
use crate::spawn::{
    Adopter, Identity, ProcessSpec, Spawned, Spawner, TerminalSpec, adopt_as_agent,
    hold_until_it_exits, piped_command,
};

use login::{Account, Credentials, Logins, Mechanism, Session};

const BANNER: &str = concat!(
    "\n",
    " __   ____  __ _      _   ___ \n",
    " \\ \\ / /  \\/  | |    /_\\ | _ )\n",
    "  \\ V /| |\\/| | |__ / _ \\| _ \\\n",
    "   \\_/ |_|  |_|____/_/ \\_\\___/\n",
    "\n",
);

/// Who this terminal's shell is, as its banner puts it.
///
/// A declared login also names the mechanism that realised it (§19.2's two
/// routes), because "rootless podman does not work in here" is a question a
/// developer can only answer if they can see which of the two they got.
fn whose_shell(session: Option<&Session>) -> String {
    match session {
        None => "root shell".to_string(),
        Some(s) if s.declared => format!(
            "login shell for `{}` (uid {}) via {}",
            s.account.name,
            s.account.uid,
            s.mechanism.describe()
        ),
        Some(s) => format!("shell as `{}` (uid {})", s.account.name, s.account.uid),
    }
}

/// The banner a terminal opens with, on a VM and inside a container.
fn terminal_motd(route: &Route) -> String {
    let session = match route {
        Route::Container(_, session) => *session,
        Route::Agent => None,
        Route::Pam(s, _) | Route::Setuid(s) => Some(*s),
    };
    let mut motd = String::from(BANNER);
    if matches!(route, Route::Container(..)) {
        motd.push_str("vmlab container terminal\n");
        motd.push_str(&format!(
            "  {} inside the container namespaces.\n",
            whose_shell(session)
        ));
        motd.push_str(
            "  The image filesystem, environment, volumes, and processes are available here.\n",
        );
        motd.push_str(
            "  Run 'busybox --list' to see tools; 'exit' ends this session; Ctrl-] detaches.\n\n",
        );
    } else {
        motd.push_str(&format!(
            "vmlab terminal - {} over virtio-serial (works with no network).\n",
            whose_shell(session)
        ));
        motd.push_str("  'exit' ends this session; Ctrl-] detaches the CLI client.\n\n");
    }
    motd
}

// Guest-only ioctls (the whole VM is ours; agent work allows unsafe here).
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, Winsize);
nix::ioctl_write_int_bad!(tiocsctty, libc::TIOCSCTTY);

fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
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

/// Open the agent port read+write (virtio ports are exclusive-open; the two
/// halves are fd clones). Retries until the device exists — the service may
/// start before the virtio-console driver has bound. A busy port means
/// another agent instance is serving: exit quietly so double-starts are
/// harmless.
pub fn open_port() -> (
    impl Read + Send + 'static,
    impl std::io::Write + Send + 'static,
) {
    loop {
        let Some(path) = find_virtio_port(PORT_NAME) else {
            eprintln!("vmlab-agent: waiting for port {PORT_NAME}");
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(port) => match port.try_clone() {
                Ok(w) => return (port, w),
                Err(e) => {
                    eprintln!("vmlab-agent: port clone failed: {e}");
                    thread::sleep(Duration::from_secs(2));
                }
            },
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                eprintln!("vmlab-agent: port {PORT_NAME} busy (another instance is serving)");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("vmlab-agent: cannot open {}: {e}", path.display());
                thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

pub struct LinuxPlatform {
    clipboard: Option<ClipboardTool>,
    /// Container micro-VM mode (spawned by cinit): sessions run inside the
    /// container instead of the init namespace.
    container: Option<Arc<ContainerCtx>>,
    spawner: LinuxSpawner,
}

/// The Linux half of the process/handle seam: PTY terminals, piped exec, and
/// the file writes behind `push` — in the init namespace, or inside the
/// container when cinit started us in container mode. Each as the agent, or
/// as a declared login the channel carried (PRD §19.2).
pub struct LinuxSpawner {
    container: Option<Arc<ContainerCtx>>,
    logins: Arc<Logins>,
}

/// Everything a container session spawn needs, prepared once at startup.
struct ContainerCtx {
    rootfs: String,
    /// Namespace fds of the workload holder — entering keeps working even
    /// after that process dies. `None` in idle mode (no namespaces; the
    /// prepared rootfs is the whole container).
    setns: Option<NsHandles>,
    setns_pid: Option<u32>,
    env: Vec<(String, String)>,
    workdir: String,
    /// §19.2's container floor: the user cinit resolved, which every session
    /// with no declared `login {}` lands as.
    floor: Option<Session>,
}

/// Open namespace handles (same shape as cinit's `Namespaces`).
struct NsHandles {
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

pub fn new_platform() -> LinuxPlatform {
    let logins = Arc::new(Logins::for_vm());
    login::start_sweeper(logins.clone());
    LinuxPlatform {
        clipboard: ClipboardTool::probe(),
        container: None,
        spawner: LinuxSpawner {
            container: None,
            logins,
        },
    }
}

/// Container-mode platform from cinit's config file.
pub fn new_platform_container(config_path: &str) -> LinuxPlatform {
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
    LinuxPlatform {
        clipboard: None,
        container: Some(container.clone()),
        spawner: LinuxSpawner {
            container: Some(container),
            logins,
        },
    }
}

impl crate::mux::Platform for LinuxPlatform {
    fn os(&self) -> &'static str {
        "linux"
    }

    fn features(&self) -> Vec<String> {
        let mut f = vec![
            features::TERMINAL.to_string(),
            features::EXEC.to_string(),
            features::FILEOPS.to_string(),
            features::TAIL.to_string(),
            features::METRICS.to_string(),
            features::TUNNEL.to_string(),
            features::WATCH.to_string(),
        ];
        if self.clipboard.is_some() {
            f.push(features::CLIPBOARD.to_string());
        }
        f
    }

    fn spawner(&self) -> &dyn Spawner {
        &self.spawner
    }

    fn path_resolver(&self) -> crate::mux::PathResolver {
        match &self.container {
            None => Arc::new(|path| path),
            Some(ctx) => {
                let rootfs = ctx.rootfs.clone();
                Arc::new(move |path: String| format!("{rootfs}/{}", path.trim_start_matches('/')))
            }
        }
    }

    fn open_eventlog(&self, mux: &Mux, id: u32, _filter: Option<String>) {
        mux.send_error(Some(id), "event log tailing is Windows-only");
    }

    fn set_clipboard(&self, mux: &Mux, text: String) {
        match &self.clipboard {
            Some(tool) => {
                if let Err(e) = tool.set(&text) {
                    mux.send_error(None, format!("clipboard: {e}"));
                }
            }
            None => mux.send_error(None, "clipboard: no display session reachable"),
        }
    }

    fn get_clipboard(&self, mux: &Mux) {
        match &self.clipboard {
            Some(tool) => match tool.get() {
                Ok(text) => mux.send_ctrl(&AgentMsg::Clipboard { text }),
                Err(e) => mux.send_error(None, format!("clipboard: {e}")),
            },
            None => mux.send_error(None, "clipboard: no display session reachable"),
        }
    }

    fn net_info(&self) -> Result<Vec<NetInterface>, String> {
        net_info()
    }

    fn os_info(&self) -> Result<OsInfo, String> {
        Ok(os_info())
    }

    fn shutdown(&self, mux: &Mux, mode: ShutdownMode) {
        let mux = mux.clone();
        thread::spawn(move || {
            // Let the ShuttingDown ack drain to the host first.
            thread::sleep(Duration::from_millis(200));
            if let Err(e) = run_shutdown(mode) {
                mux.send_error(None, format!("shutdown: {e}"));
            }
        });
    }
}

/// How a channel's work gets to be who it is — decided once, then read by
/// both the terminal and the exec shapes so they cannot drift apart.
enum Route<'a> {
    /// Inside a container micro-VM, as whoever the session is: the declared
    /// login, else §19.2's container floor, else root.
    Container(&'a ContainerCtx, Option<&'a Session>),
    /// The agent's own identity on a VM — §19.2's floor, no login at all.
    Agent,
    /// Through the guest's own `su`, which *is* the login: it sets the
    /// environment, the groups and the working directory, and the agent
    /// hands it the account and gets out of the way.
    Pam(&'a Session, &'a str),
    /// A guest with no PAM: the agent assembles the login itself.
    Setuid(&'a Session),
}

impl LinuxSpawner {
    /// The route a channel's work takes: the declared login where the open
    /// carried one, else the container floor, else the agent's own identity
    /// (PRD §19.2).
    fn route<'a>(&'a self, held: &'a Option<Arc<Held<Session>>>) -> Route<'a> {
        self.route_for(held.as_deref().map(|h| &h.value))
    }

    /// The route for a login already resolved — `None` meaning "whatever this
    /// machine's floor is", which is the container's user or nothing at all.
    fn route_for<'a>(&'a self, declared: Option<&'a Session>) -> Route<'a> {
        let session = declared.or_else(|| self.container.as_ref().and_then(|c| c.floor.as_ref()));
        match (&self.container, session) {
            (Some(ctx), session) => Route::Container(ctx, session),
            (None, None) => Route::Agent,
            (
                None,
                Some(
                    s @ Session {
                        mechanism: Mechanism::Pam { su },
                        ..
                    },
                ),
            ) => Route::Pam(s, su),
            (None, Some(s)) => Route::Setuid(s),
        }
    }

    /// Where a piped exec runs, and as whom.
    fn exec_plan(&self, route: &Route, spec: ProcessSpec) -> std::io::Result<ExecPlan> {
        if spec.argv.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty argv",
            ));
        }
        Ok(match route {
            // Reroute through the nsexec trampoline (a re-exec of this binary
            // that setns's, forks into the PID namespace, chroots, drops to
            // the session's ids, and execs) so the process genuinely lives
            // inside the container while std::process handles the pipes.
            Route::Container(ctx, session) => {
                let mut env = container_env(ctx, *session);
                env.extend(spec.env);
                ExecPlan {
                    spec: ProcessSpec {
                        argv: nsexec_argv(
                            ctx,
                            spec.argv,
                            spec.cwd.or_else(|| container_home(ctx, *session)),
                            session.and_then(login::credentials_for).as_ref(),
                        ),
                        env,
                        cwd: None,
                    },
                    credentials: None,
                    fresh_env: false,
                }
            }
            Route::Agent => ExecPlan {
                spec,
                credentials: None,
                fresh_env: false,
            },
            // Everything the caller asked for becomes the script `su -l`
            // runs, because `su` resets the environment and the working
            // directory as part of being a login.
            Route::Pam(s, su) => ExecPlan {
                spec: ProcessSpec {
                    argv: login::su_argv(
                        su,
                        &s.account.name,
                        Some(login::login_script(
                            &spec.env,
                            spec.cwd.as_deref(),
                            &spec.argv,
                        )),
                    ),
                    env: vec![],
                    cwd: None,
                },
                credentials: None,
                fresh_env: false,
            },
            Route::Setuid(s) => {
                let mut env = s.env();
                env.extend(spec.env);
                ExecPlan {
                    spec: ProcessSpec {
                        argv: spec.argv,
                        env,
                        cwd: Some(spec.cwd.unwrap_or_else(|| s.account.home.clone())),
                    },
                    credentials: login::credentials_for(s),
                    fresh_env: true,
                }
            }
        })
    }

    /// Everything the forked shell child needs, decided before the fork.
    fn shell_plan(
        &self,
        route: &Route,
        command: Option<Vec<String>>,
    ) -> std::io::Result<ShellPlan> {
        let motd = terminal_motd(route);
        let plan = match route {
            // The injected BusyBox (so a distroless image still gets a
            // toolbox) as whoever the session is.
            Route::Container(ctx, session) => ShellPlan {
                argv: command.or_else(|| container_shell(&ctx.rootfs)),
                env: container_env(ctx, *session),
                credentials: session.and_then(login::credentials_for),
                cwd: container_home(ctx, *session).unwrap_or_else(|| ctx.workdir.clone()),
                motd,
            },
            Route::Agent => ShellPlan {
                argv: command.or_else(default_shell),
                env: root_env(),
                credentials: None,
                cwd: "/root".to_string(),
                motd,
            },
            Route::Pam(s, su) => ShellPlan {
                argv: Some(login::su_argv(
                    su,
                    &s.account.name,
                    command.map(|argv| login::login_script(&[], None, &argv)),
                )),
                env: vec![
                    ("TERM".to_string(), "xterm-256color".to_string()),
                    ("PATH".to_string(), login::SUPATH.to_string()),
                ],
                credentials: None,
                cwd: s.account.home.clone(),
                motd,
            },
            Route::Setuid(s) => {
                let mut env = s.env();
                env.push(("TERM".to_string(), "xterm-256color".to_string()));
                ShellPlan {
                    argv: Some(command.unwrap_or_else(|| s.account.login_shell())),
                    env,
                    credentials: login::credentials_for(s),
                    cwd: s.account.home.clone(),
                    motd,
                }
            }
        };
        match plan.argv {
            Some(argv) if !argv.is_empty() => Ok(ShellPlan {
                argv: Some(argv),
                ..plan
            }),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no shell found in this guest",
            )),
        }
    }
}

/// Whose the session's files and terminal are — `None` for §19.2's floor on a
/// VM, where the agent is already itself.
fn owner<'a>(route: &'a Route) -> Option<&'a Account> {
    match route {
        Route::Container(_, session) => session.map(|s| &s.account),
        Route::Agent => None,
        Route::Pam(s, _) | Route::Setuid(s) => Some(&s.account),
    }
}

/// Where a container session starts, when that is somewhere other than the
/// container's own working directory.
///
/// A **declared** login lands in its own `$HOME`, the way §19.2 asks every
/// session to: `login "dev"` on a container is a person saying "attach me as
/// `dev`", and `/app` is where the *workload* runs. The floor is the workload,
/// so it keeps the workload's directory.
fn container_home(ctx: &ContainerCtx, session: Option<&Session>) -> Option<String> {
    let session = session?;
    (session.declared && session.account.home != "/")
        .then(|| session.account.home.clone())
        .filter(|home| Path::new(&format!("{}{home}", ctx.rootfs)).is_dir())
}

/// The piped spawn, resolved: what to run, as whom, and where.
#[derive(Debug, PartialEq, Eq)]
struct ExecPlan {
    spec: ProcessSpec,
    /// Ids the child drops to, in a `pre_exec`. `None` where nothing is
    /// dropped: the agent's own identity, or the PAM route, where `su` does
    /// the dropping — and the trampoline, which carries them in its argv
    /// because they only apply on the far side of the chroot.
    credentials: Option<Credentials>,
    /// Whether the child starts from an empty environment. A login does: it
    /// gets the login's variables and nothing the agent service happens to be
    /// holding. The agent's own exec inherits, as it always has.
    fresh_env: bool,
}

/// Carry out an [`ExecPlan`].
fn spawn_piped(plan: ExecPlan) -> std::io::Result<Spawned> {
    let ExecPlan {
        spec,
        credentials,
        fresh_env,
    } = plan;
    piped_command(spec, |cmd| {
        if fresh_env {
            cmd.env_clear();
        }
        if let Some(credentials) = credentials {
            // SAFETY: `pre_exec` runs between fork and exec, which is exactly
            // `apply`'s contract; its group list was allocated here, before
            // the fork.
            unsafe {
                cmd.pre_exec(move || match credentials.apply() {
                    true => Ok(()),
                    false => Err(std::io::Error::last_os_error()),
                })
            };
        }
    })
}

/// The shell spawn, resolved: what to run, as whom, and where. `argv` is
/// `Option` only while it is being resolved; [`LinuxSpawner::shell_plan`] is
/// the only constructor and never hands one back empty.
struct ShellPlan {
    argv: Option<Vec<String>>,
    env: Vec<(String, String)>,
    /// Ids the child drops to. `None` where nothing is dropped — the agent's
    /// own identity, or the PAM route, where `su` does the dropping and
    /// would be unable to if the agent had gone first.
    credentials: Option<Credentials>,
    cwd: String,
    motd: String,
}

impl Spawner for LinuxSpawner {
    fn terminal(&self, identity: &Identity, spec: TerminalSpec) -> std::io::Result<Spawned> {
        let held = self.logins.resolve(identity)?;
        let route = self.route(&held);
        let plan = self.shell_plan(&route, spec.command)?;
        let size = winsize(spec.cols, spec.rows);
        let (master, pid) = spawn_shell(&plan, &size, self.container.as_deref(), owner(&route))?;
        // The master is shared: one dup drives keystrokes in, another the VT
        // stream out, and the resize hook needs the fd itself.
        let master = Arc::new(master);
        let input = File::from(master.try_clone()?);
        let output = File::from(master.try_clone()?);
        let resize_master = master.clone();
        let spawned = Spawned {
            input: Box::new(input),
            output: Box::new(output),
            errors: None,
            resize: Some(Box::new(move |cols, rows| {
                let ws = winsize(cols, rows);
                // SAFETY: TIOCSWINSZ on a live PTY master with a valid size.
                let _ = unsafe { tiocswinsz(resize_master.as_raw_fd(), &ws) };
            })),
            kill: Box::new(move || {
                let _ = kill(pid, Signal::SIGKILL);
            }),
            wait: Box::new(move || {
                let code = match waitpid(pid, None) {
                    Ok(WaitStatus::Exited(_, code)) => code,
                    Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
                    _ => 127,
                };
                drop(master); // closes our master ref → output pump sees EOF/EIO
                code
            }),
        };
        Ok(hold_until_it_exits(spawned, held))
    }

    fn exec(&self, identity: &Identity, spec: ProcessSpec) -> std::io::Result<Spawned> {
        let held = self.logins.resolve(identity)?;
        let plan = self.exec_plan(&self.route(&held), spec)?;
        Ok(hold_until_it_exits(spawn_piped(plan)?, held))
    }

    fn adopter(&self, identity: &Identity) -> std::io::Result<Adopter> {
        let held = self.logins.resolve(identity)?;
        let account = owner(&self.route(&held)).cloned();
        match account {
            Some(account) => Ok(login::adopter_as(account, held)),
            None => Ok(adopt_as_agent()),
        }
    }
}

/// The trampoline command line `nsexec_main` parses back out: the holder
/// pid (0 in idle mode), the rootfs, the working directory, the session's
/// ids (`-` for none), then the caller's argv after a `--` separator.
fn nsexec_argv(
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

fn parse_credentials(field: &str) -> Option<Credentials> {
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

/// Guest NICs via getifaddrs: link-layer, IPv4 and IPv6 entries for one
/// interface are merged into one [`NetInterface`]. Loopback is excluded.
fn net_info() -> Result<Vec<NetInterface>, String> {
    let addrs = nix::ifaddrs::getifaddrs().map_err(|e| e.to_string())?;
    let mut out: Vec<NetInterface> = Vec::new();
    for ifa in addrs {
        if ifa
            .flags
            .contains(nix::net::if_::InterfaceFlags::IFF_LOOPBACK)
        {
            continue;
        }
        let entry = match out.iter_mut().find(|i| i.name == ifa.interface_name) {
            Some(e) => e,
            None => {
                out.push(NetInterface {
                    name: ifa.interface_name.clone(),
                    mac: None,
                    ipv4: Vec::new(),
                    ipv6: Vec::new(),
                });
                out.last_mut().unwrap()
            }
        };
        let Some(addr) = ifa.address else { continue };
        if let Some(link) = addr.as_link_addr() {
            if let Some(mac) = link.addr()
                && mac != [0u8; 6]
            {
                entry.mac = Some(
                    mac.iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(":"),
                );
            }
        } else if let Some(sin) = addr.as_sockaddr_in() {
            entry.ipv4.push(sin.ip().to_string());
        } else if let Some(sin6) = addr.as_sockaddr_in6() {
            entry.ipv6.push(sin6.ip().to_string());
        }
    }
    Ok(out)
}

/// Structured OS info from /etc/os-release + uname. Infallible: an initramfs
/// without os-release (container micro-VMs) still answers with the kernel's
/// view.
fn os_info() -> OsInfo {
    let os_release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |key: &str| {
        os_release
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    };
    let uts = uname();
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

/// (kernel release, machine arch, hostname) via uname(2).
fn uname() -> (String, String, String) {
    // SAFETY: plain uname(2) into a zeroed struct.
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } != 0 {
        return (String::new(), String::new(), String::new());
    }
    let s = |f: &[libc::c_char]| {
        // SAFETY: uname NUL-terminates every field.
        unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    (s(&uts.release), s(&uts.machine), s(&uts.nodename))
}

/// Bring the guest down: ask the init system so services stop cleanly, and
/// fall back to the raw reboot(2) syscall — which is the *only* path in a
/// container micro-VM's initramfs (no init system; the agent is cinit's
/// child and runs with CAP_SYS_BOOT).
fn run_shutdown(mode: ShutdownMode) -> Result<(), String> {
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

/// The default interactive shell: a bash login shell when the guest has
/// bash, else POSIX sh.
fn default_shell() -> Option<Vec<String>> {
    for sh in ["/bin/bash", "/usr/bin/bash"] {
        if Path::new(sh).exists() {
            return Some(vec![sh.to_string(), "-l".to_string()]);
        }
    }
    Path::new("/bin/sh")
        .exists()
        .then(|| vec!["/bin/sh".to_string(), "-l".to_string()])
}

/// Container shell (paths as the post-chroot child sees them): prefer the
/// static BusyBox cinit injects so troubleshooting commands are consistent
/// across images, then the image's own /bin/sh (see cinit's mounts).
fn container_shell(rootfs: &str) -> Option<Vec<String>> {
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

/// The agent identity's own shell environment: root, on a VM.
fn root_env() -> Vec<(String, String)> {
    [
        ("TERM", "xterm-256color"),
        ("HOME", "/root"),
        ("USER", "root"),
        ("LOGNAME", "root"),
        ("SHELL", "/bin/sh"),
        ("PATH", login::SUPATH),
        ("LANG", "C.UTF-8"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Container environment: the spec env verbatim plus a terminal type and
/// the injected BusyBox applets at the front of PATH (mirrors what cinit's
/// tty did), and — for a session that is somebody — the login variables
/// cinit's own `build_env` does not know about (§19.2).
fn container_env(ctx: &ContainerCtx, session: Option<&Session>) -> Vec<(String, String)> {
    let mut env = ctx.env.clone();
    if !env.iter().any(|(k, _)| k == "TERM") {
        env.push(("TERM".into(), "xterm-256color".into()));
    }
    let busybox_bin = vmlab_agent_proto::BUSYBOX_BIN_DIR;
    if let Some((_, path)) = env.iter_mut().find(|(k, _)| k == "PATH") {
        if !path.split(':').any(|part| part == busybox_bin) {
            *path = format!("{busybox_bin}:{path}");
        }
    } else {
        env.push(("PATH".into(), format!("{busybox_bin}:{}", login::SUPATH)));
    }
    // A **declared** login overrides what the container says about who it is:
    // cinit's `build_env` put the *workload* user's `HOME` there, and a
    // session attached as `dev` that reports `HOME=/app` is exactly the
    // half-login §19.2 forbids. The floor is the workload, so for it the
    // container's own values stand. `PATH` is neither's — it was assembled
    // above, with the toolbox on the front.
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

/// Allocate a PTY sized `size` and fork the shell: the child becomes a
/// session leader on the slave, drops to the session's ids and execs. In
/// container mode the child first joins the container's namespaces (an extra
/// fork applies the PID namespace, with the intermediate mirroring the
/// shell's exit status) and chroots into its rootfs. Returns the master
/// (close-on-exec) and the child pid. Post-fork the child only performs
/// async-signal-safe operations before execve (allocation-free, like cinit's
/// spawn).
fn spawn_shell(
    plan: &ShellPlan,
    size: &Winsize,
    container: Option<&ContainerCtx>,
    owner: Option<&Account>,
) -> std::io::Result<(OwnedFd, Pid)> {
    let shell = match plan.argv.as_deref() {
        Some(argv) if !argv.is_empty() => argv,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no shell found in this guest",
            ));
        }
    };
    let pty = openpty(size, None).map_err(std::io::Error::from)?;
    fcntl(&pty.master, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(std::io::Error::from)?;
    // The terminal belongs to whoever is about to log in on it, or nothing
    // that reopens `/dev/tty` works (PRD §19.2).
    if let Some(account) = owner {
        login::own_the_terminal(&pty.slave, account);
    }

    let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, what.to_string());
    let c_exe = CString::new(shell[0].as_str()).map_err(|_| bad("NUL in shell path"))?;
    let c_argv: Vec<CString> = shell
        .iter()
        .map(|a| CString::new(a.as_str()))
        .collect::<Result<_, _>>()
        .map_err(|_| bad("NUL in shell argv"))?;
    let env: Vec<CString> = plan
        .env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")))
        .collect::<Result<_, _>>()
        .map_err(|_| bad("NUL in shell environment"))?;
    let c_root_dir = container
        .map(|c| CString::new(c.rootfs.as_str()))
        .transpose()
        .map_err(|_| bad("NUL in rootfs path"))?;
    let c_workdir = CString::new(plan.cwd.as_str()).map_err(|_| bad("NUL in workdir"))?;
    let c_root = CString::new("/").unwrap();
    let credentials = plan.credentials.clone();
    let motd = plan.motd.as_str();
    let slave_raw = pty.slave.as_raw_fd();
    let master_raw = pty.master.as_raw_fd();

    // SAFETY: multithreaded fork; the child only performs async-signal-safe
    // operations (raw syscalls via nix + _exit) before execve.
    match unsafe { fork() }.map_err(std::io::Error::from)? {
        ForkResult::Parent { child } => {
            drop(pty.slave); // parent keeps only the master
            Ok((pty.master, child))
        }
        ForkResult::Child => {
            let die = |_what: &str| -> ! { unsafe { libc::_exit(127) } };
            if let Some(ns) = container.and_then(|c| c.setns.as_ref()) {
                if ns.enter().is_err() {
                    die("setns");
                }
                // PID setns applies to children, so this outer child
                // supervises the actual shell and mirrors its status.
                match unsafe { fork() } {
                    Ok(ForkResult::Parent { child }) => {
                        // SAFETY: closing our copies of the PTY fds.
                        unsafe {
                            libc::close(slave_raw);
                            libc::close(master_raw);
                        }
                        mirror_child_exit(child);
                    }
                    Ok(ForkResult::Child) => {}
                    Err(_) => die("fork after setns"),
                }
            }
            if setsid().is_err() {
                die("setsid");
            }
            // SAFETY: TIOCSCTTY on the fresh session's slave fd.
            if unsafe { tiocsctty(slave_raw, 0) }.is_err() {
                die("tiocsctty");
            }
            // SAFETY: post-fork, redirecting stdio onto the PTY slave. Raw
            // libc rather than nix's `dup2`, which since 0.31 wants an
            // `AsFd`/`&mut OwnedFd` pair we do not have here — and which the
            // async-signal-safe contract would not let us construct anyway.
            for fd in 0..=2 {
                if unsafe { libc::dup2(slave_raw, fd) } < 0 {
                    die("dup2");
                }
            }
            let _ = write_all_raw(libc::STDOUT_FILENO, motd.as_bytes());
            if let Some(root) = &c_root_dir
                && chroot(root.as_c_str()).is_err()
            {
                die("chroot");
            }
            // Privileges go last: the group calls need the very privilege
            // they give up, and everything above needs root.
            if let Some(credentials) = &credentials {
                // SAFETY: post-fork, pre-exec, on a group list allocated
                // before the fork.
                if !unsafe { credentials.apply() } {
                    die("setuid");
                }
            }
            if chdir(c_workdir.as_c_str()).is_err() && chdir(c_root.as_c_str()).is_err() {
                die("chdir");
            }
            let _ = execve(&c_exe, &c_argv, &env);
            die("execve");
        }
    }
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

fn write_all_raw(fd: libc::c_int, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        // SAFETY: plain write(2) on a valid fd with an in-bounds buffer.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n > 0 {
            buf = &buf[n as usize..];
        } else if n < 0 && nix::errno::Errno::last() == nix::errno::Errno::EINTR {
            continue;
        } else {
            return false;
        }
    }
    true
}

pub fn kill_process(pid: u32) {
    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
}

// ---- metrics sampling ------------------------------------------------------

/// Cumulative (busy, total) jiffies from /proc/stat's aggregate cpu line.
pub type CpuSample = (u64, u64);

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

pub fn cpu_pct(prev: &CpuSample, cur: &CpuSample) -> f32 {
    let busy = cur.0.saturating_sub(prev.0) as f32;
    let total = cur.1.saturating_sub(prev.1) as f32;
    if total <= 0.0 {
        0.0
    } else {
        (100.0 * busy / total).clamp(0.0, 100.0)
    }
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
        // c_ulong == u64 on every target we build (all 64-bit).
        let frsize: u64 = vfs.fragment_size();
        let total = vfs.blocks() * frsize;
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

struct ClipboardTool {
    get: Vec<String>,
    set: Vec<String>,
}

impl ClipboardTool {
    /// A clipboard exists only when the agent's own environment can reach a
    /// display server (never true for the root service on server templates)
    /// and a helper tool is installed.
    fn probe() -> Option<ClipboardTool> {
        let has = |bin: &str| {
            std::env::var_os("PATH")
                .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(bin).exists()))
        };
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && has("wl-copy") && has("wl-paste") {
            return Some(ClipboardTool {
                get: vec!["wl-paste".into(), "--no-newline".into()],
                set: vec!["wl-copy".into()],
            });
        }
        if std::env::var_os("DISPLAY").is_some() && has("xclip") {
            return Some(ClipboardTool {
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

    fn get(&self) -> std::io::Result<String> {
        let out = Command::new(&self.get[0])
            .args(&self.get[1..])
            .stderr(Stdio::null())
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn set(&self, text: &str) -> std::io::Result<()> {
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
        child.wait()?;
        Ok(())
    }
}

/// Detach from the launching terminal/agent: double-fork + setsid + stdio to
/// /dev/null. Used for manual hand-launches during development; the
/// systemd service runs in the foreground.
pub fn daemonize() {
    // SAFETY: standard double-fork; parents _exit immediately.
    unsafe {
        match libc::fork() {
            -1 => std::process::exit(1),
            0 => {}
            _ => libc::_exit(0),
        }
        if libc::setsid() < 0 {
            std::process::exit(1);
        }
        match libc::fork() {
            -1 => std::process::exit(1),
            0 => {}
            _ => libc::_exit(0),
        }
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..=2 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_pct_computes_delta_utilisation() {
        assert_eq!(cpu_pct(&(50, 100), &(80, 200)), 30.0);
        assert_eq!(cpu_pct(&(0, 0), &(0, 0)), 0.0);
        // Clamped even if counters go weird.
        assert_eq!(cpu_pct(&(0, 100), &(300, 200)), 100.0);
    }

    #[test]
    fn proc_samples_do_not_panic() {
        // Smoke on the build host (also has /proc).
        let (busy, total) = cpu_sample();
        assert!(total >= busy);
        let (used, total) = mem_sample();
        assert!(total >= used);
        let disks = disk_sample();
        for d in &disks {
            assert!(d.total >= d.used, "{}", d.mount);
        }
    }

    #[test]
    fn net_and_os_info_sample_the_build_host() {
        let ifaces = net_info().unwrap();
        assert!(ifaces.iter().all(|i| i.name != "lo"));
        let info = os_info();
        assert!(!info.kernel.is_empty());
        assert!(!info.arch.is_empty());
        assert_ne!(info.id, "");
    }

    #[test]
    fn default_shell_exists_on_the_build_host() {
        let shell = default_shell().unwrap();
        assert!(Path::new(&shell[0]).exists());
        assert_eq!(shell[1], "-l");
    }

    #[test]
    fn motd_mentions_detach_and_no_network() {
        let vm = terminal_motd(&vm_spawner().route_for(None));
        assert!(vm.contains("Ctrl-]"));
        assert!(vm.contains("no network"));
        assert!(vm.contains("root shell"));
        assert!(
            terminal_motd(&container_spawner(test_ctx("/rootfs", vec![])).route_for(None))
                .contains("busybox --list")
        );
    }

    /// §19.2 asks for the mechanism to be observable, and the banner is where
    /// a developer sees it without going looking for the agent's log.
    #[test]
    fn the_banner_names_the_session_and_how_it_was_realised() {
        let pam = test_session(
            "dev",
            1000,
            Mechanism::Pam {
                su: "/bin/su".into(),
            },
            true,
        );
        let banner = terminal_motd(&vm_spawner().route_for(Some(&pam)));
        assert!(
            banner.contains("login shell for `dev` (uid 1000)"),
            "{banner}"
        );
        assert!(banner.contains("PAM"), "{banner}");

        let fallback = test_session("dev", 1000, Mechanism::Setuid, true);
        let banner = terminal_motd(&vm_spawner().route_for(Some(&fallback)));
        assert!(banner.contains("setuid"), "{banner}");
        assert!(banner.contains("no PAM"), "{banner}");

        // The container floor is not something a lab author declared, so the
        // banner states who without claiming a login was asked for.
        let floor = test_session("app", 101, Mechanism::Setuid, false);
        let banner =
            terminal_motd(&container_spawner(test_ctx("/rootfs", vec![])).route_for(Some(&floor)));
        assert!(banner.contains("shell as `app` (uid 101)"), "{banner}");
        assert!(!banner.contains("login shell"), "{banner}");
    }

    fn test_session(name: &str, uid: u32, mechanism: Mechanism, declared: bool) -> Session {
        Session {
            account: Account {
                name: name.to_string(),
                uid,
                gid: uid,
                groups: vec![uid],
                home: format!("/home/{name}"),
                shell: "/bin/bash".to_string(),
            },
            mechanism,
            declared,
            runtime_dir: Some(format!("/run/user/{uid}")),
        }
    }

    fn touch_exe(path: &str) {
        std::fs::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn container_shell_prefers_busybox_then_image_sh() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().to_str().unwrap().to_string();
        assert_eq!(container_shell(&rootfs), None);

        touch_exe(&format!("{rootfs}/bin/sh"));
        assert_eq!(container_shell(&rootfs), Some(vec!["/bin/sh".to_string()]));

        touch_exe(&format!("{rootfs}{}", vmlab_agent_proto::BUSYBOX_FALLBACK));
        assert_eq!(
            container_shell(&rootfs),
            Some(vec![
                vmlab_agent_proto::BUSYBOX_FALLBACK.to_string(),
                "sh".to_string()
            ])
        );
    }

    fn test_ctx(rootfs: &str, env: Vec<(String, String)>) -> ContainerCtx {
        ContainerCtx {
            rootfs: rootfs.to_string(),
            setns: None,
            setns_pid: None,
            env,
            workdir: "/".to_string(),
            floor: None,
        }
    }

    #[test]
    fn container_env_adds_term_and_busybox_path() {
        let ctx = test_ctx("/rootfs", vec![("FOO".into(), "bar".into())]);
        let env = container_env(&ctx, None);
        assert!(env.contains(&("TERM".into(), "xterm-256color".into())));
        assert!(
            env.iter()
                .any(|(k, v)| k == "PATH" && v.starts_with(vmlab_agent_proto::BUSYBOX_BIN_DIR))
        );
        assert!(env.contains(&("FOO".into(), "bar".into())));

        // Explicit values win; PATH still gets the toolbox prepended once.
        let ctx = test_ctx(
            "/rootfs",
            vec![
                ("TERM".into(), "vt100".into()),
                ("PATH".into(), "/only".into()),
            ],
        );
        let env = container_env(&ctx, None);
        assert_eq!(env.iter().filter(|(k, _)| k == "TERM").count(), 1);
        assert!(env.contains(&(
            "PATH".into(),
            format!("{}:/only", vmlab_agent_proto::BUSYBOX_BIN_DIR)
        )));
    }

    /// The container's own env is the image's and the lab author's, so it
    /// wins — but a session still gets the login variables cinit never sets.
    #[test]
    fn a_container_session_gets_its_login_variables() {
        let ctx = test_ctx("/rootfs", vec![("HOME".into(), "/app".into())]);
        let session = test_session("app", 101, Mechanism::Setuid, false);
        let env = container_env(&ctx, Some(&session));
        assert!(env.contains(&("USER".into(), "app".into())));
        assert!(env.contains(&("LOGNAME".into(), "app".into())));
        assert!(env.contains(&("XDG_RUNTIME_DIR".into(), "/run/user/101".into())));
        assert_eq!(
            env.iter().filter(|(k, _)| k == "HOME").count(),
            1,
            "the container's own HOME is not second-guessed"
        );
        assert!(env.contains(&("HOME".into(), "/app".into())));
    }

    /// A platform in container mode, sharing one context with its spawner
    /// the way `new_platform_container` does.
    fn container_platform(ctx: ContainerCtx) -> LinuxPlatform {
        let ctx = Arc::new(ctx);
        let logins = Arc::new(Logins::for_container(&ctx.rootfs));
        LinuxPlatform {
            clipboard: None,
            container: Some(ctx.clone()),
            spawner: LinuxSpawner {
                container: Some(ctx),
                logins,
            },
        }
    }

    fn container_spawner(ctx: ContainerCtx) -> LinuxSpawner {
        let ctx = Arc::new(ctx);
        let logins = Arc::new(Logins::for_container(&ctx.rootfs));
        LinuxSpawner {
            container: Some(ctx),
            logins,
        }
    }

    fn vm_spawner() -> LinuxSpawner {
        LinuxSpawner {
            container: None,
            logins: Arc::new(Logins::for_vm()),
        }
    }

    #[test]
    fn container_platform_resolves_paths_into_rootfs() {
        use crate::mux::Platform as _;
        let p = container_platform(test_ctx("/rootfs", vec![]));
        assert_eq!(
            p.resolve_path("/var/log/app.log".into()),
            "/rootfs/var/log/app.log"
        );
        let host = new_platform();
        assert_eq!(host.resolve_path("/etc/passwd".into()), "/etc/passwd");
    }

    /// Container exec still goes through the nsexec trampoline — now behind
    /// the seam rather than as a `Platform` override.
    #[test]
    fn container_exec_wraps_argv_for_the_nsexec_trampoline() {
        let ctx = test_ctx("/rootfs", vec![]);
        let argv = nsexec_argv(&ctx, vec!["ls".into(), "-l".into()], None, None);
        assert_eq!(
            argv,
            vec![
                "/proc/self/exe",
                "--nsexec",
                "0",
                "/rootfs",
                "/",
                "-",
                "--",
                "ls",
                "-l"
            ]
        );
        // A host-supplied cwd wins over the container's workdir.
        let argv = nsexec_argv(&ctx, vec!["ls".into()], Some("/work".into()), None);
        assert_eq!(argv[4], "/work");
    }

    /// The trampoline crosses a process boundary, so who to become has to
    /// survive being written into an argv and read back out.
    #[test]
    fn the_trampoline_carries_the_sessions_ids_across_the_re_exec() {
        let ctx = test_ctx("/rootfs", vec![]);
        let credentials = Credentials {
            uid: 1000,
            gid: 1000,
            groups: vec![1000, 990],
        };
        let argv = nsexec_argv(&ctx, vec!["id".into()], None, Some(&credentials));
        assert_eq!(argv[5], "1000:1000:1000,990");
        assert_eq!(argv[6], "--");
        assert_eq!(parse_credentials(&argv[5]), Some(credentials));
        assert_eq!(parse_credentials("-"), None, "the floor becomes nobody");
        assert_eq!(
            parse_credentials("0:0:"),
            Some(Credentials {
                uid: 0,
                gid: 0,
                groups: vec![]
            })
        );
    }

    // ---- who a session runs as (PRD §19.2) --------------------------------

    fn declared(user: &str) -> Identity {
        Identity::Declared(vmlab_agent_proto::Logon {
            user: user.to_string(),
            secret: String::new(),
            elevated: false,
        })
    }

    /// §19.2: a declared account this guest does not have fails by name,
    /// rather than quietly running as root and leaving root-owned files in
    /// the developer's tree.
    #[test]
    fn a_declared_account_the_guest_does_not_have_fails_by_name() {
        let spawner = vm_spawner();
        let Err(err) = spawner.exec(
            &declared("nobody-called-this"),
            ProcessSpec {
                argv: vec!["true".into()],
                env: vec![],
                cwd: None,
            },
        ) else {
            panic!("an account this guest does not have must never spawn anything");
        };
        assert!(
            err.to_string().contains("nobody-called-this"),
            "{err}, which must name the account"
        );
    }

    /// The whole of §19.2's Linux list, run for real: the build host's own
    /// account, through the `setuid` route (the PAM route would need root to
    /// become anyone else, and this is the half that is testable without it).
    #[test]
    fn a_setuid_session_gets_the_login_environment_a_login_would() {
        use std::io::Read as _;
        let me = nix::unistd::User::from_uid(nix::unistd::Uid::effective())
            .unwrap()
            .unwrap();
        let session = Session {
            account: Account {
                name: me.name.clone(),
                uid: me.uid.as_raw(),
                gid: me.gid.as_raw(),
                groups: vec![me.gid.as_raw()],
                home: me.dir.to_string_lossy().into_owned(),
                shell: "/bin/sh".to_string(),
            },
            mechanism: Mechanism::Setuid,
            declared: true,
            runtime_dir: Some("/run/user/vmlab-test".to_string()),
        };
        // Something this process is holding that a login must not inherit —
        // the failure `env_clear` exists to prevent. Cargo sets it for the
        // test binary; nothing about a login would.
        assert!(
            std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
            "this test needs a variable of its own to watch not being inherited"
        );
        let spawner = vm_spawner();
        let plan = spawner
            .exec_plan(
                &spawner.route_for(Some(&session)),
                ProcessSpec {
                    argv: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "echo \"$USER|$LOGNAME|$HOME|$SHELL|$XDG_RUNTIME_DIR|$EXTRA|\
                         $(pwd)|$CARGO_MANIFEST_DIR\""
                            .into(),
                    ],
                    env: vec![("EXTRA".into(), "from the host".into())],
                    cwd: None,
                },
            )
            .unwrap();
        let mut spawned = spawn_piped(plan).unwrap();
        let mut out = String::new();
        spawned.output.read_to_string(&mut out).unwrap();
        assert_eq!((spawned.wait)(), 0);
        assert_eq!(
            out.trim(),
            format!(
                "{}|{}|{}|/bin/sh|/run/user/vmlab-test|from the host|{}|",
                me.name,
                me.name,
                me.dir.display(),
                me.dir.display()
            ),
            "a session must be indistinguishable from having logged in"
        );
    }

    /// Exec takes the same two routes the terminal does: `su -l … -c` runs
    /// the caller's argv as the login's own script, and the fallback
    /// assembles the login by hand.
    #[test]
    fn exec_takes_the_same_two_routes_the_terminal_does() {
        let spawner = vm_spawner();
        let asked_for = || ProcessSpec {
            argv: vec!["cargo".into(), "build".into()],
            env: vec![("RUSTFLAGS".into(), "-C debuginfo=2".into())],
            cwd: Some("/src".into()),
        };

        let pam = test_session(
            "dev",
            1000,
            Mechanism::Pam {
                su: "/bin/su".into(),
            },
            true,
        );
        let plan = spawner
            .exec_plan(&spawner.route_for(Some(&pam)), asked_for())
            .unwrap();
        assert_eq!(
            plan.spec.argv,
            vec![
                "/bin/su".to_string(),
                "-l".to_string(),
                "dev".to_string(),
                "-c".to_string(),
                "export RUSTFLAGS='-C debuginfo=2'; cd '/src' || exit 1; exec 'cargo' 'build'"
                    .to_string(),
            ],
            "`su -l` resets the environment and the cwd, so both go in the script"
        );
        assert_eq!(plan.credentials, None, "su drops privileges, not the agent");

        let setuid = test_session("dev", 1000, Mechanism::Setuid, true);
        let plan = spawner
            .exec_plan(&spawner.route_for(Some(&setuid)), asked_for())
            .unwrap();
        assert_eq!(plan.spec.argv, vec!["cargo", "build"]);
        assert_eq!(plan.spec.cwd.as_deref(), Some("/src"));
        assert!(plan.fresh_env, "a login does not inherit the agent's env");
        assert!(
            plan.spec
                .env
                .contains(&("RUSTFLAGS".into(), "-C debuginfo=2".into()))
        );
        assert!(plan.spec.env.contains(&("HOME".into(), "/home/dev".into())));
        assert_eq!(plan.credentials.map(|c| c.uid), Some(1000));

        // With no cwd asked for, a session starts where a login would.
        let plan = spawner
            .exec_plan(
                &spawner.route_for(Some(&setuid)),
                ProcessSpec {
                    argv: vec!["pwd".into()],
                    env: vec![],
                    cwd: None,
                },
            )
            .unwrap();
        assert_eq!(plan.spec.cwd.as_deref(), Some("/home/dev"));
    }

    /// §19.2's container floor: with no `login {}`, a session lands as the
    /// user cinit resolved — the declared `user`, else the image's `USER`.
    #[test]
    fn a_container_with_no_login_lands_as_cinits_user() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().to_str().unwrap().to_string();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(
            dir.path().join("etc/passwd"),
            "app:x:101:102::/app:/bin/sh\n",
        )
        .unwrap();
        touch_exe(&format!("{rootfs}/bin/sh"));

        let logins = Logins::for_container(&rootfs);
        let mut ctx = test_ctx(&rootfs, vec![]);
        ctx.floor = logins.floor("app");
        let spawner = container_spawner(ctx);

        let plan = spawner.shell_plan(&spawner.route_for(None), None).unwrap();
        assert_eq!(
            plan.credentials,
            Some(Credentials {
                uid: 101,
                gid: 102,
                groups: vec![102]
            })
        );
        assert!(
            plan.motd.contains("shell as `app` (uid 101)"),
            "{}",
            plan.motd
        );

        // …and an exec lands there too, carried across the trampoline's
        // re-exec because the ids only apply on the far side of the chroot.
        let plan = spawner
            .exec_plan(
                &spawner.route_for(None),
                ProcessSpec {
                    argv: vec!["id".into()],
                    env: vec![],
                    cwd: None,
                },
            )
            .unwrap();
        assert_eq!(plan.spec.argv[5], "101:102:102");
        assert!(plan.spec.env.contains(&("USER".into(), "app".into())));
    }

    /// A `login {}` on a container is a person saying "attach me as `dev`",
    /// so the session is `dev`'s in full — `HOME` and cwd included. cinit put
    /// the *workload* user's `HOME` in the container env, and a session that
    /// reports `USER=dev` with `HOME=/app` is the half-login §19.2 forbids.
    /// The floor is the workload, so it keeps the workload's answers.
    #[test]
    fn a_declared_login_on_a_container_gets_its_own_home_and_the_floor_keeps_the_workloads() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().to_str().unwrap().to_string();
        std::fs::create_dir_all(dir.path().join("home/dev")).unwrap();
        touch_exe(&format!("{rootfs}/bin/sh"));
        let mut ctx = test_ctx(&rootfs, vec![("HOME".into(), "/app".into())]);
        ctx.workdir = "/app".to_string();
        ctx.floor = Some(test_session("app", 101, Mechanism::Setuid, false));
        let spawner = container_spawner(ctx);

        let declared = test_session("dev", 1000, Mechanism::Setuid, true);
        let plan = spawner
            .shell_plan(&spawner.route_for(Some(&declared)), None)
            .unwrap();
        assert!(plan.env.contains(&("HOME".into(), "/home/dev".into())));
        assert_eq!(plan.cwd, "/home/dev");

        // The floor keeps the container's own `HOME` and working directory:
        // it *is* the workload, not a person attaching as someone else.
        let plan = spawner.shell_plan(&spawner.route_for(None), None).unwrap();
        assert!(plan.env.contains(&("HOME".into(), "/app".into())));
        assert_eq!(plan.cwd, "/app");
    }

    /// And a container that names no user is still root — the last rung of
    /// the same floor.
    #[test]
    fn a_container_with_no_user_at_all_is_still_root() {
        let spawner = container_spawner(test_ctx("/rootfs", vec![]));
        assert!(owner(&spawner.route_for(None)).is_none());
    }

    /// The PAM route hands `su` the login and gets out of the way: no ids to
    /// drop (it would then be unable to), and the caller's argv arrives as
    /// the script `su -l` runs.
    #[test]
    fn the_pam_route_delegates_the_whole_login_to_su() {
        let spawner = vm_spawner();
        let session = test_session(
            "dev",
            1000,
            Mechanism::Pam {
                su: "/bin/su".into(),
            },
            true,
        );
        let plan = spawner
            .shell_plan(
                &spawner.route_for(Some(&session)),
                Some(vec!["htop".into()]),
            )
            .unwrap();
        assert_eq!(
            plan.argv,
            Some(vec![
                "/bin/su".to_string(),
                "-l".to_string(),
                "dev".to_string(),
                "-c".to_string(),
                "exec 'htop'".to_string(),
            ])
        );
        assert_eq!(plan.credentials, None);
        assert_eq!(plan.cwd, "/home/dev");

        // With no command, `su -l` picks the account's own login shell.
        let plan = spawner
            .shell_plan(&spawner.route_for(Some(&session)), None)
            .unwrap();
        assert_eq!(
            plan.argv,
            Some(vec!["/bin/su".into(), "-l".into(), "dev".into()])
        );
    }

    /// The fallback assembles by hand what PAM would have done: the login
    /// shell invoked as one, the environment, and the ids.
    #[test]
    fn the_setuid_route_starts_the_accounts_own_login_shell() {
        let spawner = vm_spawner();
        let session = test_session("dev", 1000, Mechanism::Setuid, true);
        let plan = spawner
            .shell_plan(&spawner.route_for(Some(&session)), None)
            .unwrap();
        assert_eq!(plan.argv, Some(vec!["/bin/bash".into(), "-l".into()]));
        assert_eq!(
            plan.credentials,
            Some(Credentials {
                uid: 1000,
                gid: 1000,
                groups: vec![1000]
            })
        );
        assert_eq!(plan.cwd, "/home/dev");
        assert!(plan.env.contains(&("HOME".into(), "/home/dev".into())));
        assert!(plan.env.contains(&("TERM".into(), "xterm-256color".into())));
    }

    /// The agent identity is unchanged by all of this: root, in /root, with
    /// no ids to drop and no login machinery involved.
    #[test]
    fn the_agent_identity_is_still_a_plain_root_shell() {
        let spawner = vm_spawner();
        let plan = spawner.shell_plan(&spawner.route_for(None), None).unwrap();
        assert_eq!(plan.credentials, None);
        assert_eq!(plan.cwd, "/root");
        assert!(plan.env.contains(&("USER".into(), "root".into())));
    }
}
