//! The facade end to end: a real SSH client, over a socket pair, against a
//! mock agent.
//!
//! Driving it with `russh`'s own client is the point — the contract §19.3
//! writes down is what an SSH client observes, not what the handler was
//! called with, and only a client can tell "the request was answered" from
//! "the request was answered the way SSH means it".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use vmlab_agent_proto::{
    AgentMsg, ErrorCause, Frame, FrameDecoder, FrameKind, HostMsg, INITIAL_WINDOW, Logon,
    PROTO_VERSION, encode_ctrl, encode_frame,
};

use super::*;
use crate::config::model::Login;
use crate::labd::vm_agent::AgentHandle;

// ---------------------------------------------------------------------------
// The mock agent
// ---------------------------------------------------------------------------

/// What the guest side was asked to do, as the test wants to read it back.
#[derive(Debug, Clone, PartialEq)]
enum Opened {
    Terminal {
        cols: u16,
        rows: u16,
        command: Option<Vec<String>>,
        env: Vec<(String, String)>,
        logon: Option<Logon>,
    },
    Exec {
        argv: Vec<String>,
        env: Vec<(String, String)>,
        logon: Option<Logon>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// A dial the guest was asked to make, with the destination exactly as it
    /// crossed — a name stays a name, because the guest is what resolves it.
    Tunnel {
        host: String,
        port: u16,
    },
}

/// How long a dial to a `slow.` destination takes to fail. Stands in for the
/// agent's real dial budget, which is seconds.
const SLOW_DIAL: Duration = Duration::from_secs(2);

#[derive(Default)]
struct AgentLog {
    opens: Mutex<Vec<Opened>>,
    /// Bytes the host sent towards the guest, per channel.
    input: Mutex<HashMap<u32, Vec<u8>>>,
    /// Channels the host closed — the guest's only sign that a terminal,
    /// an exec or a guest-side socket may be let go.
    closed: Mutex<Vec<u32>>,
}

/// A guest agent that answers terminals, execs and tunnels.
///
/// A terminal echoes its keystrokes back — enough for a test to see bytes
/// travel both ways through the facade — and exits with the code named by
/// `exit <n>` typed into it. An exec writes a line to stdout, a line to
/// stderr, and exits with the code its command line ends in. A tunnel is an
/// echo peer that half-closes when its client does, unless its destination is
/// named `dead.<something>` or `slow.<something>` — the dial then fails the
/// way a dead address does, at once or after spending a budget.
async fn mock_agent() -> (tempfile::TempDir, AgentHandle, Arc<AgentLog>) {
    mock_agent_with(EVERY_FEATURE).await
}

/// Everything this facade asks a guest for.
const EVERY_FEATURE: &[&str] = &["terminal", "exec", "tunnel"];

/// The same guest, declaring only `features` — which is how a test stands up
/// an agent too old to serve one of the vocabularies (§19.4).
async fn mock_agent_with(features: &[&str]) -> (tempfile::TempDir, AgentHandle, Arc<AgentLog>) {
    let features: Vec<String> = features.iter().map(|f| f.to_string()).collect();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&path).expect("bind");
    let log = Arc::new(AgentLog::default());

    let served = log.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (mut rx, tx) = stream.into_split();
        let tx = Arc::new(tokio::sync::Mutex::new(tx));
        let send = |msg: AgentMsg| {
            let tx = tx.clone();
            async move {
                let _ = tx.lock().await.write_all(&encode_ctrl(&msg)).await;
            }
        };
        let send_data = |kind: FrameKind, id: u32, payload: Vec<u8>| {
            let tx = tx.clone();
            async move {
                let _ = tx
                    .lock()
                    .await
                    .write_all(&encode_frame(kind, id, &payload))
                    .await;
            }
        };

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
                match kind {
                    FrameKind::Ctrl => {
                        let msg: HostMsg = serde_json::from_slice(&payload).unwrap();
                        match msg {
                            HostMsg::Hello { token, .. } => {
                                send(AgentMsg::Hello {
                                    proto_version: PROTO_VERSION,
                                    agent_version: "mock".into(),
                                    os: "linux".into(),
                                    features: features.clone(),
                                    token,
                                })
                                .await;
                            }
                            HostMsg::OpenTerminal {
                                id,
                                cols,
                                rows,
                                command,
                                env,
                                logon,
                            } => {
                                served.opens.lock().unwrap().push(Opened::Terminal {
                                    cols,
                                    rows,
                                    command,
                                    env,
                                    logon,
                                });
                                send(AgentMsg::Opened { id }).await;
                                send_data(FrameKind::Data, id, b"prompt$ ".to_vec()).await;
                            }
                            HostMsg::OpenExec {
                                id,
                                argv,
                                env,
                                logon,
                                ..
                            } => {
                                let code = exit_code_of(&argv);
                                served.opens.lock().unwrap().push(Opened::Exec {
                                    argv,
                                    env,
                                    logon,
                                });
                                send(AgentMsg::Opened { id }).await;
                                send_data(FrameKind::Data, id, b"out\n".to_vec()).await;
                                send_data(FrameKind::DataErr, id, b"err\n".to_vec()).await;
                                send(AgentMsg::Exited { id, code }).await;
                            }
                            HostMsg::Resize { cols, rows, .. } => {
                                served
                                    .opens
                                    .lock()
                                    .unwrap()
                                    .push(Opened::Resize { cols, rows });
                            }
                            HostMsg::OpenTunnel { id, host, port } => {
                                served.opens.lock().unwrap().push(Opened::Tunnel {
                                    host: host.clone(),
                                    port,
                                });
                                // Nothing answers a `dead.` or `slow.`
                                // destination, and a dial that does not
                                // succeed fails the channel with a
                                // machine-readable cause — the distinction
                                // the facade has to keep (§19.5).
                                let dead = host.starts_with("dead.");
                                let slow = host.starts_with("slow.");
                                match dead || slow {
                                    false => send(AgentMsg::Opened { id }).await,
                                    true => {
                                        let failure = send(AgentMsg::Error {
                                            id: Some(id),
                                            msg: format!(
                                                "tunnel {host}:{port}: connection refused"
                                            ),
                                            cause: Some(ErrorCause::ConnectFailed),
                                        });
                                        // A `slow.` destination spends a
                                        // dial budget before it fails, which
                                        // is what a dead address really
                                        // does. Spawned, because the guest
                                        // goes on answering everything else
                                        // while one channel is dialling.
                                        match slow {
                                            true => {
                                                tokio::spawn(async move {
                                                    tokio::time::sleep(SLOW_DIAL).await;
                                                    failure.await;
                                                });
                                            }
                                            false => failure.await,
                                        }
                                    }
                                }
                            }
                            // The peer answers the host's FIN with its own,
                            // which is what makes a half-close observable
                            // from the client's side.
                            HostMsg::Eof { id } => send(AgentMsg::Eof { id }).await,
                            HostMsg::Close { id } => served.closed.lock().unwrap().push(id),
                            _ => {}
                        }
                    }
                    FrameKind::Data => {
                        served
                            .input
                            .lock()
                            .unwrap()
                            .entry(channel)
                            .or_default()
                            .extend(&payload);
                        // Keystrokes echo, and `exit <n>` ends the shell with
                        // that code — the two things a test needs to observe
                        // a terminal from the client's side.
                        send_data(FrameKind::Data, channel, payload.clone()).await;
                        let typed = String::from_utf8_lossy(&payload).to_string();
                        if let Some(code) = typed.strip_prefix("exit ") {
                            let code = code.trim().parse().unwrap_or(0);
                            send(AgentMsg::Exited { id: channel, code }).await;
                        }
                        send(AgentMsg::WindowAdjust {
                            id: channel,
                            bytes: INITIAL_WINDOW,
                        })
                        .await;
                    }
                    FrameKind::DataErr => {}
                }
            }
        }
    });

    let agent = AgentHandle::connect(&path, Duration::from_secs(5))
        .await
        .expect("connect the mock agent");
    (dir, agent, log)
}

/// The exit code an exec's command line asks for: the number after its last
/// `exit `, else 0.
fn exit_code_of(argv: &[String]) -> i32 {
    argv.last()
        .and_then(|line| line.rsplit("exit ").next())
        .and_then(|tail| tail.trim().parse().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The facade under test
// ---------------------------------------------------------------------------

fn login(label: &str, user: &str, default: bool) -> Login {
    Login {
        label: label.into(),
        user: user.into(),
        password: Some("vmlab123!".into()),
        elevated: None,
        default: default.then_some(true),
        span: (0, 0),
    }
}

fn logins() -> Vec<Login> {
    vec![
        login("dev", r"PROBE\dev", true),
        login("admin", r"PROBE\administrator", false),
    ]
}

/// Every event the facade emitted, which is where refusals land.
type Recorded = Arc<Mutex<Vec<(String, Value)>>>;

struct Harness {
    _agent_dir: tempfile::TempDir,
    agent_log: Arc<AgentLog>,
    events: Recorded,
    session: client::Handle<Client>,
}

struct Client;

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

fn a_key() -> russh::keys::PrivateKey {
    russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap()
}

fn spec_for(logins: Vec<Login>, guest_os: GuestOs, events: Events) -> FacadeSpec {
    FacadeSpec {
        machine: "dev01".into(),
        logins,
        guest_os,
        key: a_key(),
        host_user: Some("localdev".into()),
        events,
    }
}

/// Stand a facade up over an in-memory socket pair and connect a client to
/// it as `username`. `logins` is what the machine declares.
async fn connect_as(username: &str, logins: Vec<Login>) -> anyhow::Result<Harness> {
    connect_with(username, logins, GuestOs::Linux).await
}

async fn connect_with(
    username: &str,
    logins: Vec<Login>,
    guest_os: GuestOs,
) -> anyhow::Result<Harness> {
    connect_featured(username, logins, guest_os, EVERY_FEATURE).await
}

/// The same, against a guest whose agent declares only `features` — the
/// facade degrades per channel, so what it still serves is as much of the
/// contract as what it refuses.
async fn connect_featured(
    username: &str,
    logins: Vec<Login>,
    guest_os: GuestOs,
    features: &[&str],
) -> anyhow::Result<Harness> {
    let (agent_dir, agent, agent_log) = mock_agent_with(features).await;
    let events: Recorded = Arc::new(Mutex::new(Vec::new()));

    let sink = events.clone();
    let spec = Arc::new(spec_for(
        logins,
        guest_os,
        Arc::new(move |event: &str, data: Value| {
            sink.lock().unwrap().push((event.to_string(), data));
        }),
    ));

    let (server_side, client_side) = UnixStream::pair()?;
    tokio::spawn(serve_connection(spec, agent, server_side));

    let config = Arc::new(client::Config::default());
    let mut session = client::connect_stream(config, client_side, Client).await?;
    if !session.authenticate_none(username).await?.success() {
        anyhow::bail!("auth refused");
    }
    Ok(Harness {
        _agent_dir: agent_dir,
        agent_log,
        events,
        session,
    })
}

impl Harness {
    fn opens(&self) -> Vec<Opened> {
        self.agent_log.opens.lock().unwrap().clone()
    }

    fn refusals(&self) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == REFUSED_EVENT)
            .map(|(_, data)| data.clone())
            .collect()
    }

    /// Wait for the facade to have opened something matching `want` — the
    /// pump runs in its own task, so an assertion straight after a request
    /// would race it.
    async fn wait_for_open(&self, want: impl Fn(&Opened) -> bool) -> Opened {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(found) = self.opens().into_iter().find(|o| want(o)) {
                return found;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "never opened; saw {:?}",
                self.opens()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// The channel's answer to the request just sent: `SSH_MSG_CHANNEL_FAILURE`
/// is what a refused channel request looks like on the wire, and it carries
/// no text — which is exactly why the reason goes to the lab event log.
async fn expect_refused(channel: &mut russh::Channel<client::Msg>) {
    match channel.wait().await {
        Some(russh::ChannelMsg::Failure) => {}
        other => panic!("expected a channel failure, got {other:?}"),
    }
}

/// Stand a facade up somewhere the real `ssh(1)` can reach it, and return
/// its port — or `None` on a host with no `ssh`, which skips the caller.
///
/// It listens on loopback purely because `ProxyCommand` needs a command and
/// the product's one is `vmlab ssh-proxy`, which needs a lab daemon. The
/// facade is transport-agnostic — [`serve_connection`] takes any stream —
/// and nothing in the product ever binds a port (ADR-0012).
async fn openssh_reachable_facade(logins: Vec<Login>) -> Option<RealSsh> {
    if std::process::Command::new("ssh")
        .arg("-V")
        .output()
        .is_err()
    {
        eprintln!("no ssh(1) on this host — skipping the OpenSSH interop test");
        return None;
    }
    let (agent_dir, agent, agent_log) = mock_agent().await;
    let spec = Arc::new(spec_for(logins, GuestOs::Linux, Arc::new(|_, _| {})));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let port = listener.local_addr().ok()?.port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = serve_connection(spec, agent, stream).await;
    });
    Some(RealSsh {
        port,
        agent_log,
        _agent_dir: agent_dir,
    })
}

struct RealSsh {
    port: u16,
    agent_log: Arc<AgentLog>,
    _agent_dir: tempfile::TempDir,
}

impl RealSsh {
    /// An `ssh` aimed at this facade, with the options every one of these
    /// tests needs and nothing else: vmlab owns its own `known_hosts`, so a
    /// test must not touch the developer's, and that is not what is under
    /// test.
    fn ssh(&self) -> tokio::process::Command {
        let mut ssh = tokio::process::Command::new("ssh");
        ssh.args([
            "-p",
            &self.port.to_string(),
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
        ]);
        ssh
    }

    fn opens(&self) -> Vec<Opened> {
        self.agent_log.opens.lock().unwrap().clone()
    }
}

/// Read from a channel until `want` bytes of data have arrived.
async fn read_data(channel: &mut russh::Channel<client::Msg>, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < want {
        match channel.wait().await {
            Some(russh::ChannelMsg::Data { data }) => got.extend(&data[..]),
            Some(_) => {}
            None => break,
        }
    }
    got
}

// ---------------------------------------------------------------------------
// Auth: the username is a selector
// ---------------------------------------------------------------------------

/// The headline of §19.3's auth: `none` succeeds and the username picks an
/// identity. With no `-l`, `ssh` sends the developer's own account name,
/// which names nobody in the lab file and therefore means "the machine's
/// default login".
#[tokio::test]
async fn no_selector_attaches_as_the_machines_default_login() {
    let h = connect_as("localdev", logins()).await.unwrap();
    let channel = h.session.channel_open_session().await.unwrap();
    channel.request_shell(true).await.unwrap();

    let opened = h
        .wait_for_open(|o| matches!(o, Opened::Terminal { .. }))
        .await;
    match opened {
        Opened::Terminal { logon, .. } => {
            assert_eq!(logon.unwrap().user, r"PROBE\dev");
        }
        other => panic!("{other:?}"),
    }
}

/// `-l admin` selects the *label*, and the label is all that crosses: the
/// account name it resolves to never has to survive an SSH username.
#[tokio::test]
async fn a_login_label_selects_that_identity() {
    let h = connect_as("admin", logins()).await.unwrap();
    let channel = h.session.channel_open_session().await.unwrap();
    channel.request_shell(true).await.unwrap();

    let opened = h
        .wait_for_open(|o| matches!(o, Opened::Terminal { .. }))
        .await;
    match opened {
        Opened::Terminal { logon, .. } => {
            assert_eq!(logon.unwrap().user, r"PROBE\administrator");
        }
        other => panic!("{other:?}"),
    }
}

/// §19.2's floor by name: `root` on Linux is the agent's own identity, which
/// is also what a machine declaring no login gets.
#[tokio::test]
async fn the_floor_is_the_agent_identity() {
    for (username, declared) in [("root", logins()), ("localdev", vec![])] {
        let h = connect_as(username, declared).await.unwrap();
        let channel = h.session.channel_open_session().await.unwrap();
        channel.request_shell(true).await.unwrap();
        let opened = h
            .wait_for_open(|o| matches!(o, Opened::Terminal { .. }))
            .await;
        match opened {
            Opened::Terminal { logon, .. } => assert!(logon.is_none(), "{username}"),
            other => panic!("{other:?}"),
        }
    }
}

/// An unrecognised label is not an identity, so nothing is ever served to
/// it, and the refusal reaches the lab event log naming what the machine
/// *does* declare.
///
/// The `none` probe itself is answered — it is how OpenSSH enumerates
/// methods, not the decision — and the connection ends on the disconnect
/// that follows. See `session::Facade::auth_none` for why the refusal
/// cannot ride the auth failure itself.
#[tokio::test]
async fn an_unknown_label_is_served_nothing_and_is_recorded() {
    let (_agent_dir, agent, _log) = mock_agent().await;
    let events: Recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let spec = Arc::new(spec_for(
        logins(),
        GuestOs::Linux,
        Arc::new(move |event: &str, data: Value| {
            sink.lock().unwrap().push((event.to_string(), data));
        }),
    ));

    let (server_side, client_side) = UnixStream::pair().unwrap();
    tokio::spawn(serve_connection(spec, agent, server_side));
    let config = Arc::new(client::Config::default());
    let mut session = client::connect_stream(config, client_side, Client)
        .await
        .unwrap();
    // The probe is answered; the connection is not. Whether the client sees
    // the reply or the disconnect first is its own race, so the assertion
    // that matters is the one below it: no channel is ever served.
    let _ = session.authenticate_none("qa").await;
    assert!(
        session.channel_open_session().await.is_err(),
        "an unknown label must be served no channel"
    );

    let (name, data) = events.lock().unwrap()[0].clone();
    assert_eq!(name, REFUSED_EVENT);
    assert_eq!(data["request"], "userauth");
    let reason = data["reason"].as_str().unwrap();
    assert!(reason.contains("`qa`"), "{reason}");
    assert!(reason.contains("dev (default)"), "{reason}");
    assert!(reason.contains("admin"), "{reason}");
    assert!(reason.contains("root"), "{reason}");
}

/// And the developer is told, in vmlab's own words, at their terminal.
///
/// This is the assertion the delivery mechanism exists for, and the reason
/// it is not the `USERAUTH_BANNER` §19.3 names (see `auth_none`): only a
/// real client can show that the words actually arrive. It caught the first
/// attempt — an OpenSSH that has run out of auth methods closes without
/// reading, so a disconnect after the auth failure reached one client
/// version and not another. The loopback listener is the same test-only
/// transport [`a_real_openssh_client_authenticates_and_gets_an_exit_code`]
/// uses.
#[tokio::test]
async fn a_real_openssh_client_is_shown_the_declared_logins() {
    let Some(facade) = openssh_reachable_facade(logins()).await else {
        return;
    };
    let out = facade
        .ssh()
        .args(["-o", "BatchMode=yes", "-l", "qa", "127.0.0.1", "true"])
        .output()
        .await
        .expect("run ssh");

    assert!(!out.status.success(), "an unknown label must not connect");
    let shown = String::from_utf8_lossy(&out.stderr);
    assert!(
        shown.contains("`qa` is not a login on this machine"),
        "{shown}"
    );
    assert!(shown.contains("dev (default), admin"), "{shown}");
}

// ---------------------------------------------------------------------------
// The session vocabulary
// ---------------------------------------------------------------------------

/// `pty-req` + `shell` is one agent terminal, sized by the request, with
/// bytes travelling both ways over it.
#[tokio::test]
async fn a_pty_shell_carries_bytes_both_ways() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel
        .request_pty(true, "xterm-256color", 132, 43, 0, 0, &[])
        .await
        .unwrap();
    channel.request_shell(true).await.unwrap();

    assert_eq!(read_data(&mut channel, 8).await, b"prompt$ ");
    channel.data(&b"whoami\r"[..]).await.unwrap();
    assert_eq!(read_data(&mut channel, 7).await, b"whoami\r");

    match &h.opens()[0] {
        Opened::Terminal {
            cols,
            rows,
            command,
            ..
        } => {
            assert_eq!((*cols, *rows), (132, 43));
            assert_eq!(*command, None, "a shell is the guest's own, not a command");
        }
        other => panic!("{other:?}"),
    }
}

/// `window-change` retargets the live terminal rather than being answered
/// and dropped.
#[tokio::test]
async fn window_change_resizes_the_live_terminal() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await
        .unwrap();
    channel.request_shell(true).await.unwrap();
    read_data(&mut channel, 8).await;

    channel.window_change(120, 40, 0, 0).await.unwrap();
    h.wait_for_open(|o| {
        *o == Opened::Resize {
            cols: 120,
            rows: 40,
        }
    })
    .await;
}

/// `env` is applied over the logon's environment, and the deny-list is
/// *dropped* rather than refused — the request still succeeds, because it is
/// best-effort by design and every distribution ships `SendEnv LANG LC_*`.
#[tokio::test]
async fn env_survives_except_the_deny_list() {
    let h = connect_as("dev", logins()).await.unwrap();
    let channel = h.session.channel_open_session().await.unwrap();
    for (name, value) in [
        ("LANG", "en_GB.UTF-8"),
        ("USERPROFILE", r"C:\Users\somebody-else"),
        ("HOME", "/tmp/elsewhere"),
        ("SSH_AUTH_SOCK", "/tmp/agent"),
        ("LC_ALL", "C"),
    ] {
        // `true` asks for a reply: a refusal here would be an error, and the
        // point is that dropping one is not.
        channel.set_env(true, name, value).await.unwrap();
    }
    channel.request_shell(true).await.unwrap();

    let opened = h
        .wait_for_open(|o| matches!(o, Opened::Terminal { .. }))
        .await;
    match opened {
        Opened::Terminal { env, .. } => {
            assert_eq!(
                env,
                vec![
                    ("LANG".to_string(), "en_GB.UTF-8".to_string()),
                    ("LC_ALL".to_string(), "C".to_string()),
                ]
            );
        }
        other => panic!("{other:?}"),
    }
}

/// `exec` runs the client's command line through the guest's own shell —
/// unsplit, because pipes and quoting are the guest shell's grammar — and
/// stderr comes back on extended data 1, which is what makes `ssh`'s own
/// stream split work.
#[tokio::test]
async fn exec_runs_the_line_through_the_guests_shell() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.exec(true, &b"ls -l | wc -l"[..]).await.unwrap();

    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let mut status = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::Data { data } => out.extend(&data[..]),
            russh::ChannelMsg::ExtendedData { data, ext: 1 } => err.extend(&data[..]),
            russh::ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            _ => {}
        }
    }
    assert_eq!(out, b"out\n");
    assert_eq!(err, b"err\n");
    assert_eq!(status, Some(0));

    match &h.opens()[0] {
        Opened::Exec { argv, .. } => {
            assert_eq!(argv, &["/bin/sh", "-c", "ls -l | wc -l"]);
        }
        other => panic!("{other:?}"),
    }
}

/// `ssh -t dev01 <cmd>`: a command the client wants a terminal for is a
/// terminal *hosting that command*, not an `exec` with pipes — `top` on the
/// far end of a pipe is not what the developer asked for, and `sshd` draws
/// the line in the same place.
#[tokio::test]
async fn a_command_that_asked_for_a_terminal_gets_one() {
    let h = connect_as("dev", logins()).await.unwrap();
    let channel = h.session.channel_open_session().await.unwrap();
    channel
        .request_pty(true, "xterm", 100, 30, 0, 0, &[])
        .await
        .unwrap();
    channel.exec(true, &b"top -b"[..]).await.unwrap();

    let opened = h
        .wait_for_open(|o| matches!(o, Opened::Terminal { .. }))
        .await;
    match opened {
        Opened::Terminal {
            cols,
            rows,
            command,
            ..
        } => {
            assert_eq!((cols, rows), (100, 30));
            assert_eq!(
                command,
                Some(vec!["/bin/sh".into(), "-c".into(), "top -b".into()])
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(
        !h.opens().iter().any(|o| matches!(o, Opened::Exec { .. })),
        "a -t command must not also become an exec: {:?}",
        h.opens()
    );
}

/// The same command line on a Windows guest goes through the interpreter its
/// terminal already hosts, so `ssh dev01 <cmd>` and a shell on `dev01` speak
/// one language.
#[tokio::test]
async fn exec_on_windows_goes_through_powershell() {
    let h = connect_with("dev", logins(), GuestOs::Windows)
        .await
        .unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.exec(true, &b"Get-Process"[..]).await.unwrap();
    while channel.wait().await.is_some() {}

    match &h.opens()[0] {
        Opened::Exec { argv, .. } => {
            assert_eq!(argv[0], "powershell.exe");
            assert_eq!(argv.last().unwrap(), "Get-Process");
        }
        other => panic!("{other:?}"),
    }
}

/// A signal-killed process reports `128 + signal` as an ordinary
/// `exit-status`; `exit-signal` is never sent, because the agent reports a
/// number and the facade will not invent a signal name to dress it up as.
#[tokio::test]
async fn a_signalled_process_reports_128_plus_signal_and_never_exit_signal() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel
        .exec(true, &b"kill -9 $$; exit 137"[..])
        .await
        .unwrap();

    let mut status = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            russh::ChannelMsg::ExitSignal { signal_name, .. } => {
                panic!("the facade sent exit-signal {signal_name:?}")
            }
            _ => {}
        }
    }
    assert_eq!(status, Some(137));
}

/// A shell's exit code reaches the client the same way — `ssh`'s own exit
/// code depends on it.
#[tokio::test]
async fn a_shell_reports_its_exit_status() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.request_shell(true).await.unwrap();
    read_data(&mut channel, 8).await;
    channel.data(&b"exit 3"[..]).await.unwrap();

    let mut status = None;
    while let Some(msg) = channel.wait().await {
        if let russh::ChannelMsg::ExitStatus { exit_status } = msg {
            status = Some(exit_status);
        }
    }
    assert_eq!(status, Some(3));
}

/// `ControlMaster` exists to put many session channels on one connection, so
/// several is the expected shape rather than a surprise.
#[tokio::test]
async fn many_session_channels_share_one_connection() {
    let h = connect_as("dev", logins()).await.unwrap();
    for _ in 0..3 {
        let mut channel = h.session.channel_open_session().await.unwrap();
        channel.request_shell(true).await.unwrap();
        assert_eq!(read_data(&mut channel, 8).await, b"prompt$ ");
    }
    assert_eq!(h.opens().len(), 3);
}

// ---------------------------------------------------------------------------
// `direct-tcpip`
// ---------------------------------------------------------------------------

/// The second channel type the facade answers, and the agent's tunnel is
/// what answers it: the destination crosses verbatim — a name stays a name,
/// because the *guest* resolves it — and the channel is then that
/// connection's byte pipe.
#[tokio::test]
async fn direct_tcpip_dials_from_inside_the_guest_and_carries_bytes() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h
        .session
        .channel_open_direct_tcpip("db.internal", 5432, "127.0.0.1", 51000)
        .await
        .unwrap();

    channel.data(&b"SELECT 1"[..]).await.unwrap();
    assert_eq!(read_data(&mut channel, 8).await, b"SELECT 1");

    assert_eq!(
        h.opens(),
        vec![Opened::Tunnel {
            host: "db.internal".into(),
            port: 5432,
        }]
    );
}

/// The acceptance criterion the ticket spends its words on: a failed
/// guest-side connect is `SSH_OPEN_CONNECT_FAILED`, **not**
/// `ADMINISTRATIVELY_PROHIBITED`. A SOCKS client has to tell "nothing is
/// listening" from "vmlab refused you", and the prohibited code is spent on
/// what vmlab genuinely refuses.
#[tokio::test]
async fn a_failed_guest_connect_is_connect_failed_and_not_prohibited() {
    let h = connect_as("dev", logins()).await.unwrap();
    let failure = h
        .session
        .channel_open_direct_tcpip("dead.internal", 5432, "127.0.0.1", 51000)
        .await
        .expect_err("a dead destination must not open a channel");

    match failure {
        // The code is the whole of what a client acts on, and it is the one
        // this test exists for. The guest's own words ride with it in the
        // failure's description — asserted against a real client in
        // [`a_real_ssh_stdio_forward_is_told_why_the_dial_failed`], because
        // russh's client keeps only the code for the four it recognises.
        russh::Error::ChannelOpenFailure(reason) => assert_eq!(
            reason.code(),
            russh::ChannelOpenFailure::ConnectFailed.code(),
            "a dead destination must not read as a refusal"
        ),
        other => panic!("expected a channel open failure, got {other:?}"),
    }

    // A closed port is ordinary — `ssh -D` dials whatever the developer's
    // tooling asks for — so it is not one of vmlab's refusals and does not
    // go on the lab event log.
    assert_eq!(h.refusals(), Vec::<Value>::new());
}

/// A port outside the TCP range is answered the same way, and the guest is
/// never asked to dial it: nothing can be listening there, which is a failed
/// connect rather than anything vmlab refuses.
#[tokio::test]
async fn a_port_outside_the_tcp_range_is_a_failed_connect() {
    let h = connect_as("dev", logins()).await.unwrap();
    let failure = h
        .session
        .channel_open_direct_tcpip("db.internal", 70_000, "127.0.0.1", 51000)
        .await
        .expect_err("70000 is not a port");

    match failure {
        russh::Error::ChannelOpenFailure(reason) => assert_eq!(
            reason.code(),
            russh::ChannelOpenFailure::ConnectFailed.code()
        ),
        other => panic!("expected a channel open failure, got {other:?}"),
    }
    assert_eq!(h.opens(), Vec::new());
}

/// Each direction ends on its own: the client's EOF shuts the guest socket's
/// write half, and the peer's answering FIN comes back as the channel's EOF
/// rather than as its close.
#[tokio::test]
async fn a_tunnel_half_closes_in_each_direction() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h
        .session
        .channel_open_direct_tcpip("db.internal", 5432, "127.0.0.1", 51000)
        .await
        .unwrap();
    channel.data(&b"ping"[..]).await.unwrap();
    assert_eq!(read_data(&mut channel, 4).await, b"ping");

    channel.eof().await.unwrap();
    let mut saw_eof = false;
    while let Some(msg) = channel.wait().await {
        if matches!(msg, russh::ChannelMsg::Eof) {
            saw_eof = true;
            break;
        }
    }
    assert!(saw_eof, "the guest's half-close never reached the client");
}

/// An agent too old to tunnel refuses `direct-tcpip` **by name**, telling the
/// developer what to do about it — and goes on serving a shell, because the
/// facade degrades per channel rather than per connection (§19.3).
#[tokio::test]
async fn direct_tcpip_refuses_by_name_without_the_tunnel_feature() {
    let h = connect_featured("dev", logins(), GuestOs::Linux, &["terminal", "exec"])
        .await
        .unwrap();
    let failure = h
        .session
        .channel_open_direct_tcpip("db.internal", 5432, "127.0.0.1", 51000)
        .await
        .expect_err("an agent with no tunnel must not open one");

    match failure {
        russh::Error::ChannelOpenFailure(reason) => assert_eq!(
            reason.code(),
            russh::ChannelOpenFailure::AdministrativelyProhibited.code(),
            "vmlab's own refusal is prohibited, not a connect failure"
        ),
        other => panic!("expected a channel open failure, got {other:?}"),
    }
    let reason = h.refusals()[0]["reason"].as_str().unwrap().to_string();
    assert!(reason.contains("repair-agent"), "{reason}");
    assert_eq!(h.refusals()[0]["request"], "direct-tcpip");

    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.request_shell(true).await.unwrap();
    assert_eq!(read_data(&mut channel, 8).await, b"prompt$ ");
}

/// A connection that dies takes every channel on it down in the guest —
/// tunnels included, and *because of* the tunnel rather than in spite of it.
///
/// This is the teardown the facade has instead of a close from the client:
/// the connection ending drops the facade, which drops every channel's way
/// to the guest, which is what winds each pump and its agent channel down.
/// A tunnel pumping in its own task must therefore hold no share of that
/// state — or one live `-D` stream would pin every idle shell's guest
/// process and every other tunnel's guest socket open until the guest itself
/// spoke.
#[tokio::test]
async fn a_lost_connection_closes_every_channel_in_the_guest() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut shell = h.session.channel_open_session().await.unwrap();
    shell.request_shell(true).await.unwrap();
    assert_eq!(read_data(&mut shell, 8).await, b"prompt$ ");
    let mut tunnel = h
        .session
        .channel_open_direct_tcpip("db.internal", 5432, "127.0.0.1", 51000)
        .await
        .unwrap();
    tunnel.data(&b"open"[..]).await.unwrap();
    assert_eq!(read_data(&mut tunnel, 4).await, b"open");

    let guest = h.agent_log.clone();
    drop((shell, tunnel, h));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if guest.closed.lock().unwrap().len() == 2 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the guest was left holding {:?} of 2 channels",
            guest.closed.lock().unwrap().len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Many tunnels ride one connection, and a dial in flight must not stall the
/// ones behind it: `ssh -D` puts a channel there per TCP connection the
/// developer's tooling makes, and a dead destination spends the guest's whole
/// dial budget. So the dial cannot own the session loop, and this is the
/// assertion that says so — the second channel opens and carries bytes while
/// the first is still connecting.
#[tokio::test]
async fn a_dial_in_flight_does_not_block_the_connection() {
    let h = connect_as("dev", logins()).await.unwrap();
    let slow = h
        .session
        .channel_open_direct_tcpip("slow.internal", 5432, "127.0.0.1", 51000);
    tokio::pin!(slow);
    // Get the dial in flight without waiting for it.
    tokio::select! {
        answer = &mut slow => panic!("the slow dial answered at once: {answer:?}"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }

    let live = tokio::time::timeout(SLOW_DIAL / 4, async {
        let mut channel = h
            .session
            .channel_open_direct_tcpip("db.internal", 5432, "127.0.0.1", 51001)
            .await
            .unwrap();
        channel.data(&b"still here"[..]).await.unwrap();
        read_data(&mut channel, 10).await
    })
    .await
    .expect("a second tunnel must not wait behind the first one's dial");
    assert_eq!(live, b"still here");

    assert!(slow.await.is_err(), "the slow dial must still fail");
}

// ---------------------------------------------------------------------------
// Refusals, and the invariant behind them
// ---------------------------------------------------------------------------

/// `ssh -R`: refused, and the reason recorded is the invariant — serving it
/// would need a guest-initiated channel, which the agent protocol does not
/// have (ADR-0013).
#[tokio::test]
async fn reverse_forwarding_is_refused_by_the_invariant() {
    let h = connect_as("dev", logins()).await.unwrap();
    assert!(h.session.tcpip_forward("localhost", 8080).await.is_err());

    let refusals = h.refusals();
    let reason = refusals[0]["reason"].as_str().unwrap();
    assert!(reason.contains("forwarded-tcpip"), "{reason}");
    assert!(reason.contains("guest-initiated"), "{reason}");
    assert_eq!(refusals[0]["machine"], "dev01");
}

/// Agent forwarding is the one refusal a client says nothing about —
/// `SSH_AUTH_SOCK` is simply empty in the guest — which is exactly why it
/// has to reach the lab event log.
#[tokio::test]
async fn agent_forwarding_is_refused_and_recorded() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.agent_forward(true).await.unwrap();
    expect_refused(&mut channel).await;

    let reason = h.refusals()[0]["reason"].as_str().unwrap().to_string();
    assert!(reason.contains("auth-agent@openssh.com"), "{reason}");
}

/// X11, for the same reason and by the same rule.
#[tokio::test]
async fn x11_is_refused_by_the_invariant() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel
        .request_x11(true, false, "MIT-MAGIC-COOKIE-1", "deadbeef", 0)
        .await
        .unwrap();
    expect_refused(&mut channel).await;
    assert!(
        h.refusals()[0]["reason"]
            .as_str()
            .unwrap()
            .contains("guest-initiated")
    );
}

/// `subsystem sftp` refuses by name until #88 builds it, so a client that
/// needs it fails legibly rather than hanging.
#[tokio::test]
async fn an_unserved_subsystem_is_refused_by_name() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    expect_refused(&mut channel).await;
    assert!(h.refusals()[0]["reason"].as_str().unwrap().contains("sftp"));
}

/// Keepalive is answered with `SSH_MSG_REQUEST_FAILURE`, which *is* the
/// correct answer and is what makes `ServerAliveInterval` work: the client
/// only needs a reply, not a success.
#[tokio::test]
async fn keepalive_is_answered_and_the_connection_survives_it() {
    let h = connect_as("dev", logins()).await.unwrap();
    let _ = h.session.send_keepalive(true).await;
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.request_shell(true).await.unwrap();
    assert_eq!(read_data(&mut channel, 8).await, b"prompt$ ");
}

/// `no-more-sessions@openssh.com` is accepted and ignored: sessions opened
/// after it still work, which is what "ignored" has to mean.
///
/// OpenSSH sends it with `want_reply` clear, which is what this asserts.
/// russh answers an unrecognised global request with
/// `SSH_MSG_REQUEST_FAILURE` either way and gives a server no hook to say
/// otherwise; the reply is spurious rather than wrong, and OpenSSH drops a
/// global reply it has nothing pending for.
#[tokio::test]
async fn no_more_sessions_does_not_close_the_door() {
    let h = connect_as("dev", logins()).await.unwrap();
    h.session.no_more_sessions(false).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.request_shell(true).await.unwrap();
    assert_eq!(read_data(&mut channel, 8).await, b"prompt$ ");
}

// ---------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------

/// One socket per proxy invocation, and it goes away with the connection.
///
/// This is what "nothing listens on the host" rests on: the only thing
/// bound is a unix socket the lab command just handed out, it accepts once,
/// and it is unlinked when the connection ends. Nothing survives the `ssh`
/// that asked for it.
#[tokio::test]
async fn the_socket_serves_one_connection_and_is_then_gone() {
    let (_agent_dir, agent, _log) = mock_agent().await;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ssh.sock");

    let spec = Arc::new(spec_for(logins(), GuestOs::Linux, Arc::new(|_, _| {})));
    expose_ssh_socket(spec, agent, sock.clone()).await.unwrap();
    assert!(sock.exists());

    {
        let stream = UnixStream::connect(&sock).await.unwrap();
        let config = Arc::new(client::Config::default());
        let mut session = client::connect_stream(config, stream, Client)
            .await
            .unwrap();
        assert!(session.authenticate_none("dev").await.unwrap().success());
        let channel = session.channel_open_session().await.unwrap();
        channel.request_shell(true).await.unwrap();
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while sock.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the socket outlived its connection"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// A real OpenSSH client
// ---------------------------------------------------------------------------

/// `none` cannot be talked out of, and the exit code comes back.
///
/// The claims §19.3 makes about `none` are claims about **OpenSSH**, not
/// about SSH: the opening `none` probe is unconditional because it is how
/// that client enumerates methods, so `BatchMode`,
/// `PreferredAuthentications` and `PasswordAuthentication=no` do not stop
/// it. Only the real binary can show that, so this test drives it.
#[tokio::test]
async fn a_real_openssh_client_authenticates_and_gets_an_exit_code() {
    let Some(facade) = openssh_reachable_facade(logins()).await else {
        return;
    };

    let out = facade
        .ssh()
        .args([
            // Everything a client could use to say "do not authenticate
            // like that", all at once.
            "-o",
            "BatchMode=yes",
            "-o",
            "PreferredAuthentications=publickey",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "NumberOfPasswordPrompts=0",
            "-o",
            "IdentitiesOnly=yes",
            "-l",
            "admin",
            "127.0.0.1",
            "printf out; exit 137",
        ])
        .output()
        .await
        .expect("run ssh");

    assert_eq!(
        out.status.code(),
        Some(137),
        "ssh exited {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    // `ssh` prints its own known-hosts notice first; what matters is that
    // the guest's stderr arrived on stderr and not folded into stdout.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.ends_with("err\n"), "{stderr}");

    // And the username was read as a selector all the way through.
    match &facade.opens()[0] {
        Opened::Exec { logon, argv, .. } => {
            assert_eq!(logon.as_ref().unwrap().user, r"PROBE\administrator");
            assert_eq!(argv[2], "printf out; exit 137");
        }
        other => panic!("{other:?}"),
    }
}

/// `ssh -D` in front of the facade carries SOCKS traffic to a guest port.
///
/// This is the shape §19.3 calls mandatory rather than convenient: VS Code
/// runs `ssh -T -D <port>` and rides its *entire* protocol over that dynamic
/// forward, so what this test drives is what makes the editor work at all.
/// Only the real binary can show it — the SOCKS server is OpenSSH's, not
/// vmlab's, and the destination it hands over is a **name**, which the guest
/// resolves.
#[tokio::test]
async fn a_real_ssh_dynamic_forward_carries_socks_traffic() {
    let Some(facade) = openssh_reachable_facade(logins()).await else {
        return;
    };
    let socks = free_port();
    let mut ssh = facade
        .ssh()
        .args([
            "-o",
            "ExitOnForwardFailure=yes",
            // `-T -N`: no terminal and no command, which is exactly the
            // control connection a remote-dev client opens.
            "-T",
            "-N",
            "-D",
            &socks.to_string(),
            "-l",
            "dev",
            "127.0.0.1",
        ])
        .kill_on_drop(true)
        .spawn()
        .expect("run ssh -D");

    let mut socks = connect_when_listening(socks).await;
    // SOCKS5, no authentication.
    socks.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut greeting = [0u8; 2];
    socks.read_exact(&mut greeting).await.unwrap();
    assert_eq!(greeting, [0x05, 0x00]);

    // CONNECT to a *name*, unresolved: the guest is what resolves it, which
    // is why the host string crosses the agent channel verbatim.
    let host = b"db.internal";
    let mut request = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    request.extend_from_slice(host);
    request.extend_from_slice(&5432u16.to_be_bytes());
    socks.write_all(&request).await.unwrap();
    let mut reply = [0u8; 10];
    socks.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[0], 0x05, "not a SOCKS5 reply: {reply:?}");

    socks.write_all(b"hello").await.unwrap();
    let mut echoed = [0u8; 5];
    socks.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"hello");

    assert_eq!(
        facade.opens(),
        vec![Opened::Tunnel {
            host: "db.internal".into(),
            port: 5432,
        }]
    );
    let _ = ssh.kill().await;
}

/// And when the dial fails, the developer is told which failure it was and
/// why, in the guest's own words.
///
/// `ssh -W` is a `direct-tcpip` on its own, which makes it the one place a
/// real client narrates the open failure: the reason code prints by name, so
/// "connect failed" rather than "administratively prohibited" is visible
/// rather than only true, and the description a channel open failure carries
/// is the only text SSH lets vmlab put in front of a developer here.
#[tokio::test]
async fn a_real_ssh_stdio_forward_is_told_why_the_dial_failed() {
    let Some(facade) = openssh_reachable_facade(logins()).await else {
        return;
    };
    let out = facade
        .ssh()
        .args([
            "-o",
            "LogLevel=VERBOSE",
            "-l",
            "dev",
            "-W",
            "dead.internal:5432",
            "127.0.0.1",
        ])
        .output()
        .await
        .expect("run ssh -W");

    assert!(!out.status.success(), "a dead destination must not forward");
    let shown = String::from_utf8_lossy(&out.stderr);
    assert!(shown.contains("connect failed"), "{shown}");
    assert!(shown.contains("connection refused"), "{shown}");
}

/// A loopback port nothing is using, for a test that has to name one.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// Connect to `port` once something is listening on it — `ssh -D` binds its
/// SOCKS listener a moment after the process starts.
async fn connect_when_listening(port: u16) -> tokio::net::TcpStream {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => return stream,
            Err(e) => assert!(
                tokio::time::Instant::now() < deadline,
                "nothing ever listened on {port}: {e}"
            ),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// The pieces, in isolation
// ---------------------------------------------------------------------------

/// The deny-list is a fixed set and it is matched case-insensitively —
/// `userprofile` from a client is the same variable as `USERPROFILE`.
#[test]
fn the_env_deny_list_holds_the_profile_path_variables() {
    for denied in ["HOME", "USERPROFILE", "userprofile", "LOGNAME", "USERNAME"] {
        assert!(!env_allowed(denied), "{denied} must be dropped");
    }
    for allowed in ["LANG", "LC_ALL", "TERM", "COLORTERM"] {
        assert!(env_allowed(allowed), "{allowed} must survive");
    }
}

/// The username is a selector: a declared label or account passes through,
/// and only the daemon's own account name means "nobody in particular".
#[test]
fn the_selector_reads_a_username_as_a_label() {
    let declared = logins();
    assert_eq!(
        selector_for("admin", &declared, Some("localdev")),
        Some("admin")
    );
    assert_eq!(
        selector_for(r"PROBE\dev", &declared, Some("localdev")),
        Some(r"PROBE\dev")
    );
    assert_eq!(selector_for("localdev", &declared, Some("localdev")), None);
    assert_eq!(selector_for("qa", &declared, Some("localdev")), Some("qa"));
    // A lab file that labels a login with the developer's own name wins: the
    // declaration is explicit, and the local username is only a fallback.
    let mine = vec![login("localdev", "dev", true)];
    assert_eq!(
        selector_for("localdev", &mine, Some("localdev")),
        Some("localdev")
    );
}
