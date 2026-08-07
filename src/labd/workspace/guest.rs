//! The guest half of the workspace, as the syncer needs it (PRD §19.6).
//!
//! One trait over the file RPC session, for two reasons. It is the **whole**
//! of what crosses the seam — every guest-side effect the syncer can have is
//! one of the calls below, so "what does the syncer do to a guest" is
//! answered by reading one page. And it is the seam the reconciliation's
//! decisions are executed through, which is what lets the rules that matter —
//! temp-then-rename, the ledger written only after the rename, resume by
//! re-transfer — be tested as behaviour rather than inspected as code.
//!
//! The session it runs over is opened with the machine's **default login**
//! (§19.2): the syncer is the one named exception to vmlab's machinery running
//! as the agent identity, because otherwise the developer owns none of their
//! own source tree.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

use crate::labd::vm_agent::{Attrs, DirEntry, FileOps, LinkKind, WatchReport, WatchSession};

/// Everything the syncer can do to a guest tree.
#[async_trait]
pub trait GuestFs: Send + Sync {
    /// What is at `path` itself, never followed. `None` where nothing is.
    async fn lstat(&self, path: &str) -> Result<Option<Attrs>>;
    /// Every entry in a directory, each with its own attributes and none of
    /// them followed. The stat-walk's one primitive: the guest reports what
    /// it holds and the **host** applies the ignore rules to the answer.
    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>>;
    /// A symlink's target string, verbatim and untranslated.
    async fn readlink(&self, path: &str) -> Result<String>;
    /// The guest's own SHA-256 of what is on its disk.
    async fn digest(&self, path: &str) -> Result<String>;
    /// Bring a guest file's bytes to `local`, verified against the guest's
    /// own digest of what it read.
    async fn pull(&self, remote: &str, local: &Path) -> Result<()>;
    /// Create a directory, treating an existing one as success.
    ///
    /// `case_sensitive` is §19.6's NTFS flag, which takes only while the
    /// directory is empty — so it rides the creation, per directory, and
    /// inheritance is never relied on.
    async fn mkdir(&self, path: &str, case_sensitive: bool) -> Result<()>;
    /// Create the workspace root, and any missing parent above it.
    ///
    /// The root is a directory the syncer creates, so it carries the flag like
    /// every other one — without it the files at the top of the tree land in
    /// the one directory nobody set it on. Its parents are not the workspace
    /// and get nothing.
    async fn mkdir_root(&self, path: &str, case_sensitive: bool) -> Result<()>;
    /// Send a host file's bytes to `remote`, verified against the guest's own
    /// digest of what landed.
    async fn push(&self, local: &Path, remote: &str) -> Result<()>;
    async fn symlink(&self, target: &str, link: &str, kind: LinkKind) -> Result<()>;
    /// Rename, overwriting the destination. The atomic half of every apply.
    async fn rename(&self, from: &str, to: &str) -> Result<()>;
    /// Remove a file or symlink; already-absent is success.
    async fn remove(&self, path: &str) -> Result<()>;
    /// Remove an empty directory; already-absent is success.
    async fn rmdir(&self, path: &str) -> Result<()>;
}

/// The other seam: the guest's own watcher, which is how a guest-side edit
/// reaches the host at all.
///
/// Separate from [`GuestFs`] because its lifetime is: a file session is
/// opened per pass and thrown away, where the watch outlives every pass —
/// closing it between them would guarantee a stat-walk on the next one, which
/// is the cost the dirty set exists to avoid.
#[async_trait]
pub trait GuestWatch: Send + Sync {
    /// Swap the guest's dirty set out. At most one drain is outstanding, and
    /// there is no ack for the answer: a dropped channel already means a
    /// stat-walk, so the loss self-heals through a path that has to exist.
    async fn drain(&self) -> Result<()>;
    /// The next thing the channel says. `None` once it is gone.
    async fn recv(&mut self) -> Option<WatchReport>;
}

#[async_trait]
impl GuestFs for FileOps {
    async fn lstat(&self, path: &str) -> Result<Option<Attrs>> {
        FileOps::lstat(self, path).await
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>> {
        FileOps::readdir(self, path).await
    }

    async fn readlink(&self, path: &str) -> Result<String> {
        FileOps::readlink(self, path).await
    }

    async fn digest(&self, path: &str) -> Result<String> {
        FileOps::digest(self, path).await.map(|(sha256, _)| sha256)
    }

    async fn pull(&self, remote: &str, local: &Path) -> Result<()> {
        FileOps::pull_to(self, remote, local).await.map(|_| ())
    }

    async fn mkdir(&self, path: &str, case_sensitive: bool) -> Result<()> {
        FileOps::mkdir(self, path, case_sensitive).await
    }

    async fn mkdir_root(&self, path: &str, case_sensitive: bool) -> Result<()> {
        // Deliberately not `mkdir_p` on the root itself: that would create the
        // leaf without the flag, in the one window NTFS would have taken it.
        if let Some(parent) = guest_parent(path) {
            FileOps::mkdir_p(self, parent).await?;
        }
        FileOps::mkdir(self, path, case_sensitive).await
    }

    async fn push(&self, local: &Path, remote: &str) -> Result<()> {
        FileOps::push(self, local, remote, None).await.map(|_| ())
    }

    async fn symlink(&self, target: &str, link: &str, kind: LinkKind) -> Result<()> {
        FileOps::symlink(self, target, link, kind).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        FileOps::rename(self, from, to).await
    }

    async fn remove(&self, path: &str) -> Result<()> {
        FileOps::remove(self, path).await
    }

    async fn rmdir(&self, path: &str) -> Result<()> {
        FileOps::rmdir(self, path).await
    }
}

/// The directory a guest path sits in, in whichever separator it came with.
/// `None` where there is nothing above it to create — a Unix root, a Windows
/// drive root, or a bare relative name.
fn guest_parent(path: &str) -> Option<&str> {
    let cut = path.rfind(['/', '\\'])?;
    let parent = &path[..cut];
    (!parent.is_empty() && !parent.ends_with(':')).then_some(parent)
}

/// A shared handle is a guest session too. Without this every holder of an
/// `Arc<FakeGuest>` (or of a pooled real session) would hand-write nine
/// forwarding methods to say nothing.
#[async_trait]
impl<T: GuestFs + ?Sized> GuestFs for std::sync::Arc<T> {
    async fn lstat(&self, path: &str) -> Result<Option<Attrs>> {
        (**self).lstat(path).await
    }
    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>> {
        (**self).readdir(path).await
    }
    async fn readlink(&self, path: &str) -> Result<String> {
        (**self).readlink(path).await
    }
    async fn digest(&self, path: &str) -> Result<String> {
        (**self).digest(path).await
    }
    async fn pull(&self, remote: &str, local: &Path) -> Result<()> {
        (**self).pull(remote, local).await
    }
    async fn mkdir(&self, path: &str, case_sensitive: bool) -> Result<()> {
        (**self).mkdir(path, case_sensitive).await
    }
    async fn mkdir_root(&self, path: &str, case_sensitive: bool) -> Result<()> {
        (**self).mkdir_root(path, case_sensitive).await
    }
    async fn push(&self, local: &Path, remote: &str) -> Result<()> {
        (**self).push(local, remote).await
    }
    async fn symlink(&self, target: &str, link: &str, kind: LinkKind) -> Result<()> {
        (**self).symlink(target, link, kind).await
    }
    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        (**self).rename(from, to).await
    }
    async fn remove(&self, path: &str) -> Result<()> {
        (**self).remove(path).await
    }
    async fn rmdir(&self, path: &str) -> Result<()> {
        (**self).rmdir(path).await
    }
}

#[async_trait]
impl GuestWatch for WatchSession {
    async fn drain(&self) -> Result<()> {
        WatchSession::drain(self).await
    }

    async fn recv(&mut self) -> Option<WatchReport> {
        WatchSession::recv(self).await
    }
}

/// An in-memory guest tree, for the tests that have to watch what the syncer
/// actually does to one.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow, bail};
    use vmlab_agent_proto::watch::{EntryKind, Stat, StatRecord};

    use crate::labd::vm_agent::{ErrorCode, FileOpsError};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Node {
        File(Vec<u8>),
        Dir,
        Symlink(String),
        /// A socket, FIFO or device node: the guest holds one, and the syncer
        /// must skip it by name rather than record it.
        Special,
    }

    #[derive(Debug, Default)]
    struct Guest {
        nodes: BTreeMap<String, (Node, i64)>,
        /// Every path the guest was asked to write to, in order — so a test
        /// can assert a rename happened *after* the bytes landed, and that a
        /// target was never written to directly.
        writes: Vec<String>,
        /// Paths whose next push fails, standing in for a dropped channel.
        fail_push: Vec<String>,
        /// Paths whose pull fails, the same thing in the other direction.
        fail_pull: Vec<String>,
        /// Paths the guest refuses to be read at all.
        unreadable: Vec<String>,
        /// This guest's filesystem folds case, like a default NTFS volume:
        /// two names differing only in case are one object — **except** in a
        /// directory that was created with §19.6's flag.
        folding: bool,
        /// Directories created with the flag. Per directory, because that is
        /// how NTFS carries it and because inheritance must not be relied on.
        case_sensitive: BTreeSet<String>,
        /// The guest will not take the flag at all: a filesystem with no
        /// concept of it, or a Windows build without the component.
        refuse_case_flag: bool,
        /// …or will not take it at these directories only, which is what the
        /// machine-wide probe cannot see coming.
        refuse_case_flag_at: Vec<String>,
        /// The guest cannot create symlinks — a non-elevated Windows login, or
        /// an image §19.4's precondition does not hold for.
        refuse_symlinks: bool,
        /// Commands run against this guest, in order — the Windows
        /// preconditions' one use of anything but the file seam.
        commands: Vec<Vec<String>>,
        /// How many of the next commands fail, standing in for a guest whose
        /// `provision {}` has not installed git yet.
        fail_runs: usize,
    }

    /// Which node a path names on this guest. On a folding guest a path whose
    /// directory does not carry the flag lands on whatever is already there
    /// under any casing; everywhere else the path is itself.
    fn resolve(guest: &Guest, path: &str) -> String {
        if !guest.folding {
            return path.to_string();
        }
        let parent = match path.rfind('/') {
            Some(cut) => &path[..cut],
            None => "",
        };
        if guest.case_sensitive.contains(parent) {
            return path.to_string();
        }
        guest
            .nodes
            .keys()
            .find(|held| held.eq_ignore_ascii_case(path))
            .cloned()
            .unwrap_or_else(|| path.to_string())
    }

    /// A node's wire kind and size, the same answer an `lstat` and a
    /// directory listing must give about it.
    fn describe(node: &Node) -> (EntryKind, u64) {
        match node {
            Node::File(bytes) => (EntryKind::File, bytes.len() as u64),
            Node::Dir => (EntryKind::Dir, 0),
            Node::Symlink(target) => (EntryKind::Symlink, target.len() as u64),
            Node::Special => (EntryKind::Other, 0),
        }
    }

    /// A fake guest filesystem. Paths are whatever the caller passes, so the
    /// tests see exactly the strings that would go on the wire.
    #[derive(Debug, Default)]
    pub struct FakeGuest {
        inner: Mutex<Guest>,
    }

    impl FakeGuest {
        pub fn new() -> FakeGuest {
            FakeGuest::default()
        }

        pub fn put(&self, path: &str, node: Node, mtime_ns: i64) {
            self.inner
                .lock()
                .expect("fake guest")
                .nodes
                .insert(path.to_string(), (node, mtime_ns));
        }

        pub fn file(&self, path: &str, body: &str, mtime_ns: i64) {
            self.put(path, Node::File(body.as_bytes().to_vec()), mtime_ns);
        }

        pub fn dir(&self, path: &str) {
            self.put(path, Node::Dir, 0);
        }

        pub fn get(&self, path: &str) -> Option<Node> {
            self.inner
                .lock()
                .expect("fake guest")
                .nodes
                .get(path)
                .map(|(n, _)| n.clone())
        }

        pub fn text(&self, path: &str) -> Option<String> {
            match self.get(path)? {
                Node::File(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                _ => None,
            }
        }

        pub fn paths(&self) -> Vec<String> {
            self.inner
                .lock()
                .expect("fake guest")
                .nodes
                .keys()
                .cloned()
                .collect()
        }

        pub fn writes(&self) -> Vec<String> {
            self.inner.lock().expect("fake guest").writes.clone()
        }

        /// The next push to `path` fails, as a dropped channel would.
        pub fn fail_push(&self, path: &str) {
            self.inner
                .lock()
                .expect("fake guest")
                .fail_push
                .push(path.to_string());
        }

        /// The next pull of `path` fails, as a dropped channel would.
        pub fn fail_pull(&self, path: &str) {
            self.inner
                .lock()
                .expect("fake guest")
                .fail_pull
                .push(path.to_string());
        }

        /// Reading `path` fails — the root-owned artefact a login cannot open.
        pub fn unreadable(&self, path: &str) {
            self.inner
                .lock()
                .expect("fake guest")
                .unreadable
                .push(path.to_string());
        }

        /// This guest's filesystem folds case, like a default NTFS volume.
        pub fn folding(&self) -> &FakeGuest {
            self.inner.lock().expect("fake guest").folding = true;
            self
        }

        /// …and will not take the case-sensitivity flag either.
        pub fn refuse_case_flag(&self) -> &FakeGuest {
            self.inner.lock().expect("fake guest").refuse_case_flag = true;
            self
        }

        /// …or will not take it at this one directory, which is the case the
        /// machine-wide probe cannot see coming.
        pub fn refuse_case_flag_at(&self, path: &str) -> &FakeGuest {
            self.inner
                .lock()
                .expect("fake guest")
                .refuse_case_flag_at
                .push(path.to_string());
            self
        }

        /// This guest cannot create symlinks.
        pub fn refuse_symlinks(&self) -> &FakeGuest {
            self.inner.lock().expect("fake guest").refuse_symlinks = true;
            self
        }

        /// Record a command run against this guest, answering whether it
        /// succeeded.
        pub fn ran(&self, argv: Vec<String>) -> bool {
            let mut guest = self.inner.lock().expect("fake guest");
            guest.commands.push(argv);
            match guest.fail_runs.checked_sub(1) {
                Some(left) => {
                    guest.fail_runs = left;
                    false
                }
                None => true,
            }
        }

        /// The next `n` commands fail — a guest git has not reached yet.
        pub fn fail_runs(&self, n: usize) -> &FakeGuest {
            self.inner.lock().expect("fake guest").fail_runs = n;
            self
        }

        /// Every command run against this guest, in order.
        pub fn commands(&self) -> Vec<Vec<String>> {
            self.inner.lock().expect("fake guest").commands.clone()
        }

        /// Whether the directory at `path` was created case-sensitive.
        pub fn is_case_sensitive(&self, path: &str) -> bool {
            self.inner
                .lock()
                .expect("fake guest")
                .case_sensitive
                .contains(path)
        }

        /// Remove a path, as a guest-side `rm` would.
        pub fn unlink(&self, path: &str) {
            let mut guest = self.inner.lock().expect("fake guest");
            let key = resolve(&guest, path);
            guest.nodes.remove(&key);
        }
    }

    /// The guest's watcher, as a test drives it: mark a path and the channel
    /// nudges, exactly as the agent's dirty set does on its empty →
    /// non-empty transition.
    ///
    /// Shared rather than owned by the session, because the thing a test
    /// wants to do — write a file guest-side and say the guest noticed — has
    /// to reach a watch the syncer is already holding.
    #[derive(Debug)]
    pub struct FakeWatcher {
        guest: Arc<FakeGuest>,
        root: String,
        state: Mutex<WatchState>,
    }

    #[derive(Debug, Default)]
    struct WatchState {
        dirty: BTreeSet<String>,
        /// Coverage was lost: the next drain answers `Rescan` whatever else
        /// is in the set, which is the agent's own rule.
        overflow: bool,
        outbox: VecDeque<WatchReport>,
        drains: usize,
        closed: bool,
    }

    impl FakeWatcher {
        pub fn new(guest: Arc<FakeGuest>, root: &str) -> Arc<FakeWatcher> {
            Arc::new(FakeWatcher {
                guest,
                root: root.to_string(),
                state: Mutex::new(WatchState::default()),
            })
        }

        /// The guest noticed something at `rel` — a path relative to the
        /// watch root, which is what crosses the seam.
        pub fn mark(&self, rel: &str) {
            let mut state = self.state.lock().expect("fake watch");
            let quiet = state.dirty.is_empty() && !state.overflow;
            state.dirty.insert(rel.to_string());
            if quiet {
                state.outbox.push_back(WatchReport::Dirty);
            }
        }

        /// Coverage was lost, whichever of the three ways.
        pub fn overflow(&self) {
            let mut state = self.state.lock().expect("fake watch");
            let quiet = state.dirty.is_empty() && !state.overflow;
            state.overflow = true;
            if quiet {
                state.outbox.push_back(WatchReport::Dirty);
            }
        }

        /// The channel died — an agent restart, a torn connection.
        pub fn fail(&self, why: &str) {
            self.state
                .lock()
                .expect("fake watch")
                .outbox
                .push_back(WatchReport::Error(why.to_string()));
        }

        pub fn drains(&self) -> usize {
            self.state.lock().expect("fake watch").drains
        }

        /// One session on this watcher.
        pub fn session(self: &Arc<Self>) -> Box<dyn GuestWatch> {
            Box::new(FakeWatch(self.clone()))
        }

        /// What the agent would report for one dirty path: its current stat,
        /// or a tombstone where it is gone.
        fn record(&self, rel: &str) -> StatRecord {
            let path = super::super::scan::join_guest(&self.root, rel);
            let guest = self.guest.inner.lock().expect("fake guest");
            match guest.nodes.get(&resolve(&guest, &path)) {
                Some((node, mtime_ns)) => {
                    let (kind, size) = describe(node);
                    StatRecord {
                        path: rel.to_string(),
                        stat: Some(Stat {
                            kind,
                            size,
                            mtime_ns: *mtime_ns,
                        }),
                    }
                }
                None => StatRecord::tombstone(rel),
            }
        }
    }

    struct FakeWatch(Arc<FakeWatcher>);

    #[async_trait]
    impl GuestWatch for FakeWatch {
        async fn drain(&self) -> Result<()> {
            let (paths, overflowed) = {
                let mut state = self.0.state.lock().expect("fake watch");
                state.drains += 1;
                let overflowed = std::mem::take(&mut state.overflow);
                (std::mem::take(&mut state.dirty), overflowed)
            };
            // The set is stat-ed at drain time, not at mark time: a path
            // written and deleted inside one window has one answer, and it is
            // the one the guest holds now.
            let report = if overflowed {
                WatchReport::Rescan
            } else {
                WatchReport::Batch(paths.iter().map(|rel| self.0.record(rel)).collect())
            };
            self.0
                .state
                .lock()
                .expect("fake watch")
                .outbox
                .push_back(report);
            Ok(())
        }

        async fn recv(&mut self) -> Option<WatchReport> {
            loop {
                {
                    let mut state = self.0.state.lock().expect("fake watch");
                    if let Some(report) = state.outbox.pop_front() {
                        return Some(report);
                    }
                    if state.closed {
                        return None;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    }

    #[async_trait]
    impl GuestFs for FakeGuest {
        async fn lstat(&self, path: &str) -> Result<Option<Attrs>> {
            let guest = self.inner.lock().expect("fake guest");
            if guest.unreadable.iter().any(|p| p == path) {
                bail!("permission denied: {path}");
            }
            let Some((node, mtime_ns)) = guest.nodes.get(&resolve(&guest, path)) else {
                return Ok(None);
            };
            let (kind, size) = describe(node);
            Ok(Some(Attrs {
                kind,
                size,
                mtime_ns: *mtime_ns,
                atime_ns: *mtime_ns,
                mode: Some(0o644),
            }))
        }

        async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>> {
            let guest = self.inner.lock().expect("fake guest");
            if guest.unreadable.iter().any(|p| p == path) {
                bail!("permission denied: {path}");
            }
            let dir = resolve(&guest, path);
            if !matches!(guest.nodes.get(&dir), Some((Node::Dir, _))) {
                bail!("not a directory: {path}");
            }
            let under = format!("{}/", dir.trim_end_matches('/'));
            let mut entries = Vec::new();
            for (child, (node, mtime_ns)) in guest.nodes.iter() {
                let Some(name) = child.strip_prefix(&under) else {
                    continue;
                };
                if name.contains('/') {
                    continue;
                }
                let (kind, size) = describe(node);
                entries.push(DirEntry {
                    name: name.to_string(),
                    attrs: Attrs {
                        kind,
                        size,
                        mtime_ns: *mtime_ns,
                        atime_ns: *mtime_ns,
                        mode: Some(0o644),
                    },
                });
            }
            Ok(entries)
        }

        async fn readlink(&self, path: &str) -> Result<String> {
            match self.get(path) {
                Some(Node::Symlink(target)) => Ok(target),
                _ => Err(anyhow!("not a symlink: {path}")),
            }
        }

        async fn pull(&self, remote: &str, local: &Path) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            if guest.unreadable.iter().any(|p| p == remote) {
                bail!("permission denied: {remote}");
            }
            if let Some(at) = guest.fail_pull.iter().position(|p| p == remote) {
                guest.fail_pull.remove(at);
                bail!("the channel dropped while pulling {remote}");
            }
            let key = resolve(&guest, remote);
            match guest.nodes.get(&key) {
                Some((Node::File(bytes), _)) => Ok(std::fs::write(local, bytes)?),
                _ => Err(anyhow!("no such file: {remote}")),
            }
        }

        async fn digest(&self, path: &str) -> Result<String> {
            use sha2::{Digest, Sha256};
            let guest = self.inner.lock().expect("fake guest");
            if guest.unreadable.iter().any(|p| p == path) {
                bail!("permission denied: {path}");
            }
            match guest.nodes.get(&resolve(&guest, path)) {
                Some((Node::File(bytes), _)) => Ok(hex::encode(Sha256::digest(bytes))),
                _ => Err(anyhow!("no such file: {path}")),
            }
        }

        async fn mkdir(&self, path: &str, case_sensitive: bool) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            guest.writes.push(path.to_string());
            let refused =
                guest.refuse_case_flag || guest.refuse_case_flag_at.iter().any(|p| p == path);
            if case_sensitive && refused {
                // As a real guest does: the directory is there and the flag
                // is not, which is why the host has to fall back rather than
                // assume either.
                let key = resolve(&guest, path);
                guest.nodes.entry(key).or_insert((Node::Dir, 0));
                bail!("this guest cannot make a directory case-sensitive: {path}");
            }
            let key = resolve(&guest, path);
            guest.nodes.entry(key).or_insert((Node::Dir, 0));
            if case_sensitive {
                guest.case_sensitive.insert(path.to_string());
            }
            Ok(())
        }

        async fn mkdir_root(&self, path: &str, case_sensitive: bool) -> Result<()> {
            GuestFs::mkdir(self, path, case_sensitive).await
        }

        async fn push(&self, local: &Path, remote: &str) -> Result<()> {
            let bytes = std::fs::read(local)?;
            let mut guest = self.inner.lock().expect("fake guest");
            guest.writes.push(remote.to_string());
            // What the real session does when the guest has no directory to
            // put it in: make the parents. The workspace root itself arrives
            // this way, since it is never an entry in the host's own tree.
            let mut prefix = String::new();
            for part in remote.trim_end_matches('/').split('/') {
                if prefix.is_empty() && part.is_empty() {
                    prefix.push('/');
                    continue;
                }
                if !prefix.is_empty() && !prefix.ends_with('/') {
                    prefix.push('/');
                }
                prefix.push_str(part);
                if prefix == remote {
                    break;
                }
                guest.nodes.entry(prefix.clone()).or_insert((Node::Dir, 0));
            }
            if let Some(at) = guest.fail_push.iter().position(|p| p == remote) {
                guest.fail_push.remove(at);
                bail!("the channel dropped while pushing {remote}");
            }
            let key = resolve(&guest, remote);
            guest.nodes.insert(key, (Node::File(bytes), 9));
            Ok(())
        }

        async fn symlink(&self, target: &str, link: &str, _kind: LinkKind) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            guest.writes.push(link.to_string());
            if guest.refuse_symlinks {
                bail!("a required privilege is not held by the client: {link}");
            }
            let key = resolve(&guest, link);
            if guest.nodes.contains_key(&key) {
                bail!("already exists: {link}");
            }
            guest
                .nodes
                .insert(key, (Node::Symlink(target.to_string()), 9));
            Ok(())
        }

        async fn rename(&self, from: &str, to: &str) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            let from = resolve(&guest, from);
            let Some(node) = guest.nodes.remove(&from) else {
                bail!("no such file: {from}");
            };
            let to = resolve(&guest, to);
            guest.nodes.insert(to, node);
            Ok(())
        }

        async fn remove(&self, path: &str) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            let key = resolve(&guest, path);
            guest.nodes.remove(&key);
            Ok(())
        }

        async fn rmdir(&self, path: &str) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            let path = resolve(&guest, path);
            let under = format!("{path}/");
            if guest.nodes.keys().any(|p| p.starts_with(&under)) {
                // What a real guest answers when the directory still holds
                // something — a guest-owned subtree the syncer never touches.
                return Err(anyhow::Error::new(FileOpsError {
                    code: ErrorCode::NotEmpty,
                    msg: format!("directory not empty: {path}"),
                }));
            }
            guest.nodes.remove(&path);
            guest.case_sensitive.remove(&path);
            Ok(())
        }
    }
}
