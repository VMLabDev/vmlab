//! Host-side client for the in-guest `vmlab-agent` (guest/agent-proto): the
//! `vmlab.agent.0` virtio-serial port carrying framed, multiplexed channels
//! — terminals, streaming exec, file transfer, tails, metrics, clipboard.
//! QEMU owns the socket (`server=on,wait=off`); the daemon connects as the
//! single client and re-exposes each terminal
//! session as a per-session unix socket that is a dumb raw byte pipe (what
//! `vmlab shell` and the web terminal attach to).
//!
//! Handshake: the host sends `Hello{token}` and waits for the agent's hello
//! echoing the token. The token is the resync barrier after an online
//! snapshot restore — everything before the echo is stale replay, and the
//! frame magic lets the decoder skip mid-frame garbage (see
//! `guest/agent-proto`). No echo within the timeout means the guest has no
//! agent (template predates it) — callers turn that into an actionable
//! error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, mpsc, watch};

pub use vmlab_agent_proto::watch::StatRecord;
use vmlab_agent_proto::watch::{RecordDecoder, WatchRecord, encode_record};
use vmlab_agent_proto::{
    AgentMsg, DiskUsage, ErrorCause, FrameDecoder, FrameKind, HostMsg, INITIAL_WINDOW, MAX_PAYLOAD,
    PROTO_VERSION, RecvWindow, encode_ctrl, encode_frame, features,
};
pub use vmlab_agent_proto::{Logon, NetInterface, OsInfo, ShutdownMode};

use crate::sync::LockRecover;

/// What the agent said about itself in the handshake.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_version: String,
    pub os: String,
    pub features: Vec<String>,
}

/// One metrics sample.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disks: Vec<DiskUsage>,
}

/// Everything a session consumer can observe.
#[derive(Debug)]
pub enum SessionEvent {
    Data(Vec<u8>),
    /// Exec stderr.
    Stderr(Vec<u8>),
    /// No more guest→host bytes, and the channel is still open: a tunnel's
    /// peer shut down its write half, or an exec's output pipes drained.
    /// Host→guest bytes may still flow after it.
    Eof,
    /// Terminal shell / exec process ended.
    Exited(i32),
    /// File transfer completed (both directions).
    FileDone {
        sha256: String,
        len: u64,
    },
    /// The agent failed this channel.
    Error(String),
}

/// Guest-granted credit for host→guest payload on one channel.
struct SendCredit {
    avail: std::sync::Mutex<u64>,
    closed: AtomicBool,
    notify: Notify,
}

impl SendCredit {
    fn new() -> Self {
        Self {
            avail: std::sync::Mutex::new(INITIAL_WINDOW),
            closed: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// Take up to `want` bytes of credit, waiting for a grant if empty.
    /// Returns 0 once the channel is closed.
    async fn take(&self, want: usize) -> usize {
        loop {
            let notified = self.notify.notified();
            {
                let mut g = self.avail.lock_recover();
                if self.closed.load(Ordering::SeqCst) {
                    return 0;
                }
                if *g > 0 {
                    let n = (*g).min(want as u64).min(MAX_PAYLOAD as u64) as usize;
                    *g -= n as u64;
                    return n;
                }
            }
            notified.await;
        }
    }

    fn grant(&self, bytes: u64) {
        let mut g = self.avail.lock_recover();
        *g = g.saturating_add(bytes);
        self.notify.notify_waiters();
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

struct SessionEntry {
    tx: mpsc::Sender<SessionEvent>,
    credit: Arc<SendCredit>,
}

/// Per-session event queue depth. Flow control caps un-granted bytes, but a
/// window's worth of tiny frames can outnumber a byte-sized bound — this is
/// the safety valve; see the reader's stall policy.
const SESSION_QUEUE: usize = 2048;

/// How long the reader waits on one session's full queue before declaring
/// the consumer stuck and closing that session (never the whole port).
const STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an `open_*` waits for the agent's `opened`. Longer than the
/// agent's tunnel dial budget, so a dead destination arrives as the connect
/// failure the agent reports rather than as this timeout, which says nothing
/// about why.
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

struct Inner {
    /// The connection's runtime, so cleanup can be spawned from any thread
    /// (scripts drop sessions on non-runtime threads).
    rt: tokio::runtime::Handle,
    /// The reader task, aborted on shutdown/drop. Without this the task
    /// stays blocked in `read()` holding the socket's read half — and
    /// QEMU's `server=on` chardev serves ONE client at a time, so a
    /// half-dead connection blocks every future connect (the post-restore
    /// reconnect would queue in the listen backlog forever).
    reader: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    writer: Mutex<OwnedWriteHalf>,
    sessions: Mutex<HashMap<u32, SessionEntry>>,
    /// Waiters for `opened`/error replies to an `open_*` message, keyed by
    /// channel id (std mutex: never held across await).
    open_waiters: std::sync::Mutex<HashMap<u32, OpenWaiter>>,
    next_id: AtomicU32,
    /// The handshake result (`None` until the token echo arrives).
    hello: watch::Sender<Option<AgentInfo>>,
    /// Incremented per `pong`.
    pong: watch::Sender<u64>,
    /// Latest metrics sample.
    metrics: watch::Sender<Option<MetricsSnapshot>>,
    /// Incremented per clipboard report, with the text.
    clipboard: watch::Sender<(u64, String)>,
    /// Incremented per net_info reply, with the interfaces.
    net_info: watch::Sender<(u64, Vec<NetInterface>)>,
    /// Incremented per os_info reply, with the info.
    os_info: watch::Sender<(u64, Option<OsInfo>)>,
    /// Incremented per shutting_down ack.
    shutting_down: watch::Sender<u64>,
    /// Whether `subscribe_metrics` has been sent on this connection.
    metrics_subscribed: AtomicBool,
    token: String,
}

/// Where a pull puts the guest's bytes as they arrive.
trait PullSink {
    /// Take one chunk, or fail the pull.
    async fn write(&mut self, chunk: &[u8]) -> Result<()>;
    /// Every chunk has arrived and the digest matched.
    async fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Straight to a host file: bounded by the disk, never held in memory.
struct FileSink(tokio::fs::File);

impl PullSink for FileSink {
    async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        Ok(self.0.write_all(chunk).await?)
    }
    async fn finish(&mut self) -> Result<()> {
        Ok(self.0.flush().await?)
    }
}

/// Into memory, for a caller that wants the bytes. The ceiling is checked
/// chunk by chunk, so an oversized file is refused mid-flight rather than
/// buffered whole and then rejected.
struct MemorySink {
    remote: String,
    buf: Vec<u8>,
    limit: u64,
}

impl PullSink for MemorySink {
    async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        if self.buf.len() as u64 + chunk.len() as u64 > self.limit {
            return Err(anyhow::Error::new(crate::proto::over_inline_limit(
                format!("pull {}", self.remote),
                self.limit,
            )));
        }
        self.buf.extend_from_slice(chunk);
        Ok(())
    }
}

/// Handle to one guest's agent channel. Cheap to clone (`Arc` inner).
/// Dropping the last clone tears the connection down (the reader task holds
/// only a `Weak`).
#[derive(Clone)]
pub struct AgentHandle {
    inner: Arc<Inner>,
}

impl AgentHandle {
    /// Connect to the agent socket and complete the token handshake.
    pub async fn connect(path: &Path, handshake_timeout: Duration) -> Result<AgentHandle> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connecting agent socket {}", path.display()))?;
        let (read_half, write_half) = stream.into_split();

        let token = format!("{:016x}", rand::random::<u64>());
        let inner = Arc::new(Inner {
            rt: tokio::runtime::Handle::current(),
            reader: std::sync::Mutex::new(None),
            writer: Mutex::new(write_half),
            sessions: Mutex::new(HashMap::new()),
            open_waiters: std::sync::Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            hello: watch::Sender::new(None),
            pong: watch::Sender::new(0),
            metrics: watch::Sender::new(None),
            clipboard: watch::Sender::new((0, String::new())),
            net_info: watch::Sender::new((0, Vec::new())),
            os_info: watch::Sender::new((0, None)),
            shutting_down: watch::Sender::new(0),
            metrics_subscribed: AtomicBool::new(false),
            token,
        });
        let handle = AgentHandle {
            inner: inner.clone(),
        };

        // Reader task: holds only a Weak so it never keeps the connection
        // alive; the JoinHandle lets shutdown/drop abort it (which drops the
        // read half and actually closes the socket — see `Inner::reader`).
        let weak = Arc::downgrade(&inner);
        let task = tokio::spawn(async move { reader_task(weak, read_half).await });
        *inner.reader.lock_recover() = Some(task);

        handle
            .send_msg(&HostMsg::Hello {
                proto_version: PROTO_VERSION,
                token: handle.inner.token.clone(),
            })
            .await
            .context("sending agent handshake")?;

        let mut rx = handle.inner.hello.subscribe();
        let deadline = tokio::time::Instant::now() + handshake_timeout;
        loop {
            if rx.borrow().is_some() {
                return Ok(handle);
            }
            tokio::time::timeout_at(deadline, rx.changed())
                .await
                .map_err(|_| anyhow!("no vmlab-agent answered on the agent channel"))?
                .map_err(|_| anyhow!("agent channel closed during handshake"))?;
        }
    }

    /// The handshake info (always present after a successful `connect`).
    pub fn info(&self) -> AgentInfo {
        self.inner
            .hello
            .borrow()
            .clone()
            .expect("connect completed the handshake")
    }

    pub fn has_feature(&self, feature: &str) -> bool {
        self.info().features.iter().any(|f| f == feature)
    }

    async fn send_msg(&self, msg: &HostMsg) -> Result<()> {
        let mut w = self.inner.writer.lock().await;
        w.write_all(&encode_ctrl(msg))
            .await
            .context("agent write")?;
        w.flush().await.context("agent flush")?;
        Ok(())
    }

    async fn send_data(&self, id: u32, payload: &[u8]) -> Result<()> {
        let mut w = self.inner.writer.lock().await;
        w.write_all(&encode_frame(FrameKind::Data, id, payload))
            .await
            .context("agent write")?;
        w.flush().await.context("agent flush")?;
        Ok(())
    }

    /// Tear the connection down now — both socket halves — and fail every
    /// open session so consumers unblock. Used around snapshot restores and
    /// when replacing a dead handle; dropping the last handle does the same
    /// implicitly.
    pub async fn shutdown(&self) {
        if let Some(task) = self.inner.reader.lock_recover().take() {
            task.abort();
        }
        // Close the write half explicitly: QEMU frees its one-client
        // chardev slot only when *its* read side sees EOF, and session-held
        // handle clones (an attached terminal, a lingering tail) would
        // otherwise keep the socket half-open indefinitely, wedging every
        // future connect in the listen backlog.
        let _ = self.inner.writer.lock().await.shutdown().await;
        // Dropping the entries drops their event senders, so session
        // consumers' recv() sees end-of-stream and their pumps wind down.
        let mut sessions = self.inner.sessions.lock().await;
        for (_, entry) in sessions.drain() {
            entry.credit.close();
        }
        self.inner.open_waiters.lock_recover().clear();
    }

    /// Liveness probe.
    pub async fn ping(&self, timeout: Duration) -> bool {
        let mut rx = self.inner.pong.subscribe();
        rx.mark_unchanged();
        if self.send_msg(&HostMsg::Ping).await.is_err() {
            return false;
        }
        tokio::time::timeout(timeout, rx.changed())
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Open a channel and wait for the agent's `opened` (or error).
    async fn open(&self, build: impl FnOnce(u32) -> HostMsg) -> Result<AgentSession> {
        self.try_open(build).await.map_err(|e| e.error)
    }

    /// The open itself, keeping the agent's machine-readable cause. Only a
    /// tunnel branches on it, and only ever here: the guest dials before it
    /// answers `opened`, so a connect failure is always this reply and never
    /// a mid-stream error.
    async fn try_open(
        &self,
        build: impl FnOnce(u32) -> HostMsg,
    ) -> Result<AgentSession, OpenError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(SESSION_QUEUE);
        // The opened/error reply arrives on a oneshot so the session's event
        // queue only ever carries post-open traffic.
        let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
        let credit = Arc::new(SendCredit::new());
        self.inner.sessions.lock().await.insert(
            id,
            SessionEntry {
                tx,
                credit: credit.clone(),
            },
        );
        // Stash the oneshot where the reader finds it.
        self.inner
            .open_waiters
            .lock()
            .unwrap()
            .insert(id, opened_tx);

        let mut session = AgentSession {
            handle: self.clone(),
            id,
            rx,
            credit,
            window: RecvWindow::default(),
            closed: false,
        };
        if let Err(e) = self.send_msg(&build(id)).await {
            session.forget().await;
            return Err(OpenError::plain(e));
        }
        match tokio::time::timeout(OPEN_TIMEOUT, opened_rx).await {
            Ok(Ok(Ok(()))) => Ok(session),
            Ok(Ok(Err(refusal))) => {
                session.forget().await;
                Err(OpenError {
                    error: anyhow!("{}", refusal.msg),
                    cause: refusal.cause,
                })
            }
            Ok(Err(_)) | Err(_) => {
                session.forget().await;
                let _ = self.send_msg(&HostMsg::Close { id }).await;
                Err(OpenError::plain(anyhow!(
                    "agent did not open the channel in time"
                )))
            }
        }
    }

    /// Interactive shell session. `command` overrides the guest's default
    /// shell.
    ///
    /// `logon` is who the shell runs as (§19.2), already resolved from the
    /// machine's declaration by [`crate::labd::identity::resolve`]; `None`
    /// is the agent identity, which is what everything vmlab does on its own
    /// behalf passes.
    pub async fn open_terminal(
        &self,
        cols: u16,
        rows: u16,
        command: Option<Vec<String>>,
        logon: Option<Logon>,
    ) -> Result<AgentSession> {
        self.open(|id| HostMsg::OpenTerminal {
            id,
            cols,
            rows,
            command,
            logon,
        })
        .await
    }

    /// Streaming exec session (stdin via [`AgentSession::send`] +
    /// [`AgentSession::eof`]).
    pub async fn open_exec(
        &self,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
        logon: Option<Logon>,
    ) -> Result<AgentSession> {
        self.open(|id| HostMsg::OpenExec {
            id,
            argv,
            env,
            cwd,
            logon,
        })
        .await
    }

    /// Dial `host:port` over TCP from inside the guest; the session is then
    /// the connection's byte pipe (PRD §19.5).
    ///
    /// `host` goes across verbatim and the guest resolves it, which is what
    /// makes a domain name in a SOCKS request work, and no destination
    /// policy applies — any address the guest can reach. Only the SSH facade
    /// calls this; general host→guest TCP is the Forward plan's job (§9.8),
    /// and no daemon command reaches it.
    pub async fn open_tunnel(&self, host: String, port: u16) -> Result<AgentSession, TunnelError> {
        if !self.has_feature(features::TUNNEL) {
            return Err(TunnelError::Refused(
                "this guest's agent cannot open tunnels — rebuild the template, or push the \
                 shipped agent with `vmlab machine repair-agent`"
                    .into(),
            ));
        }
        self.try_open(|id| HostMsg::OpenTunnel { id, host, port })
            .await
            .map_err(|e| {
                let detail = format!("{:#}", e.error);
                match e.cause {
                    Some(ErrorCause::ConnectFailed) => TunnelError::ConnectFailed(detail),
                    None => TunnelError::Refused(detail),
                }
            })
    }

    /// Follow a guest file (`tail -F`); the session yields `Data` chunks.
    /// `logon` is whose view of the filesystem it is read through (§19.2).
    pub async fn open_tail(&self, path: String, logon: Option<Logon>) -> Result<AgentSession> {
        self.open(|id| HostMsg::OpenTail { id, path, logon }).await
    }

    /// Watch the guest tree at `path` recursively (§19.5). `prune` is the
    /// host's list of root-relative directory prefixes the guest registers no
    /// watcher under — the host still owns globs, negations and semantics
    /// entirely; the guest is handed a list, never asked a question.
    ///
    /// The open carries no `logon`: a watcher produces none of the
    /// developer's files, so it runs as the agent identity, which also makes
    /// coverage complete by construction rather than bounded by what one
    /// account can traverse.
    pub async fn open_watch(&self, path: String, prune: Vec<String>) -> Result<WatchSession> {
        if !self.has_feature(vmlab_agent_proto::features::WATCH) {
            bail!("the guest agent has no `watch` support — rebuild the template to update it");
        }
        let session = self
            .open(|id| HostMsg::OpenWatch { id, path, prune })
            .await?;
        Ok(WatchSession {
            session,
            decoder: RecordDecoder::new(),
            pending: std::collections::VecDeque::new(),
        })
    }

    /// Follow the Windows event log.
    pub async fn open_eventlog(&self, filter: Option<String>) -> Result<AgentSession> {
        self.open(|id| HostMsg::OpenEventLog { id, filter }).await
    }

    /// Run to completion, collecting output. `128 + signal` codes on Unix.
    pub async fn exec(
        &self,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
        logon: Option<Logon>,
    ) -> Result<ExecOutput> {
        let display = argv.join(" ");
        let mut session = self.open_exec(argv, env, cwd, logon).await?;
        if let Some(stdin) = stdin {
            session.send(&stdin).await?;
        }
        session.eof().await?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let ev = tokio::time::timeout_at(deadline, session.recv())
                .await
                .map_err(|_| anyhow!("exec `{display}` timed out after {timeout:?}"))?;
            match ev {
                Some(SessionEvent::Data(b)) => out.extend(b),
                Some(SessionEvent::Stderr(b)) => err.extend(b),
                Some(SessionEvent::Exited(code)) => {
                    return Ok(ExecOutput {
                        exit_code: code,
                        stdout: out,
                        stderr: err,
                    });
                }
                Some(SessionEvent::Error(msg)) => bail!("exec `{display}`: {msg}"),
                // `exited` is what completes a collected exec; the output
                // EOF just before it tells this caller nothing new.
                Some(SessionEvent::Eof) | Some(SessionEvent::FileDone { .. }) => {}
                None => bail!("agent channel closed during exec `{display}`"),
            }
        }
    }

    /// Push a host file into the guest, returning the verified digest+size.
    pub async fn push_file(
        &self,
        local: &Path,
        remote: &str,
        mode: Option<u32>,
    ) -> Result<(String, u64)> {
        use sha2::{Digest, Sha256};
        let mut file = tokio::fs::File::open(local)
            .await
            .with_context(|| format!("opening {}", local.display()))?;
        let mut session = self
            .open(|id| HostMsg::OpenFilePush {
                id,
                path: remote.to_string(),
                mode,
            })
            .await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            session.send(&buf[..n]).await?;
        }
        session.eof().await?;
        let local_sha = hex::encode(hasher.finalize());
        loop {
            match session.recv().await {
                Some(SessionEvent::FileDone { sha256, len }) => {
                    if sha256 != local_sha {
                        bail!("push {remote}: digest mismatch after transfer");
                    }
                    return Ok((sha256, len));
                }
                Some(SessionEvent::Error(msg)) => bail!("push {remote}: {msg}"),
                Some(_) => {}
                None => bail!("agent channel closed during push of {remote}"),
            }
        }
    }

    /// Push a host file or directory tree into the guest, returning
    /// `(files, bytes)`. Guest paths join with `/` (works on Windows too).
    pub async fn push_tree(&self, src: &Path, guest_dest: &str) -> Result<(usize, u64)> {
        let entries = walk_tree_for_push(src, guest_dest)?;
        let mut bytes = 0u64;
        let files = entries.len();
        for (local, remote, mode) in entries {
            let (_sha, len) = self.push_file(&local, &remote, mode).await?;
            bytes += len;
        }
        Ok((files, bytes))
    }

    /// Pull a guest file to the host, returning the verified digest+size.
    pub async fn pull_file(&self, remote: &str, local: &Path) -> Result<(String, u64)> {
        if let Some(parent) = local.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let file = tokio::fs::File::create(local)
            .await
            .with_context(|| format!("creating {}", local.display()))?;
        self.pull_into(remote, &mut FileSink(file)).await
    }

    /// Pull a guest file into memory, for a caller that wants the bytes rather
    /// than a file on this host — the inline form on the wire.
    ///
    /// `limit` is enforced as the bytes arrive, so an oversized file costs one
    /// chunk of memory rather than all of it, and the caller gets the limit by
    /// code instead of a file cut short.
    pub async fn pull_bytes(&self, remote: &str, limit: u64) -> Result<(String, Vec<u8>)> {
        let mut sink = MemorySink {
            remote: remote.to_string(),
            buf: Vec::new(),
            limit,
        };
        let (sha256, _len) = self.pull_into(remote, &mut sink).await?;
        Ok((sha256, sink.buf))
    }

    /// The pull itself: stream the guest's bytes into `sink`, verifying the
    /// digest the agent reports against the one we computed on the way.
    async fn pull_into(&self, remote: &str, sink: &mut impl PullSink) -> Result<(String, u64)> {
        use sha2::{Digest, Sha256};
        let mut session = self
            .open(|id| HostMsg::OpenFilePull {
                id,
                path: remote.to_string(),
            })
            .await?;
        let mut hasher = Sha256::new();
        loop {
            match session.recv().await {
                Some(SessionEvent::Data(b)) => {
                    hasher.update(&b);
                    sink.write(&b).await?;
                }
                Some(SessionEvent::FileDone { sha256, len }) => {
                    sink.finish().await?;
                    if hex::encode(hasher.finalize()) != sha256 {
                        bail!("pull {remote}: digest mismatch after transfer");
                    }
                    return Ok((sha256, len));
                }
                Some(SessionEvent::Error(msg)) => bail!("pull {remote}: {msg}"),
                Some(_) => {}
                None => bail!("agent channel closed during pull of {remote}"),
            }
        }
    }

    /// Resize a terminal session.
    pub async fn resize(&self, id: u32, cols: u16, rows: u16) -> Result<()> {
        self.send_msg(&HostMsg::Resize { id, cols, rows }).await
    }

    /// Latest metrics sample, subscribing on first use (2s cadence).
    pub async fn stats(&self, timeout: Duration) -> Result<MetricsSnapshot> {
        if let Some(m) = self.inner.metrics.borrow().clone() {
            return Ok(m);
        }
        let mut rx = self.inner.metrics.subscribe();
        if !self.inner.metrics_subscribed.swap(true, Ordering::SeqCst) {
            self.send_msg(&HostMsg::SubscribeMetrics { interval_secs: 2 })
                .await?;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(m) = rx.borrow().clone() {
                return Ok(m);
            }
            tokio::time::timeout_at(deadline, rx.changed())
                .await
                .map_err(|_| anyhow!("agent sent no metrics within {timeout:?}"))?
                .map_err(|_| anyhow!("agent channel closed"))?;
        }
    }

    /// The guest's network interfaces (loopback excluded).
    pub async fn net_interfaces(&self, timeout: Duration) -> Result<Vec<NetInterface>> {
        let mut rx = self.inner.net_info.subscribe();
        rx.mark_unchanged();
        self.send_msg(&HostMsg::NetInfo).await?;
        tokio::time::timeout(timeout, rx.changed())
            .await
            .map_err(|_| anyhow!("agent sent no net_info within {timeout:?}"))?
            .map_err(|_| anyhow!("agent channel closed"))?;
        let (_, interfaces) = rx.borrow().clone();
        Ok(interfaces)
    }

    /// Structured guest OS information.
    pub async fn osinfo(&self, timeout: Duration) -> Result<OsInfo> {
        let mut rx = self.inner.os_info.subscribe();
        rx.mark_unchanged();
        self.send_msg(&HostMsg::OsInfo).await?;
        tokio::time::timeout(timeout, rx.changed())
            .await
            .map_err(|_| anyhow!("agent sent no os_info within {timeout:?}"))?
            .map_err(|_| anyhow!("agent channel closed"))?;
        let (_, info) = rx.borrow().clone();
        info.ok_or_else(|| anyhow!("agent sent no os_info"))
    }

    /// Ask the guest to shut down. The agent acks and then the whole
    /// connection may vanish before any further bytes arrive, so a missing
    /// ack within `timeout` still counts as success — only a failed *send*
    /// is an error (the guest never got the request).
    pub async fn shutdown_guest(&self, mode: ShutdownMode, timeout: Duration) -> Result<()> {
        let mut rx = self.inner.shutting_down.subscribe();
        rx.mark_unchanged();
        self.send_msg(&HostMsg::Shutdown { mode }).await?;
        let _ = tokio::time::timeout(timeout, rx.changed()).await;
        Ok(())
    }

    pub async fn set_clipboard(&self, text: String) -> Result<()> {
        self.send_msg(&HostMsg::SetClipboard { text }).await
    }

    pub async fn get_clipboard(&self, timeout: Duration) -> Result<String> {
        let mut rx = self.inner.clipboard.subscribe();
        rx.mark_unchanged();
        self.send_msg(&HostMsg::GetClipboard).await?;
        tokio::time::timeout(timeout, rx.changed())
            .await
            .map_err(|_| anyhow!("agent sent no clipboard within {timeout:?}"))?
            .map_err(|_| anyhow!("agent channel closed"))?;
        let (_, text) = rx.borrow().clone();
        Ok(text)
    }
}

/// One open channel, held by its consumer. Dropping it closes the channel
/// on the agent side (best-effort).
pub struct AgentSession {
    handle: AgentHandle,
    pub id: u32,
    rx: mpsc::Receiver<SessionEvent>,
    credit: Arc<SendCredit>,
    window: RecvWindow,
    closed: bool,
}

impl AgentSession {
    /// Next event; grants receive window back as data is consumed. `None`
    /// once the channel (or connection) is gone.
    pub async fn recv(&mut self) -> Option<SessionEvent> {
        let ev = self.rx.recv().await?;
        if let SessionEvent::Data(b) | SessionEvent::Stderr(b) = &ev
            && let Some(grant) = self.window.recv(b.len())
        {
            let _ = self
                .handle
                .send_msg(&HostMsg::WindowAdjust {
                    id: self.id,
                    bytes: grant,
                })
                .await;
        }
        if matches!(
            ev,
            SessionEvent::Exited(_) | SessionEvent::FileDone { .. } | SessionEvent::Error(_)
        ) {
            self.closed = true; // agent already tore its side down
        }
        Some(ev)
    }

    /// Send host→guest bytes (terminal input, exec stdin, pushed file data),
    /// respecting the guest's credit window.
    pub async fn send(&self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let n = self.credit.take(bytes.len()).await;
            if n == 0 {
                bail!("agent channel closed");
            }
            self.handle.send_data(self.id, &bytes[..n]).await?;
            bytes = &bytes[n..];
        }
        Ok(())
    }

    /// No more host→guest bytes (exec stdin EOF / end of pushed file).
    pub async fn eof(&self) -> Result<()> {
        self.handle.send_msg(&HostMsg::Eof { id: self.id }).await
    }

    /// Resize this terminal session's PTY.
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.handle.resize(self.id, cols, rows).await
    }

    /// Explicitly close (also implied by drop).
    pub async fn close(mut self) {
        self.forget().await;
        let _ = self.handle.send_msg(&HostMsg::Close { id: self.id }).await;
    }

    /// Drop local state without messaging the agent.
    async fn forget(&mut self) {
        self.closed = true;
        self.handle.inner.sessions.lock().await.remove(&self.id);
        self.handle
            .inner
            .open_waiters
            .lock()
            .unwrap()
            .remove(&self.id);
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let handle = self.handle.clone();
        let id = self.id;
        handle.inner.open_waiters.lock_recover().remove(&id);
        // Spawned on the connection's own runtime: a drop on a non-runtime
        // thread (script executors) must not panic.
        let rt = handle.inner.rt.clone();
        rt.spawn(async move {
            handle.inner.sessions.lock().await.remove(&id);
            let _ = handle.send_msg(&HostMsg::Close { id }).await;
        });
    }
}

/// What a watch channel reports. Paths, never events: the guest's dirty set
/// coalesces, so a path created, modified and deleted inside one drain window
/// has no single kind to report (§19.5).
#[derive(Debug)]
pub enum WatchReport {
    /// The guest's dirty set went empty → non-empty. One nudge per drain
    /// window: drain now if idle, or let the burst batch itself.
    Dirty,
    /// The answer to a [`WatchSession::drain`]: one record per dirty path,
    /// each the path plus its current stat, or a tombstone if it is gone.
    Batch(Vec<StatRecord>),
    /// The answer to a drain when coverage was lost — a platform event queue
    /// overflowed, the guest's set hit its cap, or a subtree vanished without
    /// per-child events. All of them mean the same thing to the host: run the
    /// stat-walk. It never needs to know which fired.
    Rescan,
    /// The channel failed. The watch root vanishing arrives here naming the
    /// root, rather than as a batch of tombstones for everything under it.
    Error(String),
}

/// One open watch channel. Its records ride the channel's own credit window
/// rather than the control channel: a 30 000-path batch is megabytes of JSON,
/// and control frames are not flow-controlled.
pub struct WatchSession {
    session: AgentSession,
    decoder: RecordDecoder,
    pending: std::collections::VecDeque<WatchRecord>,
}

impl WatchSession {
    /// Swap the guest's dirty set out. At most one drain is outstanding; the
    /// answer is one [`WatchReport::Batch`] or one [`WatchReport::Rescan`],
    /// though a [`WatchReport::Dirty`] the guest sent before the drain reached
    /// it can still arrive first. There is no ack for the answer, because a
    /// dropped channel already implies a stat-walk, so the loss self-heals.
    pub async fn drain(&self) -> Result<()> {
        self.session
            .send(&encode_record(&WatchRecord::Drain))
            .await
            .context("requesting a watch drain")
    }

    /// Next record. `None` once the channel (or connection) is gone.
    pub async fn recv(&mut self) -> Option<WatchReport> {
        loop {
            if let Some(record) = self.pending.pop_front() {
                return Some(match record {
                    WatchRecord::Dirty => WatchReport::Dirty,
                    WatchRecord::Batch { entries } => WatchReport::Batch(entries),
                    WatchRecord::Rescan => WatchReport::Rescan,
                    // Host→agent only; an agent sending one is desynced.
                    WatchRecord::Drain => {
                        WatchReport::Error("agent sent a drain on a watch channel".into())
                    }
                });
            }
            match self.session.recv().await? {
                SessionEvent::Data(bytes) => {
                    self.decoder.push(&bytes);
                    loop {
                        match self.decoder.next_record() {
                            Ok(Some(record)) => self.pending.push_back(record),
                            Ok(None) => break,
                            Err(e) => return Some(WatchReport::Error(e)),
                        }
                    }
                }
                SessionEvent::Error(msg) => return Some(WatchReport::Error(msg)),
                other => {
                    return Some(WatchReport::Error(format!(
                        "unexpected {other:?} on a watch channel"
                    )));
                }
            }
        }
    }

    pub async fn close(self) {
        self.session.close().await;
    }
}

/// Collected output of [`AgentHandle::exec`].
#[derive(Debug)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A waiter for the `opened`/error reply to an `open_*` message.
type OpenWaiter = tokio::sync::oneshot::Sender<Result<(), OpenRefusal>>;

/// The agent's refusal of an `open_*`, with the machine-readable cause where
/// it sent one.
#[derive(Debug)]
struct OpenRefusal {
    msg: String,
    cause: Option<ErrorCause>,
}

/// An `open_*` that produced no channel. `error` is what an ordinary caller
/// reports; `cause` is what a tunnel branches on.
struct OpenError {
    error: anyhow::Error,
    cause: Option<ErrorCause>,
}

impl OpenError {
    /// A failure the agent gave no machine-readable cause for.
    fn plain(error: anyhow::Error) -> Self {
        Self { error, cause: None }
    }
}

/// Why [`AgentHandle::open_tunnel`] produced no tunnel. The split is the one
/// PRD §19.5 requires: the SSH facade answers `SSH_OPEN_CONNECT_FAILED` for a
/// connect failure and keeps `ADMINISTRATIVELY_PROHIBITED` for a refusal, so
/// a SOCKS client can tell "nothing is listening" from "vmlab refused you".
#[derive(Debug)]
pub enum TunnelError {
    /// The guest dialled the destination and did not get through: nothing
    /// listening, the name did not resolve, the route is dead.
    ConnectFailed(String),
    /// No dial happened. The agent has no `tunnel` feature, the channel
    /// never came up, or the agent connection is gone.
    Refused(String),
}

impl std::fmt::Display for TunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TunnelError::ConnectFailed(m) | TunnelError::Refused(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for TunnelError {}

impl Drop for Inner {
    fn drop(&mut self) {
        // Last handle gone: abort the reader so its blocked `read()` drops
        // the socket's read half (see the `reader` field for why).
        if let Some(task) = self.reader.lock_recover().take() {
            task.abort();
        }
    }
}

async fn reader_task(weak: Weak<Inner>, mut read_half: tokio::net::unix::OwnedReadHalf) {
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match read_half.read(&mut buf).await {
            Ok(0) | Err(_) => break, // QEMU gone / handle dropped
            Ok(n) => n,
        };
        let Some(inner) = weak.upgrade() else { break };
        decoder.push(&buf[..n]);
        while let Some(frame) = decoder.next_frame() {
            match frame.kind {
                FrameKind::Ctrl => match serde_json::from_slice::<AgentMsg>(&frame.payload) {
                    Ok(msg) => handle_ctrl(&inner, msg).await,
                    Err(e) => tracing::warn!("agent: unparseable ctl frame: {e}"),
                },
                FrameKind::Data => {
                    deliver(&inner, frame.channel, SessionEvent::Data(frame.payload)).await
                }
                FrameKind::DataErr => {
                    deliver(&inner, frame.channel, SessionEvent::Stderr(frame.payload)).await
                }
            }
        }
    }
    // Connection over: close every session queue so consumers see None, and
    // drop any open-waiters.
    if let Some(inner) = weak.upgrade() {
        let mut sessions = inner.sessions.lock().await;
        for (_, entry) in sessions.drain() {
            entry.credit.close();
        }
        inner.open_waiters.lock_recover().clear();
    }
}

async fn handle_ctrl(inner: &Arc<Inner>, msg: AgentMsg) {
    match msg {
        AgentMsg::Hello {
            proto_version,
            agent_version,
            os,
            features,
            token,
        } => {
            if token != inner.token {
                // Stale reply from before a snapshot restore — not ours.
                return;
            }
            if proto_version != PROTO_VERSION {
                tracing::error!(
                    "agent speaks proto v{proto_version}, host expects v{PROTO_VERSION} — \
                     rebuild the template to update its agent"
                );
                return;
            }
            let _ = inner.hello.send(Some(AgentInfo {
                agent_version,
                os,
                features,
            }));
        }
        AgentMsg::Opened { id } => {
            if let Some(w) = inner.open_waiters.lock_recover().remove(&id) {
                let _ = w.send(Ok(()));
            }
        }
        AgentMsg::Error {
            id: Some(id),
            msg,
            cause,
        } => {
            let waiter = inner.open_waiters.lock_recover().remove(&id);
            match waiter {
                Some(w) => {
                    let _ = w.send(Err(OpenRefusal { msg, cause }));
                }
                // Mid-stream failures carry no cause worth branching on: the
                // one coded failure (a tunnel's dial) happens before `opened`.
                None => deliver(inner, id, SessionEvent::Error(msg)).await,
            }
        }
        AgentMsg::Error { id: None, msg, .. } => {
            tracing::warn!("agent error: {msg}");
        }
        AgentMsg::Eof { id } => deliver(inner, id, SessionEvent::Eof).await,
        AgentMsg::Exited { id, code } => deliver(inner, id, SessionEvent::Exited(code)).await,
        AgentMsg::FileDone { id, sha256, len } => {
            deliver(inner, id, SessionEvent::FileDone { sha256, len }).await
        }
        AgentMsg::WindowAdjust { id, bytes } => {
            if let Some(entry) = inner.sessions.lock().await.get(&id) {
                entry.credit.grant(bytes);
            }
        }
        AgentMsg::Metrics {
            cpu_pct,
            mem_used,
            mem_total,
            disks,
        } => {
            let _ = inner.metrics.send(Some(MetricsSnapshot {
                cpu_pct,
                mem_used,
                mem_total,
                disks,
            }));
        }
        AgentMsg::Clipboard { text } => {
            let seq = inner.clipboard.borrow().0 + 1;
            let _ = inner.clipboard.send((seq, text));
        }
        AgentMsg::NetInfo { interfaces } => {
            let seq = inner.net_info.borrow().0 + 1;
            let _ = inner.net_info.send((seq, interfaces));
        }
        AgentMsg::OsInfo { info } => {
            let seq = inner.os_info.borrow().0 + 1;
            let _ = inner.os_info.send((seq, Some(info)));
        }
        AgentMsg::ShuttingDown { mode } => {
            tracing::debug!("agent acked shutdown ({mode:?})");
            let seq = *inner.shutting_down.borrow() + 1;
            let _ = inner.shutting_down.send(seq);
        }
        AgentMsg::Pong => {
            let seq = *inner.pong.borrow() + 1;
            let _ = inner.pong.send(seq);
        }
    }
}

/// Route a session event; a consumer stuck past [`STALL_TIMEOUT`] gets its
/// session closed (the rest of the mux keeps flowing).
async fn deliver(inner: &Arc<Inner>, id: u32, ev: SessionEvent) {
    let terminal = matches!(
        ev,
        SessionEvent::Exited(_) | SessionEvent::FileDone { .. } | SessionEvent::Error(_)
    );
    let tx = {
        let sessions = inner.sessions.lock().await;
        sessions.get(&id).map(|e| e.tx.clone())
    };
    let Some(tx) = tx else { return }; // late frames after close: normal
    if tokio::time::timeout(STALL_TIMEOUT, tx.send(ev))
        .await
        .is_err()
    {
        tracing::warn!("agent session {id}: consumer stalled >10s, closing that session");
        if let Some(entry) = inner.sessions.lock().await.remove(&id) {
            entry.credit.close();
        }
        let mut w = inner.writer.lock().await;
        let _ = w.write_all(&encode_ctrl(&HostMsg::Close { id })).await;
        return;
    }
    if terminal {
        // The agent already dropped its side; free the entry (the consumer
        // keeps draining what's queued).
        if let Some(entry) = inner.sessions.lock().await.remove(&id) {
            entry.credit.close();
        }
    }
}

/// Re-expose one terminal session as a raw-byte unix socket at `sock_path`:
/// the first client to connect is bridged to the session; when it hangs up
/// (or the shell exits) the session closes and the socket is unlinked.
/// Nobody connecting within a minute also closes it.
pub async fn expose_terminal_socket(session: AgentSession, sock_path: PathBuf) -> Result<()> {
    let _ = std::fs::remove_file(&sock_path);
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    tokio::spawn(async move {
        let mut session = session;
        let accepted = tokio::time::timeout(Duration::from_secs(60), listener.accept()).await;
        let stream = match accepted {
            Ok(Ok((stream, _))) => stream,
            _ => {
                session.close().await;
                let _ = std::fs::remove_file(&sock_path);
                return;
            }
        };
        let (mut client_rx, mut client_tx) = stream.into_split();
        let mut buf = [0u8; 8 * 1024];
        loop {
            tokio::select! {
                n = client_rx.read(&mut buf) => {
                    match n {
                        Ok(0) | Err(_) => break, // client hung up
                        Ok(n) => {
                            if session.send(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                ev = session.recv() => {
                    match ev {
                        Some(SessionEvent::Data(b)) => {
                            if client_tx.write_all(&b).await.is_err() {
                                break;
                            }
                        }
                        Some(SessionEvent::Exited(_)) | Some(SessionEvent::Error(_)) | None => break,
                        Some(_) => {}
                    }
                }
            }
        }
        session.close().await;
        let _ = std::fs::remove_file(&sock_path);
    });
    Ok(())
}

/// Match the first non-loopback IPv4 address reported for each requested
/// MAC. MACs compare case-insensitively; result order follows `macs`, which
/// is the configuration's NIC declaration order.
pub fn ipv4_by_mac(interfaces: &[NetInterface], macs: &[String]) -> Vec<Option<String>> {
    macs.iter()
        .map(|want| {
            interfaces
                .iter()
                .find(|iface| {
                    iface
                        .mac
                        .as_deref()
                        .is_some_and(|actual| actual.eq_ignore_ascii_case(want))
                })
                .and_then(|iface| {
                    iface
                        .ipv4
                        .iter()
                        .find(|address| !address.starts_with("127."))
                        .cloned()
                })
        })
        .collect()
}

/// Enumerate a host file or directory tree for a guest push: one
/// `(local file, guest path, unix mode bits)` triple per regular file.
/// Guest paths join with `/` regardless of guest OS (the agent normalises).
/// Symlinks are followed; a depth cap turns symlink cycles into an error
/// instead of an unbounded walk.
pub fn walk_tree_for_push(
    src: &Path,
    guest_dest: &str,
) -> Result<Vec<(PathBuf, String, Option<u32>)>> {
    const MAX_DEPTH: usize = 64;
    fn mode_of(p: &Path) -> Option<u32> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    }
    if !src.is_dir() {
        return Ok(vec![(
            src.to_path_buf(),
            guest_dest.to_string(),
            mode_of(src),
        )]);
    }
    let sep = if guest_dest.ends_with('/') || guest_dest.ends_with('\\') {
        ""
    } else {
        "/"
    };
    let mut out = Vec::new();
    let mut stack = vec![(src.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            bail!(
                "tree under {} exceeds {MAX_DEPTH} directory levels (symlink cycle?)",
                src.display()
            );
        }
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("reading directory {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push((path, depth + 1));
            } else {
                let rel = path.strip_prefix(src)?.to_string_lossy().replace('\\', "/");
                let mode = mode_of(&path);
                out.push((path, format!("{guest_dest}{sep}{rel}"), mode));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use vmlab_agent_proto::Frame;
    use vmlab_agent_proto::watch::{EntryKind, Stat};

    const HANDSHAKE: Duration = Duration::from_secs(5);

    /// A minimal in-process agent speaking the real frame protocol over a
    /// unix socket, mirroring what `guest/agent` does: echo terminals,
    /// canned exec output, in-memory file store, echo tunnels.
    async fn mock_agent(answer_hello: bool) -> (tempfile::TempDir, PathBuf) {
        mock_agent_with(
            answer_hello,
            vec![
                "terminal".into(),
                "exec".into(),
                "file".into(),
                "tunnel".into(),
                "watch".into(),
            ],
        )
        .await
    }

    /// The same agent, advertising exactly `features` — for the callers that
    /// have to see an agent *without* one.
    async fn mock_agent_with(
        answer_hello: bool,
        advertised: Vec<String>,
    ) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&path).expect("bind mock agent socket");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (mut rx, tx) = stream.into_split();
            let tx = Arc::new(Mutex::new(tx));
            let send = |msg: AgentMsg| {
                let tx = tx.clone();
                async move {
                    let _ = tx.lock().await.write_all(&encode_ctrl(&msg)).await;
                }
            };
            let send_data = |id: u32, payload: Vec<u8>| {
                let tx = tx.clone();
                async move {
                    let _ = tx
                        .lock()
                        .await
                        .write_all(&encode_frame(FrameKind::Data, id, &payload))
                        .await;
                }
            };

            let mut dec = FrameDecoder::new();
            let mut buf = [0u8; 8192];
            // Channel kinds the mock tracks.
            let mut terminals: Vec<u32> = Vec::new();
            let mut tunnels: Vec<u32> = Vec::new();
            let mut pushes: HashMap<u32, Vec<u8>> = HashMap::new();
            // Watch channels: the root the host named, its record decoder,
            // and how many drains it has asked for.
            let mut watches: HashMap<u32, (String, RecordDecoder, usize)> = HashMap::new();
            let mut pulled = b"pulled-file-content".repeat(1000);
            pulled.truncate(10_000);
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
                                    // A stale hello first — the host must
                                    // ignore it (wrong token).
                                    send(AgentMsg::Hello {
                                        proto_version: PROTO_VERSION,
                                        agent_version: "stale".into(),
                                        os: "linux".into(),
                                        features: vec![],
                                        token: "not-your-token".into(),
                                    })
                                    .await;
                                    if answer_hello {
                                        send(AgentMsg::Hello {
                                            proto_version: PROTO_VERSION,
                                            agent_version: "0.1.0-mock".into(),
                                            os: "linux".into(),
                                            features: advertised.clone(),
                                            token,
                                        })
                                        .await;
                                    }
                                }
                                HostMsg::Ping => send(AgentMsg::Pong).await,
                                HostMsg::OpenTerminal { id, command, .. } => {
                                    if command.as_deref() == Some(&["/no/shell".to_string()]) {
                                        send(AgentMsg::Error {
                                            id: Some(id),
                                            msg: "terminal: no shell found".into(),
                                            cause: None,
                                        })
                                        .await;
                                    } else {
                                        terminals.push(id);
                                        send(AgentMsg::Opened { id }).await;
                                        send_data(id, b"prompt$ ".to_vec()).await;
                                    }
                                }
                                HostMsg::Resize { id, cols, rows } => {
                                    send_data(id, format!("resized:{cols}x{rows}").into_bytes())
                                        .await;
                                }
                                HostMsg::OpenExec {
                                    id, argv, logon, ..
                                } => {
                                    send(AgentMsg::Opened { id }).await;
                                    // Echo who the open said to run as, so a
                                    // test can assert the identity actually
                                    // reached the wire (PRD §19.2).
                                    let who = logon
                                        .map(|l| format!(" as:{}:{}", l.user, l.elevated))
                                        .unwrap_or_default();
                                    send_data(
                                        id,
                                        format!("ran:{}{who}", argv.join(" ")).into_bytes(),
                                    )
                                    .await;
                                    let _ = tx
                                        .lock()
                                        .await
                                        .write_all(&encode_frame(
                                            FrameKind::DataErr,
                                            id,
                                            b"warning-line",
                                        ))
                                        .await;
                                    send(AgentMsg::Exited { id, code: 42 }).await;
                                }
                                // Port 0 stands in for a dead destination and
                                // `refused.invalid` for something vmlab
                                // itself will not do; everything else is an
                                // echo tunnel.
                                HostMsg::OpenTunnel { id, host, port } => {
                                    if port == 0 {
                                        send(AgentMsg::Error {
                                            id: Some(id),
                                            msg: format!(
                                                "tunnel {host}:{port}: \
                                                          connection refused"
                                            ),
                                            cause: Some(ErrorCause::ConnectFailed),
                                        })
                                        .await;
                                    } else if host == "refused.invalid" {
                                        send(AgentMsg::Error {
                                            id: Some(id),
                                            msg: "not today".into(),
                                            cause: None,
                                        })
                                        .await;
                                    } else {
                                        tunnels.push(id);
                                        send(AgentMsg::Opened { id }).await;
                                    }
                                }
                                HostMsg::OpenFilePush { id, .. } => {
                                    pushes.insert(id, Vec::new());
                                    send(AgentMsg::Opened { id }).await;
                                }
                                HostMsg::OpenFilePull { id, .. } => {
                                    send(AgentMsg::Opened { id }).await;
                                    use sha2::{Digest, Sha256};
                                    for chunk in pulled.chunks(4096) {
                                        send_data(id, chunk.to_vec()).await;
                                    }
                                    send(AgentMsg::FileDone {
                                        id,
                                        sha256: hex::encode(Sha256::digest(&pulled)),
                                        len: pulled.len() as u64,
                                    })
                                    .await;
                                }
                                // A watch: the root must exist, the channel
                                // nudges once, and each drain is answered
                                // with a batch (then a rescan).
                                HostMsg::OpenWatch { id, path, prune } => {
                                    if path.contains("vanished") {
                                        send(AgentMsg::Error {
                                            id: Some(id),
                                            msg: format!("watch root {path} is gone"),
                                            cause: None,
                                        })
                                        .await;
                                    } else {
                                        assert_eq!(prune, vec!["node_modules".to_string()]);
                                        watches.insert(id, (path, RecordDecoder::new(), 0));
                                        send(AgentMsg::Opened { id }).await;
                                        send_data(id, encode_record(&WatchRecord::Dirty)).await;
                                    }
                                }
                                HostMsg::Eof { id } => {
                                    if let Some(data) = pushes.remove(&id) {
                                        use sha2::{Digest, Sha256};
                                        send(AgentMsg::FileDone {
                                            id,
                                            sha256: hex::encode(Sha256::digest(&data)),
                                            len: data.len() as u64,
                                        })
                                        .await;
                                    }
                                    // A tunnel peer that sees the FIN shuts
                                    // its own write half — a half-close, not
                                    // the end of the channel.
                                    if tunnels.contains(&id) {
                                        send(AgentMsg::Eof { id }).await;
                                    }
                                }
                                HostMsg::Close { id } => {
                                    if terminals.contains(&id) {
                                        send(AgentMsg::Exited { id, code: 137 }).await;
                                    }
                                }
                                HostMsg::SubscribeMetrics { .. } => {
                                    send(AgentMsg::Metrics {
                                        cpu_pct: 12.5,
                                        mem_used: 100,
                                        mem_total: 200,
                                        disks: vec![],
                                    })
                                    .await;
                                }
                                HostMsg::NetInfo => {
                                    send(AgentMsg::NetInfo {
                                        interfaces: vec![NetInterface {
                                            name: "eth0".into(),
                                            mac: Some("52:54:00:AA:BB:01".into()),
                                            ipv4: vec!["10.0.0.15".into()],
                                            ipv6: vec![],
                                        }],
                                    })
                                    .await;
                                }
                                HostMsg::OsInfo => {
                                    send(AgentMsg::OsInfo {
                                        info: OsInfo {
                                            id: "mocklinux".into(),
                                            name: "Mock Linux".into(),
                                            version: "1.0".into(),
                                            kernel: "6.0.0".into(),
                                            arch: "x86_64".into(),
                                            hostname: "mock0".into(),
                                        },
                                    })
                                    .await;
                                }
                                HostMsg::Shutdown { mode } => {
                                    send(AgentMsg::ShuttingDown { mode }).await;
                                }
                                _ => {}
                            }
                        }
                        FrameKind::Data => {
                            if let Some((root, decoder, drains)) = watches.get_mut(&channel) {
                                decoder.push(&payload);
                                while let Some(record) = decoder.next_record().unwrap() {
                                    assert_eq!(record, WatchRecord::Drain);
                                    *drains += 1;
                                    let answer = if *drains == 1 {
                                        // Big enough to span many frames, so
                                        // the host reassembles one record out
                                        // of the channel's byte stream.
                                        let mut entries = vec![StatRecord::tombstone("gone.txt")];
                                        entries.extend((0..3000).map(|i| StatRecord {
                                            path: format!("{root}/f{i}"),
                                            stat: Some(Stat {
                                                kind: EntryKind::File,
                                                size: i,
                                                mtime_ns: 1_700_000_000_000_000_000,
                                            }),
                                        }));
                                        WatchRecord::Batch { entries }
                                    } else {
                                        WatchRecord::Rescan
                                    };
                                    for chunk in encode_record(&answer).chunks(4096) {
                                        send_data(channel, chunk.to_vec()).await;
                                    }
                                }
                            } else if let Some(data) = pushes.get_mut(&channel) {
                                data.extend(payload);
                            } else {
                                // Echo terminal.
                                send_data(channel, payload).await;
                            }
                        }
                        FrameKind::DataErr => {}
                    }
                }
            }
        });
        (dir, path)
    }

    /// QEMU's `server=on` chardev serves one client at a time: the next
    /// connect only proceeds once the previous socket fully closes. Both
    /// dropping the last handle and an explicit shutdown must free the slot
    /// (the reader task must not keep the read half alive) — this is the
    /// snapshot-restore reconnect path.
    #[tokio::test]
    async fn dropped_and_shutdown_handles_free_the_one_client_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(async move {
            // Serve clients strictly sequentially, like QEMU.
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut dec = FrameDecoder::new();
                let mut buf = [0u8; 4096];
                'client: loop {
                    let n = match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break 'client,
                        Ok(n) => n,
                    };
                    dec.push(&buf[..n]);
                    while let Some(f) = dec.next_frame() {
                        if f.kind != FrameKind::Ctrl {
                            continue;
                        }
                        if let Ok(HostMsg::Hello { token, .. }) =
                            serde_json::from_slice::<HostMsg>(&f.payload)
                        {
                            let reply = encode_ctrl(&AgentMsg::Hello {
                                proto_version: PROTO_VERSION,
                                agent_version: "seq-mock".into(),
                                os: "linux".into(),
                                features: vec![],
                                token,
                            });
                            let _ = stream.write_all(&reply).await;
                        }
                    }
                }
            }
        });

        // First connection handshakes, then is dropped.
        let first = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        drop(first);
        // Second connect must not hang in the backlog behind the first.
        let second = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        // Explicit shutdown (the restore path) frees the slot too, even
        // with a session-held clone still alive.
        let keep_alive = second.clone();
        second.shutdown().await;
        drop(second);
        let third = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        assert_eq!(third.info().agent_version, "seq-mock");
        drop(keep_alive);
    }

    #[tokio::test]
    async fn handshake_reports_info_and_ignores_stale_hello() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let info = agent.info();
        assert_eq!(info.agent_version, "0.1.0-mock");
        assert_eq!(info.os, "linux");
        assert!(agent.has_feature("terminal"));
        assert!(!agent.has_feature("clipboard"));
        assert!(agent.ping(Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn handshake_times_out_when_nothing_answers() {
        let (_dir, path) = mock_agent(false).await;
        let Err(err) = AgentHandle::connect(&path, Duration::from_millis(300)).await else {
            panic!("expected handshake timeout");
        };
        assert!(err.to_string().contains("no vmlab-agent answered"), "{err}");
    }

    #[tokio::test]
    async fn terminal_echoes_and_resizes() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let mut session = agent.open_terminal(80, 24, None, None).await.unwrap();
        match session.recv().await.unwrap() {
            SessionEvent::Data(b) => assert_eq!(b, b"prompt$ "),
            other => panic!("expected prompt, got {other:?}"),
        }
        session.send(b"ls\r").await.unwrap();
        match session.recv().await.unwrap() {
            SessionEvent::Data(b) => assert_eq!(b, b"ls\r"),
            other => panic!("expected echo, got {other:?}"),
        }
        agent.resize(session.id, 132, 43).await.unwrap();
        match session.recv().await.unwrap() {
            SessionEvent::Data(b) => assert_eq!(b, b"resized:132x43"),
            other => panic!("expected resize marker, got {other:?}"),
        }
        session.close().await;
    }

    #[tokio::test]
    async fn terminal_open_failure_is_an_error() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let Err(err) = agent
            .open_terminal(80, 24, Some(vec!["/no/shell".into()]), None)
            .await
        else {
            panic!("expected open failure");
        };
        assert!(err.to_string().contains("no shell found"), "{err}");
    }

    /// PRD §19.5: identity rides per-open and self-contained — the host puts
    /// the whole triple on every open rather than handing out a handshake id,
    /// so a re-handshake after a snapshot restore costs nothing.
    #[tokio::test]
    async fn an_open_carries_the_whole_logon_not_a_handle() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let out = agent
            .exec(
                vec!["whoami".into()],
                vec![],
                None,
                None,
                Duration::from_secs(5),
                Some(Logon {
                    user: r"PROBE\dev".into(),
                    secret: "vmlab123!".into(),
                    elevated: true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, br"ran:whoami as:PROBE\dev:true");
    }

    #[tokio::test]
    async fn exec_collects_streams_and_exit_code() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let out = agent
            .exec(
                vec!["echo".into(), "hi".into()],
                vec![],
                None,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 42);
        assert_eq!(out.stdout, b"ran:echo hi");
        assert_eq!(out.stderr, b"warning-line");
    }

    /// The tunnel is a plain byte pipe, and each direction ends on its own:
    /// the host's `eof` does not take the channel down, and the guest's
    /// answer arrives as an event rather than as a dead session.
    #[tokio::test]
    async fn tunnel_carries_bytes_and_reports_a_half_close() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let mut session = agent
            .open_tunnel("db.internal".into(), 5432)
            .await
            .expect("tunnel opens");
        session.send(b"SELECT 1").await.unwrap();
        match session.recv().await.unwrap() {
            SessionEvent::Data(b) => assert_eq!(b, b"SELECT 1"),
            other => panic!("expected the echo, got {other:?}"),
        }
        session.eof().await.unwrap();
        assert!(matches!(session.recv().await.unwrap(), SessionEvent::Eof));
    }

    /// A SOCKS client has to tell "nothing is listening" from "vmlab refused
    /// you", so the two arrive as different errors and never as one string a
    /// caller has to parse.
    #[tokio::test]
    async fn tunnel_connect_failure_is_distinct_from_a_refusal() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();

        let Err(TunnelError::ConnectFailed(msg)) = agent.open_tunnel("db.internal".into(), 0).await
        else {
            panic!("expected a connect failure");
        };
        assert!(msg.contains("connection refused"), "{msg}");

        let Err(TunnelError::Refused(msg)) =
            agent.open_tunnel("refused.invalid".into(), 5432).await
        else {
            panic!("expected a refusal");
        };
        assert!(msg.contains("not today"), "{msg}");
    }

    /// An agent too old to tunnel is a refusal, named as one, and the guest
    /// is never asked to dial.
    #[tokio::test]
    async fn tunnel_without_the_feature_is_refused() {
        let (_dir, path) = mock_agent_with(true, vec!["terminal".into()]).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let Err(TunnelError::Refused(msg)) = agent.open_tunnel("db.internal".into(), 5432).await
        else {
            panic!("expected a refusal");
        };
        assert!(msg.contains("repair-agent"), "{msg}");
    }

    #[tokio::test]
    async fn push_and_pull_verify_digests() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let work = tempfile::tempdir().unwrap();

        let local = work.path().join("upload.bin");
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&local, &payload).unwrap();
        let (sha, len) = agent
            .push_file(&local, "/guest/upload.bin", None)
            .await
            .unwrap();
        assert_eq!(len, payload.len() as u64);
        use sha2::{Digest, Sha256};
        assert_eq!(sha, hex::encode(Sha256::digest(&payload)));

        let dest = work.path().join("download.bin");
        let (_sha, len) = agent.pull_file("/guest/some-file", &dest).await.unwrap();
        assert_eq!(len, 10_000);
        assert_eq!(std::fs::read(&dest).unwrap().len(), 10_000);
    }

    /// The inline form of a pull: the same transfer, verified the same way,
    /// handed back as bytes instead of written to a host path.
    #[tokio::test]
    async fn pull_bytes_returns_the_verified_file() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let (sha, bytes) = agent.pull_bytes("/guest/some-file", 1 << 20).await.unwrap();
        assert_eq!(bytes.len(), 10_000);
        use sha2::{Digest, Sha256};
        assert_eq!(sha, hex::encode(Sha256::digest(&bytes)));
    }

    /// A file too big to come back inline is refused while it streams, so
    /// nothing buffers past the ceiling and the caller is told the limit
    /// rather than handed a truncated file.
    #[tokio::test]
    async fn pull_bytes_refuses_a_file_over_the_ceiling() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let err = agent
            .pull_bytes("/guest/some-file", 4_096)
            .await
            .unwrap_err();
        let coded = crate::proto::CommandError::from(err);
        assert_eq!(coded.code, crate::proto::ErrorCode::InvalidArgument);
        assert!(coded.message.contains("4096"), "{}", coded.message);
    }

    #[tokio::test]
    async fn net_osinfo_and_shutdown_round_trip() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();

        let ifaces = agent.net_interfaces(Duration::from_secs(5)).await.unwrap();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].ipv4, vec!["10.0.0.15".to_string()]);
        // MAC matching is case-insensitive and follows declaration order.
        let ips = ipv4_by_mac(
            &ifaces,
            &["aa:aa:aa:aa:aa:aa".into(), "52:54:00:aa:bb:01".into()],
        );
        assert_eq!(ips, vec![None, Some("10.0.0.15".to_string())]);

        let info = agent.osinfo(Duration::from_secs(5)).await.unwrap();
        assert_eq!(info.id, "mocklinux");
        assert_eq!(info.hostname, "mock0");

        agent
            .shutdown_guest(ShutdownMode::Powerdown, Duration::from_secs(5))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stats_subscribes_and_returns_a_sample() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let m = agent.stats(Duration::from_secs(5)).await.unwrap();
        assert_eq!(m.mem_total, 200);
        assert_eq!(m.cpu_pct, 12.5);
    }

    #[tokio::test]
    async fn exposed_terminal_socket_bridges_a_client() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let session = agent.open_terminal(80, 24, None, None).await.unwrap();
        let work = tempfile::tempdir().unwrap();
        let sock = work.path().join("term-1.sock");
        expose_terminal_socket(session, sock.clone()).await.unwrap();

        let mut client = UnixStream::connect(&sock).await.unwrap();
        // Prompt arrives through the bridge.
        let mut got = Vec::new();
        while !got.ends_with(b"prompt$ ") {
            let mut b = [0u8; 256];
            let n = client.read(&mut b).await.unwrap();
            assert!(n > 0, "bridge closed early");
            got.extend(&b[..n]);
        }
        // Keystrokes echo back through the bridge.
        client.write_all(b"whoami\r").await.unwrap();
        let mut b = [0u8; 256];
        let n = client.read(&mut b).await.unwrap();
        assert_eq!(&b[..n], b"whoami\r");
        // Hanging up unlinks the socket (session closed on the agent side).
        drop(client);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while sock.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "socket not unlinked"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The whole watch contract from the host's side: one nudge, a drain
    /// answered with stat records reassembled out of the channel's byte
    /// stream, and a second drain answered with the one overflow value.
    #[tokio::test]
    async fn watch_nudges_then_drains_into_records_and_a_rescan() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let mut watch = agent
            .open_watch("/home/dev/work".into(), vec!["node_modules".into()])
            .await
            .unwrap();

        assert!(matches!(watch.recv().await, Some(WatchReport::Dirty)));

        watch.drain().await.unwrap();
        let Some(WatchReport::Batch(entries)) = watch.recv().await else {
            panic!("expected a batch");
        };
        assert_eq!(entries.len(), 3001);
        // A tombstone is the absence of a stat, not a kind of its own.
        assert_eq!(entries[0].path, "gone.txt");
        assert_eq!(entries[0].stat, None);
        let last = entries.last().unwrap();
        assert_eq!(last.path, "/home/dev/work/f2999");
        assert_eq!(last.stat.as_ref().unwrap().kind, EntryKind::File);

        watch.drain().await.unwrap();
        assert!(matches!(watch.recv().await, Some(WatchReport::Rescan)));
        watch.close().await;
    }

    /// The root vanishing fails the channel by name, so a halt can say *the
    /// workspace directory is gone* rather than *the guest deleted 4 000
    /// files*.
    #[tokio::test]
    async fn a_vanished_watch_root_fails_the_open_by_name() {
        let (_dir, path) = mock_agent(true).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let Err(err) = agent
            .open_watch("/home/dev/vanished".into(), vec!["node_modules".into()])
            .await
        else {
            panic!("expected the open to fail");
        };
        assert!(err.to_string().contains("/home/dev/vanished"), "{err}");
    }

    /// An agent too old to watch says so where the caller can act on it,
    /// instead of timing out on a channel that never opens.
    #[tokio::test]
    async fn watching_an_agent_without_the_feature_is_refused() {
        let (_dir, path) = mock_agent_with(true, vec!["terminal".into()]).await;
        let agent = AgentHandle::connect(&path, HANDSHAKE).await.unwrap();
        let Err(err) = agent.open_watch("/home/dev/work".into(), vec![]).await else {
            panic!("expected the open to be refused");
        };
        assert!(err.to_string().contains("rebuild the template"), "{err}");
    }

    #[test]
    fn walk_tree_enumerates_files_with_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkgs/nginx/resources")).unwrap();
        std::fs::write(dir.path().join("playbook.wcl"), "x").unwrap();
        let script = dir.path().join("pkgs/nginx/resources/conf.wscript");
        std::fs::write(&script, "y").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o750)).unwrap();

        let mut entries = walk_tree_for_push(dir.path(), "/weave/pb").unwrap();
        entries.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1, "/weave/pb/pkgs/nginx/resources/conf.wscript");
        assert_eq!(entries[0].2, Some(0o750));
        assert_eq!(entries[1].1, "/weave/pb/playbook.wcl");

        // Trailing slash on the destination doesn't double up.
        let entries = walk_tree_for_push(dir.path(), "/weave/pb/").unwrap();
        assert!(entries.iter().all(|(_, to, _)| !to.contains("//")));

        // A single file maps to the destination verbatim.
        let single = walk_tree_for_push(&script, "C:/weave/x.wscript").unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].1, "C:/weave/x.wscript");
    }
}
