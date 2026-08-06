//! The recursive tree watch (§19.5): a coalescing **set** of dirty paths the
//! host drains, not a stream of events it consumes.
//!
//! What crosses the seam is a path plus its current stat, or a tombstone —
//! the same record the reconciliation stat-walk emits. Nothing platform-shaped
//! reaches the host: the backends below turn `inotify` and
//! `ReadDirectoryChangesW` into set membership and their kinds die here. A
//! compiler writing one file 400 times is one entry, and the host pays for
//! that coalescing with a single [`WatchRecord::Dirty`] nudge on the empty →
//! non-empty transition — so a build burst sends exactly one.
//!
//! The set is capped ([`DIRTY_SET_CAP`]) because a container micro-VM's dirty
//! set would otherwise be an unbounded allocation. Every way coverage can be
//! lost — the cap, a platform event queue overflowing, a subtree that vanished
//! without per-child events — collapses to the same [`WatchRecord::Rescan`],
//! and the host stat-walks. The one thing that does *not* degrade to a rescan
//! is the watch root itself vanishing: that fails the channel by name, so the
//! resulting halt can say *the workspace directory is gone* rather than *the
//! guest deleted 4 000 files*.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;

use vmlab_agent_proto::watch::{
    DIRTY_SET_CAP, EntryKind, RecordDecoder, Stat, StatRecord, WatchRecord, encode_record,
};
use vmlab_agent_proto::{AgentMsg, FrameKind, RecvWindow};

use crate::mux::{Credit, Input, Mux};

#[cfg(unix)]
mod inotify;
#[cfg(unix)]
use inotify as backend;

#[cfg(windows)]
mod rdcw;
#[cfg(windows)]
use rdcw as backend;

/// Open a watch on `root`, reporting paths relative to it. `prune` is the
/// host's list of root-relative directory prefixes to register no watcher
/// under (§19.6): registration is pruned, filtering stays host-side.
pub fn open(mux: &Mux, id: u32, root: String, prune: Vec<String>) {
    match std::fs::metadata(&root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            mux.send_error(Some(id), format!("watch root {root} is not a directory"));
            return;
        }
        Err(e) => {
            mux.send_error(Some(id), format!("watch root {root}: {e}"));
            return;
        }
    }

    let watch = Arc::new(Watch {
        mux: mux.clone(),
        id,
        root: PathBuf::from(&root),
        root_name: root,
        prune: prune.iter().map(|p| normalise(p)).collect(),
        set: DirtySet::default(),
        credit: Mutex::new(None),
        send_lock: Mutex::new(()),
        stopped: AtomicBool::new(false),
        cancel: Mutex::new(None),
    });

    // A prune entry that resolves to the root itself would leave a watch that
    // silently reports nothing — the failure class §19.6 refuses. Checked
    // here so both backends refuse it the same way.
    if watch.is_pruned("") {
        mux.send_error(
            Some(id),
            format!(
                "watch root {}: the prune list covers the root",
                watch.root_name
            ),
        );
        return;
    }

    let kill = {
        let watch = watch.clone();
        Box::new(move || watch.stop())
    };
    let Some((input, credit)) = mux.register(id, None, Some(kill)) else {
        return;
    };
    *watch.credit.lock().expect("credit lock") = Some(credit);

    // Registering the tree happens before `opened`, so a guest that cannot
    // watch (no inotify descriptors left, no directory handle) fails the open
    // rather than reporting an empty tree forever.
    if let Err(e) = backend::start(watch.clone()) {
        mux.remove_finished(id);
        mux.send_error(Some(id), format!("watch {}: {e}", watch.root_name));
        return;
    }
    mux.send_ctrl(&AgentMsg::Opened { id });

    thread::spawn(move || drain_loop(watch, input));
}

/// Serve host→agent records until the channel closes. `Drain` is the only
/// one the host sends; anything else means the stream is desynced, which is
/// unrecoverable inside a channel (there is no resync point) and so fails it.
///
/// This thread answers a drain inline, so it stops reading input while the
/// batch is on the wire. That is why the vocabulary allows **one drain
/// outstanding** and carries no request id to pipeline on: a host that sent
/// drains without reading the answers would fill this channel's input queue,
/// and the queue is what bounds it. Closing the channel unblocks it either
/// way — a dead host cannot wedge the watch, only a live one that never
/// reads.
fn drain_loop(watch: Arc<Watch>, input: std::sync::mpsc::Receiver<Input>) {
    let mut decoder = RecordDecoder::new();
    let mut window = RecvWindow::default();
    for msg in input {
        let Input::Bytes(bytes) = msg else { continue };
        if let Some(grant) = window.recv(bytes.len()) {
            watch.mux.send_ctrl(&AgentMsg::WindowAdjust {
                id: watch.id,
                bytes: grant,
            });
        }
        decoder.push(&bytes);
        loop {
            match decoder.next_record() {
                Ok(Some(WatchRecord::Drain)) => watch.drain(),
                Ok(Some(other)) => {
                    watch.fail(format!("host sent {other:?} on a watch channel"));
                    return;
                }
                Ok(None) => break,
                Err(e) => {
                    watch.fail(e);
                    return;
                }
            }
        }
    }
    // The input sender is gone: the host closed the channel (or the mux tore
    // it down). Stop watching.
    watch.stop();
}

/// One live watch channel. The backends mark paths on it; the drain loop
/// swaps the set out.
pub(crate) struct Watch {
    mux: Mux,
    id: u32,
    root: PathBuf,
    /// The root exactly as the host named it, for error text.
    root_name: String,
    prune: Vec<String>,
    set: DirtySet,
    credit: Mutex<Option<Arc<Credit>>>,
    /// Serialises record writes: the backend thread's `Dirty` nudge and the
    /// drain loop's batch would otherwise interleave mid-record.
    send_lock: Mutex<()>,
    stopped: AtomicBool,
    cancel: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Watch {
    /// The watch root, for a backend to register against.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `rel` is at or below a pruned prefix. Both the marking and the
    /// registration side ask this, so a pruned subtree is neither watched nor
    /// reported.
    pub(crate) fn is_pruned(&self, rel: &str) -> bool {
        self.prune
            .iter()
            .any(|p| rel == p || rel.starts_with(p) && rel.as_bytes().get(p.len()) == Some(&b'/'))
    }

    /// Record a path as dirty, nudging the host on the empty → non-empty
    /// transition.
    pub(crate) fn mark(&self, rel: String) {
        if self.is_pruned(&rel) {
            return;
        }
        if self.set.mark(rel) {
            self.send(&WatchRecord::Dirty);
        }
    }

    /// Coverage was lost: the next drain answers `Rescan` whatever else is in
    /// the set.
    pub(crate) fn overflow(&self) {
        if self.set.overflow() {
            self.send(&WatchRecord::Dirty);
        }
    }

    /// The watch root itself is gone — fail by name rather than reporting the
    /// tree's contents as deleted.
    pub(crate) fn fail_root_gone(&self) {
        self.fail(format!("watch root {} is gone", self.root_name));
    }

    /// Fail the channel and stop watching.
    pub(crate) fn fail(&self, msg: String) {
        self.mux.send_error(Some(self.id), msg);
        self.mux.remove_finished(self.id);
        self.stop();
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Stop watching. Idempotent; runs the backend's canceller (Windows needs
    /// one to unblock a pending read, Linux's poll loop just notices).
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(cancel) = self.cancel.lock().expect("cancel lock").take() {
            cancel();
        }
    }

    /// Hand back the way to unblock the backend's reader. Only Windows needs
    /// one — `inotify` is polled with a timeout, so its loop notices on its
    /// own.
    #[cfg(windows)]
    pub(crate) fn set_cancel(&self, cancel: Box<dyn FnOnce() + Send>) {
        if self.stopped() {
            cancel();
            return;
        }
        *self.cancel.lock().expect("cancel lock") = Some(cancel);
    }

    /// Swap the set out and answer the host.
    fn drain(&self) {
        match self.set.swap() {
            Drained::Rescan => self.send(&WatchRecord::Rescan),
            Drained::Paths(paths) => {
                let entries = paths.iter().map(|p| stat_record(&self.root, p)).collect();
                self.send(&WatchRecord::Batch { entries });
            }
        }
    }

    /// Write one record to the channel, chunked into the credit window. A
    /// record is atomic on the wire; a closed channel drops it.
    fn send(&self, record: &WatchRecord) {
        let credit = self.credit.lock().expect("credit lock").clone();
        let Some(credit) = credit else { return };
        let bytes = encode_record(record);
        let _lock = self.send_lock.lock().expect("send lock");
        let mut off = 0;
        while off < bytes.len() {
            let take = credit.take(bytes.len() - off);
            if take == 0 {
                return; // channel closed under us
            }
            self.mux
                .send_data(FrameKind::Data, self.id, &bytes[off..off + take]);
            off += take;
        }
    }
}

/// The coalescing set. Holds paths, never events: a path created, modified
/// and deleted inside one drain window has no single kind, so per-event kinds
/// are not a rejected option but an incoherent one.
#[derive(Default)]
struct DirtySet {
    state: Mutex<SetState>,
}

#[derive(Default)]
struct SetState {
    paths: BTreeSet<String>,
    overflow: bool,
}

impl SetState {
    /// Whether a drain right now would report anything.
    fn has_content(&self) -> bool {
        self.overflow || !self.paths.is_empty()
    }
}

/// What one drain window held.
enum Drained {
    Paths(BTreeSet<String>),
    Rescan,
}

impl DirtySet {
    /// Add a path. Returns whether this was the empty → non-empty transition
    /// (the host's one nudge per drain window).
    fn mark(&self, rel: String) -> bool {
        let mut state = self.state.lock().expect("dirty set");
        let was_empty = !state.has_content();
        if state.overflow {
            return false; // already collapsed; paths are pointless work
        }
        if state.paths.len() >= DIRTY_SET_CAP && !state.paths.contains(&rel) {
            // The cap is the batch bound too, so overflow frees the paths
            // rather than holding a set the host will never receive.
            state.paths.clear();
            state.overflow = true;
        } else {
            state.paths.insert(rel);
        }
        was_empty
    }

    /// Collapse to a rescan. Returns whether it was the transition.
    fn overflow(&self) -> bool {
        let mut state = self.state.lock().expect("dirty set");
        let was_empty = !state.has_content();
        state.paths.clear();
        state.overflow = true;
        was_empty
    }

    /// Take the window's contents, leaving an empty set behind.
    fn swap(&self) -> Drained {
        let mut state = self.state.lock().expect("dirty set");
        if state.overflow {
            *state = SetState::default();
            return Drained::Rescan;
        }
        Drained::Paths(std::mem::take(&mut state.paths))
    }
}

/// The path's current state, or a tombstone. Every stat failure is a
/// tombstone: the agent runs as root/SYSTEM, so the realistic failures are
/// all forms of *gone* (removed, or a parent component replaced), and the
/// host's own stat-walk would reach the same conclusion.
fn stat_record(root: &Path, rel: &str) -> StatRecord {
    let Ok(meta) = std::fs::symlink_metadata(root.join(rel)) else {
        return StatRecord::tombstone(rel);
    };
    let kind = if meta.is_symlink() {
        EntryKind::Symlink
    } else if meta.is_dir() {
        EntryKind::Dir
    } else if meta.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    StatRecord {
        path: rel.to_string(),
        stat: Some(Stat {
            kind,
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
        }),
    }
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    let Ok(mtime) = meta.modified() else { return 0 };
    match mtime.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos().min(i64::MAX as u128) as i64,
        Err(e) => -(e.duration().as_nanos().min(i64::MAX as u128) as i64),
    }
}

/// A root-relative path in the one spelling the whole vocabulary uses:
/// `/`-separated, no leading or trailing separator.
fn normalise(rel: &str) -> String {
    rel.replace('\\', "/")
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Join a directory's relative path with a child name.
pub(crate) fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

// The tests drive the real backend against a real tree, and the build host
// is a Linux one.
#[cfg(all(test, unix))]
mod tests;
