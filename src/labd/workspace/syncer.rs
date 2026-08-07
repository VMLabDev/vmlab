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
//! One pass is: walk the host, learn what the guest holds, reconcile, apply,
//! save the ledger. **The seed is simply the first pass** — an empty guest
//! tree needs no separate mechanism.
//!
//! ### How each side's changes arrive
//!
//! The host's come from vmlab's own watcher, per-path debounced. The guest's
//! come from the agent's **dirty set**, which the host drains into a pending
//! set of its own — debounced by the same rule, because an editor guest-side
//! writes a temp and renames it just as one host-side does, and reading a file
//! mid-write here writes a torn version over the *canonical* copy.
//!
//! Which question the guest is asked is the difference between the steady
//! state and the exception path, and there are exactly two:
//!
//! - the steady state **probes named paths** — the host's tree, the ledger's,
//!   and what the drain reported;
//! - a **watch discontinuity** takes the stat-walk. A discontinuity is any
//!   watch (re)open, an overflow, or a channel that died — which is the whole
//!   list, and is why there is no resync token: it would be surface with no
//!   consumer.
//!
//! **The rescan is a barrier in both directions.** Between an overflow and the
//! completed walk the host does not know the guest moved, so propagating
//! host→guest in the meantime would see *host changed, guest unchanged* and
//! overwrite guest work silently, through the ledger, with no conflict ever
//! raised. It is a **deferral** rather than a halt: no developer action, no
//! resolution, and it clears itself.
//!
//! A **fresh file session per pass** is deliberate. It costs one round trip
//! per debounce window and it makes §19.6's resume rule structural rather than
//! remembered: there is no transfer state to resume *from*, so a dropped
//! channel can only be answered by re-transferring, and an agent restart is
//! answered by the next pass's walk. The **watch** is the opposite: it outlives
//! every pass, because closing it between them would guarantee a stat-walk on
//! the next one, which is the cost the dirty set exists to avoid.
//!
//! Everything this loop declines to do, it says out loud. A path skipped, a
//! file the size guard refused, a conflict, a rescan it is waiting on: each
//! reaches the event feed by name. From inside the guest a syncer that has
//! quietly stopped looks exactly like one with nothing to do, and that is the
//! failure ADR-0014 exists to rule out.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{Mutex, watch};

use super::apply::{Target, apply};
use super::guest::{GuestFs, GuestWatch};
use super::ledger::{Kind, Ledger};
use super::plan::{Inputs, reconcile};
use super::scan::{guest_walk, host_scan, probe_guest};
use super::watcher::{Debounce, HostEvent, HostWatch, QUIET};
use super::windows::{GuestRun, Learned, Preconditions, prepare_root, set_line_endings};
use crate::labd::events::EventLog;
use crate::labd::vm_agent::WatchReport;

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
    /// What this guest costs the syncer (§19.6), resolved from the
    /// declaration before the loop starts so no pass has to ask "am I on
    /// Windows?" — and so the two degradations a non-elevated login brings are
    /// said **up front** rather than discovered at a random path hours in.
    pub preconditions: Preconditions,
}

/// How the syncer reaches a guest. A trait rather than a machine handle
/// because what the syncer needs of one is exactly three things and nothing
/// else about a machine: a file session as the default login and a watch on
/// the tree, which are the whole sync loop, and — through [`GuestRun`] — the
/// one command §19.6's Windows preconditions run before it.
#[async_trait]
pub trait GuestSessions: GuestRun {
    /// Open a file session as the machine's **default login**.
    async fn open(&self) -> Result<Box<dyn GuestFs>>;

    /// Open a watch on `root`, registering no watcher under any prefix in
    /// `prune`.
    ///
    /// It carries no login: a watcher *observes* — it produces none of the
    /// developer's files — so §19.2 puts it on the agent identity, which also
    /// makes its coverage a superset of what the login can read rather than
    /// bounded by it. The reciprocal is that a drained path the login cannot
    /// open is a named skip when the pass goes to read it.
    async fn watch(&self, root: &str, prune: Vec<String>) -> Result<Box<dyn GuestWatch>>;
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

/// What woke the loop.
enum Woke {
    Stopped,
    Host(Option<HostEvent>),
    Guest(Option<WatchReport>),
    Quiet,
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
    // Before anything else, and before any of it can fail at a random path:
    // the two ways a non-elevated Windows login degrades the workspace.
    for degradation in workspace.preconditions.degradations() {
        events.emit(
            "workspace.degraded",
            json!({"machine": workspace.machine, "reason": degradation}),
        );
    }
    // What the declaration says, corrected by what the guest actually does —
    // which every pass gets the chance to correct, because a machine still
    // provisioning is the normal state to find rather than one to give up on.
    let mut learned = Learned::from(workspace.preconditions);
    // The watcher outlives individual passes: stopping it would guarantee a
    // full rescan on the next one, and the pending set is what keeps the
    // window small.
    let host_watch = match HostWatch::start() {
        Ok(watch) => watch,
        Err(e) => {
            events.emit(
                "workspace.stopped",
                json!({"machine": workspace.machine, "reason": format!("{e:#}")}),
            );
            return;
        }
    };
    let mut host_watch = host_watch;
    let mut host_debounce = Debounce::new(QUIET);
    // The guest's, by the same rule and for the same reason: an editor
    // guest-side writes a temp and renames it too.
    let mut guest_debounce = Debounce::new(QUIET);
    // Guest paths that have gone quiet and are owed a reconciliation. Held
    // across a failed pass rather than dropped: a burst de-prioritises, it
    // never loses a save.
    let mut owed: BTreeSet<String> = BTreeSet::new();

    // The prune list the watch is open with. Remembered in the ledger because
    // it has to be known *before* the watch opens, and the walk that computes
    // it happens inside a pass — so a restart re-opens on the list it last
    // computed rather than registering a dependency tree it already knows to
    // skip.
    let mut prune = ledger.prune.clone();
    let mut guest_watch: Option<Box<dyn GuestWatch>> = None;
    // A fresh run is a watch discontinuity by definition: nothing has been
    // drained yet, and the guest may have moved under a ledger that predates
    // this process.
    let mut rescan = true;
    let mut due = true;

    loop {
        if *stop.borrow() {
            return;
        }
        if guest_watch.is_none() {
            match sessions.watch(&workspace.guest_root, prune.clone()).await {
                Ok(watch) => {
                    guest_watch = Some(watch);
                    // Every (re)open is a discontinuity: what happened while
                    // there was no watch is exactly what the walk is for.
                    rescan = true;
                    due = true;
                }
                Err(e) => {
                    events.emit(
                        "workspace.deferred",
                        json!({
                            "machine": workspace.machine,
                            "reason": format!("watching the guest tree: {e:#}"),
                            "retry_in_s": RETRY.as_secs(),
                        }),
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(RETRY) => continue,
                        _ = stop.changed() => return,
                    }
                }
            }
        }

        if due {
            due = false;
            let drained = std::mem::take(&mut owed);
            match pass(
                &workspace,
                sessions.as_ref(),
                &events,
                &host_watch,
                &Pending {
                    guest_dirty: &drained,
                    in_flight: host_debounce
                        .in_flight()
                        .union(&guest_debounce.in_flight())
                        .cloned()
                        .collect(),
                    rescan,
                },
                &mut ledger,
                &mut learned,
            )
            .await
            {
                Ok(done) => {
                    // The barrier lifts only on a completed walk.
                    rescan = false;
                    // Nothing the pass declined to decide is forgotten.
                    owed.extend(done.deferred);
                    if done.prune != prune {
                        // The rules changed under the syncer, so the guest is
                        // watching the wrong shape of tree. Reopening is a
                        // discontinuity, which is what re-establishes
                        // agreement.
                        prune = done.prune;
                        guest_watch = None;
                        continue;
                    }
                }
                Err(e) => {
                    // A pass that could not reach the guest agreed to nothing,
                    // so the next one starts over — which is what "resume is
                    // re-transfer" means in practice. What it drained is owed
                    // again, not lost.
                    owed = drained;
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

        let now = Instant::now();
        let wake = match (host_debounce.next_wake(now), guest_debounce.next_wake(now)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (wake, None) | (None, wake) => wake,
        };
        let woke = tokio::select! {
            _ = stop.changed() => Woke::Stopped,
            event = host_watch.events.recv() => Woke::Host(event),
            report = next_report(&mut guest_watch) => Woke::Guest(report),
            _ = sleep_for(wake) => Woke::Quiet,
        };
        match woke {
            Woke::Stopped => return,
            Woke::Host(Some(HostEvent::Touched(path))) => {
                host_debounce.touch(path, Instant::now());
            }
            // Coverage was lost host-side, whichever way. The tree is walked
            // again, which is also what re-registers the watch.
            Woke::Host(Some(HostEvent::Rescan)) => due = true,
            Woke::Host(None) => return,
            Woke::Guest(Some(report)) => {
                match report {
                    // The set went empty → non-empty. Draining now is what
                    // makes an idle workspace feel immediate; under load the
                    // set batches itself and this fires once.
                    WatchReport::Dirty => {
                        if let Some(watch) = &guest_watch
                            && let Err(e) = watch.drain().await
                        {
                            drop_watch(&mut guest_watch, &mut rescan, &events, &workspace, e);
                        }
                    }
                    // Each record carries the path's stat, and only the path
                    // is kept. Two reasons, both about *when* and *as whom*
                    // it was taken: the debounce means the pass reads it a
                    // quiet period later, by which time the stat describes a
                    // file that was still being written; and the watch runs
                    // as the agent identity, so its stat can describe a file
                    // the login the pass transfers under cannot even open.
                    // The pass re-reads both, and that is where the named
                    // skip comes from.
                    WatchReport::Batch(entries) => {
                        let now = Instant::now();
                        for entry in entries {
                            guest_debounce.touch(entry.path, now);
                        }
                    }
                    // Overflow. It **warns and never halts** — a build burst
                    // is wanted work that happens to be large, and halting
                    // would let a compile stop the dev machine — but it does
                    // block both directions until the walk completes.
                    WatchReport::Rescan => {
                        events.emit(
                            "workspace.rescan",
                            json!({
                                "machine": workspace.machine,
                                "reason": "the guest's watch lost coverage, so the guest tree is \
                                           walked again; both directions wait for the walk rather \
                                           than propagating over changes the host cannot see yet",
                            }),
                        );
                        rescan = true;
                        due = true;
                    }
                    WatchReport::Error(msg) => {
                        drop_watch(
                            &mut guest_watch,
                            &mut rescan,
                            &events,
                            &workspace,
                            anyhow::anyhow!(msg),
                        );
                    }
                }
            }
            // The channel is gone. Reopening it is a discontinuity, so the
            // next pass walks — which is how the loss self-heals with no ack
            // and no resync token.
            Woke::Guest(None) => {
                guest_watch = None;
                rescan = true;
            }
            Woke::Quiet => {
                // Something has gone quiet: that is the pass's trigger, and
                // everything still moving stays deferred inside it.
                let now = Instant::now();
                if !host_debounce.settled(now).is_empty() {
                    due = true;
                }
                let ready = guest_debounce.settled(now);
                if !ready.is_empty() {
                    owed.extend(ready);
                    due = true;
                }
            }
        }
    }
}

/// Report the watch as gone and arrange for the walk that re-establishes
/// agreement. Never a halt: the guest kept running, and what it did while the
/// channel was down is exactly what a stat-walk answers.
fn drop_watch(
    guest_watch: &mut Option<Box<dyn GuestWatch>>,
    rescan: &mut bool,
    events: &EventLog,
    workspace: &Workspace,
    why: anyhow::Error,
) {
    *guest_watch = None;
    *rescan = true;
    events.emit(
        "workspace.rescan",
        json!({
            "machine": workspace.machine,
            "reason": format!("the guest's watch channel failed ({why:#}), so the guest tree is \
                               walked again once it reopens"),
        }),
    );
}

/// The next thing the guest's watch says, or nothing at all while there is no
/// watch to say it — the loop reopens one at the top rather than here.
async fn next_report(watch: &mut Option<Box<dyn GuestWatch>>) -> Option<WatchReport> {
    match watch {
        Some(watch) => watch.recv().await,
        None => std::future::pending().await,
    }
}

/// `None` means "nothing pending, wait for an event" rather than "wake now".
async fn sleep_for(wake: Option<Duration>) {
    match wake {
        Some(wake) => tokio::time::sleep(wake).await,
        None => std::future::pending().await,
    }
}

/// §19.6's Windows actions, ahead of the pass that depends on them.
///
/// **Before the plan, not after the applies.** Whether this guest will really
/// make a directory case-sensitive is what decides whether a case collision is
/// an ordinary pair of paths or a refusal, and a syncer that discovered the
/// answer from a failed `mkdir` halfway through would already have landed one
/// of the pair on top of the other. So the workspace root is prepared and the
/// flag is probed first, and the plan is computed from what came back.
///
/// The line-ending setting is attempted every pass until it takes, because a
/// machine whose git arrives later in `provision {}` is the normal case and
/// giving up once would leave the tree quietly converting for the life of the
/// machine. Its warning is said once, not once a pass.
async fn preconditions(
    workspace: &Workspace,
    sessions: &dyn GuestSessions,
    guest: &dyn GuestFs,
    events: &EventLog,
    learned: &mut Learned,
) -> Result<()> {
    let available = prepare_root(
        guest,
        &workspace.guest_root,
        workspace.preconditions.case_sensitive_dirs,
    )
    .await?;
    if available != learned.case_sensitive_dirs {
        learned.case_sensitive_dirs = available;
        if !available {
            events.emit(
                "workspace.degraded",
                json!({
                    "machine": workspace.machine,
                    "path": workspace.guest_root,
                    "reason": "this guest will not make a directory case-sensitive, so two host \
                               paths differing only in case cannot both land and are refused by \
                               name",
                }),
            );
        }
    }

    if !learned.line_endings_off {
        match set_line_endings(sessions).await {
            None => learned.line_endings_off = true,
            Some(why) if !learned.line_endings_said => {
                learned.line_endings_said = true;
                events.emit(
                    "workspace.degraded",
                    json!({
                        "machine": workspace.machine,
                        "reason": format!(
                            "the guest's line-ending conversion is not off yet, so a guest-side \
                             checkout would rewrite the whole tree to CRLF and sync every file \
                             back as modified — retried every pass until it takes: {why}"
                        ),
                    }),
                );
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// What the loop knows that the pass cannot work out for itself.
struct Pending<'a> {
    /// Guest paths the drain reported and the debounce has let settle.
    guest_dirty: &'a BTreeSet<String>,
    /// Paths on either side that are still moving. Deferred, never guessed
    /// at: a file being written is not a file to read.
    in_flight: BTreeSet<String>,
    /// Take the stat-walk rather than probing named paths, and do not
    /// propagate anything until it has completed.
    rescan: bool,
}

/// What a completed pass tells the loop.
struct Passed {
    /// The prune list as the rules now stand. A change reopens the watch.
    prune: Vec<String>,
    /// Drained paths this pass declined to decide — still moving, or a named
    /// skip. Handed back so they are **de-prioritised rather than dropped**:
    /// a path nothing touches again would otherwise wait for a discontinuity
    /// to be noticed at all.
    deferred: BTreeSet<String>,
}

/// One reconciliation, end to end.
async fn pass(
    workspace: &Workspace,
    sessions: &dyn GuestSessions,
    events: &EventLog,
    watch: &HostWatch,
    pending: &Pending<'_>,
    ledger: &mut Ledger,
    learned: &mut Learned,
) -> Result<Passed> {
    let guest = sessions
        .open()
        .await
        .context("opening a file session as the machine's default login")?;
    preconditions(workspace, sessions, guest.as_ref(), events, learned).await?;

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

    let mut probe = if pending.rescan {
        guest_walk(
            guest.as_ref(),
            &workspace.guest_root,
            &ignores,
            ledger,
            workspace.max_file_bytes,
        )
        .await
        .with_context(|| format!("walking the guest tree at {}", workspace.guest_root))?
    } else {
        // What the drain reported joins the host's own set and the ledger's.
        // The obvious exclusions go before the round trip; the rest is
        // decided below, once the kind is known.
        let paths: BTreeSet<String> = scan
            .tree
            .keys()
            .chain(ledger.entries.keys())
            .cloned()
            .chain(
                pending
                    .guest_dirty
                    .iter()
                    .filter(|path| !ignores.verdict(path, false).is_guest_owned())
                    .cloned(),
            )
            .collect();
        probe_guest(
            guest.as_ref(),
            &workspace.guest_root,
            &paths,
            ledger,
            workspace.max_file_bytes,
        )
        .await
    };

    // **Filtering stays host-side**, and it happens *on receipt*: the guest
    // reported what it holds and was never asked to decide whether any of it
    // is in the synced set. That decision is the ignore set, and a guest that
    // held half of it would leave build output in one tree and out of the
    // other. It needs the entry's kind, which is why it is here and not in
    // the path set above — `build/` says nothing about a *file* called
    // `build`.
    probe.tree.retain(|path, state| {
        !ignores
            .verdict(path, state.kind == Kind::Dir)
            .is_guest_owned()
    });

    // Everything neither side can be read for, plus everything still being
    // written: deferred, never guessed at.
    let mut undecided = pending.in_flight.clone();
    for skip in scan.skipped.iter().chain(probe.skipped.iter()) {
        events.emit(
            "workspace.skipped",
            json!({"machine": workspace.machine, "path": skip.path, "reason": skip.why}),
        );
        undecided.insert(skip.path.clone());
    }

    // The ignore rules live in the tree and change under the syncer. A ledger
    // path the developer has just ignored is not a delete on either side: it
    // leaves the ledger and both copies stay where they are.
    let guest_owned: BTreeSet<String> = ledger
        .entries
        .iter()
        .filter(|(path, agreed)| {
            ignores
                .verdict(path, agreed.kind == Kind::Dir)
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
        case_folding: learned.case_folding(),
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
    // Refuse-at-seed, loudly, naming every path: where the case-sensitivity
    // flag could not be set, a collision is the one thing that must never be
    // allowed to happen quietly.
    // Its own event rather than the size guard's: the two refusals name
    // different things and offer different ways out, and one payload shape
    // that sometimes has a `size` is how a surface ends up sniffing keys.
    for collision in &plan.collisions {
        events.emit(
            "workspace.case_collision",
            json!({
                "machine": workspace.machine,
                "paths": collision.paths,
                "reason": collision.to_string(),
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

    let applied = apply(
        guest.as_ref(),
        &Target {
            host_root: workspace.host_root.clone(),
            guest_root: workspace.guest_root.clone(),
            case_sensitive_dirs: learned.case_sensitive_dirs,
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
                "reason": "one side dropped this directory, but the other still holds its own \
                           content in it",
            }),
        );
    }
    // Attempted, and named where it did not take: §19.4 makes a
    // symlink-capable image a precondition, and vmlab does not work around it
    // silently.
    for refused in &applied.symlinks_refused {
        events.emit(
            "workspace.symlink_refused",
            json!({"machine": workspace.machine, "path": refused.path, "reason": refused.why}),
        );
    }
    // One directory the guest would not take the flag on, where the probe
    // said it would. Rare enough to be worth naming individually — the
    // machine-wide answer was already settled before the plan.
    for degraded in &applied.case_insensitive_dirs {
        learned.case_sensitive_dirs = false;
        events.emit(
            "workspace.degraded",
            json!({"machine": workspace.machine, "path": degraded.path, "reason": degraded.why}),
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
    let prune = ignores.prune_list(&scan.pruned);
    if !plan.nothing_to_record() || ledger.ignore_digest != rules || ledger.prune != prune {
        ledger.ignore_digest = rules;
        ledger.prune = prune.clone();
        ledger
            .save(&workspace.ledger_path)
            .with_context(|| format!("saving {}", workspace.ledger_path.display()))?;
    }

    if applied.moved() {
        events.emit(
            "workspace.synced",
            json!({
                "machine": workspace.machine,
                "guest_placed": applied.to_guest.placed,
                "guest_removed": applied.to_guest.removed,
                "host_placed": applied.to_host.placed,
                "host_removed": applied.to_host.removed,
                "adopted": applied.adopted,
            }),
        );
    }
    Ok(Passed {
        prune,
        deferred: pending
            .guest_dirty
            .intersection(&undecided)
            .cloned()
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::guest::fake::{FakeGuest, FakeWatcher};
    use super::*;
    use crate::labd::workspace::ledger::Ledger;

    /// One shared fake guest behind every session, so each pass sees what
    /// the last one wrote — which is what a real guest does — and one shared
    /// watcher, so a test can say "the guest noticed this" to a watch the
    /// syncer is already holding.
    struct OneFake {
        guest: Arc<FakeGuest>,
        watcher: Arc<FakeWatcher>,
        /// The prune list of every watch that has been opened, in order.
        opens: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl OneFake {
        fn new(guest: Arc<FakeGuest>) -> Arc<OneFake> {
            let watcher = FakeWatcher::new(guest.clone(), "/src");
            Arc::new(OneFake {
                guest,
                watcher,
                opens: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn opens(&self) -> Vec<Vec<String>> {
            self.opens.lock().expect("opens").clone()
        }
    }

    #[async_trait]
    impl GuestSessions for OneFake {
        async fn open(&self) -> Result<Box<dyn GuestFs>> {
            Ok(Box::new(self.guest.clone()))
        }

        async fn watch(&self, _root: &str, prune: Vec<String>) -> Result<Box<dyn GuestWatch>> {
            self.opens.lock().expect("opens").push(prune);
            Ok(self.watcher.session())
        }
    }

    #[async_trait]
    impl GuestRun for OneFake {
        async fn run(&self, argv: Vec<String>) -> Result<super::super::windows::Ran> {
            let ok = self.guest.ran(argv);
            Ok(super::super::windows::Ran {
                exit_code: if ok { 0 } else { 127 },
                stderr: if ok {
                    String::new()
                } else {
                    "'git' is not recognized".into()
                },
            })
        }
    }

    fn workspace(dir: &std::path::Path, lab_local: &std::path::Path) -> Workspace {
        Workspace {
            machine: "dev01".into(),
            host_root: dir.to_path_buf(),
            guest_root: "/src".into(),
            ledger_path: Ledger::path(lab_local, "dev01"),
            max_file_bytes: 1 << 30,
            preconditions: Preconditions::default(),
        }
    }

    /// The same, on a machine whose guest family and login make §19.6's three
    /// Windows actions apply.
    fn windows_workspace(
        dir: &std::path::Path,
        lab_local: &std::path::Path,
        windows: Preconditions,
    ) -> Workspace {
        Workspace {
            preconditions: windows,
            ..workspace(dir, lab_local)
        }
    }

    /// Every event the lab emitted, as `(name, data)`.
    fn events_of(state: &std::path::Path) -> Vec<(String, serde_json::Value)> {
        let path = state.join("events.jsonl");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .map(|ev| {
                (
                    ev["event"].as_str().unwrap_or_default().to_string(),
                    ev["data"].clone(),
                )
            })
            .collect()
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
                OneFake::new(guest.clone()),
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

    /// The other direction, and the one where authoring actually happens: the
    /// developer is attached *into* the guest, so a guest-side save has to
    /// reach the canonical copy through the same ledger discipline.
    #[tokio::test]
    async fn a_guest_side_edit_reaches_the_canonical_copy() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let sessions = OneFake::new(guest.clone());
        syncers
            .start(
                workspace(dir.path(), state.path()),
                sessions.clone(),
                events,
            )
            .await;

        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/main.rs").is_some()).await
        };
        assert!(seeded, "the seed never landed");

        // The developer types, guest-side, and the guest's watcher notices.
        guest.file("/src/main.rs", "fn main() { typed_in_the_guest() }", 42);
        guest.file("/src/new.rs", "fn new() {}", 42);
        sessions.watcher.mark("main.rs");
        sessions.watcher.mark("new.rs");

        let landed = {
            let dir = dir.path().to_path_buf();
            eventually(move || {
                std::fs::read_to_string(dir.join("main.rs")).unwrap_or_default()
                    == "fn main() { typed_in_the_guest() }"
                    && dir.join("new.rs").is_file()
            })
            .await
        };
        assert!(landed, "the guest-side edit never reached the host");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.rs")).unwrap(),
            "fn new() {}"
        );
        syncers.stop("dev01").await;

        // It arrived through the drain rather than through a walk: the steady
        // state asks about named paths, and the stat-walk is the exception.
        assert!(sessions.watcher.drains() > 0, "nothing was ever drained");

        // Under the same ledger discipline: the agreement records each side
        // from that side.
        let ledger = Ledger::load(&Ledger::path(state.path(), "dev01"), dir.path(), "/src");
        assert_eq!(ledger.entries["new.rs"].guest.mtime_ns, 42);
        assert_ne!(ledger.entries["new.rs"].host.mtime_ns, 42);
    }

    /// **The guest is never asked to decide**, and the answer it gives is
    /// filtered host-side on receipt. A build writing into a guest-owned
    /// directory reaches the drain like anything else, and it is the host that
    /// declines to carry it — which is what keeps build output out of the
    /// canonical tree without the guest holding any part of the ignore set.
    #[tokio::test]
    async fn a_guest_owned_path_the_guest_reported_is_filtered_host_side() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "build/\n*.log\n").unwrap();
        std::fs::write(dir.path().join("app.rs"), "x").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let sessions = OneFake::new(guest.clone());
        syncers
            .start(
                workspace(dir.path(), state.path()),
                sessions.clone(),
                events,
            )
            .await;
        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/app.rs").is_some()).await
        };
        assert!(seeded);

        // A guest-side build, and a real edit alongside it.
        guest.dir("/src/build");
        guest.file("/src/build/app.o", "object code", 9);
        guest.file("/src/debug.log", "noise", 9);
        guest.file("/src/app.rs", "edited", 9);
        for path in ["build", "build/app.o", "debug.log", "app.rs"] {
            sessions.watcher.mark(path);
        }

        let edited = {
            let dir = dir.path().to_path_buf();
            eventually(move || {
                std::fs::read_to_string(dir.join("app.rs")).unwrap_or_default() == "edited"
            })
            .await
        };
        assert!(edited, "the real edit never landed");
        assert!(!dir.path().join("build").exists(), "build output crossed");
        assert!(!dir.path().join("debug.log").exists());
        syncers.stop("dev01").await;
    }

    /// A channel that dies is a **watch discontinuity** like any other: the
    /// watch reopens and the next pass walks, because the guest kept running
    /// and may have moved underneath us. That is the whole recovery — there is
    /// no ack for a batch and no resync token, because the loss self-heals
    /// through a path that has to exist anyway.
    #[tokio::test]
    async fn a_dropped_watch_channel_reopens_and_walks() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "a").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let sessions = OneFake::new(guest.clone());
        syncers
            .start(
                workspace(dir.path(), state.path()),
                sessions.clone(),
                events,
            )
            .await;
        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/a.rs").is_some()).await
        };
        assert!(seeded);

        // The agent restarted. What the guest did while the channel was down
        // was reported to nobody.
        guest.file("/src/while-down.rs", "written with no watch", 55);
        sessions.watcher.fail("the agent restarted");

        let found = {
            let dir = dir.path().to_path_buf();
            eventually(move || dir.join("while-down.rs").is_file()).await
        };
        assert!(found, "the reopen never walked, so the change never synced");
        assert!(
            sessions.opens().len() > 1,
            "the watch never reopened: {:?}",
            sessions.opens()
        );
        syncers.stop("dev01").await;
    }

    /// A guest-side delete propagates, and a *directory* delete expands
    /// through the **ledger** rather than through an event stream — the two
    /// platforms disagree about whether children are reported at all, and the
    /// ledger knows exactly what was agreed to be in there.
    #[tokio::test]
    async fn a_guest_side_directory_delete_expands_through_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("old")).unwrap();
        std::fs::write(dir.path().join("old/a.rs"), "a").unwrap();
        std::fs::write(dir.path().join("old/b.rs"), "b").unwrap();
        std::fs::write(dir.path().join("keep.rs"), "keep").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let sessions = OneFake::new(guest.clone());
        syncers
            .start(
                workspace(dir.path(), state.path()),
                sessions.clone(),
                events,
            )
            .await;

        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/old/b.rs").is_some()).await
        };
        assert!(seeded, "the seed never landed: {:?}", guest.paths());

        // `rm -rf old` in the guest. One platform reports the children, the
        // other reports the directory — so the host is told only the
        // directory, which is the harder of the two.
        for path in ["/src/old/a.rs", "/src/old/b.rs", "/src/old"] {
            guest.unlink(path);
        }
        sessions.watcher.mark("old");

        let gone = {
            let dir = dir.path().to_path_buf();
            eventually(move || !dir.join("old").exists()).await
        };
        assert!(gone, "the guest-side delete never reached the host");
        assert!(
            dir.path().join("keep.rs").exists(),
            "it took the wrong tree"
        );
        syncers.stop("dev01").await;
    }

    /// An overflow **warns and never halts** — a build burst is wanted work
    /// that happens to be large — but it *is* a barrier: nothing propagates
    /// in either direction until the walk that re-establishes agreement has
    /// completed, or the host would see "host changed, guest unchanged" and
    /// overwrite guest work through the ledger with no conflict raised.
    #[tokio::test]
    async fn an_overflow_forces_a_walk_that_finds_what_the_events_lost() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "a").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, mut rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let sessions = OneFake::new(guest.clone());
        syncers
            .start(
                workspace(dir.path(), state.path()),
                sessions.clone(),
                events,
            )
            .await;
        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/a.rs").is_some()).await
        };
        assert!(seeded);

        // A build burst: the guest wrote far more than its set could hold, so
        // no path was reported at all. Only the walk can find this.
        guest.file("/src/burst.rs", "written during the burst", 77);
        sessions.watcher.overflow();

        let found = {
            let dir = dir.path().to_path_buf();
            eventually(move || dir.join("burst.rs").is_file()).await
        };
        assert!(found, "the walk never ran, so the lost path never synced");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("burst.rs")).unwrap(),
            "written during the burst"
        );

        let mut warned = false;
        while let Ok(event) = rx.try_recv() {
            warned |= event.event == "workspace.rescan";
        }
        assert!(warned, "the overflow was not reported");
        syncers.stop("dev01").await;
    }

    /// The prune list is computed host-side and handed to the guest, which is
    /// never asked to decide anything. A rules change reopens the watch on
    /// the new list, because a prefix cannot be edited in place.
    #[tokio::test]
    async fn the_watch_opens_on_a_host_computed_prune_list_and_reopens_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::write(dir.path().join("app.js"), "x").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let sessions = OneFake::new(guest.clone());
        syncers
            .start(
                workspace(dir.path(), state.path()),
                sessions.clone(),
                events,
            )
            .await;

        let pruned = {
            let sessions = sessions.clone();
            eventually(move || {
                sessions
                    .opens()
                    .iter()
                    .any(|p| p == &["node_modules".to_string()])
            })
            .await
        };
        assert!(
            pruned,
            "the guest was never handed the prune list: {:?}",
            sessions.opens()
        );

        std::fs::write(dir.path().join(".gitignore"), "node_modules/\ntarget/\n").unwrap();
        let reopened = {
            let sessions = sessions.clone();
            eventually(move || {
                sessions
                    .opens()
                    .iter()
                    .any(|p| p == &["node_modules".to_string(), "target".to_string()])
            })
            .await
        };
        assert!(
            reopened,
            "the watch never reopened on the new rules: {:?}",
            sessions.opens()
        );
        syncers.stop("dev01").await;
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
                OneFake::new(guest.clone()),
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

        // Restarting places nothing new: both sides are already agreed. Only
        // the temp names count — the root is re-asserted every pass, which is
        // one idempotent `mkdir` rather than a transfer.
        let placed = |writes: Vec<String>| {
            writes
                .into_iter()
                .filter(|w| w.contains(".vmlab-sync."))
                .count()
        };
        let before = placed(guest.writes());
        syncers
            .start(
                workspace(dir.path(), state.path()),
                OneFake::new(guest.clone()),
                events,
            )
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        syncers.stop("dev01").await;
        assert_eq!(
            placed(guest.writes()),
            before,
            "a settled workspace re-pushed"
        );
    }

    /// §19.6's third Windows action: git for Windows ships `core.autocrlf`
    /// on, which would rewrite the whole tree on the first guest-side
    /// checkout and sync every file back as modified.
    #[tokio::test]
    async fn a_windows_workspace_turns_the_guests_line_ending_conversion_off() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        syncers
            .start(
                windows_workspace(
                    dir.path(),
                    state.path(),
                    Preconditions {
                        windows: true,
                        case_sensitive_dirs: true,
                        symlinks: true,
                    },
                ),
                OneFake::new(guest.clone()),
                events,
            )
            .await;

        let set = {
            let guest = guest.clone();
            eventually(move || !guest.commands().is_empty()).await
        };
        assert!(set, "the guest's git config was never touched");
        syncers.stop("dev01").await;
        assert_eq!(
            guest.commands(),
            vec![
                super::super::windows::GIT_LINE_ENDINGS
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
            ],
            "once, and nothing else"
        );
    }

    /// A guest whose git arrives later in `provision {}` is the normal case,
    /// not a failure to give up on: the setting is retried until it takes,
    /// and warned about **once** rather than once a pass.
    #[tokio::test]
    async fn the_line_ending_setting_is_retried_until_it_takes_and_warned_about_once() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let guest = Arc::new(FakeGuest::new());
        guest.fail_runs(1);
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        syncers
            .start(
                windows_workspace(
                    dir.path(),
                    state.path(),
                    Preconditions {
                        windows: true,
                        case_sensitive_dirs: true,
                        symlinks: true,
                    },
                ),
                OneFake::new(guest.clone()),
                events,
            )
            .await;
        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/a.txt").is_some()).await
        };
        assert!(seeded);

        // A second pass, which is where the retry happens.
        std::fs::write(dir.path().join("a.txt"), "again").unwrap();
        let retried = {
            let guest = guest.clone();
            eventually(move || guest.commands().len() >= 2).await
        };
        assert!(retried, "the setting was attempted once and abandoned");
        // A third pass must not attempt it a third time: it took.
        std::fs::write(dir.path().join("a.txt"), "and again").unwrap();
        let followed = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/a.txt").as_deref() == Some("and again")).await
        };
        assert!(followed);
        syncers.stop("dev01").await;

        assert_eq!(guest.commands().len(), 2, "it kept trying after it took");
        assert_eq!(
            events_of(state.path())
                .iter()
                .filter(|(name, _)| name == "workspace.degraded")
                .count(),
            1,
            "warned once a pass rather than once"
        );
    }

    /// A Linux guest costs none of the three: no flag to ask for, no
    /// privilege to warn about, and git converts nothing.
    #[tokio::test]
    async fn a_linux_workspace_runs_no_guest_commands_and_reports_no_degradation() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        syncers
            .start(
                workspace(dir.path(), state.path()),
                OneFake::new(guest.clone()),
                events,
            )
            .await;
        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/a.txt").is_some()).await
        };
        assert!(seeded);
        syncers.stop("dev01").await;

        assert_eq!(guest.commands(), Vec::<Vec<String>>::new());
        assert!(
            !events_of(state.path())
                .iter()
                .any(|(name, _)| name == "workspace.degraded")
        );
    }

    /// **Up front, before either can fail at a random path hours in**: a
    /// login declared `elevated = false` degrades the workspace in exactly
    /// two named ways, and the syncer says both before its first pass.
    #[tokio::test]
    async fn a_non_elevated_login_reports_both_degradations_before_it_syncs_anything() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        syncers
            .start(
                windows_workspace(
                    dir.path(),
                    state.path(),
                    Preconditions {
                        windows: true,
                        case_sensitive_dirs: false,
                        symlinks: false,
                    },
                ),
                OneFake::new(guest.clone()),
                events,
            )
            .await;
        let seeded = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/a.txt").is_some()).await
        };
        assert!(seeded);
        syncers.stop("dev01").await;

        let emitted = events_of(state.path());
        let degraded: Vec<&serde_json::Value> = emitted
            .iter()
            .filter(|(name, _)| name == "workspace.degraded")
            .map(|(_, data)| data)
            .collect();
        assert_eq!(degraded.len(), 2, "{emitted:?}");
        let said = format!("{degraded:?}");
        assert!(said.contains("case-sensitive"), "{said}");
        assert!(said.contains("symlink"), "{said}");

        // And before anything was synced, not after it broke.
        let first_sync = emitted
            .iter()
            .position(|(name, _)| name == "workspace.synced")
            .expect("nothing synced");
        let last_degraded = emitted
            .iter()
            .rposition(|(name, _)| name == "workspace.degraded")
            .expect("nothing degraded");
        assert!(last_degraded < first_sync, "reported after the fact");
    }

    /// The refusal that stands in for the flag, end to end: two paths
    /// differing only in case reach the developer by name, and neither copy
    /// is written.
    #[tokio::test]
    async fn a_case_collision_reaches_the_developer_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Foo.cs"), "upper").unwrap();
        std::fs::write(dir.path().join("foo.cs"), "lower").unwrap();
        std::fs::write(dir.path().join("ok.cs"), "fine").unwrap();

        let guest = Arc::new(FakeGuest::new());
        guest.folding();
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        syncers
            .start(
                windows_workspace(
                    dir.path(),
                    state.path(),
                    Preconditions {
                        windows: true,
                        case_sensitive_dirs: false,
                        symlinks: false,
                    },
                ),
                OneFake::new(guest.clone()),
                events,
            )
            .await;
        let landed = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/ok.cs").is_some()).await
        };
        assert!(landed, "the rest of the tree never arrived");
        syncers.stop("dev01").await;

        let refused: Vec<serde_json::Value> = events_of(state.path())
            .into_iter()
            .filter(|(name, _)| name == "workspace.case_collision")
            .map(|(_, data)| data)
            .collect();
        assert!(!refused.is_empty(), "the collision was silent");
        let said = refused[0]["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(said.contains("Foo.cs") && said.contains("foo.cs"), "{said}");
        assert!(guest.get("/src/Foo.cs").is_none(), "{:?}", guest.paths());
        assert!(guest.get("/src/foo.cs").is_none());
    }

    /// **On the pass that needs it, not the one after.** A guest whose login
    /// is elevated but whose filesystem will not take the flag is only found
    /// out by asking, and asking *after* the seed would mean one of the
    /// colliding pair had already landed on top of the other.
    #[tokio::test]
    async fn a_guest_that_lies_about_the_flag_still_refuses_on_the_first_pass() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Foo.cs"), "upper").unwrap();
        std::fs::write(dir.path().join("foo.cs"), "lower").unwrap();
        std::fs::write(dir.path().join("ok.cs"), "fine").unwrap();

        let guest = Arc::new(FakeGuest::new());
        // Elevated, so the declaration promises the flag — and the guest's
        // filesystem has no concept of it.
        guest.folding().refuse_case_flag();
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        syncers
            .start(
                windows_workspace(
                    dir.path(),
                    state.path(),
                    Preconditions {
                        windows: true,
                        case_sensitive_dirs: true,
                        symlinks: true,
                    },
                ),
                OneFake::new(guest.clone()),
                events,
            )
            .await;
        let landed = {
            let guest = guest.clone();
            eventually(move || guest.text("/src/ok.cs").is_some()).await
        };
        assert!(landed);
        syncers.stop("dev01").await;

        // Neither ever reached the guest — not "the second overwrote the
        // first and the next pass complained".
        assert!(guest.get("/src/Foo.cs").is_none(), "{:?}", guest.paths());
        assert!(guest.get("/src/foo.cs").is_none());

        let emitted = events_of(state.path());
        assert!(
            emitted
                .iter()
                .any(|(name, _)| name == "workspace.case_collision"),
            "{emitted:?}"
        );
        // …and the guest disagreeing with its own declaration is itself said.
        assert!(
            emitted
                .iter()
                .any(|(name, data)| name == "workspace.degraded"
                    && data["reason"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("will not make a directory case-sensitive")),
            "{emitted:?}"
        );
    }
}
