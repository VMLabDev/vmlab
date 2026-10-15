//! The `app` vocabulary (PRD §20): the records that ride an
//! [`OpenApp`](crate::HostMsg::OpenApp) **data** channel.
//!
//! A shared app puts one guest program's windows on the host desktop. The
//! window protocol is the guest family's own — waypipe or X11 for a Linux
//! guest, RDP RemoteApp for a Windows one — and the agent only carries its
//! bytes. Each transport connection the protocol makes is its own channel,
//! opened by the host with [`OpenAppConn`](crate::HostMsg::OpenAppConn), so
//! every connection has its own credit window and a stalled one cannot hold
//! up the rest.
//!
//! Waypipe and X11 clients connect *out* of the guest, and ADR-0013 gives the
//! guest no channel open. So the agent accepts the guest-side connection,
//! parks it, and tells the host with a [`Pending`](AppRecord::Pending) record
//! on the already-open app channel; the host answers with an `OpenAppConn`,
//! which adopts the oldest parked connection. RDP runs the other way: the
//! host-side client dials, and each `OpenAppConn` makes the agent dial the
//! guest's own RDP listener on loopback.
//!
//! The records are length-prefixed inside the app channel's byte stream, the
//! same framing as [`crate::watch`]:
//!
//! ```text
//! len u32 LE | JSON (len bytes)
//! ```

use serde::{Deserialize, Serialize};

/// Largest single record either side accepts. App records are small; this is
/// a decoder-buffer bound, and anything larger is a desynced stream.
pub const MAX_RECORD: usize = 1024 * 1024;

/// How long the agent keeps an accepted guest-side connection parked waiting
/// for the host's [`OpenAppConn`](crate::HostMsg::OpenAppConn) before it
/// closes it.
pub const PENDING_GRACE_SECS: u64 = 30;

/// The guest TCP port an RDP app session's connections are dialled to, on
/// the guest's loopback interface.
pub const RDP_PORT: u16 = 3389;

/// Which window protocol an app session carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AppMode {
    /// Linux guest. The agent listens on a private unix socket and runs
    /// `waypipe --socket <it> server -- <argv>`; every connection waypipe
    /// makes to that socket becomes a [`AppRecord::Pending`].
    Waypipe,
    /// Linux guest. The agent listens as a fresh X display, writes an
    /// Xauthority file holding `cookie` (32 lowercase hex digits, an
    /// `MIT-MAGIC-COOKIE-1`) and runs `argv` with `DISPLAY` and `XAUTHORITY`
    /// pointing at them. The host minted the cookie and checks it on every
    /// connection before substituting its own display's real one.
    X11 { cookie: String },
    /// Windows guest. The agent makes sure the guest can serve a RemoteApp
    /// session (an edition with an RDP host, Remote Desktop enabled,
    /// unlisted programs allowed, the account able to log on remotely), then
    /// answers every [`OpenAppConn`](crate::HostMsg::OpenAppConn) by dialling
    /// `127.0.0.1:`[`RDP_PORT`]. It starts no process: the RDP client names
    /// the program.
    Rdp,
}

/// One record on an app channel. Each variant travels in one direction only.
///
/// The agent writes `Ready`, `Pending` and `Log`. `Exited` and `Failed` are
/// written by the lab daemon on its relay to the CLI, translating the
/// control-channel [`AgentMsg::Exited`](crate::AgentMsg::Exited) and
/// [`AgentMsg::Error`](crate::AgentMsg::Error) the CLI never sees; an agent
/// never sends them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum AppRecord {
    /// **agent→host**, once: the session is set up and, for waypipe and X11,
    /// the program has been started; for RDP, the guest's RDP listener
    /// accepts connections, so the host starts its RDP client on this record.
    /// `display` is the X display name the program was given (`":12"`),
    /// absent for the other modes.
    Ready {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<String>,
    },
    /// **agent→host**: a guest-side connection was accepted and is parked
    /// until the host opens an [`OpenAppConn`](crate::HostMsg::OpenAppConn)
    /// for it. One record per connection. Never sent in [`AppMode::Rdp`].
    Pending,
    /// **agent→host**: a line the program (or waypipe) wrote to stdout or
    /// stderr, lossily decoded, without its trailing newline.
    Log { text: String },
    /// **daemon→CLI**: the program exited with `code`; the session is over.
    Exited { code: i32 },
    /// **daemon→CLI**: the session failed and is over.
    Failed { msg: String },
}

/// Encode one record for an app channel's byte stream.
pub fn encode_record(record: &AppRecord) -> Vec<u8> {
    let json = serde_json::to_vec(record).expect("app record serializes");
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out
}

/// Reassembles records from an app channel's byte stream: feed every byte
/// with [`push`](Self::push), then drain with
/// [`next_record`](Self::next_record) until it yields `None`.
#[derive(Debug, Default)]
pub struct RecordDecoder {
    buf: Vec<u8>,
}

impl RecordDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete record, or `None` while more bytes are needed. An
    /// error means the stream is unusable, and the caller fails the session.
    pub fn next_record(&mut self) -> Result<Option<AppRecord>, String> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len > MAX_RECORD {
            return Err(format!("app record of {len} bytes exceeds {MAX_RECORD}"));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let record = serde_json::from_slice(&self.buf[4..4 + len])
            .map_err(|e| format!("undecodable app record: {e}"));
        self.buf.drain(..4 + len);
        Ok(Some(record?))
    }
}

/// Whether `cookie` is a well-formed [`AppMode::X11`] cookie: exactly 32
/// lowercase hex digits.
pub fn valid_cookie(cookie: &str) -> bool {
    cookie.len() == 32
        && cookie
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &[u8], chunk: usize) -> Vec<AppRecord> {
        let mut dec = RecordDecoder::new();
        let mut out = Vec::new();
        for part in bytes.chunks(chunk.max(1)) {
            dec.push(part);
            while let Some(r) = dec.next_record().unwrap() {
                out.push(r);
            }
        }
        out
    }

    /// Records survive being split across arbitrary frame boundaries.
    #[test]
    fn records_reassemble_across_any_chunking() {
        let records = vec![
            AppRecord::Ready {
                display: Some(":12".into()),
            },
            AppRecord::Pending,
            AppRecord::Log {
                text: "waypipe: hello".into(),
            },
            AppRecord::Exited { code: 3 },
            AppRecord::Failed { msg: "gone".into() },
        ];
        let bytes: Vec<u8> = records.iter().flat_map(encode_record).collect();
        for chunk in [1, 3, 7, 64, bytes.len()] {
            assert_eq!(decode_all(&bytes, chunk), records, "chunk {chunk}");
        }
    }

    /// The JSON spelling is part of the contract the agent and the CLI share.
    #[test]
    fn the_wire_spelling_is_stable() {
        assert_eq!(
            serde_json::to_string(&AppRecord::Pending).unwrap(),
            r#"{"record":"pending"}"#
        );
        assert_eq!(
            serde_json::to_string(&AppRecord::Ready { display: None }).unwrap(),
            r#"{"record":"ready"}"#
        );
        assert_eq!(
            serde_json::to_string(&AppMode::X11 {
                cookie: "00".repeat(16)
            })
            .unwrap(),
            format!(r#"{{"mode":"x11","cookie":"{}"}}"#, "00".repeat(16))
        );
        assert_eq!(
            serde_json::to_string(&AppMode::Rdp).unwrap(),
            r#"{"mode":"rdp"}"#
        );
    }

    /// An oversized length prefix is a desynced stream, not a huge record.
    #[test]
    fn an_oversized_record_is_refused() {
        let mut dec = RecordDecoder::new();
        dec.push(&((MAX_RECORD as u32) + 1).to_le_bytes());
        assert!(dec.next_record().is_err());
    }

    #[test]
    fn cookies_are_32_lowercase_hex_digits() {
        assert!(valid_cookie(&"0123456789abcdef".repeat(2)));
        assert!(!valid_cookie(&"0123456789ABCDEF".repeat(2)));
        assert!(!valid_cookie("abc"));
        assert!(!valid_cookie(&"g".repeat(32)));
    }
}
