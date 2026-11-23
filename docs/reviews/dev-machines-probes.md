# Probes for the dev machines review

These are the temporary tests behind `docs/reviews/dev-machines.md` (card
#672). They are not in the tree. Each one **passes while its fault is
present**, so a fix card turns the final assertions round and keeps the rest.

They were committed as `1e15e53b` on the branch `card-672` (base `6eaa1a92`,
`next` on 2026-11-23), run on helios, and removed by the next commit. The
command:

```
cargo test --all-features --lib review672 -- --test-threads=1
```

Result: `10 passed; 0 failed; 0 ignored; 2590 filtered out`.

| Probe | Finding |
|---|---|
| `review672_p1_…` | F6 |
| `review672_p2_…` | F1 |
| `review672_p3_…` | F4 |
| `review672_p4_…` | F7 |
| `review672_p5_…` | F9 |
| `review672_p6_…`, `review672_p7_…` | F8 |
| `review672_p8_…` | F2 |
| `review672_p9_…` | F3 |
| `review672_p10_…` | F5 |

The probes use the in-memory guest (`FakeGuest`) and the machine double
(`FakeMachine`), not a VM. `review672_p1` waits 2 s to observe that a pass does
not start. That wait is in the probe only. The test proposed for F6 needs no
clock.

## At the end of `mod tests` of `src/labd/workspace/syncer.rs`

```rust
    // ---- card 672 review probes: each PASSES while its fault is present ----

    #[tokio::test]
    async fn review672_p1_one_silent_guest_stops_an_unrelated_workspace() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir_a.path().join("a.rs"), "a").unwrap();
        std::fs::write(dir_b.path().join("b.rs"), "b").unwrap();
        let a = workspace(dir_a.path(), state.path());
        let b = Workspace {
            machine: "dev02".into(),
            ledger_path: Ledger::path(state.path(), "dev02"),
            ..workspace(dir_b.path(), state.path())
        };
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let entered_a = Arc::new(Notify::new());
        let entered_b = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let task_a = tokio::spawn(isolated_pass(
            a,
            Arc::new(PausedSession {
                sessions: OneFake::new(Arc::new(FakeGuest::new())),
                opened: entered_a.clone(),
                release: Some(release.clone()),
            }),
            events.clone(),
        ));
        entered_a.notified().await;
        let task_b = tokio::spawn(isolated_pass(
            b,
            Arc::new(PausedSession {
                sessions: OneFake::new(Arc::new(FakeGuest::new())),
                opened: entered_b.clone(),
                release: None,
            }),
            events,
        ));
        // Observation only: dev02 shares nothing with dev01, and does not
        // reach its own guest while dev01's guest stays silent.
        let b_ran = tokio::time::timeout(Duration::from_secs(2), entered_b.notified())
            .await
            .is_ok();
        release.notify_one();
        task_a.await.unwrap();
        task_b.await.unwrap();
        assert!(!b_ran, "dev02 ran while dev01's guest was silent");
    }

    #[tokio::test]
    async fn review672_p2_a_guest_rule_brings_the_lab_state_directory_into_scope() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), ".vmlab/\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".vmlab/smb")).unwrap();
        std::fs::write(dir.path().join(".vmlab/smb/creds"), "host-secret").unwrap();
        std::fs::write(dir.path().join("main.rs"), "m").unwrap();
        let (syncers, guest, sessions) = seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
        let syncer = syncers.get("dev01").await.unwrap();
        syncer.pass_now().await.unwrap();
        let kept_out = guest.get("/src/.vmlab/smb/creds").is_none();

        guest.file("/src/.vmlabignore", "!.vmlab/\n!.vmlab/**\n", 7);
        sessions.watcher.mark(".vmlabignore");
        let leaked = {
            let guest = guest.clone();
            eventually(move || {
                guest.text("/src/.vmlab/smb/creds").as_deref() == Some("host-secret")
            })
            .await
        };
        syncer.pass_now().await.unwrap();

        guest.file("/src/.vmlab/smb/creds", "guest-wrote-this", 9);
        sessions.watcher.mark(".vmlab/smb/creds");
        let creds = dir.path().join(".vmlab/smb/creds");
        let overwritten = eventually(move || {
            std::fs::read_to_string(&creds).ok().as_deref() == Some("guest-wrote-this")
        })
        .await;
        let report = syncer.report();
        syncers.stop("dev01").await;

        assert!(kept_out, "the .gitignore rule keeps the state directory out");
        assert!(leaked, "the host file reached the guest: {:?}", guest.paths());
        assert!(overwritten, "the guest's bytes reached the host: {report:?}");
        assert!(report.halt.is_none(), "{:?}", report.halt);
    }

    #[tokio::test]
    async fn review672_p3_a_stop_records_nothing_about_owed_guest_work() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "agreed").unwrap();
        let (syncers, guest, sessions) = seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
        let syncer = syncers.get("dev01").await.unwrap();
        syncer.pass_now().await.unwrap();

        guest.file("/src/main.rs", "guest work", 900);
        guest.file("/src/new.rs", "new", 901);
        sessions.watcher.mark("main.rs");
        sessions.watcher.mark("new.rs");
        // `down`, `destroy` and a partial stop all end the syncer this way.
        syncers.stop("dev01").await;

        let ws = workspace(dir.path(), state.path());
        let on_host = std::fs::read_to_string(dir.path().join("main.rs")).unwrap();
        let restore = syncers.before_restore("dev01", false, Some(&ws)).await;
        let capture = syncers.before_capture("dev01", Some(&ws)).await;
        assert_eq!(on_host, "agreed", "the guest work never reached the host");
        assert!(!dir.path().join("new.rs").exists());
        assert!(restore.is_ok(), "{restore:?}");
        assert!(capture.is_ok(), "{capture:?}");
    }
```

## At the end of `mod tests` of `src/labd/workspace/diff.rs`

```rust
    // ---- card 672 review probes: each PASSES while its fault is present ----

    struct Lying(Arc<FakeGuest>);

    #[async_trait::async_trait]
    impl GuestFs for Lying {
        async fn lstat(&self, path: &str) -> Result<Option<crate::labd::vm_agent::Attrs>> {
            Ok(GuestFs::lstat(&self.0, path).await?.map(|mut attrs| {
                attrs.size = 1;
                attrs
            }))
        }
        async fn pull(&self, _: &str, local: &Path) -> Result<()> {
            std::fs::write(local, vec![b'a'; INLINE as usize + 1])?;
            Ok(())
        }
        async fn readdir(&self, _: &str) -> Result<Vec<crate::labd::vm_agent::DirEntry>> {
            unimplemented!()
        }
        async fn readlink(&self, _: &str) -> Result<String> {
            unimplemented!()
        }
        async fn digest(&self, _: &str) -> Result<String> {
            unimplemented!()
        }
        async fn mkdir(&self, _: &str, _: bool) -> Result<()> {
            unimplemented!()
        }
        async fn mkdir_root(&self, _: &str, _: bool) -> Result<()> {
            unimplemented!()
        }
        async fn push(&self, _: &Path, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn symlink(
            &self,
            _: &str,
            _: &str,
            _: crate::labd::vm_agent::LinkKind,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn rename(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn remove(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn rmdir(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn review672_p4_the_inline_cap_trusts_the_size_the_guest_reports() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let guest = Arc::new(FakeGuest::new());
        guest.file("/src/big.log", "x", 1);
        let sides = one(
            &Lying(guest),
            &workspace(dir.path()),
            "big.log",
            &scratch.path().join("0"),
        )
        .await;
        let copy = sides.guest.unwrap();
        assert_eq!(copy.size, INLINE + 1, "read whole into memory");
        assert_eq!(copy.text.map(|t| t.len() as u64), Some(INLINE + 1));
        assert!(copy.omitted.is_none());
    }

    #[tokio::test]
    async fn review672_p5_a_link_and_a_file_with_the_same_bytes_read_as_identical() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("target", dir.path().join("x")).unwrap();
        let guest = Arc::new(FakeGuest::new());
        guest.file("/src/x", "target", 1);
        let sides = one(
            &guest,
            &workspace(dir.path()),
            "x",
            &scratch.path().join("0"),
        )
        .await;
        assert!(sides.identical);
    }

    #[tokio::test]
    async fn review672_p6_terminal_escapes_in_a_guest_file_count_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let guest = Arc::new(FakeGuest::new());
        guest.file("/src/a.txt", "\u{1b}[2J\u{1b}]52;c;ZXZpbA==\u{7}gone\n", 1);
        let sides = one(
            &guest,
            &workspace(dir.path()),
            "a.txt",
            &scratch.path().join("0"),
        )
        .await;
        let text = sides.guest.unwrap().text.expect("taken as text");
        assert!(text.contains('\u{1b}'));
    }
```

## At the end of `mod tests` of `src/cli/dev.rs`

```rust
    // ---- card 672 review probe: PASSES while its fault is present ----

    #[test]
    fn review672_p7_guest_names_and_text_reach_the_terminal_with_their_escapes() {
        let name = "a\u{1b}[2J\u{1b}]52;c;ZXZpbA==\u{7}\nfake line.rs";
        crate::labd::workspace::ignore::validate_entry_name(name, false)
            .expect("a name the syncer accepts from a guest");
        let said = sync_report(
            "dev01",
            &WorkspaceSyncStatus {
                conflicts: vec![crate::status::WorkspaceConflictStatus {
                    path: name.into(),
                    reason: "both sides changed it since they last agreed".into(),
                }],
                conflicts_total: 1,
                ..halted_sync()
            },
        );
        assert!(said.contains('\u{1b}'), "{said:?}");
        assert!(said.contains("\nfake line.rs"), "{said:?}");

        let diff: Diff = serde_json::from_value(serde_json::json!({
            "machine": "dev01", "host_root": "/h", "guest_root": "/src",
            "files": [{
                "path": "a.txt",
                "host": {"size": 2, "digest": "aa", "text": "x\n", "omitted": null},
                "guest": {"size": 9, "digest": "bb", "text": "\u{1b}[2Jgone\n", "omitted": null},
                "identical": false
            }]
        }))
        .unwrap();
        assert!(render_diff(&diff).contains('\u{1b}'));
    }
```

## At the end of `mod tests` of `src/labd/lab.rs`

```rust
    // ---- card 672 review probes: each PASSES while its fault is present ----

    const REVIEW672_LAB: &str = r#"import <vmlab.wcl>
lab "t" {
  @dev(default = true, workspace = "./src") vm "dev01" { template = "x86_64/t" }
}"#;

    #[tokio::test]
    async fn review672_p8_a_restore_with_the_host_workspace_away_skips_the_halt_and_the_note() {
        use crate::labd::workspace::ledger::Ledger;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let sh = shared(dir.path());
        let lab = lab_of(
            dir.path(),
            REVIEW672_LAB,
            vec![FakeMachine::new("dev01", MachineKind::Vm, &sh)],
        );
        lab.snapshot("dev01", "s").await.unwrap();
        let ws = lab.workspace_of("dev01").expect("the workspace resolves");
        let mut ledger = Ledger::new(&ws.host_root, &ws.guest_root);
        ledger.halted = vec!["main.rs".into()];
        ledger.save(&ws.ledger_path).unwrap();
        // Control: with the directory there, the halt on the ledger refuses.
        let control = lab.restore("dev01", "s", false).await;

        std::fs::rename(dir.path().join("src"), dir.path().join("src.away")).unwrap();
        let restored = lab.restore("dev01", "s", false).await;
        std::fs::rename(dir.path().join("src.away"), dir.path().join("src")).unwrap();
        let after = Ledger::load(&ws.ledger_path, &ws.host_root, &ws.guest_root);

        assert!(control.is_err(), "control: the halt refuses");
        assert!(restored.is_ok(), "{restored:?}");
        assert!(!after.reseed_owed, "no re-seed is owed after the rewind");
        assert_eq!(after.halted, ["main.rs"], "the halt still stands");
    }

    #[tokio::test]
    async fn review672_p9_a_dev_machine_started_outside_up_has_no_syncer_and_both_brackets_pass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let sh = shared(dir.path());
        let lab = lab_of(
            dir.path(),
            REVIEW672_LAB,
            vec![FakeMachine::new("dev01", MachineKind::Vm, &sh)],
        );
        lab.start_machine("dev01").await.unwrap();
        let running = lab.machine("dev01").unwrap().state().await == PowerState::Running;
        let syncing = lab.workspaces.get("dev01").await.is_some();
        let captured = lab.snapshot("dev01", "s").await;
        let restored = lab.restore("dev01", "s", false).await;
        assert!(running);
        assert!(!syncing, "nothing started a syncer");
        assert!(captured.is_ok(), "{captured:?}");
        assert!(restored.is_ok(), "{restored:?}");
    }

    #[tokio::test]
    async fn review672_p10_a_destroy_does_not_ask_about_a_halted_workspace() {
        use crate::labd::workspace::ledger::Ledger;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let sh = shared(dir.path());
        let lab = lab_of(
            dir.path(),
            REVIEW672_LAB,
            vec![FakeMachine::new("dev01", MachineKind::Vm, &sh)],
        );
        let ws = lab.workspace_of("dev01").expect("the workspace resolves");
        let mut ledger = Ledger::new(&ws.host_root, &ws.guest_root);
        ledger.halted = vec!["main.rs".into()];
        ledger.save(&ws.ledger_path).unwrap();
        let destroyed = lab.destroy_machine("dev01").await;
        assert!(destroyed.is_ok(), "{destroyed:?}");
        assert!(!ws.ledger_path.exists(), "the halt went with the ledger");
    }
```
