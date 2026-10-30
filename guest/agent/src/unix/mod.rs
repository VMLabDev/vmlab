//! The Unix platform half, Linux's and macOS's alike: the agent port, the
//! PTY terminal (a namespace-free descendant of `guest/cinit/src/tty.rs`),
//! piped exec, the logins behind both, network and OS facts, the clock and
//! the power state.
//!
//! What the two kernels answer differently lives in one submodule each,
//! re-exported as `os`: where the agent port is, how metrics are sampled,
//! how the guest is brought down, which clipboard is reachable, and which
//! features exist at all. [`linux`] reads `/proc` and `/sys` and adds what
//! only Linux serves — the workspace watch, process trees, shared apps and
//! container micro-VMs ([`container`]); [`macos`] asks the Mach host and
//! `sysctl`, and serves none of those four (PRD §7.4).
//!
//! Who a session runs as lives next door in [`login`]: PRD §19.2's declared
//! logins, and the container floor — the user cinit resolved, which every
//! session inherits when no `login {}` is declared.

pub mod login;

#[cfg(target_os = "linux")]
pub mod container;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub mod shepherd;
#[cfg(target_os = "linux")]
use linux as os;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

pub use os::{cpu_sample, disk_sample, mem_sample};

/// Cumulative (busy, total) CPU time, in whatever unit this kernel counts
/// it in: two samples apart give the utilisation between them.
pub type CpuSample = (u64, u64);

/// The locale every session gets: every glibc and musl guest has it, and
/// macOS's own `/etc/zprofile` sets it for a login that has none.
const LANG: &str = "C.UTF-8";

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, chdir, execve, fork, setsid};
use std::os::unix::process::CommandExt;

use vmlab_agent_proto::{NetInterface, OsInfo, PORT_NAME, ShutdownMode, features};

#[cfg(target_os = "linux")]
use crate::app::AppHome;
use crate::logon::Held;
use crate::mux::Mux;
use crate::spawn::{
    Adopter, Identity, ProcessSpec, Spawned, Spawner, TerminalSpec, adopt_as_agent,
    hold_until_it_exits, piped_command,
};

#[cfg(target_os = "linux")]
use container::ContainerCtx;
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
    let mut motd = String::from(BANNER);
    let session = match route {
        #[cfg(target_os = "linux")]
        Route::Container(_, session) => {
            motd.push_str("vmlab container terminal\n");
            motd.push_str(&format!(
                "  {} inside the container namespaces.\n",
                whose_shell(*session)
            ));
            motd.push_str(
                "  The image filesystem, environment, volumes, and processes are available here.\n",
            );
            motd.push_str(
                "  Run 'busybox --list' to see tools; 'exit' ends this session; Ctrl-] detaches.\n\n",
            );
            return motd;
        }
        Route::Agent => None,
        Route::Pam(s, _) | Route::Setuid(s) => Some(*s),
    };
    motd.push_str(&format!(
        "vmlab terminal - {} over virtio-serial (works with no network).\n",
        whose_shell(session)
    ));
    motd.push_str("  'exit' ends this session; Ctrl-] detaches the CLI client.\n\n");
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

static PORT_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// `--port <device>`: serve on this serial device instead of the virtio port.
pub fn set_port_override(path: PathBuf) {
    let _ = PORT_OVERRIDE.set(path);
}

/// The device the agent channel rides, as the platform found it.
struct Port {
    path: PathBuf,
    /// Whether it is a terminal device whose line discipline has to come off
    /// before a frame survives it: a UART, and every port on macOS, where
    /// the virtio console is a tty too. A Linux virtio port is a plain
    /// character device and has no termios to set.
    tty: bool,
    /// Said once the port is found, when where it was found is news.
    note: Option<String>,
}

/// An explicit `--port`, else wherever this kernel puts the agent port.
fn find_port() -> Option<Port> {
    match PORT_OVERRIDE.get() {
        Some(path) => Some(Port {
            path: path.clone(),
            tty: true,
            note: Some(format!("serving on {}", path.display())),
        }),
        None => os::find_port(),
    }
}

/// Put a serial device in raw mode at 115200 8N1, no flow control — the
/// line discipline would otherwise cook the frames.
fn configure_serial(port: &File) -> std::io::Result<()> {
    use nix::sys::termios::{
        BaudRate, ControlFlags, SetArg, cfmakeraw, cfsetspeed, tcgetattr, tcsetattr,
    };
    let mut t = tcgetattr(port)?;
    cfmakeraw(&mut t);
    cfsetspeed(&mut t, BaudRate::B115200)?;
    t.control_flags |= ControlFlags::CLOCAL | ControlFlags::CREAD;
    t.control_flags &= !ControlFlags::CRTSCTS;
    tcsetattr(port, SetArg::TCSANOW, &t)?;
    Ok(())
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
        let Some(Port { path, tty, note }) = find_port() else {
            eprintln!("vmlab-agent: waiting for port {PORT_NAME}");
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        if let Some(note) = note {
            eprintln!("vmlab-agent: {note}");
        }
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(port) => match port.try_clone() {
                Ok(w) => {
                    if tty && let Err(e) = configure_serial(&port) {
                        eprintln!("vmlab-agent: cannot configure {}: {e}", path.display());
                        thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    return (port, w);
                }
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

pub struct UnixPlatform {
    clipboard: Option<os::Clipboard>,
    /// Container micro-VM mode (spawned by cinit): sessions run inside the
    /// container instead of the init namespace.
    #[cfg(target_os = "linux")]
    container: Option<Arc<ContainerCtx>>,
    spawner: UnixSpawner,
    #[cfg(target_os = "linux")]
    app_paths: linux::AppPaths,
}

/// The Unix half of the process/handle seam: PTY terminals, piped exec, and
/// the file writes behind `push` — in the init namespace, or inside the
/// container when cinit started us in container mode. Each as the agent, or
/// as a declared login the channel carried (PRD §19.2).
pub struct UnixSpawner {
    #[cfg(target_os = "linux")]
    container: Option<Arc<ContainerCtx>>,
    logins: Arc<Logins>,
}

pub fn new_platform() -> UnixPlatform {
    let logins = Arc::new(Logins::for_vm());
    login::start_sweeper(logins.clone());
    UnixPlatform {
        clipboard: os::Clipboard::probe(),
        #[cfg(target_os = "linux")]
        container: None,
        spawner: UnixSpawner {
            #[cfg(target_os = "linux")]
            container: None,
            logins,
        },
        #[cfg(target_os = "linux")]
        app_paths: linux::AppPaths::default(),
    }
}

#[cfg(all(test, target_os = "linux"))]
impl UnixPlatform {
    /// Put shared-app sessions somewhere a test can write and look, rather
    /// than `/run` and the build host's own `/tmp/.X11-unix`.
    pub fn with_app_paths(mut self, run: &str, x11_unix: &str) -> UnixPlatform {
        self.app_paths = linux::AppPaths {
            run: run.to_string(),
            x11_unix: x11_unix.to_string(),
        };
        self
    }
}

/// What every Unix agent serves, whichever kernel it runs on.
const COMMON_FEATURES: &[&str] = &[
    features::TERMINAL,
    features::EXEC,
    features::FILEOPS,
    features::TAIL,
    features::METRICS,
    features::SET_TIME,
];

/// The answer a guest without a reachable clipboard gives — it never
/// advertises the feature, so only a host that asked anyway hears it.
const NO_DISPLAY: &str = "no display session is reachable from the agent";

impl crate::mux::Platform for UnixPlatform {
    fn os(&self) -> &'static str {
        os::NAME
    }

    fn features(&self) -> Vec<String> {
        let mut f: Vec<String> = COMMON_FEATURES
            .iter()
            .chain(os::FEATURES)
            .map(|f| f.to_string())
            .collect();
        if self.clipboard.is_some() {
            f.push(features::CLIPBOARD.to_string());
            f.push(features::CLIPBOARD_REPLY.to_string());
        }
        f
    }

    fn spawner(&self) -> &dyn Spawner {
        &self.spawner
    }

    fn path_resolver(&self) -> crate::mux::PathResolver {
        #[cfg(target_os = "linux")]
        if let Some(ctx) = &self.container {
            let rootfs = ctx.rootfs.clone();
            return Arc::new(move |path: String| {
                format!("{rootfs}/{}", path.trim_start_matches('/'))
            });
        }
        Arc::new(|path| path)
    }

    fn open_eventlog(&self, mux: &Mux, id: u32, _filter: Option<String>) {
        mux.send_error(Some(id), "event log tailing is Windows-only");
    }

    fn set_clipboard(&self, mux: &Mux, text: String) {
        let outcome = match &self.clipboard {
            Some(clipboard) => clipboard.set(&text),
            None => Err(NO_DISPLAY.to_string()),
        };
        crate::clipboard::answer_set(mux, outcome);
    }

    fn get_clipboard(&self, mux: &Mux) {
        let outcome = match &self.clipboard {
            Some(clipboard) => clipboard.get(),
            None => Err(NO_DISPLAY.to_string()),
        };
        crate::clipboard::answer_get(mux, outcome);
    }

    fn net_info(&self) -> Result<Vec<NetInterface>, String> {
        net_info()
    }

    fn os_info(&self) -> Result<OsInfo, String> {
        Ok(os::os_info())
    }

    #[cfg(target_os = "linux")]
    fn tree_status(&self, tree: &str) -> Result<u32, String> {
        shepherd::status(tree)
    }

    fn shutdown(&self, mux: &Mux, mode: ShutdownMode) {
        let mux = mux.clone();
        thread::spawn(move || {
            // Let the ShuttingDown ack drain to the host first.
            thread::sleep(Duration::from_millis(200));
            if let Err(e) = os::shutdown(mode) {
                mux.send_error(None, format!("shutdown: {e}"));
            }
        });
    }

    fn set_time(&self, unix_ms: i64) -> Result<(), String> {
        os::set_clock(unix_ms)
    }

    #[cfg(target_os = "linux")]
    fn app_home(&self, identity: &Identity, name: &str) -> Result<AppHome, String> {
        let held = self
            .spawner
            .logins
            .resolve(identity)
            .map_err(|e| e.to_string())?;
        let owner = owner(&self.spawner.route(&held)).map(|a| (a.uid, a.gid));
        let resolve = self.path_resolver();
        let seen = format!("{}/{name}", self.app_paths.run);
        let dir = PathBuf::from(resolve(seen.clone()));
        linux::make_app_dir(
            &PathBuf::from(resolve(self.app_paths.run.clone())),
            &dir,
            owner,
        )
        .map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        Ok(AppHome {
            dir,
            seen,
            x11_unix: PathBuf::from(resolve(self.app_paths.x11_unix.clone())),
            owner,
            hostname: uname().2,
        })
    }
}

/// How a channel's work gets to be who it is — decided once, then read by
/// both the terminal and the exec shapes so they cannot drift apart.
enum Route<'a> {
    /// Inside a container micro-VM, as whoever the session is: the declared
    /// login, else §19.2's container floor, else root.
    #[cfg(target_os = "linux")]
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

impl UnixSpawner {
    /// The route a channel's work takes: the declared login where the open
    /// carried one, else the container floor, else the agent's own identity
    /// (PRD §19.2).
    fn route<'a>(&'a self, held: &'a Option<Arc<Held<Session>>>) -> Route<'a> {
        self.route_for(held.as_deref().map(|h| &h.value))
    }

    /// The route for a login already resolved — `None` meaning "whatever this
    /// machine's floor is", which is the container's user or nothing at all.
    fn route_for<'a>(&'a self, declared: Option<&'a Session>) -> Route<'a> {
        #[cfg(target_os = "linux")]
        if let Some(ctx) = &self.container {
            return Route::Container(ctx, declared.or(ctx.floor.as_ref()));
        }
        match declared {
            None => Route::Agent,
            Some(
                s @ Session {
                    mechanism: Mechanism::Pam { su },
                    ..
                },
            ) => Route::Pam(s, su),
            Some(s) => Route::Setuid(s),
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
            #[cfg(target_os = "linux")]
            Route::Container(ctx, session) => {
                let mut env = container::container_env(ctx, *session);
                env.extend(spec.env);
                ExecPlan {
                    spec: ProcessSpec {
                        argv: container::nsexec_argv(
                            ctx,
                            spec.argv,
                            spec.cwd
                                .or_else(|| container::container_home(ctx, *session)),
                            session.and_then(login::credentials_for).as_ref(),
                        ),
                        env,
                        cwd: None,
                        tree: spec.tree,
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
                    tree: spec.tree,
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
                        tree: spec.tree,
                    },
                    credentials: login::credentials_for(s),
                    fresh_env: true,
                }
            }
        })
    }

    /// Everything the forked shell child needs, decided before the fork.
    ///
    /// `overrides` are applied *over* the environment the route already
    /// brings, never instead of it: a terminal open's `env` arrives this
    /// way, and everything vmlab opens on its own behalf sends none.
    fn shell_plan(
        &self,
        route: &Route,
        command: Option<Vec<String>>,
        overrides: Vec<(String, String)>,
    ) -> std::io::Result<ShellPlan> {
        let motd = terminal_motd(route);
        let plan = match route {
            // The injected BusyBox (so a distroless image still gets a
            // toolbox) as whoever the session is.
            #[cfg(target_os = "linux")]
            Route::Container(ctx, session) => {
                let mut env = container::container_env(ctx, *session);
                env.extend(overrides);
                ShellPlan {
                    argv: command.or_else(|| container::container_shell(&ctx.rootfs)),
                    env,
                    credentials: session.and_then(login::credentials_for),
                    cwd: container::container_home(ctx, *session)
                        .unwrap_or_else(|| ctx.workdir.clone()),
                    motd,
                }
            }
            Route::Agent => {
                let mut env = root_env();
                env.extend(overrides);
                ShellPlan {
                    argv: command.or_else(default_shell),
                    env,
                    credentials: None,
                    cwd: os::ROOT_HOME.to_string(),
                    motd,
                }
            }
            // `su -l` resets the environment as part of being a login, so
            // overrides are re-applied inside the script it runs — the same
            // place an exec's are. With neither a command nor an override
            // there is no script at all, which is what keeps an ordinary
            // attach a plain interactive `su -l`.
            Route::Pam(s, su) => {
                let inner = match (command, overrides.is_empty()) {
                    (Some(argv), _) => Some(argv),
                    (None, false) => Some(s.account.login_shell()),
                    (None, true) => None,
                };
                ShellPlan {
                    argv: Some(login::su_argv(
                        su,
                        &s.account.name,
                        inner.map(|argv| login::login_script(&overrides, None, &argv)),
                    )),
                    env: vec![
                        ("TERM".to_string(), "xterm-256color".to_string()),
                        ("PATH".to_string(), os::SUPATH.to_string()),
                    ],
                    credentials: None,
                    cwd: s.account.home.clone(),
                    motd,
                }
            }
            Route::Setuid(s) => {
                let mut env = s.env();
                env.push(("TERM".to_string(), "xterm-256color".to_string()));
                env.extend(overrides);
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
        #[cfg(target_os = "linux")]
        Route::Container(_, session) => session.map(|s| &s.account),
        Route::Agent => None,
        Route::Pam(s, _) | Route::Setuid(s) => Some(&s.account),
    }
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
    spawn_piped_with(plan, |_| {})
}

/// Carry out an [`ExecPlan`], with `extra` preparing the command last.
fn spawn_piped_with(plan: ExecPlan, extra: impl FnOnce(&mut Command)) -> std::io::Result<Spawned> {
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
        extra(cmd);
    })
}

/// The shell spawn, resolved: what to run, as whom, and where. `argv` is
/// `Option` only while it is being resolved; [`UnixSpawner::shell_plan`] is
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

impl Spawner for UnixSpawner {
    fn terminal(&self, identity: &Identity, spec: TerminalSpec) -> std::io::Result<Spawned> {
        let held = self.logins.resolve(identity)?;
        let route = self.route(&held);
        let plan = self.shell_plan(&route, spec.command, spec.env)?;
        let size = winsize(spec.cols, spec.rows);
        let (master, pid) = spawn_shell(&plan, &size, self.jail()?, owner(&route))?;
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
        let mut plan = self.exec_plan(&self.route(&held), spec)?;
        // The shepherd wraps the whole plan, `su` and the container
        // trampoline included, so whatever they start is in the tree too.
        let spawned = match plan.spec.tree.take() {
            #[cfg(target_os = "linux")]
            Some(tree) => shepherd::spawn(plan, &tree)?,
            // Only Linux advertises `features::TREE`: an orphan elsewhere is
            // reparented to an init this agent cannot count it under.
            #[cfg(not(target_os = "linux"))]
            Some(tree) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!("process tree {tree}: process trees need a Linux or Windows guest"),
                ));
            }
            None => spawn_piped(plan)?,
        };
        Ok(hold_until_it_exits(spawned, held))
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

/// The default interactive shell: the first of this kernel's usual shells
/// the guest has, as a login shell.
fn default_shell() -> Option<Vec<String>> {
    os::SHELLS
        .iter()
        .find(|sh| Path::new(sh).exists())
        .map(|sh| vec![sh.to_string(), "-l".to_string()])
}

/// The agent identity's own shell environment: root, on a VM.
fn root_env() -> Vec<(String, String)> {
    [
        ("TERM", "xterm-256color"),
        ("HOME", os::ROOT_HOME),
        ("USER", "root"),
        ("LOGNAME", "root"),
        ("SHELL", "/bin/sh"),
        ("PATH", os::SUPATH),
        ("LANG", LANG),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Where a terminal's child is confined before its shell starts: a container
/// micro-VM's namespaces and root filesystem (see [`container::Jail`]).
#[cfg(target_os = "linux")]
use container::Jail;

/// No other Unix runs container micro-VMs, so no other Unix has a jail to
/// put a shell in: uninhabited, so a terminal there can never be handed one.
#[cfg(not(target_os = "linux"))]
enum Jail {}

#[cfg(not(target_os = "linux"))]
impl Jail {
    unsafe fn enter(&self, _close: [libc::c_int; 2]) -> bool {
        match *self {}
    }

    unsafe fn chroot(&self) -> bool {
        match *self {}
    }
}

impl UnixSpawner {
    /// The jail a terminal's child enters, if this agent serves a container.
    #[cfg(target_os = "linux")]
    fn jail(&self) -> std::io::Result<Option<Jail<'_>>> {
        self.container
            .as_deref()
            .map(ContainerCtx::jail)
            .transpose()
    }

    #[cfg(not(target_os = "linux"))]
    fn jail(&self) -> std::io::Result<Option<Jail>> {
        Ok(None)
    }
}

/// Allocate a PTY sized `size` and fork the shell: the child becomes a
/// session leader on the slave, drops to the session's ids and execs. In
/// container mode the child first enters the container's [`Jail`]. Returns
/// the master (close-on-exec) and the child pid. Post-fork the child only
/// performs async-signal-safe operations before execve (allocation-free,
/// like cinit's spawn).
fn spawn_shell(
    plan: &ShellPlan,
    size: &Winsize,
    jail: Option<Jail>,
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
            // SAFETY: post-fork, as `enter` requires.
            if let Some(jail) = &jail
                && !unsafe { jail.enter([slave_raw, master_raw]) }
            {
                die("setns");
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
            // SAFETY: post-fork, as `chroot` requires.
            if let Some(jail) = &jail
                && !unsafe { jail.chroot() }
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

/// Utilisation between two cumulative (busy, total) samples, in percent.
pub fn cpu_pct(prev: &CpuSample, cur: &CpuSample) -> f32 {
    let busy = cur.0.saturating_sub(prev.0) as f32;
    let total = cur.1.saturating_sub(prev.1) as f32;
    if total <= 0.0 {
        0.0
    } else {
        (100.0 * busy / total).clamp(0.0, 100.0)
    }
}

/// Detach from the launching terminal/agent: double-fork + setsid + stdio to
/// /dev/null. Used for manual hand-launches during development; the
/// systemd service and the launchd daemon run in the foreground.
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

// The tests run on the build host, which is Linux, and cover the container
// routes beside the VM ones.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::container::{container_env, container_shell, nsexec_argv, parse_credentials};
    use super::linux::{AppPaths, os_info};
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

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

    /// The BusyBox shim is the last entry on PATH, never the first: the
    /// image's own `mount`, `ps` and the rest win, and the shim answers only
    /// for what a distroless image lacks (§18). Before this it led, and a
    /// `mount -o remount` in a `debian:12` container silently got BusyBox
    /// semantics instead of util-linux's.
    #[test]
    fn container_env_adds_term_and_puts_the_busybox_shim_last_on_path() {
        let ctx = test_ctx("/rootfs", vec![("FOO".into(), "bar".into())]);
        let env = container_env(&ctx, None);
        assert!(env.contains(&("TERM".into(), "xterm-256color".into())));
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .expect("a PATH");
        assert_eq!(
            path.split(':').next_back(),
            Some(vmlab_agent_proto::BUSYBOX_BIN_DIR),
            "{path}"
        );
        assert!(path.starts_with(os::SUPATH), "{path}");
        assert!(env.contains(&("FOO".into(), "bar".into())));

        // Explicit values win; PATH still gets the shim appended once.
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
            format!("/only:{}", vmlab_agent_proto::BUSYBOX_BIN_DIR)
        )));

        // Already there, wherever the image put it: left alone.
        let ctx = test_ctx(
            "/rootfs",
            vec![(
                "PATH".into(),
                format!("{}:/only", vmlab_agent_proto::BUSYBOX_BIN_DIR),
            )],
        );
        let env = container_env(&ctx, None);
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
    fn container_platform(ctx: ContainerCtx) -> UnixPlatform {
        let ctx = Arc::new(ctx);
        let logins = Arc::new(Logins::for_container(&ctx.rootfs));
        UnixPlatform {
            clipboard: None,
            container: Some(ctx.clone()),
            spawner: UnixSpawner {
                container: Some(ctx),
                logins,
            },
            app_paths: AppPaths::default(),
        }
    }

    fn container_spawner(ctx: ContainerCtx) -> UnixSpawner {
        let ctx = Arc::new(ctx);
        let logins = Arc::new(Logins::for_container(&ctx.rootfs));
        UnixSpawner {
            container: Some(ctx),
            logins,
        }
    }

    fn vm_spawner() -> UnixSpawner {
        UnixSpawner {
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

    /// A container's shared-app session lives inside its root filesystem,
    /// where the program can reach it, and the program is told the path as
    /// it sees it from in there (PRD §20.3).
    #[test]
    fn a_container_app_session_lives_inside_the_rootfs() {
        use crate::mux::Platform as _;
        let rootfs = tempfile::tempdir().unwrap();
        let p = container_platform(test_ctx(rootfs.path().to_str().unwrap(), vec![]));
        let home = p.app_home(&Identity::Agent, "1-7").unwrap();
        assert_eq!(home.seen, "/run/vmlab-app/1-7");
        assert_eq!(home.dir, rootfs.path().join("run/vmlab-app/1-7"));
        assert_eq!(home.x11_unix, rootfs.path().join("tmp/.X11-unix"));
        assert_eq!(
            fs::metadata(&home.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(home.owner, None, "the floor here is the agent itself");
        // A directory an earlier agent left at the name is replaced.
        fs::write(home.dir.join("stale"), b"").unwrap();
        let again = p.app_home(&Identity::Agent, "1-7").unwrap();
        assert!(!again.dir.join("stale").exists());
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
                tree: None,
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
                    tree: None,
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
            tree: None,
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
                    tree: None,
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

        let plan = spawner
            .shell_plan(&spawner.route_for(None), None, vec![])
            .unwrap();
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
                    tree: None,
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
            .shell_plan(&spawner.route_for(Some(&declared)), None, vec![])
            .unwrap();
        assert!(plan.env.contains(&("HOME".into(), "/home/dev".into())));
        assert_eq!(plan.cwd, "/home/dev");

        // The floor keeps the container's own `HOME` and working directory:
        // it *is* the workload, not a person attaching as someone else.
        let plan = spawner
            .shell_plan(&spawner.route_for(None), None, vec![])
            .unwrap();
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
                vec![],
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
            .shell_plan(&spawner.route_for(Some(&session)), None, vec![])
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
            .shell_plan(&spawner.route_for(Some(&session)), None, vec![])
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

    /// A terminal's `env` is applied *over* whatever the route already
    /// brings, and never instead of it: the account's own `HOME` survives a
    /// client that sent `LANG`.
    #[test]
    fn a_terminals_overrides_are_applied_over_the_routes_own_environment() {
        let spawner = vm_spawner();
        let session = test_session("dev", 1000, Mechanism::Setuid, true);
        let plan = spawner
            .shell_plan(
                &spawner.route_for(Some(&session)),
                None,
                vec![("LANG".into(), "en_GB.UTF-8".into())],
            )
            .unwrap();
        assert!(plan.env.contains(&("HOME".into(), "/home/dev".into())));
        assert!(plan.env.contains(&("LANG".into(), "en_GB.UTF-8".into())));
    }

    /// `su -l` resets the environment as part of being a login, so overrides
    /// ride the script it runs. With none, a shell stays a plain
    /// interactive `su -l` — the login shell is not replaced by a script
    /// just because overrides are possible.
    #[test]
    fn the_pam_route_re_applies_overrides_inside_the_login() {
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
                None,
                vec![("LANG".into(), "en_GB.UTF-8".into())],
            )
            .unwrap();
        assert_eq!(
            plan.argv,
            Some(vec![
                "/bin/su".to_string(),
                "-l".to_string(),
                "dev".to_string(),
                "-c".to_string(),
                "export LANG='en_GB.UTF-8'; exec '/bin/bash' '-l'".to_string(),
            ])
        );
    }

    /// The agent identity is unchanged by all of this: root, in /root, with
    /// no ids to drop and no login machinery involved.
    #[test]
    fn the_agent_identity_is_still_a_plain_root_shell() {
        let spawner = vm_spawner();
        let plan = spawner
            .shell_plan(&spawner.route_for(None), None, vec![])
            .unwrap();
        assert_eq!(plan.credentials, None);
        assert_eq!(plan.cwd, "/root");
        assert!(plan.env.contains(&("USER".into(), "root".into())));
    }
}
