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

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use vmlab_agent_proto::{
    AgentMsg, Frame, FrameKind, HostMsg, INITIAL_WINDOW, MAX_PAYLOAD, NetInterface, OsInfo,
    PROTO_VERSION, ShutdownMode, encode_ctrl, encode_frame,
};

/// Host→guest bytes for one session, fed by the reader dispatch and drained
/// by the session's input pump.
pub enum Input {
    Bytes(Vec<u8>),
    /// `HostMsg::Eof` — no more bytes (exec stdin closed; file push done).
    Eof,
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
    input: Option<SyncSender<Input>>,
    credit: Arc<Credit>,
    /// Applies a terminal resize (PTY sessions only).
    resize: Option<Box<dyn Fn(u16, u16) + Send>>,
    /// Force-stops the session's work (kill the process, stop the tail).
    /// Pump threads notice via closed channels/credit and exit.
    kill: Option<Box<dyn FnOnce() + Send>>,
}

/// Everything sessions need to talk back to the host. Cheap to clone.
#[derive(Clone)]
pub struct Mux {
    inner: Arc<MuxInner>,
}

struct MuxInner {
    /// Encoded frames for the port writer thread. Bounded: senders block
    /// when the host is slow/absent, which is the flow-control backstop for
    /// control traffic (data traffic is credit-limited before it gets here).
    out: SyncSender<Vec<u8>>,
    sessions: Mutex<HashMap<u32, Session>>,
}

/// How many encoded frames may queue for the port writer before senders
/// block. Sized to absorb bursts, not to buffer a detached host forever.
const OUT_QUEUE: usize = 256;

/// Per-session input queue depth. The host may have at most
/// `INITIAL_WINDOW` un-granted bytes in flight, so the queue never fills
/// while both sides respect the window; the bound is a safety net.
const INPUT_QUEUE: usize = 2 * (INITIAL_WINDOW as usize / 1024);

impl Mux {
    /// Start the writer thread over the port's write half and return the mux.
    pub fn new(mut port_w: impl Write + Send + 'static) -> Mux {
        let (out, rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = sync_channel(OUT_QUEUE);
        thread::spawn(move || {
            for frame in rx {
                // Blocks until the host side is connected — that is the
                // point of doing it on this dedicated thread.
                if port_w.write_all(&frame).is_err() {
                    // Port write errors are not recoverable from here; keep
                    // draining so senders don't wedge (the reader loop owns
                    // reconnect/exit policy).
                    continue;
                }
                let _ = port_w.flush();
            }
        });
        Mux {
            inner: Arc::new(MuxInner {
                out,
                sessions: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Send a control message to the host.
    pub fn send_ctrl(&self, msg: &AgentMsg) {
        let _ = self.inner.out.send(encode_ctrl(msg));
    }

    /// Send channel payload bytes. The caller must have taken credit first.
    pub fn send_data(&self, kind: FrameKind, channel: u32, payload: &[u8]) {
        debug_assert!(payload.len() <= MAX_PAYLOAD);
        let _ = self.inner.out.send(encode_frame(kind, channel, payload));
    }

    pub fn send_error(&self, id: Option<u32>, msg: impl Into<String>) {
        self.send_ctrl(&AgentMsg::Error {
            id,
            msg: msg.into(),
        });
    }

    /// Register a new session. Returns the input receiver for the session's
    /// pump and its send credit, or `None` (+ an error to the host) if the
    /// id is already live.
    pub fn register(
        &self,
        id: u32,
        resize: Option<Box<dyn Fn(u16, u16) + Send>>,
        kill: Option<Box<dyn FnOnce() + Send>>,
    ) -> Option<(Receiver<Input>, Arc<Credit>)> {
        let mut sessions = self.inner.sessions.lock().unwrap();
        if sessions.contains_key(&id) || id == 0 {
            drop(sessions);
            self.send_error(Some(id), "channel id already in use");
            return None;
        }
        let (tx, rx) = sync_channel(INPUT_QUEUE);
        let credit = Arc::new(Credit::new());
        sessions.insert(
            id,
            Session {
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
            Some(s) => s.kill = Some(kill),
            None => {
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
        let session = self.inner.sessions.lock().unwrap().remove(&id);
        if let Some(mut s) = session {
            s.credit.close();
            s.input = None;
            s.kill = None;
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
    pub fn route_input(&self, id: u32, input: Input) {
        let tx = {
            let sessions = self.inner.sessions.lock().unwrap();
            sessions.get(&id).and_then(|s| s.input.clone())
        };
        if let Some(tx) = tx {
            // Blocking send: the input queue bounds a host that ignores its
            // send window.
            let _ = tx.send(input);
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
                // had open is gone.
                self.remove_all();
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
                    agent_version: env!("CARGO_PKG_VERSION").to_string(),
                    os: platform.os().to_string(),
                    features: platform.features(),
                    token,
                });
            }
            HostMsg::OpenTerminal {
                id,
                cols,
                rows,
                command,
            } => platform.open_terminal(self, id, cols, rows, command),
            HostMsg::Resize { id, cols, rows } => self.resize(id, cols, rows),
            HostMsg::OpenExec { id, argv, env, cwd } => {
                platform.open_exec(self, id, argv, env, cwd)
            }
            HostMsg::Eof { id } => self.route_input(id, Input::Eof),
            HostMsg::OpenFilePush { id, path, mode } => {
                crate::files::open_push(self, id, platform.resolve_path(path), mode)
            }
            HostMsg::OpenFilePull { id, path } => {
                crate::files::open_pull(self, id, platform.resolve_path(path))
            }
            HostMsg::OpenTail { id, path } => {
                crate::tail::open(self, id, platform.resolve_path(path))
            }
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
            HostMsg::Shutdown { mode } => {
                // Ack before executing: the reply may be the last bytes this
                // guest ever puts on the wire.
                self.send_ctrl(&AgentMsg::ShuttingDown { mode });
                platform.shutdown(self, mode);
            }
            HostMsg::WindowAdjust { id, bytes } => self.grant(id, bytes),
            HostMsg::Close { id } => self.remove(id),
            HostMsg::Ping => self.send_ctrl(&AgentMsg::Pong),
        }
    }
}

/// The OS-specific half the mux dispatches into.
pub trait Platform: Sync {
    fn os(&self) -> &'static str;
    fn features(&self) -> Vec<String>;
    /// Spawn a shell on a PTY/ConPTY bridged to channel `id`; must register
    /// the session, emit `Opened`, and emit `Exited` + remove when the shell
    /// ends.
    fn open_terminal(&self, mux: &Mux, id: u32, cols: u16, rows: u16, command: Option<Vec<String>>);
    /// Streaming exec. The default spawns on the platform directly;
    /// container mode reroutes through the namespace/chroot trampoline.
    fn open_exec(
        &self,
        mux: &Mux,
        id: u32,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
    ) {
        crate::exec::open(mux, id, argv, env, cwd);
    }
    /// Map a host-supplied guest path (file transfer, tail) — container mode
    /// resolves it inside the container rootfs.
    fn resolve_path(&self, path: String) -> String {
        path
    }
    fn open_eventlog(&self, mux: &Mux, id: u32, filter: Option<String>);
    fn set_clipboard(&self, mux: &Mux, text: String);
    fn get_clipboard(&self, mux: &Mux);
    /// The guest's network interfaces (loopback excluded).
    fn net_info(&self) -> Result<Vec<NetInterface>, String>;
    /// Structured OS information.
    fn os_info(&self) -> Result<OsInfo, String>;
    /// Bring the guest down. The `ShuttingDown` ack is already queued;
    /// implementations delay briefly so it flushes, and report failure via
    /// `mux` if the OS refuses.
    fn shutdown(&self, mux: &Mux, mode: ShutdownMode);
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
    use std::sync::mpsc::channel;

    struct NullPlatform;
    impl Platform for NullPlatform {
        fn os(&self) -> &'static str {
            "test"
        }
        fn features(&self) -> Vec<String> {
            vec!["terminal".into()]
        }
        fn open_terminal(&self, _: &Mux, _: u32, _: u16, _: u16, _: Option<Vec<String>>) {}
        fn open_eventlog(&self, mux: &Mux, id: u32, _: Option<String>) {
            mux.send_error(Some(id), "unsupported");
        }
        fn set_clipboard(&self, _: &Mux, _: String) {}
        fn get_clipboard(&self, mux: &Mux) {
            mux.send_error(None, "unsupported");
        }
        fn net_info(&self) -> Result<Vec<NetInterface>, String> {
            Ok(vec![NetInterface {
                name: "eth0".into(),
                mac: Some("52:54:00:00:00:01".into()),
                ipv4: vec!["10.0.0.2".into()],
                ipv6: vec![],
            }])
        }
        fn os_info(&self) -> Result<OsInfo, String> {
            Ok(OsInfo {
                id: "test".into(),
                name: "Test OS".into(),
                version: "1".into(),
                kernel: "0.0".into(),
                arch: "x86_64".into(),
                hostname: "testhost".into(),
            })
        }
        fn shutdown(&self, _: &Mux, _: ShutdownMode) {}
    }

    /// A Write that forwards every produced frame to a channel for
    /// inspection.
    struct CapturePort(std::sync::mpsc::Sender<Vec<u8>>);
    impl Write for CapturePort {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.send(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_mux() -> (Mux, std::sync::mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = channel();
        (Mux::new(CapturePort(tx)), rx)
    }

    fn next_ctrl(rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> AgentMsg {
        let mut dec = vmlab_agent_proto::FrameDecoder::new();
        loop {
            let bytes = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            dec.push(&bytes);
            if let Some(f) = dec.next_frame() {
                assert_eq!(f.kind, FrameKind::Ctrl);
                return serde_json::from_slice(&f.payload).unwrap();
            }
        }
    }

    #[test]
    fn hello_resets_and_echoes_token() {
        let (mux, rx) = capture_mux();
        mux.handle_msg(
            HostMsg::Hello {
                proto_version: PROTO_VERSION,
                token: "tok-1".into(),
            },
            &NullPlatform,
        );
        match next_ctrl(&rx) {
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
                assert_eq!(features, vec!["terminal".to_string()]);
            }
            other => panic!("expected hello, got {other:?}"),
        }
    }

    #[test]
    fn ping_pongs() {
        let (mux, rx) = capture_mux();
        mux.handle_msg(HostMsg::Ping, &NullPlatform);
        assert_eq!(next_ctrl(&rx), AgentMsg::Pong);
    }

    #[test]
    fn net_info_replies_with_interfaces() {
        let (mux, rx) = capture_mux();
        mux.handle_msg(HostMsg::NetInfo, &NullPlatform);
        match next_ctrl(&rx) {
            AgentMsg::NetInfo { interfaces } => {
                assert_eq!(interfaces.len(), 1);
                assert_eq!(interfaces[0].name, "eth0");
            }
            other => panic!("expected net_info, got {other:?}"),
        }
    }

    #[test]
    fn os_info_replies_with_info() {
        let (mux, rx) = capture_mux();
        mux.handle_msg(HostMsg::OsInfo, &NullPlatform);
        match next_ctrl(&rx) {
            AgentMsg::OsInfo { info } => assert_eq!(info.id, "test"),
            other => panic!("expected os_info, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_acks_before_executing() {
        let (mux, rx) = capture_mux();
        mux.handle_msg(
            HostMsg::Shutdown {
                mode: ShutdownMode::Reboot,
            },
            &NullPlatform,
        );
        assert_eq!(
            next_ctrl(&rx),
            AgentMsg::ShuttingDown {
                mode: ShutdownMode::Reboot
            }
        );
    }

    #[test]
    fn duplicate_channel_id_is_an_error() {
        let (mux, rx) = capture_mux();
        let first = mux.register(7, None, None);
        assert!(first.is_some());
        assert!(mux.register(7, None, None).is_none());
        match next_ctrl(&rx) {
            AgentMsg::Error { id: Some(7), .. } => {}
            other => panic!("expected error for id 7, got {other:?}"),
        }
    }

    #[test]
    fn channel_zero_cannot_be_a_session() {
        let (mux, _rx) = capture_mux();
        assert!(mux.register(0, None, None).is_none());
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
        let (mux, _rx) = capture_mux();
        let killed = Arc::new(Mutex::new(false));
        let k = killed.clone();
        let (input, credit) = mux
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
    fn route_input_reaches_session_and_ignores_strays() {
        let (mux, _rx) = capture_mux();
        let (input, _credit) = mux.register(9, None, None).unwrap();
        mux.route_input(9, Input::Bytes(b"abc".to_vec()));
        mux.route_input(9, Input::Eof);
        mux.route_input(1234, Input::Bytes(b"stray".to_vec())); // no panic
        match input.recv().unwrap() {
            Input::Bytes(b) => assert_eq!(b, b"abc"),
            Input::Eof => panic!("bytes first"),
        }
        assert!(matches!(input.recv().unwrap(), Input::Eof));
    }

    #[test]
    fn pump_out_respects_credit_and_frames_data() {
        let (mux, rx) = capture_mux();
        let (_input, credit) = mux.register(5, None, None).unwrap();
        let data = vec![7u8; 100_000]; // > one frame, < initial window
        pump_out(&mux, 5, FrameKind::Data, &credit, &data[..]);
        let mut dec = vmlab_agent_proto::FrameDecoder::new();
        let mut got = Vec::new();
        while got.len() < data.len() {
            let bytes = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            dec.push(&bytes);
            while let Some(f) = dec.next_frame() {
                assert_eq!(f.kind, FrameKind::Data);
                assert_eq!(f.channel, 5);
                assert!(f.payload.len() <= MAX_PAYLOAD);
                got.extend(f.payload);
            }
        }
        assert_eq!(got, data);
    }
}
