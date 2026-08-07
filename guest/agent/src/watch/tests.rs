//! Watch tests. The set semantics are exercised directly; everything else
//! runs the real backend against a real tree on the build host, through the
//! same dispatch a guest uses.

use std::time::{Duration, Instant};

use vmlab_agent_proto::watch::{EntryKind, RecordDecoder, StatRecord, WatchRecord, encode_record};
use vmlab_agent_proto::{AgentMsg, Frame, FrameKind, HostMsg};

use super::*;
use crate::mux::Input;
use crate::testutil::{Capture, capture_mux};

// --- the set ------------------------------------------------------------

/// Whether the set owes the host a nudge, and clear it — what the sender
/// thread does when it takes one.
fn took_nudge(set: &DirtySet) -> bool {
    let mut state = set.state.lock().unwrap();
    std::mem::replace(&mut state.nudge, false)
}

fn drain(set: &DirtySet) -> Drained {
    set.request_drain();
    match set.take_work() {
        Some(Work::Drain(drained)) => drained,
        _ => panic!("a requested drain is the sender thread's first work"),
    }
}

#[test]
fn one_path_written_many_times_is_one_entry() {
    let set = DirtySet::default();
    set.mark("src/main.rs".into());
    assert!(took_nudge(&set), "empty -> non-empty nudges");
    for _ in 0..400 {
        set.mark("src/main.rs".into());
    }
    set.mark("src/other.rs".into());
    assert!(!took_nudge(&set), "no second nudge inside one drain window");
    let Drained::Paths(paths) = drain(&set) else {
        panic!("expected paths")
    };
    assert_eq!(paths.len(), 2);
    // The window is over: the next path nudges again.
    set.mark("src/main.rs".into());
    assert!(took_nudge(&set));
}

#[test]
fn the_cap_collapses_the_batch_to_a_rescan_and_frees_the_paths() {
    let set = DirtySet::default();
    for i in 0..DIRTY_SET_CAP + 10 {
        set.mark(format!("f{i}"));
    }
    assert!(
        set.state.lock().unwrap().paths.is_empty(),
        "an overflowed set holds no paths"
    );
    assert!(matches!(drain(&set), Drained::Rescan));
    // Draining clears it: the next window starts fresh.
    assert!(matches!(drain(&set), Drained::Paths(p) if p.is_empty()));
}

#[test]
fn overflow_on_an_empty_set_still_nudges_once() {
    let set = DirtySet::default();
    set.overflow();
    assert!(took_nudge(&set));
    set.overflow();
    set.mark("late.rs".into());
    assert!(!took_nudge(&set));
    assert!(matches!(drain(&set), Drained::Rescan));
}

/// A batch answers every mark made before the swap, so the nudge that
/// brought it is spent — but a mark that lands after the swap keeps its own,
/// or the host would never hear about it.
#[test]
fn a_drain_spends_the_nudge_it_answers_and_no_other() {
    let set = DirtySet::default();
    set.mark("early.rs".into());
    let Drained::Paths(paths) = drain(&set) else {
        panic!("expected paths")
    };
    assert_eq!(paths.len(), 1);
    assert!(!took_nudge(&set), "the batch carried it");
    set.mark("late.rs".into());
    assert!(took_nudge(&set));
}

#[test]
fn prune_matches_a_prefix_only_at_a_path_boundary() {
    let watch = pruned_watch(&["node_modules", "build/out"]);
    assert!(watch.is_pruned("node_modules"));
    assert!(watch.is_pruned("node_modules/a/b.js"));
    assert!(watch.is_pruned("build/out/x"));
    assert!(!watch.is_pruned("node_modules_two/a.js"));
    assert!(!watch.is_pruned("build"));
    assert!(!watch.is_pruned("src/node_modules"));
}

/// The host may spell a prune entry with separators either way round; one
/// spelling reaches the matcher.
#[test]
fn prune_entries_normalise_to_one_spelling() {
    let watch = pruned_watch(&["/node_modules/", "build\\out"]);
    assert!(watch.is_pruned("node_modules/a"));
    assert!(watch.is_pruned("build/out"));
}

fn pruned_watch(prune: &[&str]) -> Watch {
    let (mux, _cap) = capture_mux();
    Watch {
        mux,
        id: 1,
        root: PathBuf::from("/nowhere"),
        root_name: "/nowhere".into(),
        prune: prune.iter().map(|p| normalise(p)).collect(),
        set: DirtySet::default(),
        credit: Mutex::new(None),
        stopped: AtomicBool::new(false),
        cancel: Mutex::new(None),
    }
}

// --- the channel --------------------------------------------------------

/// Reads watch records off one channel, ignoring the control traffic that is
/// not this channel's.
struct Watcher {
    cap: Capture,
    channel: u32,
    decoder: RecordDecoder,
    pending: Vec<WatchRecord>,
}

impl Watcher {
    /// The next record, or `None` if the channel failed (with the error).
    fn record(&mut self) -> Result<WatchRecord, String> {
        loop {
            if !self.pending.is_empty() {
                return Ok(self.pending.remove(0));
            }
            let frame = self.cap.frame();
            match frame.kind {
                FrameKind::Ctrl => {
                    if let AgentMsg::Error { id, msg, .. } =
                        serde_json::from_slice::<AgentMsg>(&frame.payload).unwrap()
                        && id == Some(self.channel)
                    {
                        return Err(msg);
                    }
                }
                _ => {
                    assert_eq!(frame.channel, self.channel);
                    self.decoder.push(&frame.payload);
                    while let Some(r) = self.decoder.next_record().unwrap() {
                        self.pending.push(r);
                    }
                }
            }
        }
    }

    /// Drain, tolerating a `Dirty` nudge that crossed it on the wire.
    fn drain(&mut self, mux: &Mux) -> WatchRecord {
        mux.route_input(
            self.channel,
            Input::Bytes(encode_record(&WatchRecord::Drain)),
        );
        loop {
            match self.record().expect("channel alive") {
                WatchRecord::Dirty => continue,
                other => return other,
            }
        }
    }

    /// The batch a drain answers with, as (path, kind) pairs.
    fn batch(&mut self, mux: &Mux) -> Vec<StatRecord> {
        match self.drain(mux) {
            WatchRecord::Batch { entries } => entries,
            other => panic!("expected a batch, got {other:?}"),
        }
    }
}

fn open_watch(root: &std::path::Path, prune: Vec<String>) -> (Mux, Watcher) {
    let (mux, mut cap) = capture_mux();
    let msg = HostMsg::OpenWatch {
        id: 4,
        path: root.to_string_lossy().into_owned(),
        prune,
    };
    mux.handle_frame(
        Frame {
            kind: FrameKind::Ctrl,
            channel: 0,
            payload: serde_json::to_vec(&msg).unwrap(),
        },
        &TestPlatform,
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 4 });
    (
        mux,
        Watcher {
            cap,
            channel: 4,
            decoder: RecordDecoder::new(),
            pending: Vec::new(),
        },
    )
}

/// Dispatch needs a platform; nothing but `resolve_path` is reached here.
struct TestPlatform;

impl crate::mux::Platform for TestPlatform {
    fn os(&self) -> &'static str {
        "test"
    }
    fn features(&self) -> Vec<String> {
        vec![vmlab_agent_proto::features::WATCH.to_string()]
    }
    fn open_terminal(&self, _: &Mux, _: u32, _: u16, _: u16, _: Option<Vec<String>>) {}
    fn open_eventlog(&self, _: &Mux, _: u32, _: Option<String>) {}
    fn set_clipboard(&self, _: &Mux, _: String) {}
    fn get_clipboard(&self, _: &Mux) {}
    fn net_info(&self) -> Result<Vec<vmlab_agent_proto::NetInterface>, String> {
        Ok(vec![])
    }
    fn os_info(&self) -> Result<vmlab_agent_proto::OsInfo, String> {
        Err("unsupported".into())
    }
    fn shutdown(&self, _: &Mux, _: vmlab_agent_proto::ShutdownMode) {}
}

fn find<'a>(entries: &'a [StatRecord], path: &str) -> &'a StatRecord {
    entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{path} not in {entries:?}"))
}

#[test]
fn a_missing_root_fails_the_open_by_name() {
    let (mux, mut cap) = capture_mux();
    open(&mux, 3, "/nonexistent/workspace".into(), vec![]);
    match cap.ctrl() {
        AgentMsg::Error { id, msg, .. } => {
            assert_eq!(id, Some(3));
            assert!(msg.contains("/nonexistent/workspace"), "{msg}");
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn a_file_as_the_root_fails_the_open() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let (mux, mut cap) = capture_mux();
    open(&mux, 3, file.to_string_lossy().into_owned(), vec![]);
    match cap.ctrl() {
        AgentMsg::Error { msg, .. } => assert!(msg.contains("not a directory"), "{msg}"),
        other => panic!("expected an error, got {other:?}"),
    }
}

/// A prune entry that resolves to the root would leave a watch reporting
/// nothing at all — refused at the open rather than served silently.
#[test]
fn a_prune_list_covering_the_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (mux, mut cap) = capture_mux();
    open(
        &mux,
        3,
        dir.path().to_string_lossy().into_owned(),
        vec!["/".into()],
    );
    match cap.ctrl() {
        AgentMsg::Error { msg, .. } => assert!(msg.contains("covers the root"), "{msg}"),
        other => panic!("expected an error, got {other:?}"),
    }
}

/// The shape of the whole contract: one nudge, then a drain that answers with
/// stat records — never with event kinds.
#[test]
fn a_write_nudges_once_and_drains_as_a_stat_record() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let (mux, mut watcher) = open_watch(dir.path(), vec![]);

    std::fs::write(dir.path().join("src/main.rs"), b"fn main() {}").unwrap();
    for _ in 0..40 {
        std::fs::write(dir.path().join("src/main.rs"), b"fn main() { }").unwrap();
    }
    assert_eq!(watcher.record().unwrap(), WatchRecord::Dirty);

    let entries = watcher.batch(&mux);
    let record = find(&entries, "src/main.rs");
    let stat = record.stat.as_ref().expect("a live file, not a tombstone");
    assert_eq!(stat.kind, EntryKind::File);
    assert_eq!(stat.size, 13);
    assert!(stat.mtime_ns > 0);
}

#[test]
fn a_deleted_path_drains_as_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doomed.txt"), b"bye").unwrap();
    let (mux, mut watcher) = open_watch(dir.path(), vec![]);

    std::fs::remove_file(dir.path().join("doomed.txt")).unwrap();
    assert_eq!(watcher.record().unwrap(), WatchRecord::Dirty);
    let entries = watcher.batch(&mux);
    assert_eq!(find(&entries, "doomed.txt").stat, None);
}

/// A directory created after the open has to be watched too, and whatever
/// landed inside it before the watch existed still has to be reported.
#[test]
fn a_new_subtree_is_watched_and_its_contents_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (mux, mut watcher) = open_watch(dir.path(), vec![]);

    std::fs::create_dir_all(dir.path().join("pkg/deep")).unwrap();
    std::fs::write(dir.path().join("pkg/deep/new.rs"), b"x").unwrap();
    let entries = wait_for(&mux, &mut watcher, "pkg/deep/new.rs");
    assert_eq!(
        find(&entries, "pkg").stat.as_ref().unwrap().kind,
        EntryKind::Dir
    );

    // The new directory is genuinely watched: a later write in it reports.
    std::fs::write(dir.path().join("pkg/deep/second.rs"), b"y").unwrap();
    let entries = wait_for(&mux, &mut watcher, "pkg/deep/second.rs");
    assert!(find(&entries, "pkg/deep/second.rs").stat.is_some());
}

/// Drain until `path` shows up and return everything drained on the way: a
/// directory and what landed inside it can fall either side of a drain
/// window, and which one is nobody's business — the set only promises that
/// every changed path arrives.
fn wait_for(mux: &Mux, watcher: &mut Watcher, path: &str) -> Vec<StatRecord> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen: Vec<StatRecord> = Vec::new();
    loop {
        seen.extend(watcher.batch(mux));
        if seen.iter().any(|e| e.path == path) {
            return seen;
        }
        assert!(Instant::now() < deadline, "{path} never arrived");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A directory moved away takes its children's events with it, and the host
/// is not allowed to infer that a directory tombstone implies them — so it
/// collapses to the one overflow value.
#[test]
fn a_directory_moved_away_collapses_to_a_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let away = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("old/inner")).unwrap();
    std::fs::write(dir.path().join("old/inner/a.txt"), b"a").unwrap();
    let (mux, mut watcher) = open_watch(dir.path(), vec![]);

    std::fs::rename(dir.path().join("old"), away.path().join("moved")).unwrap();
    assert_eq!(watcher.record().unwrap(), WatchRecord::Dirty);
    assert_eq!(watcher.drain(&mux), WatchRecord::Rescan);
    // The window resets: the next drain is an ordinary batch again.
    assert!(matches!(watcher.drain(&mux), WatchRecord::Batch { .. }));
}

#[test]
fn a_pruned_subtree_gets_no_watcher() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let (mux, mut watcher) = open_watch(dir.path(), vec!["node_modules".into()]);

    std::fs::write(dir.path().join("node_modules/pkg/index.js"), b"noise").unwrap();
    std::fs::write(dir.path().join("src/watched.rs"), b"real").unwrap();

    // The pruned write produces nothing; the watched one produces the nudge.
    assert_eq!(watcher.record().unwrap(), WatchRecord::Dirty);
    let entries = watcher.batch(&mux);
    assert!(entries.iter().any(|e| e.path == "src/watched.rs"));
    assert!(
        !entries.iter().any(|e| e.path.starts_with("node_modules")),
        "pruned paths leaked: {entries:?}"
    );
}

/// Renaming a pruned tree — `mv node_modules node_modules.bak` — must not
/// cost a whole-tree rescan: the host asked for that subtree to be inert.
#[test]
fn renaming_a_pruned_directory_forces_no_rescan() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let (mux, mut watcher) = open_watch(dir.path(), vec!["node_modules".into()]);

    std::fs::rename(
        dir.path().join("node_modules"),
        dir.path().join("node_modules.bak"),
    )
    .unwrap();
    std::fs::write(dir.path().join("src/after.rs"), b"x").unwrap();

    let entries = wait_for(&mux, &mut watcher, "src/after.rs");
    assert!(
        !entries.iter().any(|e| e.path.starts_with("node_modules/")),
        "pruned paths leaked: {entries:?}"
    );
}

#[test]
fn the_root_vanishing_fails_the_channel_by_name() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let (_mux, mut watcher) = open_watch(&root, vec![]);

    std::fs::remove_dir(&root).unwrap();
    let err = watcher.record().expect_err("the channel must fail");
    assert!(err.contains("workspace"), "{err}");
    assert!(err.contains("gone"), "{err}");
}

/// Symlinks are entries to report, not directories to descend: a symlink loop
/// must not wedge the registration walk, and the link reports as a link.
#[test]
fn a_symlink_is_reported_as_one_and_never_followed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    std::os::unix::fs::symlink(dir.path(), dir.path().join("real/loop")).unwrap();
    let (mux, mut watcher) = open_watch(dir.path(), vec![]);

    std::fs::write(dir.path().join("real/file.txt"), b"x").unwrap();
    assert_eq!(watcher.record().unwrap(), WatchRecord::Dirty);
    let entries = watcher.batch(&mux);
    assert!(find(&entries, "real/file.txt").stat.is_some());

    std::os::unix::fs::symlink("file.txt", dir.path().join("real/link")).unwrap();
    let entries = wait_for(&mux, &mut watcher, "real/link");
    assert_eq!(
        find(&entries, "real/link").stat.as_ref().unwrap().kind,
        EntryKind::Symlink
    );
}

// --- overlayfs ----------------------------------------------------------

/// Marker the in-namespace half prints once the record has arrived.
const OVERLAY_OK: &str = "OVERLAY-COPY-UP-REPORTED";
/// Where the in-namespace half finds the merged mount.
const OVERLAY_ROOT: &str = "VMLAB_WATCH_OVERLAY_ROOT";

/// A container micro-VM's workspace lives on the merged overlayfs mount, and
/// the first write to a file still on the lower layer copies it up — the case
/// that used to break `inotify` silently, and the reason §19.4 makes a recent
/// guest kernel a precondition. Watching *directories* rather than file
/// inodes is what makes the watch survive it, and this runs the real thing
/// over a real overlay mount.
///
/// The mount needs a user + mount namespace. Where the environment refuses
/// one (Ubuntu's AppArmor policy restricts unprivileged user namespaces, so
/// CI lands here), the test says so and stops rather than passing quietly.
#[test]
fn a_copy_up_on_overlayfs_still_reports() {
    let dir = tempfile::tempdir().unwrap();
    for sub in ["lower/src", "upper", "work", "merged"] {
        std::fs::create_dir_all(dir.path().join(sub)).unwrap();
    }
    std::fs::write(dir.path().join("lower/src/lib.rs"), b"lower layer\n").unwrap();
    let base = dir.path().display();
    let mount = format!(
        "mount -t overlay overlay -o lowerdir={base}/lower,upperdir={base}/upper,\
         workdir={base}/work {base}/merged"
    );

    let exe = std::env::current_exe().unwrap();
    let output = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "sh", "-c"])
        .arg(format!("{mount} && exec \"$0\" \"$@\""))
        .arg(&exe)
        .args([
            "watch::tests::a_copy_up_child_reports_the_written_path",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env(OVERLAY_ROOT, dir.path().join("merged"))
        .output();

    let Ok(output) = output else {
        eprintln!("skipped: no `unshare` on this host");
        return;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if text.contains(OVERLAY_OK) {
        return;
    }
    // The mount failed before the watch ever ran: the environment, not the
    // watch. Anything that got as far as running the test is a real failure.
    if !text.contains("running 1 test") {
        eprintln!("skipped: this host allows no unprivileged overlay mount:\n{text}");
        return;
    }
    panic!("the watch missed an overlayfs copy-up:\n{text}");
}

/// The other half of [`a_copy_up_on_overlayfs_still_reports`], run inside the
/// namespace where the overlay is mounted. Ignored: its parent runs it.
#[test]
#[ignore]
fn a_copy_up_child_reports_the_written_path() {
    let Ok(root) = std::env::var(OVERLAY_ROOT) else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let (mux, mut watcher) = open_watch(&root, vec![]);

    // Appending to a file that only exists on the lower layer copies it up:
    // a new inode on the upper layer, under a directory watched on the
    // merged mount.
    std::fs::write(root.join("src/lib.rs"), b"copied up\n").unwrap();
    assert_eq!(watcher.record().unwrap(), WatchRecord::Dirty);
    let entries = wait_for(&mux, &mut watcher, "src/lib.rs");
    let stat = find(&entries, "src/lib.rs").stat.as_ref().unwrap();
    assert_eq!(stat.kind, EntryKind::File);
    assert_eq!(stat.size, 10);

    // And the watch is still live afterwards.
    std::fs::write(root.join("src/after.rs"), b"x").unwrap();
    wait_for(&mux, &mut watcher, "src/after.rs");
    println!("{OVERLAY_OK}");
}
