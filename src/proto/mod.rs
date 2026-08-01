//! CLI ↔ daemon wire protocol (PRD §3.1): JSON lines over unix domain
//! sockets, supporting request/response, a subscribable event stream, and
//! streamed output for long operations. Supervisor ↔ lab-daemon control uses
//! the same protocol.

pub mod client;
pub mod error;
pub mod report;
pub mod server;
pub mod vocab;

pub use error::{CommandError, ErrorCode};
pub use vocab::{ArgSpec, CommandSpec, LabRequest, OneWay, Region, SupRequest, WireRequest};

/// The most one file transfer may carry inline, base64, in a single wire
/// message — `machine.push_file`'s `data` and `machine.pull_file`'s reply.
///
/// Inline bytes are how a caller that holds a file rather than a path — a
/// browser, above all — moves one through the daemon, and they are the one
/// thing on this transport that is not a small JSON object. The ceiling is
/// what keeps "not small" from meaning "unbounded": [`server::MAX_REQ_LINE`]
/// is derived from it, so a request that respects it always fits, and a
/// transfer that exceeds it is refused by code
/// ([`ErrorCode::InvalidArgument`], naming the limit) rather than discovered
/// as a truncated file or a dropped connection. Larger transfers use the
/// host-path forms, which stream and never touch the wire.
pub const INLINE_FILE_LIMIT: u64 = 8 * 1024 * 1024;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One wire message, one JSON line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Client → server command.
    Req {
        id: u64,
        cmd: String,
        #[serde(default)]
        args: Value,
    },
    /// Server → client final answer for `id`.
    Resp {
        id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        ok: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        err: Option<String>,
        /// Why `err` happened, in the daemon's own terms. Absent on success,
        /// and absent from a daemon older than ADR-0007 — which is why
        /// [`ErrorCode::Failed`] stands in for a missing one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<ErrorCode>,
    },
    /// Server → client incremental output for a long-running `id`
    /// (template builds, provision runs). Always followed eventually by a
    /// `Resp` with the same id.
    Stream { id: u64, chunk: String },
    /// Server → client broadcast event (after `subscribe`).
    Event { event: String, data: Value },
}

/// A structured daemon event (PRD §8.1) as carried on the wire and in logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    /// Lab the event belongs to; empty for host-scoped events.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lab: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    pub ts: chrono::DateTime<chrono::Utc>,
}

impl Event {
    pub fn new(event: impl Into<String>, lab: impl Into<String>, data: Value) -> Self {
        Self {
            event: event.into(),
            lab: lab.into(),
            data,
            ts: chrono::Utc::now(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The daemon answered, and the answer was a failure it classified.
    #[error("{0}")]
    Remote(CommandError),
    #[error("connection closed")]
    Closed,
}

impl ProtoError {
    /// What this failure means to a caller. Everything that is not the
    /// daemon's own verdict is a failure of the call itself.
    pub fn code(&self) -> ErrorCode {
        match self {
            ProtoError::Remote(e) => e.code,
            _ => ErrorCode::Failed,
        }
    }
}

impl From<ProtoError> for CommandError {
    fn from(e: ProtoError) -> CommandError {
        match e {
            ProtoError::Remote(inner) => inner,
            other => CommandError::failed(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip() {
        let m = Message::Req {
            id: 7,
            cmd: "status".into(),
            args: serde_json::json!({"a": 1}),
        };
        let line = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&line).unwrap();
        match back {
            Message::Req { id, cmd, args } => {
                assert_eq!(id, 7);
                assert_eq!(cmd, "status");
                assert_eq!(args["a"], 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn resp_omits_empty_sides() {
        let m = Message::Resp {
            id: 1,
            ok: Some(serde_json::json!(true)),
            err: None,
            code: None,
        };
        let line = serde_json::to_string(&m).unwrap();
        assert!(!line.contains("err"));
        assert!(!line.contains("code"));
    }

    /// A failure carries both halves: the code a caller branches on and the
    /// prose a human reads.
    #[test]
    fn resp_carries_an_error_code_beside_the_message() {
        let m = Message::Resp {
            id: 1,
            ok: None,
            err: Some("dc01 is already running".into()),
            code: Some(ErrorCode::Conflict),
        };
        let line = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&line).unwrap();
        let Message::Resp { err, code, .. } = back else {
            panic!("wrong variant");
        };
        assert_eq!(code, Some(ErrorCode::Conflict));
        assert_eq!(err.as_deref(), Some("dc01 is already running"));
    }

    /// A reply from a daemon that predates ADR-0007 still parses; the missing
    /// code simply is not there.
    #[test]
    fn resp_without_a_code_still_parses() {
        let back: Message = serde_json::from_str(r#"{"type":"resp","id":1,"err":"nope"}"#).unwrap();
        let Message::Resp { code, .. } = back else {
            panic!("wrong variant");
        };
        assert_eq!(code, None);
    }
}
