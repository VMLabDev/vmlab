# Probes for the second labd workspace sync review

These are the temporary tests behind `docs/reviews/labd-sync-2.md`. They are not
in the tree. Each one **passes while its fault is present**, so a fix card
turns the final assertions round and keeps the rest.

Each block was pasted at the end of `mod tests` of the file its heading names,
on commit `fec7ca65`, in the checkout of card #946 on helios, and run with:

```
cargo test --all-features --lib review946 -- --nocapture --test-threads=1
```

Result: 11 passed, 0 failed (10 in the first run, with `review946_p3b` and
`review946_p6b` run again alone after a change to the probe). The names carry
the probe numbers of the run, which are not the finding codes. The map is:

| Probe | Finding |
|---|---|
| `review946_p6_…`, `review946_p6b_…` | F12 |
| `review946_p3_…`, `review946_p3b_…` | F13 |
| `review946_p1_…`, `review946_p1b_…` | F14 |
| `review946_p5_…` | F15 |
| `review946_p8_…` | F16 |
| `review946_p2_…` | F17 |
| `review946_p9_…` | F18 |
| `review946_p10_…` | F20 |

F19 has no probe: the test
`an_incomplete_reseed_keeps_the_loop_and_restart_behind_the_barrier`, which is
in the tree, shows it.

`review946_p1b` sleeps 3 s to show that nothing arrives. That is a probe, not a
test to keep: the test proposed for F14 is at watcher level and needs no sleep.
`review946_p9` prints times. They show the growth and are not limits.

For F17, what git does with the same lines was checked on helios (git 2.56.0)
in a scratch repository under the checkout, removed afterwards: a `.gitignore`
with the lines `obj\`, `x{y`, `x}y`, `[z-a]` and `build/`, beside files named
`x{y` and `x}y` and a directory `build`. `git status --porcelain --ignored`
exits 0 and lists `build/`, `x{y` and `x}y` as ignored.

## What the probes printed

```
P1  events of the delete and the create: [Touched("d"), Touched("d"), Rescan, Touched("d"), Rescan, Touched("d")]
P1  events of the two writes: [Touched("top.rs"), Touched("top.rs"), Touched("top.rs")]
P1b the walk carried the remade file: true
P1b guest copy 3 s after the save: Some("two")
P1b carried after an unrelated save: true
P2  line "obj\\": ERROR in …/.gitignore: error parsing glob 'obj\': dangling '\'
P2  line "a[b": read
P2  line "x{y": ERROR in …/.gitignore: error parsing glob 'x{y': unclosed alternate group; missing '}' (maybe escape '{' with '[{]'?)
P2  line "x}y": ERROR in …/.gitignore: error parsing glob 'x}y': unopened alternate group; missing '{' (maybe escape '}' with '[}]'?)
P2  line "[z-a]": ERROR in …/.gitignore: error parsing glob '[z-a]': invalid range; 'z' > 'a'
P2  line "ok/": read
P3  pass 1 conflicts [Conflict { path: "a.rs", kind: ModifiedAndDeleted }] withheld {}
P3  pass 2 actions [Remove { direction: ToHost, path: "a.rs", kind: File }] conflicts []
P3b unconfirmed before the break: {}
P3b halted true after the break: Some([("main.rs", "one side changed it and the other deleted it")])
P3b withheld on the ledger: {}
P3b host file deleted: true, halt now: false
P5  reseed None rescans 0 unsynced [] trouble None
P5  guest holds late.rs: true, host holds it: false, capture Ok(()), destroy Ok(())
P6  pass 0: 50 of 100 host files removed, no halt
P6  pass 1: 25 of 50 host files removed, no halt
P6  pass 2: 12 of 25 host files removed, no halt
P6  pass 3: 13 of 13 host files removed, no halt
P6  100 of 100 host files removed in 4 passes
P6b after the first part: 55 host files, halt false
P6b after the rest: 55 host files, halted true
P8  refused once: true, report [Oversize { path: "dump.sql", direction: ToHost, size: 128, cap: 64 }]
P8  after one more pass: oversize [] unsynced [] skipped []
P8  capture Ok(()) restore Ok(()) destroy Ok(())
P9  16890 bytes of rules: read in 9.320409ms, 2000 prune prefixes after 141.204309ms
P9  70890 bytes of rules: read in 31.886396ms, 8000 prune prefixes after 2.096166959s
P10 after the seed: trouble None
P10 after the host changed the link: guest Some(Symlink("main.rs")) trouble Some("1 workspace actions failed: link: link: destination changed after scanning; rescan required")
```

The first run of `review946_p3b` asked for its pass with `pass_now` after the
break. That pass is a flush, and a flush over a walk holds every missing guest
path back, so the path was withheld and the host file stayed. The probe below
waits for the pass the start runs by itself, which is the unprotected one.

## `src/labd/workspace/watcher.rs`

```rust
    // ---- card 946 review probes: each PASSES while its fault is present ----
    #[test]
    fn review946_p1_a_directory_made_again_is_not_watched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let mut watch = HostWatch::start().unwrap();
        let dirs = vec![String::new(), "d".to_string()];
        assert!(watch.register(dir.path(), &dirs).is_empty());
        std::fs::remove_dir(dir.path().join("d")).unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let first = watch.settle();
        println!("REVIEW946 P1 events of the delete and the create: {first:?}");
        // The scan that answers the rescan finds the same directory list.
        assert!(watch.register(dir.path(), &dirs).is_empty());
        std::fs::write(dir.path().join("d/new.rs"), "x").unwrap();
        std::fs::write(dir.path().join("top.rs"), "x").unwrap();
        let events = watch.settle();
        println!("REVIEW946 P1 events of the two writes: {events:?}");
        assert!(events.contains(&HostEvent::Touched("top.rs".into())));
        assert!(
            !events.contains(&HostEvent::Touched("d/new.rs".into())),
            "fault not present: the new directory is watched"
        );
    }
```

## `src/labd/workspace/ignore.rs`

```rust
    // ---- card 946 review probes ----
    #[test]
    fn review946_p2_a_rule_line_the_glob_parser_refuses_fails_the_read() {
        let mut failed = 0;
        for line in ["obj\\", "a[b", "x{y", "x}y", "[z-a]", "ok/"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(".gitignore"), format!("{line}\n")).unwrap();
            let mut ignores = Ignores::new();
            let got = ignores.read_dir_rules(dir.path(), "");
            println!(
                "REVIEW946 P2 line {line:?}: {}",
                match &got {
                    Ok(()) => "read".to_string(),
                    Err(e) => format!("ERROR {e:#}"),
                }
            );
            failed += usize::from(got.is_err());
        }
        assert!(failed > 0, "fault not present: every line was read");
    }

    #[test]
    fn review946_p9_every_literal_rule_line_becomes_a_prune_prefix() {
        for n in [2_000usize, 8_000] {
            let dir = tempfile::tempdir().unwrap();
            let body: String = (0..n).map(|i| format!("gen{i}/\n")).collect();
            std::fs::write(dir.path().join(".gitignore"), &body).unwrap();
            let started = std::time::Instant::now();
            let mut ignores = Ignores::new();
            ignores.read_dir_rules(dir.path(), "").unwrap();
            let read = started.elapsed();
            let prune = ignores.prune_list(&[]);
            println!(
                "REVIEW946 P9 {} bytes of rules: read in {read:?}, {} prune prefixes after {:?}",
                body.len(),
                prune.len(),
                started.elapsed()
            );
            assert_eq!(prune.len(), n);
        }
    }
```

## `src/labd/workspace/plan.rs`

```rust
    // ---- card 946 review probes: each PASSES while its fault is present ----
    #[test]
    fn review946_p3_a_withheld_absence_is_deleted_after_a_host_edit_is_taken_back() {
        let mut ledger = Ledger::new(Path::new("/h"), "/g");
        ledger.agree(
            "a.rs",
            Agreed {
                kind: Kind::File,
                digest: "d1".into(),
                host: Side::new(1, 1),
                guest: Side::new(1, 1),
            },
        );
        // Borne out by an earlier start.
        ledger.unconfirmed.clear();
        let guest = BTreeMap::new();
        let empty = BTreeSet::new();
        let none = BTreeMap::new();
        // Pass 1 follows a break. The guest lacks the file, the host edited it.
        let host: BTreeMap<String, State> =
            [("a.rs".to_string(), file(Some("d2"), 1, 2))].into();
        let plan = reconcile(&Inputs {
            host: &host,
            guest: &guest,
            ledger: &ledger,
            undecided: &empty,
            guest_owned: &empty,
            resolved: &none,
            max_file_bytes: CAP,
            case_folding: false,
            unwitnessed: true,
        });
        println!(
            "REVIEW946 P3 pass 1 conflicts {:?} withheld {:?}",
            plan.conflicts, plan.withheld
        );
        assert_eq!(plan.conflicts.len(), 1);
        assert!(plan.withheld.is_empty());
        // What syncer.rs does with it.
        ledger.withheld = plan.withheld.clone();
        // Pass 2: the developer takes the host edit back. No break.
        let host: BTreeMap<String, State> =
            [("a.rs".to_string(), file(Some("d1"), 1, 3))].into();
        let plan = reconcile(&Inputs {
            host: &host,
            guest: &guest,
            ledger: &ledger,
            undecided: &empty,
            guest_owned: &empty,
            resolved: &none,
            max_file_bytes: CAP,
            case_folding: false,
            unwitnessed: false,
        });
        println!(
            "REVIEW946 P3 pass 2 actions {:?} conflicts {:?}",
            plan.actions, plan.conflicts
        );
        assert_eq!(
            plan.actions,
            vec![Action::Remove {
                direction: Direction::ToHost,
                path: "a.rs".into(),
                kind: Kind::File,
            }],
            "fault not present"
        );
    }

    #[test]
    fn review946_p6_a_guest_deletion_seen_in_slices_never_trips_the_bulk_guard() {
        let mut ledger = Ledger::new(Path::new("/h"), "/g");
        let mut host = BTreeMap::new();
        let mut guest = BTreeMap::new();
        for n in 0..100 {
            let path = format!("src/f{n:03}.rs");
            ledger.agree(
                &path,
                Agreed {
                    kind: Kind::File,
                    digest: "d".into(),
                    host: Side::new(1, 1),
                    guest: Side::new(1, 1),
                },
            );
            host.insert(path.clone(), file(None, 1, 1));
            guest.insert(path, file(None, 1, 1));
        }
        let empty = BTreeSet::new();
        let none = BTreeMap::new();
        let (mut removed, mut passes) = (0, 0);
        while !ledger.entries.is_empty() {
            let left = ledger.entries.len();
            // How far the guest's `rm -rf` has got when this pass looks.
            let slice = if left <= BULK_DELETE_FLOOR {
                left
            } else {
                left / 2
            };
            let doomed: Vec<String> = guest.keys().take(slice).cloned().collect();
            for path in &doomed {
                guest.remove(path);
            }
            let plan = reconcile(&Inputs {
                host: &host,
                guest: &guest,
                ledger: &ledger,
                undecided: &empty,
                guest_owned: &empty,
                resolved: &none,
                max_file_bytes: CAP,
                case_folding: false,
                unwitnessed: false,
            });
            assert!(plan.bulk_delete.is_none(), "the guard fired at {left}");
            assert!(plan.conflicts.is_empty());
            assert_eq!(plan.actions.len(), slice);
            for action in &plan.actions {
                assert_eq!(action.direction(), Direction::ToHost);
                ledger.forget(action.path());
                host.remove(action.path());
            }
            println!("REVIEW946 P6 pass {passes}: {slice} of {left} host files removed, no halt");
            removed += slice;
            passes += 1;
        }
        println!("REVIEW946 P6 {removed} of 100 host files removed in {passes} passes");
        assert_eq!(removed, 100);
    }
```

## `src/labd/workspace/syncer.rs`

```rust
    // ---- card 946 review probes: each PASSES while its fault is present ----
    #[tokio::test]
    async fn review946_p1b_a_host_edit_in_a_directory_made_again_does_not_sync() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("d/a.rs"), "one").unwrap();
        std::fs::write(dir.path().join("top.rs"), "t").unwrap();
        let (syncers, guest, _sessions) = seeded_lab(dir.path(), state.path(), "/src/d/a.rs").await;
        let syncer = syncers.get("dev01").await.unwrap();
        syncer.pass_now().await.unwrap();
        // What a branch switch away and back, or `rm -rf d && mkdir d`, does.
        std::fs::remove_dir_all(dir.path().join("d")).unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("d/a.rs"), "two").unwrap();
        let landed = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/d/a.rs").as_deref() == Some("two")).await
        };
        println!("REVIEW946 P1b the walk carried the remade file: {landed}");
        syncer.pass_now().await.unwrap();
        syncer.pass_now().await.unwrap();
        // An ordinary save in the directory, with the workspace at rest.
        std::fs::write(dir.path().join("d/a.rs"), "three").unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let after_wait = guest.text("/src/d/a.rs");
        println!("REVIEW946 P1b guest copy 3 s after the save: {after_wait:?}");
        // An unrelated event is what carries it.
        std::fs::write(dir.path().join("top.rs"), "t2").unwrap();
        let carried = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/d/a.rs").as_deref() == Some("three")).await
        };
        println!("REVIEW946 P1b carried after an unrelated save: {carried}");
        syncers.stop("dev01").await;
        assert!(landed);
        assert_eq!(after_wait.as_deref(), Some("two"), "fault not present");
    }

    #[tokio::test]
    async fn review946_p3b_a_host_file_is_deleted_for_an_absence_nothing_saw() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "agreed").unwrap();
        std::fs::write(dir.path().join("other.rs"), "o").unwrap();
        let (syncers, guest, sessions) = seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
        let ws = workspace(dir.path(), state.path());
        syncers.get("dev01").await.unwrap().pass_now().await.unwrap();
        // A second start bears the agreements out.
        syncers.stop("dev01").await;
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        syncers.start(ws.clone(), sessions.clone(), events).await;
        syncers.get("dev01").await.unwrap().pass_now().await.unwrap();
        syncers.stop("dev01").await;
        println!(
            "REVIEW946 P3b unconfirmed before the break: {:?}",
            Ledger::load(&ws.ledger_path, &ws.host_root, &ws.guest_root).unconfirmed
        );
        // The machine is away. The guest loses the file with nothing watching,
        // and the host edits it.
        guest.unlink("/src/main.rs");
        std::fs::write(dir.path().join("main.rs"), "edited on the host").unwrap();
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        syncers.start(ws.clone(), sessions.clone(), events).await;
        let syncer = syncers.get("dev01").await.unwrap();
        // The start's own pass, with no verb asking for it.
        let halted = eventually(|| syncer.report().halt.is_some()).await;
        let report = syncer.report();
        println!(
            "REVIEW946 P3b halted {halted} after the break: {:?}",
            report.halt.as_ref().map(|halt| halt.reasons())
        );
        println!(
            "REVIEW946 P3b withheld on the ledger: {:?}",
            Ledger::load(&ws.ledger_path, &ws.host_root, &ws.guest_root).withheld
        );
        // The developer takes the host edit back.
        std::fs::write(dir.path().join("main.rs"), "agreed").unwrap();
        let gone = {
            let path = dir.path().join("main.rs");
            eventually(move || !path.exists()).await
        };
        let report = syncer.report();
        println!("REVIEW946 P3b host file deleted: {gone}, halt now: {:?}", report.halt.is_some());
        syncers.stop("dev01").await;
        assert!(gone, "fault not present: the host file survived");
    }

    #[tokio::test]
    async fn review946_p5_a_guest_file_made_while_the_re_seed_ran_is_never_seen() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "the canonical version").unwrap();
        let guest = Arc::new(FakeGuest::new());
        let mut sessions = OneFake::new(guest.clone());
        Arc::get_mut(&mut sessions).unwrap().before_watch = |guest, _| {
            if guest.get("/restored").is_some() && guest.get("/src/late.rs").is_none() {
                guest.file("/src/late.rs", "written while the re-seed ran", 7);
            }
        };
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let ws = workspace(dir.path(), state.path());
        syncers.start(ws.clone(), sessions.clone(), events).await;
        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.get("/src/main.rs").is_some()).await
        };
        assert!(seeded);
        syncers.get("dev01").await.unwrap().pass_now().await.unwrap();
        restore_bracket(&syncers, &ws, || {
            rewind(&guest);
            guest.file("/restored", "", 1);
        })
        .await;
        let syncer = syncers.get("dev01").await.unwrap();
        let reopened = {
            let guest = guest.clone();
            eventually(move || guest.get("/src/late.rs").is_some()).await
        };
        assert!(reopened, "the watch never reopened after the re-seed");
        syncer.pass_now().await.unwrap();
        let report = syncer.pass_now().await.unwrap();
        let capture = syncers.before_capture("dev01", Some(&ws), true).await;
        let destroy = syncers.before_destroy("dev01", false, Some(&ws)).await;
        println!(
            "REVIEW946 P5 reseed {:?} rescans {} unsynced {:?} trouble {:?}",
            report.reseed, report.rescans, report.unsynced, report.trouble
        );
        println!(
            "REVIEW946 P5 guest holds late.rs: {}, host holds it: {}, capture {:?}, destroy {:?}",
            guest.get("/src/late.rs").is_some(),
            dir.path().join("late.rs").exists(),
            capture.as_ref().map_err(|e| format!("{e:#}")),
            destroy.as_ref().map_err(|e| format!("{e:#}")),
        );
        syncers.stop("dev01").await;
        assert!(guest.get("/src/late.rs").is_some());
        assert!(!dir.path().join("late.rs").exists(), "fault not present");
        assert!(report.unsynced.is_empty());
        assert!(capture.is_ok());
    }

    #[tokio::test]
    async fn review946_p8_an_oversize_guest_file_is_forgotten_and_nothing_refuses_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "m").unwrap();
        let guest = Arc::new(FakeGuest::new());
        let sessions = OneFake::new(guest.clone());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let mut ws = workspace(dir.path(), state.path());
        ws.max_file_bytes = 64;
        syncers.start(ws.clone(), sessions.clone(), events).await;
        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.get("/src/main.rs").is_some()).await
        };
        assert!(seeded);
        let syncer = syncers.get("dev01").await.unwrap();
        syncer.pass_now().await.unwrap();
        guest.file("/src/dump.sql", &"x".repeat(128), 5);
        sessions.watcher.mark("dump.sql");
        let refused = eventually(|| !syncer.report().oversize.is_empty()).await;
        println!(
            "REVIEW946 P8 refused once: {refused}, report {:?}",
            syncer.report().oversize
        );
        let report = syncer.pass_now().await.unwrap();
        println!(
            "REVIEW946 P8 after one more pass: oversize {:?} unsynced {:?} skipped {:?}",
            report.oversize, report.unsynced, report.skipped
        );
        let capture = syncers.before_capture("dev01", Some(&ws), true).await;
        let restore = syncers
            .before_restore("dev01", false, Some(&ws), true)
            .await;
        let destroy = syncers.before_destroy("dev01", false, Some(&ws)).await;
        println!(
            "REVIEW946 P8 capture {:?} restore {:?} destroy {:?}",
            capture.as_ref().map_err(|e| format!("{e:#}")),
            restore.as_ref().map_err(|e| format!("{e:#}")),
            destroy.as_ref().map_err(|e| format!("{e:#}")),
        );
        syncers.stop("dev01").await;
        assert!(refused);
        assert!(guest.get("/src/dump.sql").is_some());
        assert!(!dir.path().join("dump.sql").exists());
        assert!(restore.is_ok() && destroy.is_ok(), "fault not present");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn review946_p10_a_link_whose_lstat_size_is_not_its_target_length_cannot_be_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "m").unwrap();
        std::os::unix::fs::symlink("main.rs", dir.path().join("link")).unwrap();
        let guest = Arc::new(FakeGuest::new());
        // What NTFS answers for a symlink: a reparse point has length 0.
        guest.report_size("/src/link", 0);
        let sessions = OneFake::new(guest.clone());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let ws = workspace(dir.path(), state.path());
        syncers.start(ws.clone(), sessions.clone(), events).await;
        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.get("/src/link").is_some()).await
        };
        assert!(seeded, "the link never landed");
        let syncer = syncers.get("dev01").await.unwrap();
        let report = syncer.pass_now().await.unwrap();
        println!("REVIEW946 P10 after the seed: trouble {:?}", report.trouble);
        std::fs::remove_file(dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink("other.rs", dir.path().join("link")).unwrap();
        let mut report = syncer.pass_now().await.unwrap();
        for _ in 0..3 {
            report = syncer.pass_now().await.unwrap();
        }
        println!(
            "REVIEW946 P10 after the host changed the link: guest {:?} trouble {:?}",
            guest.get("/src/link"),
            report.trouble
        );
        syncers.stop("dev01").await;
        assert!(
            matches!(guest.get("/src/link"), Some(Node::Symlink(target)) if target == "main.rs"),
            "fault not present: the guest link was replaced"
        );
        assert!(report.trouble.is_some());
    }

    #[tokio::test]
    async fn review946_p6b_a_guest_rm_rf_seen_in_two_passes_loses_the_first_part() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        for n in 0..100 {
            std::fs::write(dir.path().join(format!("f{n:03}.rs")), "x").unwrap();
        }
        let (syncers, guest, sessions) = seeded_lab(dir.path(), state.path(), "/src/f099.rs").await;
        let syncer = syncers.get("dev01").await.unwrap();
        syncer.pass_now().await.unwrap();
        let on_host = |dir: &std::path::Path| std::fs::read_dir(dir).unwrap().count();
        assert_eq!(on_host(dir.path()), 100);
        // The guest's `rm -rf` has removed 45 files when the first pass looks.
        for n in 0..45 {
            guest.unlink(&format!("/src/f{n:03}.rs"));
            sessions.watcher.mark(&format!("f{n:03}.rs"));
        }
        let first = {
            let dir = dir.path().to_path_buf();
            eventually(move || std::fs::read_dir(&dir).unwrap().count() == 55).await
        };
        println!(
            "REVIEW946 P6b after the first part: {} host files, halt {}",
            on_host(dir.path()),
            syncer.report().halt.is_some()
        );
        // And the rest by the next.
        for n in 45..100 {
            guest.unlink(&format!("/src/f{n:03}.rs"));
            sessions.watcher.mark(&format!("f{n:03}.rs"));
        }
        let halted = eventually(|| syncer.report().halt.is_some()).await;
        println!(
            "REVIEW946 P6b after the rest: {} host files, halted {halted}",
            on_host(dir.path())
        );
        syncers.stop("dev01").await;
        assert!(first, "fault not present: the first part was held");
        assert!(halted);
        assert_eq!(on_host(dir.path()), 55);
    }
```
