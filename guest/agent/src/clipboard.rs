//! The clipboard's reply contract, shared by both platforms.
//!
//! An agent advertising [`features::CLIPBOARD_REPLY`] answers every
//! clipboard request — a set with [`AgentMsg::ClipboardSet`], a get with
//! [`AgentMsg::Clipboard`], either with [`AgentMsg::ClipboardFailed`] — so the
//! host can refuse immediately, by reason, instead of reporting a copy that
//! never happened or waiting out a reply that will never come.
//!
//! The Windows service reaches the clipboard through a helper it spawns into
//! the desktop session (`windows/clipboard.rs`); the lines the two exchange
//! over their named pipe live here, beside the replies they turn into, so the
//! mapping is testable on any host.
//!
//! [`features::CLIPBOARD_REPLY`]: vmlab_agent_proto::features::CLIPBOARD_REPLY

use vmlab_agent_proto::AgentMsg;

use crate::mux::Mux;

/// Why a Windows guest has no clipboard to reach: the clipboard belongs to an
/// interactive session, and at the lock screen with nobody logged on there is
/// none. The host prefixes the machine's name.
#[cfg_attr(not(windows), allow(dead_code))]
pub const NO_DESKTOP_LOGON: &str = "no one is logged on to the desktop; the Windows clipboard \
     belongs to an interactive session (it works once a user logs on)";

/// Answer a `set_clipboard`.
pub fn answer_set(mux: &Mux, outcome: Result<(), String>) {
    mux.send_ctrl(&match outcome {
        Ok(()) => AgentMsg::ClipboardSet,
        Err(msg) => AgentMsg::ClipboardFailed { msg },
    });
}

/// Answer a `get_clipboard`.
pub fn answer_get(mux: &Mux, outcome: Result<String, String>) {
    mux.send_ctrl(&match outcome {
        Ok(text) => AgentMsg::Clipboard { text },
        Err(msg) => AgentMsg::ClipboardFailed { msg },
    });
}

/// What `WTSQueryUserToken` failing with `code` means for the clipboard:
/// `ERROR_NO_TOKEN` is the console session with nobody logged on to it — the
/// lock screen of a machine nobody has signed in to — and anything else is
/// reported as itself.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn token_failure(code: i32) -> String {
    const ERROR_NO_TOKEN: i32 = 1008;
    if code == ERROR_NO_TOKEN {
        NO_DESKTOP_LOGON.to_string()
    } else {
        format!(
            "cannot reach the desktop session's user (WTSQueryUserToken: {})",
            std::io::Error::from_raw_os_error(code)
        )
    }
}

/// The JSON lines the Windows service and its desktop-session helper trade
/// over the named pipe, one object per line.
///
/// - service → helper: `{"set": "<text>"}` / `{"get": true}`.
/// - helper → service: `{"clip": "<text>"}` (a get's reply, and every
///   clipboard change the helper sees), `{"set_ok": true}`, and
///   `{"failed": "<why>"}` for a request it could not serve.
#[cfg_attr(not(windows), allow(dead_code))]
pub mod helper {
    use serde_json::{Value, json};
    use vmlab_agent_proto::AgentMsg;

    // FILE_GENERIC_READ and FILE_GENERIC_WRITE without FILE_APPEND_DATA,
    // which aliases FILE_CREATE_PIPE_INSTANCE on named pipes.
    pub const PIPE_READ: u32 = 0x0012_0089;
    pub const PIPE_WRITE: u32 = 0x0012_0112;
    pub const PIPE_SECURITY: &str = "D:P(A;;GA;;;SY)(A;;0x12019b;;;IU)";

    /// A pending launch is reusable too: concurrent requests must wait for
    /// the same process rather than starting competing helpers.
    pub fn reuse_process(console: u32, session: u32, exit: Option<u32>) -> bool {
        console != u32::MAX && console == session && exit.is_none()
    }

    /// Process exit wins over a stale connected flag or a timeout. The
    /// helper preserves its startup Win32 error as its process exit code.
    pub fn startup(connected: bool, exit: Option<u32>, timed_out: bool) -> Result<bool, String> {
        if let Some(code) = exit {
            Err(format!(
                "exited before serving clipboard requests (status {code}, 0x{code:08x}: {})",
                std::io::Error::from_raw_os_error(code as i32)
            ))
        } else if connected {
            Ok(true)
        } else if timed_out {
            Err("is still running but did not connect within 5s".into())
        } else {
            Ok(false)
        }
    }

    pub fn pipe_connected(success: bool, code: i32) -> Result<(), String> {
        const ERROR_PIPE_CONNECTED: i32 = 535;
        if success || code == ERROR_PIPE_CONNECTED {
            Ok(())
        } else {
            Err(format!(
                "clipboard ConnectNamedPipe failed: {}",
                std::io::Error::from_raw_os_error(code)
            ))
        }
    }

    /// A request the service puts to the helper.
    #[derive(Debug, PartialEq, Eq)]
    pub enum Request {
        Set(String),
        Get,
    }

    pub fn set_request(text: &str) -> String {
        json!({ "set": text }).to_string()
    }

    pub fn get_request() -> String {
        json!({ "get": true }).to_string()
    }

    pub fn parse_request(line: &str) -> Option<Request> {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        if let Some(text) = v["set"].as_str() {
            Some(Request::Set(text.to_string()))
        } else if v["get"].as_bool() == Some(true) {
            Some(Request::Get)
        } else {
            None
        }
    }

    pub fn clip(text: &str) -> String {
        json!({ "clip": text }).to_string()
    }

    pub fn set_ok() -> String {
        json!({ "set_ok": true }).to_string()
    }

    pub fn failed(msg: &str) -> String {
        json!({ "failed": msg }).to_string()
    }

    /// What the host is told about one helper line; `None` for a line that
    /// is not one of the helper's.
    pub fn reply_msg(line: &str) -> Option<AgentMsg> {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        if let Some(text) = v["clip"].as_str() {
            Some(AgentMsg::Clipboard {
                text: text.to_string(),
            })
        } else if v["set_ok"].as_bool() == Some(true) {
            Some(AgentMsg::ClipboardSet)
        } else {
            v["failed"].as_str().map(|msg| AgentMsg::ClipboardFailed {
                msg: msg.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::capture_mux;

    #[test]
    fn a_set_is_acknowledged_or_refused_by_reason() {
        let (mux, mut cap) = capture_mux();
        answer_set(&mux, Ok(()));
        assert_eq!(cap.ctrl(), AgentMsg::ClipboardSet);
        answer_set(&mux, Err(NO_DESKTOP_LOGON.into()));
        assert_eq!(
            cap.ctrl(),
            AgentMsg::ClipboardFailed {
                msg: NO_DESKTOP_LOGON.into()
            }
        );
    }

    #[test]
    fn a_get_answers_the_text_or_why_not() {
        let (mux, mut cap) = capture_mux();
        answer_get(&mux, Ok("hi".into()));
        assert_eq!(cap.ctrl(), AgentMsg::Clipboard { text: "hi".into() });
        answer_get(&mux, Err("held open".into()));
        assert_eq!(
            cap.ctrl(),
            AgentMsg::ClipboardFailed {
                msg: "held open".into()
            }
        );
    }

    /// The lock screen of a machine nobody has signed in to is
    /// `ERROR_NO_TOKEN`, and it is named as such — not as an OS error code.
    #[test]
    fn no_token_is_nobody_logged_on() {
        assert_eq!(token_failure(1008), NO_DESKTOP_LOGON);
        let other = token_failure(5);
        assert!(other.starts_with("cannot reach"), "{other}");
    }

    #[test]
    fn helper_requests_roundtrip() {
        assert_eq!(
            helper::parse_request(&helper::set_request("a\nb")),
            Some(helper::Request::Set("a\nb".into()))
        );
        assert_eq!(
            helper::parse_request(&helper::get_request()),
            Some(helper::Request::Get)
        );
        assert_eq!(helper::parse_request("{}"), None);
        assert_eq!(helper::parse_request("garbage"), None);
    }

    #[test]
    fn helper_lines_become_host_replies() {
        assert_eq!(
            helper::reply_msg(&helper::clip("x")),
            Some(AgentMsg::Clipboard { text: "x".into() })
        );
        assert_eq!(
            helper::reply_msg(&helper::set_ok()),
            Some(AgentMsg::ClipboardSet)
        );
        assert_eq!(
            helper::reply_msg(&helper::failed("busy")),
            Some(AgentMsg::ClipboardFailed { msg: "busy".into() })
        );
        assert_eq!(helper::reply_msg("{\"other\":1}"), None);
    }

    #[test]
    fn helper_reuse_requires_the_live_console_session() {
        assert!(helper::reuse_process(1, 1, None));
        assert!(!helper::reuse_process(1, 2, None));
        assert!(!helper::reuse_process(2, 1, None));
        assert!(!helper::reuse_process(u32::MAX, u32::MAX, None));
        assert!(!helper::reuse_process(1, 1, Some(0)));
        assert!(!helper::reuse_process(1, 1, Some(5)));
    }

    #[test]
    fn helper_startup_waits_for_connection_without_relaunching() {
        assert_eq!(helper::startup(false, None, false), Ok(false));
        assert_eq!(helper::startup(true, None, false), Ok(true));
        assert_eq!(helper::startup(true, None, true), Ok(true));
        assert!(
            helper::startup(false, None, true)
                .unwrap_err()
                .contains("still running")
        );
    }

    #[test]
    fn helper_early_exit_preserves_the_actual_error_instead_of_a_timeout() {
        for (connected, timed_out) in [(false, false), (true, false), (false, true)] {
            let error = helper::startup(connected, Some(5), timed_out).unwrap_err();
            assert!(error.contains("status 5, 0x00000005"), "{error}");
            assert!(!error.contains("within 5s"), "{error}");
        }
        let loader = helper::startup(false, Some(0xc000_0142), false).unwrap_err();
        assert!(loader.contains("0xc0000142"), "{loader}");
    }

    #[test]
    fn an_already_connected_pipe_is_success_not_a_launch_failure() {
        assert!(helper::pipe_connected(true, 0).is_ok());
        assert!(helper::pipe_connected(true, 5).is_ok());
        assert!(helper::pipe_connected(false, 535).is_ok());
        assert!(
            helper::pipe_connected(false, 5)
                .unwrap_err()
                .contains("ConnectNamedPipe")
        );
    }

    #[test]
    fn desktop_pipe_access_cannot_create_server_instances() {
        const FILE_CREATE_PIPE_INSTANCE: u32 = 4;
        assert_eq!(
            (helper::PIPE_READ | helper::PIPE_WRITE) & FILE_CREATE_PIPE_INSTANCE,
            0
        );
        assert_eq!(helper::PIPE_READ | helper::PIPE_WRITE, 0x0012_019b);
        assert_eq!(helper::PIPE_SECURITY, "D:P(A;;GA;;;SY)(A;;0x12019b;;;IU)");
    }
}
