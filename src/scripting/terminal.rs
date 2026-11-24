//! Interactive terminal sessions for scripts: `vm.terminal()` /
//! `container.terminal()` return a [`TerminalHandle`] wrapping one
//! vmlab-agent shell session (in-process — no unix-socket hop), driven
//! send/expect style:
//!
//! ```wscript
//! let t = vm.terminal()?
//! t.send_line("hostname")
//! let out = t.expect("myhost", 10)?
//! t.close()
//! ```
//!
//! Output accumulates in a buffer; `expect` consumes through the end of the
//! regex match and returns the consumed text, so successive expects walk the
//! stream. The shell sees a real PTY: prompts, echoes and ANSI sequences are
//! all in the buffer (match accordingly).
//!
//! The buffer holds [`TERM_BUFFER_LIMIT`] at most. A guest that writes more
//! than that with no `expect` matching and no `read` taking it ends the
//! session, and the `expect` that waited fails naming the limit.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use wscript::Script;

use crate::labd::vm_agent::{AgentSession, SessionEvent};

/// Default terminal size for script sessions — wide enough that prompts and
/// command echoes rarely wrap mid-pattern.
pub const SCRIPT_COLS: u16 = 120;
pub const SCRIPT_ROWS: u16 = 32;

/// The most output a terminal holds that no `expect` has matched and no
/// `read` has taken. Past it the session is ended (PRD §2: a guest-to-host
/// channel bounds what it uses): a guest that writes without end must not
/// grow the lab daemon for the whole of an `expect` timeout. It is also the
/// most one `expect` scan covers.
pub(crate) const TERM_BUFFER_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Script)]
#[script(name = "Term")]
#[script(opaque)]
pub struct TerminalHandle {
    pub(crate) machine: String,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) state: Arc<Mutex<TermState>>,
}

pub(crate) struct TermState {
    pub session: Option<AgentSession>,
    pub buf: Vec<u8>,
    /// Why the session ended (shell exit, agent error), once it has.
    pub ended: Option<String>,
}

impl TerminalHandle {
    pub(crate) fn new(machine: String, rt: tokio::runtime::Handle, session: AgentSession) -> Self {
        TerminalHandle {
            machine,
            rt,
            state: Arc::new(Mutex::new(TermState {
                session: Some(session),
                buf: Vec::new(),
                ended: None,
            })),
        }
    }

    /// Send raw bytes to the shell.
    pub fn send(&self, text: &str) -> Result<(), String> {
        let st = self.state.lock().expect("term state");
        let Some(session) = st.session.as_ref() else {
            return Err(format!("{}: terminal session is closed", self.machine));
        };
        super::cancel::block_on(&self.rt, session.send(text.as_bytes()))
            .map_err(|e| format!("{e:#}"))
    }

    /// Send a line: `text` + carriage return (what a PTY expects from Enter;
    /// works for both POSIX shells and PowerShell).
    pub fn send_line(&self, text: &str) -> Result<(), String> {
        self.send(&format!("{text}\r"))
    }

    /// Pull whatever the shell has produced so far (non-blocking-ish: drains
    /// data already queued, without waiting for more).
    pub fn read(&self) -> String {
        let mut st = self.state.lock().expect("term state");
        self.pump(&mut st, Duration::from_millis(50));
        let text = String::from_utf8_lossy(&st.buf).into_owned();
        st.buf.clear();
        text
    }

    /// Wait until the accumulated output matches `pattern` (a regex);
    /// consume and return the text through the end of the match. On timeout
    /// the error carries the unmatched buffer tail for debugging.
    pub fn expect(&self, pattern: &str, timeout_secs: i64) -> Result<String, String> {
        let re = regex::Regex::new(pattern).map_err(|e| format!("bad pattern: {e}"))?;
        let deadline = std::time::Instant::now() + super::script_timeout(timeout_secs, 0)?;
        let mut st = self.state.lock().expect("term state");
        // How much of the buffer the last scan covered with no match: the
        // same bytes are not scanned a second time.
        let mut scanned = None;
        loop {
            if scanned != Some(st.buf.len()) {
                // Borrowed, not copied, while the output is valid UTF-8.
                let text = String::from_utf8_lossy(&st.buf);
                if let Some(found) = re.find(&text) {
                    let consumed = text[..found.end()].to_string();
                    let rest = text.as_bytes()[found.end()..].to_vec();
                    drop(text);
                    st.buf = rest;
                    return Ok(consumed);
                }
                scanned = Some(st.buf.len());
            }
            if let Some(why) = &st.ended {
                return Err(format!(
                    "{}: terminal ended ({why}) before /{pattern}/ matched; tail: {:?}",
                    self.machine,
                    tail(&st.buf)
                ));
            }
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(format!(
                    "{}: timed out after {timeout_secs}s waiting for /{pattern}/; tail: {:?}",
                    self.machine,
                    tail(&st.buf)
                ));
            };
            self.pump(&mut st, remaining.min(Duration::from_millis(500)));
        }
    }

    /// Resize the session's PTY.
    pub fn resize(&self, cols: i64, rows: i64) -> Result<(), String> {
        let st = self.state.lock().expect("term state");
        let Some(session) = st.session.as_ref() else {
            return Err(format!("{}: terminal session is closed", self.machine));
        };
        super::cancel::block_on(
            &self.rt,
            session.resize(
                cols.clamp(2, u16::MAX as i64) as u16,
                rows.clamp(2, u16::MAX as i64) as u16,
            ),
        )
        .map_err(|e| format!("{e:#}"))
    }

    /// End the session (kills the shell). Also implied when the handle is
    /// garbage-collected, but explicit close is deterministic.
    pub fn close(&self) {
        let mut st = self.state.lock().expect("term state");
        if let Some(session) = st.session.take() {
            self.rt.block_on(session.close());
        }
        st.ended.get_or_insert_with(|| "closed".to_string());
    }

    /// Drain pending session events for up to `wait`, appending data to the
    /// buffer. Returns early on the first quiet gap.
    fn pump(&self, st: &mut TermState, wait: Duration) {
        let Some(session) = st.session.as_mut() else {
            return;
        };
        let mut remaining = wait;
        loop {
            let started = std::time::Instant::now();
            let ev = super::cancel::block_on(&self.rt, async {
                tokio::time::timeout(remaining, session.recv()).await
            });
            match ev {
                Err(_) => return, // quiet: nothing new within the window
                Ok(None) => {
                    st.ended
                        .get_or_insert_with(|| "agent channel closed".into());
                    st.session = None;
                    return;
                }
                Ok(Some(SessionEvent::Data(b))) | Ok(Some(SessionEvent::Stderr(b))) => {
                    if st.buf.len() + b.len() > TERM_BUFFER_LIMIT {
                        // The guest writes and the script takes nothing:
                        // end the session rather than hold more. Closing
                        // the channel tells the guest.
                        st.ended = Some(format!(
                            "the guest wrote past the terminal buffer's limit of {} MiB with \
                             no match and no read",
                            TERM_BUFFER_LIMIT / (1024 * 1024)
                        ));
                        if let Some(session) = st.session.take() {
                            self.rt.block_on(session.close());
                        }
                        return;
                    }
                    st.buf.extend(b);
                    // Keep draining, but only within the original window.
                    remaining = remaining.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return;
                    }
                }
                Ok(Some(SessionEvent::Exited(code))) => {
                    st.ended = Some(format!("shell exited with code {code}"));
                    st.session = None;
                    return;
                }
                Ok(Some(SessionEvent::Error(msg))) => {
                    st.ended = Some(msg);
                    st.session = None;
                    return;
                }
                // A terminal ends by exiting, not by an output EOF.
                Ok(Some(SessionEvent::Eof)) => {}
            }
        }
    }
}

/// The last few hundred bytes of output, for timeout diagnostics.
fn tail(buf: &[u8]) -> String {
    const TAIL: usize = 300;
    if buf.len() <= TAIL {
        return String::from_utf8_lossy(buf).into_owned();
    }
    // Start on a character, not inside one.
    let mut start = buf.len() - TAIL;
    while start < buf.len() && buf[start] & 0xC0 == 0x80 {
        start += 1;
    }
    format!("…{}", String::from_utf8_lossy(&buf[start..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::vm_agent::AgentHandle;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use vmlab_agent_proto::{
        AgentMsg, Frame, FrameDecoder, FrameKind, HostMsg, INITIAL_WINDOW, MAX_PAYLOAD,
        PROTO_VERSION, encode_ctrl, encode_frame, features,
    };

    #[tokio::test]
    async fn card_861_expect_keeps_a_character_split_across_reads() {
        let term = TerminalHandle {
            machine: "vm".into(),
            rt: tokio::runtime::Handle::current(),
            state: Arc::new(Mutex::new(TermState {
                session: None,
                buf: b"\xffok\xe2\x82".to_vec(),
                ended: Some("closed".into()),
            })),
        };
        assert_eq!(term.expect("ok", 0).unwrap(), "�ok");
        term.state.lock().unwrap().buf.push(0xac);
        assert_eq!(term.read(), "€");
    }

    #[tokio::test]
    async fn a_huge_timeout_is_an_error_not_a_panic() {
        let term = TerminalHandle {
            machine: "vm".into(),
            rt: tokio::runtime::Handle::current(),
            state: Arc::new(Mutex::new(TermState {
                session: None,
                buf: Vec::new(),
                ended: Some("closed".into()),
            })),
        };
        let error = term.expect("x", i64::MAX).unwrap_err();
        assert!(error.contains("timeout_secs"), "{error}");
    }

    /// How the flooding agent's connection ended.
    #[derive(Debug, PartialEq)]
    enum FloodEnd {
        /// The host closed the terminal's channel.
        ClosedByHost,
        /// Every byte of the bound was sent and taken.
        Exhausted,
        /// The host went away.
        Disconnected,
    }

    /// An agent double whose terminals never stop writing: `open_terminal`
    /// is answered, then full `Data` frames go out as long as the host's
    /// window covers one (the flood peer of the `vm_agent.rs` tests). It
    /// sends `bound` bytes at most, so it ends by itself against a host that
    /// takes everything.
    async fn flooding_agent(listener: tokio::net::UnixListener, bound: u64) -> FloodEnd {
        let (stream, _) = listener.accept().await.expect("accept");
        let (mut rx, mut tx) = stream.into_split();
        // channel -> (send credit, bytes still to send)
        let mut floods: HashMap<u32, (u64, u64)> = HashMap::new();
        let mut dec = FrameDecoder::new();
        let mut buf = [0u8; 8192];
        loop {
            for (id, (credit, left)) in floods.iter_mut() {
                let frame = encode_frame(FrameKind::Data, *id, &[b'x'; MAX_PAYLOAD]);
                while *credit >= MAX_PAYLOAD as u64 && *left >= MAX_PAYLOAD as u64 {
                    if tx.write_all(&frame).await.is_err() {
                        return FloodEnd::Disconnected;
                    }
                    *credit -= MAX_PAYLOAD as u64;
                    *left -= MAX_PAYLOAD as u64;
                }
            }
            if !floods.is_empty() && floods.values().all(|(_, left)| *left < MAX_PAYLOAD as u64) {
                return FloodEnd::Exhausted;
            }
            let n = match rx.read(&mut buf).await {
                Ok(0) | Err(_) => return FloodEnd::Disconnected,
                Ok(n) => n,
            };
            dec.push(&buf[..n]);
            while let Some(Frame { kind, payload, .. }) = dec.next_frame() {
                if kind != FrameKind::Ctrl {
                    continue;
                }
                let reply = match serde_json::from_slice::<HostMsg>(&payload).unwrap() {
                    HostMsg::Hello { token, .. } => Some(AgentMsg::Hello {
                        proto_version: PROTO_VERSION,
                        agent_version: "0.1.0-flood".into(),
                        os: "linux".into(),
                        features: vec![features::TERMINAL.into()],
                        token,
                        instance: Some("flood-1".into()),
                    }),
                    HostMsg::OpenTerminal { id, .. } => {
                        floods.insert(id, (INITIAL_WINDOW, bound));
                        Some(AgentMsg::Opened { id })
                    }
                    HostMsg::WindowAdjust { id, bytes } => {
                        if let Some(flood) = floods.get_mut(&id) {
                            flood.0 += bytes;
                        }
                        None
                    }
                    HostMsg::Close { .. } => return FloodEnd::ClosedByHost,
                    _ => None,
                };
                if let Some(msg) = reply
                    && tx.write_all(&encode_ctrl(&msg).unwrap()).await.is_err()
                {
                    return FloodEnd::Disconnected;
                }
            }
        }
    }

    /// A guest whose shell writes without end and never prints the pattern
    /// (`cat /dev/zero`, or a guest turned hostile) must not grow the lab
    /// daemon for the whole of an `expect` timeout: the terminal ends at its
    /// limit and says so (review `docs/reviews/wscript.md`, F6).
    ///
    /// No clock is involved. The flood is twice the limit and then stops:
    /// a terminal with no limit takes all of it and fails here on what its
    /// buffer holds.
    #[test]
    fn a_terminal_that_floods_is_ended_at_a_limit() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.sock");
        let bound = 2 * TERM_BUFFER_LIMIT as u64;
        let agent_end = {
            let _in_rt = rt.enter();
            let listener = tokio::net::UnixListener::bind(&path).expect("bind");
            rt.spawn(flooding_agent(listener, bound))
        };
        let session = rt.block_on(async {
            let agent = AgentHandle::connect(&path, Duration::from_secs(5))
                .await
                .unwrap();
            agent
                .open_terminal(SCRIPT_COLS, SCRIPT_ROWS, None, vec![], None)
                .await
                .unwrap()
        });
        let t = TerminalHandle::new("box".into(), rt.handle().clone(), session);

        let err = t.expect("never", 3600).unwrap_err();

        let held = t.state.lock().unwrap().buf.len();
        assert!(
            held <= TERM_BUFFER_LIMIT,
            "the buffer holds {held} bytes, past the limit of {TERM_BUFFER_LIMIT}"
        );
        assert!(
            err.contains("limit of 16 MiB"),
            "the error names the limit: {err}"
        );
        assert!(
            t.state.lock().unwrap().session.is_none(),
            "the session is ended"
        );
        // The guest is told: its channel is closed, not left to stall.
        assert_eq!(rt.block_on(agent_end).unwrap(), FloodEnd::ClosedByHost);
    }
}
