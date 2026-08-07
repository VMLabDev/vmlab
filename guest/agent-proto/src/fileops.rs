//! The `fileops` vocabulary (PRD §19.5): the records that ride an
//! [`OpenFileOps`](crate::HostMsg::OpenFileOps) channel.
//!
//! **One channel that *is* an RPC session**, not a set of control messages.
//! Control frames are JSON and explicitly not flow-controlled, so a 40 MB
//! editor-server push would arrive base64-inflated and sit in front of every
//! keystroke and metrics sample; a channel per read is worse, since an SFTP
//! client keeps ~64 requests of 32 KiB in flight. So the records live inside
//! the channel's own credit window instead:
//!
//! ```text
//! json len u32 LE | payload len u32 LE | JSON (json len) | payload (payload len)
//! ```
//!
//! That keeps agent-proto's "JSON for control, raw for bulk" split at the
//! record level: a [`Read`](Op::Read) reply and a [`Write`](Op::Write) request
//! carry their bytes raw rather than base64-inflated inside the metadata.
//!
//! Three properties §19 fixes:
//!
//! 1. **Handle-based and offset-addressed** — `open → handle`, read/write at
//!    offset, `close`. A path-addressed vocabulary cannot express a client
//!    that opens once and writes 400 times, and cannot hold `O_APPEND` or
//!    `fsetstat` semantics at all. Handles are scoped to the channel and die
//!    with it.
//! 2. **Pipelined**: many requests outstanding at once, replies matched by
//!    [`Request::id`] and free to complete out of order. This is the
//!    throughput decision — serialised against the measured 59–111 ms round
//!    trip the SSH facade would deliver under 1 MB/s where the raw channel
//!    does 80.
//! 3. **SFTP-shaped by intent, in vmlab's spelling**, because the facade
//!    *transcodes* rather than adapts. Two places where the spelling differs
//!    deliberately: [`Op::Mkdir`] carries a case-sensitivity flag (NTFS
//!    accepts it only while the directory is empty, so it can never be a
//!    later `setstat`), and [`Op::Symlink`] carries the link kind, because
//!    Windows requires file-vs-directory at creation and a dangling link does
//!    not reveal it.
//!
//! An operation that fails answers [`Reply::Error`] against its own request
//! id; the channel stays live. Only a record the receiver cannot frame is
//! fatal — there is no resynchronisation point inside a channel — and that
//! fails the channel through [`AgentMsg::Error`](crate::AgentMsg::Error).

use serde::{Deserialize, Serialize};

pub use crate::watch::EntryKind;

/// Largest metadata record either side accepts — a decoder-buffer bound
/// rather than a policy. A [`Reply::Entries`] for a huge directory is the
/// biggest thing that travels here; anything past this is a desynced stream.
pub const MAX_META: usize = 8 * 1024 * 1024;

/// Largest raw payload one record may carry, and so the largest
/// [`Op::Read`] a client may ask for or [`Op::Write`] it may send. Sized well
/// above the 32 KiB an SFTP client uses, and inside the initial credit window
/// so a single record never deadlocks against it.
pub const MAX_DATA: usize = 128 * 1024;

/// How many entries one [`Op::ReadDir`] answers with before the client has to
/// ask again. A bound on the reply rather than on the directory: a million-
/// entry directory streams instead of arriving as one record.
pub const READDIR_CHUNK: usize = 512;

/// One request, host→agent. `id` is the client's; it is echoed by the reply
/// and is what makes out-of-order completion legible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

/// What a [`Request`] asks for. The set is what `scp` and the editors issue,
/// in vmlab's spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Open `path`, answering [`Reply::Handle`]. `mode` is Unix permission
    /// bits applied when the open creates the file, ignored on Windows —
    /// §19.5's "`mode` moves to `open`/`setstat`".
    Open {
        path: String,
        #[serde(default)]
        flags: OpenFlags,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
    /// Release a file or directory handle. Answers [`Reply::Ok`].
    Close { handle: u64 },
    /// Read `len` bytes at `offset`. Answers [`Reply::Data`] with the bytes
    /// as the record's raw payload.
    ///
    /// **A short or empty payload means end-of-file**, not a partial read —
    /// so a `len` past [`MAX_DATA`] is refused rather than quietly clamped,
    /// which would read as a truncated file.
    Read { handle: u64, offset: u64, len: u32 },
    /// Write the record's raw payload at `offset`. Answers [`Reply::Ok`]. On
    /// a handle opened with [`OpenFlags::append`] the offset is ignored and
    /// the bytes land at the end, which is what `O_APPEND` means.
    Write { handle: u64, offset: u64 },
    /// Attributes of an open handle. Answers [`Reply::Attrs`].
    Fstat { handle: u64 },
    /// Apply attributes to an open handle. Answers [`Reply::Ok`].
    Fsetstat { handle: u64, attrs: SetAttrs },
    /// Attributes of `path`, following symlinks. Answers [`Reply::Attrs`].
    Stat { path: String },
    /// Attributes of `path` itself, never followed. Answers [`Reply::Attrs`].
    Lstat { path: String },
    /// Apply attributes to `path`. Answers [`Reply::Ok`].
    Setstat { path: String, attrs: SetAttrs },
    /// Open a directory for reading, answering [`Reply::Handle`].
    OpenDir { path: String },
    /// Next slice of a directory handle's entries, answering
    /// [`Reply::Entries`]. Repeat until it reports `eof`.
    ReadDir { handle: u64 },
    /// Create a directory, answering [`Reply::Ok`].
    ///
    /// `case_sensitive` is the flag plain SFTP has no concept of, and the
    /// reason it rides the creation rather than a later
    /// [`Setstat`](Op::Setstat): NTFS accepts the per-directory case
    /// sensitivity flag only while the directory is still empty. Elsewhere
    /// (and on a filesystem that is already case-sensitive) it is satisfied
    /// by construction and costs nothing.
    Mkdir {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        #[serde(default)]
        case_sensitive: bool,
    },
    /// Remove an empty directory. Answers [`Reply::Ok`].
    Rmdir { path: String },
    /// Remove a file or symlink. Answers [`Reply::Ok`].
    Remove { path: String },
    /// Rename, answering [`Reply::Ok`]. Overwrites an existing `to` where the
    /// guest OS allows it.
    Rename { from: String, to: String },
    /// Canonicalise `path`, answering [`Reply::Name`]. The one the facade
    /// could not have invented for itself: it has to work on a Windows drive
    /// letter.
    Realpath { path: String },
    /// Create a symlink at `link` pointing at `target`, answering
    /// [`Reply::Ok`]. `kind` is carried because Windows requires
    /// file-vs-directory at creation and a dangling link does not reveal it.
    Symlink {
        target: String,
        link: String,
        kind: LinkKind,
    },
    /// Read a symlink's target, answering [`Reply::Name`].
    Readlink { path: String },
    /// The guest's own SHA-256 of what is on its disk at `path`, answering
    /// [`Reply::Digest`]. This is what keeps the whole-file transfer's
    /// strongest property when it retires: a push and a pull still verify
    /// what actually landed, and the workspace syncer leans on the same
    /// answer for change detection (§19.6).
    Digest { path: String },
}

/// How an [`Op::Open`] opens. All-false is a read-only open, which is what a
/// client that omits the field meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OpenFlags {
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    /// Create the file if it is absent.
    #[serde(default)]
    pub create: bool,
    /// Truncate an existing file to nothing.
    #[serde(default)]
    pub truncate: bool,
    /// Fail if the file already exists (implies `create`).
    #[serde(default)]
    pub exclusive: bool,
    /// Every write lands at the end, whatever offset it names.
    #[serde(default)]
    pub append: bool,
}

impl OpenFlags {
    /// Read an existing file.
    pub fn read() -> OpenFlags {
        OpenFlags {
            read: true,
            ..OpenFlags::default()
        }
    }

    /// Create or truncate a file and write it from the start — what a push
    /// does.
    pub fn create_truncate() -> OpenFlags {
        OpenFlags {
            write: true,
            create: true,
            truncate: true,
            ..OpenFlags::default()
        }
    }
}

/// Which kind of link [`Op::Symlink`] creates. Windows picks a different
/// object for each and cannot infer it from a target that is not there yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    File,
    Dir,
}

/// What is at a path, as [`Op::Stat`] and friends report it. `kind` is the
/// watch vocabulary's, so a syncer comparing a stat-walk against a watch
/// batch compares like with like (§19.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attrs {
    pub kind: EntryKind,
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Access time in nanoseconds since the Unix epoch.
    pub atime_ns: i64,
    /// Unix permission bits. Absent where the guest has none to report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

/// What a [`Op::Setstat`] / [`Op::Fsetstat`] changes. Every field is
/// optional and an absent one is left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SetAttrs {
    /// Unix permission bits, ignored on Windows — the same rule the
    /// whole-file transfer's `mode` carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Truncate or extend to this length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atime_ns: Option<i64>,
}

/// One directory entry in a [`Reply::Entries`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// The entry's own name, never a path. `.` and `..` are not reported.
    pub name: String,
    /// The entry itself, never followed through a symlink.
    pub attrs: Attrs,
}

/// One reply, agent→host, against the [`Request::id`] it answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub reply: Reply,
}

/// What an operation produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// It worked and had nothing to say.
    Ok,
    Handle {
        handle: u64,
    },
    /// The read's bytes are the record's raw payload; the metadata says only
    /// that this is a read reply. Fewer bytes than asked for (including none)
    /// is end-of-file.
    Data,
    Attrs {
        attrs: Attrs,
    },
    Entries {
        entries: Vec<DirEntry>,
        eof: bool,
    },
    /// A [`Op::Realpath`] or [`Op::Readlink`] answer.
    Name {
        path: String,
    },
    Digest {
        sha256: String,
        len: u64,
    },
    /// The operation failed. The channel is unaffected — a client that asked
    /// for a file that is not there keeps its session.
    Error {
        code: ErrorCode,
        msg: String,
    },
}

/// Why an operation failed, in the vocabulary the SSH facade transcodes into
/// SFTP status codes. `Failure` is the catch-all; `msg` always carries the
/// detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NoSuchFile,
    PermissionDenied,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    NotEmpty,
    /// The handle is not one this channel handed out, or is the wrong kind
    /// for the operation.
    BadHandle,
    /// This guest cannot do it at all (a symlink where the account holds no
    /// privilege to create one, a case-sensitivity flag the filesystem has no
    /// concept of).
    Unsupported,
    Failure,
}

impl ErrorCode {
    /// The code an OS error maps to. Everything unclassified is `Failure`,
    /// which is honest: the caller reads `msg`.
    pub fn of(err: &std::io::Error) -> ErrorCode {
        use std::io::ErrorKind as K;
        match err.kind() {
            K::NotFound => ErrorCode::NoSuchFile,
            K::PermissionDenied => ErrorCode::PermissionDenied,
            K::AlreadyExists => ErrorCode::AlreadyExists,
            K::NotADirectory => ErrorCode::NotADirectory,
            K::IsADirectory => ErrorCode::IsADirectory,
            K::DirectoryNotEmpty => ErrorCode::NotEmpty,
            K::Unsupported => ErrorCode::Unsupported,
            _ => ErrorCode::Failure,
        }
    }
}

/// Encode one record: JSON metadata, then the raw payload. Panics if the
/// payload exceeds [`MAX_DATA`] — callers chunk their reads and writes.
pub fn encode_record<T: Serialize>(msg: &T, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= MAX_DATA, "fileops payload too large");
    let json = serde_json::to_vec(msg).expect("fileops record serializes");
    let mut out = Vec::with_capacity(8 + json.len() + payload.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(payload);
    out
}

/// Reassembles records from a fileops channel's byte stream: feed every byte
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

    /// The next complete record and its raw payload, or `None` while more
    /// bytes are needed. An error means the stream is unusable: there is no
    /// resynchronisation point inside a channel, so the caller fails the
    /// channel rather than trying to skip the record.
    pub fn next_record<T: serde::de::DeserializeOwned>(
        &mut self,
    ) -> Result<Option<(T, Vec<u8>)>, String> {
        if self.buf.len() < 8 {
            return Ok(None);
        }
        let meta = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        let data = u32::from_le_bytes(self.buf[4..8].try_into().unwrap()) as usize;
        if meta > MAX_META {
            return Err(format!("fileops record of {meta} bytes exceeds {MAX_META}"));
        }
        if data > MAX_DATA {
            return Err(format!(
                "fileops payload of {data} bytes exceeds {MAX_DATA}"
            ));
        }
        if self.buf.len() < 8 + meta + data {
            return Ok(None);
        }
        let parsed: Result<T, String> = serde_json::from_slice(&self.buf[8..8 + meta])
            .map_err(|e| format!("undecodable fileops record: {e}"));
        let payload = self.buf[8 + meta..8 + meta + data].to_vec();
        self.buf.drain(..8 + meta + data);
        Ok(Some((parsed?, payload)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs() -> Attrs {
        Attrs {
            kind: EntryKind::File,
            size: 4096,
            mtime_ns: 1_700_000_000_123_456_789,
            atime_ns: 1_700_000_000_000_000_000,
            mode: Some(0o644),
        }
    }

    /// Every op and every reply survives the wire, whatever the chunking the
    /// credit window happens to impose — a record is chunked across as many
    /// frames as its length needs, so the decoder is fed arbitrary slices.
    #[test]
    fn records_round_trip_whatever_the_chunking() {
        let requests = [
            Request {
                id: 1,
                op: Op::Open {
                    path: "/srv/app.conf".into(),
                    flags: OpenFlags::create_truncate(),
                    mode: Some(0o640),
                },
            },
            Request {
                id: 2,
                op: Op::Write {
                    handle: 7,
                    offset: 65536,
                },
            },
            Request {
                id: 3,
                op: Op::Read {
                    handle: 7,
                    offset: 0,
                    len: 32768,
                },
            },
            Request {
                id: 4,
                op: Op::Mkdir {
                    path: "C:\\src".into(),
                    mode: Some(0o755),
                    case_sensitive: true,
                },
            },
            Request {
                id: 5,
                op: Op::Symlink {
                    target: "../lib".into(),
                    link: "/srv/lib".into(),
                    kind: LinkKind::Dir,
                },
            },
            Request {
                id: 6,
                op: Op::Setstat {
                    path: "/srv/app.conf".into(),
                    attrs: SetAttrs {
                        mode: Some(0o600),
                        size: Some(0),
                        mtime_ns: Some(-1),
                        atime_ns: None,
                    },
                },
            },
            Request {
                id: 7,
                op: Op::Digest {
                    path: "/srv/app.conf".into(),
                },
            },
        ];
        let mut wire = Vec::new();
        for (i, r) in requests.iter().enumerate() {
            // Only a write carries bytes; the rest are metadata alone.
            let payload = if i == 1 {
                vec![0xabu8; 300]
            } else {
                Vec::new()
            };
            wire.extend(encode_record(r, &payload));
        }
        for chunk in [1, 7, 64, 4096] {
            let mut dec = RecordDecoder::new();
            let mut got: Vec<(Request, Vec<u8>)> = Vec::new();
            for part in wire.chunks(chunk) {
                dec.push(part);
                while let Some(r) = dec.next_record().unwrap() {
                    got.push(r);
                }
            }
            assert_eq!(got.len(), requests.len(), "chunk {chunk}");
            for (i, (req, payload)) in got.iter().enumerate() {
                assert_eq!(req, &requests[i], "chunk {chunk}");
                assert_eq!(payload.len(), if i == 1 { 300 } else { 0 });
            }
        }
    }

    #[test]
    fn replies_round_trip() {
        let replies = vec![
            Response {
                id: 1,
                reply: Reply::Ok,
            },
            Response {
                id: 2,
                reply: Reply::Handle { handle: 9 },
            },
            Response {
                id: 3,
                reply: Reply::Data,
            },
            Response {
                id: 4,
                reply: Reply::Attrs { attrs: attrs() },
            },
            Response {
                id: 5,
                reply: Reply::Entries {
                    entries: vec![DirEntry {
                        name: "main.rs".into(),
                        attrs: attrs(),
                    }],
                    eof: true,
                },
            },
            Response {
                id: 6,
                reply: Reply::Name {
                    path: "C:\\src\\app".into(),
                },
            },
            Response {
                id: 7,
                reply: Reply::Digest {
                    sha256: "ab".repeat(32),
                    len: 1 << 30,
                },
            },
            Response {
                id: 8,
                reply: Reply::Error {
                    code: ErrorCode::NoSuchFile,
                    msg: "no such file or directory".into(),
                },
            },
        ];
        let mut wire = Vec::new();
        for r in &replies {
            let payload = if matches!(r.reply, Reply::Data) {
                b"file bytes".to_vec()
            } else {
                Vec::new()
            };
            wire.extend(encode_record(r, &payload));
        }
        let mut dec = RecordDecoder::new();
        dec.push(&wire);
        for want in &replies {
            let (got, payload): (Response, Vec<u8>) = dec.next_record().unwrap().unwrap();
            assert_eq!(&got, want);
            if matches!(want.reply, Reply::Data) {
                assert_eq!(payload, b"file bytes");
            }
        }
        assert!(dec.next_record::<Response>().unwrap().is_none());
    }

    /// The bytes of a read or a write are raw, not base64 inside the JSON —
    /// that split is the whole reason the vocabulary is framed rather than
    /// sent as control messages.
    #[test]
    fn payload_bytes_ride_raw_beside_the_json() {
        let payload: Vec<u8> = (0..=255u8).collect();
        let wire = encode_record(
            &Request {
                id: 4,
                op: Op::Write {
                    handle: 1,
                    offset: 0,
                },
            },
            &payload,
        );
        let meta = u32::from_le_bytes(wire[..4].try_into().unwrap()) as usize;
        assert_eq!(
            u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize,
            payload.len()
        );
        assert_eq!(&wire[8 + meta..], &payload[..]);
        assert_eq!(
            std::str::from_utf8(&wire[8..8 + meta]).unwrap(),
            r#"{"id":4,"op":"write","handle":1,"offset":0}"#
        );
    }

    /// §19.5's two spellings plain SFTP has no room for: the case-sensitivity
    /// flag rides the `mkdir` (NTFS takes it only while the directory is
    /// empty, so it can never be a later `setstat`) and the link kind rides
    /// the symlink (Windows picks the object at creation).
    #[test]
    fn mkdir_carries_case_sensitivity_and_a_symlink_carries_its_kind() {
        assert_eq!(
            serde_json::to_string(&Request {
                id: 1,
                op: Op::Mkdir {
                    path: "/src".into(),
                    mode: None,
                    case_sensitive: true,
                },
            })
            .unwrap(),
            r#"{"id":1,"op":"mkdir","path":"/src","case_sensitive":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Request {
                id: 2,
                op: Op::Symlink {
                    target: "/src/pkg".into(),
                    link: "/src/link".into(),
                    kind: LinkKind::Dir,
                },
            })
            .unwrap(),
            r#"{"id":2,"op":"symlink","target":"/src/pkg","link":"/src/link","kind":"dir"}"#
        );
    }

    /// An open with no flags is a read-only open, and `mode` is absent rather
    /// than null — the field only means anything when the open creates.
    #[test]
    fn an_open_defaults_to_read_only() {
        let json = r#"{"id":1,"op":"open","path":"/etc/motd"}"#;
        let (req, _): (Request, Vec<u8>) = {
            let mut dec = RecordDecoder::new();
            dec.push(&encode_record(
                &serde_json::from_str::<serde_json::Value>(json).unwrap(),
                b"",
            ));
            dec.next_record().unwrap().unwrap()
        };
        assert_eq!(
            req,
            Request {
                id: 1,
                op: Op::Open {
                    path: "/etc/motd".into(),
                    flags: OpenFlags::default(),
                    mode: None,
                },
            }
        );
    }

    #[test]
    fn an_oversized_record_fails_the_stream() {
        let mut dec = RecordDecoder::new();
        dec.push(&(MAX_META as u32 + 1).to_le_bytes());
        dec.push(&0u32.to_le_bytes());
        assert!(
            dec.next_record::<Request>()
                .unwrap_err()
                .contains("exceeds")
        );

        let mut dec = RecordDecoder::new();
        dec.push(&2u32.to_le_bytes());
        dec.push(&(MAX_DATA as u32 + 1).to_le_bytes());
        assert!(
            dec.next_record::<Request>()
                .unwrap_err()
                .contains("payload")
        );
    }

    #[test]
    fn undecodable_json_fails_the_stream() {
        let mut dec = RecordDecoder::new();
        dec.push(&2u32.to_le_bytes());
        dec.push(&0u32.to_le_bytes());
        dec.push(b"[]");
        assert!(dec.next_record::<Request>().is_err());
    }

    /// Every OS error a filesystem op can produce reaches the client as
    /// something it can branch on, so the facade never has to parse `msg`.
    #[test]
    fn os_errors_map_onto_codes_the_facade_can_transcode() {
        use std::io::{Error, ErrorKind};
        for (kind, want) in [
            (ErrorKind::NotFound, ErrorCode::NoSuchFile),
            (ErrorKind::PermissionDenied, ErrorCode::PermissionDenied),
            (ErrorKind::AlreadyExists, ErrorCode::AlreadyExists),
            (ErrorKind::NotADirectory, ErrorCode::NotADirectory),
            (ErrorKind::DirectoryNotEmpty, ErrorCode::NotEmpty),
            (ErrorKind::WouldBlock, ErrorCode::Failure),
        ] {
            assert_eq!(ErrorCode::of(&Error::new(kind, "x")), want, "{kind:?}");
        }
    }
}
