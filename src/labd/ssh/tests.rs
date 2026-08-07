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
    AgentMsg, Frame, FrameDecoder, FrameKind, HostMsg, INITIAL_WINDOW, Logon, PROTO_VERSION,
    encode_ctrl, encode_frame,
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
}

#[derive(Default)]
struct AgentLog {
    opens: Mutex<Vec<Opened>>,
    /// Bytes the host sent towards the guest, per channel.
    input: Mutex<HashMap<u32, Vec<u8>>>,
}

/// A guest agent that answers terminals and execs.
///
/// A terminal echoes its keystrokes back — enough for a test to see bytes
/// travel both ways through the facade — and exits with the code named by
/// `exit <n>` typed into it. An exec writes a line to stdout, a line to
/// stderr, and exits with the code its command line ends in.
async fn mock_agent() -> (tempfile::TempDir, AgentHandle, Arc<AgentLog>) {
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
                                    features: vec!["terminal".into(), "exec".into()],
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
    let (agent_dir, agent, agent_log) = mock_agent().await;
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
    let out = tokio::process::Command::new("ssh")
        .args([
            "-p",
            &facade.port.to_string(),
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "BatchMode=yes",
            "-l",
            "qa",
            "127.0.0.1",
            "true",
        ])
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

    let out = tokio::process::Command::new("ssh")
        .args([
            "-p",
            &facade.port.to_string(),
            // vmlab owns its own known_hosts; a test must not touch the
            // developer's, and this is not what is under test.
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
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
    match &facade.agent_log.opens.lock().unwrap()[0] {
        Opened::Exec { logon, argv, .. } => {
            assert_eq!(logon.as_ref().unwrap().user, r"PROBE\administrator");
            assert_eq!(argv[2], "printf out; exit 137");
        }
        other => panic!("{other:?}"),
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
