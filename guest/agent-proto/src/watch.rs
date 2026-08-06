//! The `watch` vocabulary (PRD §19.5): the records that ride an
//! [`OpenWatch`](crate::HostMsg::OpenWatch) **data** channel.
//!
//! **The watch reports paths, not events.** The agent holds a coalescing set
//! of dirty paths; the host drains it and gets one [`StatRecord`] per path —
//! the path plus its current kind, size and mtime, or a tombstone if the path
//! is gone. That record is byte-identical to the one the reconciliation
//! stat-walk emits, so one vocabulary serves both and no platform event kind
//! ever crosses the seam. `inotify` and `ReadDirectoryChangesW` disagreeing
//! on renames, on in-place same-size writes and on whether a directory delete
//! implies its children therefore never becomes a vocabulary problem.
//!
//! The records are length-prefixed inside the channel's byte stream:
//!
//! ```text
//! len u32 LE | JSON (len bytes)
//! ```
//!
//! A data channel rather than the control channel, because a 30 000-path
//! batch is megabytes of JSON and control frames are not flow-controlled — it
//! would sit in front of every keystroke and metrics sample. A record is
//! chunked across as many [`Data`](crate::FrameKind::Data) frames as its
//! length needs, so a receiver feeds every byte it gets to a
//! [`RecordDecoder`].
//!
//! Two things this vocabulary deliberately lacks: **no request id** (at most
//! one [`Drain`](WatchRecord::Drain) is ever outstanding, and a field that is
//! always the same value invites pipelining that set-swap semantics cannot
//! support) and **no batch ack** (a dropped channel already implies a
//! stat-walk, so the loss self-heals through a path that has to exist
//! anyway).

use serde::{Deserialize, Serialize};

/// Cap on the guest's dirty set. Without one, a container micro-VM's set is
/// an unbounded allocation. Exceeding it collapses the whole batch to
/// [`WatchRecord::Rescan`], which is also why the cap doubles as the batch
/// bound: a drain never needs pagination. Sized so a full batch is a few MB
/// of JSON — past that the host's stat-walk is cheaper than the transfer.
pub const DIRTY_SET_CAP: usize = 20_000;

/// Largest single record either side accepts, a decoder-buffer bound rather
/// than a policy: a full [`DIRTY_SET_CAP`] batch of long paths fits with room
/// to spare, and anything larger is a desynced stream.
pub const MAX_RECORD: usize = 32 * 1024 * 1024;

/// One record on a watch channel. Each variant travels in one direction
/// only; a receiver that sees a wrong-direction record treats the channel as
/// desynced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum WatchRecord {
    /// **agent→host**, unsolicited: the dirty set went empty → non-empty.
    /// Exactly one nudge per drain window, so a build burst sends one and the
    /// host drains immediately when idle but batches naturally under load.
    Dirty,
    /// **host→agent**: swap the dirty set out atomically. The agent answers
    /// with one [`Batch`](WatchRecord::Batch) or one
    /// [`Rescan`](WatchRecord::Rescan).
    Drain,
    /// **agent→host**: the swapped-out set, one record per path.
    Batch { entries: Vec<StatRecord> },
    /// **agent→host**: overflow — the batch is replaced by this. Every source
    /// (a platform's own event-queue overflow, the [`DIRTY_SET_CAP`], a
    /// subtree that vanished without per-child events) collapses to this one
    /// value: the host runs a stat-walk and never needs to know which fired.
    Rescan,
}

/// One path's state. Absent `stat` is a **tombstone**: nothing is at `path`
/// any more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatRecord {
    /// Relative to the watch root, `/`-separated on every guest OS.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stat: Option<Stat>,
}

impl StatRecord {
    /// The path is gone.
    pub fn tombstone(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            stat: None,
        }
    }
}

/// What is at a path right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    pub kind: EntryKind,
    /// Bytes for a file, the link target's length for a symlink, otherwise
    /// whatever the OS reports for the directory entry itself.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch (negative
    /// before it).
    pub mtime_ns: i64,
}

/// The kind of a directory entry, never followed through a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    /// Socket, fifo, device — the host decides what to do with it (§19.6
    /// skips special files loudly).
    Other,
}

/// Encode one record for a watch channel's byte stream.
pub fn encode_record(record: &WatchRecord) -> Vec<u8> {
    let json = serde_json::to_vec(record).expect("watch record serializes");
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out
}

/// Reassembles records from a watch channel's byte stream: feed every byte
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
    /// error means the stream is unusable (oversized or unparseable record):
    /// there is no resynchronisation point inside a channel, so the caller
    /// fails the channel — which the host answers with a stat-walk.
    pub fn next_record(&mut self) -> Result<Option<WatchRecord>, String> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len > MAX_RECORD {
            return Err(format!("watch record of {len} bytes exceeds {MAX_RECORD}"));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let record = serde_json::from_slice(&self.buf[4..4 + len])
            .map_err(|e| format!("undecodable watch record: {e}"));
        self.buf.drain(..4 + len);
        Ok(Some(record?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &[u8], chunk: usize) -> Vec<WatchRecord> {
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

    fn sample_batch() -> WatchRecord {
        WatchRecord::Batch {
            entries: vec![
                StatRecord {
                    path: "src/main.rs".into(),
                    stat: Some(Stat {
                        kind: EntryKind::File,
                        size: 4096,
                        mtime_ns: 1_700_000_000_123_456_789,
                    }),
                },
                StatRecord::tombstone("src/old.rs"),
            ],
        }
    }

    #[test]
    fn records_round_trip_whatever_the_chunking() {
        let mut wire = Vec::new();
        for r in [
            WatchRecord::Dirty,
            WatchRecord::Drain,
            sample_batch(),
            WatchRecord::Rescan,
        ] {
            wire.extend(encode_record(&r));
        }
        for chunk in [1, 7, 64, 4096] {
            let got = decode_all(&wire, chunk);
            assert_eq!(got.len(), 4, "chunk {chunk}");
            assert_eq!(got[0], WatchRecord::Dirty);
            assert_eq!(got[1], WatchRecord::Drain);
            assert_eq!(got[2], sample_batch());
            assert_eq!(got[3], WatchRecord::Rescan);
        }
    }

    /// A tombstone is the *absence* of a stat, not a kind: the host tells the
    /// two apart without a per-event vocabulary.
    #[test]
    fn a_tombstone_carries_no_stat() {
        let json = serde_json::to_string(&StatRecord::tombstone("gone.txt")).unwrap();
        assert_eq!(json, r#"{"path":"gone.txt"}"#);
    }

    #[test]
    fn record_tags_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&WatchRecord::Rescan).unwrap(),
            r#"{"record":"rescan"}"#
        );
        assert_eq!(
            serde_json::to_string(&WatchRecord::Batch {
                entries: vec![StatRecord {
                    path: "a".into(),
                    stat: Some(Stat {
                        kind: EntryKind::Dir,
                        size: 0,
                        mtime_ns: 1,
                    }),
                }],
            })
            .unwrap(),
            r#"{"record":"batch","entries":[{"path":"a","stat":{"kind":"dir","size":0,"mtime_ns":1}}]}"#
        );
    }

    #[test]
    fn an_oversized_length_fails_the_stream() {
        let mut dec = RecordDecoder::new();
        dec.push(&(MAX_RECORD as u32 + 1).to_le_bytes());
        let err = dec.next_record().unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn undecodable_json_fails_the_stream() {
        let mut dec = RecordDecoder::new();
        dec.push(&(2u32).to_le_bytes());
        dec.push(b"[]");
        assert!(dec.next_record().is_err());
    }
}
