//! The host-side watcher, and the debounce that governs when what it reports
//! may be read (PRD §19.6).
//!
//! **Per-path debounce is not a performance tweak.** Editors write a temp and
//! rename, compilers write in chunks, and without a quiet period the syncer
//! reads a file mid-write — so a path is only reconciled once it has stopped
//! moving. Everything still in flight is *deferred*, never guessed at.
//!
//! **Registration is per directory and pruned.** `inotify` costs one watch
//! descriptor per directory, `max_user_watches` defaults to 8192, and a
//! `node_modules` tree is routinely tens of thousands of directories — so
//! registering recursively is *silently incomplete*, which is the exact
//! failure class that disqualified the share transports. The watcher is handed
//! the directory list the scan already produced, which has every guest-owned
//! subtree pruned out of it by construction.
//!
//! **Everything that loses coverage collapses to one value.** A queue
//! overflow, a new directory nobody is watching yet, a watch that could not be
//! placed: all of them are [`HostEvent::Rescan`], and the syncer answers every
//! one of them the same way — walk the tree again. It never needs to know
//! which fired.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};
use tokio::sync::mpsc;

use super::ignore::join_rel;
use crate::sync::LockRecover;

/// How long a path must be still before it is read. Long enough to cover an
/// editor's write-temp-then-rename and a compiler's chunked write, short
/// enough that a save feels immediate guest-side.
pub const QUIET: Duration = Duration::from_millis(250);

/// How long a poll waits before rechecking whether the watch was stopped.
const POLL_MS: u16 = 250;

/// What every directory watch asks for. Symlinks are entries to report, never
/// directories to descend, so a watch never follows one.
const FLAGS: AddWatchFlags = AddWatchFlags::IN_MODIFY
    .union(AddWatchFlags::IN_ATTRIB)
    .union(AddWatchFlags::IN_CREATE)
    .union(AddWatchFlags::IN_DELETE)
    .union(AddWatchFlags::IN_MOVED_FROM)
    .union(AddWatchFlags::IN_MOVED_TO)
    .union(AddWatchFlags::IN_DELETE_SELF)
    .union(AddWatchFlags::IN_MOVE_SELF)
    .union(AddWatchFlags::IN_CLOSE_WRITE)
    .union(AddWatchFlags::IN_ONLYDIR)
    .union(AddWatchFlags::IN_DONT_FOLLOW);

/// What the host watcher reports. Paths, never event kinds — the kinds
/// disagree between platforms and between one write and the next, and the
/// syncer decides from the ledger anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// Something happened at this root-relative path.
    Touched(String),
    /// Coverage was lost. Walk the tree again; it never matters which of the
    /// several causes fired.
    Rescan,
}

/// Which directory each watch descriptor is on, both ways round: the
/// registrar works in paths and the event pump works in descriptors.
#[derive(Default)]
struct Registry {
    by_dir: HashMap<String, WatchDescriptor>,
    by_wd: HashMap<WatchDescriptor, String>,
}

/// A running host-side watch over one workspace.
pub struct HostWatch {
    inotify: Arc<Inotify>,
    registry: Arc<std::sync::Mutex<Registry>>,
    stop: Arc<AtomicBool>,
    pub events: mpsc::UnboundedReceiver<HostEvent>,
}

impl HostWatch {
    /// Start the watcher, registering nothing yet — the first
    /// [`register`](HostWatch::register) does that from the scan's own
    /// directory list, so the watch and the ignore rules can never disagree.
    pub fn start() -> Result<HostWatch> {
        let inotify = Arc::new(
            Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)
                .context("starting the workspace watcher")?,
        );
        let (tx, events) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(std::sync::Mutex::new(Registry::default()));
        // A blocking read loop on its own thread: `inotify` has no async
        // surface worth the wrapper, and the tree walk that answers a rescan
        // is blocking anyway.
        {
            let (inotify, stop, registry) = (inotify.clone(), stop.clone(), registry.clone());
            std::thread::Builder::new()
                .name("vmlab-workspace-watch".into())
                .spawn(move || pump(inotify, stop, registry, tx))
                .context("starting the workspace watcher thread")?;
        }
        Ok(HostWatch {
            inotify,
            registry,
            stop,
            events,
        })
    }

    /// Bring the registered set in line with `dirs` — the directories the
    /// scan just walked, which already exclude everything guest-owned.
    ///
    /// A directory that cannot be registered is reported as a whole-tree
    /// rescan rather than passed over: a subtree that quietly stops being
    /// watched is the silent failure this design refuses.
    pub fn register(&self, root: &Path, dirs: &[String]) -> Vec<String> {
        let wanted: BTreeSet<&String> = dirs.iter().collect();
        let mut registry = self.registry.lock_recover();
        let stale: Vec<String> = registry
            .by_dir
            .keys()
            .filter(|dir| !wanted.contains(dir))
            .cloned()
            .collect();
        for dir in stale {
            if let Some(wd) = registry.by_dir.remove(&dir) {
                registry.by_wd.remove(&wd);
                let _ = self.inotify.rm_watch(wd);
            }
        }
        let mut failed = Vec::new();
        for dir in dirs {
            if registry.by_dir.contains_key(dir) {
                continue;
            }
            let path = if dir.is_empty() {
                root.to_path_buf()
            } else {
                root.join(dir)
            };
            match self.inotify.add_watch(&path, FLAGS) {
                Ok(wd) => {
                    registry.by_dir.insert(dir.clone(), wd);
                    registry.by_wd.insert(wd, dir.clone());
                }
                Err(e) => failed.push(format!("{}: {e}", path.display())),
            }
        }
        failed
    }

    /// How many directories are registered right now.
    #[cfg(test)]
    pub fn registered(&self) -> usize {
        self.registry.lock_recover().by_dir.len()
    }
}

impl Drop for HostWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Read events until the watch is dropped, turning each into a path relative
/// to the workspace root.
fn pump(
    inotify: Arc<Inotify>,
    stop: Arc<AtomicBool>,
    registry: Arc<std::sync::Mutex<Registry>>,
    tx: mpsc::UnboundedSender<HostEvent>,
) {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::os::fd::AsFd;

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let fd = inotify.as_fd();
        let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(POLL_MS)) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return,
        }
        let events = match inotify.read_events() {
            Ok(events) => events,
            Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return,
        };
        for event in events {
            use nix::sys::inotify::AddWatchFlags as F;
            // The queue overflowed: the kernel drops events whole-tree, so
            // nothing partial can be recovered from what did arrive.
            if event.mask.contains(F::IN_Q_OVERFLOW) {
                if tx.send(HostEvent::Rescan).is_err() {
                    return;
                }
                continue;
            }
            // A directory appearing is coverage this watch does not have yet;
            // the syncer answers a rescan by walking and re-registering, which
            // is the only thing that can place a watch inside it.
            if event.mask.contains(F::IN_ISDIR) && tx.send(HostEvent::Rescan).is_err() {
                return;
            }
            let dir = registry.lock_recover().by_wd.get(&event.wd).cloned();
            let Some(dir) = dir else {
                // A descriptor the registry no longer holds: the watch set
                // moved under this event. Coverage is in doubt, so say so
                // rather than dropping it.
                if tx.send(HostEvent::Rescan).is_err() {
                    return;
                }
                continue;
            };
            let rel = match event.name.as_ref().and_then(|n| n.to_str()) {
                Some(name) => join_rel(&dir, name),
                // An event about the watched directory itself.
                None => dir,
            };
            if tx.send(HostEvent::Touched(rel)).is_err() {
                return;
            }
        }
    }
}

/// Paths that have been touched and are not yet still enough to read.
///
/// Per path, not per tree: a burst under one subtree must not hold up a single
/// save elsewhere.
#[derive(Debug, Default)]
pub struct Debounce {
    quiet: Duration,
    seen: HashMap<String, Instant>,
}

impl Debounce {
    pub fn new(quiet: Duration) -> Debounce {
        Debounce {
            quiet,
            seen: HashMap::new(),
        }
    }

    /// Something happened at `path`: its quiet period starts again.
    pub fn touch(&mut self, path: String, now: Instant) {
        self.seen.insert(path, now);
    }

    /// The paths that have been still for the whole quiet period, removed
    /// from the pending set.
    pub fn settled(&mut self, now: Instant) -> Vec<String> {
        let settled: Vec<String> = self
            .seen
            .iter()
            .filter(|(_, seen)| now.duration_since(**seen) >= self.quiet)
            .map(|(path, _)| path.clone())
            .collect();
        for path in &settled {
            self.seen.remove(path);
        }
        settled
    }

    /// What is still moving — deferred rather than read, and handed to the
    /// reconciliation as undecided so nothing torn is ever propagated.
    pub fn in_flight(&self) -> BTreeSet<String> {
        self.seen.keys().cloned().collect()
    }

    /// How long until the next path settles, if anything is pending.
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        self.seen
            .values()
            .map(|seen| self.quiet.saturating_sub(now.duration_since(*seen)))
            .min()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// A path is read only once it has stopped moving. A compiler writing in
    /// chunks keeps pushing its own deadline out, which is the whole point:
    /// a partial read would write a torn version across the seam.
    #[test]
    fn a_path_settles_only_after_it_goes_quiet() {
        let base = Instant::now();
        let mut debounce = Debounce::new(Duration::from_millis(100));
        debounce.touch("a.txt".into(), base);
        assert!(debounce.settled(at(base, 50)).is_empty());
        debounce.touch("a.txt".into(), at(base, 60));
        assert!(debounce.settled(at(base, 120)).is_empty(), "still moving");
        assert_eq!(debounce.settled(at(base, 161)), vec!["a.txt".to_string()]);
        assert!(debounce.is_empty());
    }

    /// Per path, not per tree: a burst under one subtree must not starve a
    /// single save somewhere else.
    #[test]
    fn one_busy_path_does_not_hold_up_another() {
        let base = Instant::now();
        let mut debounce = Debounce::new(Duration::from_millis(100));
        debounce.touch("quiet.rs".into(), base);
        for tick in [0, 40, 80, 120] {
            debounce.touch("busy.log".into(), at(base, tick));
        }
        assert_eq!(
            debounce.settled(at(base, 130)),
            vec!["quiet.rs".to_string()]
        );
        assert_eq!(
            debounce.in_flight(),
            BTreeSet::from(["busy.log".to_string()])
        );
    }

    /// What is still in flight is handed to the reconciliation as undecided,
    /// so a file being written is deferred rather than read.
    #[test]
    fn what_is_still_moving_is_reported_as_in_flight() {
        let base = Instant::now();
        let mut debounce = Debounce::new(Duration::from_millis(100));
        debounce.touch("a.txt".into(), base);
        assert_eq!(debounce.in_flight(), BTreeSet::from(["a.txt".to_string()]));
        assert_eq!(debounce.next_wake(base), Some(Duration::from_millis(100)));
        assert_eq!(
            debounce.next_wake(at(base, 60)),
            Some(Duration::from_millis(40))
        );
        assert_eq!(debounce.next_wake(at(base, 200)), Some(Duration::ZERO));
        debounce.settled(at(base, 200));
        assert_eq!(debounce.next_wake(at(base, 200)), None);
    }

    /// The watch reports a real host-side edit as a path, and registration
    /// covers what the scan handed it.
    #[tokio::test]
    async fn a_host_side_edit_arrives_as_a_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let mut watch = HostWatch::start().unwrap();
        assert!(
            watch
                .register(dir.path(), &["".to_string(), "src".to_string()])
                .is_empty()
        );
        assert_eq!(watch.registered(), 2);

        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        let mut touched = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, watch.events.recv()).await {
                Ok(Some(HostEvent::Touched(path))) => {
                    touched.push(path.clone());
                    if path == "src/main.rs" {
                        break;
                    }
                }
                Ok(Some(HostEvent::Rescan)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(touched.contains(&"src/main.rs".to_string()), "{touched:?}");
    }

    /// A guest-owned tree costs no watch descriptors, because the watcher is
    /// handed the scan's already-pruned list and never walks for itself.
    #[tokio::test]
    async fn registration_drops_directories_that_left_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        let watch = HostWatch::start().unwrap();
        watch.register(dir.path(), &["".to_string(), "node_modules".to_string()]);
        assert_eq!(watch.registered(), 2);
        watch.register(dir.path(), &["".to_string()]);
        assert_eq!(watch.registered(), 1);
    }

    /// A directory that cannot be watched is named rather than passed over.
    #[tokio::test]
    async fn an_unwatchable_directory_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let watch = HostWatch::start().unwrap();
        let failed = watch.register(dir.path(), &["gone".to_string()]);
        assert_eq!(failed.len(), 1);
        assert!(failed[0].contains("gone"), "{failed:?}");
    }
}
