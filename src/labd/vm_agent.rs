//! Host-side client for the in-guest `vmlab-agent` (guest/agent-proto): the
//! `vmlab.agent.0` virtio-serial port carrying framed, multiplexed channels
//! — terminals, streaming exec, file transfer, tails, metrics, clipboard.
//! QEMU owns the socket (`server=on,wait=off`); the daemon connects as the
//! single client, like the QGA client does, and re-exposes each terminal
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

use vmlab_agent_proto::{
    AgentMsg, DiskUsage, FrameDecoder, FrameKind, HostMsg, INITIAL_WINDOW, MAX_PAYLOAD,
    PROTO_VERSION, RecvWindow, encode_ctrl, encode_frame,
};

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
                let mut g = self.avail.lock().unwrap();
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
        let mut g = self.avail.lock().unwrap();
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

struct Inner {
    /// The connection's runtime, so cleanup can be spawned from any thread
    /// (scripts drop sessions on non-runtime threads).
    rt: tokio::runtime::Handle,
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
    /// Whether `subscribe_metrics` has been sent on this connection.
    metrics_subscribed: AtomicBool,
    token: String,
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
            writer: Mutex::new(write_half),
            sessions: Mutex::new(HashMap::new()),
            open_waiters: std::sync::Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            hello: watch::Sender::new(None),
            pong: watch::Sender::new(0),
            metrics: watch::Sender::new(None),
            clipboard: watch::Sender::new((0, String::new())),
            metrics_subscribed: AtomicBool::new(false),
            token,
        });
        let handle = AgentHandle {
            inner: inner.clone(),
        };

        // Reader task: holds only a Weak so dropping every handle closes the
        // connection and ends the task.
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move { reader_task(weak, read_half).await });

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
            return Err(e);
        }
        match tokio::time::timeout(Duration::from_secs(15), opened_rx).await {
            Ok(Ok(Ok(()))) => Ok(session),
            Ok(Ok(Err(msg))) => {
                session.forget().await;
                bail!("{msg}");
            }
            Ok(Err(_)) | Err(_) => {
                session.forget().await;
                let _ = self.send_msg(&HostMsg::Close { id }).await;
                bail!("agent did not open the channel in time");
            }
        }
    }

    /// Interactive shell session. `command` overrides the guest's default
    /// shell.
    pub async fn open_terminal(
        &self,
        cols: u16,
        rows: u16,
        command: Option<Vec<String>>,
    ) -> Result<AgentSession> {
        self.open(|id| HostMsg::OpenTerminal {
            id,
            cols,
            rows,
            command,
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
    ) -> Result<AgentSession> {
        self.open(|id| HostMsg::OpenExec { id, argv, env, cwd })
            .await
    }

    /// Follow a guest file (`tail -F`); the session yields `Data` chunks.
    pub async fn open_tail(&self, path: String) -> Result<AgentSession> {
        self.open(|id| HostMsg::OpenTail { id, path }).await
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
    ) -> Result<ExecOutput> {
        let display = argv.join(" ");
        let mut session = self.open_exec(argv, env, cwd).await?;
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
                Some(SessionEvent::FileDone { .. }) => {}
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

    /// Pull a guest file to the host, returning the verified digest+size.
    pub async fn pull_file(&self, remote: &str, local: &Path) -> Result<(String, u64)> {
        use sha2::{Digest, Sha256};
        if let Some(parent) = local.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let mut file = tokio::fs::File::create(local)
            .await
            .with_context(|| format!("creating {}", local.display()))?;
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
                    file.write_all(&b).await?;
                }
                Some(SessionEvent::FileDone { sha256, len }) => {
                    file.flush().await?;
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
        handle.inner.open_waiters.lock().unwrap().remove(&id);
        // Spawned on the connection's own runtime: a drop on a non-runtime
        // thread (script executors) must not panic.
        let rt = handle.inner.rt.clone();
        rt.spawn(async move {
            handle.inner.sessions.lock().await.remove(&id);
            let _ = handle.send_msg(&HostMsg::Close { id }).await;
        });
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
type OpenWaiter = tokio::sync::oneshot::Sender<Result<(), String>>;

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
        inner.open_waiters.lock().unwrap().clear();
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
            if let Some(w) = inner.open_waiters.lock().unwrap().remove(&id) {
                let _ = w.send(Ok(()));
            }
        }
        AgentMsg::Error { id: Some(id), msg } => {
            let waiter = inner.open_waiters.lock().unwrap().remove(&id);
            match waiter {
                Some(w) => {
                    let _ = w.send(Err(msg));
                }
                None => deliver(inner, id, SessionEvent::Error(msg)).await,
            }
        }
        AgentMsg::Error { id: None, msg } => {
            tracing::warn!("agent error: {msg}");
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use vmlab_agent_proto::Frame;

    const HANDSHAKE: Duration = Duration::from_secs(5);

    /// A minimal in-process agent speaking the real frame protocol over a
    /// unix socket, mirroring what `guest/agent` does: echo terminals,
    /// canned exec output, in-memory file store.
    async fn mock_agent(answer_hello: bool) -> (tempfile::TempDir, PathBuf) {
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
            let mut pushes: HashMap<u32, Vec<u8>> = HashMap::new();
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
                                            features: vec![
                                                "terminal".into(),
                                                "exec".into(),
                                                "file".into(),
                                            ],
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
                                HostMsg::OpenExec { id, argv, .. } => {
                                    send(AgentMsg::Opened { id }).await;
                                    send_data(id, format!("ran:{}", argv.join(" ")).into_bytes())
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
                                _ => {}
                            }
                        }
                        FrameKind::Data => {
                            if let Some(data) = pushes.get_mut(&channel) {
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
        let mut session = agent.open_terminal(80, 24, None).await.unwrap();
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
            .open_terminal(80, 24, Some(vec!["/no/shell".into()]))
            .await
        else {
            panic!("expected open failure");
        };
        assert!(err.to_string().contains("no shell found"), "{err}");
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
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 42);
        assert_eq!(out.stdout, b"ran:echo hi");
        assert_eq!(out.stderr, b"warning-line");
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
        let session = agent.open_terminal(80, 24, None).await.unwrap();
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
}
