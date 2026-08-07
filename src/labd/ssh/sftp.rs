//! `subsystem sftp`, answered host-side and transcoded onto `fileops`
//! (PRD §19.3 and §19.5, ADR-0012).
//!
//! The facade implements SFTP itself rather than shipping an implementation
//! into two guest targets — which is also what makes the file vocabulary
//! reusable by the console and the syncer. What crosses the agent channel is
//! `fileops`, so a client packet is **transcoded**: one SFTP request becomes
//! one `fileops` request and its reply becomes one SFTP response. Nothing in
//! between invents a file abstraction of its own, which is why `realpath` on
//! a Windows drive letter is the guest's own answer, carried back verbatim.
//!
//! **Identity is the property this exists to keep.** The session is opened
//! with the connection's logon and no other, so a file operation resolves the
//! same (account, secret) as the shell and lands on the same cached logon,
//! the same `LogonId` and the same view of mapped drives (§19.2). There is one
//! `logon` value on a connection and both opens carry it, so that is true by
//! construction rather than by discipline.
//!
//! **Flow control is the coupling §19.3 calls a requirement rather than an
//! implementation detail**: the facade must never grant SSH window it cannot
//! back with agent credit. Three bounds hold it, and none of them is a queue
//! that grows with the transfer:
//!
//! - the client's bytes arrive one chunk at a time over a depth-1 channel, and
//!   russh re-grants SSH window only when the handler that fed it returns;
//! - at most [`INFLIGHT_OPS`] reads or writes are outstanding, each of which
//!   blocks on the guest's own credit before it is on the wire;
//! - everything that is *not* a read or a write drains those first, which is
//!   also what keeps a `close` from overtaking the writes it must follow.
//!
//! So the tens-of-megabytes editor-server push is throttled by the guest, and
//! `labd` holds a bounded window of it rather than the difference. The
//! back-pressure reaches the *client* rather than a queue here, which is the
//! trade §19.3 is asking for: a client that stops reading its own channel
//! stops being able to write to it, exactly as a shell's does.
//!
//! Version 3 and nothing else. It is what OpenSSH speaks, and what `scp`
//! (SFTP-backed since OpenSSH 9.0) and the editors issue. No extension is
//! advertised: each one a client asks for is answered `OP_UNSUPPORTED`, which
//! is a status carrying vmlab's own words — the one refusal in the facade that
//! the protocol lets vmlab narrate itself.

use std::sync::Arc;

use russh::ChannelId;
use russh::server::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use vmlab_agent_proto::fileops::MAX_DATA;

use super::session::ToGuest;
use crate::labd::vm_agent::{
    Attrs, EntryKind, ErrorCode, FileOps, LinkKind, Op, OpenFlags, Reply, SetAttrs,
};
use crate::sync::LockRecover;

/// The protocol version the facade speaks. OpenSSH's client and every editor
/// in §19's set speak 3; nothing here needs 4's typed attributes, and 6's
/// negotiation would be a second dialect to keep honest for no consumer.
const VERSION: u32 = 3;

/// How many reads or writes ride the `fileops` session at once.
///
/// This is the throughput decision §19.5 makes and the bound the flow-control
/// requirement rests on, in one number: an SFTP client keeps ~64 requests of
/// 32 KiB in flight, and serialising them against the channel's round trip
/// would deliver under 1 MB/s where the raw channel does 80. Matched to the
/// window the whole-file transfer already uses.
const INFLIGHT_OPS: usize = 16;

/// How many finished replies may wait for the SSH channel's own window. Small
/// on purpose: a reply that cannot be written is the client not reading, and
/// queueing more of them is buffering in `labd` for a client's benefit.
const REPLY_QUEUE: usize = 4;

/// Largest packet the facade will frame. An SFTP client's write is its read
/// buffer plus a header — 32 KiB by default, and 255 KiB at the very most —
/// so anything past this is a desynchronised stream rather than a big request.
const MAX_PACKET: usize = 256 * 1024;

/// The handles opened with `O_APPEND`, whose writes cannot be pipelined.
///
/// `O_APPEND` means the offset is not the client's to choose (§19.5): the
/// bytes land at the end, wherever the write said. Two appends running at once
/// would therefore land in *completion* order and interleave, so they are the
/// one write the pipeline must not carry — and this is how it knows which.
#[derive(Default)]
struct Appends(std::sync::Mutex<std::collections::HashSet<u64>>);

impl Appends {
    fn insert(&self, handle: u64) {
        self.0.lock_recover().insert(handle);
    }

    fn remove(&self, handle: u64) {
        self.0.lock_recover().remove(&handle);
    }

    fn holds(&self, handle: u64) -> bool {
        self.0.lock_recover().contains(&handle)
    }
}

/// Packet types (RFC draft-ietf-secsh-filexfer-02, which is version 3).
pub(super) mod kind {
    pub const INIT: u8 = 1;
    pub const VERSION: u8 = 2;
    pub const OPEN: u8 = 3;
    pub const CLOSE: u8 = 4;
    pub const READ: u8 = 5;
    pub const WRITE: u8 = 6;
    pub const LSTAT: u8 = 7;
    pub const FSTAT: u8 = 8;
    pub const SETSTAT: u8 = 9;
    pub const FSETSTAT: u8 = 10;
    pub const OPENDIR: u8 = 11;
    pub const READDIR: u8 = 12;
    pub const REMOVE: u8 = 13;
    pub const MKDIR: u8 = 14;
    pub const RMDIR: u8 = 15;
    pub const REALPATH: u8 = 16;
    pub const STAT: u8 = 17;
    pub const RENAME: u8 = 18;
    pub const READLINK: u8 = 19;
    pub const SYMLINK: u8 = 20;

    pub const STATUS: u8 = 101;
    pub const HANDLE: u8 = 102;
    pub const DATA: u8 = 103;
    pub const NAME: u8 = 104;
    pub const ATTRS: u8 = 105;
}

/// The status codes a version-3 reply carries.
pub(super) mod status {
    pub const OK: u32 = 0;
    pub const EOF: u32 = 1;
    pub const NO_SUCH_FILE: u32 = 2;
    pub const PERMISSION_DENIED: u32 = 3;
    pub const FAILURE: u32 = 4;
    pub const BAD_MESSAGE: u32 = 5;
    pub const OP_UNSUPPORTED: u32 = 8;
}

/// Which fields an attribute block carries.
const ATTR_SIZE: u32 = 0x0000_0001;
const ATTR_UIDGID: u32 = 0x0000_0002;
const ATTR_PERMISSIONS: u32 = 0x0000_0004;
const ATTR_ACMODTIME: u32 = 0x0000_0008;
const ATTR_EXTENDED: u32 = 0x8000_0000;

/// How an `open` opens, as version 3 spells it.
const PF_READ: u32 = 0x0000_0001;
const PF_WRITE: u32 = 0x0000_0002;
const PF_APPEND: u32 = 0x0000_0004;
const PF_CREAT: u32 = 0x0000_0008;
const PF_TRUNC: u32 = 0x0000_0010;
const PF_EXCL: u32 = 0x0000_0020;

/// The file-type bits a version-3 `permissions` field carries. A client reads
/// them rather than a kind field — `scp -r` and every `ls` decide "directory"
/// here — so they are put back from [`Attrs::kind`], which is the guest's
/// authority on what a path is.
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

// ---------------------------------------------------------------------------
// Serving one channel
// ---------------------------------------------------------------------------

/// Serve `subsystem sftp` on `channel` until the client is done with it.
///
/// The `fileops` session is this channel's: its handles are scoped to it and
/// die with it, so a client that hangs up mid-transfer leaves the guest
/// holding nothing.
pub(super) async fn serve(
    ops: FileOps,
    mut from_client: mpsc::Receiver<ToGuest>,
    handle: Handle,
    channel: ChannelId,
) {
    let ops = Arc::new(ops);
    let (replies, mut outbox) = mpsc::channel::<Vec<u8>>(REPLY_QUEUE);

    // One writer, so two operations finishing at once cannot interleave their
    // packets inside the channel's byte stream. It is also where the reverse
    // direction's flow control lives: `data` waits for the client's window.
    let writer = {
        let handle = handle.clone();
        tokio::spawn(async move {
            while let Some(bytes) = outbox.recv().await {
                if handle.data(channel, bytes).await.is_err() {
                    break;
                }
            }
        })
    };

    let failure = pump(&ops, &mut from_client, &replies).await;
    drop(replies);
    let _ = writer.await;

    if let Some(reason) = &failure {
        let _ = handle
            .extended_data(channel, 1, format!("vmlab: sftp: {reason}\n"))
            .await;
    }
    // `sftp-server` exits 0 when its client hangs up, and `scp`'s own exit
    // code is this status — so it is sent for the ordinary end of a session
    // as much as for the failed one.
    let code = u32::from(failure.is_some());
    let _ = handle.exit_status_request(channel, code).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

/// Frame packets off the channel and answer them, returning the reason the
/// stream became unusable — there is no resynchronisation point once framing
/// is lost, so that reason ends the session.
async fn pump(
    ops: &Arc<FileOps>,
    from_client: &mut mpsc::Receiver<ToGuest>,
    replies: &mpsc::Sender<Vec<u8>>,
) -> Option<String> {
    let mut decoder = PacketDecoder::default();
    let mut inflight: JoinSet<()> = JoinSet::new();
    let appends = Arc::new(Appends::default());

    while let Some(msg) = from_client.recv().await {
        let bytes = match msg {
            ToGuest::Data(bytes) => bytes,
            // The client has said everything it is going to say; what is
            // already in flight still finishes.
            ToGuest::Eof => break,
            // A subsystem has no terminal to resize.
            ToGuest::Resize(_, _) => continue,
        };
        decoder.push(&bytes);
        loop {
            match decoder.next_packet() {
                Ok(None) => break,
                Err(e) => {
                    inflight.shutdown().await;
                    return Some(e);
                }
                Ok(Some(request)) => {
                    if request.is_bulk(&appends) {
                        // Pipelined, and bounded: the wait here is what stops
                        // the SSH window being re-granted past what the guest
                        // has credit for.
                        while inflight.len() >= INFLIGHT_OPS {
                            inflight.join_next().await;
                        }
                        let ops = ops.clone();
                        let replies = replies.clone();
                        let appends = appends.clone();
                        inflight.spawn(async move {
                            let _ = replies.send(answer(&ops, request, &appends).await).await;
                        });
                    } else {
                        // Everything else waits for the reads and writes
                        // already out. Reordering `close` in front of the
                        // writes it must follow would be a truncated file, and
                        // nothing outside a transfer is hot enough to be worth
                        // the risk.
                        drain(&mut inflight).await;
                        if replies
                            .send(answer(ops, request, &appends).await)
                            .await
                            .is_err()
                        {
                            return None;
                        }
                    }
                }
            }
        }
    }
    drain(&mut inflight).await;
    None
}

async fn drain(inflight: &mut JoinSet<()>) {
    while inflight.join_next().await.is_some() {}
}

// ---------------------------------------------------------------------------
// The transcode
// ---------------------------------------------------------------------------

/// One client request, answered as one reply packet.
async fn answer(ops: &FileOps, request: Request, appends: &Appends) -> Vec<u8> {
    let id = request.id;
    match request.op {
        SftpOp::Init => {
            // Nothing is negotiated: a client that offered 4 or 6 gets 3, and
            // no extension is advertised.
            packet(kind::VERSION, {
                let mut body = Vec::new();
                put_u32(&mut body, VERSION);
                body
            })
        }
        SftpOp::Open { path, pflags, mode } => {
            let flags = OpenFlags {
                read: pflags & PF_READ != 0,
                // `O_APPEND` without `O_WRITE` is still a write, and OpenSSH's
                // client sends exactly that for an append upload.
                write: pflags & (PF_WRITE | PF_APPEND) != 0,
                create: pflags & PF_CREAT != 0,
                truncate: pflags & PF_TRUNC != 0,
                exclusive: pflags & PF_EXCL != 0,
                append: pflags & PF_APPEND != 0,
            };
            let opened = ask(ops, Op::Open { path, flags, mode }, &[]).await;
            if let Ok((Reply::Handle { handle }, _)) = &opened
                && flags.append
            {
                appends.insert(*handle);
            }
            handle_or_status(id, opened)
        }
        SftpOp::Close { handle } => {
            appends.remove(handle);
            ok_or_status(id, ask(ops, Op::Close { handle }, &[]).await)
        }
        SftpOp::Read {
            handle,
            offset,
            len,
        } => read(ops, id, handle, offset, len).await,
        SftpOp::Write {
            handle,
            offset,
            data,
        } => write(ops, id, handle, offset, &data).await,
        SftpOp::Stat { path } => attrs_or_status(id, ask(ops, Op::Stat { path }, &[]).await),
        SftpOp::Lstat { path } => attrs_or_status(id, ask(ops, Op::Lstat { path }, &[]).await),
        SftpOp::Fstat { handle } => attrs_or_status(id, ask(ops, Op::Fstat { handle }, &[]).await),
        SftpOp::Setstat { path, attrs } => {
            ok_or_status(id, ask(ops, Op::Setstat { path, attrs }, &[]).await)
        }
        SftpOp::Fsetstat { handle, attrs } => {
            ok_or_status(id, ask(ops, Op::Fsetstat { handle, attrs }, &[]).await)
        }
        SftpOp::OpenDir { path } => handle_or_status(id, ask(ops, Op::OpenDir { path }, &[]).await),
        SftpOp::ReadDir { handle } => match ask(ops, Op::ReadDir { handle }, &[]).await {
            Ok((Reply::Entries { entries, eof }, _)) => {
                // An empty last slice is how a directory ends: version 3 has
                // no `eof` field, so end-of-listing *is* the status.
                if entries.is_empty() && eof {
                    return status_packet(id, status::EOF, "end of directory");
                }
                packet(kind::NAME, {
                    let mut body = Vec::new();
                    put_u32(&mut body, id);
                    put_u32(&mut body, entries.len() as u32);
                    for entry in &entries {
                        put_str(&mut body, &entry.name);
                        put_str(&mut body, &longname(&entry.name, &entry.attrs));
                        put_attrs(&mut body, &entry.attrs);
                    }
                    body
                })
            }
            Ok((other, _)) => unexpected(id, &other),
            Err(failed) => failed.packet(id),
        },
        SftpOp::Remove { path } => ok_or_status(id, ask(ops, Op::Remove { path }, &[]).await),
        SftpOp::Mkdir { path, mode } => ok_or_status(
            id,
            ask(
                ops,
                Op::Mkdir {
                    path,
                    mode,
                    // The flag §19.5 gives `mkdir` belongs to the workspace
                    // syncer, which asks for it explicitly. SFTP has no way to
                    // spell it, so a client-made directory inherits whatever
                    // the guest's filesystem already does.
                    case_sensitive: false,
                },
                &[],
            )
            .await,
        ),
        SftpOp::Rmdir { path } => ok_or_status(id, ask(ops, Op::Rmdir { path }, &[]).await),
        SftpOp::Rename { from, to } => {
            ok_or_status(id, ask(ops, Op::Rename { from, to }, &[]).await)
        }
        SftpOp::Realpath { path } => name_or_status(id, ask(ops, Op::Realpath { path }, &[]).await),
        SftpOp::Readlink { path } => name_or_status(id, ask(ops, Op::Readlink { path }, &[]).await),
        SftpOp::Symlink { target, link } => {
            // Windows picks a different object for a file link and a directory
            // link and cannot infer it from the target (§19.5) — but SFTP
            // never says which, so the target is stat'd for it. A target that
            // is not there yet is a file link, which is the guess that costs
            // least: a dangling directory link on Windows is the one that
            // cannot be walked.
            let kind = match ops
                .request(
                    Op::Stat {
                        path: target_from(&link, &target),
                    },
                    &[],
                )
                .await
            {
                Ok((Reply::Attrs { attrs }, _)) if attrs.kind == EntryKind::Dir => LinkKind::Dir,
                _ => LinkKind::File,
            };
            ok_or_status(id, ask(ops, Op::Symlink { target, link, kind }, &[]).await)
        }
        // A status *can* carry vmlab's own words, unlike the channel-request
        // refusals §19.3 leaves the client to narrate.
        SftpOp::Unsupported { kind } => status_packet(
            id,
            status::OP_UNSUPPORTED,
            &format!(
                "vmlab's SFTP serves version {VERSION} and no extension (request type {kind})"
            ),
        ),
        SftpOp::Malformed => status_packet(
            id,
            status::BAD_MESSAGE,
            "vmlab could not read that SFTP request",
        ),
    }
}

/// A read, in as many `fileops` reads as the record cap needs.
///
/// A short answer is end-of-file — the guest fills a read or hits the end —
/// so a short slice ends the loop rather than being retried.
async fn read(ops: &FileOps, id: u32, handle: u64, offset: u64, len: u32) -> Vec<u8> {
    let want = len as usize;
    let mut got: Vec<u8> = Vec::new();
    while got.len() < want {
        let chunk = (want - got.len()).min(MAX_DATA) as u32;
        match ask(
            ops,
            Op::Read {
                handle,
                offset: offset + got.len() as u64,
                len: chunk,
            },
            &[],
        )
        .await
        {
            Ok((Reply::Data, bytes)) => {
                let short = bytes.len() < chunk as usize;
                got.extend(bytes);
                if short {
                    break;
                }
            }
            Ok((other, _)) => return unexpected(id, &other),
            Err(failed) => return failed.packet(id),
        }
    }
    if got.is_empty() {
        return status_packet(id, status::EOF, "end of file");
    }
    packet(kind::DATA, {
        let mut body = Vec::new();
        put_u32(&mut body, id);
        put_bytes(&mut body, &got);
        body
    })
}

/// A write, in as many `fileops` writes as the record cap needs. Each one
/// waits for the guest's credit, which is what the SSH window is backed by.
async fn write(ops: &FileOps, id: u32, handle: u64, offset: u64, data: &[u8]) -> Vec<u8> {
    let mut at = offset;
    for chunk in data.chunks(MAX_DATA) {
        match ask(ops, Op::Write { handle, offset: at }, chunk).await {
            Ok((Reply::Ok, _)) => at += chunk.len() as u64,
            Ok((other, _)) => return unexpected(id, &other),
            Err(failed) => return failed.packet(id),
        }
    }
    status_packet(id, status::OK, "")
}

/// One `fileops` call, with its failure already in SFTP's vocabulary.
async fn ask(ops: &FileOps, op: Op, payload: &[u8]) -> Result<(Reply, Vec<u8>), Failed> {
    match ops.request(op, payload).await {
        Ok((Reply::Error { code, msg }, _)) => Err(Failed {
            code: status_of(code),
            msg,
        }),
        Ok(answer) => Ok(answer),
        // The channel itself is gone. The client is told per request; the
        // session ends when its channel does.
        Err(e) => Err(Failed {
            code: status::FAILURE,
            msg: format!("{e:#}"),
        }),
    }
}

/// A failed operation, ready to be a status packet.
struct Failed {
    code: u32,
    msg: String,
}

impl Failed {
    fn packet(&self, id: u32) -> Vec<u8> {
        status_packet(id, self.code, &self.msg)
    }
}

/// Which SFTP status a guest-side failure is.
///
/// Version 3's set is small, so several distinct guest failures land on
/// `FAILURE` — which costs nothing, because the message the client prints is
/// the guest's own and it names the path.
fn status_of(code: ErrorCode) -> u32 {
    match code {
        ErrorCode::NoSuchFile => status::NO_SUCH_FILE,
        ErrorCode::PermissionDenied => status::PERMISSION_DENIED,
        ErrorCode::Unsupported => status::OP_UNSUPPORTED,
        ErrorCode::AlreadyExists
        | ErrorCode::NotADirectory
        | ErrorCode::IsADirectory
        | ErrorCode::NotEmpty
        | ErrorCode::BadHandle
        | ErrorCode::Failure => status::FAILURE,
    }
}

fn ok_or_status(id: u32, answer: Result<(Reply, Vec<u8>), Failed>) -> Vec<u8> {
    match answer {
        Ok((Reply::Ok, _)) => status_packet(id, status::OK, ""),
        Ok((other, _)) => unexpected(id, &other),
        Err(failed) => failed.packet(id),
    }
}

fn handle_or_status(id: u32, answer: Result<(Reply, Vec<u8>), Failed>) -> Vec<u8> {
    match answer {
        Ok((Reply::Handle { handle }, _)) => packet(kind::HANDLE, {
            let mut body = Vec::new();
            put_u32(&mut body, id);
            put_bytes(&mut body, &handle.to_be_bytes());
            body
        }),
        Ok((other, _)) => unexpected(id, &other),
        Err(failed) => failed.packet(id),
    }
}

fn attrs_or_status(id: u32, answer: Result<(Reply, Vec<u8>), Failed>) -> Vec<u8> {
    match answer {
        Ok((Reply::Attrs { attrs }, _)) => packet(kind::ATTRS, {
            let mut body = Vec::new();
            put_u32(&mut body, id);
            put_attrs(&mut body, &attrs);
            body
        }),
        Ok((other, _)) => unexpected(id, &other),
        Err(failed) => failed.packet(id),
    }
}

/// A `realpath` or `readlink` answer: one name, and the attributes a version-3
/// client is told to ignore on it.
fn name_or_status(id: u32, answer: Result<(Reply, Vec<u8>), Failed>) -> Vec<u8> {
    match answer {
        Ok((Reply::Name { path }, _)) => packet(kind::NAME, {
            let mut body = Vec::new();
            put_u32(&mut body, id);
            put_u32(&mut body, 1);
            put_str(&mut body, &path);
            put_str(&mut body, &path);
            put_u32(&mut body, 0); // no attributes
            body
        }),
        Ok((other, _)) => unexpected(id, &other),
        Err(failed) => failed.packet(id),
    }
}

/// Where a symlink's target points, as a path something can be asked about.
///
/// A relative target is relative **to the link's own directory** — not to any
/// cwd the guest happens to have — so the probe that decides the link kind has
/// to join the two before it stats. Only the probe: the target itself crosses
/// to the guest verbatim, because that is what gets written into the link.
///
/// Both separators, and a Windows drive letter, because this runs against
/// either guest family.
fn target_from(link: &str, target: &str) -> String {
    let absolute = target.starts_with(['/', '\\']) || target.as_bytes().get(1) == Some(&b':');
    if absolute {
        return target.to_string();
    }
    match link.rfind(['/', '\\']) {
        Some(cut) => format!("{}{target}", &link[..=cut]),
        None => target.to_string(),
    }
}

/// The guest answered something the operation never asks for. Not a protocol
/// error on the client's side, so it is a status rather than the end of the
/// channel.
fn unexpected(id: u32, reply: &Reply) -> Vec<u8> {
    status_packet(
        id,
        status::FAILURE,
        &format!("vmlab: the guest answered with {reply:?}"),
    )
}

// ---------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------

/// One request off the client's stream.
#[derive(Debug)]
struct Request {
    id: u32,
    op: SftpOp,
}

impl Request {
    /// Whether this is one of the two operations a transfer is made of, and
    /// so the two that run pipelined. Everything else drains them first.
    ///
    /// A write to an appending handle is not one of them: `O_APPEND` puts the
    /// bytes wherever the end happens to be when the write lands, so racing
    /// two of them interleaves the file.
    fn is_bulk(&self, appends: &Appends) -> bool {
        match &self.op {
            SftpOp::Read { .. } => true,
            SftpOp::Write { handle, .. } => !appends.holds(*handle),
            _ => false,
        }
    }
}

/// What the client asked for, in SFTP's own shape — turned into `fileops` by
/// [`answer`] and nowhere else.
#[derive(Debug)]
enum SftpOp {
    /// The version the client offered, already dropped: nothing is negotiated
    /// (see [`answer`]), so there is nothing to carry.
    Init,
    Open {
        path: String,
        pflags: u32,
        mode: Option<u32>,
    },
    Close {
        handle: u64,
    },
    Read {
        handle: u64,
        offset: u64,
        len: u32,
    },
    Write {
        handle: u64,
        offset: u64,
        data: Vec<u8>,
    },
    Stat {
        path: String,
    },
    Lstat {
        path: String,
    },
    Fstat {
        handle: u64,
    },
    Setstat {
        path: String,
        attrs: SetAttrs,
    },
    Fsetstat {
        handle: u64,
        attrs: SetAttrs,
    },
    OpenDir {
        path: String,
    },
    ReadDir {
        handle: u64,
    },
    Remove {
        path: String,
    },
    Mkdir {
        path: String,
        mode: Option<u32>,
    },
    Rmdir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Realpath {
        path: String,
    },
    Readlink {
        path: String,
    },
    Symlink {
        target: String,
        link: String,
    },
    /// A request type nothing in the client set sends, including every
    /// extension: answered by name rather than ignored.
    Unsupported {
        kind: u8,
    },
    /// The type is known and its body is not readable. The packet framing
    /// survives it, so the client is told and the session carries on.
    Malformed,
}

/// Reassembles packets from the channel's byte stream.
#[derive(Default)]
struct PacketDecoder {
    buf: Vec<u8>,
}

impl PacketDecoder {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete packet, or `None` while more bytes are needed. An
    /// error means the stream is unusable — a length nothing could satisfy or
    /// a packet with no room for a type — and there is nothing to resynchronise
    /// against, so the caller ends the session.
    fn next_packet(&mut self) -> Result<Option<Request>, String> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len == 0 {
            return Err("an SFTP packet of no length".into());
        }
        if len > MAX_PACKET {
            return Err(format!(
                "an SFTP packet of {len} bytes exceeds {MAX_PACKET}"
            ));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let packet: Vec<u8> = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        parse(&packet)
    }
}

/// One framed packet as a request. `Err` is a packet with nothing to answer
/// *against* — no type, or no request id to echo — which is unanswerable
/// rather than merely wrong.
fn parse(packet: &[u8]) -> Result<Option<Request>, String> {
    let mut r = Reader::new(packet);
    let Some(kind) = r.u8() else {
        return Err("an SFTP packet with no type".into());
    };
    // `init` is the one packet whose second field is a version rather than a
    // request id, and it is answered against id 0 (which is what a `version`
    // reply carries: nothing).
    if kind == kind::INIT {
        if r.u32().is_none() {
            return Err("an SFTP `init` with no version".into());
        }
        return Ok(Some(Request {
            id: 0,
            op: SftpOp::Init,
        }));
    }
    let Some(id) = r.u32() else {
        return Err(format!("an SFTP request (type {kind}) with no id"));
    };
    let op = read_op(kind, &mut r).unwrap_or(SftpOp::Malformed);
    Ok(Some(Request { id, op }))
}

/// The body of one request. `None` is a body that ran out, which the caller
/// answers `BAD_MESSAGE`.
fn read_op(kind: u8, r: &mut Reader<'_>) -> Option<SftpOp> {
    Some(match kind {
        kind::OPEN => {
            let path = r.text()?;
            let pflags = r.u32()?;
            let attrs = r.attrs()?;
            SftpOp::Open {
                path,
                pflags,
                mode: attrs.mode,
            }
        }
        kind::CLOSE => SftpOp::Close {
            handle: r.handle()?,
        },
        kind::READ => SftpOp::Read {
            handle: r.handle()?,
            offset: r.u64()?,
            len: r.u32()?,
        },
        kind::WRITE => SftpOp::Write {
            handle: r.handle()?,
            offset: r.u64()?,
            data: r.string()?.to_vec(),
        },
        kind::LSTAT => SftpOp::Lstat { path: r.text()? },
        kind::STAT => SftpOp::Stat { path: r.text()? },
        kind::FSTAT => SftpOp::Fstat {
            handle: r.handle()?,
        },
        kind::SETSTAT => SftpOp::Setstat {
            path: r.text()?,
            attrs: r.attrs()?,
        },
        kind::FSETSTAT => SftpOp::Fsetstat {
            handle: r.handle()?,
            attrs: r.attrs()?,
        },
        kind::OPENDIR => SftpOp::OpenDir { path: r.text()? },
        kind::READDIR => SftpOp::ReadDir {
            handle: r.handle()?,
        },
        kind::REMOVE => SftpOp::Remove { path: r.text()? },
        kind::MKDIR => {
            let path = r.text()?;
            let attrs = r.attrs()?;
            SftpOp::Mkdir {
                path,
                mode: attrs.mode,
            }
        }
        kind::RMDIR => SftpOp::Rmdir { path: r.text()? },
        kind::REALPATH => SftpOp::Realpath { path: r.text()? },
        kind::RENAME => SftpOp::Rename {
            from: r.text()?,
            to: r.text()?,
        },
        kind::READLINK => SftpOp::Readlink { path: r.text()? },
        // The one place the draft and every real client disagree: OpenSSH
        // sends the link's *target* first and the link second, and both ends
        // of the world's SFTP traffic do it that way. The client set §19
        // serves is that world.
        kind::SYMLINK => SftpOp::Symlink {
            target: r.text()?,
            link: r.text()?,
        },
        other => SftpOp::Unsupported { kind: other },
    })
}

/// Reads the primitives a packet body is made of.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    /// A path or a name. Lossy, because a guest filesystem's bytes are not
    /// vmlab's to reject: `fileops` speaks paths as strings, so a name that is
    /// not UTF-8 arrives as one the guest will not find rather than killing
    /// the session.
    fn text(&mut self) -> Option<String> {
        Some(String::from_utf8_lossy(self.string()?).into_owned())
    }

    /// A handle, which is the `fileops` handle this facade minted — see
    /// [`handle_or_status`], which is the only place one is ever written.
    fn handle(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.string()?.try_into().ok()?))
    }

    /// A version-3 attribute block. `uid`/`gid` are read past rather than
    /// carried: `fileops` has no ownership to set, and a session is already
    /// one account's view of the filesystem (§19.2).
    fn attrs(&mut self) -> Option<SetAttrs> {
        let flags = self.u32()?;
        let mut attrs = SetAttrs::default();
        if flags & ATTR_SIZE != 0 {
            attrs.size = Some(self.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            self.u32()?;
            self.u32()?;
        }
        if flags & ATTR_PERMISSIONS != 0 {
            attrs.mode = Some(self.u32()? & 0o7777);
        }
        if flags & ATTR_ACMODTIME != 0 {
            attrs.atime_ns = Some(i64::from(self.u32()?) * 1_000_000_000);
            attrs.mtime_ns = Some(i64::from(self.u32()?) * 1_000_000_000);
        }
        if flags & ATTR_EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.string()?;
                self.string()?;
            }
        }
        Some(attrs)
    }
}

fn packet(kind: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    out.extend_from_slice(&((body.len() + 1) as u32).to_be_bytes());
    out.push(kind);
    out.extend_from_slice(&body);
    out
}

fn status_packet(id: u32, code: u32, msg: &str) -> Vec<u8> {
    packet(kind::STATUS, {
        let mut body = Vec::new();
        put_u32(&mut body, id);
        put_u32(&mut body, code);
        put_str(&mut body, msg);
        put_str(&mut body, "en");
        body
    })
}

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

fn put_attrs(out: &mut Vec<u8>, attrs: &Attrs) {
    put_u32(out, ATTR_SIZE | ATTR_PERMISSIONS | ATTR_ACMODTIME);
    put_u64(out, attrs.size);
    put_u32(out, permissions(attrs));
    put_u32(out, seconds(attrs.atime_ns));
    put_u32(out, seconds(attrs.mtime_ns));
}

/// The `permissions` field, with the file type put back.
///
/// A client decides "this is a directory" from these bits — `scp -r` walks on
/// them — and a Windows guest reports no Unix mode at all, so the type comes
/// from [`Attrs::kind`] and the bits fall back to what the entry plainly is.
fn permissions(attrs: &Attrs) -> u32 {
    let kind = match attrs.kind {
        EntryKind::Dir => S_IFDIR,
        EntryKind::Symlink => S_IFLNK,
        EntryKind::File => S_IFREG,
        EntryKind::Other => 0,
    };
    let bits = attrs.mode.unwrap_or(match attrs.kind {
        EntryKind::Dir => 0o755,
        EntryKind::Symlink => 0o777,
        _ => 0o644,
    });
    kind | (bits & 0o7777)
}

/// Whole seconds since the epoch, which is all version 3 carries.
fn seconds(ns: i64) -> u32 {
    (ns / 1_000_000_000).clamp(0, i64::from(u32::MAX)) as u32
}

/// The `ls -l` line a version-3 directory listing carries beside the
/// attributes.
///
/// Owner and group are `-`: `fileops` reports no uid or gid, and inventing a
/// name for the column would be the facade claiming to know something it does
/// not. Every client in §19's set reads the attributes instead and shows this
/// only when a human asked for `ls -l`.
fn longname(name: &str, attrs: &Attrs) -> String {
    let perms = permissions(attrs);
    let kind = match attrs.kind {
        EntryKind::Dir => 'd',
        EntryKind::Symlink => 'l',
        EntryKind::File => '-',
        EntryKind::Other => '?',
    };
    let mut bits = String::from(kind);
    for shift in [6, 3, 0] {
        let triple = (perms >> shift) & 0o7;
        bits.push(if triple & 0o4 != 0 { 'r' } else { '-' });
        bits.push(if triple & 0o2 != 0 { 'w' } else { '-' });
        bits.push(if triple & 0o1 != 0 { 'x' } else { '-' });
    }
    let when = chrono::DateTime::from_timestamp(attrs.mtime_ns / 1_000_000_000, 0)
        .map(|t| t.format("%b %e %H:%M").to_string())
        .unwrap_or_else(|| "            ".into());
    format!(
        "{bits} 1 {:<8} {:<8} {:>10} {when} {name}",
        "-", "-", attrs.size
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(kind: EntryKind, mode: Option<u32>) -> Attrs {
        Attrs {
            kind,
            size: 4096,
            mtime_ns: 1_700_000_000_000_000_000,
            atime_ns: 1_700_000_000_000_000_000,
            mode,
        }
    }

    fn request(kind: u8, body: &[u8]) -> Option<Request> {
        let mut wire = Vec::new();
        wire.extend_from_slice(&((body.len() + 1) as u32).to_be_bytes());
        wire.push(kind);
        wire.extend_from_slice(body);
        let mut dec = PacketDecoder::default();
        dec.push(&wire);
        dec.next_packet().unwrap()
    }

    /// A record arrives in whatever slices the SSH window imposes, and a
    /// packet is only a packet once all of it is there.
    #[test]
    fn packets_reassemble_out_of_arbitrary_chunks() {
        let mut wire = Vec::new();
        for id in 1..=3u32 {
            let mut body = Vec::new();
            put_u32(&mut body, id);
            put_str(&mut body, "/srv/app.conf");
            wire.extend(packet(kind::STAT, body));
        }
        for chunk in [1usize, 5, 64, 4096] {
            let mut dec = PacketDecoder::default();
            let mut ids = Vec::new();
            for part in wire.chunks(chunk) {
                dec.push(part);
                while let Some(request) = dec.next_packet().unwrap() {
                    match request.op {
                        SftpOp::Stat { path } => assert_eq!(path, "/srv/app.conf"),
                        _ => panic!("not a stat"),
                    }
                    ids.push(request.id);
                }
            }
            assert_eq!(ids, vec![1, 2, 3], "chunk {chunk}");
        }
    }

    /// The framing bound is a decoder-buffer bound: past it the stream is
    /// desynchronised, and there is no resynchronisation point to look for.
    #[test]
    fn an_oversized_packet_ends_the_session() {
        let mut dec = PacketDecoder::default();
        dec.push(&((MAX_PACKET + 1) as u32).to_be_bytes());
        assert!(dec.next_packet().unwrap_err().contains("exceeds"));
    }

    /// A body that runs out is answered, not fatal: framing survived it, so
    /// the client gets `BAD_MESSAGE` against its own request id and keeps its
    /// session.
    #[test]
    fn a_truncated_body_is_answered_against_its_id() {
        let mut body = Vec::new();
        put_u32(&mut body, 77);
        put_u32(&mut body, 64); // a name 64 bytes long, and none of it
        let request = request(kind::STAT, &body).unwrap();
        assert_eq!(request.id, 77);
        assert!(matches!(request.op, SftpOp::Malformed));
    }

    /// `open` carries version 3's `pflags` and an attribute block, and the
    /// permission bits in it are the `mode` §19.5 moved onto `open`.
    #[test]
    fn an_open_carries_its_flags_and_its_mode() {
        let mut body = Vec::new();
        put_u32(&mut body, 1);
        put_str(&mut body, "/srv/app.conf");
        put_u32(&mut body, PF_WRITE | PF_CREAT | PF_TRUNC);
        put_u32(&mut body, ATTR_PERMISSIONS);
        put_u32(&mut body, 0o100_640); // the type bits ride along, and are dropped
        match request(kind::OPEN, &body).unwrap().op {
            SftpOp::Open { path, pflags, mode } => {
                assert_eq!(path, "/srv/app.conf");
                assert_eq!(pflags, PF_WRITE | PF_CREAT | PF_TRUNC);
                assert_eq!(mode, Some(0o640));
            }
            _ => panic!("not an open"),
        }
    }

    /// The attribute block a client sends: every field optional, `uid`/`gid`
    /// read past, and version 3's whole seconds widened to the nanoseconds
    /// `fileops` carries.
    #[test]
    fn a_setstat_reads_every_optional_field() {
        let mut body = Vec::new();
        put_u32(&mut body, 9);
        put_str(&mut body, "/srv/app.conf");
        put_u32(
            &mut body,
            ATTR_SIZE | ATTR_UIDGID | ATTR_PERMISSIONS | ATTR_ACMODTIME,
        );
        put_u64(&mut body, 1024);
        put_u32(&mut body, 1000); // uid
        put_u32(&mut body, 1000); // gid
        put_u32(&mut body, 0o644);
        put_u32(&mut body, 1_700_000_000); // atime
        put_u32(&mut body, 1_700_000_001); // mtime
        match request(kind::SETSTAT, &body).unwrap().op {
            SftpOp::Setstat { attrs, .. } => {
                assert_eq!(attrs.size, Some(1024));
                assert_eq!(attrs.mode, Some(0o644));
                assert_eq!(attrs.atime_ns, Some(1_700_000_000_000_000_000));
                assert_eq!(attrs.mtime_ns, Some(1_700_000_001_000_000_000));
            }
            _ => panic!("not a setstat"),
        }
    }

    /// OpenSSH sends `symlink` with the target first and the link second —
    /// the reverse of the draft, and what every real client does.
    #[test]
    fn a_symlink_reads_the_target_before_the_link() {
        let mut body = Vec::new();
        put_u32(&mut body, 3);
        put_str(&mut body, "/srv/pkg");
        put_str(&mut body, "/srv/link");
        match request(kind::SYMLINK, &body).unwrap().op {
            SftpOp::Symlink { target, link } => {
                assert_eq!(target, "/srv/pkg");
                assert_eq!(link, "/srv/link");
            }
            _ => panic!("not a symlink"),
        }
    }

    /// Reads and writes are the two that pipeline; everything else drains
    /// them first, which is what keeps a `close` behind its writes.
    ///
    /// A write to an appending handle is the exception: `O_APPEND` ignores the
    /// offset, so two of them racing would interleave the file.
    #[test]
    fn reads_and_writes_pipeline_except_onto_an_appending_handle() {
        let appends = Appends::default();
        appends.insert(9);
        let bulk = |op| Request { id: 1, op }.is_bulk(&appends);
        assert!(bulk(SftpOp::Read {
            handle: 1,
            offset: 0,
            len: 32768
        }));
        assert!(bulk(SftpOp::Write {
            handle: 1,
            offset: 0,
            data: vec![]
        }));
        assert!(!bulk(SftpOp::Write {
            handle: 9,
            offset: 0,
            data: vec![]
        }));
        assert!(!bulk(SftpOp::Close { handle: 1 }));
        assert!(!bulk(SftpOp::Stat { path: "/x".into() }));
        // And a closed handle stops being one, so its id can be reused.
        appends.remove(9);
        assert!(bulk(SftpOp::Write {
            handle: 9,
            offset: 0,
            data: vec![]
        }));
    }

    /// A symlink's target is relative to the **link's** directory, which is
    /// the base the kind probe has to stat against — Windows picks a different
    /// object for a file link and a directory link, and getting it from the
    /// wrong base makes every relative directory link an unwalkable file link.
    #[test]
    fn a_relative_symlink_target_resolves_against_the_link() {
        assert_eq!(target_from("/srv/app/link", "../lib"), "/srv/app/../lib");
        assert_eq!(
            target_from(r"C:\src\app\link", "..\\lib"),
            r"C:\src\app\..\lib"
        );
        // An absolute target is already the answer, on either family.
        assert_eq!(target_from("/srv/app/link", "/usr/lib"), "/usr/lib");
        assert_eq!(target_from(r"C:\src\link", r"D:\lib"), r"D:\lib");
        // And a bare name has no directory to resolve against.
        assert_eq!(target_from("link", "lib"), "lib");
    }

    /// The file type a client branches on comes from the guest's `kind`, not
    /// from a mode a Windows guest never reports — otherwise `scp -r` would
    /// walk a directory as a file.
    #[test]
    fn a_windows_entry_still_says_what_it_is() {
        assert_eq!(permissions(&attrs(EntryKind::Dir, None)), 0o040_755);
        assert_eq!(permissions(&attrs(EntryKind::File, None)), 0o100_644);
        assert_eq!(permissions(&attrs(EntryKind::Symlink, None)), 0o120_777);
        // And a Linux guest's own bits survive, with the type put back.
        assert_eq!(permissions(&attrs(EntryKind::File, Some(0o600))), 0o100_600);
        assert_eq!(permissions(&attrs(EntryKind::Dir, Some(0o700))), 0o040_700);
    }

    /// The `ls -l` line names the kind and the mode; ownership is `-`,
    /// because `fileops` reports none and the facade will not invent one.
    #[test]
    fn a_longname_reads_like_ls() {
        let line = longname("main.rs", &attrs(EntryKind::File, Some(0o644)));
        assert!(
            line.starts_with("-rw-r--r-- 1 -        -        "),
            "{line}"
        );
        assert!(line.contains(" 4096 "), "{line}");
        assert!(line.ends_with(" main.rs"), "{line}");
        let dir = longname("src", &attrs(EntryKind::Dir, Some(0o755)));
        assert!(dir.starts_with("drwxr-xr-x "), "{dir}");
    }

    /// Every guest failure reaches the client as a status it can branch on —
    /// "not there" and "not allowed" are the two `scp` reports differently.
    #[test]
    fn guest_failures_transcode_onto_version_3_statuses() {
        assert_eq!(status_of(ErrorCode::NoSuchFile), status::NO_SUCH_FILE);
        assert_eq!(
            status_of(ErrorCode::PermissionDenied),
            status::PERMISSION_DENIED
        );
        assert_eq!(status_of(ErrorCode::Unsupported), status::OP_UNSUPPORTED);
        assert_eq!(status_of(ErrorCode::NotEmpty), status::FAILURE);
        assert_eq!(status_of(ErrorCode::BadHandle), status::FAILURE);
    }
}
