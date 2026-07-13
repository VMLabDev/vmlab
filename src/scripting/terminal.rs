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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use wscript::Script;

use crate::labd::vm_agent::{AgentSession, SessionEvent};

/// Default terminal size for script sessions — wide enough that prompts and
/// command echoes rarely wrap mid-pattern.
pub const SCRIPT_COLS: u16 = 120;
pub const SCRIPT_ROWS: u16 = 32;

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
        self.rt
            .block_on(session.send(text.as_bytes()))
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
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs.max(0) as u64);
        let mut st = self.state.lock().expect("term state");
        loop {
            let text = String::from_utf8_lossy(&st.buf).into_owned();
            if let Some(found) = re.find(&text) {
                let consumed: String = text[..found.end()].to_string();
                st.buf = text.as_bytes()[found.end()..].to_vec();
                return Ok(consumed);
            }
            if let Some(why) = &st.ended {
                return Err(format!(
                    "{}: terminal ended ({why}) before /{pattern}/ matched; tail: {:?}",
                    self.machine,
                    tail(&text)
                ));
            }
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(format!(
                    "{}: timed out after {timeout_secs}s waiting for /{pattern}/; tail: {:?}",
                    self.machine,
                    tail(&text)
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
        self.rt
            .block_on(session.resize(
                cols.clamp(2, u16::MAX as i64) as u16,
                rows.clamp(2, u16::MAX as i64) as u16,
            ))
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
            let ev = self
                .rt
                .block_on(async { tokio::time::timeout(remaining, session.recv()).await });
            match ev {
                Err(_) => return, // quiet: nothing new within the window
                Ok(None) => {
                    st.ended
                        .get_or_insert_with(|| "agent channel closed".into());
                    st.session = None;
                    return;
                }
                Ok(Some(SessionEvent::Data(b))) | Ok(Some(SessionEvent::Stderr(b))) => {
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
                Ok(Some(SessionEvent::FileDone { .. })) => {}
            }
        }
    }
}

/// The last few hundred bytes of output, for timeout diagnostics.
fn tail(text: &str) -> String {
    const TAIL: usize = 300;
    if text.len() <= TAIL {
        text.to_string()
    } else {
        let mut start = text.len() - TAIL;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        format!("…{}", &text[start..])
    }
}
