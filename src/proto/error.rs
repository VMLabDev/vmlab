//! Structured failures on the wire (ADR-0007).
//!
//! A daemon reply used to carry prose and nothing else, so the web layer
//! classified HTTP status by substring-matching the daemon's wording — and
//! rewording an error silently changed an API contract. A reply now carries an
//! [`ErrorCode`] alongside the message: the code is the contract, the message
//! is free to change.

use serde::{Deserialize, Serialize};

/// Why a request failed, in the daemon's own terms.
///
/// The set is deliberately small. A code exists only where a surface acts on
/// the distinction — the web layer's HTTP status, the CLI's exit code — so
/// adding one means some caller will branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The command is not in this daemon's vocabulary.
    UnknownCommand,
    /// The arguments are missing, ill-typed, or out of range.
    InvalidArgument,
    /// The addressed thing — lab, machine, template, snapshot — does not exist.
    NotFound,
    /// The thing exists but is in a state that forbids the request: already
    /// running, already built, already there.
    Conflict,
    /// The thing exists and the request is well-formed, but this machine
    /// cannot serve it — no display, no console log, an agent without the
    /// feature.
    Unsupported,
    /// The operation was understood and attempted, and it failed. The default
    /// for anything a daemon does not classify more precisely.
    Failed,
    /// The daemon broke: a panic, a poisoned lock, a bug.
    Internal,
}

impl ErrorCode {
    /// The serialised spelling, for reports and generated clients.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::UnknownCommand => "unknown_command",
            ErrorCode::InvalidArgument => "invalid_argument",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Failed => "failed",
            ErrorCode::Internal => "internal",
        }
    }

    /// The HTTP status a REST surface answers with. Part of the published
    /// contract, not a web-layer preference: an integrator writing against
    /// the daemon should see the same classification the console does.
    pub fn http_status(self) -> u16 {
        match self {
            // A command or argument the daemon does not accept is the
            // caller's mistake, whichever half of it was wrong.
            ErrorCode::UnknownCommand | ErrorCode::InvalidArgument => 400,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict => 409,
            // Well-formed, and this machine simply cannot serve it: no
            // display, no console log, an agent feature never negotiated.
            ErrorCode::Unsupported => 501,
            // The daemon tried and failed, so from the browser's side the
            // thing behind the web server misbehaved.
            ErrorCode::Failed => 502,
            ErrorCode::Internal => 500,
        }
    }

    /// The process exit code `vmlab` leaves behind, so a script can branch on
    /// what went wrong without parsing the message.
    pub fn exit_code(self) -> u8 {
        match self {
            ErrorCode::UnknownCommand | ErrorCode::InvalidArgument => 2,
            ErrorCode::NotFound => 4,
            ErrorCode::Conflict => 5,
            ErrorCode::Unsupported => 6,
            // What every CLI failure has always exited with.
            ErrorCode::Failed | ErrorCode::Internal => 1,
        }
    }

    /// Every code, in declaration order — the input to generated clients and
    /// to the protocol report.
    pub const ALL: &'static [ErrorCode] = &[
        ErrorCode::UnknownCommand,
        ErrorCode::InvalidArgument,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::Unsupported,
        ErrorCode::Failed,
        ErrorCode::Internal,
    ];
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failed request: a code a caller can branch on, and prose for a human.
///
/// This is what a daemon command handler returns and what the client hands
/// back. `From<String>`/`From<anyhow::Error>` land on [`ErrorCode::Failed`], so
/// a handler that has nothing better to say still compiles — the constructors
/// below are for the sites that *do* know why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    pub code: ErrorCode,
    pub message: String,
}

impl CommandError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The command is not in this daemon's vocabulary.
    pub fn unknown_command(cmd: &str) -> Self {
        Self::new(
            ErrorCode::UnknownCommand,
            format!("unknown command `{cmd}`"),
        )
    }
    /// Arguments missing, ill-typed, or out of range.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }
    /// No such lab, machine, template or snapshot.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }
    /// Already running, already exists, already built.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }
    /// This machine cannot serve the request at all.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, message)
    }
    /// Attempted and failed, with nothing more specific to say.
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Failed, message)
    }
    /// The daemon broke.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    /// Re-code an error whose message is already right. Used where a helper
    /// produces the prose but only the caller knows what the failure means.
    pub fn with_code(mut self, code: ErrorCode) -> Self {
        self.code = code;
        self
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CommandError {}

impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self::failed(message)
    }
}

impl From<&str> for CommandError {
    fn from(message: &str) -> Self {
        Self::failed(message)
    }
}

impl From<anyhow::Error> for CommandError {
    fn from(e: anyhow::Error) -> Self {
        Self::failed(format!("{e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_through_their_wire_spelling() {
        for code in ErrorCode::ALL {
            let line = serde_json::to_string(code).unwrap();
            assert_eq!(line, format!("\"{}\"", code.as_str()));
            let back: ErrorCode = serde_json::from_str(&line).unwrap();
            assert_eq!(back, *code);
        }
    }

    /// A handler with nothing specific to say still reports a code, and it is
    /// the one that used to mean "bad gateway" in the web layer.
    /// Every code classifies for both surfaces, and a caller's own mistake is
    /// always distinguishable from a generic failure — that is the whole
    /// point of a script being able to branch on the exit code.
    #[test]
    fn every_code_classifies_for_both_surfaces() {
        for code in ErrorCode::ALL {
            let (status, exit) = (code.http_status(), code.exit_code());
            assert!((400..600).contains(&status), "{code}: status {status}");
            assert!(exit > 0, "{code}: a failure never exits 0");
            if (400..500).contains(&status) {
                assert_ne!(
                    exit, 1,
                    "{code}: the caller's fault, but exits like a crash"
                );
            }
        }
    }

    #[test]
    fn unclassified_failures_default_to_failed() {
        let e: CommandError = "it broke".to_string().into();
        assert_eq!(e.code, ErrorCode::Failed);
        assert_eq!(e.message, "it broke");
        let e: CommandError = anyhow::anyhow!("root").context("outer").into();
        assert_eq!(e.code, ErrorCode::Failed);
        assert_eq!(e.message, "outer: root");
    }
}
