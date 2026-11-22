//! The portable heart of the agent: one reader loop decodes frames off the
//! virtio port and dispatches them; a session table maps channel ids to live
//! terminals / execs / transfers; a single writer thread owns port writes
//! (virtio-serial writes block while no host client is attached — see
//! `guest/cinit/src/ctl.rs` for the original discussion of that quirk).
//!
//! Flow control: guest→host payloads consume per-channel [`Credit`] granted
//! by host `window_adjust` messages; host→guest payloads are drained through
//! per-session input channels whose pump threads grant credit back after the
//! bytes are actually consumed (written into the PTY / child stdin / file).

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use vmlab_agent_proto::{
    AgentMsg, Frame, FrameKind, HostMsg, INITIAL_WINDOW, Logon, MAX_PAYLOAD, NetInterface, OsInfo,
    PROTO_VERSION, ShutdownMode, encode_ctrl, encode_frame, valid_tree_name,
};

use crate::app::{AppSpec, Apps};
#[cfg(target_os = "linux")]
use crate::spawn::Identity;
use crate::spawn::{ProcessSpec, Spawner, TerminalSpec};

/// How a platform maps a host-supplied guest path. Shared rather than called
/// through `&dyn Platform` because a file session keeps resolving paths long
/// after the open returned. Fails where the path cannot be resolved at all,
/// a symlink loop inside a container.
pub type PathResolver = Arc<dyn Fn(String, Last) -> std::io::Result<String> + Send + Sync>;

/// Whether a path's last component is followed when it is a symlink.
///
/// It matters where paths are resolved by the agent rather than the kernel,
/// in a container: an `open` or a `stat` goes through a link, while a
/// `remove`, `rename`, `lstat` or `readlink` acts on the link itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Last {
    Follow,
    Keep,
}

/// Host→guest bytes for one session, fed by the reader dispatch and drained
/// by the session's input pump.
pub enum Input {
    Bytes(Vec<u8>),
    /// `HostMsg::Eof` — no more bytes (exec stdin closed; file push done).
    Eof,
}

/// One session's input queue. It is bounded by the bytes it holds, the unit
/// the host's send window counts in, so a host that respects its window can
/// never fill it, whatever size its frames are; and a push never waits, so
/// the agent's one reader thread is never held by a session that has stopped
/// reading (review F6). Small frames are joined, so what the queue holds
/// costs what its bytes cost.
struct InputQueue {
    state: Mutex<InputState>,
    cv: Condvar,
}

#[derive(Default)]
struct InputState {
    items: VecDeque<Input>,
    /// Payload bytes in `items`.
    bytes: usize,
    /// The session was removed: what is queued is still delivered, then the
    /// pump sees the end.
    tx_gone: bool,
    /// The pump is gone: input is discarded.
    rx_gone: bool,
}

/// The host sent more than its window allows.
struct InputOverrun;

impl InputQueue {
    fn push(&self, input: Input) -> Result<(), InputOverrun> {
        let mut g = self.state.lock().unwrap();
        if g.rx_gone {
            return Ok(());
        }
        match input {
            // Nothing for a pump to act on, and no window to bound it by.
            Input::Bytes(b) if b.is_empty() => return Ok(()),
            Input::Bytes(b) => {
                if g.bytes + b.len() > INPUT_QUEUE_BYTES {
                    return Err(InputOverrun);
                }
                g.bytes += b.len();
                match g.items.back_mut() {
                    Some(Input::Bytes(last)) if last.len() + b.len() <= MAX_PAYLOAD => {
                        last.extend_from_slice(&b)
                    }
                    _ => g.items.push_back(Input::Bytes(b)),
                }
            }
            // A second EOF behind an unread one says nothing new.
            Input::Eof if matches!(g.items.back(), Some(Input::Eof)) => return Ok(()),
            Input::Eof => g.items.push_back(Input::Eof),
        }
        self.cv.notify_one();
        Ok(())
    }
}

/// The session table's end of an [`InputQueue`]; dropping it ends the pump's
/// stream once the queue is drained.
struct InputTx(Arc<InputQueue>);

impl Drop for InputTx {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().tx_gone = true;
        self.0.cv.notify_all();
    }
}

/// A session's input, for its pump: iterate it, or [`InputRx::recv`]. Both
/// end when the session is removed and the queue is empty.
pub struct InputRx(Arc<InputQueue>);

/// The session is gone and its input queue is empty.
#[derive(Debug)]
pub struct InputClosed;

impl InputRx {
    /// Block until the host sends something, or the session is removed.
    pub fn recv(&self) -> Result<Input, InputClosed> {
        let mut g = self.0.state.lock().unwrap();
        loop {
            if let Some(input) = g.items.pop_front() {
                if let Input::Bytes(b) = &input {
                    g.bytes -= b.len();
                }
                return Ok(input);
            }
            if g.tx_gone {
                return Err(InputClosed);
            }
            g = self.0.cv.wait(g).unwrap();
        }
    }
}

impl Iterator for InputRx {
    type Item = Input;

    fn next(&mut self) -> Option<Input> {
        self.recv().ok()
    }
}

impl Drop for InputRx {
    fn drop(&mut self) {
        let mut g = self.0.state.lock().unwrap();
        g.rx_gone = true;
        g.items.clear();
        g.bytes = 0;
    }
}

fn input_queue() -> (InputTx, InputRx) {
    let queue = Arc::new(InputQueue {
        state: Mutex::new(InputState::default()),
        cv: Condvar::new(),
    });
    (InputTx(queue.clone()), InputRx(queue))
}

/// Guest→host send credit for one channel. Output pumps block in
/// [`Credit::take`] until the host grants window; closing wakes them with 0.
pub struct Credit {
    avail: Mutex<(u64, bool)>, // (credit, closed)
    cv: Condvar,
}

impl Credit {
    fn new() -> Self {
        Self {
            avail: Mutex::new((INITIAL_WINDOW, false)),
            cv: Condvar::new(),
        }
    }

    /// Block until credit is available (or the channel closed), then take up
    /// to `want` bytes of it. Returns 0 when the channel is closed.
    pub fn take(&self, want: usize) -> usize {
        let mut g = self.avail.lock().unwrap();
        loop {
            if g.1 {
                return 0;
            }
            if g.0 > 0 {
                let n = g.0.min(want as u64).min(MAX_PAYLOAD as u64) as usize;
                g.0 -= n as u64;
                return n;
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    pub fn grant(&self, bytes: u64) {
        let mut g = self.avail.lock().unwrap();
        g.0 = g.0.saturating_add(bytes);
        self.cv.notify_all();
    }

    pub fn close(&self) {
        self.avail.lock().unwrap().1 = true;
        self.cv.notify_all();
    }
}

/// One live channel.
struct Session {
    channel: Channel,
    input: Option<InputTx>,
    credit: Arc<Credit>,
    /// Applies a terminal resize (PTY sessions only).
    resize: Option<Box<dyn Fn(u16, u16) + Send + Sync>>,
    /// Force-stops the session's work (kill the process, stop the tail).
    /// Pump threads notice via closed channels/credit and exit.
    kill: Option<Box<dyn FnOnce() + Send>>,
}

/// Everything sessions need to talk back to the host. Cheap to clone.
#[derive(Clone)]
pub struct Mux {
    inner: Arc<MuxInner>,
    channel: Option<Channel>,
}

#[derive(Clone)]
struct Channel {
    id: u32,
    cancelled: Arc<AtomicBool>,
}

impl Channel {
    fn new(id: u32) -> Self {
        Self {
            id,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct Outbound {
    bytes: Vec<u8>,
    cancelled: Option<Arc<AtomicBool>>,
}

impl Outbound {
    fn cancelled(&self) -> bool {
        self.cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }
}

struct MuxInner {
    /// Encoded frames for the port writer thread. Bounded: senders block
    /// when the host is slow/absent, which is the flow-control backstop for
    /// control traffic (data traffic is credit-limited before it gets here).
    out: SyncSender<Outbound>,
    sessions: Mutex<HashMap<u32, Session>>,
    /// This process, as the handshake and every pong name it: a restarted
    /// agent answers the host's pings on the same channel, and only a
    /// different instance tells the host its handshake is stale.
    instance: String,
    /// Live shared-app sessions (PRD §20), which `open_app_conn` looks up.
    apps: Apps,
}

/// What the handshake says this agent is: the build stamp
/// `guest/build-agent.sh` compiles in (`agent=<rev>`, the same string it
/// writes to the asset's `VERSION`), so the host can tell whether the agent
/// in a guest is the one it would push (§19.4). A plain `cargo build` has no
/// stamp and answers with the crate version.
const AGENT_VERSION: &str = match option_env!("VMLAB_AGENT_STAMP") {
    Some(stamp) => stamp,
    None => env!("CARGO_PKG_VERSION"),
};

/// A name for this agent process no earlier one on the guest is likely to
/// have had: std's per-process random hash keys over the pid and the clock,
/// so it needs no randomness source of its own.
fn new_instance() -> String {
    use std::hash::BuildHasher;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let n = std::collections::hash_map::RandomState::new().hash_one((std::process::id(), now));
    format!("{n:016x}")
}

/// The guest's system clock as `get_time` reports it: milliseconds since the
/// Unix epoch, UTC, negative for a clock set before 1970.
fn unix_ms_now() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// How many encoded frames may queue for the port writer before senders
/// block. Sized to absorb bursts, not to buffer a detached host forever.
const OUT_QUEUE: usize = 256;

/// How many payload bytes a session's input queue holds. The host may have
/// at most `INITIAL_WINDOW` un-granted bytes in flight, and a pump grants
/// only what it has taken off the queue, so the queue never passes this
/// while the host respects the window. A host that does not loses the
/// channel ([`Mux::route_input`]).
const INPUT_QUEUE_BYTES: usize = INITIAL_WINDOW as usize;

/// How long the port writer waits before offering a refused frame again.
const WRITE_RETRY: std::time::Duration = std::time::Duration::from_millis(200);

/// The one write failure the port writer must **not** retry: the platform
/// gave up waiting for the request, but the driver still holds it and will
/// put the bytes on the wire whenever the host drains the port. Offering
/// the frame again would deliver it twice. A port adapter that abandons a
/// write wraps this in the error it returns (see `windows::port`).
#[derive(Debug)]
pub struct WriteAbandoned;

impl std::fmt::Display for WriteAbandoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("port write abandoned; the driver will still deliver it")
    }
}

impl std::error::Error for WriteAbandoned {}

/// Whether a failed write is one the driver will complete on its own.
fn write_was_abandoned(e: &std::io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.is::<WriteAbandoned>())
}

impl Mux {
    /// Start the writer thread over the port's write half and return the mux.
    pub fn new(mut port_w: impl Write + Send + 'static) -> Mux {
        let (out, rx): (SyncSender<Outbound>, Receiver<Outbound>) = sync_channel(OUT_QUEUE);
        thread::spawn(move || {
            for frame in rx {
                // Blocks until the host side is connected — that is the
                // point of doing it on this dedicated thread.
                //
                // A refused frame is offered again rather than dropped: a
                // port connection being reset underneath us (the Windows
                // adapter does that to a write that pends too long) refuses
                // every write until the reader has reopened it, and each
                // frame it refused was a reply or a credit grant some host
                // call is waiting on. Dropping one left that call waiting
                // forever with nothing logged on either side. The one
                // exception is a write the driver still owns, which it will
                // deliver itself.
                let mut told = false;
                while !frame.cancelled() {
                    let Err(e) = port_w.write_all(&frame.bytes) else {
                        break;
                    };
                    if write_was_abandoned(&e) {
                        break;
                    }
                    if !told {
                        eprintln!("vmlab-agent: port write failed ({e}); retrying until it goes");
                        told = true;
                    }
                    thread::sleep(WRITE_RETRY);
                }
                let _ = port_w.flush();
            }
        });
        Mux {
            channel: None,
            inner: Arc::new(MuxInner {
                out,
                sessions: Mutex::new(HashMap::new()),
                instance: new_instance(),
                apps: Apps::default(),
            }),
        }
    }

    /// Bind all replies and cleanup to this opening of a channel. Old
    /// workers retain its cancellation flag even if the host reuses the id.
    /// Whether `other` is a handle onto the same port connection as this.
    pub fn same_port(&self, other: &Mux) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn for_channel(&self, id: u32) -> Self {
        Self {
            inner: self.inner.clone(),
            channel: Some(Channel::new(id)),
        }
    }

    fn send(&self, bytes: Vec<u8>) {
        let frame = Outbound {
            bytes,
            cancelled: self.channel.as_ref().map(|c| c.cancelled.clone()),
        };
        if !frame.cancelled() {
            let _ = self.inner.out.send(frame);
        }
    }

    /// Announce ownership of the legacy serial port without a host request.
    #[cfg(unix)]
    pub fn announce_serial(&self) {
        // One byte between complete frames, skipped by FrameDecoder.
        self.send(vec![0]);
    }

    /// Send a control message to the host.
    pub fn send_ctrl(&self, msg: &AgentMsg) {
        let bytes = encode_ctrl(msg).unwrap_or_else(|error| {
            let detail = error.to_string();
            let reply = match msg {
                AgentMsg::Clipboard { .. } | AgentMsg::ClipboardFailed { .. } => {
                    AgentMsg::ClipboardFailed { msg: detail }
                }
                AgentMsg::TimeSetFailed { .. } => AgentMsg::TimeSetFailed { msg: detail },
                AgentMsg::Error { id, .. } => AgentMsg::Error {
                    id: *id,
                    msg: detail,
                },
                _ => AgentMsg::Error {
                    id: None,
                    msg: detail,
                },
            };
            encode_ctrl(&reply).expect("bounded control error serializes")
        });
        self.send(bytes);
    }

    /// Send channel payload bytes. The caller must have taken credit first.
    pub fn send_data(&self, kind: FrameKind, channel: u32, payload: &[u8]) {
        debug_assert!(payload.len() <= MAX_PAYLOAD);
        self.send(encode_frame(kind, channel, payload));
    }

    /// The shared-app session table.
    pub fn apps(&self) -> &Apps {
        &self.inner.apps
    }

    pub fn send_error(&self, id: Option<u32>, msg: impl Into<String>) {
        self.send_ctrl(&AgentMsg::Error {
            id,
            msg: msg.into(),
        });
    }

    /// Register a new session. Returns the input receiver for the session's
    /// pump and its send credit, or `None` (+ an error to the host) if the
    /// id is already live. Use a channel-bound mux from [`Mux::for_channel`]
    /// for registration and every worker belonging to the session.
    pub fn register(
        &self,
        id: u32,
        resize: Option<Box<dyn Fn(u16, u16) + Send + Sync>>,
        kill: Option<Box<dyn FnOnce() + Send>>,
    ) -> Option<(InputRx, Arc<Credit>)> {
        let mut sessions = self.inner.sessions.lock().unwrap();
        if sessions.contains_key(&id) || id == 0 {
            drop(sessions);
            self.send_error(Some(id), "channel id already in use");
            return None;
        }
        let (tx, rx) = input_queue();
        let credit = Arc::new(Credit::new());
        let channel = self.channel.clone().expect("channel-bound session mux");
        assert_eq!(channel.id, id);
        sessions.insert(
            id,
            Session {
                channel,
                input: Some(tx),
                credit: credit.clone(),
                resize,
                kill,
            },
        );
        Some((rx, credit))
    }

    /// Install a session's kill hook after the fact — for sessions whose
    /// cancel handle only exists once an OS call succeeds (Windows event-log
    /// subscriptions). Runs the hook immediately if the session is already
    /// gone.
    #[cfg(windows)]
    pub fn set_kill(&self, id: u32, kill: Box<dyn FnOnce() + Send>) {
        let mut sessions = self.inner.sessions.lock().unwrap();
        match sessions.get_mut(&id) {
            Some(s) if self.owns(s) => s.kill = Some(kill),
            _ => {
                drop(sessions);
                kill();
            }
        }
    }

    /// Remove a session (host `close`, or the session finished on its own).
    /// Idempotent.
    pub fn remove(&self, id: u32) {
        let session = self.inner.sessions.lock().unwrap().remove(&id);
        if let Some(mut s) = session {
            s.channel.cancelled.store(true, Ordering::SeqCst);
            s.credit.close();
            s.input = None; // drop the sender: input pump sees EOF
            if let Some(kill) = s.kill.take() {
                kill();
            }
        }
    }

    /// Remove a session that ended on its own — like [`Mux::remove`] but
    /// without the kill hook (the process is already reaped; its pid may
    /// have been recycled).
    pub fn remove_finished(&self, id: u32) {
        let session = {
            let mut sessions = self.inner.sessions.lock().unwrap();
            if !sessions.get(&id).is_some_and(|s| self.owns(s)) {
                return;
            }
            sessions.remove(&id)
        };
        if let Some(mut s) = session {
            s.credit.close();
            s.input = None;
            s.kill = None;
        }
    }

    fn owns(&self, session: &Session) -> bool {
        self.channel
            .as_ref()
            .is_none_or(|c| Arc::ptr_eq(&c.cancelled, &session.channel.cancelled))
    }

    /// The port connection was reset under every live session: fail each
    /// one, telling the host why by id.
    ///
    /// A reset loses bytes in both directions — whatever the guest had not
    /// yet read is discarded with the port handle, and the host cannot tell
    /// which of its requests those were. So nothing that was open survives
    /// it: every session is torn down as on a host re-handshake, and the
    /// host hears an error on each, which is what fails the calls waiting
    /// on them promptly instead of leaving them to a watchdog.
    pub fn reset(&self, reason: &str) {
        let ids: Vec<u32> = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        if !ids.is_empty() {
            eprintln!(
                "vmlab-agent: {reason}; failing {} open channel(s)",
                ids.len()
            );
        }
        for id in ids {
            self.remove(id);
            self.send_error(Some(id), format!("channel lost: {reason}"));
        }
    }

    /// Tear down every session (host re-handshake).
    pub fn remove_all(&self) {
        let ids: Vec<u32> = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        for id in ids {
            self.remove(id);
        }
    }

    pub fn resize(&self, id: u32, cols: u16, rows: u16) {
        let sessions = self.inner.sessions.lock().unwrap();
        match sessions.get(&id).and_then(|s| s.resize.as_ref()) {
            Some(f) => f(cols, rows),
            None => {
                drop(sessions);
                self.send_error(Some(id), "resize: no such terminal");
            }
        }
    }

    pub fn grant(&self, id: u32, bytes: u64) {
        if let Some(s) = self.inner.sessions.lock().unwrap().get(&id) {
            s.credit.grant(bytes);
        }
    }

    /// Route host→guest payload bytes / EOF into a session's input queue.
    /// Unknown ids are dropped silently: a `close` crossing in-flight data
    /// is normal, not an error.
    ///
    /// This runs on the agent's one reader thread and never waits for the
    /// session: a pump that has stopped reading leaves its bytes queued, and
    /// every other channel, the pings and this channel's own `close` are
    /// still served. A host that sends past its window is not waited for
    /// either. Its channel is closed and it is told why, the same answer the
    /// host gives a guest that overruns the other direction.
    pub fn route_input(&self, id: u32, input: Input) {
        let overrun = {
            let sessions = self.inner.sessions.lock().unwrap();
            sessions
                .get(&id)
                .and_then(|s| s.input.as_ref())
                .is_some_and(|tx| tx.0.push(input).is_err())
        };
        if overrun {
            self.remove(id);
            self.send_error(
                Some(id),
                format!(
                    "the host sent more than the receive window of {INITIAL_WINDOW} bytes \
                     without credit; channel closed"
                ),
            );
        }
    }

    /// Handle one decoded frame. `platform` supplies the OS-specific
    /// handlers (terminal spawn, clipboard, event log).
    pub fn handle_frame(&self, frame: Frame, platform: &dyn Platform) {
        match frame.kind {
            FrameKind::Ctrl => match serde_json::from_slice::<HostMsg>(&frame.payload) {
                Ok(msg) => self.handle_msg(msg, platform),
                Err(e) => self.send_error(None, format!("bad ctl message: {e}")),
            },
            FrameKind::Data => self.route_input(frame.channel, Input::Bytes(frame.payload)),
            // The host never sends stderr-kind frames; tolerate them as data.
            FrameKind::DataErr => self.route_input(frame.channel, Input::Bytes(frame.payload)),
        }
    }

    fn handle_msg(&self, msg: HostMsg, platform: &dyn Platform) {
        match msg {
            HostMsg::Hello {
                proto_version,
                token,
            } => {
                // Fresh host handshake: whatever the previous host connection
                // had open is gone, its metrics subscription included — a
                // new host that never subscribed must not receive samples.
                self.remove_all();
                crate::metrics::release(self);
                if proto_version != PROTO_VERSION {
                    self.send_error(
                        None,
                        format!(
                            "protocol mismatch: host speaks v{proto_version}, agent v{PROTO_VERSION}"
                        ),
                    );
                }
                self.send_ctrl(&AgentMsg::Hello {
                    proto_version: PROTO_VERSION,
                    agent_version: AGENT_VERSION.to_string(),
                    os: platform.os().to_string(),
                    features: platform.features(),
                    token,
                    instance: Some(self.inner.instance.clone()),
                });
            }
            // Everything a person invokes carries the machine's declared
            // login (PRD §19.2); an open with no `logon` is vmlab's own
            // machinery and keeps the agent identity.
            HostMsg::OpenTerminal {
                id,
                cols,
                rows,
                command,
                env,
                logon,
            } => crate::terminal::open(
                self,
                platform.spawner(),
                &logon.into(),
                id,
                TerminalSpec {
                    command,
                    cols,
                    rows,
                    env,
                },
            ),
            HostMsg::Resize { id, cols, rows } => self.resize(id, cols, rows),
            HostMsg::OpenExec {
                id,
                argv,
                env,
                cwd,
                logon,
                tree,
            } => {
                if let Some(name) = tree.as_deref().filter(|n| !valid_tree_name(n)) {
                    self.send_error(Some(id), format!("exec: bad tree name {name:?}"));
                    return;
                }
                crate::exec::open(
                    self,
                    platform.spawner(),
                    &logon.into(),
                    id,
                    ProcessSpec {
                        argv,
                        env,
                        cwd,
                        tree,
                    },
                )
            }
            HostMsg::Eof { id } => self.route_input(id, Input::Eof),
            // A file session takes the resolver rather than one resolved
            // path: every request in it names its own (§19.5).
            HostMsg::OpenFileOps { id, logon } => crate::fileops::open(
                self,
                platform.spawner(),
                &logon.into(),
                id,
                platform.path_resolver(),
            ),
            HostMsg::OpenTail { id, path, logon } => match platform.resolve_path(path.clone()) {
                Ok(resolved) => {
                    crate::tail::open(self, platform.spawner(), &logon.into(), id, resolved)
                }
                Err(e) => self.send_error(Some(id), format!("tail {path}: {e}")),
            },
            #[cfg(any(target_os = "linux", windows))]
            HostMsg::OpenWatch { id, path, prune } => match platform.resolve_path(path.clone()) {
                Ok(resolved) => crate::watch::open(self, id, resolved, prune),
                Err(e) => self.send_error(Some(id), format!("watch {path}: {e}")),
            },
            // No recursive watch backend here, and `features::WATCH` is not
            // advertised: a host that opens one anyway is told why.
            #[cfg(not(any(target_os = "linux", windows)))]
            HostMsg::OpenWatch { id, .. } => self.send_error(
                Some(id),
                "the workspace watch needs a Linux or Windows guest",
            ),
            HostMsg::OpenEventLog { id, filter } => platform.open_eventlog(self, id, filter),
            HostMsg::SetClipboard { text } => platform.set_clipboard(self, text),
            HostMsg::GetClipboard => platform.get_clipboard(self),
            HostMsg::SubscribeMetrics { interval_secs } => {
                crate::metrics::subscribe(self, interval_secs)
            }
            HostMsg::UnsubscribeMetrics => crate::metrics::unsubscribe(),
            HostMsg::NetInfo => match platform.net_info() {
                Ok(interfaces) => self.send_ctrl(&AgentMsg::NetInfo { interfaces }),
                Err(e) => self.send_error(None, format!("net_info: {e}")),
            },
            HostMsg::OsInfo => match platform.os_info() {
                Ok(info) => self.send_ctrl(&AgentMsg::OsInfo { info }),
                Err(e) => self.send_error(None, format!("os_info: {e}")),
            },
            // Always answered, so a host waiting on it never times out: a
            // tree that cannot be read is reported as drained, and why.
            HostMsg::TreeStatus { tree } => {
                let alive = match valid_tree_name(&tree) {
                    true => platform.tree_status(&tree).unwrap_or_else(|e| {
                        self.send_error(None, format!("tree_status {tree}: {e}"));
                        0
                    }),
                    false => 0,
                };
                self.send_ctrl(&AgentMsg::TreeStatus { tree, alive });
            }
            HostMsg::Shutdown { mode } => {
                // Ack before executing: the reply may be the last bytes this
                // guest ever puts on the wire.
                self.send_ctrl(&AgentMsg::ShuttingDown { mode });
                platform.shutdown(self, mode);
            }
            // Always answered, one way or the other: the host is waiting on
            // it to know whether a restored guest's clock caught up.
            HostMsg::SetTime { unix_ms } => match platform.set_time(unix_ms) {
                Ok(()) => self.send_ctrl(&AgentMsg::TimeSet),
                Err(msg) => self.send_ctrl(&AgentMsg::TimeSetFailed { msg }),
            },
            HostMsg::GetTime => self.send_ctrl(&AgentMsg::Time {
                unix_ms: unix_ms_now(),
            }),
            HostMsg::OpenApp {
                id,
                mode,
                argv,
                env,
                cwd,
                logon,
                split_logs,
            } => crate::app::open(
                self,
                platform,
                id,
                mode,
                AppSpec {
                    argv,
                    env,
                    cwd,
                    split_logs,
                },
                logon,
            ),
            HostMsg::OpenAppConn { id, app } => crate::app::open_conn(self, id, app),
            HostMsg::WindowAdjust { id, bytes } => self.grant(id, bytes),
            HostMsg::Close { id } => self.remove(id),
            HostMsg::Ping => self.send_ctrl(&AgentMsg::Pong {
                instance: Some(self.inner.instance.clone()),
            }),
        }
    }
}

/// The OS-specific half the mux dispatches into.
pub trait Platform: Sync {
    fn os(&self) -> &'static str;
    fn features(&self) -> Vec<String>;
    /// The seam every guest-side process and file handle is created
    /// through. Terminals, exec and file sessions all come from here, so
    /// identity (PRD §19.2) is decided in one place.
    fn spawner(&self) -> &dyn Spawner;
    /// How host-supplied guest paths are mapped — container mode resolves
    /// them inside the container rootfs. Handed out as a value because a
    /// file session outlives the call that opened it and resolves a path per
    /// request, not once.
    fn path_resolver(&self) -> PathResolver {
        Arc::new(|path, _| Ok(path))
    }
    /// Map one host-supplied guest path (tail, watch), which is followed.
    fn resolve_path(&self, path: String) -> std::io::Result<String> {
        (self.path_resolver())(path, Last::Follow)
    }
    fn open_eventlog(&self, mux: &Mux, id: u32, filter: Option<String>);
    fn set_clipboard(&self, mux: &Mux, text: String);
    fn get_clipboard(&self, mux: &Mux);
    /// The guest's network interfaces (loopback excluded).
    fn net_info(&self) -> Result<Vec<NetInterface>, String>;
    /// Structured OS information.
    fn os_info(&self) -> Result<OsInfo, String>;
    /// How many processes of the named exec tree are still alive. Only
    /// asked of a platform advertising `features::TREE`; the default has no
    /// trees to count.
    fn tree_status(&self, _tree: &str) -> Result<u32, String> {
        Ok(0)
    }
    /// Bring the guest down. The `ShuttingDown` ack is already queued;
    /// implementations delay briefly so it flushes, and report failure via
    /// `mux` if the OS refuses.
    fn shutdown(&self, mux: &Mux, mode: ShutdownMode);
    /// Set the guest's system clock to `unix_ms` milliseconds since the Unix
    /// epoch, UTC — a guest restored from an online snapshot wakes at
    /// capture time. Only asked of a platform advertising
    /// `features::SET_TIME`; the error names why the OS refused.
    fn set_time(&self, unix_ms: i64) -> Result<(), String>;
    /// Make the guest able to serve `logon` a RemoteApp session (PRD §20.3):
    /// Remote Desktop on, unlisted programs allowed, the account able to log
    /// on remotely. The error is what the open fails with.
    fn app_rdp_prepare(&self, _logon: &Logon) -> Result<(), String> {
        Err("shared apps over RDP need a Windows guest".into())
    }
    /// Make a waypipe or X11 session's private directory, named `name`,
    /// owned by `identity` with mode 0700, and say where it and the X socket
    /// directory are — for the agent and for the program.
    #[cfg(target_os = "linux")]
    fn app_home(&self, _identity: &Identity, _name: &str) -> Result<crate::app::AppHome, String> {
        Err("shared apps over waypipe and X11 need a Linux guest".into())
    }
}

/// Standard guest→host output pump: read chunks from `src`, respect the
/// session's credit window, forward as `kind` frames. Returns when the
/// source ends or the session closes.
pub fn pump_out(mux: &Mux, id: u32, kind: FrameKind, credit: &Credit, mut src: impl std::io::Read) {
    let mut buf = [0u8; 32 * 1024];
    loop {
        match src.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut off = 0;
                while off < n {
                    let take = credit.take(n - off);
                    if take == 0 {
                        return; // session closed under us
                    }
                    mux.send_data(kind, id, &buf[off..off + take]);
                    off += take;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_spawner::TestPlatform;
    use crate::testutil::capture_mux;
    use vmlab_agent_proto::FrameDecoder;

    #[test]
    fn hello_resets_and_echoes_token() {
        let (mux, mut cap) = capture_mux();
        mux.handle_msg(
            HostMsg::Hello {
                proto_version: PROTO_VERSION,
                token: "tok-1".into(),
            },
            &TestPlatform::new(),
        );
        match cap.ctrl() {
            AgentMsg::Hello {
                proto_version,
                token,
                os,
                features,
                ..
            } => {
                assert_eq!(proto_version, PROTO_VERSION);
                assert_eq!(token, "tok-1");
                assert_eq!(os, "test");
                // Spelled out rather than through `features::*`: what is
                // asserted here is the spelling that lands on the wire.
                assert_eq!(features, vec!["terminal".to_string(), "watch".to_string()]);
            }
            other => panic!("expected hello, got {other:?}"),
        }
    }

    #[test]
    fn ping_pongs_as_the_instance_that_handshook() {
        let (mux, mut cap) = capture_mux();
        let platform = TestPlatform::new();
        mux.handle_msg(
            HostMsg::Hello {
                proto_version: PROTO_VERSION,
                token: "tok-1".into(),
            },
            &platform,
        );
        let AgentMsg::Hello {
            instance: Some(handshook),
            ..
        } = cap.ctrl()
        else {
            panic!("the hello names no instance");
        };
        mux.handle_msg(HostMsg::Ping, &platform);
        assert_eq!(
            cap.ctrl(),
            AgentMsg::Pong {
                instance: Some(handshook.clone())
            }
        );

        // A restarted agent is a new mux, and answers as someone else.
        let (restarted, mut cap) = capture_mux();
        restarted.handle_msg(HostMsg::Ping, &platform);
        assert_ne!(
            cap.ctrl(),
            AgentMsg::Pong {
                instance: Some(handshook)
            }
        );
    }

    #[test]
    fn net_info_replies_with_interfaces() {
        let (mux, mut cap) = capture_mux();
        mux.handle_msg(HostMsg::NetInfo, &TestPlatform::new());
        match cap.ctrl() {
            AgentMsg::NetInfo { interfaces } => {
                assert_eq!(interfaces.len(), 1);
                assert_eq!(interfaces[0].name, "eth0");
            }
            other => panic!("expected net_info, got {other:?}"),
        }
    }

    #[test]
    fn os_info_replies_with_info() {
        let (mux, mut cap) = capture_mux();
        mux.handle_msg(HostMsg::OsInfo, &TestPlatform::new());
        match cap.ctrl() {
            AgentMsg::OsInfo { info } => assert_eq!(info.id, "test"),
            other => panic!("expected os_info, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_acks_before_executing() {
        let (mux, mut cap) = capture_mux();
        mux.handle_msg(
            HostMsg::Shutdown {
                mode: ShutdownMode::Reboot,
            },
            &TestPlatform::new(),
        );
        assert_eq!(
            cap.ctrl(),
            AgentMsg::ShuttingDown {
                mode: ShutdownMode::Reboot
            }
        );
    }

    #[test]
    fn set_time_is_answered_time_set_once_the_clock_is_set() {
        let (mux, mut cap) = capture_mux();
        let platform = TestPlatform::new();
        mux.handle_msg(
            HostMsg::SetTime {
                unix_ms: 1_760_000_000_123,
            },
            &platform,
        );
        assert_eq!(cap.ctrl(), AgentMsg::TimeSet);
        assert_eq!(*platform.clock.lock().unwrap(), vec![1_760_000_000_123]);
    }

    /// `get_time` answers the clock this agent reads, which is the guest's.
    #[test]
    fn get_time_is_answered_with_the_guest_clock() {
        let (mux, mut cap) = capture_mux();
        let before = unix_ms_now();
        mux.handle_msg(HostMsg::GetTime, &TestPlatform::new());
        let after = unix_ms_now();
        match cap.ctrl() {
            AgentMsg::Time { unix_ms } => {
                assert!((before..=after).contains(&unix_ms), "{unix_ms}")
            }
            other => panic!("expected time, got {other:?}"),
        }
    }

    #[test]
    fn set_time_is_answered_time_set_failed_with_the_refusal() {
        let (mux, mut cap) = capture_mux();
        let platform = TestPlatform::new();
        *platform.clock_refusal.lock().unwrap() = Some("privilege not held".into());
        mux.handle_msg(
            HostMsg::SetTime {
                unix_ms: 1_760_000_000_123,
            },
            &platform,
        );
        assert_eq!(
            cap.ctrl(),
            AgentMsg::TimeSetFailed {
                msg: "privilege not held".into()
            }
        );
        assert!(platform.clock.lock().unwrap().is_empty());
    }

    /// A port that refuses a frame for a while — the Windows adapter does,
    /// from the moment a write stalls until the reader has reopened the
    /// port — gets that frame again once it will take it, exactly once. A
    /// dropped frame was a reply or a credit grant some host call waited
    /// on forever.
    #[test]
    fn a_refused_frame_is_offered_again_until_the_port_takes_it() {
        struct Refusing {
            left: usize,
            tx: std::sync::mpsc::Sender<Vec<u8>>,
        }
        impl Write for Refusing {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if self.left > 0 {
                    self.left -= 1;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "port connection is being reset",
                    ));
                }
                let _ = self.tx.send(buf.to_vec());
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mux = Mux::new(Refusing { left: 3, tx });
        mux.send_ctrl(&AgentMsg::Eof { id: 11 });
        mux.send_ctrl(&AgentMsg::Eof { id: 12 });
        let mut dec = FrameDecoder::new();
        let mut got = Vec::new();
        while got.len() < 2 {
            let bytes = rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the refused frame arrives once the port takes it");
            dec.push(&bytes);
            while let Some(f) = dec.next_frame() {
                got.push(serde_json::from_slice::<AgentMsg>(&f.payload).unwrap());
            }
        }
        assert_eq!(
            got,
            vec![AgentMsg::Eof { id: 11 }, AgentMsg::Eof { id: 12 }],
            "each frame exactly once, in order"
        );
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "nothing is delivered twice"
        );
    }

    /// The one refusal that is not retried: a write the driver still owns
    /// and will deliver itself. Offering it again would put the frame on
    /// the wire twice.
    #[test]
    fn an_abandoned_write_is_not_offered_again() {
        struct Abandoning {
            first: bool,
            tx: std::sync::mpsc::Sender<Vec<u8>>,
        }
        impl Write for Abandoning {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if std::mem::take(&mut self.first) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        WriteAbandoned,
                    ));
                }
                let _ = self.tx.send(buf.to_vec());
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mux = Mux::new(Abandoning { first: true, tx });
        mux.send_ctrl(&AgentMsg::Eof { id: 21 });
        mux.send_ctrl(&AgentMsg::Eof { id: 22 });
        let mut dec = FrameDecoder::new();
        dec.push(
            &rx.recv_timeout(std::time::Duration::from_secs(10))
                .expect("the second frame arrives"),
        );
        let f = dec.next_frame().expect("one frame");
        assert_eq!(
            serde_json::from_slice::<AgentMsg>(&f.payload).unwrap(),
            AgentMsg::Eof { id: 22 },
            "the abandoned frame is the driver's to deliver, not ours again"
        );
    }

    /// A port connection reset under live sessions loses bytes the host
    /// cannot account for, so every session dies with it and the host is
    /// told on each by id — which is what fails the calls waiting on them
    /// at once rather than leaving them to a watchdog.
    #[test]
    fn a_port_reset_fails_every_open_channel_by_id() {
        let (mux, mut cap) = capture_mux();
        let killed = Arc::new(Mutex::new(Vec::new()));
        let mut inputs = Vec::new();
        for id in [3u32, 4] {
            let k = killed.clone();
            let (input, credit) = mux
                .for_channel(id)
                .register(id, None, Some(Box::new(move || k.lock().unwrap().push(id))))
                .unwrap();
            inputs.push((input, credit));
        }
        mux.reset("port connection reset (port I/O stalled)");
        let mut told = Vec::new();
        for _ in 0..2 {
            match cap.ctrl() {
                AgentMsg::Error { id: Some(id), msg } => {
                    assert_eq!(
                        msg,
                        "channel lost: port connection reset (port I/O stalled)"
                    );
                    told.push(id);
                }
                other => panic!("expected a channel error, got {other:?}"),
            }
        }
        told.sort();
        assert_eq!(told, vec![3, 4]);
        let mut k = killed.lock().unwrap().clone();
        k.sort();
        assert_eq!(k, vec![3, 4], "each session's kill hook ran");
        for (input, credit) in inputs {
            assert_eq!(credit.take(10), 0, "credit closed");
            assert!(input.recv().is_err(), "input ended");
        }
        // Nothing is left to fail twice.
        mux.reset("again");
        assert!(mux.inner.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn duplicate_channel_id_is_an_error() {
        let (mux, mut cap) = capture_mux();
        let first = mux.for_channel(7).register(7, None, None);
        assert!(first.is_some());
        assert!(mux.for_channel(7).register(7, None, None).is_none());
        match cap.ctrl() {
            AgentMsg::Error { id: Some(7), .. } => {}
            other => panic!("expected error for id 7, got {other:?}"),
        }
    }

    #[test]
    fn channel_zero_cannot_be_a_session() {
        let (mux, _cap) = capture_mux();
        assert!(mux.for_channel(0).register(0, None, None).is_none());
    }

    #[test]
    fn credit_gates_output_and_close_wakes() {
        let credit = Arc::new(Credit::new());
        // Drain the initial window.
        assert_eq!(credit.take(usize::MAX), MAX_PAYLOAD);
        let mut left = INITIAL_WINDOW as usize - MAX_PAYLOAD;
        while left > 0 {
            left -= credit.take(left);
        }
        // Now a take blocks until grant (exercised from another thread)...
        let c2 = credit.clone();
        let t = thread::spawn(move || c2.take(100));
        thread::sleep(std::time::Duration::from_millis(50));
        credit.grant(40);
        assert_eq!(t.join().unwrap(), 40);
        // ...and close wakes blocked takers with 0.
        let c2 = credit.clone();
        let t = thread::spawn(move || c2.take(100));
        thread::sleep(std::time::Duration::from_millis(50));
        credit.close();
        assert_eq!(t.join().unwrap(), 0);
    }

    #[test]
    fn remove_kills_and_closes() {
        let (mux, _cap) = capture_mux();
        let killed = Arc::new(Mutex::new(false));
        let k = killed.clone();
        let (input, credit) = mux
            .for_channel(3)
            .register(3, None, Some(Box::new(move || *k.lock().unwrap() = true)))
            .unwrap();
        mux.remove(3);
        assert!(*killed.lock().unwrap());
        assert_eq!(credit.take(10), 0, "credit must be closed");
        // The input sender is gone: recv sees end-of-stream.
        assert!(input.recv().is_err());
        // Idempotent.
        mux.remove(3);
    }

    #[test]
    fn close_discards_queued_frames_and_old_workers_cannot_retire_a_reused_id() {
        struct BlockedPort {
            frames: std::sync::mpsc::Sender<Vec<u8>>,
            entered: std::sync::mpsc::SyncSender<()>,
            release: std::sync::mpsc::Receiver<()>,
            first: bool,
        }
        impl Write for BlockedPort {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if std::mem::take(&mut self.first) {
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                self.frames.send(bytes.to_vec()).unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let mux = Mux::new(BlockedPort {
            frames: tx,
            entered: entered_tx,
            release: release_rx,
            first: true,
        });
        let barrier = AgentMsg::Pong { instance: None };
        mux.send_ctrl(&barrier);
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();

        let old = mux.for_channel(7);
        let (_input, credit) = old.register(7, None, None).unwrap();
        assert_eq!(credit.take(10), 10);
        old.send_data(FrameKind::Data, 7, b"queued stdout");
        old.send_data(FrameKind::DataErr, 7, b"queued stderr");
        old.send_ctrl(&AgentMsg::WindowAdjust { id: 7, bytes: 10 });
        old.send_error(Some(7), "queued error");
        old.send_ctrl(&AgentMsg::Eof { id: 7 });
        old.send_ctrl(&AgentMsg::Exited { id: 7, code: 9 });
        mux.remove(7);

        let current = mux.for_channel(7);
        let (_input, _credit) = current.register(7, None, None).unwrap();
        old.send_ctrl(&AgentMsg::Exited { id: 7, code: 9 });
        old.remove_finished(7);
        assert!(mux.inner.sessions.lock().unwrap().contains_key(&7));
        let opened = AgentMsg::Opened { id: 7 };
        let eof = AgentMsg::Eof { id: 7 };
        let exited = AgentMsg::Exited { id: 7, code: 0 };
        current.send_ctrl(&opened);
        current.send_ctrl(&eof);
        current.send_ctrl(&exited);
        current.remove_finished(7);
        mux.send_ctrl(&barrier);
        release_tx.send(()).unwrap();

        for msg in [&barrier, &opened, &eof, &exited, &barrier] {
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap(),
                encode_ctrl(msg).unwrap()
            );
        }
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err()
        );
    }

    #[test]
    fn route_input_reaches_session_and_ignores_strays() {
        let (mux, _cap) = capture_mux();
        let (input, _credit) = mux.for_channel(9).register(9, None, None).unwrap();
        mux.route_input(9, Input::Bytes(b"abc".to_vec()));
        mux.route_input(9, Input::Eof);
        mux.route_input(1234, Input::Bytes(b"stray".to_vec())); // no panic
        match input.recv().unwrap() {
            Input::Bytes(b) => assert_eq!(b, b"abc"),
            Input::Eof => panic!("bytes first"),
        }
        assert!(matches!(input.recv().unwrap(), Input::Eof));
    }

    /// Review F6 (card #507). The host's window counts bytes, so a session
    /// whose pump has stopped reading can still be sent a window's worth of
    /// one-byte frames. Routing them must not hold up the reader thread: the
    /// ping behind them is answered.
    #[test]
    fn a_session_that_stops_reading_does_not_block_other_channels() {
        let (mux, mut cap) = capture_mux();
        // Held and never read: a terminal whose program ignores its input.
        let (_input, _credit) = mux.for_channel(9).register(9, None, None).unwrap();
        let (done, routed) = std::sync::mpsc::channel();
        let reader = mux.clone();
        thread::spawn(move || {
            for _ in 0..600 {
                reader.route_input(9, Input::Bytes(vec![b'x']));
            }
            reader.handle_msg(HostMsg::Ping, &TestPlatform::new());
            let _ = done.send(());
        });
        routed
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("the reader got past a session that does not read its input");
        assert!(matches!(cap.ctrl(), AgentMsg::Pong { .. }));
    }

    /// A host that ignores its window is not waited for either: the channel
    /// is closed and the host is told, and the reader goes on.
    #[test]
    fn a_host_that_overruns_its_window_loses_the_channel() {
        let (mux, mut cap) = capture_mux();
        let killed = Arc::new(AtomicBool::new(false));
        let k = killed.clone();
        let (input, _credit) = mux
            .for_channel(9)
            .register(9, None, Some(Box::new(move || k.store(true, Ordering::SeqCst))))
            .unwrap();
        // Exactly the window, in small frames: all of it is held.
        let frame = vec![b'x'; 1024];
        for _ in 0..INITIAL_WINDOW as usize / frame.len() {
            mux.route_input(9, Input::Bytes(frame.clone()));
        }
        assert!(!killed.load(Ordering::SeqCst), "a full window is in bounds");
        mux.route_input(9, Input::Bytes(vec![b'x']));
        assert!(killed.load(Ordering::SeqCst));
        match cap.ctrl() {
            AgentMsg::Error { id: Some(9), msg } => assert!(msg.contains("window"), "{msg}"),
            other => panic!("expected an error for channel 9, got {other:?}"),
        }
        // What was in bounds is still delivered, then the stream ends.
        let mut got = 0;
        for chunk in input {
            match chunk {
                Input::Bytes(b) => {
                    assert!(b.len() <= MAX_PAYLOAD);
                    got += b.len();
                }
                Input::Eof => panic!("no EOF was sent"),
            }
        }
        assert_eq!(got, INITIAL_WINDOW as usize);
    }

    /// Joining small frames changes neither the bytes nor where an EOF
    /// falls among them, and taking bytes off the queue makes room again.
    #[test]
    fn the_input_queue_keeps_order_and_frees_what_is_read() {
        let (mux, _cap) = capture_mux();
        let (input, _credit) = mux.for_channel(9).register(9, None, None).unwrap();
        for b in b"abc" {
            mux.route_input(9, Input::Bytes(vec![*b]));
        }
        mux.route_input(9, Input::Bytes(Vec::new()));
        mux.route_input(9, Input::Eof);
        mux.route_input(9, Input::Eof);
        mux.route_input(9, Input::Bytes(b"d".to_vec()));
        assert!(matches!(input.recv().unwrap(), Input::Bytes(b) if b == b"abc"));
        assert!(matches!(input.recv().unwrap(), Input::Eof));
        assert!(matches!(input.recv().unwrap(), Input::Bytes(b) if b == b"d"));
        // Two windows through a pump that reads: never an overrun.
        let frame = vec![0u8; MAX_PAYLOAD];
        for _ in 0..2 * INITIAL_WINDOW as usize / MAX_PAYLOAD {
            mux.route_input(9, Input::Bytes(frame.clone()));
            assert!(matches!(input.recv().unwrap(), Input::Bytes(b) if b.len() == MAX_PAYLOAD));
        }
        mux.remove(9);
        assert!(input.recv().is_err());
    }

    #[test]
    fn oversized_control_replies_fail_without_stopping_the_writer() {
        let (mux, mut cap) = capture_mux();
        mux.send_ctrl(&AgentMsg::Clipboard {
            text: "x".repeat(300_000),
        });
        assert!(matches!(cap.ctrl(), AgentMsg::ClipboardFailed { msg }
            if msg.contains("65536")));
        mux.send_ctrl(&AgentMsg::OsInfo {
            info: OsInfo {
                id: "linux".into(),
                name: "x".repeat(300_000),
                version: String::new(),
                kernel: String::new(),
                arch: String::new(),
                hostname: String::new(),
            },
        });
        assert!(matches!(cap.ctrl(), AgentMsg::Error { id: None, msg }
            if msg.contains("65536")));
        mux.send_ctrl(&AgentMsg::Metrics {
            cpu_pct: 0.0,
            mem_used: 0,
            mem_total: 0,
            disks: vec![vmlab_agent_proto::DiskUsage {
                mount: "x".repeat(300_000),
                used: 0,
                total: 0,
            }],
        });
        assert!(matches!(cap.ctrl(), AgentMsg::Error { id: None, msg }
            if msg.contains("65536")));
        mux.send_error(Some(7), "x".repeat(300_000));
        assert!(matches!(cap.ctrl(), AgentMsg::Error { id: Some(7), msg }
            if msg.contains("65536")));
        mux.send_ctrl(&AgentMsg::Pong { instance: None });
        assert_eq!(cap.ctrl(), AgentMsg::Pong { instance: None });
    }

    #[test]
    fn pump_out_respects_credit_and_frames_data() {
        let (mux, mut cap) = capture_mux();
        let mux = mux.for_channel(5);
        let (_input, credit) = mux.register(5, None, None).unwrap();
        let data = vec![7u8; 100_000]; // > one frame, < initial window
        pump_out(&mux, 5, FrameKind::Data, &credit, &data[..]);
        let mut got = Vec::new();
        while got.len() < data.len() {
            let f = cap.frame();
            assert_eq!(f.kind, FrameKind::Data);
            assert_eq!(f.channel, 5);
            assert!(f.payload.len() <= MAX_PAYLOAD);
            got.extend(f.payload);
        }
        assert_eq!(got, data);
    }
}
