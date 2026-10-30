//! Shared apps (PRD §20): one guest program's windows on the host desktop,
//! carried by the guest family's own window protocol (ADR-0016).
//!
//! The host end runs here, in the CLI process, because only a process started
//! in the user's desktop session can reach the compositor or the X server. The
//! lab daemon opens the agent session and hands the CLI two unix sockets
//! (`machine.app_open`): `control`, carrying the session's records, and
//! `conns`, where every connection becomes one transport connection of the
//! protocol. This module picks the protocol ([`select_mode`]) and drives a
//! session ([`run`]); [`waypipe`] and [`rdp`] are the two host ends that are
//! client programs, and the X11 end is a cookie-checking proxy.

pub mod rdp;
pub mod waypipe;
pub mod x11;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use vmlab_agent_proto::app::{AppRecord, RecordDecoder};

use crate::proto::client::LabClient;
use crate::proto::{CommandError, ErrorCode, LabRequest};

/// Why a macOS guest shares no app (PRD §20.1): the three protocols are
/// waypipe and X11 from a Linux guest and RDP RemoteApp from a Windows one,
/// and macOS serves none of them.
pub const MACOS_UNSHARED: &str = "shared apps are not supported on macOS guests — vmlab carries \
     waypipe and X11 from Linux guests and RDP RemoteApp from Windows ones, and macOS serves \
     none of them; use `vmlab console` for its desktop";

/// The refusal for a macOS `machine`.
fn unshared(machine: &str) -> String {
    format!("`{machine}` is a macOS guest: {MACOS_UNSHARED}")
}

/// The refusal when the CLI has no desktop to put a window on (PRD §20.1).
pub const NO_GRAPHICAL_SESSION: &str =
    "no graphical session: neither WAYLAND_DISPLAY nor DISPLAY is set";

/// Which guest family a machine belongs to, as far as sharing an app goes:
/// Windows when its effective profile name starts with `windows`, macOS when
/// it starts with `macos`, else Linux (the same rule as `labd::guest_os`). A
/// container is Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestFamily {
    Linux,
    Windows,
    /// No window protocol vmlab carries runs on macOS, so it shares none.
    Macos,
}

impl GuestFamily {
    /// The family of a machine whose effective profile is `profile`.
    pub fn of_profile(profile: Option<&str>) -> Self {
        match profile {
            Some(p) if p.starts_with("windows") => Self::Windows,
            Some(p) if p.starts_with("macos") => Self::Macos,
            _ => Self::Linux,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Linux => "a Linux guest",
            Self::Windows => "a Windows guest",
            Self::Macos => "a macOS guest",
        }
    }
}

/// The window protocol a session carries (PRD §20.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    Waypipe,
    X11,
    Rdp,
}

impl Mode {
    /// The spelling `machine.app_open` and `--via` use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waypipe => "waypipe",
            Self::X11 => "x11",
            Self::Rdp => "rdp",
        }
    }

    /// The guest family that can serve this protocol.
    fn family(self) -> GuestFamily {
        match self {
            Self::Waypipe | Self::X11 => GuestFamily::Linux,
            Self::Rdp => GuestFamily::Windows,
        }
    }
}

/// What the CLI's own environment offers: the host session and which host
/// binaries it found. Read once by [`HostEnv::detect`], so the choice in
/// [`select_mode`] stays a pure function of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostEnv {
    /// `WAYLAND_DISPLAY` is set.
    pub wayland: bool,
    /// `DISPLAY` is set.
    pub x11: bool,
    /// The waypipe client, from host config or `PATH`.
    pub waypipe: Option<PathBuf>,
    /// The FreeRDP client, from host config or `PATH`.
    pub freerdp: Option<PathBuf>,
}

impl HostEnv {
    /// Read the host session from the environment and find the two client
    /// binaries (PRD §20.4): the host config's `waypipe` and `freerdp` when
    /// set, else `waypipe`, then `xfreerdp3` and `xfreerdp`, on `PATH`.
    pub fn detect() -> Self {
        let set = |var| std::env::var_os(var).is_some_and(|v| !v.is_empty());
        let cfg = crate::config::host::HostConfig::load_default().unwrap_or_default();
        Self {
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            waypipe: cfg.waypipe.or_else(|| on_path("waypipe")),
            freerdp: cfg
                .freerdp
                .or_else(|| on_path("xfreerdp3"))
                .or_else(|| on_path("xfreerdp")),
        }
    }
}

/// `bin`'s full path when it is an executable file on `PATH`.
fn on_path(bin: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

/// The protocol [`select_mode`] chose, and the notice to print on stderr when
/// it is a fallback rather than the first choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub mode: Mode,
    pub notice: Option<String>,
}

/// Pick the protocol for one `app run` (PRD §20.1), or the refusal to print.
///
/// Windows is always `rdp`. A Linux guest on a Wayland host is `waypipe`,
/// falling back to `x11` through Xwayland when the host has no waypipe and
/// `DISPLAY` is set; on an X11 host it is `x11`. `--via` overrides the choice
/// and never falls back, and one the guest family cannot serve is refused by
/// name.
pub fn select_mode(
    machine: &str,
    family: GuestFamily,
    via: Option<Mode>,
    host: &HostEnv,
) -> Result<Selected, String> {
    // Before anything about the host: no host could share from it.
    if family == GuestFamily::Macos {
        return Err(unshared(machine));
    }
    if !host.wayland && !host.x11 {
        return Err(NO_GRAPHICAL_SESSION.into());
    }
    if let Some(mode) = via
        && mode.family() != family
    {
        return Err(format!(
            "`--via {}` shares apps from {}s; `{machine}` is {}",
            mode.as_str(),
            match mode.family() {
                GuestFamily::Linux => "Linux guest",
                GuestFamily::Windows => "Windows guest",
                GuestFamily::Macos => "macOS guest",
            },
            family.noun(),
        ));
    }
    let chosen = |mode| Ok(Selected { mode, notice: None });
    match (family, via) {
        (GuestFamily::Macos, _) => Err(unshared(machine)),
        (GuestFamily::Windows, _) => match host.freerdp {
            Some(_) => chosen(Mode::Rdp),
            None => Err(
                "no FreeRDP 3 on this host: shared apps from a Windows guest need \
                 `xfreerdp3` or `xfreerdp` on PATH, or `freerdp` in config.wcl"
                    .into(),
            ),
        },
        (GuestFamily::Linux, Some(Mode::Waypipe)) => {
            if !host.wayland {
                Err("`--via waypipe` needs a Wayland session: WAYLAND_DISPLAY is not set".into())
            } else if host.waypipe.is_none() {
                Err(NO_HOST_WAYPIPE.into())
            } else {
                chosen(Mode::Waypipe)
            }
        }
        (GuestFamily::Linux, Some(_)) => {
            if host.x11 {
                chosen(Mode::X11)
            } else {
                Err("`--via x11` needs an X server: DISPLAY is not set".into())
            }
        }
        (GuestFamily::Linux, None) => {
            if !host.wayland {
                chosen(Mode::X11)
            } else if host.waypipe.is_some() {
                chosen(Mode::Waypipe)
            } else if host.x11 {
                Ok(Selected {
                    mode: Mode::X11,
                    notice: Some(format!(
                        "{NO_HOST_WAYPIPE}; falling back to X11 through Xwayland"
                    )),
                })
            } else {
                Err(format!(
                    "{NO_HOST_WAYPIPE}, and DISPLAY is not set, so there is no X11 to fall \
                     back to"
                ))
            }
        }
    }
}

const NO_HOST_WAYPIPE: &str = "this host has no `waypipe` on PATH (or `waypipe` in config.wcl)";

/// Whether a refusal from the lab daemon falls back to X11, and the notice
/// that says so (PRD §20.1): the daemon answers `unsupported` when the guest
/// has no waypipe for the login, and the CLI falls back when it chose waypipe
/// itself and the host has `DISPLAY` set.
pub fn daemon_fallback(
    mode: Mode,
    via: Option<Mode>,
    host: &HostEnv,
    refusal: &CommandError,
) -> Option<String> {
    (mode == Mode::Waypipe && via.is_none() && host.x11 && refusal.code == ErrorCode::Unsupported)
        .then(|| format!("{}; falling back to X11 through Xwayland", refusal.message))
}

/// One `app run`, as the command line spelled it.
#[derive(Debug, Clone)]
pub struct Request {
    pub machine: String,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
}

/// Run one shared-app session to its end and return the exit code `app run`
/// exits with: the program's, FreeRDP's for `rdp`, or 130/143 when it was
/// interrupted.
///
/// `selected` is [`select_mode`]'s answer. A waypipe session the daemon
/// refuses because the guest has no waypipe is reopened as X11 when
/// [`daemon_fallback`] says so.
pub async fn run(
    client: &LabClient,
    req: &Request,
    selected: Mode,
    via: Option<Mode>,
    host: &HostEnv,
) -> Result<i32> {
    match attempt(client, req, selected, host).await? {
        Attempt::Ran(code) => Ok(code),
        Attempt::Refused(refusal) => match daemon_fallback(selected, via, host, &refusal) {
            Some(notice) => {
                eprintln!("{notice}");
                match attempt(client, req, Mode::X11, host).await? {
                    Attempt::Ran(code) => Ok(code),
                    Attempt::Refused(refusal) => Err(refusal.into()),
                }
            }
            None => Err(refusal.into()),
        },
    }
}

enum Attempt {
    /// The daemon refused to open the session; nothing ran.
    Refused(CommandError),
    /// The session ran and ended with this exit code.
    Ran(i32),
}

/// Set up the host end for `mode`, open the session, and drive it.
async fn attempt(client: &LabClient, req: &Request, mode: Mode, host: &HostEnv) -> Result<Attempt> {
    let mut end = match mode {
        Mode::Waypipe => {
            let bin = host
                .waypipe
                .as_deref()
                .ok_or_else(|| anyhow!(NO_HOST_WAYPIPE))?;
            HostEnd::Waypipe(waypipe::Client::start(bin).await?)
        }
        Mode::X11 => {
            let (cookie, adopt) = x11_end()?;
            HostEnd::X11 { cookie, adopt }
        }
        Mode::Rdp => HostEnd::Rdp(rdp::Server::bind()?),
    };
    let (argv, cwd) = match mode {
        // The RDP client names the program; the agent starts nothing.
        Mode::Rdp => (Vec::new(), None),
        _ => (req.argv.clone(), req.cwd.clone()),
    };
    let reply = client
        .send(LabRequest::MachineAppOpen {
            machine: req.machine.clone(),
            mode: mode.as_str().into(),
            argv,
            cwd,
            cookie: match &end {
                HostEnd::X11 { cookie, .. } => Some(cookie.clone()),
                _ => None,
            },
            user: req.user.clone(),
            password: req.password.clone(),
        })
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Ok(Attempt::Refused(CommandError::from(e))),
    };
    let path = |key: &str| {
        reply[key]
            .as_str()
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("machine.app_open answered without `{key}`"))
    };
    let (control, conns) = (path("control")?, path("conns")?);
    if let HostEnd::Rdp(server) = &mut end {
        let creds = rdp::Credentials::from_reply(&reply["rdp"])?;
        server.prepare(host.freerdp.as_deref(), req, &creds)?;
    }
    drive(&req.machine, &control, &conns, &mut end)
        .await
        .map(Attempt::Ran)
}

/// Adopt one guest-side connection: splice the `conns` stream it arrived on
/// to the host end.
type Adopt =
    Arc<dyn Fn(UnixStream) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

/// The host end of one session.
enum HostEnd {
    Waypipe(waypipe::Client),
    X11 { cookie: String, adopt: Adopt },
    Rdp(rdp::Server),
}

/// Something the host end did that the session reacts to.
enum HostEvent {
    /// The host-side client opened a transport connection (RDP).
    Accepted(UnixStream),
    /// The host-side client exited with this code.
    Exited(i32),
}

impl HostEnd {
    /// The next thing the host end does. Pends forever for an end with
    /// nothing to report, which is X11's.
    async fn event(&mut self) -> Result<HostEvent> {
        match self {
            Self::Waypipe(client) => Ok(HostEvent::Exited(client.wait().await?)),
            Self::X11 { .. } => std::future::pending().await,
            Self::Rdp(server) => server.event().await,
        }
    }
}

/// The X11 host end: a fresh session cookie, and the proxy that checks it on
/// every connection and swaps in the host display's own (T5's
/// `appshare::x11`). The one place this module touches X11.
fn x11_end() -> Result<(String, Adopt)> {
    let display = Arc::new(x11::HostDisplay::from_env()?);
    let cookie = x11::mint_cookie();
    let session_cookie = cookie.clone();
    let adopt: Adopt = Arc::new(move |guest: UnixStream| {
        let (display, cookie) = (display.clone(), session_cookie.clone());
        Box::pin(async move { x11::proxy(guest, &display, &cookie).await })
    });
    Ok((cookie, adopt))
}

/// Drive an opened session: relay its records and adopt its connections until
/// it ends, returning the exit code `app run` exits with.
async fn drive(machine: &str, control: &Path, conns: &Path, end: &mut HostEnd) -> Result<i32> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut control = UnixStream::connect(control)
        .await
        .with_context(|| format!("connecting to {}", control.display()))?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut decoder = RecordDecoder::new();
    let mut buf = vec![0u8; 16 * 1024];
    let adopt: Option<Adopt> = match end {
        HostEnd::Waypipe(client) => Some(client.adopter()),
        HostEnd::X11 { adopt, .. } => Some(adopt.clone()),
        HostEnd::Rdp(_) => None,
    };
    loop {
        tokio::select! {
            read = control.read(&mut buf) => {
                let n = read.context("reading the session's control socket")?;
                if n == 0 {
                    bail!("the lab daemon closed the session on `{machine}` without saying why");
                }
                decoder.push(&buf[..n]);
                while let Some(record) = decoder.next_record().map_err(|e| anyhow!(e))? {
                    match record {
                        AppRecord::Ready { .. } => {
                            if let HostEnd::Rdp(server) = end {
                                server.start()?;
                            }
                        }
                        AppRecord::Pending => match &adopt {
                            Some(adopt) => splice_pending(machine, conns, adopt.clone()),
                            None => bail!("the agent announced a connection an RDP session never makes"),
                        },
                        AppRecord::Log { text } => eprintln!("[{machine}] {text}"),
                        AppRecord::Exited { code } => return Ok(code),
                        AppRecord::Failed { msg } => bail!(msg),
                    }
                }
            }
            event = end.event() => match event? {
                HostEvent::Accepted(stream) => splice_accepted(machine, conns, stream),
                HostEvent::Exited(code) => return match end {
                    HostEnd::Rdp(_) => Ok(code),
                    _ => Err(anyhow!("the host's waypipe client exited with code {code}")),
                },
            },
            _ = tokio::signal::ctrl_c() => return Ok(130),
            _ = sigterm.recv() => return Ok(143),
        }
    }
}

/// A `pending` record: open the connection on `conns` that adopts the parked
/// guest connection, and hand it to the host end.
fn splice_pending(machine: &str, conns: &Path, adopt: Adopt) {
    let (machine, conns) = (machine.to_string(), conns.to_path_buf());
    tokio::spawn(async move {
        let result = async {
            let guest = UnixStream::connect(&conns).await?;
            adopt(guest).await
        }
        .await;
        if let Err(e) = result {
            eprintln!("[{machine}] a connection failed: {e:#}");
        }
    });
}

/// A connection the host-side client made: carry it to the guest through a
/// new connection on `conns`.
fn splice_accepted(machine: &str, conns: &Path, mut host: UnixStream) {
    let (machine, conns) = (machine.to_string(), conns.to_path_buf());
    tokio::spawn(async move {
        let result = async {
            let mut guest = UnixStream::connect(&conns).await?;
            tokio::io::copy_bidirectional(&mut host, &mut guest).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = result {
            eprintln!("[{machine}] a connection failed: {e:#}");
        }
    });
}

/// A private directory for one session's host-side sockets: mode 0700 under
/// `$XDG_RUNTIME_DIR` (the system temp dir without one), removed on drop.
fn private_dir() -> Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;

    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    tempfile::Builder::new()
        .prefix("vmlab-app-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&base)
        .with_context(|| format!("making a private directory in {}", base.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(wayland: bool, x11: bool, waypipe: bool, freerdp: bool) -> HostEnv {
        HostEnv {
            wayland,
            x11,
            waypipe: waypipe.then(|| PathBuf::from("/usr/bin/waypipe")),
            freerdp: freerdp.then(|| PathBuf::from("/usr/bin/xfreerdp3")),
        }
    }

    fn pick(family: GuestFamily, via: Option<Mode>, host: &HostEnv) -> Result<Selected, String> {
        select_mode("web", family, via, host)
    }

    fn mode(mode: Mode) -> Result<Selected, String> {
        Ok(Selected { mode, notice: None })
    }

    /// PRD §20.1's table, row by row: a Linux guest on Wayland rides waypipe,
    /// on X11 it rides X11, and a Windows guest rides RDP on either.
    #[test]
    fn every_row_of_the_protocol_table_picks_its_protocol() {
        use GuestFamily::*;
        assert_eq!(
            pick(Linux, None, &host(true, true, true, true)),
            mode(Mode::Waypipe)
        );
        assert_eq!(
            pick(Linux, None, &host(true, false, true, false)),
            mode(Mode::Waypipe)
        );
        assert_eq!(
            pick(Linux, None, &host(false, true, true, true)),
            mode(Mode::X11)
        );
        assert_eq!(
            pick(Linux, None, &host(false, true, false, false)),
            mode(Mode::X11)
        );
        assert_eq!(
            pick(Windows, None, &host(true, true, true, true)),
            mode(Mode::Rdp)
        );
        assert_eq!(
            pick(Windows, None, &host(false, true, false, true)),
            mode(Mode::Rdp)
        );
        assert_eq!(
            pick(Windows, None, &host(true, false, false, true)),
            mode(Mode::Rdp)
        );
    }

    /// A host with no waypipe falls back to X11 through Xwayland, and says
    /// so; with no `DISPLAY` either it refuses and names the missing waypipe.
    #[test]
    fn a_host_without_waypipe_falls_back_to_x11_or_refuses() {
        let fell_back = pick(GuestFamily::Linux, None, &host(true, true, false, false)).unwrap();
        assert_eq!(fell_back.mode, Mode::X11);
        assert_eq!(
            fell_back.notice.as_deref(),
            Some(
                "this host has no `waypipe` on PATH (or `waypipe` in config.wcl); falling \
                 back to X11 through Xwayland"
            )
        );
        let refused = pick(GuestFamily::Linux, None, &host(true, false, false, false)).unwrap_err();
        assert!(refused.contains("no `waypipe`"), "{refused}");
        assert!(refused.contains("no X11 to fall back to"), "{refused}");
    }

    /// With neither `WAYLAND_DISPLAY` nor `DISPLAY` there is no desktop, and
    /// no `--via` changes that.
    #[test]
    fn no_graphical_session_is_refused_in_the_prds_words() {
        for family in [GuestFamily::Linux, GuestFamily::Windows] {
            for via in [None, Some(Mode::Waypipe), Some(Mode::X11), Some(Mode::Rdp)] {
                assert_eq!(
                    pick(family, via, &host(false, false, true, true)),
                    Err("no graphical session: neither WAYLAND_DISPLAY nor DISPLAY is set".into())
                );
            }
        }
    }

    /// `--via` overrides the choice where the family can serve it, and never
    /// falls back: a forced waypipe with no host waypipe is refused.
    #[test]
    fn via_overrides_the_choice_without_falling_back() {
        use GuestFamily::*;
        assert_eq!(
            pick(Linux, Some(Mode::X11), &host(true, true, true, false)),
            mode(Mode::X11)
        );
        assert_eq!(
            pick(Linux, Some(Mode::Waypipe), &host(true, true, true, false)),
            mode(Mode::Waypipe)
        );
        assert_eq!(
            pick(Windows, Some(Mode::Rdp), &host(true, true, false, true)),
            mode(Mode::Rdp)
        );
        let forced = pick(Linux, Some(Mode::Waypipe), &host(true, true, false, false)).unwrap_err();
        assert!(forced.contains("no `waypipe`"), "{forced}");
        assert_eq!(
            pick(Linux, Some(Mode::Waypipe), &host(false, true, true, false)),
            Err("`--via waypipe` needs a Wayland session: WAYLAND_DISPLAY is not set".into())
        );
        assert_eq!(
            pick(Linux, Some(Mode::X11), &host(true, false, true, false)),
            Err("`--via x11` needs an X server: DISPLAY is not set".into())
        );
    }

    /// A `--via` the guest family cannot serve is refused by name (PRD §20.1).
    #[test]
    fn a_protocol_the_family_cannot_serve_is_refused_by_name() {
        let all = host(true, true, true, true);
        assert_eq!(
            pick(GuestFamily::Linux, Some(Mode::Rdp), &all),
            Err("`--via rdp` shares apps from Windows guests; `web` is a Linux guest".into())
        );
        assert_eq!(
            pick(GuestFamily::Windows, Some(Mode::Waypipe), &all),
            Err("`--via waypipe` shares apps from Linux guests; `web` is a Windows guest".into())
        );
        assert_eq!(
            pick(GuestFamily::Windows, Some(Mode::X11), &all),
            Err("`--via x11` shares apps from Linux guests; `web` is a Windows guest".into())
        );
    }

    /// A Windows guest with no FreeRDP on the host is refused, naming both
    /// binaries and the config key.
    #[test]
    fn a_windows_guest_needs_freerdp_on_the_host() {
        let refused = pick(GuestFamily::Windows, None, &host(true, true, true, false)).unwrap_err();
        assert!(refused.contains("`xfreerdp3` or `xfreerdp`"), "{refused}");
        assert!(refused.contains("`freerdp` in config.wcl"), "{refused}");
    }

    /// The daemon's `unsupported` for a guest without waypipe falls back to
    /// X11 only when the CLI chose waypipe itself and `DISPLAY` is set, and
    /// the notice carries the daemon's reason.
    #[test]
    fn a_guest_without_waypipe_falls_back_only_when_waypipe_was_not_forced() {
        let refusal = CommandError::new(
            ErrorCode::Unsupported,
            "`web` has no `waypipe` on PATH for `dev`",
        );
        let both = host(true, true, true, false);
        assert_eq!(
            daemon_fallback(Mode::Waypipe, None, &both, &refusal).as_deref(),
            Some("`web` has no `waypipe` on PATH for `dev`; falling back to X11 through Xwayland")
        );
        assert_eq!(
            daemon_fallback(Mode::Waypipe, Some(Mode::Waypipe), &both, &refusal),
            None
        );
        assert_eq!(
            daemon_fallback(
                Mode::Waypipe,
                None,
                &host(true, false, true, false),
                &refusal
            ),
            None
        );
        let other = CommandError::new(ErrorCode::NotFound, "no machine `web`");
        assert_eq!(daemon_fallback(Mode::Waypipe, None, &both, &other), None);
    }

    /// The guest family follows the profile name, the way `labd::guest_os`
    /// reads it.
    #[test]
    fn the_guest_family_follows_the_profile_name() {
        assert_eq!(
            GuestFamily::of_profile(Some("windows-11")),
            GuestFamily::Windows
        );
        assert_eq!(
            GuestFamily::of_profile(Some("windows-legacy")),
            GuestFamily::Windows
        );
        assert_eq!(GuestFamily::of_profile(Some("alpine")), GuestFamily::Linux);
        assert_eq!(GuestFamily::of_profile(None), GuestFamily::Linux);
    }

    /// A stand-in lab daemon: a `control` socket that sends `records`, and a
    /// `conns` socket that accepts one connection and says `conn` on it.
    async fn fake_daemon(records: Vec<AppRecord>) -> (tempfile::TempDir, PathBuf, PathBuf) {
        use tokio::io::AsyncWriteExt;
        use tokio::net::UnixListener;
        use vmlab_agent_proto::app::encode_record;

        let dir = tempfile::tempdir().unwrap();
        let (control, conns) = (dir.path().join("control"), dir.path().join("conns"));
        let control_l = UnixListener::bind(&control).unwrap();
        let conns_l = UnixListener::bind(&conns).unwrap();
        tokio::spawn(async move {
            let (mut c, _) = control_l.accept().await.unwrap();
            for r in records {
                c.write_all(&encode_record(&r)).await.unwrap();
            }
            // Hold the socket open until the CLI hangs up.
            let _ = c.read(&mut [0u8; 1]).await;
        });
        tokio::spawn(async move {
            if let Ok((mut s, _)) = conns_l.accept().await {
                s.write_all(b"conn").await.unwrap();
            }
        });
        (dir, control, conns)
    }

    /// The session driver adopts each `pending` connection through `conns`,
    /// and `exited` ends the session with the program's code.
    #[tokio::test]
    async fn a_pending_connection_is_adopted_and_exited_ends_the_session() {
        let (_dir, control, conns) = fake_daemon(vec![
            AppRecord::Ready { display: None },
            AppRecord::Pending,
            AppRecord::Log {
                text: "hello".into(),
            },
        ])
        .await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
        let adopt: Adopt = Arc::new(move |mut guest: UnixStream| {
            let tx = tx.clone();
            Box::pin(async move {
                let mut got = [0u8; 4];
                guest.read_exact(&mut got).await?;
                let _ = tx.lock().unwrap().take().unwrap().send(got);
                Ok(())
            })
        });
        let mut end = HostEnd::X11 {
            cookie: String::new(),
            adopt,
        };
        {
            let driving = drive("web", &control, &conns, &mut end);
            tokio::pin!(driving);
            // The fake daemon never sends `exited`, so the adoption is what
            // finishes first.
            let got = tokio::select! {
                got = rx => got.unwrap(),
                r = &mut driving => panic!("session ended early: {:?}", r.map_err(|e| e.to_string())),
            };
            assert_eq!(&got, b"conn");
        }

        let (_dir, control, conns) = fake_daemon(vec![AppRecord::Exited { code: 3 }]).await;
        assert_eq!(drive("web", &control, &conns, &mut end).await.unwrap(), 3);
    }

    /// FreeRDP starts on the agent's `ready`, which waits for the guest's RDP
    /// listener, and not on the daemon's reply: started sooner, its
    /// connection is refused and it gives up. Its exit code is the
    /// session's.
    #[tokio::test]
    async fn freerdp_starts_on_ready_and_its_exit_ends_the_session() {
        use std::os::unix::fs::PermissionsExt;

        let bin_dir = tempfile::tempdir().unwrap();
        let (bin, started) = (
            bin_dir.path().join("freerdp"),
            bin_dir.path().join("started"),
        );
        std::fs::write(
            &bin,
            format!("#!/bin/sh\ncat > '{}'\nexit 7\n", started.display()),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let req = Request {
            machine: "win".into(),
            argv: vec!["notepad.exe".into()],
            cwd: None,
            user: None,
            password: None,
        };
        let creds = rdp::Credentials {
            user: "dev".into(),
            domain: None,
            password: Some("pw".into()),
        };
        let rdp_end = || {
            let mut server = rdp::Server::bind().unwrap();
            server.prepare(Some(&bin), &req, &creds).unwrap();
            HostEnd::Rdp(server)
        };

        let (_dir, control, conns) = fake_daemon(vec![
            AppRecord::Log {
                text: "waiting for Remote Desktop to listen on 127.0.0.1:3389".into(),
            },
            AppRecord::Failed {
                msg: "no listener".into(),
            },
        ])
        .await;
        let mut end = rdp_end();
        assert!(drive("win", &control, &conns, &mut end).await.is_err());
        assert!(!started.exists(), "FreeRDP started before `ready`");

        let (_dir, control, conns) = fake_daemon(vec![AppRecord::Ready { display: None }]).await;
        let mut end = rdp_end();
        assert_eq!(drive("win", &control, &conns, &mut end).await.unwrap(), 7);
        let stdin = std::fs::read_to_string(&started).unwrap();
        assert!(stdin.contains("/app:program:notepad.exe\n"), "{stdin}");
    }

    /// `failed` ends the session with the daemon's message.
    #[tokio::test]
    async fn failed_ends_the_session_with_its_message() {
        let (_dir, control, conns) = fake_daemon(vec![AppRecord::Failed {
            msg: "the agent went away".into(),
        }])
        .await;
        let mut end = HostEnd::X11 {
            cookie: String::new(),
            adopt: Arc::new(|_| Box::pin(async { Ok(()) })),
        };
        let err = drive("web", &control, &conns, &mut end).await.unwrap_err();
        assert_eq!(err.to_string(), "the agent went away");
    }
}
