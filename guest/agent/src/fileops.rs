//! The guest half of the `fileops` RPC session (PRD §19.5).
//!
//! One channel, many requests in flight. The reader thread does nothing but
//! frame records off the channel and hand them to a small pool of workers, so
//! a slow request — a digest over a gigabyte, a read off a cold disk — never
//! stands in front of the ones behind it. Replies carry the request's own id
//! and go out in whatever order they finish, which is what the pipelining is
//! for: against the round trip the SSH facade pays, serialising would cost it
//! two orders of magnitude of throughput.
//!
//! **Handles are scoped to the channel and die with it.** They live in this
//! session's table; a host `close` (or a dropped connection) drops the table
//! and with it every open file and directory, so a guest cannot be left
//! holding handles for a host that has forgotten them.
//!
//! Identity rides the open, not the request (§19.2): a session is one
//! account's view of the filesystem for its whole life, and each worker
//! adopts that identity for the life of its thread — the same shape `tail`
//! uses, and for the same reason. Minting failures fail the *open*, loudly.
//!
//! An operation that fails answers [`Reply::Error`] and the session carries
//! on. Only a record the decoder cannot frame kills the channel: there is no
//! resynchronisation point inside one.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use vmlab_agent_proto::fileops::{
    Attrs, DirEntry, ErrorCode, LinkKind, MAX_DATA, Op, OpenFlags, READDIR_CHUNK, RecordDecoder,
    Reply, Request, Response, SetAttrs, encode_record,
};
use vmlab_agent_proto::watch::EntryKind;
use vmlab_agent_proto::{AgentMsg, FrameKind, RecvWindow};

use crate::mux::{Credit, Input, Mux, PathResolver};
use crate::spawn::{Adopter, Identity, Spawner};

/// How many requests are served at once. Enough that a slow operation does
/// not stall the ones queued behind it; small enough that a host pipelining
/// hard does not turn into an unbounded thread count.
const WORKERS: usize = 4;

/// One request waiting for a worker, with a write's bytes where it has them.
type Work = (Request, Vec<u8>);

/// The workers' shared end of the queue: `Receiver` is not clonable, so the
/// pool takes turns on one.
type WorkQueue = Arc<Mutex<Receiver<Work>>>;

/// How many decoded requests may queue for the workers. The bound is the
/// back-pressure: the reader grants receive window only once a request is
/// enqueued, so a host that pipelines past what the guest can serve is
/// throttled by the credit window rather than by an unbounded queue.
const WORK_QUEUE: usize = 64;

/// Open a file session on channel `id`, serving `identity`'s view of the
/// filesystem. `resolve` maps host-supplied guest paths (a container
/// micro-VM resolves them inside its rootfs).
pub fn open(mux: &Mux, spawner: &dyn Spawner, identity: &Identity, id: u32, resolve: PathResolver) {
    // Minting can fail — a missing account, a wrong secret — and §19.2 says
    // that is loud rather than a silent fall back to the agent identity.
    let adopter = match spawner.adopter(identity) {
        Ok(a) => a,
        Err(e) => {
            mux.send_error(Some(id), format!("fileops: {e}"));
            return;
        }
    };
    let Some((input, credit)) = mux.register(id, None, None) else {
        return;
    };
    let session = Arc::new(Session {
        mux: mux.clone(),
        id,
        credit,
        wire: Mutex::new(()),
        handles: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(1),
        resolve,
    });
    mux.send_ctrl(&AgentMsg::Opened { id });

    let (tx, rx) = sync_channel::<Work>(WORK_QUEUE);
    let rx = Arc::new(Mutex::new(rx));
    let adopter = Arc::new(adopter);
    for _ in 0..WORKERS {
        let session = session.clone();
        let rx = rx.clone();
        let adopter = adopter.clone();
        thread::spawn(move || worker(session, rx, adopter));
    }
    thread::spawn(move || read_loop(session, input, tx));
}

/// Frame records off the channel and hand them to the workers. The one thing
/// this thread must not do is serve a request: a read that blocks here would
/// stop the channel being read at all, and with it every reply's credit.
fn read_loop(session: Arc<Session>, input: Receiver<Input>, tx: SyncSender<Work>) {
    let mut decoder = RecordDecoder::new();
    let mut window = RecvWindow::default();
    for msg in input {
        let Input::Bytes(bytes) = msg else { continue };
        let consumed = bytes.len();
        decoder.push(&bytes);
        loop {
            match decoder.next_record::<Request>() {
                Ok(Some(work)) => {
                    if tx.send(work).is_err() {
                        return; // workers gone: the session is over
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    session.mux.send_error(Some(session.id), e);
                    session.mux.remove_finished(session.id);
                    return;
                }
            }
        }
        // Granted only now: a full work queue is what throttles a host that
        // pipelines past what this guest can serve.
        if let Some(grant) = window.recv(consumed) {
            session.mux.send_ctrl(&AgentMsg::WindowAdjust {
                id: session.id,
                bytes: grant,
            });
        }
    }
    // The host closed the channel (or the mux tore it down): drop every
    // handle this session held.
    session.handles.lock().expect("handles lock").clear();
}

/// Serve requests until the queue closes, as the session's identity.
fn worker(session: Arc<Session>, rx: WorkQueue, adopter: Arc<Adopter>) {
    // Adopted for the thread's life rather than per request: every open this
    // thread makes is then that account's (see [`crate::spawn::Adopted`]).
    let _adopted = match adopter() {
        Ok(a) => a,
        Err(e) => {
            session
                .mux
                .send_error(Some(session.id), format!("fileops: {e}"));
            session.mux.remove_finished(session.id);
            return;
        }
    };
    loop {
        let work = { rx.lock().expect("work queue lock").recv() };
        let Ok((request, payload)) = work else { return };
        let (reply, bytes) = session.serve(request.op, payload);
        session.reply(request.id, reply, &bytes);
    }
}

/// One live file session: its handle table and its half of the wire.
struct Session {
    mux: Mux,
    id: u32,
    credit: Arc<Credit>,
    /// Held for the length of one record, so two workers replying at once
    /// cannot interleave their bytes inside the channel's byte stream.
    wire: Mutex<()>,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_handle: AtomicU64,
    resolve: PathResolver,
}

/// What a handle refers to. A file is addressed by offset on every read and
/// write, so workers share it without a lock; a directory is a cursor, so it
/// takes one.
enum Handle {
    File {
        file: File,
        append: bool,
    },
    /// Boxed: a `ReadDir` is hundreds of bytes on Windows and every file
    /// handle would otherwise carry that weight.
    Dir(Box<Mutex<DirCursor>>),
}

struct DirCursor {
    iter: std::fs::ReadDir,
    done: bool,
}

impl Session {
    /// Write one reply to the channel, chunked into the credit window. The
    /// record is atomic on the wire; a closed channel drops it.
    fn reply(&self, id: u64, reply: Reply, payload: &[u8]) {
        let bytes = encode_record(&Response { id, reply }, payload);
        let _wire = self.wire.lock().expect("wire lock");
        let mut off = 0;
        while off < bytes.len() {
            let take = self.credit.take(bytes.len() - off);
            if take == 0 {
                return; // channel closed under us
            }
            self.mux
                .send_data(FrameKind::Data, self.id, &bytes[off..off + take]);
            off += take;
        }
    }

    fn path(&self, path: &str) -> String {
        (self.resolve)(path.to_string())
    }

    fn take_handle(&self, handle: u64) -> Option<Arc<Handle>> {
        self.handles.lock().expect("handles lock").remove(&handle)
    }

    fn get_handle(&self, handle: u64) -> Option<Arc<Handle>> {
        self.handles
            .lock()
            .expect("handles lock")
            .get(&handle)
            .cloned()
    }

    fn insert_handle(&self, handle: Handle) -> u64 {
        let id = self.next_handle.fetch_add(1, Ordering::SeqCst);
        self.handles
            .lock()
            .expect("handles lock")
            .insert(id, Arc::new(handle));
        id
    }

    /// Serve one operation. The payload is a write's bytes; the returned
    /// bytes are a read's.
    fn serve(&self, op: Op, payload: Vec<u8>) -> (Reply, Vec<u8>) {
        match self.try_serve(op, payload) {
            Ok(answer) => answer,
            Err(e) => (
                Reply::Error {
                    code: e.code,
                    msg: e.msg,
                },
                Vec::new(),
            ),
        }
    }

    fn try_serve(&self, op: Op, payload: Vec<u8>) -> Result<(Reply, Vec<u8>), OpError> {
        let ok = |reply| Ok((reply, Vec::new()));
        match op {
            Op::Open { path, flags, mode } => {
                let path = self.path(&path);
                let file = open_file(&path, flags, mode).context(&path)?;
                ok(Reply::Handle {
                    handle: self.insert_handle(Handle::File {
                        file,
                        append: flags.append,
                    }),
                })
            }
            Op::Close { handle } => {
                self.take_handle(handle).ok_or_else(bad_handle)?;
                ok(Reply::Ok)
            }
            Op::Read {
                handle,
                offset,
                len,
            } => {
                let entry = self.get_handle(handle).ok_or_else(bad_handle)?;
                let Handle::File { file, .. } = entry.as_ref() else {
                    return Err(bad_handle());
                };
                // Refused rather than quietly clamped: a short answer means
                // end-of-file, so silently returning less than was asked for
                // would read as a truncated file.
                if len as usize > MAX_DATA {
                    return Err(OpError {
                        code: ErrorCode::Failure,
                        msg: format!("read of {len} bytes exceeds the {MAX_DATA}-byte record cap"),
                    });
                }
                let mut buf = vec![0u8; len as usize];
                let n = read_at(file, offset, &mut buf)?;
                buf.truncate(n);
                Ok((Reply::Data, buf))
            }
            Op::Write { handle, offset } => {
                let entry = self.get_handle(handle).ok_or_else(bad_handle)?;
                let Handle::File { file, append } = entry.as_ref() else {
                    return Err(bad_handle());
                };
                write_at(file, offset, &payload, *append)?;
                ok(Reply::Ok)
            }
            Op::Fstat { handle } => {
                let entry = self.get_handle(handle).ok_or_else(bad_handle)?;
                let Handle::File { file, .. } = entry.as_ref() else {
                    return Err(bad_handle());
                };
                ok(Reply::Attrs {
                    attrs: attrs_of(&file.metadata()?),
                })
            }
            Op::Fsetstat { handle, attrs } => {
                let entry = self.get_handle(handle).ok_or_else(bad_handle)?;
                let Handle::File { file, .. } = entry.as_ref() else {
                    return Err(bad_handle());
                };
                if let Some(size) = attrs.size {
                    file.set_len(size)?;
                }
                apply_attrs_to_file(file, &attrs)?;
                ok(Reply::Ok)
            }
            Op::Stat { path } => {
                let path = self.path(&path);
                ok(Reply::Attrs {
                    attrs: attrs_of(&std::fs::metadata(&path).context(&path)?),
                })
            }
            Op::Lstat { path } => {
                let path = self.path(&path);
                ok(Reply::Attrs {
                    attrs: attrs_of(&std::fs::symlink_metadata(&path).context(&path)?),
                })
            }
            // Path-addressed, and it opens the file only for what needs a
            // handle: a mode-only setstat must not demand write access to
            // something the caller only owns.
            Op::Setstat { path, attrs } => {
                let path = self.path(&path);
                if let Some(mode) = attrs.mode {
                    set_mode(&path, mode).context(&path)?;
                }
                if attrs.size.is_some() || attrs.mtime_ns.is_some() || attrs.atime_ns.is_some() {
                    let file = OpenOptions::new().write(true).open(&path).context(&path)?;
                    if let Some(size) = attrs.size {
                        file.set_len(size).context(&path)?;
                    }
                    if attrs.mtime_ns.is_some() || attrs.atime_ns.is_some() {
                        set_times(&file, &attrs).context(&path)?;
                    }
                }
                ok(Reply::Ok)
            }
            Op::OpenDir { path } => {
                let path = self.path(&path);
                let iter = std::fs::read_dir(&path).context(&path)?;
                ok(Reply::Handle {
                    handle: self.insert_handle(Handle::Dir(Box::new(Mutex::new(DirCursor {
                        iter,
                        done: false,
                    })))),
                })
            }
            Op::ReadDir { handle } => {
                let entry = self.get_handle(handle).ok_or_else(bad_handle)?;
                let Handle::Dir(cursor) = entry.as_ref() else {
                    return Err(bad_handle());
                };
                let mut cursor = cursor.lock().expect("dir cursor lock");
                let mut entries = Vec::new();
                while entries.len() < READDIR_CHUNK {
                    match cursor.iter.next() {
                        None => {
                            cursor.done = true;
                            break;
                        }
                        // An entry that vanished between the listing and the
                        // stat is skipped, not fatal: a directory read is a
                        // snapshot of something that keeps moving.
                        Some(Err(_)) => continue,
                        Some(Ok(entry)) => {
                            let Ok(meta) = entry.metadata() else { continue };
                            entries.push(DirEntry {
                                name: entry.file_name().to_string_lossy().into_owned(),
                                attrs: attrs_of(&meta),
                            });
                        }
                    }
                }
                ok(Reply::Entries {
                    entries,
                    eof: cursor.done,
                })
            }
            Op::Mkdir {
                path,
                mode,
                case_sensitive,
            } => {
                let path = self.path(&path);
                std::fs::create_dir(&path).context(&path)?;
                if let Some(mode) = mode {
                    set_mode(&path, mode)?;
                }
                // While the directory is still empty — the only window NTFS
                // accepts the flag in, which is why it rides the creation.
                if case_sensitive {
                    set_case_sensitive(&path).context(&path)?;
                }
                ok(Reply::Ok)
            }
            Op::Rmdir { path } => {
                let path = self.path(&path);
                std::fs::remove_dir(&path).context(&path)?;
                ok(Reply::Ok)
            }
            Op::Remove { path } => {
                let path = self.path(&path);
                std::fs::remove_file(&path).context(&path)?;
                ok(Reply::Ok)
            }
            Op::Rename { from, to } => {
                let (from, to) = (self.path(&from), self.path(&to));
                std::fs::rename(&from, &to).context(&from)?;
                ok(Reply::Ok)
            }
            Op::Realpath { path } => {
                let path = self.path(&path);
                ok(Reply::Name {
                    path: realpath(&path),
                })
            }
            Op::Symlink { target, link, kind } => {
                let link = self.path(&link);
                symlink(&target, &link, kind).context(&link)?;
                ok(Reply::Ok)
            }
            Op::Readlink { path } => {
                let path = self.path(&path);
                ok(Reply::Name {
                    path: std::fs::read_link(&path)
                        .context(&path)?
                        .to_string_lossy()
                        .into_owned(),
                })
            }
            // The guest's own digest of what is on its disk: the strongest
            // thing the whole-file transfer offered, kept when it retired.
            Op::Digest { path } => {
                let path = self.path(&path);
                let (sha256, len) = digest_file(&path).context(&path)?;
                ok(Reply::Digest { sha256, len })
            }
        }
    }
}

/// A failed operation, in the vocabulary the reply carries.
struct OpError {
    code: ErrorCode,
    msg: String,
}

impl From<std::io::Error> for OpError {
    fn from(e: std::io::Error) -> OpError {
        OpError {
            code: ErrorCode::of(&e),
            msg: e.to_string(),
        }
    }
}

fn bad_handle() -> OpError {
    OpError {
        code: ErrorCode::BadHandle,
        msg: "no such handle on this channel".into(),
    }
}

/// Name the path in the failure. A client pipelining 64 requests cannot tell
/// from the id alone which file "permission denied" was about.
trait Context<T> {
    fn context(self, path: &str) -> Result<T, OpError>;
}

impl<T> Context<T> for std::io::Result<T> {
    fn context(self, path: &str) -> Result<T, OpError> {
        self.map_err(|e| OpError {
            code: ErrorCode::of(&e),
            msg: format!("{path}: {e}"),
        })
    }
}

fn open_file(path: &str, flags: OpenFlags, mode: Option<u32>) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    // An open that names no access at all is a read, which is what a client
    // that sent no flags meant.
    opts.read(flags.read || !(flags.write || flags.append));
    opts.write(flags.write);
    opts.append(flags.append);
    opts.truncate(flags.truncate);
    opts.create(flags.create && !flags.exclusive);
    opts.create_new(flags.exclusive);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    let file = opts.open(path)?;
    // `OpenOptionsExt::mode` only takes when the open actually created the
    // file, and a push over one that already exists still means the mode it
    // asked for. Windows has no Unix bits and ignores this entirely — the
    // rule the retired whole-file `mode` carried, kept verbatim.
    if let Some(mode) = mode
        && (flags.create || flags.exclusive)
    {
        set_mode_of(&file, mode)?;
    }
    Ok(file)
}

#[cfg(unix)]
fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64)? {
            0 => break,
            n => done += n,
        }
    }
    Ok(done)
}

#[cfg(windows)]
fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        match file.seek_read(&mut buf[done..], offset + done as u64)? {
            0 => break,
            n => done += n,
        }
    }
    Ok(done)
}

fn write_at(file: &File, offset: u64, buf: &[u8], append: bool) -> std::io::Result<()> {
    if append {
        // `O_APPEND` means the offset is not the client's to choose.
        use std::io::Write;
        let mut sink: &File = file;
        return sink.write_all(buf);
    }
    let mut done = 0;
    while done < buf.len() {
        let n = write_some(file, offset + done as u64, &buf[done..])?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        done += n;
    }
    Ok(())
}

#[cfg(unix)]
fn write_some(file: &File, offset: u64, buf: &[u8]) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(windows)]
fn write_some(file: &File, offset: u64, buf: &[u8]) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

fn attrs_of(meta: &std::fs::Metadata) -> Attrs {
    let kind = if meta.is_dir() {
        EntryKind::Dir
    } else if meta.is_symlink() {
        EntryKind::Symlink
    } else if meta.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    Attrs {
        kind,
        size: meta.len(),
        mtime_ns: meta.modified().map(nanos).unwrap_or(0),
        atime_ns: meta.accessed().map(nanos).unwrap_or(0),
        mode: mode_of(meta),
    }
}

fn nanos(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Apply the settable attributes. `size` is the caller's to apply first (it
/// needs the file, not the path).
fn apply_attrs_to_file(file: &File, attrs: &SetAttrs) -> std::io::Result<()> {
    if let Some(mode) = attrs.mode {
        set_mode_of(file, mode)?;
    }
    if attrs.mtime_ns.is_some() || attrs.atime_ns.is_some() {
        set_times(file, attrs)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode_of(file: &File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

/// Unix permission bits, ignored on Windows — the rule the retired
/// whole-file `mode` carried, kept verbatim.
#[cfg(not(unix))]
fn set_mode_of(_file: &File, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &str, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &str, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

fn set_times(file: &File, attrs: &SetAttrs) -> std::io::Result<()> {
    let mut times = std::fs::FileTimes::new();
    if let Some(ns) = attrs.mtime_ns {
        times = times.set_modified(from_nanos(ns));
    }
    if let Some(ns) = attrs.atime_ns {
        times = times.set_accessed(from_nanos(ns));
    }
    file.set_times(times)
}

fn from_nanos(ns: i64) -> SystemTime {
    let d = std::time::Duration::from_nanos(ns.unsigned_abs());
    if ns < 0 {
        UNIX_EPOCH - d
    } else {
        UNIX_EPOCH + d
    }
}

/// Ask the filesystem for a case-sensitive directory.
///
/// On a Linux guest every directory already is one, so the request is
/// satisfied by construction. On Windows it is NTFS's per-directory flag,
/// which only takes while the directory is empty — the whole reason it rides
/// [`Op::Mkdir`] rather than a later `setstat`.
#[cfg(not(windows))]
fn set_case_sensitive(_path: &str) -> std::io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn set_case_sensitive(path: &str) -> std::io::Result<()> {
    let out = std::process::Command::new("fsutil.exe")
        .args(["file", "setCaseSensitiveInfo", path, "enable"])
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    // A filesystem with no concept of the flag, or a Windows build without
    // WSL's optional component, refuses it — say so rather than reporting a
    // case-sensitive directory that is not one.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "this guest cannot make a directory case-sensitive: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    ))
}

#[cfg(unix)]
fn symlink(target: &str, link: &str, _kind: LinkKind) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows picks a different object for a file link and a directory link and
/// cannot infer which from a target that is not there yet — so the kind rides
/// the request (§19.5).
#[cfg(windows)]
fn symlink(target: &str, link: &str, kind: LinkKind) -> std::io::Result<()> {
    match kind {
        LinkKind::File => std::os::windows::fs::symlink_file(target, link),
        LinkKind::Dir => std::os::windows::fs::symlink_dir(target, link),
    }
}

/// Canonicalise where the path exists, and fall back to what the client can
/// still use where it does not — a client asks `realpath` about files it is
/// about to create, and an error there would break the very first thing an
/// SFTP session does.
fn realpath(path: &str) -> String {
    if let Ok(real) = std::fs::canonicalize(path) {
        let real = real.to_string_lossy().into_owned();
        // Windows canonicalises to the `\\?\` verbatim form, which most tools
        // cannot take back.
        return real
            .strip_prefix(r"\\?\")
            .map(str::to_string)
            .unwrap_or(real);
    }
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p).to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
}

fn digest_file(path: &str) -> std::io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut len = 0u64;
    loop {
        match file.read(&mut buf)? {
            0 => break,
            n => {
                hasher.update(&buf[..n]);
                len += n as u64;
            }
        }
    }
    Ok((hex(&hasher.finalize()), len))
}

pub fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
