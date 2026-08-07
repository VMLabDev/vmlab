//! The syncer itself: the lab-daemon-owned loop that keeps one machine's
//! workspace in step (PRD §19.6, ADR-0014).
//!
//! **Lab-daemon-owned, not client-owned.** It is started by `up` and lives
//! until the machine stops; the `vmlab` process that asked for the `up` can
//! exit and the workspace keeps syncing, because a developer's source tree
//! must not depend on a terminal staying open.
//!
//! **It starts after provisioning, not at machine-ready** — the syncer writes
//! as the machine's **default login** (§19.2), the one named exception to
//! vmlab's machinery running as the agent identity, and that account does not
//! exist until provisioning creates it. Without the exception the developer
//! would own none of their own source tree.
//!
//! One pass is: walk the host, ask the guest about those paths, reconcile,
//! apply, save the ledger. **The seed is simply the first pass** — an empty
//! guest tree needs no separate mechanism.
//!
//! A **fresh file session per pass** is deliberate. It costs one round trip
//! per debounce window and it makes §19.6's resume rule structural rather than
//! remembered: there is no transfer state to resume *from*, so a dropped
//! channel can only be answered by re-transferring, and an agent restart is
//! answered by the next pass's full walk.
//!
//! Everything this loop declines to do, it says out loud. A path skipped, a
//! file the size guard refused, a conflict, a guest-side change this direction
//! does not carry: each reaches the event feed by name. From inside the guest
//! a syncer that has quietly stopped looks exactly like one with nothing to
//! do, and that is the failure ADR-0014 exists to rule out.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{Mutex, watch};

use super::apply::{Target, apply};
use super::guest::GuestFs;
use super::ledger::Ledger;
use super::plan::{Inputs, reconcile};
use super::scan::{host_scan, probe_guest};
use super::watcher::{Debounce, HostEvent, HostWatch, QUIET};
use crate::labd::events::EventLog;

/// How long to wait before retrying a pass that could not reach the guest.
/// The guest is booted and provisioned by the time the syncer starts, so a
/// failure here is a blip rather than a state to poll through.
const RETRY: Duration = Duration::from_secs(5);

/// One machine's workspace, resolved.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub machine: String,
    /// The canonical host directory, absolute.
    pub host_root: PathBuf,
    /// Where the working copy lands in the guest.
    pub guest_root: String,
    /// Where the ledger lives, under the lab's `.vmlab/`.
    pub ledger_path: PathBuf,
    /// The size guard's per-file cap.
    pub max_file_bytes: u64,
}

/// How the syncer reaches a guest. A trait rather than a machine handle
/// because what the syncer needs is exactly "a file session as the default
/// login", and nothing else about a machine.
#[async_trait]
pub trait GuestSessions: Send + Sync {
    /// Open a file session as the machine's **default login**.
    async fn open(&self) -> Result<Box<dyn GuestFs>>;
}

/// Every workspace syncer running in one lab, one per machine.
///
/// A machine's syncer is independent: two dev machines may share one host
/// workspace, because the host is a hub rather than a peer — each has its own
/// ledger against the host and there is never a guest↔guest comparison.
#[derive(Default)]
pub struct WorkspaceSyncers {
    running: Mutex<HashMap<String, Running>>,
}

struct Running {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl WorkspaceSyncers {
    /// Start (or restart) the syncer for one machine. Replacing a running one
    /// is the restart path: the old loop is stopped first, so two loops never
    /// write the same ledger.
    pub async fn start(
        &self,
        workspace: Workspace,
        sessions: Arc<dyn GuestSessions>,
        events: Arc<EventLog>,
    ) {
        self.stop(&workspace.machine).await;
        let (stop, halted) = watch::channel(false);
        let machine = workspace.machine.clone();
        let task = tokio::spawn(run(workspace, sessions, events, halted));
        self.running
            .lock()
            .await
            .insert(machine, Running { stop, task });
    }

    /// Stop one machine's syncer and wait for its current work to unwind.
    pub async fn stop(&self, machine: &str) {
        let running = self.running.lock().await.remove(machine);
        if let Some(running) = running {
            let _ = running.stop.send(true);
            let _ = running.task.await;
        }
    }

    /// Stop every syncer in the lab — `down` and `destroy`.
    pub async fn stop_all(&self) {
        let all: Vec<String> = self.running.lock().await.keys().cloned().collect();
        for machine in all {
            self.stop(&machine).await;
        }
    }

    #[cfg(test)]
    pub async fn is_running(&self, machine: &str) -> bool {
        self.running.lock().await.contains_key(machine)
    }
}

/// The loop. Runs until `stop` flips, whatever the machine or the channel
/// does in the meantime.
async fn run(
    workspace: Workspace,
    sessions: Arc<dyn GuestSessions>,
    events: Arc<EventLog>,
    mut stop: watch::Receiver<bool>,
) {
    let mut ledger = Ledger::load(
        &workspace.ledger_path,
        &workspace.host_root,
        &workspace.guest_root,
    );
    // The watcher outlives individual passes: stopping it would guarantee a
    // full rescan on the next one, and the pending set is what keeps the
    // window small.
    let watch = match HostWatch::start() {
        Ok(watch) => watch,
        Err(e) => {
            events.emit(
                "workspace.stopped",
                json!({"machine": workspace.machine, "reason": format!("{e:#}")}),
            );
            return;
        }
    };
    let mut watch = watch;
    let mut debounce = Debounce::new(QUIET);
    // The seed: the first pass, and nothing else.
    let mut due = true;

    loop {
        if *stop.borrow() {
            return;
        }
        if due {
            due = false;
            match pass(
                &workspace,
                sessions.as_ref(),
                &events,
                &watch,
                &debounce,
                &mut ledger,
            )
            .await
            {
                Ok(()) => {}
                Err(e) => {
                    // A pass that could not reach the guest agreed to nothing,
                    // so the next one starts over — which is what "resume is
                    // re-transfer" means in practice.
                    events.emit(
                        "workspace.deferred",
                        json!({
                            "machine": workspace.machine,
                            "reason": format!("{e:#}"),
                            "retry_in_s": RETRY.as_secs(),
                        }),
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(RETRY) => {}
                        _ = stop.changed() => return,
                    }
                    due = true;
                    continue;
                }
            }
        }

        let now = std::time::Instant::now();
        let wake = debounce.next_wake(now);
        tokio::select! {
            _ = stop.changed() => return,
            event = watch.events.recv() => match event {
                Some(HostEvent::Touched(path)) => {
                    debounce.touch(path, std::time::Instant::now());
                }
                // Coverage was lost, whichever way. The tree is walked again,
                // which is also what re-registers the watch.
                Some(HostEvent::Rescan) => due = true,
                None => return,
            },
            _ = sleep_for(wake) => {
                // Something has gone quiet: that is the pass's trigger, and
                // everything still moving stays deferred inside it.
                if !debounce.settled(std::time::Instant::now()).is_empty() {
                    due = true;
                }
            }
        }
    }
}

/// `None` means "nothing pending, wait for an event" rather than "wake now".
async fn sleep_for(wake: Option<Duration>) {
    match wake {
        Some(wake) => tokio::time::sleep(wake).await,
        None => std::future::pending().await,
    }
}

/// One reconciliation, end to end.
async fn pass(
    workspace: &Workspace,
    sessions: &dyn GuestSessions,
    events: &EventLog,
    watch: &HostWatch,
    debounce: &Debounce,
    ledger: &mut Ledger,
) -> Result<()> {
    let guest = sessions
        .open()
        .await
        .context("opening a file session as the machine's default login")?;

    let root = workspace.host_root.clone();
    let scan_ledger = ledger.clone();
    let cap = workspace.max_file_bytes;
    let (scan, ignores) = tokio::task::spawn_blocking(move || host_scan(&root, &scan_ledger, cap))
        .await
        .map_err(|e| anyhow::anyhow!("the workspace scan panicked: {e}"))?
        .with_context(|| format!("walking {}", workspace.host_root.display()))?;

    for failed in watch.register(&workspace.host_root, &scan.dirs) {
        // Never quietly: an unwatched subtree is a subtree that silently
        // stops syncing.
        events.emit(
            "workspace.unwatched",
            json!({"machine": workspace.machine, "directory": failed}),
        );
    }

    let paths: BTreeSet<String> = scan
        .tree
        .keys()
        .chain(ledger.entries.keys())
        .cloned()
        .collect();
    let probe = probe_guest(guest.as_ref(), &workspace.guest_root, &paths, ledger).await;

    // Everything neither side can be read for, plus everything still being
    // written: deferred, never guessed at.
    let mut undecided = debounce.in_flight();
    for skip in scan.skipped.iter().chain(probe.skipped.iter()) {
        events.emit(
            "workspace.skipped",
            json!({"machine": workspace.machine, "path": skip.path, "reason": skip.why}),
        );
        undecided.insert(skip.path.clone());
    }

    // The ignore rules live in the tree and change under the syncer. A ledger
    // path the developer has just ignored is not a host-side delete: it leaves
    // the ledger and both copies stay where they are.
    let guest_owned: BTreeSet<String> = ledger
        .entries
        .iter()
        .filter(|(path, agreed)| {
            ignores
                .verdict(
                    path,
                    agreed.kind == crate::labd::workspace::ledger::Kind::Dir,
                )
                .is_guest_owned()
        })
        .map(|(path, _)| path.clone())
        .collect();

    let plan = reconcile(&Inputs {
        host: &scan.tree,
        guest: &probe.tree,
        ledger,
        undecided: &undecided,
        guest_owned: &guest_owned,
        max_file_bytes: workspace.max_file_bytes,
    });

    // Before the transfer, always, and naming the file and both ways out.
    for refusal in &plan.oversize {
        events.emit(
            "workspace.refused",
            json!({
                "machine": workspace.machine,
                "path": refusal.path,
                "size": refusal.size,
                "cap": refusal.cap,
                "reason": refusal.to_string(),
            }),
        );
    }
    // Scan then report, every conflicting path in the batch: a host-side
    // `git pull` collides in batches, and one at a time would turn one pull
    // into thirty round trips. The halt this becomes is its own ticket; the
    // paths are named either way.
    if !plan.conflicts.is_empty() {
        events.emit(
            "workspace.conflict",
            json!({
                "machine": workspace.machine,
                "paths": plan.conflicts.iter().map(|c| json!({
                    "path": c.path,
                    "reason": c.kind.to_string(),
                })).collect::<Vec<_>>(),
            }),
        );
    }
    // This direction leaves guest-side changes alone. Saying so is not
    // optional: from the guest, an unpropagated edit is nothing happening.
    if !plan.pending_guest.is_empty() {
        events.emit(
            "workspace.pending",
            json!({
                "machine": workspace.machine,
                "count": plan.pending_guest.len(),
                "paths": plan.pending_guest.iter().take(10)
                    .map(|p| p.path.clone()).collect::<Vec<_>>(),
            }),
        );
    }

    let applied = apply(
        guest.as_ref(),
        &Target {
            host_root: workspace.host_root.clone(),
            guest_root: workspace.guest_root.clone(),
            // The Windows preconditions — the case-sensitivity flag, the
            // symlink warning, the guest's line-ending setting — are their
            // own ticket; the flag rides this one value when they land.
            case_sensitive_dirs: false,
        },
        &plan,
        ledger,
    )
    .await;

    for path in &applied.left_standing {
        events.emit(
            "workspace.left_standing",
            json!({
                "machine": workspace.machine,
                "path": path,
                "reason": "the host dropped this directory, but the guest still holds its own \
                           content in it",
            }),
        );
    }
    for failure in &applied.failures {
        events.emit(
            "workspace.failed",
            json!({"machine": workspace.machine, "path": failure.path, "reason": failure.why}),
        );
    }

    // Written after the applies, so every agreement in it is one whose rename
    // completed. Losing this file loses agreements, which the next pass
    // re-derives by digest; writing it early would lose work.
    let rules = ignores.digest();
    if !plan.nothing_to_record() || ledger.ignore_digest != rules {
        ledger.ignore_digest = rules;
        ledger
            .save(&workspace.ledger_path)
            .with_context(|| format!("saving {}", workspace.ledger_path.display()))?;
    }

    if applied.placed > 0 || applied.removed > 0 || applied.adopted > 0 {
        events.emit(
            "workspace.synced",
            json!({
                "machine": workspace.machine,
                "placed": applied.placed,
                "removed": applied.removed,
                "adopted": applied.adopted,
            }),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::guest::fake::FakeGuest;
    use super::*;
    use crate::labd::workspace::ledger::Ledger;

    /// One shared fake guest behind every session, so each pass sees what
    /// the last one wrote — which is what a real guest does.
    struct OneFake(Arc<FakeGuest>);

    #[async_trait]
    impl GuestSessions for OneFake {
        async fn open(&self) -> Result<Box<dyn GuestFs>> {
            Ok(Box::new(self.0.clone()))
        }
    }

    fn workspace(dir: &std::path::Path, lab_local: &std::path::Path) -> Workspace {
        Workspace {
            machine: "dev01".into(),
            host_root: dir.to_path_buf(),
            guest_root: "/src".into(),
            ledger_path: Ledger::path(lab_local, "dev01"),
            max_file_bytes: 1 << 30,
        }
    }

    /// Wait for a predicate to hold, so the test asserts on the syncer's
    /// output rather than on a sleep.
    async fn eventually(mut check: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        check()
    }

    /// The whole acceptance case in one: the declared workspace appears in
    /// the guest, and a later host-side edit lands too — with the loop owned
    /// by nothing but the lab daemon.
    #[tokio::test]
    async fn a_workspace_seeds_and_then_follows_host_side_edits() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        syncers
            .start(
                workspace(dir.path(), state.path()),
                Arc::new(OneFake(guest.clone())),
                events,
            )
            .await;

        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/src/main.rs").is_some()).await
        };
        assert!(seeded, "the seed never landed: {:?}", guest.paths());
        assert!(syncers.is_running("dev01").await);

        std::fs::write(dir.path().join("src/main.rs"), "fn main() { work() }").unwrap();
        let followed = {
            let guest = guest.clone();
            eventually(move || {
                guest.text("/src/src/main.rs").as_deref() == Some("fn main() { work() }")
            })
            .await
        };
        assert!(followed, "the host-side edit never propagated");

        syncers.stop("dev01").await;
        assert!(!syncers.is_running("dev01").await);
    }

    /// The ledger is written where `destroy` will wipe it, and it survives a
    /// restart of the syncer — a settled workspace transfers nothing.
    #[tokio::test]
    async fn the_ledger_lands_in_lab_local_state_and_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        syncers
            .start(
                workspace(dir.path(), state.path()),
                Arc::new(OneFake(guest.clone())),
                events.clone(),
            )
            .await;

        let path = Ledger::path(state.path(), "dev01");
        let landed = {
            let path = path.clone();
            eventually(move || path.is_file()).await
        };
        assert!(landed, "no ledger was written");
        syncers.stop("dev01").await;

        let ledger = Ledger::load(&path, dir.path(), "/src");
        assert!(ledger.entries.contains_key("a.txt"));

        // Restarting writes nothing new: both sides are already agreed.
        let before = guest.writes().len();
        syncers
            .start(
                workspace(dir.path(), state.path()),
                Arc::new(OneFake(guest.clone())),
                events,
            )
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        syncers.stop("dev01").await;
        assert_eq!(
            guest.writes().len(),
            before,
            "a settled workspace re-pushed"
        );
    }
}
