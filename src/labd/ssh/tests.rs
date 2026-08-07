//! The facade end to end: a real SSH client, over a socket pair, against a
//! mock agent.
//!
//! Driving it with `russh`'s own client is the point — the contract §19.3
//! writes down is what an SSH client observes, not what the handler was
//! called with, and only a client can tell "the request was answered" from
//! "the request was answered the way SSH means it".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use vmlab_agent_proto::fileops;
use vmlab_agent_proto::{
    AgentMsg, ErrorCause, Frame, FrameDecoder, FrameKind, HostMsg, INITIAL_WINDOW, Logon,
    PROTO_VERSION, encode_ctrl, encode_frame,
};

use super::*;
use crate::config::model::Login;
use crate::labd::vm_agent::{AgentHandle, Attrs, EntryKind, ErrorCode, Op, Reply};

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
    /// A `fileops` session — what `subsystem sftp` opens, and the record that
    /// shows whose files it is about to touch (§19.2).
    FileOps {
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

/// Whether the mock guest gives a file session credit for more bytes.
///
/// Withheld is the guest that has stopped keeping up: it takes what it is
/// given and grants nothing back. That is the stall the facade must not buffer
/// through — the two stacked flow-control layers have to stay stacked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Credit {
    Granted,
    Withheld,
}

/// The directory a mock guest's files live in — a real one, so a test can run
/// the actual `scp` and `sftp` binaries and then look at what landed.
fn guest_root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("guest")
}

/// A guest agent that answers terminals, execs, tunnels and file sessions.
///
/// A terminal echoes its keystrokes back — enough for a test to see bytes
/// travel both ways through the facade — and exits with the code named by
/// `exit <n>` typed into it. An exec writes a line to stdout, a line to
/// stderr, and exits with the code its command line ends in. A tunnel is an
/// echo peer that half-closes when its client does, unless its destination is
/// named `dead.<something>` or `slow.<something>` — the dial then fails the
/// way a dead address does, at once or after spending a budget. A `fileops`
/// session is served against a **real directory**, because a transcode can
/// only be checked against the clients it exists for, and those clients want
/// files.
async fn mock_agent() -> (tempfile::TempDir, AgentHandle, Arc<AgentLog>) {
    mock_agent_with(EVERY_FEATURE).await
}

/// Everything this facade asks a guest for.
const EVERY_FEATURE: &[&str] = &["terminal", "exec", "tunnel", "fileops"];

/// The same guest, declaring only `features` — which is how a test stands up
/// an agent too old to serve one of the vocabularies (§19.4).
async fn mock_agent_with(features: &[&str]) -> (tempfile::TempDir, AgentHandle, Arc<AgentLog>) {
    mock_agent_like(features, Credit::Granted).await
}

async fn mock_agent_like(
    features: &[&str],
    credit: Credit,
) -> (tempfile::TempDir, AgentHandle, Arc<AgentLog>) {
    let features: Vec<String> = features.iter().map(|f| f.to_string()).collect();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(guest_root(dir.path())).expect("guest root");
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
        // One file session per channel, exactly as the guest scopes them.
        let mut files: HashMap<u32, MockFiles> = HashMap::new();
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
                            HostMsg::OpenFileOps { id, logon } => {
                                served.opens.lock().unwrap().push(Opened::FileOps { logon });
                                files.insert(id, MockFiles::default());
                                send(AgentMsg::Opened { id }).await;
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
                    FrameKind::Data if files.contains_key(&channel) => {
                        let session = files.get_mut(&channel).expect("the session just matched");
                        // A guest with no credit to give takes the bytes and
                        // says nothing: the facade must stall against it
                        // rather than absorb the difference.
                        if credit == Credit::Withheld {
                            continue;
                        }
                        // Credit back what was consumed, like the agent's own
                        // reader does — a transfer is many times the initial
                        // window, so a mock that never granted would stall.
                        send(AgentMsg::WindowAdjust {
                            id: channel,
                            bytes: payload.len() as u64,
                        })
                        .await;
                        for (response, bytes) in session.serve(&payload) {
                            for chunk in fileops::encode_record(&response, &bytes).chunks(4096) {
                                send_data(FrameKind::Data, channel, chunk.to_vec()).await;
                            }
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

// ---------------------------------------------------------------------------
// The mock agent's file session
// ---------------------------------------------------------------------------

/// One open `fileops` channel on the mock, served against the real
/// filesystem.
///
/// Real files rather than an in-memory map: the transcode's whole job is to be
/// what `scp` and an editor expect, and the only way to check that is to let
/// those binaries move actual files and then look at what is on disk.
///
/// It answers a batch of framed requests in **reverse** order, so a transcode
/// that assumed replies come back in the order it asked would fail here rather
/// than against a guest.
#[derive(Default)]
struct MockFiles {
    decoder: fileops::RecordDecoder,
    handles: HashMap<u64, MockHandle>,
    next_handle: u64,
}

enum MockHandle {
    File(std::fs::File),
    Dir {
        entries: Vec<fileops::DirEntry>,
        at: usize,
    },
}

impl MockFiles {
    fn serve(&mut self, bytes: &[u8]) -> Vec<(fileops::Response, Vec<u8>)> {
        self.decoder.push(bytes);
        let mut out = Vec::new();
        while let Some((request, payload)) = self
            .decoder
            .next_record::<fileops::Request>()
            .expect("framed request")
        {
            let (reply, bytes) = self.op(request.op, payload);
            out.push((
                fileops::Response {
                    id: request.id,
                    reply,
                },
                bytes,
            ));
        }
        out.reverse();
        out
    }

    fn insert(&mut self, handle: MockHandle) -> Reply {
        self.next_handle += 1;
        self.handles.insert(self.next_handle, handle);
        Reply::Handle {
            handle: self.next_handle,
        }
    }

    fn file(&self, handle: u64) -> Result<&std::fs::File, Reply> {
        match self.handles.get(&handle) {
            Some(MockHandle::File(file)) => Ok(file),
            _ => Err(Reply::Error {
                code: ErrorCode::BadHandle,
                msg: "no such handle on this channel".into(),
            }),
        }
    }

    fn op(&mut self, op: Op, payload: Vec<u8>) -> (Reply, Vec<u8>) {
        use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};

        let plain = |reply| (reply, Vec::new());
        // The path goes in the message, as the guest's own does: a client
        // pipelining 64 requests cannot tell from the id alone which file
        // "permission denied" was about.
        let attempt = |path: &str, r: std::io::Result<()>| match r {
            Ok(()) => (Reply::Ok, Vec::new()),
            Err(e) => (failed_at(path, &e), Vec::new()),
        };
        match op {
            Op::Open { path, flags, mode } => {
                let mut opts = std::fs::OpenOptions::new();
                opts.read(flags.read || !(flags.write || flags.append))
                    .write(flags.write)
                    .append(flags.append)
                    .truncate(flags.truncate)
                    .create(flags.create && !flags.exclusive)
                    .create_new(flags.exclusive);
                if let Some(mode) = mode {
                    opts.mode(mode);
                }
                match opts.open(&path) {
                    Ok(file) => plain(self.insert(MockHandle::File(file))),
                    Err(e) => plain(failed_at(&path, &e)),
                }
            }
            Op::Close { handle } => match self.handles.remove(&handle) {
                Some(_) => plain(Reply::Ok),
                None => plain(Reply::Error {
                    code: ErrorCode::BadHandle,
                    msg: "no such handle on this channel".into(),
                }),
            },
            Op::Read {
                handle,
                offset,
                len,
            } => match self.file(handle) {
                Err(e) => plain(e),
                Ok(file) => {
                    let mut buf = vec![0u8; len as usize];
                    let mut done = 0;
                    while done < buf.len() {
                        match file.read_at(&mut buf[done..], offset + done as u64) {
                            Ok(0) => break,
                            Ok(n) => done += n,
                            Err(e) => return plain(failed(&e)),
                        }
                    }
                    buf.truncate(done);
                    (Reply::Data, buf)
                }
            },
            Op::Write { handle, offset } => match self.file(handle) {
                Err(e) => plain(e),
                Ok(file) => attempt("", file.write_all_at(&payload, offset)),
            },
            Op::Fstat { handle } => match self.file(handle) {
                Err(e) => plain(e),
                Ok(file) => match file.metadata() {
                    Ok(meta) => plain(Reply::Attrs {
                        attrs: attrs_of(&meta),
                    }),
                    Err(e) => plain(failed(&e)),
                },
            },
            Op::Fsetstat { handle, attrs } => match self.file(handle) {
                Err(e) => plain(e),
                Ok(file) => {
                    if let Some(size) = attrs.size
                        && let Err(e) = file.set_len(size)
                    {
                        return plain(failed(&e));
                    }
                    if let Some(mode) = attrs.mode
                        && let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(mode))
                    {
                        return plain(failed(&e));
                    }
                    attempt("", set_times(file, attrs.atime_ns, attrs.mtime_ns))
                }
            },
            Op::Stat { path } => match std::fs::metadata(&path) {
                Ok(meta) => plain(Reply::Attrs {
                    attrs: attrs_of(&meta),
                }),
                Err(e) => plain(failed_at(&path, &e)),
            },
            Op::Lstat { path } => match std::fs::symlink_metadata(&path) {
                Ok(meta) => plain(Reply::Attrs {
                    attrs: attrs_of(&meta),
                }),
                Err(e) => plain(failed_at(&path, &e)),
            },
            Op::Setstat { path, attrs } => {
                if let Some(mode) = attrs.mode
                    && let Err(e) =
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                {
                    return plain(failed_at(&path, &e));
                }
                if attrs.size.is_none() && attrs.mtime_ns.is_none() && attrs.atime_ns.is_none() {
                    return plain(Reply::Ok);
                }
                match std::fs::OpenOptions::new().write(true).open(&path) {
                    Err(e) => plain(failed_at(&path, &e)),
                    Ok(file) => {
                        if let Some(size) = attrs.size
                            && let Err(e) = file.set_len(size)
                        {
                            return plain(failed_at(&path, &e));
                        }
                        attempt(&path, set_times(&file, attrs.atime_ns, attrs.mtime_ns))
                    }
                }
            }
            Op::OpenDir { path } => match std::fs::read_dir(&path) {
                Err(e) => plain(failed_at(&path, &e)),
                Ok(iter) => {
                    let entries = iter
                        .flatten()
                        .filter_map(|entry| {
                            let meta = entry.metadata().ok()?;
                            Some(fileops::DirEntry {
                                name: entry.file_name().to_string_lossy().into_owned(),
                                attrs: attrs_of(&meta),
                            })
                        })
                        .collect();
                    plain(self.insert(MockHandle::Dir { entries, at: 0 }))
                }
            },
            Op::ReadDir { handle } => match self.handles.get_mut(&handle) {
                Some(MockHandle::Dir { entries, at }) => {
                    let end = (*at + fileops::READDIR_CHUNK).min(entries.len());
                    let slice = entries[*at..end].to_vec();
                    *at = end;
                    plain(Reply::Entries {
                        entries: slice,
                        eof: end == entries.len(),
                    })
                }
                _ => plain(Reply::Error {
                    code: ErrorCode::BadHandle,
                    msg: "not a directory handle".into(),
                }),
            },
            Op::Mkdir { path, mode, .. } => {
                if let Err(e) = std::fs::create_dir(&path) {
                    return plain(failed_at(&path, &e));
                }
                match mode {
                    Some(mode) => attempt(
                        &path,
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)),
                    ),
                    None => plain(Reply::Ok),
                }
            }
            Op::Rmdir { path } => attempt(&path, std::fs::remove_dir(&path)),
            Op::Remove { path } => attempt(&path, std::fs::remove_file(&path)),
            Op::Rename { from, to } => attempt(&from, std::fs::rename(&from, &to)),
            Op::Realpath { path } => plain(Reply::Name {
                path: std::fs::canonicalize(&path)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(path),
            }),
            Op::Symlink { target, link, .. } => {
                attempt(&link, std::os::unix::fs::symlink(&target, &link))
            }
            Op::Readlink { path } => match std::fs::read_link(&path) {
                Ok(target) => plain(Reply::Name {
                    path: target.to_string_lossy().into_owned(),
                }),
                Err(e) => plain(failed_at(&path, &e)),
            },
            Op::Digest { path } => match std::fs::read(&path) {
                Ok(bytes) => {
                    use sha2::{Digest, Sha256};
                    plain(Reply::Digest {
                        sha256: hex::encode(Sha256::digest(&bytes)),
                        len: bytes.len() as u64,
                    })
                }
                Err(e) => plain(failed_at(&path, &e)),
            },
        }
    }
}

fn failed(e: &std::io::Error) -> Reply {
    Reply::Error {
        code: ErrorCode::of(e),
        msg: e.to_string(),
    }
}

/// The same, naming the path — which is what the guest's own file session
/// does, and what a client pipelining 64 requests needs to tell them apart.
fn failed_at(path: &str, e: &std::io::Error) -> Reply {
    Reply::Error {
        code: ErrorCode::of(e),
        msg: match path.is_empty() {
            true => e.to_string(),
            false => format!("{path}: {e}"),
        },
    }
}

fn set_times(
    file: &std::fs::File,
    atime_ns: Option<i64>,
    mtime_ns: Option<i64>,
) -> std::io::Result<()> {
    if atime_ns.is_none() && mtime_ns.is_none() {
        return Ok(());
    }
    let at = |ns: i64| std::time::UNIX_EPOCH + Duration::from_nanos(ns.unsigned_abs());
    let mut times = std::fs::FileTimes::new();
    if let Some(ns) = atime_ns {
        times = times.set_accessed(at(ns));
    }
    if let Some(ns) = mtime_ns {
        times = times.set_modified(at(ns));
    }
    file.set_times(times)
}

fn attrs_of(meta: &std::fs::Metadata) -> Attrs {
    use std::os::unix::fs::PermissionsExt;
    let nanos = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
    };
    Attrs {
        kind: if meta.is_dir() {
            EntryKind::Dir
        } else if meta.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        },
        size: meta.len(),
        mtime_ns: nanos(meta.modified()),
        atime_ns: nanos(meta.accessed()),
        mode: Some(meta.permissions().mode() & 0o7777),
    }
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
    agent_dir: tempfile::TempDir,
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
    connect_like(username, logins, guest_os, features, Credit::Granted).await
}

/// The same, against a guest that takes a file session's bytes and grants
/// credit for none — the stall the facade must not buffer through (§19.3).
async fn connect_stalled(username: &str, logins: Vec<Login>) -> anyhow::Result<Harness> {
    connect_like(
        username,
        logins,
        GuestOs::Linux,
        EVERY_FEATURE,
        Credit::Withheld,
    )
    .await
}

async fn connect_like(
    username: &str,
    logins: Vec<Login>,
    guest_os: GuestOs,
    features: &[&str],
    credit: Credit,
) -> anyhow::Result<Harness> {
    let (agent_dir, agent, agent_log) = mock_agent_like(features, credit).await;
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
        agent_dir,
        agent_log,
        events,
        session,
    })
}

impl Harness {
    /// The directory the mock guest's files live in.
    fn guest(&self) -> PathBuf {
        guest_root(self.agent_dir.path())
    }

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
    if openssh_version().is_none() {
        eprintln!("no ssh(1) on this host — skipping the OpenSSH interop test");
        return None;
    }
    let (agent_dir, agent, agent_log) = mock_agent().await;
    let spec = Arc::new(spec_for(logins, GuestOs::Linux, Arc::new(|_, _| {})));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let port = listener.local_addr().ok()?.port();
    tokio::spawn(async move {
        // One connection per client invocation, and a `scp` up followed by a
        // `scp` down is two — served against the same mock guest, so the
        // second sees what the first wrote.
        while let Ok((stream, _)) = listener.accept().await {
            let spec = spec.clone();
            let agent = agent.clone();
            tokio::spawn(async move {
                let _ = serve_connection(spec, agent, stream).await;
            });
        }
    });
    Some(RealSsh {
        port,
        agent_log,
        agent_dir,
    })
}

/// The major version of the `ssh(1)` on this host, or `None` where there is
/// none. `scp` speaks SFTP by default from OpenSSH 9.0, which is what makes
/// `scp` a test of this transcode at all.
fn openssh_version() -> Option<u32> {
    // `ssh -V` prints to stderr.
    let out = std::process::Command::new("ssh").arg("-V").output().ok()?;
    let shown = String::from_utf8_lossy(&out.stderr).into_owned();
    shown
        .split("OpenSSH_")
        .nth(1)?
        .split(['.', 'p', ' '])
        .next()?
        .parse()
        .ok()
}

struct RealSsh {
    port: u16,
    agent_log: Arc<AgentLog>,
    agent_dir: tempfile::TempDir,
}

impl RealSsh {
    /// The directory the mock guest's files live in.
    fn guest(&self) -> PathBuf {
        guest_root(self.agent_dir.path())
    }

    /// The options that keep a test off the developer's own `known_hosts`.
    fn args(&self, port_flag: &str) -> Vec<String> {
        vec![
            port_flag.into(),
            self.port.to_string(),
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "BatchMode=yes".into(),
        ]
    }
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
    assert_eq!(h.refusals()[0]["request"], "direct-tcpip");
    // §19.4's words: what is missing, and *both* remedies — the rebuild that
    // is policy and the repair verb that is a tool, aimed at this machine.
    assert!(reason.contains("`direct-tcpip`"), "{reason}");
    assert!(reason.contains("serves no `tunnel`"), "{reason}");
    assert!(reason.contains("rebuild the template"), "{reason}");
    assert!(reason.contains("repair-agent dev01"), "{reason}");

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

/// `sftp` is the only subsystem the facade serves; anything else is refused
/// **by name**, so a client that needs one fails legibly rather than hanging.
#[tokio::test]
async fn an_unserved_subsystem_is_refused_by_name() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "netconf").await.unwrap();
    expect_refused(&mut channel).await;
    assert!(
        h.refusals()[0]["reason"]
            .as_str()
            .unwrap()
            .contains("netconf")
    );
}

// ---------------------------------------------------------------------------
// A stale agent (§19.4)
// ---------------------------------------------------------------------------

/// **The facade degrades per channel.** A machine whose agent predates §19 is
/// still a perfectly good machine, and a shell on it is exactly as good as any
/// other — nothing about a terminal needs `fileops` or `tunnel`.
#[tokio::test]
async fn a_stale_agent_still_serves_a_shell() {
    let h = connect_featured("dev", logins(), GuestOs::Linux, &["terminal", "exec"])
        .await
        .unwrap();
    let mut channel = h.session.channel_open_session().await.unwrap();
    channel.request_shell(true).await.unwrap();
    assert_eq!(read_data(&mut channel, 8).await, b"prompt$ ");
    assert!(h.refusals().is_empty(), "{:?}", h.refusals());
}

// What such an agent genuinely cannot serve is refused by name, with both
// remedies in the reason: `sftp` in
// `an_agent_without_fileops_still_serves_a_shell_and_refuses_sftp_by_name`,
// and `direct-tcpip` in
// `direct_tcpip_refuses_by_name_without_the_tunnel_feature` — each beside the
// channel it is about rather than duplicated here.

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
// `subsystem sftp`, transcoded onto `fileops`
// ---------------------------------------------------------------------------

use super::sftp::{kind, status};

/// A minimal SFTP client over one channel: enough to send a packet and read
/// the reply, so a test can assert on the wire the facade actually writes.
///
/// The real `scp` and `sftp` binaries drive the transcode further down this
/// file; this is for the answers only a hand-built packet can ask for — a
/// Windows drive letter on a Linux test host, an extension no client of ours
/// sends, a push with no reader.
struct Sftp {
    stream: russh::ChannelStream<client::Msg>,
    next_id: u32,
}

impl Sftp {
    /// Open `subsystem sftp` and do the version handshake.
    async fn open(session: &client::Handle<Client>) -> Sftp {
        let channel = session.channel_open_session().await.unwrap();
        channel.request_subsystem(true, "sftp").await.unwrap();
        let mut sftp = Sftp {
            stream: channel.into_stream(),
            next_id: 0,
        };
        let mut body = Vec::new();
        put_u32(&mut body, 3);
        sftp.write(kind::INIT, &body).await;
        let (kind, body) = sftp.reply().await;
        assert_eq!(kind, super::sftp::kind::VERSION);
        assert_eq!(u32::from_be_bytes(body[..4].try_into().unwrap()), 3);
        sftp
    }

    async fn write(&mut self, kind: u8, body: &[u8]) {
        let mut packet = Vec::new();
        put_u32(&mut packet, (body.len() + 1) as u32);
        packet.push(kind);
        packet.extend_from_slice(body);
        self.stream.write_all(&packet).await.unwrap();
    }

    /// One reply packet: its type, and everything after it.
    async fn reply(&mut self) -> (u8, Vec<u8>) {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).await.unwrap();
        let mut packet = vec![0u8; u32::from_be_bytes(len) as usize];
        self.stream.read_exact(&mut packet).await.unwrap();
        (packet[0], packet[1..].to_vec())
    }

    /// One request, one reply. The id is the caller's business only when it
    /// is checking that the reply echoes it, which [`Sftp::reply`] leaves
    /// visible in the body.
    async fn ask(&mut self, kind: u8, args: &[u8]) -> (u8, Vec<u8>) {
        self.next_id += 1;
        let mut body = Vec::new();
        put_u32(&mut body, self.next_id);
        body.extend_from_slice(args);
        self.write(kind, &body).await;
        self.reply().await
    }

    /// A request whose only acceptable answer is a handle.
    async fn handle(&mut self, kind: u8, args: &[u8]) -> Vec<u8> {
        match self.ask(kind, args).await {
            (kind::HANDLE, body) => body[8..].to_vec(),
            (kind, body) => panic!("expected a handle, got type {kind}: {body:?}"),
        }
    }

    /// The status code a reply carries, and the words with it.
    async fn status(&mut self, kind: u8, args: &[u8]) -> (u32, String) {
        match self.ask(kind, args).await {
            (kind::STATUS, body) => (
                u32::from_be_bytes(body[4..8].try_into().unwrap()),
                read_str(&body[8..]).0,
            ),
            (kind, body) => panic!("expected a status, got type {kind}: {body:?}"),
        }
    }
}

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

/// One argument list holding just a path. A request that also carries an
/// attribute block appends its own.
fn path_args(path: &Path) -> Vec<u8> {
    let mut args = Vec::new();
    put_str(&mut args, &path.to_string_lossy());
    args
}

/// A length-prefixed string, and where it ended.
fn read_str(bytes: &[u8]) -> (String, usize) {
    let len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    (
        String::from_utf8_lossy(&bytes[4..4 + len]).into_owned(),
        4 + len,
    )
}

/// The names a `NAME` reply carries: `(filename, longname)` per entry.
fn read_names(body: &[u8]) -> Vec<(String, String)> {
    let count = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
    let mut at = 8;
    let mut names = Vec::new();
    for _ in 0..count {
        let (name, used) = read_str(&body[at..]);
        at += used;
        let (long, used) = read_str(&body[at..]);
        at += used;
        // The attribute block, skipped: only its flags are fixed here, and
        // every field this facade sets is checked in `read_attrs`.
        let flags = u32::from_be_bytes(body[at..at + 4].try_into().unwrap());
        at += 4 + attr_len(flags);
        names.push((name, long));
    }
    names
}

/// How many bytes of attribute block follow the flags word.
fn attr_len(flags: u32) -> usize {
    let mut len = 0;
    if flags & 0x1 != 0 {
        len += 8; // size
    }
    if flags & 0x2 != 0 {
        len += 8; // uid + gid
    }
    if flags & 0x4 != 0 {
        len += 4; // permissions
    }
    if flags & 0x8 != 0 {
        len += 8; // atime + mtime
    }
    len
}

/// `(size, permissions)` out of an `ATTRS` reply.
fn read_attrs(body: &[u8]) -> (u64, u32) {
    let flags = u32::from_be_bytes(body[4..8].try_into().unwrap());
    assert_eq!(flags & 0x1, 0x1, "size is always reported");
    assert_eq!(flags & 0x4, 0x4, "permissions are always reported");
    (
        u64::from_be_bytes(body[8..16].try_into().unwrap()),
        u32::from_be_bytes(body[16..20].try_into().unwrap()),
    )
}

/// A `write` packet's arguments, for the tests that push bytes by hand.
fn write_args(handle: &[u8], offset: u64, data: &[u8]) -> Vec<u8> {
    let mut args = Vec::new();
    put_bytes(&mut args, handle);
    put_u64(&mut args, offset);
    put_bytes(&mut args, data);
    args
}

/// **The property this ticket exists for** (§19.2): file operations resolve
/// the same (account, secret) as the shell.
///
/// One connection, a shell channel and an sftp channel, and the *same* logon
/// on both opens — which is what makes them land on one cached logon, one
/// `LogonId` and one view of mapped drives. It is true by construction here
/// rather than by discipline: there is one resolved logon on a connection and
/// both opens carry it.
#[tokio::test]
async fn file_operations_run_under_the_same_logon_as_the_shell() {
    let h = connect_as("dev", logins()).await.unwrap();
    let channel = h.session.channel_open_session().await.unwrap();
    channel.request_shell(true).await.unwrap();
    let shell = h
        .wait_for_open(|o| matches!(o, Opened::Terminal { .. }))
        .await;

    let _sftp = Sftp::open(&h.session).await;
    let files = h
        .wait_for_open(|o| matches!(o, Opened::FileOps { .. }))
        .await;

    match (shell, files) {
        (Opened::Terminal { logon: shell, .. }, Opened::FileOps { logon: files }) => {
            assert_eq!(shell.as_ref().unwrap().user, r"PROBE\dev");
            assert_eq!(shell, files, "sftp must be the shell's logon, not another");
        }
        other => panic!("{other:?}"),
    }
}

/// And a `-l admin` connection's file session is *that* identity — the
/// selector reaches the file session the same way it reaches the shell.
#[tokio::test]
async fn a_login_label_selects_the_file_sessions_identity_too() {
    let h = connect_as("admin", logins()).await.unwrap();
    let _sftp = Sftp::open(&h.session).await;
    match h
        .wait_for_open(|o| matches!(o, Opened::FileOps { .. }))
        .await
    {
        Opened::FileOps { logon } => assert_eq!(logon.unwrap().user, r"PROBE\administrator"),
        other => panic!("{other:?}"),
    }
}

/// §19.3's per-channel degradation: an agent too old for `fileops` still
/// serves a shell, and `subsystem sftp` refuses **by name**, naming the
/// capability and the repair verb rather than leaving an editor to hang.
#[tokio::test]
async fn an_agent_without_fileops_still_serves_a_shell_and_refuses_sftp_by_name() {
    let h = connect_featured(
        "dev",
        logins(),
        GuestOs::Linux,
        &["terminal", "exec", "tunnel"],
    )
    .await
    .unwrap();

    let mut sftp = h.session.channel_open_session().await.unwrap();
    sftp.request_subsystem(true, "sftp").await.unwrap();
    expect_refused(&mut sftp).await;

    let refusal = h.refusals()[0].clone();
    assert_eq!(refusal["request"], "subsystem sftp");
    let reason = refusal["reason"].as_str().unwrap();
    assert!(reason.contains("fileops"), "{reason}");
    assert!(reason.contains("repair-agent"), "{reason}");

    // The shell is untouched: the facade degrades per channel, not per
    // connection.
    let mut shell = h.session.channel_open_session().await.unwrap();
    shell.request_shell(true).await.unwrap();
    assert_eq!(read_data(&mut shell, 8).await, b"prompt$ ");
}

/// The whole vocabulary §19.5 names `fileops` for, driven from the client's
/// side and checked against what actually landed in the guest's directory.
#[tokio::test]
async fn the_transcode_covers_the_vocabulary_a_client_issues() {
    let h = connect_as("dev", logins()).await.unwrap();
    let root = h.guest();
    let mut sftp = Sftp::open(&h.session).await;

    // realpath, which is the first thing every client sends.
    let (kind, body) = sftp.ask(kind::REALPATH, &path_args(&root)).await;
    assert_eq!(kind, super::sftp::kind::NAME);
    assert_eq!(read_names(&body)[0].0, root.to_string_lossy());

    // open → write → write → close, at explicit offsets.
    let file = root.join("hello.txt");
    let mut args = path_args(&file);
    put_u32(&mut args, 0x2 | 0x8 | 0x10); // write | create | truncate
    put_u32(&mut args, 0x4); // permissions follow
    put_u32(&mut args, 0o640);
    let handle = sftp.handle(kind::OPEN, &args).await;
    assert_eq!(
        sftp.status(kind::WRITE, &write_args(&handle, 0, b"hello "))
            .await
            .0,
        status::OK
    );
    assert_eq!(
        sftp.status(kind::WRITE, &write_args(&handle, 6, b"world"))
            .await
            .0,
        status::OK
    );
    let mut close = Vec::new();
    put_bytes(&mut close, &handle);
    assert_eq!(sftp.status(kind::CLOSE, &close).await.0, status::OK);
    assert_eq!(std::fs::read(&file).unwrap(), b"hello world");

    // stat: the size and the mode the open asked for, with the file type put
    // back into the permission bits a client branches on.
    let (kind, body) = sftp.ask(kind::STAT, &path_args(&file)).await;
    assert_eq!(kind, super::sftp::kind::ATTRS);
    assert_eq!(read_attrs(&body), (11, 0o100_640));

    // read, and the empty read past the end that is how a client learns where
    // the file stopped.
    let mut args = path_args(&file);
    put_u32(&mut args, 0x1); // read
    put_u32(&mut args, 0); // no attributes
    let handle = sftp.handle(kind::OPEN, &args).await;
    let mut read = Vec::new();
    put_bytes(&mut read, &handle);
    put_u64(&mut read, 0);
    put_u32(&mut read, 4096);
    let (kind, body) = sftp.ask(super::sftp::kind::READ, &read).await;
    assert_eq!(kind, super::sftp::kind::DATA);
    assert_eq!(read_str(&body[4..]).0, "hello world");
    let mut read = Vec::new();
    put_bytes(&mut read, &handle);
    put_u64(&mut read, 11);
    put_u32(&mut read, 4096);
    assert_eq!(sftp.status(kind::READ, &read).await.0, status::EOF);
    let mut close = Vec::new();
    put_bytes(&mut close, &handle);
    sftp.status(kind::CLOSE, &close).await;

    // mkdir, rename, and a directory listing that reports what moved into it.
    let dir = root.join("src");
    let mut args = path_args(&dir);
    put_u32(&mut args, 0x4);
    put_u32(&mut args, 0o755);
    assert_eq!(sftp.status(kind::MKDIR, &args).await.0, status::OK);
    let moved = dir.join("hello.txt");
    let mut args = path_args(&file);
    args.extend(path_args(&moved));
    assert_eq!(sftp.status(kind::RENAME, &args).await.0, status::OK);

    let handle = sftp.handle(kind::OPENDIR, &path_args(&dir)).await;
    let mut read = Vec::new();
    put_bytes(&mut read, &handle);
    let (kind, body) = sftp.ask(super::sftp::kind::READDIR, &read).await;
    assert_eq!(kind, super::sftp::kind::NAME);
    let entries = read_names(&body);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "hello.txt");
    assert!(entries[0].1.starts_with("-rw-r-----"), "{}", entries[0].1);
    // The listing ends with a status, because version 3 has no `eof` field.
    let mut read = Vec::new();
    put_bytes(&mut read, &handle);
    assert_eq!(sftp.status(kind::READDIR, &read).await.0, status::EOF);
    let mut close = Vec::new();
    put_bytes(&mut close, &handle);
    sftp.status(kind::CLOSE, &close).await;

    // symlink and readlink. OpenSSH sends the target first — the reverse of
    // the draft — and a link whose target is a file gets a file link.
    let link = dir.join("link.txt");
    let mut args = path_args(&moved);
    args.extend(path_args(&link));
    assert_eq!(sftp.status(kind::SYMLINK, &args).await.0, status::OK);
    let (kind, body) = sftp
        .ask(super::sftp::kind::READLINK, &path_args(&link))
        .await;
    assert_eq!(kind, super::sftp::kind::NAME);
    assert_eq!(read_names(&body)[0].0, moved.to_string_lossy());
    // lstat sees the link itself; stat follows it.
    let (_, body) = sftp.ask(kind::LSTAT, &path_args(&link)).await;
    assert_eq!(read_attrs(&body).1 & 0o170_000, 0o120_000);
    let (_, body) = sftp.ask(kind::STAT, &path_args(&link)).await;
    assert_eq!(read_attrs(&body).1 & 0o170_000, 0o100_000);

    // setstat, then remove and rmdir — and the failure a client must be able
    // to tell apart from every other failure.
    let mut args = path_args(&moved);
    put_u32(&mut args, 0x4);
    put_u32(&mut args, 0o600);
    assert_eq!(sftp.status(kind::SETSTAT, &args).await.0, status::OK);
    let (_, body) = sftp.ask(kind::STAT, &path_args(&moved)).await;
    assert_eq!(read_attrs(&body).1, 0o100_600);

    assert_eq!(
        sftp.status(kind::REMOVE, &path_args(&link)).await.0,
        status::OK
    );
    assert_eq!(
        sftp.status(kind::REMOVE, &path_args(&moved)).await.0,
        status::OK
    );
    assert_eq!(
        sftp.status(kind::RMDIR, &path_args(&dir)).await.0,
        status::OK
    );
    assert!(!dir.exists());

    let (code, msg) = sftp.status(kind::STAT, &path_args(&moved)).await;
    assert_eq!(code, status::NO_SUCH_FILE);
    assert!(msg.contains("hello.txt"), "{msg}");
}

/// `realpath` is the guest's answer, carried back verbatim — which is the
/// whole reason §19.5 says the facade *transcodes* rather than adapts. A tidier
/// abstraction discovers at this exact point that it cannot express a Windows
/// drive letter.
#[tokio::test]
async fn realpath_carries_a_windows_drive_letter_back_verbatim() {
    let h = connect_with("dev", logins(), GuestOs::Windows)
        .await
        .unwrap();
    // The mock answers `realpath` with what the path canonicalises to, and a
    // Windows path on this host canonicalises to itself — which is exactly
    // the shape a Windows guest sends back, backslashes and all.
    let mut sftp = Sftp::open(&h.session).await;
    let mut args = Vec::new();
    put_str(&mut args, r"C:\Users\dev\project");
    let (kind, body) = sftp.ask(kind::REALPATH, &args).await;
    assert_eq!(kind, super::sftp::kind::NAME);
    assert_eq!(read_names(&body)[0].0, r"C:\Users\dev\project");
}

/// An extension nothing in the client set needs is answered by name.
///
/// This is the one refusal in the whole facade the protocol lets vmlab
/// narrate itself: a status carries a message where a channel-request failure
/// does not (§19.3).
#[tokio::test]
async fn an_extension_is_refused_in_vmlabs_own_words() {
    let h = connect_as("dev", logins()).await.unwrap();
    let mut sftp = Sftp::open(&h.session).await;
    let mut args = Vec::new();
    put_str(&mut args, "statvfs@openssh.com");
    put_str(&mut args, "/");
    let (code, msg) = sftp.status(200, &args).await; // SSH_FXP_EXTENDED
    assert_eq!(code, status::OP_UNSUPPORTED);
    assert!(msg.contains("vmlab"), "{msg}");
}

/// **The coupling §19.3 calls a requirement**: the facade must never grant SSH
/// window it cannot back with agent credit.
///
/// The guest here takes the bytes and never grants credit for more, which is
/// the shape of a guest that has stopped keeping up. The naive facade
/// acknowledges the client generously and buffers the difference inside
/// `labd` — against a tens-of-megabytes editor-server push, an unbounded
/// buffer in a long-lived process. So: push far more than any window, and
/// assert the client is *stopped*, having handed over a bounded amount.
#[tokio::test]
async fn a_push_a_stalled_guest_cannot_take_stops_rather_than_buffering() {
    let h = connect_stalled("dev", logins()).await.unwrap();

    let channel = h.session.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    let mut stream = channel.into_stream();

    let sent = Arc::new(AtomicUsize::new(0));
    let pushing = sent.clone();
    let push = async move {
        // The handle is never answered — the guest is not replying — so the
        // write packets name one the guest will reject if it ever wakes up.
        // What is under test is how many bytes the *facade* takes before it
        // stops taking them.
        let payload = vec![b'x'; 32 * 1024];
        let mut offset = 0u64;
        loop {
            let mut body = Vec::new();
            put_u32(&mut body, 1);
            body.extend(write_args(&1u64.to_be_bytes(), offset, &payload));
            let mut packet = Vec::new();
            put_u32(&mut packet, (body.len() + 1) as u32);
            packet.push(kind::WRITE);
            packet.extend(body);
            if stream.write_all(&packet).await.is_err() {
                return;
            }
            offset += payload.len() as u64;
            pushing.fetch_add(packet.len(), Ordering::SeqCst);
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), push).await;

    // The bound is the two windows plus the requests between them, not the
    // transfer: an SSH window, one chunk in flight towards the file session,
    // and the outstanding operations each waiting on the guest's own credit.
    let sent = sent.load(Ordering::SeqCst);
    assert!(sent > 0, "the facade took nothing at all");
    assert!(
        sent < 4 * 1024 * 1024,
        "the facade took {sent} bytes from a guest that granted credit for none — \
         that difference is a buffer in labd"
    );
}

/// A large transfer does not starve an interactive session on the same
/// connection (§19.3).
///
/// The transfer and the shell are separate channels with separate pumps, and
/// the connection's read loop only ever waits for the guest to take one chunk
/// — so a shell keeps round-tripping while megabytes move beside it.
#[tokio::test]
async fn a_large_transfer_does_not_starve_an_interactive_session() {
    let h = connect_as("dev", logins()).await.unwrap();
    let root = h.guest();

    let mut shell = h.session.channel_open_session().await.unwrap();
    shell.request_shell(true).await.unwrap();
    assert_eq!(read_data(&mut shell, 8).await, b"prompt$ ");

    let mut sftp = Sftp::open(&h.session).await;
    let file = root.join("big.bin");
    let mut args = path_args(&file);
    put_u32(&mut args, 0x2 | 0x8 | 0x10);
    put_u32(&mut args, 0);
    let handle = sftp.handle(kind::OPEN, &args).await;

    let transfer = tokio::spawn(async move {
        let payload = vec![b'z'; 32 * 1024];
        // 8 MiB: many times either window, so the transfer is genuinely in
        // flight for the whole of the interactive traffic below.
        for i in 0..256u64 {
            let (code, msg) = sftp
                .status(kind::WRITE, &write_args(&handle, i * 32 * 1024, &payload))
                .await;
            assert_eq!(code, status::OK, "{msg}");
        }
        sftp
    });

    // Every keystroke comes back while that runs, and none of them waits long.
    for i in 0..20 {
        let typed = format!("echo {i}\r");
        shell.data(typed.as_bytes()).await.unwrap();
        let echoed =
            tokio::time::timeout(Duration::from_secs(5), read_data(&mut shell, typed.len()))
                .await
                .unwrap_or_else(|_| panic!("keystroke {i} never came back while the transfer ran"));
        assert_eq!(echoed, typed.as_bytes());
    }

    transfer.await.unwrap();
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 8 * 1024 * 1024);
}

// ---------------------------------------------------------------------------
// A real `scp` and a real `sftp`
// ---------------------------------------------------------------------------

/// `scp` in both directions, with the binary the developer actually types.
///
/// `scp` has spoken SFTP since OpenSSH 9.0, so this is a test of the
/// transcode and not of a legacy protocol: it opens, writes, sets attributes
/// and closes on the way up, and stats, opens and reads on the way down.
#[tokio::test]
async fn scp_moves_a_file_in_both_directions() {
    let Some(facade) = openssh_reachable_facade(logins()).await else {
        return;
    };
    if openssh_version().unwrap_or(0) < 9 {
        eprintln!("ssh(1) predates scp-over-SFTP — skipping");
        return;
    }
    let host = tempfile::tempdir().unwrap();
    let source = host.path().join("payload.bin");
    // Bigger than one SFTP read, so the transfer is many requests rather than
    // one.
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &bytes).unwrap();
    let landed = facade.guest().join("payload.bin");

    let mut up = facade.args("-P");
    up.push("-s".into()); // the SFTP protocol, said out loud
    up.push(source.to_string_lossy().into_owned());
    up.push(format!("dev@127.0.0.1:{}", landed.display()));
    let out = tokio::process::Command::new("scp")
        .args(&up)
        .output()
        .await
        .expect("run scp");
    assert!(
        out.status.success(),
        "scp up failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(&landed).unwrap(), bytes);

    let back = host.path().join("back.bin");
    let mut down = facade.args("-P");
    down.push("-s".into());
    down.push(format!("dev@127.0.0.1:{}", landed.display()));
    down.push(back.to_string_lossy().into_owned());
    let out = tokio::process::Command::new("scp")
        .args(&down)
        .output()
        .await
        .expect("run scp");
    assert!(
        out.status.success(),
        "scp down failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(&back).unwrap(), bytes);

    // And both invocations ran as the selected identity, not as the agent.
    let opens = facade.agent_log.opens.lock().unwrap().clone();
    let sessions: Vec<_> = opens
        .iter()
        .filter_map(|o| match o {
            Opened::FileOps { logon } => Some(logon.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sessions.len(), 2, "one file session per scp: {opens:?}");
    for logon in sessions {
        assert_eq!(logon.unwrap().user, r"PROBE\dev");
    }
}

/// The `sftp` binary, driven through a batch of what a developer types.
///
/// A batch aborts on the first failure and exits non-zero, so the exit code
/// alone is the assertion that every one of these was answered the way the
/// client expected.
#[tokio::test]
async fn sftp_runs_a_batch_of_what_a_developer_types() {
    let Some(facade) = openssh_reachable_facade(logins()).await else {
        return;
    };
    let host = tempfile::tempdir().unwrap();
    let source = host.path().join("notes.txt");
    std::fs::write(&source, b"one\ntwo\nthree\n").unwrap();
    let guest = facade.guest();
    let batch = host.path().join("batch");
    std::fs::write(
        &batch,
        format!(
            "mkdir {dir}\n\
             put {src} {dir}/notes.txt\n\
             ls -l {dir}\n\
             rename {dir}/notes.txt {dir}/renamed.txt\n\
             chmod 600 {dir}/renamed.txt\n\
             get {dir}/renamed.txt {back}\n\
             rm {dir}/renamed.txt\n\
             rmdir {dir}\n",
            dir = guest.join("work").display(),
            src = source.display(),
            back = host.path().join("back.txt").display(),
        ),
    )
    .unwrap();

    let mut args = facade.args("-P");
    args.push("-b".into());
    args.push(batch.to_string_lossy().into_owned());
    args.push("dev@127.0.0.1".into());
    let out = tokio::process::Command::new("sftp")
        .args(&args)
        .output()
        .await
        .expect("run sftp");
    assert!(
        out.status.success(),
        "sftp batch failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(host.path().join("back.txt")).unwrap(),
        b"one\ntwo\nthree\n"
    );
    assert!(
        !guest.join("work").exists(),
        "the batch cleaned up after it"
    );
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
