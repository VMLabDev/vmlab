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
}
