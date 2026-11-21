# Probes for the labd workspace sync review

These are the temporary tests behind `docs/reviews/labd-sync.md`. They are not
in the tree. Each one **passes while its fault is present**, so a fix card
turns the final assertions round and keeps the rest.

They were pasted at the end of `mod tests` in `src/labd/workspace/syncer.rs`
(they use that module's private helpers `seeded_lab`, `workspace`, `eventually`,
`restore_bracket` and `OneFake`) on commit `47dabad7`, and run on helios with:

```
cargo test --all-features --lib review380 -- --nocapture --test-threads=1
```

Result: `11 passed; 0 failed`. The names carry the probe numbers of the run,
which are not the finding codes. The map is:

| Probe | Finding |
|---|---|
| `review380_f1_…` | F1 |
| `review380_f2_…` | F2 |
| `review380_f2b_…` | F9 |
| `review380_f3_…` | F3 |
| `review380_f4_…` | F4 |
| `review380_f5_…` | F6 |
| `review380_f6_…` | F8 |
| `review380_f7_…` | F11 |
| `review380_f8_…` | F7 |
| `review380_f9_…` | F5 |
| `review380_f10_…` | F10 |

```rust
// ---- card 380 review probes: each PASSES when the fault is present ----

#[tokio::test]
async fn review380_f1_unreadable_guest_dir_on_overflow_walk_deletes_host_files() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("lib")).unwrap();
    std::fs::write(dir.path().join("lib/a.rs"), "a").unwrap();
    std::fs::write(dir.path().join("main.rs"), "m").unwrap();
    let (syncers, guest, sessions) =
        seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
    let syncer = syncers.get("dev01").await.unwrap();
    syncer.pass_now().await.unwrap();
    assert!(guest.get("/src/lib/a.rs").is_some());
    guest.unreadable("/src/lib");
    sessions.watcher.overflow();
    let gone = {
        let dir = dir.path().to_path_buf();
        eventually(move || !dir.join("lib/a.rs").exists()).await
    };
    println!("REVIEW380 F1 host file deleted: {gone}");
    println!("REVIEW380 F1 guest still holds it: {}", guest.get("/src/lib/a.rs").is_some());
    println!("REVIEW380 F1 halt: {:?}", syncer.report().halt.is_some());
    syncers.stop("dev01").await;
    assert!(gone, "fault not present: the host file survived");
    assert!(guest.get("/src/lib/a.rs").is_some());
}

#[tokio::test]
async fn review380_f2_guest_planted_ignore_symlink_discloses_a_host_file() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret");
    std::fs::write(&secret, "hunter2-token\nsecond/line\n").unwrap();
    std::fs::write(dir.path().join("main.rs"), "m").unwrap();
    let (syncers, guest, sessions) =
        seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
    let syncer = syncers.get("dev01").await.unwrap();
    syncer.pass_now().await.unwrap();
    guest.put(
        "/src/.vmlabignore",
        Node::Symlink(secret.display().to_string()),
        5,
    );
    sessions.watcher.mark(".vmlabignore");
    let leaked = {
        let sessions = sessions.clone();
        eventually(move || {
            sessions
                .opens()
                .iter()
                .any(|prune| prune.iter().any(|p| p == "hunter2-token"))
        })
        .await
    };
    println!("REVIEW380 F2 watch opens: {:?}", sessions.opens());
    syncers.stop("dev01").await;
    assert!(leaked, "fault not present: no host line reached the guest");
}

#[test]
fn review380_f2b_a_directory_named_like_a_rule_file_fails_every_scan() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "m").unwrap();
    std::fs::create_dir(dir.path().join(".gitignore")).unwrap();
    let ledger = Ledger::new(dir.path(), "/src");
    let scanned = host_scan(dir.path(), &ledger, 1 << 30);
    println!("REVIEW380 F2b scan: {:?}", scanned.as_ref().err().map(|e| format!("{e:#}")));
    assert!(scanned.is_err(), "fault not present");
}

#[tokio::test]
async fn review380_f3_restore_neither_flushes_nor_refuses_on_unsynced_guest_work() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "canonical").unwrap();
    let (syncers, guest, sessions) =
        seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
    let syncer = syncers.get("dev01").await.unwrap();
    syncer.pass_now().await.unwrap();
    let ws = workspace(dir.path(), state.path());
    guest.file("/src/main.rs", "an hour of guest work", 50);
    guest.file("/src/new.rs", "a new guest file", 51);
    sessions.watcher.mark("main.rs");
    sessions.watcher.mark("new.rs");
    let verdict = syncers.before_restore("dev01", false, Some(&ws)).await;
    println!("REVIEW380 F3 before_restore without the flag: {verdict:?}");
    // The restore itself rewinds nothing here: the guest keeps its work,
    // and only the bracket runs.
    restore_bracket(&syncers, &ws, || {}).await;
    let syncer = syncers.get("dev01").await.unwrap();
    assert!(eventually(|| syncer.report().passes > 0 && syncer.report().reseed.is_none()).await);
    let host = std::fs::read_to_string(dir.path().join("main.rs")).unwrap();
    println!("REVIEW380 F3 host main.rs: {host:?}");
    println!("REVIEW380 F3 guest main.rs: {:?}", guest.text("/src/main.rs"));
    println!("REVIEW380 F3 guest new.rs: {:?}", guest.get("/src/new.rs"));
    println!("REVIEW380 F3 host new.rs exists: {}", dir.path().join("new.rs").exists());
    syncers.stop("dev01").await;
    assert!(verdict.is_ok(), "fault not present: the restore was refused");
    assert_eq!(host, "canonical");
    assert_eq!(guest.text("/src/main.rs").as_deref(), Some("canonical"));
    assert!(guest.get("/src/new.rs").is_none());
    assert!(!dir.path().join("new.rs").exists());
}

#[tokio::test]
async fn review380_f4_a_socket_in_the_tree_keeps_the_re_seed_owed() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "canonical").unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(dir.path().join("dev.sock")).unwrap();
    let (syncers, _guest, sessions) =
        seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
    let syncer = syncers.get("dev01").await.unwrap();
    let before = syncer.pass_now().await.unwrap();
    println!(
        "REVIEW380 F4 steady state: halt={} trouble={:?} skipped={:?}",
        before.halt.is_some(),
        before.trouble,
        before.skipped
    );
    assert!(before.halt.is_none() && before.trouble.is_none());
    let ws = workspace(dir.path(), state.path());
    let opens = sessions.opens().len();
    restore_bracket(&syncers, &ws, || {}).await;
    let syncer = syncers.get("dev01").await.unwrap();
    assert!(eventually(|| syncer.report().passes > 0).await);
    // More than one retry period.
    tokio::time::sleep(Duration::from_secs(11)).await;
    let after = syncer.report();
    println!(
        "REVIEW380 F4 after restore: passes={} reseed={:?} watch reopened={}",
        after.passes,
        after.reseed.is_some(),
        sessions.opens().len() > opens
    );
    let owed = Ledger::load(&ws.ledger_path, &ws.host_root, &ws.guest_root).reseed_owed;
    syncers.stop("dev01").await;
    assert!(after.reseed.is_some(), "fault not present: the re-seed completed");
    assert!(after.passes >= 2);
    assert_eq!(sessions.opens().len(), opens);
    assert!(owed);
}

#[tokio::test]
async fn review380_f5_an_unreported_guest_write_is_carried_without_a_quiet_period() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "whole").unwrap();
    std::fs::write(dir.path().join("other.rs"), "one").unwrap();
    let (syncers, guest, sessions) =
        seeded_lab(dir.path(), state.path(), "/src/other.rs").await;
    let syncer = syncers.get("dev01").await.unwrap();
    syncer.pass_now().await.unwrap();
    let drains = sessions.watcher.drains();
    // The guest is in the middle of writing main.rs: no event has reached
    // the host yet, so no quiet period has started for it.
    guest.file("/src/main.rs", "half-writ", 70);
    // An unrelated host edit asks for a pass.
    std::fs::write(dir.path().join("other.rs"), "two").unwrap();
    let carried = {
        let dir = dir.path().to_path_buf();
        eventually(move || {
            std::fs::read_to_string(dir.join("main.rs")).unwrap_or_default() == "half-writ"
        })
        .await
    };
    println!(
        "REVIEW380 F5 torn copy on the host: {carried}, guest drains since: {}",
        sessions.watcher.drains() - drains
    );
    syncers.stop("dev01").await;
    assert!(carried, "fault not present");
}

#[tokio::test]
async fn review380_f6_guest_authored_ignore_rule_applies_one_pass_late() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "m").unwrap();
    let (syncers, guest, sessions) =
        seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
    let syncer = syncers.get("dev01").await.unwrap();
    syncer.pass_now().await.unwrap();
    guest.file("/src/.gitignore", "target/\n", 60);
    guest.dir("/src/target");
    guest.file("/src/target/out.bin", "build output", 61);
    sessions.watcher.mark(".gitignore");
    sessions.watcher.mark("target");
    sessions.watcher.mark("target/out.bin");
    let ruled = {
        let dir = dir.path().to_path_buf();
        eventually(move || dir.join(".gitignore").is_file()).await
    };
    assert!(ruled);
    syncer.pass_now().await.unwrap();
    syncer.pass_now().await.unwrap();
    let on_host = dir.path().join("target/out.bin").exists();
    let ledger = Ledger::load(
        &workspace(dir.path(), state.path()).ledger_path,
        dir.path(),
        "/src",
    );
    println!(
        "REVIEW380 F6 ignored build output on the host: {on_host}, still in ledger: {}",
        ledger.entries.contains_key("target/out.bin")
    );
    syncers.stop("dev01").await;
    assert!(on_host, "fault not present");
}

#[tokio::test]
async fn review380_f7_a_directory_the_guest_deleted_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir_all(dir.path().join("crate/target")).unwrap();
    std::fs::write(dir.path().join("crate/target/host.o"), "host build").unwrap();
    std::fs::write(dir.path().join("crate/a.rs"), "a").unwrap();
    let (syncers, guest, sessions) =
        seeded_lab(dir.path(), state.path(), "/src/crate/a.rs").await;
    let syncer = syncers.get("dev01").await.unwrap();
    syncer.pass_now().await.unwrap();
    guest.unlink("/src/crate/a.rs");
    guest.unlink("/src/crate");
    sessions.watcher.mark("crate/a.rs");
    sessions.watcher.mark("crate");
    let removed = {
        let dir = dir.path().to_path_buf();
        eventually(move || !dir.join("crate/a.rs").exists()).await
    };
    assert!(removed, "the guest delete never reached the host");
    assert!(guest.get("/src/crate").is_none() || true);
    let back = {
        let guest = guest.clone();
        eventually(move || guest.get("/src/crate").is_some()).await
    };
    println!("REVIEW380 F7 deleted guest directory is back: {back}");
    syncers.stop("dev01").await;
    assert!(back, "fault not present");
}

/// The developer deletes a guest file in the moment the syncer has closed
/// the watch to reopen it on a changed prune list.
struct DeleteOnReopen {
    sessions: Arc<OneFake>,
    watches: AtomicU64,
}

#[async_trait]
impl GuestRun for DeleteOnReopen {
    async fn run(&self, argv: Vec<String>) -> Result<super::super::windows::Ran> {
        self.sessions.run(argv).await
    }
}

#[async_trait]
impl GuestSessions for DeleteOnReopen {
    async fn open(&self) -> Result<Box<dyn GuestFs>> {
        self.sessions.open().await
    }

    async fn watch(&self, root: &str, prune: Vec<String>) -> Result<Box<dyn GuestWatch>> {
        if self.watches.fetch_add(1, Ordering::SeqCst) == 1 {
            self.sessions.guest.unlink("/src/old.rs");
        }
        self.sessions.watch(root, prune).await
    }

    async fn claim_root(&self, root: &str) -> Result<()> {
        self.sessions.claim_root(root).await
    }
}

#[tokio::test]
async fn review380_f8_a_guest_delete_across_a_prune_reopen_is_undone() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "m").unwrap();
    std::fs::write(dir.path().join("old.rs"), "old").unwrap();
    let guest = Arc::new(FakeGuest::new());
    let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
    let syncers = WorkspaceSyncers::default();
    let inner = OneFake::new(guest.clone());
    let sessions = Arc::new(DeleteOnReopen {
        sessions: inner.clone(),
        watches: AtomicU64::new(0),
    });
    syncers
        .start(workspace(dir.path(), state.path()), sessions.clone(), events)
        .await;
    let seeded = {
        let guest = guest.clone();
        eventually(move || guest.get("/src/old.rs").is_some()).await
    };
    assert!(seeded);
    let syncer = syncers.get("dev01").await.unwrap();
    syncer.pass_now().await.unwrap();
    // The host adds a rule that names a directory: the prune list changes
    // and the syncer reopens the guest watch.
    std::fs::write(dir.path().join(".vmlabignore"), "build/\n").unwrap();
    let reopened = {
        let sessions = sessions.clone();
        eventually(move || sessions.watches.load(Ordering::SeqCst) >= 2).await
    };
    assert!(reopened, "the watch never reopened: {:?}", inner.opens());
    // The delete happened as the watch reopened. Give the walk its pass.
    syncer.pass_now().await.unwrap();
    syncer.pass_now().await.unwrap();
    let report = syncer.report();
    println!(
        "REVIEW380 F8 guest old.rs: {:?}, host old.rs exists: {}, halt: {:?}",
        guest.get("/src/old.rs"),
        dir.path().join("old.rs").exists(),
        report.halt.as_ref().map(|h| h.reasons()),
    );
    syncers.stop("dev01").await;
    assert!(
        guest.get("/src/old.rs").is_some() || report.halt.is_some(),
        "fault not present: the delete was carried to the host"
    );
    assert!(dir.path().join("old.rs").exists());
}

#[tokio::test]
async fn review380_f9_a_halted_workspace_keeps_every_reported_path_owed() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "host").unwrap();
    let guest = Arc::new(FakeGuest::new());
    guest.dir("/src");
    guest.file("/src/main.rs", "guest", 1);
    let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
    let syncers = WorkspaceSyncers::default();
    let sessions = OneFake::new(guest.clone());
    syncers
        .start(workspace(dir.path(), state.path()), sessions.clone(), events)
        .await;
    let syncer = syncers.get("dev01").await.unwrap();
    assert!(eventually(|| syncer.report().halt.is_some()).await);
    // Paths that exist on neither side, and three that are not paths.
    for n in 0..3000 {
        sessions.watcher.mark(&format!("ghost/{n}"));
    }
    sessions.watcher.mark("../../outside");
    sessions.watcher.mark("/etc/passwd");
    sessions.watcher.mark("");
    assert!(eventually(|| syncer.report().unsynced.len() >= 3000).await);
    let started = Instant::now();
    syncer.pass_now().await.unwrap();
    syncer.pass_now().await.unwrap();
    let report = syncer.report();
    println!(
        "REVIEW380 F9 owed after two more passes: {}, two passes took {:?}, trouble: {:?}",
        report.unsynced.len(),
        started.elapsed(),
        report.trouble.as_deref().map(|t| t.chars().take(200).collect::<String>()),
    );
    syncers.stop("dev01").await;
    assert!(report.unsynced.len() >= 3000, "fault not present");
}

#[tokio::test]
async fn review380_f10_a_host_name_that_is_not_utf8_is_trouble_on_every_pass() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "m").unwrap();
    let odd = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    std::fs::write(dir.path().join(odd), "latin-1 name").unwrap();
    let (syncers, guest, _sessions) =
        seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
    let syncer = syncers.get("dev01").await.unwrap();
    let first = syncer.pass_now().await.unwrap();
    let second = syncer.pass_now().await.unwrap();
    println!(
        "REVIEW380 F10 trouble: {:?} / {:?}, skipped: {:?}, guest paths: {:?}",
        first.trouble, second.trouble, second.skipped, guest.paths()
    );
    let refused = syncers
        .before_capture("dev01", Some(&workspace(dir.path(), state.path())))
        .await;
    println!("REVIEW380 F10 capture: {:?}", refused.as_ref().err().map(|e| e.to_string().chars().take(160).collect::<String>()));
    syncers.stop("dev01").await;
    assert!(second.trouble.is_some(), "fault not present");
    assert!(second.skipped.is_empty());
    assert!(refused.is_err());
}
```
