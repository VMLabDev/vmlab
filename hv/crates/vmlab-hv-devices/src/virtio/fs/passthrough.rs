//! A passthrough FUSE filesystem rooted at a host directory.
//!
//! Safety model: every inode carries an `O_PATH` descriptor for its file and
//! the path it was reached by, one component at a time. [`FileSystem::lookup`]
//! opens exactly one more component with `O_NOFOLLOW` under the parent's
//! descriptor, so a guest can never make the server traverse a path it did
//! not vet name by name: `..`, absolute paths, multi-component names and
//! symlink traversal cannot escape the root. Symlinks are served as their
//! own inodes; the guest resolves their targets inside its own tree.
//!
//! Read-only shares refuse every mutating operation with `EROFS`.
//!
//! The session state — open inodes with their paths and lookup counts, open
//! file and directory handles — serialises into the snapshot format
//! ([`PassthroughFs::save_state`]) and restores by re-walking each recorded
//! path from the root, the way virtiofsd's `find-paths` migration mode does.
//! A path that no longer exists restores as a dead inode answering `ENOENT`.
//!
//! Inode numbers are the server's own (`st_ino` is mapped onto them), so the
//! host may renumber its files under us without confusing the guest.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;

use crate::virtio::fs::fuse::abi::fuse_abi::{CreateIn, FsOptions, ROOT_ID, stat64};
use crate::virtio::fs::fuse::filesystem::{
    Context, DirEntry, Entry, FileSystem, OpenOptions, SetattrValid, ZeroCopyReader, ZeroCopyWriter,
};

/// How long the guest may cache attributes and entries before revalidating.
/// Short, like virtiofsd's defaults, because the host tree can change under
/// the guest.
const TIMEOUT: Duration = Duration::from_secs(1);

/// Snapshot format magic: "IPFS" (in-process fs).
const MAGIC: u32 = 0x5346_5049;

// Bound guest-held descriptors and bookkeeping per share. Tests exercise
// the same admission paths with a smaller budget.
#[cfg(not(test))]
const SESSION_LIMIT: usize = 4096;
#[cfg(test)]
const SESSION_LIMIT: usize = 32;
const INITIAL_REQUEST_LIMIT: usize = super::fuse::server::MAX_BUFFER_SIZE as usize + 4096;

/// A passthrough filesystem serving `root` on the host. Clones share one
/// session, so the device and its FUSE server observe the same inodes.
#[derive(Clone)]
pub struct PassthroughFs {
    root: PathBuf,
    read_only: bool,
    inner: std::sync::Arc<Mutex<Session>>,
    request_limit: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// Everything the guest has made the server remember.
struct Session {
    writeback_cache: bool,
    /// `O_PATH` descriptors by inode; the root holds the share directory.
    inodes: HashMap<u64, Inode>,
    /// Live path → inode, so repeat lookups of one file share one guest
    /// inode and one attribute cache.
    by_path: HashMap<PathBuf, u64>,
    /// Open files and directories by handle.
    handles: HashMap<u64, Handle>,
    /// Open handle counts by inode, so FORGET is constant-time.
    handles_by_inode: HashMap<u64, usize>,
    next_ino: u64,
    next_handle: u64,
}

/// One guest-known file. `fd` is an `O_PATH` handle on the inode; `path` is
/// its location under the root at lookup time, moved along by every guest
/// rename (the snapshot form). The parent's `O_PATH` descriptor and the own
/// name serve the `*at` syscalls, which `O_PATH` descriptors alone cannot
/// express (fchmod, readlink, truncate...).
struct Inode {
    fd: OwnedFd,
    parent: OwnedFd,
    name: Vec<u8>,
    path: PathBuf,
    kind: FileKind,
    /// LOOKUP and READDIRPLUS references not yet cancelled by FORGET.
    lookups: u64,
    /// Set when a restored path no longer exists; every access answers
    /// `ENOENT` until the guest forgets the inode.
    dead: bool,
}

/// An open file or directory.
struct Handle {
    ino: u64,
    kind: HandleKind,
}

enum HandleKind {
    File {
        // A restored handle keeps its ID even when its file cannot be reopened.
        file: Option<File>,
        flags: i32,
        /// Path under the root at open time, for the snapshot.
        path: PathBuf,
    },
    Dir {
        /// The directory stream, opened by the first READDIR (and again
        /// after a restore). The handle holds no entries: every request
        /// reads its own from the host, so a handle costs one descriptor
        /// whatever the size of the directory.
        dir: Option<File>,
        /// The cookie the stream stands at, when that is known. A request
        /// at another offset seeks first.
        position: Option<u64>,
    },
}

/// One directory entry as getdents64 served it.
struct HostDirent {
    name: Vec<u8>,
    host_ino: u64,
    d_type: u8,
    /// The host's `d_off`: the cookie that resumes after this entry.
    cookie: u64,
}

/// The most one getdents64 call reads for a request.
const DIR_BATCH_MAX: usize = 64 << 10;
/// The least: room for any one record (19 bytes and a 255-byte name).
const DIR_BATCH_MIN: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl PassthroughFs {
    /// Open the share rooted at `root`, which must be an existing
    /// directory. `read_only` refuses all mutating operations.
    pub fn new(root: PathBuf, read_only: bool) -> io::Result<Self> {
        let fd = open_path(&root, libc::O_PATH | libc::O_DIRECTORY)?;
        Ok(Self {
            root: root.canonicalize().unwrap_or(root),
            read_only,
            request_limit: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                INITIAL_REQUEST_LIMIT,
            )),
            inner: std::sync::Arc::new(Mutex::new(Session {
                writeback_cache: false,
                inodes: HashMap::from([(
                    ROOT_ID,
                    Inode {
                        parent: fd
                            .try_clone()
                            .expect("the share root clones its own descriptor"),
                        fd,
                        name: Vec::new(),
                        path: PathBuf::new(),
                        kind: FileKind::Dir,
                        lookups: 1,
                        dead: false,
                    },
                )]),
                by_path: [(PathBuf::new(), ROOT_ID)].into(),
                handles: HashMap::new(),
                handles_by_inode: HashMap::new(),
                next_ino: ROOT_ID + 1,
                next_handle: 1,
            })),
        })
    }

    /// The host directory this filesystem serves.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether mutating operations are refused.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Largest negotiated payload plus the FUSE header allowance. Before
    /// INIT (or on a fresh restore), use the server's absolute maximum.
    pub fn request_limit(&self) -> usize {
        self.request_limit
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Serialise the session: every inode with its path and lookup count,
    /// every open handle with the path it was opened at.
    pub fn save_state(&self) -> Vec<u8> {
        let session = self.inner.lock();
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.push(2); // format version
        out.push(u8::from(session.writeback_cache));
        out.extend_from_slice(&(session.inodes.len() as u32).to_le_bytes());
        let mut inodes: Vec<(&u64, &Inode)> = session.inodes.iter().collect();
        inodes.sort_by_key(|(ino, _)| **ino);
        for (ino, inode) in inodes {
            let path = inode.path.as_os_str().as_bytes();
            out.extend_from_slice(&ino.to_le_bytes());
            out.extend_from_slice(&inode.lookups.to_le_bytes());
            out.push(match inode.kind {
                FileKind::File => 0,
                FileKind::Dir => 1,
                FileKind::Symlink => 2,
                FileKind::Other => 3,
            });
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path);
        }
        out.extend_from_slice(&(session.handles.len() as u32).to_le_bytes());
        let mut handles: Vec<(&u64, &Handle)> = session.handles.iter().collect();
        handles.sort_by_key(|(h, _)| **h);
        for (handle, open) in handles {
            out.extend_from_slice(&handle.to_le_bytes());
            out.extend_from_slice(&open.ino.to_le_bytes());
            match &open.kind {
                HandleKind::File { flags, path, .. } => {
                    out.push(0);
                    out.extend_from_slice(&(*flags as u32).to_le_bytes());
                    let path = path.as_os_str().as_bytes();
                    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
                    out.extend_from_slice(path);
                }
                HandleKind::Dir { .. } => out.push(1),
            }
        }
        out.extend_from_slice(&session.next_ino.to_le_bytes());
        out.extend_from_slice(&session.next_handle.to_le_bytes());
        out
    }

    /// Replace the session with one rebuilt from [`Self::save_state`] bytes:
    /// every inode is re-walked from the root, every file handle reopened at
    /// its recorded path (creation flags stripped), every directory handle
    /// reset to its start. A missing path becomes a dead inode; an open file
    /// that vanished keeps its handle ID and answers `ESTALE` on I/O.
    pub fn restore_state(&self, state: &[u8]) -> io::Result<()> {
        let err = || io::Error::new(io::ErrorKind::InvalidData, "bad passthrough state");
        if state.len() < 10 || u32::from_le_bytes(state[0..4].try_into().unwrap()) != MAGIC {
            return Err(err());
        }
        let (writeback_cache, mut cur) = match state[4] {
            // Version 1 did not record negotiated writeback. Preserve its
            // host append behavior when restoring those sessions.
            1 => (false, 5usize),
            2 => match state[5] {
                0 => (false, 6),
                1 => (true, 6),
                _ => return Err(err()),
            },
            version => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported passthrough state version {version}"),
                ));
            }
        };
        let mut take = |len: usize| -> io::Result<&[u8]> {
            let end = cur.checked_add(len).ok_or_else(err)?;
            let slice = state.get(cur..end).ok_or_else(err)?;
            cur = end;
            Ok(slice)
        };
        let root_fd = self.inner.lock().inodes[&ROOT_ID].fd.try_clone()?;

        let count = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        if count > SESSION_LIMIT {
            return Err(err());
        }
        let mut inodes: HashMap<u64, Inode> = HashMap::with_capacity(count);
        let mut by_path = HashMap::new();
        for _ in 0..count {
            let ino = u64::from_le_bytes(take(8)?.try_into().unwrap());
            let lookups = u64::from_le_bytes(take(8)?.try_into().unwrap());
            let kind = match take(1)?[0] {
                0 => FileKind::File,
                1 => FileKind::Dir,
                2 => FileKind::Symlink,
                3 => FileKind::Other,
                _ => return Err(err()),
            };
            let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            let path = PathBuf::from(OsStr::from_bytes(take(len)?));
            if ino == ROOT_ID {
                if !path.as_os_str().is_empty() {
                    return Err(err());
                }
                by_path.insert(PathBuf::new(), ino);
                let fd = root_fd.try_clone()?;
                let parent = fd.try_clone()?;
                inodes.insert(
                    ino,
                    Inode {
                        fd,
                        parent,
                        name: Vec::new(),
                        path,
                        kind: FileKind::Dir,
                        lookups,
                        dead: false,
                    },
                );
                continue;
            }
            // Re-walk the recorded path one component at a time from the
            // root: a snapshot is data at rest, not a trusted guest.
            let mut fd = root_fd.try_clone()?;
            let mut parent = root_fd.try_clone()?;
            let mut name = Vec::new();
            let mut alive = true;
            for component in path.components() {
                let Some(component) = normal_component(&component) else {
                    alive = false;
                    break;
                };
                match openat(&fd, component, libc::O_PATH | libc::O_NOFOLLOW) {
                    Ok(child) => {
                        parent = fd;
                        fd = child;
                        name = component.to_vec();
                    }
                    Err(_) => {
                        alive = false;
                        break;
                    }
                }
            }
            if alive {
                alive = kind_of(fstat(fd.as_raw_fd())?.st_mode) == kind;
            }
            if alive {
                by_path.insert(path.clone(), ino);
            }
            inodes.insert(
                ino,
                Inode {
                    fd: if alive { fd } else { root_fd.try_clone()? },
                    parent,
                    name,
                    path,
                    kind,
                    lookups,
                    dead: !alive,
                },
            );
        }

        let count = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        if count > SESSION_LIMIT {
            return Err(err());
        }
        let mut handles: HashMap<u64, Handle> = HashMap::with_capacity(count);
        for _ in 0..count {
            let handle = u64::from_le_bytes(take(8)?.try_into().unwrap());
            let ino = u64::from_le_bytes(take(8)?.try_into().unwrap());
            let Some(inode) = inodes.get(&ino) else {
                return Err(err());
            };
            let kind = match take(1)?[0] {
                0 => {
                    let flags = host_open_flags(
                        i32::from_le_bytes(take(4)?.try_into().unwrap()),
                        writeback_cache,
                    );
                    let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
                    let path = PathBuf::from(OsStr::from_bytes(take(len)?));
                    let file = if inode.dead {
                        None
                    } else {
                        reopen(&inode.fd, flags).ok()
                    };
                    HandleKind::File { file, flags, path }
                }
                1 => HandleKind::Dir {
                    dir: None,
                    position: None,
                },
                _ => return Err(err()),
            };
            handles.insert(handle, Handle { ino, kind });
        }
        let next_ino = u64::from_le_bytes(take(8)?.try_into().unwrap());
        let next_handle = u64::from_le_bytes(take(8)?.try_into().unwrap());
        if cur != state.len() {
            return Err(err());
        }
        let mut session = self.inner.lock();
        session.writeback_cache = writeback_cache;
        session.inodes = inodes;
        session.by_path = by_path;
        session.handles_by_inode.clear();
        for open in handles.values() {
            *session.handles_by_inode.entry(open.ino).or_default() += 1;
        }
        session.handles = handles;
        session.next_ino = next_ino;
        session.next_handle = next_handle;
        Ok(())
    }

    fn refuse_writes(&self) -> io::Result<()> {
        if self.read_only {
            Err(io::Error::from_raw_os_error(libc::EROFS))
        } else {
            Ok(())
        }
    }
}

impl FileSystem for PassthroughFs {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        let mut want = FsOptions::ASYNC_READ
            | FsOptions::AUTO_INVAL_DATA
            | FsOptions::DO_READDIRPLUS
            | FsOptions::READDIRPLUS_AUTO
            | FsOptions::PARALLEL_DIROPS
            | FsOptions::ABORT_ERROR
            | FsOptions::MAX_PAGES
            | FsOptions::CACHE_SYMLINKS
            | FsOptions::BIG_WRITES;
        if !self.read_only {
            // The guest buffers writes and pushes them later; without
            // writeback it would flush every write synchronously.
            want |= FsOptions::WRITEBACK_CACHE;
        }
        let enabled = want & capable;
        self.inner.lock().writeback_cache = enabled.contains(FsOptions::WRITEBACK_CACHE);
        let payload = if enabled.intersects(FsOptions::BIG_WRITES | FsOptions::MAX_PAGES) {
            super::fuse::server::MAX_BUFFER_SIZE as usize
        } else {
            4096
        };
        self.request_limit
            .store(payload + 4096, std::sync::atomic::Ordering::Relaxed);
        Ok(enabled)
    }

    fn destroy(&self) {
        let mut session = self.inner.lock();
        // Keep the pinned share root so reset also works after a host rename.
        session.inodes.retain(|&ino, _| ino == ROOT_ID);
        session.by_path.retain(|_, ino| *ino == ROOT_ID);
        session.handles.clear();
        session.handles_by_inode.clear();
        session.next_ino = ROOT_ID + 1;
        session.next_handle = 1;
        session.writeback_cache = false;
        self.request_limit
            .store(INITIAL_REQUEST_LIMIT, std::sync::atomic::Ordering::Relaxed);
    }

    fn lookup(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let path = parent.path.join(OsStr::from_bytes(bytes));
        let dirfd = parent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, kind_of(st.st_mode), &st)
    }

    fn forget(&self, _ctx: &Context, inode: u64, count: u64) {
        self.inner.lock().forget_one(inode, count);
    }

    fn batch_forget(&self, _ctx: &Context, requests: Vec<(u64, u64)>) {
        let mut session = self.inner.lock();
        for (inode, count) in requests {
            session.forget_one(inode, count);
        }
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: Option<u64>,
    ) -> io::Result<(stat64, Duration)> {
        let session = self.inner.lock();
        let fd = if let Some(handle) = handle
            && let Some(open) = session.handles.get(&handle)
            && let HandleKind::File { file, .. } = &open.kind
        {
            file.as_ref()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ESTALE))?
                .as_raw_fd()
        } else {
            live_inode(&session, inode)?.fd.as_raw_fd()
        };
        let mut st = fstat(fd)?;
        st.st_ino = inode;
        Ok((st, TIMEOUT))
    }

    fn setattr(
        &self,
        _ctx: &Context,
        inode: u64,
        attr: stat64,
        handle: Option<u64>,
        valid: SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        self.refuse_writes()?;
        let session = self.inner.lock();
        let state = live_inode(&session, inode)?;
        if let Some(handle) = handle {
            session.file_handle(inode, handle)?;
        }
        if valid.contains(SetattrValid::SIZE)
            && kind_of(fstat(state.fd.as_raw_fd())?.st_mode) != FileKind::File
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        if valid.contains(SetattrValid::MODE) {
            fchmodat(&state.fd, attr.st_mode)?;
        }
        if valid.intersects(SetattrValid::UID | SetattrValid::GID) {
            fchownat(&state.fd, valid, &attr)?;
        }
        if valid.contains(SetattrValid::SIZE) {
            truncateat(&state.fd, attr.st_size)?;
        }
        if valid.intersects(SetattrValid::ATIME | SetattrValid::MTIME) {
            utimensat(&state.fd, valid, &attr)?;
        }
        let mut st = fstat(state.fd.as_raw_fd())?;
        st.st_ino = inode;
        Ok((st, TIMEOUT))
    }

    fn readlink(&self, _ctx: &Context, inode: u64) -> io::Result<Vec<u8>> {
        let session = self.inner.lock();
        let inode = live_inode(&session, inode)?;
        if inode.kind != FileKind::Symlink {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // readlinkat on the parent's descriptor: one component, never a
        // traversal, and unlike the /proc fd link it spells the target.
        let mut target = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: `cname` is NUL-terminated and `target` is writable for
        // its length; the fd is owned.
        let n = unsafe {
            libc::readlinkat(
                inode.parent.as_raw_fd(),
                cname(&inode.name).as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        target.truncate(n as usize);
        Ok(target)
    }

    fn symlink(
        &self,
        _ctx: &Context,
        target: &CStr,
        parent: u64,
        name: &CStr,
    ) -> io::Result<Entry> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let path = parent.path.join(OsStr::from_bytes(bytes));
        symlinkat(target, &parent.fd, bytes)?;
        let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let dirfd = parent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, FileKind::Symlink, &st)
    }

    fn mknod(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        rdev: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        self.refuse_writes()?;
        if !matches!(mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFSOCK) {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let path = parent.path.join(OsStr::from_bytes(bytes));
        mknodat(&parent.fd, bytes, mode as libc::mode_t, rdev as libc::dev_t)?;
        let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let dirfd = parent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, kind_of(st.st_mode), &st)
    }

    fn mkdir(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let path = parent.path.join(OsStr::from_bytes(bytes));
        mkdirat(&parent.fd, bytes, mode as libc::mode_t)?;
        let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let dirfd = parent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, FileKind::Dir, &st)
    }

    fn unlink(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.refuse_writes()?;
        let session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        unlinkat(&parent.fd, checked_name(name)?, false)
    }

    fn rmdir(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<()> {
        self.refuse_writes()?;
        let session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        unlinkat(&parent.fd, checked_name(name)?, true)
    }

    fn rename(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        newparent: u64,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        let parent = live_inode(&session, parent)?;
        let newparent = live_inode(&session, newparent)?;
        if parent.kind != FileKind::Dir || newparent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let name = checked_name(name)?;
        let newname = checked_name(newname)?;
        let from = parent.path.join(OsStr::from_bytes(name));
        let to = newparent.path.join(OsStr::from_bytes(newname));
        // Cloned before the rename: once the host has moved the file, the
        // session must be able to follow it without a step that can fail.
        let dir = parent.fd.try_clone()?;
        let newdir = newparent.fd.try_clone()?;
        // rename(2) does nothing, and succeeds, when both names are links
        // of one file. Then neither name moved.
        let same_file = flags & libc::RENAME_EXCHANGE == 0
            && matches!(
                (host_identity(&dir, name), host_identity(&newdir, newname)),
                (Some(a), Some(b)) if a == b
            );
        renameat2(&dir, name, &newdir, newname, flags)?;
        if from == to || same_file {
            return Ok(());
        }
        if flags & libc::RENAME_EXCHANGE != 0 {
            let moved = session.take_subtree(&from);
            let back = session.take_subtree(&to);
            session.place_subtree(moved, &to, newdir, newname);
            session.place_subtree(back, &from, dir, name);
        } else {
            // Whatever the destination name held is replaced: its inodes
            // stay with the guest until FORGET, but no longer own the name.
            session.take_subtree(&to);
            let moved = session.take_subtree(&from);
            session.place_subtree(moved, &to, newdir, newname);
        }
        Ok(())
    }

    fn link(
        &self,
        _ctx: &Context,
        inode: u64,
        newparent: u64,
        newname: &CStr,
    ) -> io::Result<Entry> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        let inode = live_inode(&session, inode)?;
        let newparent = live_inode(&session, newparent)?;
        if newparent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(newname)?;
        linkat(&inode.fd, &newparent.fd, bytes)?;
        let fd = openat(&newparent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let path = newparent.path.join(OsStr::from_bytes(bytes));
        let dirfd = newparent.fd.try_clone()?;
        session.entry_for(path, fd, dirfd, bytes, kind_of(st.st_mode), &st)
    }

    fn open(
        &self,
        _ctx: &Context,
        inode: u64,
        flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions, Option<u32>)> {
        let mut session = self.inner.lock();
        let handle = session.open_file(self.read_only, inode, flags as i32)?;
        Ok((Some(handle), OpenOptions::empty(), None))
    }

    fn create(
        &self,
        _ctx: &Context,
        parent: u64,
        name: &CStr,
        args: CreateIn,
    ) -> io::Result<(Entry, Option<u64>, OpenOptions, Option<u32>)> {
        self.refuse_writes()?;
        let mut session = self.inner.lock();
        session.check_handle_capacity()?;
        if session.inodes.len() >= SESSION_LIMIT {
            return Err(io::Error::from_raw_os_error(libc::ENFILE));
        }
        let parent = live_inode(&session, parent)?;
        if parent.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let bytes = checked_name(name)?;
        let flags = host_open_flags(args.flags as i32, session.writeback_cache);
        // EXCL makes creation atomic. Existing names are vetted with
        // O_PATH|NOFOLLOW and opened through that fd, never by name again.
        let file = match createat(
            &parent.fd,
            bytes,
            flags | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            args.mode,
        ) {
            Ok(file) => file,
            Err(e)
                if e.raw_os_error() == Some(libc::EEXIST)
                    && args.flags as i32 & libc::O_EXCL == 0 =>
            {
                let fd = openat(&parent.fd, bytes, libc::O_PATH | libc::O_NOFOLLOW)?;
                let file = reopen(&fd, flags)?;
                if args.flags as i32 & libc::O_TRUNC != 0 {
                    file.set_len(0)?;
                }
                file
            }
            Err(e) => return Err(e),
        };
        let path = parent.path.join(OsStr::from_bytes(bytes));
        let fd = open_path(
            Path::new(&format!("/proc/self/fd/{}", file.as_raw_fd())),
            libc::O_PATH,
        )?;
        let st = fstat(fd.as_raw_fd())?;
        let dirfd = parent.fd.try_clone()?;
        let entry = session.entry_for(path.clone(), fd, dirfd, bytes, kind_of(st.st_mode), &st)?;
        let handle = session.insert_file_handle(entry.inode, file, flags, path)?;
        Ok((entry, Some(handle), OpenOptions::empty(), None))
    }

    fn read(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let session = self.inner.lock();
        let mut file = session.file_handle(inode, handle)?;
        // The reply carries the header too: never claim more than the
        // writable descriptors hold.
        let size = (size as usize).min(w.available_bytes());
        w.write_from(&mut file, size, offset)
    }

    fn write(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        r: &mut dyn ZeroCopyReader,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<usize> {
        // The open already refuses write flags on a read-only share; this
        // guards a handle restored into a share that became read-only.
        self.refuse_writes()?;
        let session = self.inner.lock();
        let mut file = session.file_handle(inode, handle)?;
        r.read_to(&mut file, size as usize, offset)
    }

    fn flush(&self, _ctx: &Context, _inode: u64, _handle: u64, _lock_owner: u64) -> io::Result<()> {
        // Every operation acts on the host file directly; there is nothing
        // buffered to flush.
        Ok(())
    }

    fn fsync(&self, _ctx: &Context, inode: u64, datasync: bool, handle: u64) -> io::Result<()> {
        let session = self.inner.lock();
        let file = session.file_handle(inode, handle)?;
        fsync(file.as_raw_fd(), datasync)
    }

    fn release(
        &self,
        _ctx: &Context,
        inode: u64,
        _flags: u32,
        handle: u64,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.inner.lock().forget_handle(inode, handle);
        Ok(())
    }

    fn statfs(&self, _ctx: &Context, _inode: u64) -> io::Result<libc::statvfs64> {
        let session = self.inner.lock();
        let root = live_inode(&session, ROOT_ID)?;
        // SAFETY: statvfs64 is a POD struct; zeroing is its init.
        let mut st: libc::statvfs64 = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is an out-parameter of statvfs(2); the fd is owned.
        if unsafe { libc::fstatvfs64(root.fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    fn opendir(
        &self,
        _ctx: &Context,
        inode: u64,
        flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        {
            let session = self.inner.lock();
            let inode = live_inode(&session, inode)?;
            if inode.kind != FileKind::Dir {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
            }
        }
        if flags as i32 & libc::O_ACCMODE != libc::O_RDONLY
            || flags as i32 & (libc::O_TRUNC | libc::O_APPEND | libc::O_CREAT) != 0
        {
            self.refuse_writes()?;
        }
        let mut session = self.inner.lock();
        session.check_handle_capacity()?;
        let handle = session.next_handle;
        session.next_handle += 1;
        session.handles.insert(
            handle,
            Handle {
                ino: inode,
                kind: HandleKind::Dir {
                    dir: None,
                    position: None,
                },
            },
        );
        *session.handles_by_inode.entry(inode).or_default() += 1;
        Ok((Some(handle), OpenOptions::empty()))
    }

    fn readdir(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let mut session = self.inner.lock();
        let dir_path = live_inode(&session, inode)?.path.clone();
        // The cookie of an entry is the host's own: it resumes after that
        // entry. Each batch is bounded, so a request costs what it serves
        // and not the size of the directory.
        let mut used = 0usize;
        let mut position = offset;
        while used <= size as usize {
            let Some((entries, next)) =
                session.dir_batch(inode, handle, position, size as usize)?
            else {
                break;
            };
            position = next;
            for entry in &entries {
                if used > size as usize {
                    return Ok(());
                }
                let ino = session
                    .by_path
                    .get(&dir_path.join(OsStr::from_bytes(&entry.name)))
                    .copied()
                    .unwrap_or(entry.host_ino);
                match add_entry(DirEntry {
                    ino,
                    offset: entry.cookie,
                    type_: entry.d_type as u32,
                    name: &entry.name,
                }) {
                    Ok(0) => return Ok(()),
                    Ok(len) => used += len,
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    fn readdirplus(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let mut session = self.inner.lock();
        // Every entry handed to `add_entry` carries one lookup the guest
        // answers with FORGET. A request that fails is answered with the
        // error alone, so the guest sees none of them: take them back.
        let mut served = Vec::new();
        let result = session.readdirplus(inode, handle, size, offset, &mut served, add_entry);
        if result.is_err() {
            for ino in served {
                session.forget_one(ino, 1);
            }
        }
        result
    }

    fn fsyncdir(&self, _ctx: &Context, inode: u64, datasync: bool, _handle: u64) -> io::Result<()> {
        let session = self.inner.lock();
        let inode = live_inode(&session, inode)?;
        // O_PATH descriptors cannot be synced; reopen the directory.
        let dir = reopen(&inode.fd, libc::O_RDONLY | libc::O_DIRECTORY)?;
        fsync(dir.as_raw_fd(), datasync)
    }

    fn releasedir(&self, _ctx: &Context, inode: u64, _flags: u32, handle: u64) -> io::Result<()> {
        self.inner.lock().forget_handle(inode, handle);
        Ok(())
    }

    fn access(&self, _ctx: &Context, _inode: u64, _mask: u32) -> io::Result<()> {
        // Permission checking happens on each host syscall; the guest's own
        // mode bits are the policy it sees.
        Ok(())
    }

    fn lseek(
        &self,
        _ctx: &Context,
        inode: u64,
        handle: u64,
        offset: u64,
        whence: u32,
    ) -> io::Result<u64> {
        let session = self.inner.lock();
        let file = session.file_handle(inode, handle)?;
        // SAFETY: lseek(2) on an owned fd; the return is the new offset.
        let out = unsafe { libc::lseek(file.as_raw_fd(), offset as i64, whence as i32) };
        if out < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(out as u64)
    }

    // xattrs, locks, ioctl and DAX stay unimplemented: the kernel takes the
    // ENOSYS defaults (no xattrs, local POSIX locks, no cache window).
}

impl Session {
    /// One FORGET: drop the inode when no lookup count and no handle keeps
    /// it.
    fn forget_one(&mut self, inode: u64, count: u64) {
        if inode == ROOT_ID {
            return;
        }
        let lookups = match self.inodes.get_mut(&inode) {
            Some(state) => {
                state.lookups = state.lookups.saturating_sub(count);
                state.lookups
            }
            None => return,
        };
        if lookups == 0
            && !self.holds(inode)
            && let Some(state) = self.inodes.remove(&inode)
            // Another live path may have taken the slot after a rename.
            && self.by_path.get(&state.path) == Some(&inode)
        {
            self.by_path.remove(&state.path);
        }
    }

    /// Take the inode at `root` and every inode below it out of `by_path`,
    /// each with its path relative to `root`. The inodes themselves stay.
    fn take_subtree(&mut self, root: &Path) -> Vec<(PathBuf, u64)> {
        let paths: Vec<PathBuf> = self
            .by_path
            .keys()
            .filter(|path| path.starts_with(root))
            .cloned()
            .collect();
        paths
            .into_iter()
            .map(|path| {
                let ino = self.by_path.remove(&path).expect("key just listed");
                let below = path.strip_prefix(root).expect("filtered on the prefix");
                (below.to_path_buf(), ino)
            })
            .collect()
    }

    /// Record a subtree from [`Self::take_subtree`] at `root`, the name
    /// `name` under the directory `parent`: a rename moved it there. Only
    /// the subtree's own top changes parent and name; everything below
    /// keeps both and changes path.
    fn place_subtree(
        &mut self,
        subtree: Vec<(PathBuf, u64)>,
        root: &Path,
        parent: OwnedFd,
        name: &[u8],
    ) {
        let mut parent = Some(parent);
        for (below, ino) in &subtree {
            let top = below.as_os_str().is_empty();
            let path = if top {
                root.to_path_buf()
            } else {
                root.join(below)
            };
            if let Some(state) = self.inodes.get_mut(ino) {
                if top && let Some(parent) = parent.take() {
                    state.parent = parent;
                    state.name = name.to_vec();
                }
                state.path = path.clone();
            }
            self.by_path.insert(path, *ino);
        }
        // An open handle records its path for the snapshot.
        for open in self.handles.values_mut() {
            if let HandleKind::File { path, .. } = &mut open.kind
                && subtree.iter().any(|(_, ino)| *ino == open.ino)
                && let Some(state) = self.inodes.get(&open.ino)
            {
                path.clone_from(&state.path);
            }
        }
    }

    fn holds(&self, inode: u64) -> bool {
        self.handles_by_inode.contains_key(&inode)
    }

    /// The inode for `path`, newly opened (`fd`) or already live: repeat
    /// lookups of one file keep one guest inode. Bumps the lookup count.
    #[allow(clippy::too_many_arguments)]
    fn entry_for(
        &mut self,
        path: PathBuf,
        fd: OwnedFd,
        parent: OwnedFd,
        name: &[u8],
        kind: FileKind,
        st: &stat64,
    ) -> io::Result<Entry> {
        let ino = match self.by_path.get(&path) {
            Some(&ino) => {
                // Refresh the descriptor: the old one may belong to a file
                // the host replaced since the last lookup.
                if let Some(state) = self.inodes.get_mut(&ino) {
                    state.fd = fd;
                    state.parent = parent;
                    state.name = name.to_vec();
                    state.kind = kind;
                    state.dead = false;
                }
                ino
            }
            None => {
                if self.inodes.len() >= SESSION_LIMIT {
                    return Err(io::Error::from_raw_os_error(libc::ENFILE));
                }
                let ino = self.next_ino;
                self.next_ino += 1;
                self.by_path.insert(path.clone(), ino);
                self.inodes.insert(
                    ino,
                    Inode {
                        fd,
                        parent,
                        name: name.to_vec(),
                        path,
                        kind,
                        lookups: 0,
                        dead: false,
                    },
                );
                ino
            }
        };
        let state = self.inodes.get_mut(&ino).expect("just inserted");
        state.lookups = state.lookups.saturating_add(1);
        let mut attr = *st;
        attr.st_ino = ino;
        Ok(Entry {
            inode: ino,
            generation: 0,
            attr,
            attr_flags: 0,
            attr_timeout: TIMEOUT,
            entry_timeout: TIMEOUT,
        })
    }

    /// Resolve one cached dirent to a session inode (used by READDIRPLUS).
    fn resolve(&mut self, path: &Path, dir: &OwnedFd, name: &[u8]) -> io::Result<Entry> {
        if let Some(&ino) = self.by_path.get(path) {
            let state = self.inodes.get(&ino).expect("by_path agrees with inodes");
            let st = fstat(state.fd.as_raw_fd())?;
            let kind = state.kind;
            let fd = state.fd.try_clone()?;
            let parent = state.parent.try_clone()?;
            let name = state.name.clone();
            let path = state.path.clone();
            return self.entry_for(path, fd, parent, &name, kind, &st);
        }
        let fd = openat(dir, name, libc::O_PATH | libc::O_NOFOLLOW)?;
        let st = fstat(fd.as_raw_fd())?;
        let parent = dir.try_clone()?;
        self.entry_for(
            path.to_path_buf(),
            fd,
            parent,
            name,
            kind_of(st.st_mode),
            &st,
        )
    }

    fn check_handle_capacity(&self) -> io::Result<()> {
        if self.handles.len() >= SESSION_LIMIT {
            return Err(io::Error::from_raw_os_error(libc::ENFILE));
        }
        Ok(())
    }

    fn insert_file_handle(
        &mut self,
        ino: u64,
        file: File,
        flags: i32,
        path: PathBuf,
    ) -> io::Result<u64> {
        self.check_handle_capacity()?;
        let handle = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(
            handle,
            Handle {
                ino,
                kind: HandleKind::File {
                    file: Some(file),
                    flags,
                    path,
                },
            },
        );
        *self.handles_by_inode.entry(ino).or_default() += 1;
        Ok(handle)
    }

    fn open_file(&mut self, read_only: bool, inode: u64, flags: i32) -> io::Result<u64> {
        self.check_handle_capacity()?;
        let state = live_inode(self, inode)?;
        if read_only
            && (flags & libc::O_ACCMODE != libc::O_RDONLY
                || flags & (libc::O_TRUNC | libc::O_APPEND | libc::O_CREAT) != 0)
        {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        let host_flags = host_open_flags(flags, self.writeback_cache);
        let file = reopen(&state.fd, host_flags)?;
        if flags & libc::O_TRUNC != 0 {
            file.set_len(0)?;
        }
        let path = state.path.clone();
        self.insert_file_handle(inode, file, host_flags, path)
    }

    /// A clone of the open file behind `handle` for one I/O operation.
    fn file_handle(&self, inode: u64, handle: u64) -> io::Result<File> {
        let open = self
            .handles
            .get(&handle)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
        if open.ino != inode {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        match &open.kind {
            HandleKind::File {
                file: Some(file), ..
            } => file
                .try_clone()
                .map_err(|e| io::Error::new(e.kind(), format!("reopening for I/O: {e}"))),
            HandleKind::File { file: None, .. } => Err(io::Error::from_raw_os_error(libc::ESTALE)),
            HandleKind::Dir { .. } => Err(io::Error::from_raw_os_error(libc::EISDIR)),
        }
    }

    /// READDIRPLUS under the session lock. `served` takes the inode of each
    /// entry `add_entry` accepted, in order: the caller undoes their lookups
    /// when the request fails.
    fn readdirplus(
        &mut self,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        served: &mut Vec<u64>,
        add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let live = live_inode(self, inode)?;
        let dir_path = live.path.clone();
        let dir_fd = live.fd.try_clone()?;
        // Cookies are the host's own, as in `readdir`. A READDIRPLUS record
        // is several times its getdents64 record, so read a smaller batch:
        // what does not fit is read again by the next request.
        let mut used = 0usize;
        let mut position = offset;
        while used <= size as usize {
            let Some((entries, next)) =
                self.dir_batch(inode, handle, position, size as usize / 4)?
            else {
                break;
            };
            position = next;
            for cached in &entries {
                if used > size as usize {
                    return Ok(());
                }
                let path = dir_path.join(OsStr::from_bytes(&cached.name));
                // Resolve (or create) the session inode so the Entry carries
                // the server's own ino and a lookup count the kernel will
                // FORGET.
                let entry = match self.resolve(&path, &dir_fd, &cached.name) {
                    Ok(entry) => entry,
                    // The inode table is full. Skipping the entry would
                    // hide a file that exists, so say it: end a reply that
                    // already holds entries here (the guest asks again at
                    // the last cookie it took), and fail one that holds
                    // none.
                    Err(e) if e.raw_os_error() == Some(libc::ENFILE) => {
                        return if served.is_empty() { Err(e) } else { Ok(()) };
                    }
                    Err(_) => continue, // raced away; the guest re-lookups
                };
                let ino = entry.inode;
                match add_entry(
                    DirEntry {
                        ino,
                        offset: cached.cookie,
                        type_: cached.d_type as u32,
                        name: &cached.name,
                    },
                    entry,
                ) {
                    // The entry did not fit and the guest never sees it, so
                    // no FORGET comes for it: drop its lookup here.
                    Ok(0) => {
                        self.forget_one(ino, 1);
                        return Ok(());
                    }
                    Ok(len) => {
                        served.push(ino);
                        used += len;
                    }
                    Err(e) => {
                        self.forget_one(ino, 1);
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }

    /// One bounded batch of the entries of `handle` after the cookie
    /// `offset`, with the cookie the stream then stands at. `None` is the
    /// end of the directory. A batch can be empty before the end (it held
    /// only `.` and `..`). Offset 0 rewinds, so the guest sees the
    /// directory as it is now.
    fn dir_batch(
        &mut self,
        inode: u64,
        handle: u64,
        offset: u64,
        budget: usize,
    ) -> io::Result<Option<(Vec<HostDirent>, u64)>> {
        let unopened = match self.handles.get(&handle) {
            Some(open) if open.ino != inode => {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            Some(Handle {
                kind: HandleKind::Dir { dir, .. },
                ..
            }) => dir.is_none(),
            Some(_) => return Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
            None => return Err(io::Error::from_raw_os_error(libc::EBADF)),
        };
        let opened = if unopened {
            let fd = &live_inode(self, inode)?.fd;
            Some(reopen(fd, libc::O_RDONLY | libc::O_DIRECTORY)?)
        } else {
            None
        };
        let Some(Handle {
            kind: HandleKind::Dir { dir, position },
            ..
        }) = self.handles.get_mut(&handle)
        else {
            unreachable!("checked above, under the same lock");
        };
        if let Some(opened) = opened {
            *dir = Some(opened);
            *position = None;
        }
        let dir = dir.as_ref().expect("opened above");
        // Forget the position first: a failed seek or read leaves the
        // stream where nobody knows.
        if position.take() != Some(offset) {
            let target =
                i64::try_from(offset).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
            // SAFETY: lseek(2) on an owned fd; the return is the new offset.
            if unsafe { libc::lseek64(dir.as_raw_fd(), target, libc::SEEK_SET) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let Some((entries, last)) = read_dir_batch(dir, budget)? else {
            return Ok(None);
        };
        *position = Some(last);
        Ok(Some((entries, last)))
    }

    /// Directory entries the session keeps in memory between requests,
    /// over every open directory handle.
    #[cfg(test)]
    fn held_dirents(&self) -> usize {
        self.handles
            .values()
            .map(|open| match &open.kind {
                // A directory handle is a descriptor and a cookie.
                HandleKind::Dir {
                    dir: _,
                    position: _,
                } => 0,
                HandleKind::File { .. } => 0,
            })
            .sum()
    }

    fn forget_handle(&mut self, inode: u64, handle: u64) {
        if self
            .handles
            .get(&handle)
            .is_some_and(|open| open.ino == inode)
        {
            self.handles.remove(&handle);
            if let Some(count) = self.handles_by_inode.get_mut(&inode) {
                *count -= 1;
                if *count == 0 {
                    self.handles_by_inode.remove(&inode);
                }
            }
            // The open kept the inode alive beyond its last FORGET.
            self.forget_one(inode, 0);
        }
    }
}

/// The live inode `ino`, or `ENOENT` for one the guest was never given or
/// one whose path died with a restore.
fn live_inode(session: &Session, ino: u64) -> io::Result<&Inode> {
    session
        .inodes
        .get(&ino)
        .filter(|inode| !inode.dead)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
}

/// A single path component from the guest, or the error the kernel expects.
fn checked_name(name: &CStr) -> io::Result<&[u8]> {
    let bytes = name.to_bytes();
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.contains(&b'/')
        || bytes.len() > 255
    {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(bytes)
}

/// The bytes of one normal path component, or `None` for anything else
/// (`..`, root, current dir).
fn normal_component<'a>(component: &std::path::Component<'a>) -> Option<&'a [u8]> {
    match component {
        std::path::Component::Normal(name) => Some(name.as_bytes()),
        _ => None,
    }
}

/// One `openat(2)` with `O_CLOEXEC`, owning the result.
fn openat(dir: &OwnedFd, name: &[u8], flags: i32) -> io::Result<OwnedFd> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated and outlives the call; the fd is
    // owned from return.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), cname.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the syscall above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `open(2)` on a whole path; only the share root, which the host named.
fn open_path(path: &Path, flags: i32) -> io::Result<OwnedFd> {
    let cname = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; the fd is owned from return.
    let fd = unsafe { libc::open(cname.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the syscall above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn host_open_flags(mut flags: i32, writeback_cache: bool) -> i32 {
    // With writeback, the guest owns the append offset. Linux positional
    // writes on an O_APPEND descriptor would still append at host EOF.
    if writeback_cache {
        flags &= !libc::O_APPEND;
    }
    (flags
        & (libc::O_ACCMODE
            | libc::O_DIRECT
            | libc::O_NONBLOCK
            | libc::O_NOATIME
            | libc::O_APPEND
            | libc::O_SYNC
            | libc::O_DSYNC))
        | libc::O_CLOEXEC
}

/// Reopen the inode held by an `O_PATH` descriptor through its procfs fd link.
fn reopen(path_fd: &OwnedFd, flags: i32) -> io::Result<File> {
    // Inspect the held inode, not its saved name or kind. This also
    // protects restored handles when a regular file became a special file.
    match kind_of(fstat(path_fd.as_raw_fd())?.st_mode) {
        FileKind::File => {}
        FileKind::Dir if flags & libc::O_DIRECTORY != 0 => {}
        FileKind::Symlink => return Err(io::Error::from_raw_os_error(libc::ELOOP)),
        _ => return Err(io::Error::from_raw_os_error(libc::EPERM)),
    }
    let path = CString::new(format!("/proc/self/fd/{}", path_fd.as_raw_fd()))
        .expect("proc path has no NUL");
    // SAFETY: `path` is NUL-terminated; the fd is owned from return.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the open above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> io::Result<stat64> {
    // SAFETY: stat64 is a POD struct; zeroing is its init.
    let mut st: stat64 = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is an out-parameter of fstat(2); the fd is owned.
    if unsafe { libc::fstat64(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn kind_of(mode: libc::mode_t) -> FileKind {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => FileKind::Dir,
        libc::S_IFLNK => FileKind::Symlink,
        libc::S_IFREG => FileKind::File,
        _ => FileKind::Other,
    }
}

fn symlinkat(target: &CStr, dir: &OwnedFd, name: &[u8]) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: both strings are NUL-terminated; the fd is owned.
    if unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), cname.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mknodat(dir: &OwnedFd, name: &[u8], mode: libc::mode_t, rdev: libc::dev_t) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; the fd is owned.
    if unsafe { libc::mknodat(dir.as_raw_fd(), cname.as_ptr(), mode, rdev) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mkdirat(dir: &OwnedFd, name: &[u8], mode: libc::mode_t) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; the fd is owned.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), cname.as_ptr(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn createat(dir: &OwnedFd, name: &[u8], flags: i32, mode: u32) -> io::Result<File> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; the fd is owned from return.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            cname.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd is fresh and owned from the open above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn unlinkat(dir: &OwnedFd, name: &[u8], directory: bool) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: `cname` is NUL-terminated; the fd is owned.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), cname.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Which host file `name` in `dir` is, as (device, inode); `None` when it
/// cannot be told.
fn host_identity(dir: &OwnedFd, name: &[u8]) -> Option<(u64, u64)> {
    let fd = openat(dir, name, libc::O_PATH | libc::O_NOFOLLOW).ok()?;
    let st = fstat(fd.as_raw_fd()).ok()?;
    Some((st.st_dev, st.st_ino))
}

fn renameat2(
    old_dir: &OwnedFd,
    old_name: &[u8],
    new_dir: &OwnedFd,
    new_name: &[u8],
    flags: u32,
) -> io::Result<()> {
    let old =
        CString::new(old_name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let new =
        CString::new(new_name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: both strings are NUL-terminated; both fds are owned.
    if unsafe {
        libc::renameat2(
            old_dir.as_raw_fd(),
            old.as_ptr(),
            new_dir.as_raw_fd(),
            new.as_ptr(),
            flags as libc::c_uint,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn linkat(path_fd: &OwnedFd, dir: &OwnedFd, name: &[u8]) -> io::Result<()> {
    let cname =
        CString::new(name.to_vec()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cname` is NUL-terminated; both fds are owned; AT_EMPTY_PATH
    // names the file the descriptor points at.
    if unsafe {
        libc::linkat(
            path_fd.as_raw_fd(),
            c"".as_ptr(),
            dir.as_raw_fd(),
            cname.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Change the held inode, never a saved name that may have been replaced.
fn fchmodat(fd: &OwnedFd, mode: libc::mode_t) -> io::Result<()> {
    match kind_of(fstat(fd.as_raw_fd())?.st_mode) {
        FileKind::File | FileKind::Dir => {}
        FileKind::Symlink => return Err(io::Error::from_raw_os_error(libc::ELOOP)),
        FileKind::Other => return Err(io::Error::from_raw_os_error(libc::EPERM)),
    }
    // Unlike opening for read, chmod through the held fd also works for
    // an owner restoring permissions on a mode-000 file. Never allow a
    // symlink here: chmod would follow its target.
    let path = CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
    // SAFETY: the path is NUL-terminated and the owned fd remains live.
    if unsafe { libc::chmod(path.as_ptr(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A NUL-terminated copy of one component.
fn cname(name: &[u8]) -> CString {
    CString::new(name.to_vec()).expect("a component has no NUL")
}

fn fchownat(fd: &OwnedFd, valid: SetattrValid, attr: &stat64) -> io::Result<()> {
    // -1 leaves a field unchanged; uid_t and gid_t are unsigned on Linux.
    let uid = if valid.contains(SetattrValid::UID) {
        attr.st_uid
    } else {
        u32::MAX
    };
    let gid = if valid.contains(SetattrValid::GID) {
        attr.st_gid
    } else {
        u32::MAX
    };
    // SAFETY: the empty path names the owned O_PATH fd, including symlinks.
    if unsafe {
        libc::fchownat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            uid,
            gid,
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Truncate through a write-mode reopen: O_PATH handles cannot carry
/// ftruncate(2).
fn truncateat(fd: &OwnedFd, size: i64) -> io::Result<()> {
    if kind_of(fstat(fd.as_raw_fd())?.st_mode) != FileKind::File {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let file = reopen(fd, libc::O_WRONLY)?;
    // SAFETY: the fd is owned.
    if unsafe { libc::ftruncate(file.as_raw_fd(), size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn utimensat(fd: &OwnedFd, valid: SetattrValid, attr: &stat64) -> io::Result<()> {
    let omit = libc::UTIME_OMIT;
    let times = [
        if valid.contains(SetattrValid::ATIME) {
            libc::timespec {
                tv_sec: attr.st_atime,
                tv_nsec: attr.st_atime_nsec,
            }
        } else {
            libc::timespec {
                tv_sec: 0,
                tv_nsec: omit,
            }
        },
        if valid.contains(SetattrValid::MTIME) {
            libc::timespec {
                tv_sec: attr.st_mtime,
                tv_nsec: attr.st_mtime_nsec,
            }
        } else {
            libc::timespec {
                tv_sec: 0,
                tv_nsec: omit,
            }
        },
    ];
    // SAFETY: the empty path names the owned O_PATH fd and times outlives the call.
    if unsafe {
        libc::utimensat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            times.as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fsync(fd: RawFd, datasync: bool) -> io::Result<()> {
    // SAFETY: the fd is owned.
    if unsafe {
        if datasync {
            libc::fdatasync(fd)
        } else {
            libc::fsync(fd)
        }
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read one batch of a directory stream with a single getdents64 of at
/// most `budget` bytes (kept within `DIR_BATCH_MIN..=DIR_BATCH_MAX`),
/// skipping `.` and `..` (the guest VFS synthesises them). Returns the
/// entries and the cookie of the last record read, or `None` at the end.
fn read_dir_batch(dir: &File, budget: usize) -> io::Result<Option<(Vec<HostDirent>, u64)>> {
    let mut buffer = vec![0u8; budget.clamp(DIR_BATCH_MIN, DIR_BATCH_MAX)];
    // SAFETY: `buffer` is writable for its length; the fd is owned.
    let n = unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            dir.as_raw_fd(),
            buffer.as_mut_ptr(),
            buffer.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Ok(None);
    }
    let mut entries = Vec::new();
    let mut last = 0u64;
    let mut pos = 0usize;
    while pos < n as usize {
        if pos + DIRENT_FIXED > n as usize {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        // Parse bytes rather than forming an aligned reference into a u8 buffer.
        let record = &buffer[pos..n as usize];
        let ino = u64::from_ne_bytes(record[..8].try_into().unwrap());
        // `d_off` is signed on the host and never negative.
        let cookie = u64::from_ne_bytes(record[8..16].try_into().unwrap());
        let reclen = u16::from_ne_bytes(record[16..18].try_into().unwrap()) as usize;
        if reclen < DIRENT_FIXED || pos + reclen > n as usize {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        let name = buffer[pos + DIRENT_FIXED..pos + reclen]
            .iter()
            .copied()
            .take_while(|b| *b != 0)
            .collect::<Vec<u8>>();
        if name != b"." && name != b".." {
            entries.push(HostDirent {
                name,
                host_ino: ino,
                d_type: record[18],
                cookie,
            });
        }
        last = cookie;
        pos += reclen;
    }
    Ok(Some((entries, last)))
}

/// offsetof(d_type) in `struct linux_dirent64`: 8 + 8 + 2 + 1.
const DIRENT_FIXED: usize = 19;

#[cfg(test)]
mod tests {
    use crate::virtio::fs::fuse::file_buf::FileVolatileSlice;
    use std::ffi::CString;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn ctx() -> Context {
        Context::new()
    }

    fn name(bytes: &[u8]) -> CString {
        CString::new(bytes.to_vec()).unwrap()
    }

    /// A writable share over a scratch directory.
    fn scratch() -> (tempfile::TempDir, PassthroughFs) {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"hello share\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/nested.txt"), b"nested\n").unwrap();
        std::os::unix::fs::symlink("hello.txt", dir.path().join("link")).unwrap();
        (dir, fs)
    }

    /// LOOKUP one name under the root.
    fn lookup(fs: &PassthroughFs, bytes: &[u8]) -> io::Result<Entry> {
        FileSystem::lookup(fs, &ctx(), ROOT_ID, &name(bytes))
    }

    #[test]
    fn lookup_serves_children_with_server_inodes() {
        let (_dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        assert!(entry.inode != ROOT_ID);
        // The server's inode number is what the guest sees in stat.
        assert_eq!(entry.attr.st_ino, entry.inode);
        assert!(entry.attr.st_mode & libc::S_IFMT == libc::S_IFREG);
        // Repeat lookups of one path share one inode.
        assert_eq!(lookup(&fs, b"hello.txt").unwrap().inode, entry.inode);
    }

    #[test]
    fn hostile_create_symlink_cannot_truncate_outside_share() {
        let (dir, fs) = scratch();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"private").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        let result = fs.create(
            &ctx(),
            ROOT_ID,
            c"escape",
            CreateIn {
                flags: (libc::O_RDWR | libc::O_TRUNC) as u32,
                mode: 0o600,
                ..Default::default()
            },
        );
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"private");
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ELOOP));
    }

    #[test]
    fn hostile_readonly_open_refuses_mutating_flags() {
        let (dir, _) = scratch();
        let fs = PassthroughFs::new(dir.path().to_path_buf(), true).unwrap();
        let ino = lookup(&fs, b"hello.txt").unwrap().inode;
        for flags in [libc::O_TRUNC, libc::O_APPEND, libc::O_CREAT] {
            let result = fs.open(&ctx(), ino, flags as u32, 0);
            assert_eq!(
                std::fs::read(dir.path().join("hello.txt")).unwrap(),
                b"hello share\n"
            );
            assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EROFS));
        }
    }

    #[test]
    fn hostile_setattr_swapped_name_changes_only_original_inode() {
        let (dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"private").unwrap();
        std::fs::rename(dir.path().join("hello.txt"), dir.path().join("original")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("hello.txt")).unwrap();
        let mut attr = entry.attr;
        attr.st_size = 2;
        attr.st_mode = 0o640;
        fs.setattr(
            &ctx(),
            entry.inode,
            attr,
            None,
            SetattrValid::SIZE | SetattrValid::MODE,
        )
        .unwrap();
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"private");
        assert_eq!(std::fs::read(dir.path().join("original")).unwrap(), b"he");
    }

    #[test]
    fn hostile_mknod_refuses_fifo_and_devices() {
        let (_dir, fs) = scratch();
        for mode in [libc::S_IFIFO, libc::S_IFCHR, libc::S_IFBLK] {
            assert_eq!(
                fs.mknod(&ctx(), ROOT_ID, c"special", mode | 0o600, 0, 0)
                    .unwrap_err()
                    .raw_os_error(),
                Some(libc::EPERM)
            );
        }
    }

    #[test]
    fn hostile_fifo_open_is_refused_even_with_nonblock() {
        let (dir, fs) = scratch();
        let root = open_path(dir.path(), libc::O_PATH).unwrap();
        mknodat(&root, b"fifo", libc::S_IFIFO | 0o600, 0).unwrap();
        let ino = lookup(&fs, b"fifo").unwrap().inode;
        // NONBLOCK keeps the pre-fix regression safe to run.
        assert_eq!(
            fs.open(&ctx(), ino, libc::O_NONBLOCK as u32, 0)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
    }

    #[test]
    fn hostile_inode_lookup_flood_is_bounded() {
        let (dir, fs) = scratch();
        for i in 0..32 {
            let n = format!("inode-{i}");
            std::fs::write(dir.path().join(&n), b"").unwrap();
            let result = lookup(&fs, n.as_bytes());
            if i == 31 {
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENFILE));
            } else {
                result.unwrap();
            }
        }
    }

    #[test]
    fn hostile_handle_open_flood_is_bounded() {
        let (_dir, fs) = scratch();
        let ino = lookup(&fs, b"hello.txt").unwrap().inode;
        for _ in 0..32 {
            fs.open(&ctx(), ino, 0, 0).unwrap();
        }
        assert_eq!(
            fs.open(&ctx(), ino, 0, 0).unwrap_err().raw_os_error(),
            Some(libc::ENFILE)
        );
        assert_eq!(
            fs.opendir(&ctx(), ROOT_ID, 0).unwrap_err().raw_os_error(),
            Some(libc::ENFILE)
        );
    }

    #[test]
    fn hostile_symlink_setattr_size_is_refused() {
        let (dir, fs) = scratch();
        let entry = lookup(&fs, b"link").unwrap();
        let mut attr = entry.attr;
        attr.st_size = 0;
        let result = fs.setattr(&ctx(), entry.inode, attr, None, SetattrValid::SIZE);
        assert_eq!(
            std::fs::read(dir.path().join("hello.txt")).unwrap(),
            b"hello share\n"
        );
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn hostile_open_flags_cannot_turn_data_handle_into_path_handle() {
        let (_dir, fs) = scratch();
        let ino = lookup(&fs, b"hello.txt").unwrap().inode;
        let (handle, _, _) = fs.open(&ctx(), ino, libc::O_PATH as u32, 0).unwrap();
        let file = fs.inner.lock().file_handle(ino, handle.unwrap()).unwrap();
        use std::os::unix::fs::FileExt;
        let mut data = [0; 5];
        file.read_exact_at(&mut data, 0).unwrap();
        assert_eq!(&data, b"hello");
    }

    #[test]
    fn handle_budget_is_reclaimed_only_by_matching_release() {
        let (_dir, fs) = scratch();
        let ino = lookup(&fs, b"hello.txt").unwrap().inode;
        let mut handles = Vec::new();
        for _ in 0..SESSION_LIMIT {
            handles.push(fs.open(&ctx(), ino, 0, 0).unwrap().0.unwrap());
        }
        fs.inner.lock().forget_handle(ROOT_ID, handles[0]);
        assert!(fs.inner.lock().file_handle(ino, handles[0]).is_ok());
        fs.forget(&ctx(), ino, 1);
        for handle in handles {
            fs.inner.lock().forget_handle(ino, handle);
        }
        assert!(!fs.inner.lock().inodes.contains_key(&ino));
        assert!(fs.inner.lock().handles_by_inode.is_empty());
        assert!(fs.opendir(&ctx(), ROOT_ID, 0).is_ok());
    }

    #[test]
    fn create_existing_regular_file_uses_vetted_inode() {
        let (dir, fs) = scratch();
        let (entry, handle, _, _) = fs
            .create(
                &ctx(),
                ROOT_ID,
                c"hello.txt",
                CreateIn {
                    flags: (libc::O_RDWR | libc::O_TRUNC) as u32,
                    mode: 0o600,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(entry.attr.st_size, 0);
        assert!(
            fs.inner
                .lock()
                .file_handle(entry.inode, handle.unwrap())
                .is_ok()
        );
        assert!(
            std::fs::read(dir.path().join("hello.txt"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn lookup_rejects_escapes_and_odd_names() {
        let (_dir, fs) = scratch();
        for bad in [&b".."[..], b".", b"", b"a/b", b"hello.txt/."] {
            let err = match lookup(&fs, bad) {
                Err(e) => e,
                Ok(_) => panic!("{bad:?} must not resolve"),
            };
            assert!(
                err.raw_os_error() == Some(libc::EINVAL)
                    || err.raw_os_error() == Some(libc::ENOENT),
                "{bad:?}: {err}"
            );
        }
        // A name that does not exist is a plain miss.
        assert_eq!(
            lookup(&fs, b"missing").unwrap_err().raw_os_error(),
            Some(libc::ENOENT)
        );
    }

    #[test]
    fn lookup_walks_directories_one_component_at_a_time() {
        let (_dir, fs) = scratch();
        let sub = lookup(&fs, b"sub").unwrap();
        let nested = FileSystem::lookup(&fs, &ctx(), sub.inode, &name(b"nested.txt")).unwrap();
        assert!(nested.attr.st_mode & libc::S_IFMT == libc::S_IFREG);
    }

    #[test]
    fn symlinks_are_served_not_followed() {
        let (_dir, fs) = scratch();
        let link = lookup(&fs, b"link").unwrap();
        assert!(link.attr.st_mode & libc::S_IFMT == libc::S_IFLNK);
        // The target is data, not a traversal: reading it returns the name.
        let target = FileSystem::readlink(&fs, &ctx(), link.inode).unwrap();
        assert_eq!(target, b"hello.txt");
        // Even a symlink pointing out of the share resolves as text; the
        // guest walks it inside its own tree.
        std::os::unix::fs::symlink("../../etc/passwd", _dir.path().join("escape")).unwrap();
        let escape = lookup(&fs, b"escape").unwrap();
        assert_eq!(
            FileSystem::readlink(&fs, &ctx(), escape.inode).unwrap(),
            b"../../etc/passwd"
        );
    }

    #[test]
    fn open_read_write_round_trip() {
        let (_dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        let (handle, _, _) =
            FileSystem::open(&fs, &ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
        let handle = handle.unwrap();

        // Read through the zero-copy path used on the queue: a memory
        // buffer standing in for the writable descriptors.
        let mut served = Vec::new();
        let mut writer = SinkWriter(&mut served);
        let n =
            FileSystem::read(&fs, &ctx(), entry.inode, handle, &mut writer, 5, 0, None, 0).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&served[..5], b"hello");

        // Write through the zero-copy reader path.
        let (handle, _, _) =
            FileSystem::open(&fs, &ctx(), entry.inode, libc::O_RDWR as u32, 0).unwrap();
        let handle = handle.unwrap();
        let data = b"HELLO";
        let mut reader = SliceReader(data);
        let n = FileSystem::write(
            &fs,
            &ctx(),
            entry.inode,
            handle,
            &mut reader,
            data.len() as u32,
            0,
            None,
            false,
            0,
            0,
        )
        .unwrap();
        assert_eq!(n, data.len());
        assert_eq!(
            std::fs::read(_dir.path().join("hello.txt")).unwrap(),
            b"HELLO share\n"
        );
    }

    #[test]
    fn append_uses_host_eof_instead_of_stale_guest_offset() {
        let (dir, fs) = scratch();
        fs.init(FsOptions::all() - FsOptions::WRITEBACK_CACHE)
            .unwrap();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        let guest_offset = entry.attr.st_size as u64;
        let (handle, _, _) = fs
            .open(
                &ctx(),
                entry.inode,
                (libc::O_WRONLY | libc::O_APPEND) as u32,
                0,
            )
            .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("hello.txt"))
            .unwrap()
            .write_all(b"host append\n")
            .unwrap();
        let data = b"guest append\n";
        let written = fs
            .write(
                &ctx(),
                entry.inode,
                handle.unwrap(),
                &mut SliceReader(data),
                data.len() as u32,
                guest_offset,
                None,
                false,
                0,
                0,
            )
            .unwrap();
        assert_eq!(written, data.len());
        assert_eq!(
            std::fs::read(dir.path().join("hello.txt")).unwrap(),
            b"hello share\nhost append\nguest append\n"
        );
    }

    #[test]
    fn append_handle_under_writeback_honours_the_write_offset() {
        let (dir, fs) = scratch();
        assert!(
            fs.init(FsOptions::all())
                .unwrap()
                .contains(FsOptions::WRITEBACK_CACHE)
        );
        let path = dir.path().join("append.txt");
        std::fs::File::create(&path).unwrap();
        let inode = lookup(&fs, b"append.txt").unwrap().inode;
        let (handle, _, _) = fs
            .open(&ctx(), inode, (libc::O_WRONLY | libc::O_APPEND) as u32, 0)
            .unwrap();
        let handle = handle.unwrap();

        for data in [b"a\n".as_slice(), b"a\nb\n".as_slice()] {
            let written = fs
                .write(
                    &ctx(),
                    inode,
                    handle,
                    &mut SliceReader(data),
                    data.len() as u32,
                    0,
                    None,
                    true,
                    0,
                    0,
                )
                .unwrap();
            assert_eq!(written, data.len());
        }
        assert_eq!(std::fs::read(path).unwrap(), b"a\nb\n");
    }

    #[test]
    fn restored_writeback_session_creates_handles_without_host_append() {
        let (dir, fs) = scratch();
        fs.init(FsOptions::all()).unwrap();
        let restored = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        restored.restore_state(&fs.save_state()).unwrap();
        let (entry, handle, _, _) = restored
            .create(
                &ctx(),
                ROOT_ID,
                &name(b"append.txt"),
                CreateIn {
                    flags: (libc::O_WRONLY | libc::O_APPEND) as u32,
                    mode: 0o600,
                    ..Default::default()
                },
            )
            .unwrap();
        let file = restored
            .inner
            .lock()
            .file_handle(entry.inode, handle.unwrap())
            .unwrap();
        // SAFETY: F_GETFL takes no third argument and the fd is owned.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(flags & libc::O_APPEND, 0);
    }

    #[test]
    fn synchronous_open_flags_survive_open_and_restore() {
        let (_dir, fs) = scratch();
        let ino = lookup(&fs, b"hello.txt").unwrap().inode;
        for flag in [libc::O_SYNC, libc::O_DSYNC] {
            let (handle, _, _) = fs
                .open(&ctx(), ino, (libc::O_WRONLY | flag) as u32, 0)
                .unwrap();
            for restored in [false, true] {
                if restored {
                    fs.restore_state(&fs.save_state()).unwrap();
                }
                let file = fs.inner.lock().file_handle(ino, handle.unwrap()).unwrap();
                // SAFETY: F_GETFL takes no third argument and the fd is owned.
                let actual = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
                assert_ne!(actual, -1);
                assert_eq!(actual & flag, flag);
            }
        }
    }

    /// A `ZeroCopyWriter` into a buffer, for driving reads off the queue.
    struct SinkWriter<'a>(&'a mut Vec<u8>);

    impl std::io::Write for SinkWriter<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl ZeroCopyWriter for SinkWriter<'_> {
        fn write_from(
            &mut self,
            f: &mut dyn crate::virtio::fs::fuse::file_traits::FileReadWriteVolatile,
            count: usize,
            off: u64,
        ) -> io::Result<usize> {
            // Stand in for the writable descriptors: read the file into a
            // buffer a volatile slice points at.
            let mut buf = vec![0u8; count];
            // SAFETY: `buf` outlives the call; the length matches.
            let slice = unsafe { FileVolatileSlice::from_raw_ptr(buf.as_mut_ptr(), buf.len()) };
            let n = f.read_at_volatile(slice, off)?;
            self.0.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn available_bytes(&self) -> usize {
            usize::MAX
        }
    }

    /// A `ZeroCopyReader` over a buffer, for driving writes onto the queue.
    struct SliceReader<'a>(&'a [u8]);

    impl std::io::Read for SliceReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    impl ZeroCopyReader for SliceReader<'_> {
        fn read_to(
            &mut self,
            f: &mut dyn crate::virtio::fs::fuse::file_traits::FileReadWriteVolatile,
            count: usize,
            off: u64,
        ) -> io::Result<usize> {
            let want = count.min(self.0.len());
            let mut consumed = vec![0u8; want];
            std::io::Read::read_exact(self, &mut consumed)?;
            // Hand the file a volatile view of the bytes, as the queue would.
            // SAFETY: `consumed` outlives the call; the length matches.
            let slice = unsafe {
                FileVolatileSlice::from_raw_ptr(consumed.as_ptr().cast_mut(), consumed.len())
            };
            f.write_at_volatile(slice, off)?;
            Ok(consumed.len())
        }
    }

    #[test]
    fn create_unlink_and_mkdir() {
        let (dir, fs) = scratch();
        let (entry, handle, _, _) = FileSystem::create(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"new.txt"),
            CreateIn {
                flags: libc::O_RDWR as u32,
                mode: 0o644,
                fuse_flags: 0,
                umask: 0o022,
            },
        )
        .unwrap();
        assert!(handle.is_some());
        assert!(dir.path().join("new.txt").is_file());

        FileSystem::mkdir(&fs, &ctx(), ROOT_ID, &name(b"dir"), 0o755, 0o022).unwrap();
        assert!(dir.path().join("dir").is_dir());

        FileSystem::unlink(&fs, &ctx(), ROOT_ID, &name(b"new.txt")).unwrap();
        assert!(!dir.path().join("new.txt").exists());

        // The handle outlives the name: the guest keeps reading the file it
        // opened, the way an unlinked open file behaves.
        let (st, _) = FileSystem::getattr(&fs, &ctx(), entry.inode, handle).unwrap();
        assert!(st.st_mode & libc::S_IFMT == libc::S_IFREG);
    }

    #[test]
    fn readdir_serves_entries_with_cookies() {
        let (_dir, fs) = scratch();
        let (handle, _) = FileSystem::opendir(&fs, &ctx(), ROOT_ID, libc::O_RDONLY as u32).unwrap();
        let handle = handle.unwrap();
        let mut names = Vec::new();
        let mut offset = 0;
        loop {
            let mut batch: Vec<(u64, Vec<u8>)> = Vec::new();
            let before = names.len();
            FileSystem::readdir(&fs, &ctx(), ROOT_ID, handle, 128, offset, &mut |d| {
                batch.push((d.offset, d.name.to_vec()));
                Ok(d.name.len() + 24)
            })
            .unwrap();
            if batch.is_empty() && names.len() == before {
                break;
            }
            names.extend(batch.iter().map(|(_, n)| n.clone()));
            match batch.last() {
                Some((next, _)) => offset = *next,
                None => break,
            }
        }
        names.sort();
        assert_eq!(
            names,
            vec![b"hello.txt".to_vec(), b"link".to_vec(), b"sub".to_vec()]
        );
        FileSystem::releasedir(&fs, &ctx(), ROOT_ID, 0, handle).unwrap();
    }

    #[test]
    fn readdirplus_does_not_leak_the_entry_that_did_not_fit() {
        let (_dir, fs) = scratch();
        let (handle, _) = FileSystem::opendir(&fs, &ctx(), ROOT_ID, libc::O_RDONLY as u32).unwrap();
        let handle = handle.unwrap();
        // The reply buffer takes one entry and refuses the second.
        let mut offered: Vec<(Vec<u8>, u64)> = Vec::new();
        FileSystem::readdirplus(&fs, &ctx(), ROOT_ID, handle, 4096, 0, &mut |d, e| {
            offered.push((d.name.to_vec(), e.inode));
            Ok(if offered.len() == 1 { 160 } else { 0 })
        })
        .unwrap();
        assert_eq!(offered.len(), 2, "one entry served, one refused");
        let session = fs.inner.lock();
        let (taken, taken_ino) = &offered[0];
        let (refused, refused_ino) = &offered[1];
        // The served entry keeps the lookup the guest will FORGET.
        assert_eq!(
            session.by_path.get(Path::new(OsStr::from_bytes(taken))),
            Some(taken_ino)
        );
        assert_eq!(session.inodes[taken_ino].lookups, 1);
        // The guest never saw the refused one and sends no FORGET for it.
        assert!(
            !session
                .by_path
                .contains_key(Path::new(OsStr::from_bytes(refused))),
            "the entry that did not fit is still in the inode table"
        );
        assert!(!session.inodes.contains_key(refused_ino));
        assert_eq!(session.inodes.len(), 2, "the root and the served entry");
    }

    #[test]
    fn readdirplus_takes_back_its_lookups_when_the_request_fails() {
        let (_dir, fs) = scratch();
        let known = lookup(&fs, b"hello.txt").unwrap().inode;
        let (handle, _) = FileSystem::opendir(&fs, &ctx(), ROOT_ID, libc::O_RDONLY as u32).unwrap();
        let handle = handle.unwrap();
        // The reply is the error alone: the guest sees no entry of it.
        let mut offered = 0;
        let err = FileSystem::readdirplus(&fs, &ctx(), ROOT_ID, handle, 4096, 0, &mut |_, _| {
            offered += 1;
            if offered == 3 {
                Err(io::Error::from_raw_os_error(libc::EOVERFLOW))
            } else {
                Ok(160)
            }
        })
        .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EOVERFLOW));
        let session = fs.inner.lock();
        // Only what the guest looked up before the request stays.
        assert_eq!(session.inodes.len(), 2);
        assert_eq!(session.inodes[&known].lookups, 1);
    }

    #[test]
    fn readdirplus_reports_a_full_inode_table() {
        const FILES: usize = 40;
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        for n in 0..FILES {
            std::fs::write(dir.path().join(format!("f{n:02}")), []).unwrap();
        }
        let (handle, _) = FileSystem::opendir(&fs, &ctx(), ROOT_ID, libc::O_RDONLY as u32).unwrap();
        let handle = handle.unwrap();
        // List to the end the way the guest does, and never FORGET: either
        // every file is listed or the guest is told the table is full.
        let mut names = Vec::new();
        let mut offset = 0;
        let mut error = None;
        for _ in 0..=FILES {
            let mut last = None;
            let result =
                FileSystem::readdirplus(&fs, &ctx(), ROOT_ID, handle, 4096, offset, &mut |d, _| {
                    names.push(d.name.to_vec());
                    last = Some(d.offset);
                    Ok(d.name.len() + 152)
                });
            if let Err(e) = result {
                // A failed request served nothing.
                assert_eq!(last, None);
                error = Some(e.raw_os_error());
                break;
            }
            match last {
                Some(next) => offset = next,
                None => break,
            }
        }
        if error.is_none() {
            assert_eq!(
                names.len(),
                FILES,
                "a listing with no error leaves files out"
            );
        } else {
            assert_eq!(error, Some(Some(libc::ENFILE)));
            // What was listed is what the table holds beside the root.
            assert_eq!(names.len(), SESSION_LIMIT - 1);
            assert_eq!(fs.inner.lock().inodes.len(), SESSION_LIMIT);
        }
    }

    /// List one directory handle to its end through the cookies, the way
    /// the guest does: each request resumes at the last entry it took.
    fn list_all(fs: &PassthroughFs, handle: u64, size: u32) -> Vec<Vec<u8>> {
        let mut names = Vec::new();
        let mut offset = 0;
        loop {
            let mut last = None;
            FileSystem::readdir(fs, &ctx(), ROOT_ID, handle, size, offset, &mut |d| {
                names.push(d.name.to_vec());
                last = Some(d.offset);
                Ok(d.name.len() + 24)
            })
            .unwrap();
            match last {
                Some(next) => offset = next,
                None => return names,
            }
        }
    }

    #[test]
    fn directory_caches_are_bounded_across_handles() {
        const FILES: usize = 2000;
        let (dir, fs) = scratch();
        for n in 0..FILES {
            std::fs::write(dir.path().join(format!("f{n:04}")), []).unwrap();
        }
        // Every handle the share admits, each read one time from the start.
        let mut handles = Vec::new();
        for _ in 0..SESSION_LIMIT {
            let (handle, _) =
                FileSystem::opendir(&fs, &ctx(), ROOT_ID, libc::O_RDONLY as u32).unwrap();
            let handle = handle.unwrap();
            let mut served = 0;
            FileSystem::readdir(&fs, &ctx(), ROOT_ID, handle, 4096, 0, &mut |d| {
                served += 1;
                Ok(d.name.len() + 24)
            })
            .unwrap();
            assert!(served > 0);
            handles.push(handle);
        }
        // The budget: what the session holds between requests must not grow
        // with the number of handles. One listing's worth is the ceiling.
        let held = fs.inner.lock().held_dirents();
        assert!(
            held <= FILES,
            "{} handles hold {held} directory entries",
            handles.len()
        );
        // A bounded server still lists the whole directory on every handle.
        for handle in handles {
            let mut names = list_all(&fs, handle, 4096);
            names.sort();
            names.dedup();
            assert_eq!(names.len(), FILES + 3);
            FileSystem::releasedir(&fs, &ctx(), ROOT_ID, 0, handle).unwrap();
        }
    }

    #[test]
    fn readdir_handles_mixed_record_lengths() {
        let (dir, fs) = scratch();
        let expected = (1..=64).map(|n| "x".repeat(n)).collect::<Vec<_>>();
        for name in &expected {
            std::fs::write(dir.path().join(name), []).unwrap();
        }
        let (handle, _) = FileSystem::opendir(&fs, &ctx(), ROOT_ID, libc::O_RDONLY as u32).unwrap();
        let handle = handle.unwrap();
        let mut names = Vec::new();
        FileSystem::readdir(&fs, &ctx(), ROOT_ID, handle, 65536, 0, &mut |d| {
            names.push(d.name.to_vec());
            Ok(d.name.len() + 24)
        })
        .unwrap();
        for name in expected {
            assert!(names.contains(&name.into_bytes()));
        }
        FileSystem::releasedir(&fs, &ctx(), ROOT_ID, 0, handle).unwrap();
    }

    #[test]
    fn forget_reaps_inodes_but_handles_keep_them() {
        let (_dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        let (handle, _, _) =
            FileSystem::open(&fs, &ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
        // Burn every lookup; the open handle must keep the inode alive.
        FileSystem::forget(&fs, &ctx(), entry.inode, entry.inode);
        let (st, _) = FileSystem::getattr(&fs, &ctx(), entry.inode, handle).unwrap();
        assert_eq!(st.st_ino, entry.inode);
        // Releasing the handle lets the last reference go.
        FileSystem::release(
            &fs,
            &ctx(),
            entry.inode,
            0,
            handle.unwrap(),
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            FileSystem::getattr(&fs, &ctx(), entry.inode, None)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOENT)
        );
    }

    #[test]
    fn read_only_shares_refuse_every_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path().to_path_buf(), true).unwrap();
        std::fs::write(dir.path().join("file"), b"data").unwrap();
        let entry = lookup(&fs, b"file").unwrap();

        let err = FileSystem::open(&fs, &ctx(), entry.inode, libc::O_WRONLY as u32, 0).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EROFS));

        fn expect_erofs(result: io::Result<impl Sized>) {
            let Err(err) = result else {
                panic!("a read-only share must refuse every mutation");
            };
            assert_eq!(err.raw_os_error(), Some(libc::EROFS), "{err}");
        }
        expect_erofs(FileSystem::mkdir(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"d"),
            0o755,
            0,
        ));
        expect_erofs(FileSystem::unlink(&fs, &ctx(), ROOT_ID, &name(b"file")));
        expect_erofs(FileSystem::symlink(
            &fs,
            &ctx(),
            &name(b"x"),
            ROOT_ID,
            &name(b"l"),
        ));
        // SAFETY: stat64 is POD; zeroing is its init.
        let mut attr: stat64 = unsafe { std::mem::zeroed() };
        attr.st_size = 0;
        expect_erofs(FileSystem::setattr(
            &fs,
            &ctx(),
            entry.inode,
            attr,
            None,
            SetattrValid::SIZE,
        ));
        expect_erofs(FileSystem::create(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"c"),
            CreateIn {
                flags: libc::O_WRONLY as u32,
                mode: 0o644,
                fuse_flags: 0,
                umask: 0,
            },
        ));

        // Reads still work.
        let (handle, _, _) =
            FileSystem::open(&fs, &ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
        assert!(handle.is_some());
    }

    #[test]
    fn setattr_changes_size_and_times() {
        let (dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        let mut attr = FileSystem::getattr(&fs, &ctx(), entry.inode, None)
            .unwrap()
            .0;
        attr.st_size = 4;
        let (st, _) =
            FileSystem::setattr(&fs, &ctx(), entry.inode, attr, None, SetattrValid::SIZE).unwrap();
        assert_eq!(st.st_size, 4);
        assert_eq!(
            std::fs::read(dir.path().join("hello.txt")).unwrap(),
            b"hell"
        );
    }

    #[test]
    fn statfs_reports_the_share() {
        let (_dir, fs) = scratch();
        let st = FileSystem::statfs(&fs, &ctx(), ROOT_ID).unwrap();
        assert!(st.f_bsize > 0);
    }

    #[test]
    fn session_state_round_trips_by_path() {
        let (dir, fs) = scratch();
        // Open a file and a directory, leave inodes live.
        let hello = lookup(&fs, b"hello.txt").unwrap();
        let (file_handle, _, _) =
            FileSystem::open(&fs, &ctx(), hello.inode, libc::O_RDWR as u32, 0).unwrap();
        let sub = lookup(&fs, b"sub").unwrap();
        let (dir_handle, _) =
            FileSystem::opendir(&fs, &ctx(), sub.inode, libc::O_RDONLY as u32).unwrap();
        let nested = FileSystem::lookup(&fs, &ctx(), sub.inode, &name(b"nested.txt")).unwrap();
        let state = fs.save_state();

        // Restore into a fresh filesystem over the same directory: same
        // inodes, same handles, contents readable through both.
        let fs2 = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        fs2.restore_state(&state).unwrap();
        let mut served = Vec::new();
        let mut writer = SinkWriter(&mut served);
        FileSystem::read(
            &fs2,
            &ctx(),
            hello.inode,
            file_handle.unwrap(),
            &mut writer,
            u32::MAX,
            0,
            None,
            0,
        )
        .unwrap();
        assert_eq!(served, b"hello share\n");
        let (st, _) = FileSystem::getattr(&fs2, &ctx(), nested.inode, None).unwrap();
        assert!(st.st_mode & libc::S_IFMT == libc::S_IFREG);
        // The old server still works too: the two sessions are
        // independent copies of the same tree.
        assert!(dir_handle.is_some());
    }

    #[test]
    fn restore_marks_missing_paths_dead() {
        let (dir, fs) = scratch();
        let hello = lookup(&fs, b"hello.txt").unwrap();
        let state = fs.save_state();
        std::fs::remove_file(dir.path().join("hello.txt")).unwrap();

        let fs2 = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        fs2.restore_state(&state).unwrap();
        // The inode restored but answers ENOENT, like virtiofsd's
        // find-paths over a vanished file.
        assert_eq!(
            FileSystem::getattr(&fs2, &ctx(), hello.inode, None)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOENT)
        );
        // The root survives.
        let entry = lookup(&fs2, b"sub").unwrap();
        assert!(entry.attr.st_mode & libc::S_IFMT == libc::S_IFDIR);
    }

    #[test]
    fn restored_handle_on_a_vanished_file_fails_io() {
        let (dir, fs) = scratch();
        let entry = lookup(&fs, b"hello.txt").unwrap();
        let (handle, _, _) =
            FileSystem::open(&fs, &ctx(), entry.inode, libc::O_RDWR as u32, 0).unwrap();
        let handle = handle.unwrap();
        let state = fs.save_state();
        std::fs::remove_file(dir.path().join("hello.txt")).unwrap();

        let restored = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        restored.restore_state(&state).unwrap();
        let data = b"lost write";
        let write = FileSystem::write(
            &restored,
            &ctx(),
            entry.inode,
            handle,
            &mut SliceReader(data),
            data.len() as u32,
            0,
            None,
            false,
            0,
            0,
        );
        assert_eq!(
            write.map_err(|err| err.raw_os_error()),
            Err(Some(libc::ESTALE))
        );
        let mut served = Vec::new();
        let read = FileSystem::read(
            &restored,
            &ctx(),
            entry.inode,
            handle,
            &mut SinkWriter(&mut served),
            1,
            0,
            None,
            0,
        );
        assert_eq!(
            read.map_err(|err| err.raw_os_error()),
            Err(Some(libc::ESTALE))
        );
        let sync = FileSystem::fsync(&restored, &ctx(), entry.inode, false, handle);
        assert_eq!(
            sync.map_err(|err| err.raw_os_error()),
            Err(Some(libc::ESTALE))
        );
    }

    #[test]
    fn restore_refuses_tampered_state() {
        let (_dir, fs) = scratch();
        let state = fs.save_state();
        let fs2 =
            PassthroughFs::new(tempfile::tempdir().unwrap().path().to_path_buf(), false).unwrap();
        assert!(fs2.restore_state(&state[..state.len() - 1]).is_err());
        assert!(fs2.restore_state(&[0u8; 12]).is_err());
    }

    #[test]
    fn write_through_the_open_handle_persists() {
        let (dir, fs) = scratch();
        let (entry, handle, _, _) = FileSystem::create(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"log.txt"),
            CreateIn {
                flags: libc::O_WRONLY as u32,
                mode: 0o600,
                fuse_flags: 0,
                umask: 0,
            },
        )
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join("log.txt"))
            .unwrap();
        // The same bytes the queue would hand the write op.
        file.write_all(b"0123456789").unwrap();
        FileSystem::fsync(&fs, &ctx(), entry.inode, false, handle.unwrap()).unwrap();
        let mut attr = FileSystem::getattr(&fs, &ctx(), entry.inode, None)
            .unwrap()
            .0;
        attr.st_mode = 0o600;
        let (st, _) =
            FileSystem::setattr(&fs, &ctx(), entry.inode, attr, None, SetattrValid::MODE).unwrap();
        assert_eq!(st.st_mode & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(dir.path().join("log.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    /// Read the whole of inode `ino` through a fresh read-only handle.
    fn read_all(fs: &PassthroughFs, ino: u64) -> Vec<u8> {
        let (handle, _, _) = FileSystem::open(fs, &ctx(), ino, libc::O_RDONLY as u32, 0).unwrap();
        let mut served = Vec::new();
        let mut writer = SinkWriter(&mut served);
        let n = FileSystem::read(
            fs,
            &ctx(),
            ino,
            handle.unwrap(),
            &mut writer,
            64,
            0,
            None,
            0,
        )
        .unwrap();
        served.truncate(n);
        served
    }

    fn create_in() -> CreateIn {
        CreateIn {
            flags: libc::O_RDWR as u32,
            mode: 0o644,
            fuse_flags: 0,
            umask: 0o022,
        }
    }

    #[test]
    fn rename_then_create_at_old_name_keeps_the_renamed_inode() {
        let (dir, fs) = scratch();
        let moved = lookup(&fs, b"hello.txt").unwrap().inode;
        FileSystem::rename(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"hello.txt"),
            ROOT_ID,
            &name(b"moved"),
            0,
        )
        .unwrap();

        let (created, _, _, _) =
            FileSystem::create(&fs, &ctx(), ROOT_ID, &name(b"hello.txt"), create_in()).unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"new").unwrap();

        // Two files, two node ids: the one the guest held follows the file
        // to its new name, and the old name is a new inode.
        assert_ne!(created.inode, moved);
        assert_eq!(read_all(&fs, moved), b"hello share\n");
        assert_eq!(read_all(&fs, created.inode), b"new");
        assert_eq!(lookup(&fs, b"moved").unwrap().inode, moved);
        assert_eq!(lookup(&fs, b"hello.txt").unwrap().inode, created.inode);
    }

    #[test]
    fn rename_carries_a_directory_subtree_and_a_symlink() {
        let (dir, fs) = scratch();
        let sub = lookup(&fs, b"sub").unwrap().inode;
        let nested = FileSystem::lookup(&fs, &ctx(), sub, &name(b"nested.txt"))
            .unwrap()
            .inode;
        let link = lookup(&fs, b"link").unwrap().inode;
        for (old, new) in [(&b"sub"[..], &b"dir2"[..]), (b"link", b"link2")] {
            FileSystem::rename(&fs, &ctx(), ROOT_ID, &name(old), ROOT_ID, &name(new), 0).unwrap();
        }

        // The old names are taken again by other files.
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/nested.txt"), b"other\n").unwrap();
        std::os::unix::fs::symlink("elsewhere", dir.path().join("link")).unwrap();
        let sub2 = lookup(&fs, b"sub").unwrap().inode;
        assert_ne!(sub2, sub);
        let nested2 = FileSystem::lookup(&fs, &ctx(), sub2, &name(b"nested.txt"))
            .unwrap()
            .inode;
        assert_ne!(nested2, nested);
        assert_ne!(lookup(&fs, b"link").unwrap().inode, link);

        assert_eq!(lookup(&fs, b"dir2").unwrap().inode, sub);
        assert_eq!(
            FileSystem::lookup(&fs, &ctx(), sub, &name(b"nested.txt"))
                .unwrap()
                .inode,
            nested
        );
        assert_eq!(read_all(&fs, nested), b"nested\n");
        assert_eq!(read_all(&fs, nested2), b"other\n");
        // readlink goes by the parent descriptor and the own name.
        assert_eq!(
            FileSystem::readlink(&fs, &ctx(), link).unwrap(),
            b"hello.txt"
        );

        // The snapshot form follows the rename too.
        let state = fs.save_state();
        let fs2 = PassthroughFs::new(dir.path().to_path_buf(), false).unwrap();
        fs2.restore_state(&state).unwrap();
        assert_eq!(read_all(&fs2, nested), b"nested\n");
        assert_eq!(read_all(&fs2, nested2), b"other\n");
    }

    #[test]
    fn rename_over_a_known_file_and_exchange_keep_node_ids_with_their_files() {
        let (dir, fs) = scratch();
        std::fs::write(dir.path().join("target"), b"target\n").unwrap();
        let hello = lookup(&fs, b"hello.txt").unwrap().inode;
        let target = lookup(&fs, b"target").unwrap().inode;
        let sub = lookup(&fs, b"sub").unwrap().inode;
        let nested = FileSystem::lookup(&fs, &ctx(), sub, &name(b"nested.txt"))
            .unwrap()
            .inode;

        // Replace: the name now answers with the moved inode, and the
        // replaced one is left to its FORGET without taking the name along.
        FileSystem::rename(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"hello.txt"),
            ROOT_ID,
            &name(b"target"),
            0,
        )
        .unwrap();
        assert_eq!(lookup(&fs, b"target").unwrap().inode, hello);
        assert_eq!(read_all(&fs, hello), b"hello share\n");
        FileSystem::forget(&fs, &ctx(), target, 1);
        assert_eq!(lookup(&fs, b"target").unwrap().inode, hello);

        // Exchange a file with a directory: both subtrees swap.
        FileSystem::rename(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"target"),
            ROOT_ID,
            &name(b"sub"),
            libc::RENAME_EXCHANGE,
        )
        .unwrap();
        assert_eq!(lookup(&fs, b"sub").unwrap().inode, hello);
        assert_eq!(lookup(&fs, b"target").unwrap().inode, sub);
        assert_eq!(
            FileSystem::lookup(&fs, &ctx(), sub, &name(b"nested.txt"))
                .unwrap()
                .inode,
            nested
        );
        assert_eq!(read_all(&fs, hello), b"hello share\n");
    }

    #[test]
    fn rename_between_two_links_of_one_file_moves_nothing() {
        let (dir, fs) = scratch();
        std::fs::hard_link(dir.path().join("hello.txt"), dir.path().join("twin")).unwrap();
        let hello = lookup(&fs, b"hello.txt").unwrap().inode;
        let twin = lookup(&fs, b"twin").unwrap().inode;
        // rename(2) does nothing when both names are one file.
        FileSystem::rename(
            &fs,
            &ctx(),
            ROOT_ID,
            &name(b"hello.txt"),
            ROOT_ID,
            &name(b"twin"),
            0,
        )
        .unwrap();
        assert!(dir.path().join("hello.txt").exists());
        assert_eq!(lookup(&fs, b"hello.txt").unwrap().inode, hello);
        assert_eq!(lookup(&fs, b"twin").unwrap().inode, twin);
    }
}
