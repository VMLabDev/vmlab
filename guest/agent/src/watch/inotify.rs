//! Linux backend: one `inotify` watch descriptor per directory.
//!
//! Registration is what the prune list buys. `inotify` costs a watch
//! descriptor per directory where `ReadDirectoryChangesW` is a single
//! recursive handle, `max_user_watches` defaults to 8192, and a
//! `node_modules` tree is routinely tens of thousands of directories — so an
//! unpruned registration is *silently incomplete*, the exact failure class
//! §19.6 refuses. Running out of descriptors therefore fails loudly here
//! instead: at open it fails the open, later it fails the channel, and both
//! name the limit.
//!
//! Nothing about the event kinds reaches the host; they are read here and
//! turned into set membership. Two cases need more than a mark:
//!
//! - a directory **created or moved in** has no events for what is already
//!   inside it, so its subtree is registered and marked in one walk;
//! - a directory **moved away** reports nothing for its children at all, and
//!   §19.5 rules out the host inferring that a directory tombstone implies
//!   them — so that collapses to a rescan, the same value every other lost
//!   coverage uses.
//!
//! Container micro-VMs watch the merged overlayfs mount. Events survive a
//! copy-up on any kernel that hashes overlay inodes for fsnotify, which the
//! Alpine `linux-virt` kernel vmlab pins is far past; a full VM's own kernel
//! being recent enough is a stated §19.4 precondition of a dev image.

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::thread;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor};

use super::{Watch, join_rel};

/// How long a poll waits before rechecking whether the channel closed.
const POLL_MS: u16 = 250;

/// What every directory watch asks for. Content and metadata changes, plus
/// the name-level events that keep the registry in step. Symlinks are entries
/// to report, never directories to descend, so watches never follow one.
const FLAGS: AddWatchFlags = AddWatchFlags::IN_MODIFY
    .union(AddWatchFlags::IN_ATTRIB)
    .union(AddWatchFlags::IN_CREATE)
    .union(AddWatchFlags::IN_DELETE)
    .union(AddWatchFlags::IN_MOVED_FROM)
    .union(AddWatchFlags::IN_MOVED_TO)
    .union(AddWatchFlags::IN_DELETE_SELF)
    .union(AddWatchFlags::IN_MOVE_SELF)
    .union(AddWatchFlags::IN_ONLYDIR)
    .union(AddWatchFlags::IN_DONT_FOLLOW);

/// Register the tree and start reading events. Returns once the root and
/// everything under it (minus the prune list) is watched, so an open that
/// succeeds is an open that sees the tree.
pub(super) fn start(watch: Arc<Watch>) -> Result<(), String> {
    let inotify =
        Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC).map_err(describe)?;
    let mut registry = Registry::default();
    registry.add_tree(&inotify, &watch, String::new(), false)?;
    let Some(root_wd) = registry.by_path.get("").copied() else {
        // The only way the root goes unregistered: it vanished mid-walk.
        return Err("the root vanished during registration".to_string());
    };
    // Nothing to cancel: the poll loop notices `stopped` within POLL_MS.
    thread::spawn(move || run(watch, inotify, registry, root_wd));
    Ok(())
}

fn run(watch: Arc<Watch>, inotify: Inotify, mut registry: Registry, root_wd: WatchDescriptor) {
    while !watch.stopped() {
        let mut fds = [PollFd::new(inotify.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(POLL_MS)) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => {}
            Err(e) => return watch.fail(format!("watch poll: {}", describe(e))),
        }
        let events = match inotify.read_events() {
            Ok(events) => events,
            Err(Errno::EAGAIN | Errno::EINTR) => continue,
            Err(e) => return watch.fail(format!("watch read: {}", describe(e))),
        };
        for event in events {
            if !handle(&watch, &inotify, &mut registry, root_wd, event) {
                return;
            }
        }
    }
}

/// Fold one event into the dirty set. Returns whether to keep watching.
fn handle(
    watch: &Arc<Watch>,
    inotify: &Inotify,
    registry: &mut Registry,
    root_wd: WatchDescriptor,
    event: InotifyEvent,
) -> bool {
    let mask = event.mask;
    if mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
        // Queue-wide, not per-path: the kernel cannot say what was lost.
        watch.overflow();
        return true;
    }
    let Some(dir) = registry.wds.get(&event.wd).cloned() else {
        return true; // an event for a watch we already dropped
    };
    if mask.intersects(
        AddWatchFlags::IN_DELETE_SELF
            .union(AddWatchFlags::IN_MOVE_SELF)
            .union(AddWatchFlags::IN_UNMOUNT),
    ) {
        if event.wd == root_wd {
            watch.fail_root_gone();
            return false;
        }
        // A subdirectory: its parent's own event names it.
        registry.forget_subtree(inotify, &dir);
        return true;
    }
    if mask.contains(AddWatchFlags::IN_IGNORED) {
        registry.forget_subtree(inotify, &dir);
        return true;
    }
    let Some(name) = event.name else { return true };
    // Non-UTF-8 names cannot cross a JSON seam at all; a lossy name is still
    // a path the host can stat and reconcile.
    let rel = join_rel(&dir, &name.to_string_lossy());
    watch.mark(rel.clone());
    if !mask.contains(AddWatchFlags::IN_ISDIR) {
        return true;
    }
    if mask.intersects(AddWatchFlags::IN_CREATE.union(AddWatchFlags::IN_MOVED_TO)) {
        // Whatever landed inside before the watch existed produced no event.
        if let Err(e) = registry.add_tree(inotify, watch, rel, true) {
            watch.fail(format!("watch {}: {e}", dir));
            return false;
        }
    } else if mask.contains(AddWatchFlags::IN_MOVED_FROM) {
        registry.forget_subtree(inotify, &rel);
        watch.overflow();
    } else if mask.contains(AddWatchFlags::IN_DELETE) {
        // `rmdir` needs an empty directory, so every child was deleted with
        // an event of its own first.
        registry.forget_subtree(inotify, &rel);
    }
    true
}

/// Which directory each watch descriptor is on, both ways round.
#[derive(Default)]
struct Registry {
    wds: HashMap<WatchDescriptor, String>,
    by_path: HashMap<String, WatchDescriptor>,
}

impl Registry {
    /// Watch `start` and every directory below it, skipping pruned prefixes.
    /// With `mark`, every entry found on the way is added to the dirty set —
    /// what a freshly appeared subtree needs and a first registration does
    /// not (a watch open is a stat-walk trigger anyway).
    fn add_tree(
        &mut self,
        inotify: &Inotify,
        watch: &Arc<Watch>,
        start: String,
        mark: bool,
    ) -> Result<(), String> {
        let mut stack = vec![start];
        while let Some(rel) = stack.pop() {
            if watch.is_pruned(&rel) {
                continue;
            }
            let full = watch.root().join(&rel);
            match inotify.add_watch(&full, FLAGS) {
                Ok(wd) => {
                    self.wds.insert(wd, rel.clone());
                    self.by_path.insert(rel.clone(), wd);
                }
                // Gone (or replaced by a file) between the event and here.
                Err(Errno::ENOENT | Errno::ENOTDIR) => continue,
                Err(e) => return Err(describe(e)),
            }
            let Ok(entries) = std::fs::read_dir(&full) else {
                continue;
            };
            for entry in entries.flatten() {
                let child = join_rel(&rel, &entry.file_name().to_string_lossy());
                if mark {
                    watch.mark(child.clone());
                }
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(child);
                }
            }
        }
        Ok(())
    }

    /// Drop `rel` and everything below it from the registry.
    fn forget_subtree(&mut self, inotify: &Inotify, rel: &str) {
        let gone: Vec<String> = self
            .by_path
            .keys()
            .filter(|p| {
                *p == rel || p.starts_with(rel) && p.as_bytes().get(rel.len()) == Some(&b'/')
            })
            .cloned()
            .collect();
        for path in gone {
            if let Some(wd) = self.by_path.remove(&path) {
                self.wds.remove(&wd);
                // Already invalid whenever the directory is what vanished.
                let _ = inotify.rm_watch(wd);
            }
        }
    }
}

/// Turn the errno into something a developer can act on — the watch-limit
/// case is the one that actually happens, and "No space left on device" is a
/// lie about what ran out.
fn describe(e: Errno) -> String {
    match e {
        Errno::ENOSPC => "out of inotify watches: raise fs.inotify.max_user_watches \
             in the guest, or ignore more of the tree"
            .to_string(),
        Errno::EMFILE => "out of inotify instances (fs.inotify.max_user_instances)".to_string(),
        other => other.to_string(),
    }
}
