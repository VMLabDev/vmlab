//! The lab daemon's half of a **shared app** (PRD §20.3, ADR-0016):
//! `machine.app_open`.
//!
//! The host end of every window protocol runs in the CLI, because only a
//! process in the user's desktop session can reach the compositor or the X
//! server, and the daemon is headless (§11). So the daemon only brokers: it
//! opens the agent's app session and hands the CLI two unix sockets in the
//! machine's run directory.
//!
//! - **control** (`app-<id>.sock`) serves one client and relays the app
//!   channel's records to it verbatim, then one [`AppRecord::Exited`] or
//!   [`AppRecord::Failed`] translated from the control channel, which the CLI
//!   never sees. The client hanging up closes the session, which kills the
//!   guest program.
//! - **conns** (`app-<id>-conns.sock`) turns every connection into one
//!   `open_app_conn` channel and splices the two with half-close.
//!
//! Both sockets are unlinked when the session ends, however it ends; a
//! machine stopping ends it, because the agent channel closes under it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use vmlab_agent_proto::app::{self, AppMode, AppRecord};
use vmlab_agent_proto::features;

use crate::labd::guest_os::GuestOs;
use crate::labd::one_shot::ACCEPT_GRACE;
use crate::labd::vm_agent::{AgentHandle, AgentSession, Logon, SessionEvent};
use crate::proto::CommandError;

/// How long the `waypipe` probe may take. It is one `command -v` in a shell.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How many record bytes the control relay holds for a client that has not
/// connected yet. Records are small and the client connects at once; a
/// backlog this size means nobody is coming, and the session fails rather
/// than buffering without bound.
const BACKLOG_LIMIT: usize = 4 * 1024 * 1024;

/// The machine a shared app is opened on, as far as the broker needs it.
pub struct Target {
    pub name: String,
    pub guest_os: GuestOs,
    /// Where the session's sockets go, beside the machine's terminal sockets.
    pub run_dir: PathBuf,
}

/// `machine.app_open`'s arguments, before any of them is checked.
pub struct AppRequest {
    pub mode: String,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub cookie: Option<String>,
}

/// The three window protocols, before an X11 cookie is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    Waypipe,
    X11,
    Rdp,
}

impl Via {
    fn parse(mode: &str) -> Option<Via> {
        match mode {
            "waypipe" => Some(Via::Waypipe),
            "x11" => Some(Via::X11),
            "rdp" => Some(Via::Rdp),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Via::Waypipe => "waypipe",
            Via::X11 => "x11",
            Via::Rdp => "rdp",
        }
    }

    /// The guest family whose own window protocol this is (§20.1).
    fn family(self) -> GuestOs {
        match self {
            Via::Waypipe | Via::X11 => GuestOs::Linux,
            Via::Rdp => GuestOs::Windows,
        }
    }

    fn feature(self) -> &'static str {
        match self {
            Via::Waypipe => features::APP_WAYPIPE,
            Via::X11 => features::APP_X11,
            Via::Rdp => features::APP_RDP,
        }
    }
}

fn family_name(os: GuestOs) -> &'static str {
    match os {
        GuestOs::Linux => "Linux",
        GuestOs::Windows => "Windows",
    }
}

/// Open a shared app on `target` and serve its two sockets.
///
/// `logon` resolves who the program runs as (§20.2). It is a closure so the
/// refusals keep §20.3's order: a mode the machine cannot serve is refused
/// before an identity it would never use is looked up.
pub async fn open(
    target: &Target,
    agent: &AgentHandle,
    req: AppRequest,
    logon: impl FnOnce() -> Result<Option<Logon>, CommandError>,
) -> Result<Value, CommandError> {
    let machine = target.name.as_str();
    let via = Via::parse(&req.mode).ok_or_else(|| {
        CommandError::invalid(format!(
            "unknown shared-app mode `{}`: expected `waypipe`, `x11` or `rdp`",
            req.mode
        ))
    })?;
    if via.family() != target.guest_os {
        return Err(CommandError::unsupported(format!(
            "`{}` shares apps from {} guests; `{machine}` is a {} guest",
            via.name(),
            family_name(via.family()),
            family_name(target.guest_os),
        )));
    }
    if !agent.has_feature(via.feature()) {
        return Err(CommandError::unsupported(format!(
            "the agent on `{machine}` predates shared apps over `{}`; run `vmlab machine \
             repair-agent {machine}`",
            via.name()
        )));
    }

    let logon = logon()?;
    if via == Via::Rdp && logon.is_none() {
        return Err(CommandError::invalid(format!(
            "RDP needs an account: declare a `login {{}}` on `{machine}` or pass \
             --user/--password"
        )));
    }

    let mode = match via {
        Via::Waypipe => AppMode::Waypipe,
        Via::Rdp => AppMode::Rdp,
        Via::X11 => match req.cookie {
            Some(cookie) if app::valid_cookie(&cookie) => AppMode::X11 { cookie },
            Some(_) => {
                return Err(CommandError::invalid(
                    "an `x11` cookie is 32 lowercase hex digits",
                ));
            }
            None => {
                return Err(CommandError::invalid(
                    "`x11` needs the cookie the caller minted for this session",
                ));
            }
        },
    };
    match (via, req.argv.is_empty()) {
        (Via::Rdp, false) => {
            return Err(CommandError::invalid(
                "`rdp` takes no argv: the RDP client names the program",
            ));
        }
        (Via::Waypipe | Via::X11, true) => {
            return Err(CommandError::invalid(format!(
                "`{}` needs a program to run",
                via.name()
            )));
        }
        _ => {}
    }

    if via == Via::Waypipe {
        probe_waypipe(machine, agent, logon.as_ref()).await?;
    }

    let rdp = logon
        .as_ref()
        .filter(|_| via == Via::Rdp)
        .map(|l| json!({"user": l.user, "password": l.secret}));
    let session = agent
        .open_app(mode, req.argv, Vec::new(), req.cwd, logon)
        .await
        .map_err(|e| CommandError::from(e).prefixed(machine))?;
    let id = session.id;
    let control = target.run_dir.join(format!("app-{id}.sock"));
    let conns = target.run_dir.join(format!("app-{id}-conns.sock"));
    serve(session, agent.clone(), control.clone(), conns.clone())?;

    let mut reply = json!({"session": id, "control": control, "conns": conns});
    if let Some(rdp) = rdp {
        reply["rdp"] = rdp;
    }
    Ok(reply)
}

/// Refuse a waypipe session the guest cannot run, as `unsupported` — the one
/// code the CLI turns into its X11 fallback (§20.1). Asked as the identity
/// the program would run as, since `PATH` is that login's.
async fn probe_waypipe(
    machine: &str,
    agent: &AgentHandle,
    logon: Option<&Logon>,
) -> Result<(), CommandError> {
    let argv = ["sh", "-c", "command -v waypipe"]
        .map(String::from)
        .to_vec();
    let out = agent
        .exec(argv, Vec::new(), None, None, PROBE_TIMEOUT, logon.cloned())
        .await
        .map_err(|e| CommandError::from(e).prefixed(machine))?;
    if out.exit_code != 0 || String::from_utf8_lossy(&out.stdout).trim().is_empty() {
        let who = logon.map_or("the agent identity", |l| l.user.as_str());
        return Err(CommandError::unsupported(format!(
            "`{machine}` has no `waypipe` on PATH for `{who}`"
        )));
    }
    Ok(())
}

/// Bind both sockets and run the session behind them. Returns once they
/// exist, since the reply that names them has not been sent yet.
fn serve(
    session: AgentSession,
    agent: AgentHandle,
    control: PathBuf,
    conns: PathBuf,
) -> Result<()> {
    let control_listener = bind(&control)?;
    let conns_listener = match bind(&conns) {
        Ok(l) => l,
        Err(e) => {
            let _ = std::fs::remove_file(&control);
            return Err(e);
        }
    };
    let app = session.id;
    tokio::spawn(async move {
        let (notes_tx, notes) = tokio::sync::mpsc::unbounded_channel();
        let accepting = tokio::spawn(accept_conns(conns_listener, agent, app, notes_tx));
        relay_control(session, control_listener, notes).await;
        // Dropping the accept loop drops every splice it runs, and each
        // dropped channel is closed on the agent.
        accepting.abort();
        let _ = std::fs::remove_file(&control);
        let _ = std::fs::remove_file(&conns);
    });
    Ok(())
}

/// A private socket at `path`: the same run directory as `term-<id>.sock`,
/// readable by the daemon's user alone.
fn bind(path: &Path) -> Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    Ok(listener)
}

/// What one session event puts on the control socket.
enum Relay {
    /// Record bytes, already framed by the agent.
    More(Vec<u8>),
    /// The last record: the session is over.
    Last(Vec<u8>),
}

fn relay_of(ev: Option<SessionEvent>) -> Option<Relay> {
    let last = |record| Some(Relay::Last(app::encode_record(&record)));
    match ev {
        Some(SessionEvent::Data(bytes)) => Some(Relay::More(bytes)),
        Some(SessionEvent::Exited(code)) => last(AppRecord::Exited { code }),
        Some(SessionEvent::Error(msg)) => last(AppRecord::Failed { msg }),
        None => last(AppRecord::Failed {
            msg: "the agent channel closed".into(),
        }),
        Some(SessionEvent::Stderr(_) | SessionEvent::Eof) => None,
    }
}

/// Serve the control socket's one client until the session ends.
///
/// The session is drained from the start, client or no client: the agent
/// multiplexer closes a channel whose consumer stalls, so records that arrive
/// before the CLI connects wait here instead. A session that ends before then
/// still reaches the client, so a program that exits at once still reports
/// its code.
///
/// `notes` are the daemon's own lines for the CLI — a connection the agent
/// would not open — sent as `log` records beside the agent's.
async fn relay_control(
    mut session: AgentSession,
    listener: UnixListener,
    mut notes: tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let note = |text: String| app::encode_record(&AppRecord::Log { text });
    let grace = tokio::time::sleep(ACCEPT_GRACE);
    tokio::pin!(grace);
    let mut backlog = Vec::new();
    let mut over = false;
    let client = loop {
        tokio::select! {
            accepted = listener.accept() => break accepted.ok().map(|(stream, _)| stream),
            _ = &mut grace => break None,
            Some(text) = notes.recv(), if !over => backlog.extend(note(text)),
            ev = session.recv(), if !over => match relay_of(ev) {
                Some(Relay::More(bytes)) => {
                    backlog.extend(bytes);
                    if backlog.len() > BACKLOG_LIMIT {
                        backlog.extend(app::encode_record(&AppRecord::Failed {
                            msg: "nobody read the session's records".into(),
                        }));
                        session.close().await;
                        return finish_unread(listener, backlog, grace).await;
                    }
                }
                Some(Relay::Last(bytes)) => {
                    backlog.extend(bytes);
                    over = true;
                }
                None => {}
            },
        }
    };
    drop(listener);
    let Some(client) = client else {
        // Nobody came: the program the open started has nobody to show its
        // windows to, so it goes rather than lingering in the guest.
        if !over {
            session.close().await;
        }
        return;
    };

    let (mut client_rx, mut client_tx) = client.into_split();
    if client_tx.write_all(&backlog).await.is_err() {
        if !over {
            session.close().await;
        }
        return;
    }
    if over {
        return;
    }
    let mut ignored = [0u8; 1024];
    loop {
        tokio::select! {
            n = client_rx.read(&mut ignored) => if let Ok(0) | Err(_) = n {
                session.close().await;
                return;
            },
            Some(text) = notes.recv() => {
                if client_tx.write_all(&note(text)).await.is_err() {
                    session.close().await;
                    return;
                }
            }
            ev = session.recv() => match relay_of(ev) {
                Some(Relay::More(bytes)) => {
                    if client_tx.write_all(&bytes).await.is_err() {
                        session.close().await;
                        return;
                    }
                }
                Some(Relay::Last(bytes)) => {
                    let _ = client_tx.write_all(&bytes).await;
                    return;
                }
                None => {}
            },
        }
    }
}

/// Hand a client that arrives within the grace whatever was held for it,
/// once the session is already over.
async fn finish_unread(
    listener: UnixListener,
    backlog: Vec<u8>,
    grace: std::pin::Pin<&mut tokio::time::Sleep>,
) {
    tokio::select! {
        accepted = listener.accept() => if let Ok((mut client, _)) = accepted {
            let _ = client.write_all(&backlog).await;
        },
        _ = grace => {}
    }
}

/// Accept transport connections for the app session `app` until aborted,
/// telling `notes` of each one the agent would not open.
async fn accept_conns(
    listener: UnixListener,
    agent: AgentHandle,
    app: u32,
    notes: tokio::sync::mpsc::UnboundedSender<String>,
) {
    let mut splices = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    splices.spawn(conn(stream, agent.clone(), app, notes.clone()));
                }
                Err(e) => {
                    tracing::warn!("app session {app}: accepting a connection failed: {e}");
                    return;
                }
            },
            Some(_) = splices.join_next() => {}
        }
    }
}

/// One transport connection: its own agent channel, spliced both ways.
async fn conn(
    stream: UnixStream,
    agent: AgentHandle,
    app: u32,
    notes: tokio::sync::mpsc::UnboundedSender<String>,
) {
    match agent.open_app_conn(app).await {
        Ok(session) => splice(stream, session).await,
        // Only this connection fails: dropping the stream closes it, and the
        // protocol's own client decides what one refused connection means.
        // Its client sees only the close, so the reason goes to the CLI.
        Err(e) => {
            tracing::warn!("app session {app}: opening a connection failed: {e:#}");
            let _ = notes.send(format!("a connection could not be opened: {e:#}"));
        }
    }
}

/// Copy bytes both ways with half-close, as an exec's channel carries them:
/// the client's EOF becomes the agent's `eof`, and the agent's EOF shuts the
/// client's write half. Done when both halves are, or at once when the
/// channel ends.
async fn splice(stream: UnixStream, mut session: AgentSession) {
    let (mut client_rx, mut client_tx) = stream.into_split();
    let sender = session.sender();
    let mut up = tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match client_rx.read(&mut buf).await {
                Ok(0) => {
                    let _ = sender.eof().await;
                    return;
                }
                Ok(n) => {
                    if sender.send(&buf[..n]).await.is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    let half_closed = loop {
        match session.recv().await {
            Some(SessionEvent::Data(bytes)) => {
                if client_tx.write_all(&bytes).await.is_err() {
                    break false;
                }
            }
            Some(SessionEvent::Eof) => {
                let _ = client_tx.shutdown().await;
                break true;
            }
            Some(SessionEvent::Stderr(_)) => {}
            Some(SessionEvent::Exited(_) | SessionEvent::Error(_)) | None => break false,
        }
    };
    if half_closed {
        let _ = (&mut up).await;
    } else {
        up.abort();
    }
    session.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};
    use vmlab_agent_proto::{
        AgentMsg, Frame, FrameDecoder, FrameKind, HostMsg, PROTO_VERSION, encode_ctrl, encode_frame,
    };

    /// What the scripted agent saw the host send, apart from the handshake
    /// and credit grants.
    #[derive(Debug)]
    enum Seen {
        Ctrl(HostMsg),
        Data(u32, Vec<u8>),
    }

    /// An agent whose every answer the test writes. It handshakes with the
    /// features it was given and otherwise only reports what arrived.
    struct Scripted {
        seen: mpsc::UnboundedReceiver<Seen>,
        tx: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    }

    impl Scripted {
        async fn say(&self, msg: AgentMsg) {
            let _ = self.tx.lock().await.write_all(&encode_ctrl(&msg)).await;
        }

        async fn data(&self, id: u32, bytes: &[u8]) {
            let frame = encode_frame(FrameKind::Data, id, bytes);
            let _ = self.tx.lock().await.write_all(&frame).await;
        }

        async fn record(&self, id: u32, record: AppRecord) {
            self.data(id, &app::encode_record(&record)).await;
        }

        async fn next(&mut self) -> Seen {
            tokio::time::timeout(Duration::from_secs(5), self.seen.recv())
                .await
                .expect("the host sent nothing")
                .expect("the host hung up")
        }

        async fn next_ctrl(&mut self) -> HostMsg {
            match self.next().await {
                Seen::Ctrl(msg) => msg,
                other => panic!("expected a control message, got {other:?}"),
            }
        }

        /// Answer the waypipe probe with `found`, checking it ran as `who`.
        async fn answer_probe(&mut self, found: bool, who: Option<&str>) {
            let HostMsg::OpenExec {
                id, argv, logon, ..
            } = self.next_ctrl().await
            else {
                panic!("expected the waypipe probe");
            };
            assert_eq!(argv, ["sh", "-c", "command -v waypipe"]);
            assert_eq!(logon.map(|l| l.user).as_deref(), who);
            self.say(AgentMsg::Opened { id }).await;
            // A collected exec closes its stdin at once.
            assert!(matches!(self.next_ctrl().await, HostMsg::Eof { id: eof } if eof == id));
            if found {
                self.data(id, b"/usr/bin/waypipe\n").await;
            }
            self.say(AgentMsg::Exited {
                id,
                code: if found { 0 } else { 1 },
            })
            .await;
        }

        /// Accept the `open_app`, returning its channel and mode.
        async fn accept_app(&mut self) -> (u32, AppMode, Vec<String>, Option<Logon>) {
            let HostMsg::OpenApp {
                id,
                mode,
                argv,
                logon,
                ..
            } = self.next_ctrl().await
            else {
                panic!("expected open_app");
            };
            self.say(AgentMsg::Opened { id }).await;
            (id, mode, argv, logon)
        }
    }

    async fn scripted(features: &[&str]) -> (tempfile::TempDir, AgentHandle, Scripted) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (seen_tx, seen) = mpsc::unbounded_channel();
        let (tx_slot, tx_rx) = tokio::sync::oneshot::channel();
        let features: Vec<String> = features.iter().map(|f| f.to_string()).collect();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut rx, tx) = stream.into_split();
            let tx = Arc::new(Mutex::new(tx));
            let _ = tx_slot.send(tx.clone());
            let mut dec = FrameDecoder::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = match rx.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                dec.push(&buf[..n]);
                while let Some(Frame {
                    kind,
                    channel,
                    payload,
                }) = dec.next_frame()
                {
                    if kind != FrameKind::Ctrl {
                        let _ = seen_tx.send(Seen::Data(channel, payload));
                        continue;
                    }
                    match serde_json::from_slice(&payload).unwrap() {
                        HostMsg::Hello { token, .. } => {
                            let hello = AgentMsg::Hello {
                                proto_version: PROTO_VERSION,
                                agent_version: "0.1.0-mock".into(),
                                os: "linux".into(),
                                features: features.clone(),
                                token,
                                instance: None,
                            };
                            let _ = tx.lock().await.write_all(&encode_ctrl(&hello)).await;
                        }
                        HostMsg::WindowAdjust { .. } => {}
                        msg => {
                            let _ = seen_tx.send(Seen::Ctrl(msg));
                        }
                    }
                }
            }
        });
        let handle = AgentHandle::connect(&path, Duration::from_secs(5))
            .await
            .unwrap();
        let tx = tx_rx.await.unwrap();
        (dir, handle, Scripted { seen, tx })
    }

    const ALL: &[&str] = &[features::APP_WAYPIPE, features::APP_X11, features::APP_RDP];

    fn target(dir: &Path, os: GuestOs) -> Target {
        Target {
            name: "web".into(),
            guest_os: os,
            run_dir: dir.join("run"),
        }
    }

    fn request(mode: &str, argv: &[&str]) -> AppRequest {
        AppRequest {
            mode: mode.into(),
            argv: argv.iter().map(|a| a.to_string()).collect(),
            cwd: None,
            cookie: None,
        }
    }

    fn dev() -> Option<Logon> {
        Some(Logon {
            user: "dev".into(),
            secret: "hunter2".into(),
            elevated: false,
        })
    }

    fn cookie() -> String {
        "0123456789abcdef".repeat(2)
    }

    /// Open on a fresh scripted agent, answering the agent's side with
    /// `script` concurrently.
    async fn open_scripted<F, Fut>(
        os: GuestOs,
        req: AppRequest,
        logon: Option<Logon>,
        script: F,
    ) -> (tempfile::TempDir, Scripted, Result<Value, CommandError>)
    where
        F: FnOnce(Scripted) -> Fut,
        Fut: Future<Output = Scripted>,
    {
        let (dir, agent, script_end) = scripted(ALL).await;
        let target = target(dir.path(), os);
        let (reply, script_end) =
            tokio::join!(open(&target, &agent, req, || Ok(logon)), script(script_end));
        (dir, script_end, reply)
    }

    async fn read_record(stream: &mut UnixStream) -> AppRecord {
        let mut dec = app::RecordDecoder::new();
        let mut buf = [0u8; 4096];
        loop {
            if let Some(r) = dec.next_record().unwrap() {
                return r;
            }
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("no record arrived")
                .unwrap();
            assert!(n > 0, "control socket closed before a record");
            dec.push(&buf[..n]);
        }
    }

    async fn gone(path: &Path) -> bool {
        for _ in 0..100 {
            if !path.exists() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn path_of(reply: &Value, key: &str) -> PathBuf {
        PathBuf::from(reply[key].as_str().unwrap())
    }

    /// A waypipe open probes the guest as the login, opens the session as
    /// that login and answers with the two sockets, private to the daemon's
    /// user, named after the session — and no RDP credentials.
    #[tokio::test]
    async fn a_waypipe_open_probes_then_replies_with_both_sockets() {
        let (_dir, _agent, reply) = open_scripted(
            GuestOs::Linux,
            request("waypipe", &["foot"]),
            dev(),
            |mut a| async move {
                a.answer_probe(true, Some("dev")).await;
                let (id, mode, argv, logon) = a.accept_app().await;
                assert_eq!(id, 2);
                assert_eq!(mode, AppMode::Waypipe);
                assert_eq!(argv, ["foot"]);
                assert_eq!(logon.unwrap().user, "dev");
                a
            },
        )
        .await;
        let reply = reply.unwrap();
        assert_eq!(reply["session"], 2);
        assert!(reply.get("rdp").is_none());
        let control = path_of(&reply, "control");
        let conns = path_of(&reply, "conns");
        assert!(control.ends_with("run/app-2.sock"), "{control:?}");
        assert!(conns.ends_with("run/app-2-conns.sock"), "{conns:?}");
        use std::os::unix::fs::PermissionsExt;
        for sock in [&control, &conns] {
            let mode = std::fs::metadata(sock).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{sock:?}");
        }
    }

    /// An X11 open carries the caller's cookie to the agent and needs no
    /// probe: X11 needs nothing installed in the guest.
    #[tokio::test]
    async fn an_x11_open_carries_the_cookie() {
        let mut req = request("x11", &["xeyes"]);
        req.cookie = Some(cookie());
        let (_dir, _agent, reply) = open_scripted(GuestOs::Linux, req, None, |mut a| async move {
            let (_, mode, _, logon) = a.accept_app().await;
            assert_eq!(mode, AppMode::X11 { cookie: cookie() });
            assert!(logon.is_none(), "no login is the agent identity");
            a
        })
        .await;
        let reply = reply.unwrap();
        assert_eq!(reply["session"], 1);
        assert!(reply.get("rdp").is_none());
    }

    /// An RDP open hands the CLI the resolved login's account and secret,
    /// which FreeRDP needs, and sends the agent no argv.
    #[tokio::test]
    async fn an_rdp_open_replies_with_the_logins_credentials() {
        let (_dir, _agent, reply) = open_scripted(
            GuestOs::Windows,
            request("rdp", &[]),
            dev(),
            |mut a| async move {
                let (_, mode, argv, _) = a.accept_app().await;
                assert_eq!(mode, AppMode::Rdp);
                assert!(argv.is_empty());
                a
            },
        )
        .await;
        let reply = reply.unwrap();
        assert_eq!(reply["rdp"], json!({"user": "dev", "password": "hunter2"}));
    }

    /// The agent refusing the open (Windows Home, say) is the reply, under
    /// the machine's name, and leaves no socket behind.
    #[tokio::test]
    async fn an_agent_refusal_is_the_reply() {
        let (dir, _agent, reply) = open_scripted(
            GuestOs::Windows,
            request("rdp", &[]),
            dev(),
            |mut a| async move {
                let HostMsg::OpenApp { id, .. } = a.next_ctrl().await else {
                    panic!("expected open_app");
                };
                a.say(AgentMsg::Error {
                    id: Some(id),
                    msg: "Windows Core has no Remote Desktop host".into(),
                })
                .await;
                a
            },
        )
        .await;
        let err = reply.unwrap_err();
        assert_eq!(err.message, "web: Windows Core has no Remote Desktop host");
        assert!(!dir.path().join("run").exists());
    }

    async fn x11_session() -> (tempfile::TempDir, Scripted, u32, Value) {
        let mut req = request("x11", &["xeyes"]);
        req.cookie = Some(cookie());
        let (dir, agent, reply) = open_scripted(GuestOs::Linux, req, None, |mut a| async move {
            a.accept_app().await;
            a
        })
        .await;
        let reply = reply.unwrap();
        let id = reply["session"].as_u64().unwrap() as u32;
        (dir, agent, id, reply)
    }

    /// The agent's records reach the CLI byte for byte — including those
    /// sent before it connected — and its exit becomes an `exited` record,
    /// after which the control socket closes and both sockets go.
    #[tokio::test]
    async fn records_are_relayed_and_the_exit_ends_the_session() {
        let (_dir, agent, id, reply) = x11_session().await;
        agent
            .record(
                id,
                AppRecord::Ready {
                    display: Some(":10".into()),
                },
            )
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut control = UnixStream::connect(path_of(&reply, "control"))
            .await
            .unwrap();
        assert_eq!(
            read_record(&mut control).await,
            AppRecord::Ready {
                display: Some(":10".into())
            }
        );
        agent.record(id, AppRecord::Pending).await;
        assert_eq!(read_record(&mut control).await, AppRecord::Pending);
        agent.say(AgentMsg::Exited { id, code: 3 }).await;
        assert_eq!(
            read_record(&mut control).await,
            AppRecord::Exited { code: 3 }
        );
        let mut rest = Vec::new();
        control.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        assert!(gone(&path_of(&reply, "control")).await);
        assert!(gone(&path_of(&reply, "conns")).await);
    }

    /// An agent error on the app channel becomes a `failed` record.
    #[tokio::test]
    async fn an_agent_error_becomes_a_failed_record() {
        let (_dir, agent, id, reply) = x11_session().await;
        let mut control = UnixStream::connect(path_of(&reply, "control"))
            .await
            .unwrap();
        agent
            .say(AgentMsg::Error {
                id: Some(id),
                msg: "Xauthority unwritable".into(),
            })
            .await;
        assert_eq!(
            read_record(&mut control).await,
            AppRecord::Failed {
                msg: "Xauthority unwritable".into()
            }
        );
        assert!(gone(&path_of(&reply, "conns")).await);
    }

    /// The CLI hanging up on the control socket closes the app channel on
    /// the agent, which kills the program, and unlinks both sockets.
    #[tokio::test]
    async fn the_cli_hanging_up_closes_the_session() {
        let (_dir, mut agent, id, reply) = x11_session().await;
        let control = UnixStream::connect(path_of(&reply, "control"))
            .await
            .unwrap();
        drop(control);
        match agent.next_ctrl().await {
            HostMsg::Close { id: closed } => assert_eq!(closed, id),
            other => panic!("expected close, got {other:?}"),
        }
        assert!(gone(&path_of(&reply, "control")).await);
        assert!(gone(&path_of(&reply, "conns")).await);
    }

    /// A connection to `conns` becomes an `open_app_conn` naming the session,
    /// bytes flow both ways, and each side's EOF half-closes the other: the
    /// guest's EOF shuts the client's write half while the client can still
    /// send, and the client's EOF reaches the agent as `eof`.
    #[tokio::test]
    async fn a_connection_is_spliced_with_half_close() {
        let (_dir, mut agent, app, reply) = x11_session().await;
        let _control = UnixStream::connect(path_of(&reply, "control"))
            .await
            .unwrap();
        let mut client = UnixStream::connect(path_of(&reply, "conns")).await.unwrap();
        let HostMsg::OpenAppConn { id, app: of } = agent.next_ctrl().await else {
            panic!("expected open_app_conn");
        };
        assert_eq!(of, app);
        agent.say(AgentMsg::Opened { id }).await;

        agent.data(id, b"from-guest").await;
        agent.say(AgentMsg::Eof { id }).await;
        let mut got = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut got))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            got, b"from-guest",
            "the guest's EOF shuts the client's read side"
        );

        client.write_all(b"from-host").await.unwrap();
        match agent.next().await {
            Seen::Data(ch, bytes) => assert_eq!((ch, bytes.as_slice()), (id, &b"from-host"[..])),
            other => panic!("expected data, got {other:?}"),
        }
        client.shutdown().await.unwrap();
        match agent.next_ctrl().await {
            HostMsg::Eof { id: eof } => assert_eq!(eof, id),
            other => panic!("expected eof, got {other:?}"),
        }
        // Both halves done: the connection's channel closes, and the session
        // goes on.
        match agent.next_ctrl().await {
            HostMsg::Close { id: closed } => assert_eq!(closed, id),
            other => panic!("expected close, got {other:?}"),
        }
        assert!(path_of(&reply, "conns").exists());
    }

    /// The client's EOF first: the agent hears `eof` and can still answer.
    #[tokio::test]
    async fn the_clients_eof_leaves_the_guest_half_open() {
        let (_dir, mut agent, _app, reply) = x11_session().await;
        let _control = UnixStream::connect(path_of(&reply, "control"))
            .await
            .unwrap();
        let mut client = UnixStream::connect(path_of(&reply, "conns")).await.unwrap();
        let HostMsg::OpenAppConn { id, .. } = agent.next_ctrl().await else {
            panic!("expected open_app_conn");
        };
        agent.say(AgentMsg::Opened { id }).await;
        client.shutdown().await.unwrap();
        match agent.next_ctrl().await {
            HostMsg::Eof { id: eof } => assert_eq!(eof, id),
            other => panic!("expected eof, got {other:?}"),
        }
        agent.data(id, b"late-reply").await;
        agent.say(AgentMsg::Eof { id }).await;
        let mut got = Vec::new();
        client.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"late-reply");
    }

    /// A connection the agent will not open closes that client only, and
    /// the CLI is told why, since that client sees only the close; the next
    /// connection is served.
    #[tokio::test]
    async fn a_refused_connection_closes_only_that_client() {
        let (_dir, mut agent, _app, reply) = x11_session().await;
        let mut control = UnixStream::connect(path_of(&reply, "control"))
            .await
            .unwrap();
        let mut refused = UnixStream::connect(path_of(&reply, "conns")).await.unwrap();
        let HostMsg::OpenAppConn { id, .. } = agent.next_ctrl().await else {
            panic!("expected open_app_conn");
        };
        agent
            .say(AgentMsg::Error {
                id: Some(id),
                msg: "no parked connection".into(),
            })
            .await;
        let mut got = Vec::new();
        refused.read_to_end(&mut got).await.unwrap();
        assert!(got.is_empty());
        assert_eq!(
            read_record(&mut control).await,
            AppRecord::Log {
                text: "a connection could not be opened: no parked connection".into()
            }
        );

        let _next = UnixStream::connect(path_of(&reply, "conns")).await.unwrap();
        assert!(matches!(
            agent.next_ctrl().await,
            HostMsg::OpenAppConn { .. }
        ));
    }

    /// The machine stopping closes the agent channel under the session: the
    /// CLI is told, and both sockets go.
    #[tokio::test]
    async fn the_agent_going_away_fails_the_session() {
        let (_dir, agent, _id, reply) = x11_session().await;
        let mut control = UnixStream::connect(path_of(&reply, "control"))
            .await
            .unwrap();
        let _ = agent.tx.lock().await.shutdown().await;
        assert_eq!(
            read_record(&mut control).await,
            AppRecord::Failed {
                msg: "the agent channel closed".into()
            }
        );
        assert!(gone(&path_of(&reply, "control")).await);
        assert!(gone(&path_of(&reply, "conns")).await);
    }

    /// Every refusal before the session opens, with its code and words.
    #[tokio::test]
    async fn refusals_name_their_reason() {
        use crate::proto::error::ErrorCode::{InvalidArgument, Unsupported};
        let refuse = |os, features: &'static [&'static str], req, logon| async move {
            let (dir, agent, _script) = scripted(features).await;
            let err = open(&target(dir.path(), os), &agent, req, || Ok(logon))
                .await
                .unwrap_err();
            (err.code, err.message)
        };
        let x11 = |cookie: Option<&str>, argv: &[&str]| {
            let mut req = request("x11", argv);
            req.cookie = cookie.map(String::from);
            req
        };
        let cases = [
            (
                refuse(GuestOs::Linux, ALL, request("vnc", &["x"]), None).await,
                (
                    InvalidArgument,
                    "unknown shared-app mode `vnc`: expected `waypipe`, `x11` or `rdp`",
                ),
            ),
            (
                refuse(GuestOs::Linux, ALL, request("rdp", &[]), dev()).await,
                (
                    Unsupported,
                    "`rdp` shares apps from Windows guests; `web` is a Linux guest",
                ),
            ),
            (
                refuse(GuestOs::Windows, ALL, request("waypipe", &["x"]), dev()).await,
                (
                    Unsupported,
                    "`waypipe` shares apps from Linux guests; `web` is a Windows guest",
                ),
            ),
            (
                refuse(
                    GuestOs::Linux,
                    &[features::APP_WAYPIPE],
                    x11(Some(&cookie()), &["x"]),
                    None,
                )
                .await,
                (
                    Unsupported,
                    "the agent on `web` predates shared apps over `x11`; run `vmlab machine \
                     repair-agent web`",
                ),
            ),
            (
                refuse(GuestOs::Windows, ALL, request("rdp", &[]), None).await,
                (
                    InvalidArgument,
                    "RDP needs an account: declare a `login {}` on `web` or pass \
                     --user/--password",
                ),
            ),
            (
                refuse(GuestOs::Linux, ALL, x11(None, &["x"]), None).await,
                (
                    InvalidArgument,
                    "`x11` needs the cookie the caller minted for this session",
                ),
            ),
            (
                refuse(GuestOs::Linux, ALL, x11(Some("ABC"), &["x"]), None).await,
                (
                    InvalidArgument,
                    "an `x11` cookie is 32 lowercase hex digits",
                ),
            ),
            (
                refuse(GuestOs::Linux, ALL, x11(Some(&cookie()), &[]), None).await,
                (InvalidArgument, "`x11` needs a program to run"),
            ),
            (
                refuse(GuestOs::Linux, ALL, request("waypipe", &[]), None).await,
                (InvalidArgument, "`waypipe` needs a program to run"),
            ),
            (
                refuse(GuestOs::Windows, ALL, request("rdp", &["notepad"]), dev()).await,
                (
                    InvalidArgument,
                    "`rdp` takes no argv: the RDP client names the program",
                ),
            ),
        ];
        for ((code, message), (want_code, want)) in cases {
            assert_eq!((code, message.as_str()), (want_code, want));
        }
    }

    /// A login resolution that fails is the reply, unchanged.
    #[tokio::test]
    async fn a_failed_login_resolution_is_the_reply() {
        let (dir, agent, _script) = scripted(ALL).await;
        let err = open(
            &target(dir.path(), GuestOs::Windows),
            &agent,
            request("rdp", &[]),
            || Err(CommandError::failed("`ghost` is not a login declared")),
        )
        .await
        .unwrap_err();
        assert_eq!(err, CommandError::failed("`ghost` is not a login declared"));
    }

    /// A guest with no `waypipe` for the login is `unsupported`, naming the
    /// account — or the agent identity when there is no login — which is
    /// what the CLI falls back to X11 on.
    #[tokio::test]
    async fn a_guest_without_waypipe_is_unsupported() {
        for (logon, who) in [(dev(), "dev"), (None, "the agent identity")] {
            let expect = logon.as_ref().map(|l| l.user.clone());
            let (_dir, _agent, reply) = open_scripted(
                GuestOs::Linux,
                request("waypipe", &["foot"]),
                logon,
                |mut a| async move {
                    a.answer_probe(false, expect.as_deref()).await;
                    a
                },
            )
            .await;
            let err = reply.unwrap_err();
            assert_eq!(err.code, crate::proto::error::ErrorCode::Unsupported);
            assert_eq!(
                err.message,
                format!("`web` has no `waypipe` on PATH for `{who}`")
            );
        }
    }
}
