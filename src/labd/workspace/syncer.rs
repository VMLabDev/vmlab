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
//!
//! ### Halting, and the three things that are not a halt
//!
//! A conflict stops **this machine's** workspace, both directions, until a
//! developer resolves it ([`halt`](super::halt)). Three other ways a pass can
//! decline to move a file deliberately are *not* halts, and keeping them apart
//! is most of what this loop does:
//!
//! - a **rescan** is a deferral in both directions that clears itself when the
//!   walk completes — no developer action, no resolution;
//! - a **held git lock** is timing ([`locks`](super::locks)), and clears when
//!   git lets go;
//! - **volume, a named skip and a refused symlink** warn and carry on, because
//!   a build burst, a root-owned artefact and a `.sock` in the tree are all
//!   normal and none of them may stop a dev machine.
//!
//! While halted the loop keeps its watch open and keeps draining the guest's
//! dirty set into its own pending set, so the guest's set stays small and a
//! long halt costs no rescan; every drained path stays owed, because nothing
//! was agreed about any of it. The only work a halted pass still does is work
//! that moves nothing the developer did not ask for: adopting two sides that
//! already match, and carrying out a resolution.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{Mutex, Notify, watch};

use super::apply::{Target, apply};
use super::guest::{GuestFs, GuestWatch};
use super::halt::{self, Halt};
use super::ignore::TEMP_PREFIX;
use super::ledger::{Kind, Ledger};
use super::locks;
use super::plan::{Inputs, Oversize, Volume, Winner, reconcile};
use super::scan::{Skip, guest_locks, guest_walk, host_scan, join_guest, probe_guest};
use super::watcher::{Debounce, HostEvent, HostWatch, QUIET};
use super::windows::{GuestRun, Learned, Preconditions, prepare_root, set_line_endings};
use crate::labd::events::EventLog;
use crate::labd::vm_agent::WatchReport;

/// How long to wait before retrying a pass that could not reach the guest.
/// The guest is booted and provisioned by the time the syncer starts, so a
/// failure here is a blip rather than a state to poll through.
const RETRY: Duration = Duration::from_secs(5);

/// How long a verb waits for the pass it asked for. Bounded so a machine whose
/// agent has stopped answering says so at a terminal rather than holding one.
const VERB_TIMEOUT: Duration = Duration::from_secs(120);

/// How soon a pass that deferred `.git`'s mutable set looks again.
///
/// A deferral has to re-arm the loop itself, because the thing that clears it
/// is a *deletion in the guest* and nothing guarantees the host hears about it:
/// waiting for the next unrelated edit would leave the mutable set stalled for
/// as long as the developer happened not to type. Short, because a git lock is
/// held for milliseconds and the cost of looking is one `lstat`.
const LOCK_RETRY: Duration = Duration::from_secs(1);

/// How many halted paths and named skips the projection carries.
///
/// A cap rather than the whole list because the 30 000-file case is real —
/// un-ignoring a populated `node_modules` is one `.vmlabignore` edit away — and
/// a status projection is polled. What is dropped is always *said* to have been
/// dropped, and `--all` needs no list at all.
pub const REPORTED: usize = 500;

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

/// What one machine's syncer last decided, and what a surface reads off it.
///
/// One value rather than a verb per question (ADR-0004's habit): a halted
/// workspace, a burst worth a word, a walk being waited on and a path skipped
/// by name are all *the same report*, produced once at the end of a pass. The
/// console reads it and shows the halt; the CLI reads it and offers the
/// resolution the console deliberately does not.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// The workspace is stopped, both directions, on this machine.
    pub halt: Option<Halt>,
    /// The last pass carried an unusual amount of work under one subtree.
    /// A warning: everything it counted was still carried.
    pub volume: Option<Volume>,
    /// Both directions are waiting for a stat-walk, and why — the overflow
    /// symptom, said as a state rather than left as a pause.
    pub rescan: Option<String>,
    /// Both directions are waiting for the **bracket's re-seed** (§19.6): a
    /// snapshot restore rewound the guest, and nothing propagates until the
    /// tree has been carried back to host truth.
    ///
    /// Its own field rather than [`rescan`](Report::rescan)'s because the two
    /// are opposite answers to the same pause. A rescan means *we do not know
    /// what the guest did*; a re-seed means *we know exactly, because vmlab
    /// did it* — and a surface that conflated them would tell a developer to
    /// wait for a walk that is never going to run.
    pub reseed: Option<String>,
    /// How many watch discontinuities this syncer has answered with a walk
    /// since it started. Repeated overflows are the symptom that says the
    /// guest is writing faster than the watch can report, which one event lost
    /// in a log does not.
    pub rescans: u64,
    /// Paths neither direction touched, by name — special files, and anything
    /// the syncer's login could not open.
    pub skipped: Vec<Skip>,
    /// Files the size guard refused before transfer, by name.
    pub oversize: Vec<Oversize>,
    /// `.git`'s mutable set, waiting on a lock held on one side or the other.
    pub deferred: Vec<String>,
    /// Changes the last pass did not carry across, by name — still inside a
    /// debounce window, owed behind a halt, or waiting on a lock.
    ///
    /// The question a **snapshot capture** asks before it goes ahead
    /// ([`bracket`](super::bracket)): a snapshot of a tree the canonical copy
    /// has never seen restores to somewhere meaningless. Carried in the
    /// projection too, because "how far behind is my workspace" is a question
    /// `dev sync status` should not have to be halted to answer.
    pub unsynced: Vec<String>,
    /// The last pass could not finish — a dropped channel, a guest that has
    /// stopped answering. Not a halt: nothing was agreed, so the next pass
    /// starts over.
    pub trouble: Option<String>,
    /// Passes completed since the syncer started, so a surface can tell "in
    /// step" from "has never managed one".
    pub passes: u64,
}

impl Report {
    /// The report as every surface reads it (ADR-0004).
    ///
    /// Everything needed to **show** a halt and nothing that acts on one: the
    /// console reads this and displays it, and the resolution it does not offer
    /// is spelled out in `resolve` as words rather than as a button. The
    /// path lists are capped at [`REPORTED`], with `conflicts_total` saying
    /// what the cap dropped — a truncation nobody is told about is exactly the
    /// silent-incompleteness class §19.6 keeps refusing.
    pub fn project(&self) -> crate::status::WorkspaceSyncStatus {
        crate::status::WorkspaceSyncStatus {
            halt: self.halt.as_ref().map(Halt::headline),
            conflicts: self
                .halt
                .iter()
                .flat_map(Halt::reasons)
                .take(REPORTED)
                .map(|(path, reason)| crate::status::WorkspaceConflictStatus { path, reason })
                .collect(),
            conflicts_total: self.halt.as_ref().map_or(0, |halt| halt.paths().len()),
            resolve: self.halt.as_ref().map(Halt::routes),
            volume: self.volume.as_ref().map(Volume::to_string),
            rescan: self.rescan.clone(),
            reseed: self.reseed.clone(),
            rescans: self.rescans,
            skipped: self
                .skipped
                .iter()
                .map(|skip| crate::status::WorkspaceSkipStatus {
                    path: skip.path.clone(),
                    reason: skip.why.clone(),
                })
                .chain(
                    self.oversize
                        .iter()
                        .map(|refused| crate::status::WorkspaceSkipStatus {
                            path: refused.path.clone(),
                            reason: refused.to_string(),
                        }),
                )
                .take(REPORTED)
                .collect(),
            deferred: self.deferred.clone(),
            unsynced: self.unsynced.iter().take(REPORTED).cloned().collect(),
            trouble: self.trouble.clone(),
            passes: self.passes,
        }
    }
}

/// One machine's syncer, as everything outside the loop sees it.
///
/// The loop owns the sync; this is the seam the four `dev sync` verbs reach it
/// through, and it is deliberately narrow: read the last report, hand in a
/// resolution, ask for a pass. Nothing here can make the loop do anything it
/// would not do on its own — a resolution is an input to the next
/// reconciliation, not an act.
pub struct Syncer {
    pub workspace: Workspace,
    report: std::sync::Mutex<Report>,
    /// Resolutions handed in and not yet carried out. Taken by the pass and
    /// put back for any path whose apply failed, so `resolve` never has to be
    /// typed twice because a channel blinked.
    resolved: std::sync::Mutex<BTreeMap<String, Winner>>,
    /// A verb asking for a pass now.
    wake: Notify,
    /// The last pass a verb asked for, and the last one the loop finished —
    /// which is how `flush` waits for *its own* pass rather than for whichever
    /// one happened to be running.
    requested: AtomicU64,
    served: watch::Sender<u64>,
}

impl Syncer {
    fn new(workspace: Workspace) -> Syncer {
        Syncer {
            workspace,
            report: std::sync::Mutex::new(Report::default()),
            resolved: std::sync::Mutex::new(BTreeMap::new()),
            wake: Notify::new(),
            requested: AtomicU64::new(0),
            served: watch::channel(0).0,
        }
    }

    /// What the last completed pass decided.
    pub fn report(&self) -> Report {
        self.report.lock().expect("workspace report").clone()
    }

    fn publish(&self, report: Report) {
        *self.report.lock().expect("workspace report") = report;
    }

    /// Every path this machine's workspace is currently halted on — what
    /// `--all` expands to and what `dev sync diff` defaults to.
    ///
    /// Answered here rather than assembled by the caller because a caller's
    /// list is a snapshot of a projection it polled, and acting on a stale one
    /// is a developer answering a question that has since changed.
    pub fn halted_paths(&self) -> Vec<String> {
        self.report()
            .halt
            .map(|halt| halt.paths())
            .unwrap_or_default()
    }

    fn take_resolutions(&self) -> BTreeMap<String, Winner> {
        std::mem::take(&mut *self.resolved.lock().expect("workspace resolutions"))
    }

    fn restore_resolutions(&self, entries: impl IntoIterator<Item = (String, Winner)>) {
        let mut held = self.resolved.lock().expect("workspace resolutions");
        for (path, winner) in entries {
            held.entry(path).or_insert(winner);
        }
    }

    /// Record who wins at these paths and wait for the pass that carries it
    /// out. Waiting is the point: a resolution the developer cannot see the
    /// effect of is indistinguishable from one that was dropped.
    pub async fn resolve(&self, paths: Vec<String>, winner: Winner) -> Result<Report> {
        {
            let mut held = self.resolved.lock().expect("workspace resolutions");
            for path in paths {
                held.insert(path, winner);
            }
        }
        self.pass_now().await
    }

    /// Ask for a pass now and wait for it to finish.
    pub async fn pass_now(&self) -> Result<Report> {
        let want = self.requested.fetch_add(1, Ordering::SeqCst) + 1;
        let mut served = self.served.subscribe();
        self.wake.notify_one();
        let wait = async {
            while *served.borrow() < want {
                if served.changed().await.is_err() {
                    // The loop is gone: the machine stopped under us.
                    break;
                }
            }
        };
        tokio::time::timeout(VERB_TIMEOUT, wait)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "the workspace syncer for \"{}\" did not complete a pass within {}s — `vmlab \
                 status` says whether the machine is still answering",
                    self.workspace.machine,
                    VERB_TIMEOUT.as_secs(),
                )
            })?;
        Ok(self.report())
    }
}

/// Every workspace syncer running in one lab, one per machine.
///
/// A machine's syncer is independent: two dev machines may share one host
/// workspace, because the host is a hub rather than a peer — each has its own
/// ledger against the host and there is never a guest↔guest comparison. **One
/// halt per machine** falls out of that: A halting on its own divergence
/// leaves B's loop untouched, because there is nothing shared between them but
/// a directory.
#[derive(Default)]
pub struct WorkspaceSyncers {
    running: Mutex<HashMap<String, Running>>,
}

struct Running {
    syncer: Arc<Syncer>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    /// Everything needed to start this machine's loop again — held so a
    /// [`Bracket`] can put back exactly the syncer it took away.
    sessions: Arc<dyn GuestSessions>,
    events: Arc<EventLog>,
}

/// One machine's syncer, taken off the workspace across a snapshot restore
/// (§19.6).
///
/// **Suspending is the bracket's first half, and it has to be a real stop.**
/// Arming a flag would leave a pass that is already scanning free to finish
/// against a guest that gets rewound underneath it, and *that* pass is the one
/// that carries five hundred rolled-back files onto the canonical copy. So the
/// loop is stopped and waited for — which also closes the watch, whose channel
/// the restore was about to drop anyway.
pub struct Bracket {
    workspace: Workspace,
    sessions: Arc<dyn GuestSessions>,
    events: Arc<EventLog>,
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
        let syncer = Arc::new(Syncer::new(workspace));
        let task = tokio::spawn(run(
            syncer.clone(),
            sessions.clone(),
            events.clone(),
            halted,
        ));
        self.running.lock().await.insert(
            machine,
            Running {
                syncer,
                stop,
                task,
                sessions,
                events,
            },
        );
    }

    /// The pre-flight flush that brackets a **capture** (§19.6).
    ///
    /// Flushing before a capture is what makes the snapshot coherent with the
    /// host tree, so restoring it later lands somewhere meaningful rather than
    /// mid-transfer — and if the guest has work the canonical copy has never
    /// seen, this **refuses, with no escape flag**. A machine with no
    /// workspace has nothing to bracket and passes straight through.
    ///
    /// `declared` is the workspace of a machine that is **down**, where there
    /// is no syncer to flush. A stopped machine cannot be brought into step at
    /// all, so this refuses only on the halt its ledger still records: making
    /// every down dev machine unsnapshottable would be a bigger obstruction
    /// than the incoherence it guards against, and the halt is the part a
    /// developer can actually answer.
    pub async fn before_capture(&self, machine: &str, declared: Option<&Workspace>) -> Result<()> {
        let Some(syncer) = self.get(machine).await else {
            let ledger = recorded(declared).unwrap_or_else(Ledger::about_nothing);
            return refuse(super::bracket::Outstanding::when_stopped(
                machine,
                &ledger.halted,
                ledger.reseed_owed,
            ));
        };
        let report = match syncer.pass_now().await {
            Ok(report) => report,
            // The flush did not come back at all. That is the strongest reason
            // of the lot to refuse — not knowing what the guest holds is worse
            // than knowing it is behind — so it becomes the report's own
            // `trouble` rather than a second error path with its own words.
            Err(e) => Report {
                trouble: Some(format!("{e:#}")),
                ..syncer.report()
            },
        };
        refuse(super::bracket::Outstanding::of(machine, &report))
    }

    /// What a **restore** has to be asked twice about (§19.6).
    ///
    /// A halt is the one state where restoring destroys something a developer
    /// was about to be asked about, so it refuses — but only until the flag is
    /// given, because wanting to throw the guest copy away is frequently why
    /// someone restores.
    ///
    /// It reaches a machine that is **down** through the same `declared`
    /// workspace, because a restore does not need a running machine and the
    /// halt is exactly the state a developer must not lose by having stopped
    /// one.
    pub async fn before_restore(
        &self,
        machine: &str,
        discard: bool,
        declared: Option<&Workspace>,
    ) -> Result<()> {
        if discard {
            return Ok(());
        }
        match self.get(machine).await {
            Some(syncer) => refuse(super::bracket::halted(machine, &syncer.report())),
            // A restore is not refused for owing a re-seed: it is about to
            // ask for another one, and the second answers the first.
            None => refuse(super::bracket::halted_when_stopped(
                machine,
                &recorded(declared)
                    .unwrap_or_else(Ledger::about_nothing)
                    .halted,
            )),
        }
    }

    /// Take one machine's syncer off the workspace for the duration of a
    /// restore, or `None` for a machine that has none.
    ///
    /// Every caller must hand the [`Bracket`] back to [`resume`](Self::resume),
    /// including on the path where the restore itself failed: a workspace whose
    /// syncer quietly never came back is the silent-divergence failure ADR-0014
    /// exists to rule out, and it would look exactly like a machine with
    /// nothing to sync.
    pub async fn suspend(&self, machine: &str) -> Option<Bracket> {
        let running = self.running.lock().await.remove(machine)?;
        let _ = running.stop.send(true);
        // Waited for, not just signalled: the point of suspending is that no
        // pass is in flight when the guest is rewound.
        let _ = running.task.await;
        Some(Bracket {
            workspace: running.syncer.workspace.clone(),
            sessions: running.sessions,
            events: running.events,
        })
    }

    /// Note on one machine's ledger that its guest is about to be rewound
    /// (§19.6) — the other half of [`suspend`](Self::suspend), and the half
    /// that survives the machine being down.
    ///
    /// **Only with the syncer already off**, and that is checked rather than
    /// left to a comment. A running loop holds the ledger in memory and saves
    /// it *whole*, so a note written under one is erased by the next pass to
    /// complete — after which the resumed syncer stat-walks a rolled-back tree,
    /// reads every file in it as a guest-side edit, and carries them onto the
    /// canonical copy. That failure is silent, and it is precisely the one this
    /// bracket exists to prevent, so the ordering is enforced where it can be
    /// rather than described where it cannot.
    pub async fn mark_rewound(&self, workspace: &Workspace) -> Result<()> {
        if self.running.lock().await.contains_key(&workspace.machine) {
            anyhow::bail!(
                "\"{}\"'s workspace was marked as rewound while its syncer was still running: the \
                 note would be erased by the next pass to complete, and the restore would then \
                 propagate the rolled-back tree onto the canonical copy (§19.6). Suspend first.",
                workspace.machine,
            );
        }
        Ledger::mark_rewound(
            &workspace.ledger_path,
            &workspace.host_root,
            &workspace.guest_root,
        )
        .with_context(|| {
            format!(
                "noting on \"{}\"'s sync ledger that its workspace is about to be rewound",
                workspace.machine,
            )
        })
    }

    /// Put the syncer back.
    ///
    /// Whether it owes the bracket's re-seed is **not** decided here: it is
    /// written on the ledger before the rewind ([`Ledger::mark_rewound`]) and
    /// read by the loop at start-up, so a restore of a machine that is *down*
    /// — where there is no syncer to suspend and none to resume — owes exactly
    /// the same re-seed as this path does. One fact, in one place.
    pub async fn resume(&self, bracket: Bracket) {
        self.start(bracket.workspace, bracket.sessions, bracket.events)
            .await;
    }

    /// One machine's syncer, or `None` — which is every machine that is not a
    /// dev machine with a workspace, plus every one that is not up.
    pub async fn get(&self, machine: &str) -> Option<Arc<Syncer>> {
        self.running
            .lock()
            .await
            .get(machine)
            .map(|running| running.syncer.clone())
    }

    /// The same, as the error every `dev sync` verb gives for a machine that
    /// has no syncer to talk to.
    pub async fn expect(&self, machine: &str) -> Result<Arc<Syncer>> {
        self.get(machine).await.ok_or_else(|| {
            anyhow::anyhow!(
                "\"{machine}\" has no workspace syncer running: it is not up, or it is not a dev \
                 machine declaring `@dev(workspace = …)`"
            )
        })
    }

    /// What every running syncer last decided, keyed by machine — what the
    /// status projection folds in.
    pub async fn reports(&self) -> HashMap<String, Report> {
        self.running
            .lock()
            .await
            .iter()
            .map(|(machine, running)| (machine.clone(), running.syncer.report()))
            .collect()
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
    /// A `dev sync` verb asked for a pass.
    Asked,
    Quiet,
}

/// A refusal as its words, or nothing to refuse.
///
/// One shape for both brackets, so a refusal is always the `Display` of the
/// value that decided it — never a sentence assembled at the call site, which
/// is how two surfaces end up saying different things about one state.
fn refuse(said: Option<impl std::fmt::Display>) -> Result<()> {
    match said {
        Some(said) => Err(anyhow::anyhow!("{said}")),
        None => Ok(()),
    }
}

/// What a stopped machine's ledger still records about its workspace.
///
/// Read off disk rather than remembered, because the syncer that knew it went
/// with the machine — and read here rather than by the caller so the two
/// brackets cannot disagree about where the answer lives.
fn recorded(declared: Option<&Workspace>) -> Option<Ledger> {
    let workspace = declared?;
    Some(Ledger::load(
        &workspace.ledger_path,
        &workspace.host_root,
        &workspace.guest_root,
    ))
}

/// Why both directions are stopped while the bracket's re-seed runs.
///
/// Said as a **state** rather than left as a pause, like the rescan barrier
/// beside it: a surface asked "why is nothing moving" during a re-convergence
/// has to be able to answer without reading a log.
const RESEEDING: &str = "a snapshot restore rewound this machine, so the workspace is being carried back to the \
     canonical copy before anything else runs — nothing flows guest→host during it, and the \
     watch stays closed until it completes";

/// The loop. Runs until `stop` flips, whatever the machine or the channel
/// does in the meantime.
async fn run(
    syncer: Arc<Syncer>,
    sessions: Arc<dyn GuestSessions>,
    events: Arc<EventLog>,
    mut stop: watch::Receiver<bool>,
) {
    let workspace = syncer.workspace.clone();
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
    // Guest-side lock files seen and not yet observed gone. The watcher names
    // a path once, at the moment it appears, so a lock held across several
    // passes has to be re-asked about rather than re-reported.
    let mut lock_candidates: BTreeSet<String> = BTreeSet::new();
    // What the marker file at the guest's workspace root currently says, so it
    // is written when the halt changes and not once a pass.
    let mut marker: Option<String> = None;
    // Carried so the report can say how often coverage has been lost, which is
    // the overflow *symptom* — one lost event in a log is not.
    let mut rescans = 0u64;
    let mut passes = 0u64;
    // When to run a pass nothing else will ask for — see [`LOCK_RETRY`].
    let mut look_again: Option<Instant> = None;
    // The re-seed has just completed, so the watch about to open is the one
    // (re)open that is **not** a discontinuity — see below.
    let mut seeded = false;
    // The bracket's second half, read off the ledger rather than passed in:
    // a snapshot restore notes the rewind there precisely so that a machine
    // restored while it was *down* owes the same re-seed as one restored while
    // it was up (§19.6).
    let mut reseed = ledger.reseed_owed;

    if reseed {
        syncer.publish(Report {
            reseed: Some(RESEEDING.to_string()),
            ..Report::default()
        });
    }

    loop {
        if *stop.borrow() {
            return;
        }
        // **The re-seed completes before the watch reopens**, or the syncer's
        // own writes fill a fresh dirty set with tens of thousands of
        // self-inflicted paths. Nothing else runs first: no watch, no probe,
        // and above all no ordinary reconciliation, which would read the
        // rewound tree as five hundred guest-side edits.
        if reseed {
            let serving = syncer.requested.load(Ordering::SeqCst);
            match reconverge(
                &workspace,
                sessions.as_ref(),
                &events,
                &mut ledger,
                &mut learned,
            )
            .await
            {
                Ok(report) => {
                    passes += 1;
                    reseed = false;
                    // A restore takes the bracket's re-seed **rather than a
                    // stat-walk**: the walk asks what the guest did while
                    // nobody was watching, and vmlab already knows.
                    seeded = true;
                    rescan = false;
                    // The rules may have changed on the host while the machine
                    // was rolled back, and the re-seed has just recomputed
                    // them — so the watch opens on the current list.
                    prune = ledger.prune.clone();
                    syncer.publish(Report {
                        rescans,
                        passes,
                        ..report
                    });
                    let _ = syncer.served.send(serving);
                }
                Err(e) => {
                    // The barrier **stays**. A re-seed that failed leaves a
                    // guest holding rolled-back content and a ledger that
                    // still describes it, and an ordinary pass over that pair
                    // is exactly the propagation this bracket exists to
                    // prevent — so the loop retries rather than falling back
                    // to one.
                    syncer.publish(Report {
                        reseed: Some(RESEEDING.to_string()),
                        trouble: Some(format!("{e:#}")),
                        rescans,
                        passes,
                        ..syncer.report()
                    });
                    let _ = syncer.served.send(serving);
                    events.emit(
                        "workspace.deferred",
                        json!({
                            "machine": workspace.machine,
                            "reason": format!(
                                "the workspace could not re-converge after the snapshot restore, \
                                 so both directions stay stopped rather than propagating over a \
                                 rolled-back tree: {e:#}"
                            ),
                            "retry_in_s": RETRY.as_secs(),
                        }),
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(RETRY) => {}
                        _ = stop.changed() => return,
                    }
                    continue;
                }
            }
        }
        if guest_watch.is_none() {
            match sessions.watch(&workspace.guest_root, prune.clone()).await {
                Ok(watch) => {
                    guest_watch = Some(watch);
                    // Every (re)open is a discontinuity: what happened while
                    // there was no watch is exactly what the walk is for. The
                    // one exception is the open that follows a re-seed, where
                    // the answer is already in hand.
                    rescan = !std::mem::take(&mut seeded);
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
            // Read before the pass, so a verb that asks while one is running
            // waits for the *next* one — the pass under way was computed
            // without its resolution in hand.
            let serving = syncer.requested.load(Ordering::SeqCst);
            let drained = std::mem::take(&mut owed);
            let resolutions = syncer.take_resolutions();
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
                    lock_candidates: &lock_candidates,
                    resolved: &resolutions,
                    marker: marker.clone(),
                },
                &mut ledger,
                &mut learned,
            )
            .await
            {
                Ok(done) => {
                    passes += 1;
                    if rescan {
                        rescans += 1;
                    }
                    // The barrier lifts only on a completed walk.
                    rescan = false;
                    // Nothing the pass declined to decide is forgotten.
                    owed.extend(done.deferred);
                    lock_candidates = done.locks;
                    look_again =
                        (!done.report.deferred.is_empty()).then(|| Instant::now() + LOCK_RETRY);
                    marker = done.marker;
                    syncer.restore_resolutions(done.unresolved);
                    // Guest-side work the canonical copy has not seen: owed
                    // from this pass, plus whatever the guest is still writing.
                    // What a snapshot capture refuses on, so it is assembled
                    // where the whole of it is known rather than inside the
                    // pass, which sees only its own half.
                    //
                    // **Guest-side only.** A host-side save inside its own
                    // debounce window means the *guest* is momentarily behind,
                    // which a restore fixes and a capture does not lose — and
                    // counting it would let an editor open on the host refuse
                    // a capture that has no flag to answer it with.
                    let unsynced: BTreeSet<String> = owed
                        .iter()
                        .cloned()
                        .chain(guest_debounce.in_flight())
                        .collect();
                    syncer.publish(Report {
                        rescan: None,
                        reseed: None,
                        rescans,
                        passes,
                        unsynced: unsynced.into_iter().collect(),
                        ..done.report
                    });
                    let _ = syncer.served.send(serving);
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
                    // again, and so is every resolution it was carrying: a
                    // developer must not have to type `resolve` twice because
                    // a channel blinked.
                    owed = drained;
                    syncer.restore_resolutions(resolutions);
                    syncer.publish(Report {
                        trouble: Some(format!("{e:#}")),
                        rescans,
                        passes,
                        ..syncer.report()
                    });
                    // Answered even though it failed: a `dev sync flush`
                    // waiting on this pass wants the trouble, not a timeout.
                    let _ = syncer.served.send(serving);
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
        let wake = [
            host_debounce.next_wake(now),
            guest_debounce.next_wake(now),
            // A held lock clears itself, but only a pass can notice: the loop
            // re-arms so the mutable set is not stalled until whenever the
            // developer next types.
            look_again.map(|at| at.saturating_duration_since(now)),
        ]
        .into_iter()
        .flatten()
        .min();
        let woke = tokio::select! {
            _ = stop.changed() => Woke::Stopped,
            _ = syncer.wake.notified() => Woke::Asked,
            event = host_watch.events.recv() => Woke::Host(event),
            report = next_report(&mut guest_watch) => Woke::Guest(report),
            _ = sleep_for(wake) => Woke::Quiet,
        };
        match woke {
            Woke::Stopped => return,
            // A verb wants a pass now: `flush` drains what is pending, and
            // `resolve` has just handed in the input that clears a halt.
            Woke::Asked => due = true,
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
                            drop_watch(
                                &mut guest_watch,
                                &mut rescan,
                                &events,
                                &workspace,
                                &syncer,
                                e,
                            );
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
                        let why = "the guest's watch lost coverage, so the guest tree is walked \
                                   again; both directions wait for the walk rather than \
                                   propagating over changes the host cannot see yet";
                        events.emit(
                            "workspace.rescan",
                            json!({"machine": workspace.machine, "reason": why}),
                        );
                        // Said as a *state*, not just as an event: a surface
                        // asked "why is nothing moving" during the barrier has
                        // to be able to answer without reading a log.
                        syncer.publish(Report {
                            rescan: Some(why.to_string()),
                            ..syncer.report()
                        });
                        rescan = true;
                        due = true;
                    }
                    WatchReport::Error(msg) => {
                        drop_watch(
                            &mut guest_watch,
                            &mut rescan,
                            &events,
                            &workspace,
                            &syncer,
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
                if look_again.is_some_and(|at| at <= now) {
                    look_again = None;
                    due = true;
                }
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
    syncer: &Syncer,
    why: anyhow::Error,
) {
    *guest_watch = None;
    *rescan = true;
    let said = format!(
        "the guest's watch channel failed ({why:#}), so the guest tree is walked again once it \
         reopens"
    );
    events.emit(
        "workspace.rescan",
        json!({"machine": workspace.machine, "reason": said}),
    );
    syncer.publish(Report {
        rescan: Some(said),
        ..syncer.report()
    });
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
    /// Guest-side git locks seen on an earlier pass and not yet observed gone.
    lock_candidates: &'a BTreeSet<String>,
    /// Who wins where, as the developer has already said.
    resolved: &'a BTreeMap<String, Winner>,
    /// What the guest's halt marker currently says, so an unchanged halt does
    /// not rewrite it once a pass.
    marker: Option<String>,
}

/// What a completed pass tells the loop.
struct Passed {
    /// The prune list as the rules now stand. A change reopens the watch.
    prune: Vec<String>,
    /// Drained paths this pass declined to decide — still moving, a named
    /// skip, or anything at all while the workspace is halted. Handed back so
    /// they are **de-prioritised rather than dropped**: a path nothing touches
    /// again would otherwise wait for a discontinuity to be noticed at all.
    deferred: BTreeSet<String>,
    /// Guest-side locks still held, to re-ask about next pass.
    locks: BTreeSet<String>,
    /// Resolutions to hand back: their apply failed, so the developer's answer
    /// has not been carried out yet.
    unresolved: BTreeMap<String, Winner>,
    /// What the marker now says, `None` where there is none.
    marker: Option<String>,
    report: Report,
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
    let skipped: Vec<Skip> = scan
        .skipped
        .iter()
        .chain(probe.skipped.iter())
        .cloned()
        .collect();
    for skip in &skipped {
        events.emit(
            "workspace.skipped",
            json!({"machine": workspace.machine, "path": skip.path, "reason": skip.why}),
        );
        undecided.insert(skip.path.clone());
    }

    // `.git`'s mutable set, while either side holds a lock on it. **Timing,
    // not a conflict**: the paths are left exactly as they are on both sides
    // and the next pass reconsiders them, so nothing is reported as needing a
    // developer and nothing can be resolved.
    let mut held = scan.locks.clone();
    held.extend(probe.locks.iter().cloned());
    if !pending.rescan {
        // The steady state never walks, so a lock is only known from the
        // watcher having named it once — which means re-asking about the ones
        // already seen rather than waiting to be told again. A lock still
        // inside its debounce window counts too: the debounce exists to stop
        // the syncer *reading* a file mid-write, and a lock is never read, only
        // noticed.
        let candidates: BTreeSet<String> = pending
            .lock_candidates
            .iter()
            .chain(
                pending
                    .guest_dirty
                    .iter()
                    .chain(pending.in_flight.iter())
                    .filter(|path| locks::is_lock(path)),
            )
            .cloned()
            .collect();
        held.extend(guest_locks(guest.as_ref(), &workspace.guest_root, &candidates).await);
    }
    let deferred = locks::deferred(
        &held,
        scan.tree
            .keys()
            .chain(probe.tree.keys())
            .chain(ledger.entries.keys()),
    );
    if !deferred.is_empty() {
        events.emit(
            "workspace.deferred",
            json!({
                "machine": workspace.machine,
                "reason": locks::why(&held, deferred.len()),
                "paths": deferred.iter().take(REPORTED).collect::<Vec<_>>(),
            }),
        );
        undecided.extend(deferred.iter().cloned());
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
        resolved: pending.resolved,
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
    // A burst warns and carries on. The distinction that decides it: the size
    // guard refuses because a 4 GB `.vhdx` is unwanted work, where a build
    // burst is wanted work that happens to be large — and halting here would
    // let a `cargo build` into an un-ignored `target/` stop the dev machine.
    if let Some(volume) = &plan.volume {
        events.emit(
            "workspace.volume",
            json!({
                "machine": workspace.machine,
                "path": volume.prefix,
                "paths": volume.paths,
                "bytes": volume.bytes,
                "reason": volume.to_string(),
            }),
        );
    }

    // **Scan then halt**, naming every conflicting path in the batch: a
    // host-side `git pull` collides in batches, and halting on the first would
    // turn one pull into thirty resolve-and-resume round trips. The rules'
    // digest in the ledger is what lets this say *these conflict because you
    // just changed the rules* — the un-ignored `.env` case, where the two
    // sides differing is the normal situation.
    let rules = ignores.digest();
    let rules_changed = !ledger.ignore_digest.is_empty() && ledger.ignore_digest != rules;
    let halt = Halt::of(&workspace.machine, &plan, rules_changed);
    if let Some(halt) = &halt {
        events.emit(
            "workspace.halted",
            json!({
                "machine": workspace.machine,
                "reason": halt.headline(),
                "rules_changed": halt.rules_changed,
                "paths": halt.reasons().into_iter().take(REPORTED).map(|(path, why)| json!({
                    "path": path,
                    "reason": why,
                })).collect::<Vec<_>>(),
                "total": halt.paths().len(),
                "resolve": halt.routes(),
            }),
        );
    }

    // **The whole workspace stops, both directions.** What still runs is only
    // what moves nothing the developer did not ask for: the ledger-only work,
    // which is what makes *make the two sides identical by hand* a resolution
    // route needing no verb, and the actions at paths a resolution names.
    //
    // Nothing is interrupted to get here. A pass scans, reconciles and only
    // then applies, so the halt is decided before this pass has transferred
    // anything and the previous pass's transfers completed long ago — which is
    // §19.6's *finish the file in flight* as a property of the shape rather
    // than as a thing to remember.
    let carried = match halt {
        Some(_) => plan.while_halted(pending.resolved),
        None => plan.clone(),
    };
    let applied = apply(
        guest.as_ref(),
        &Target {
            host_root: workspace.host_root.clone(),
            guest_root: workspace.guest_root.clone(),
            case_sensitive_dirs: learned.case_sensitive_dirs,
        },
        &carried,
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
    //
    // The **rules' digest** is the one field a halted pass leaves alone. The
    // ledger records what the two sides have agreed *under* a set of rules, and
    // a halt is precisely the state of not having agreed under the new ones —
    // recording them anyway would cost the halt its own explanation on the very
    // next pass, which is the sentence a developer who just edited
    // `.vmlabignore` needs most.
    let prune = ignores.prune_list(&scan.pruned);
    let record_rules = if halt.is_some() {
        ledger.ignore_digest.clone()
    } else {
        rules
    };
    // The halted paths ride the ledger for the snapshot bracket's sake
    // (§19.6): a restore refuses while a halt stands, and `vmlab down` takes
    // the syncer holding it — so without this, stopping a machine would be a
    // way to lose the refusal along with it.
    let halted_now = halt.as_ref().map(Halt::paths).unwrap_or_default();
    if !plan.nothing_to_record()
        || ledger.ignore_digest != record_rules
        || ledger.prune != prune
        || ledger.halted != halted_now
    {
        ledger.ignore_digest = record_rules;
        ledger.prune = prune.clone();
        ledger.halted = halted_now;
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

    // The guest's only view of a halt (§19.6). Written after the applies so it
    // describes a state that has settled, and only when it changes — a file
    // rewritten every pass would churn the editor and the guest's own
    // `git status` for nothing.
    let wanted = halt.as_ref().map(halt::marker);
    if wanted != pending.marker {
        match place_marker(guest.as_ref(), &workspace.guest_root, wanted.as_deref()).await {
            Ok(()) => {}
            // Never a reason to stop: the halt itself is already reported
            // host-side, and losing the guest's copy of the news is worse
            // handled by pretending the halt did not happen.
            Err(e) => events.emit(
                "workspace.failed",
                json!({
                    "machine": workspace.machine,
                    "path": join_guest(&workspace.guest_root, halt::MARKER),
                    "reason": format!(
                        "the halt marker could not be written into the guest, so from inside the \
                         machine this halt is invisible: {e:#}"
                    ),
                }),
            ),
        }
    }

    // A resolution whose apply failed has not been carried out, so the
    // developer's answer is kept rather than spent — the alternative is
    // typing `resolve` twice because a channel blinked.
    let unresolved: BTreeMap<String, Winner> = applied
        .failures
        .iter()
        .filter_map(|failure| {
            pending
                .resolved
                .get(&failure.path)
                .map(|winner| (failure.path.clone(), *winner))
        })
        .collect();

    Ok(Passed {
        prune,
        // **While halted, everything drained stays owed.** Nothing was agreed
        // about any of it, so a path dropped here would be a guest-side edit
        // waiting for a discontinuity to be noticed at all — and the host
        // keeping the pending set is exactly what stops a long halt costing a
        // rescan.
        deferred: match halt {
            Some(_) => pending.guest_dirty.clone(),
            None => pending
                .guest_dirty
                .intersection(&undecided)
                .cloned()
                .collect(),
        },
        locks: held,
        unresolved,
        marker: wanted,
        report: Report {
            halt,
            volume: plan.volume.clone(),
            skipped: skipped.into_iter().take(REPORTED).collect(),
            oversize: plan.oversize.clone(),
            deferred: deferred.into_iter().take(REPORTED).collect(),
            ..Report::default()
        },
    })
}

/// The bracket's re-seed as one pass: the whole workspace carried back to the
/// canonical copy after a snapshot restore (§19.6, [`reseed`](super::reseed)).
///
/// It stands in for a reconciliation rather than beside one, and the two
/// differences are the point. It asks the guest nothing it would act on — the
/// tree is walked only to decide what to overwrite and delete — and it takes no
/// stat-walk in the ordinary sense, because there is no question about what
/// happened to that tree: vmlab did it.
///
/// The halt marker is not written or read here. A restore that reached this
/// point either had no halt or was asked for it explicitly, and the halt it
/// discarded is gone with the guest state it was about — so the marker goes
/// with the rest of the rolled-back tree, as an ordinary removal or an
/// overwrite, and the loop's own `marker` starts empty on the restarted loop.
async fn reconverge(
    workspace: &Workspace,
    sessions: &dyn GuestSessions,
    events: &EventLog,
    ledger: &mut Ledger,
    learned: &mut Learned,
) -> Result<Report> {
    let guest = sessions
        .open()
        .await
        .context("opening a file session as the machine's default login")?;
    // Before the re-seed for the same reason it comes before an ordinary pass:
    // whether this guest will hold two names differing only in case decides
    // whether a colliding pair is transferred or refused, and finding out from
    // a failed `mkdir` half way through means one of them has already landed
    // on the other.
    preconditions(workspace, sessions, guest.as_ref(), events, learned).await?;

    let done = super::reseed::reconverge(
        guest.as_ref(),
        workspace,
        learned.case_sensitive_dirs,
        learned.case_folding(),
        ledger,
    )
    .await?;
    ledger
        .save(&workspace.ledger_path)
        .with_context(|| format!("saving {}", workspace.ledger_path.display()))?;

    events.emit(
        "workspace.reconverged",
        json!({
            "machine": workspace.machine,
            "placed": done.placed,
            "removed": done.removed,
            "adopted": done.adopted,
            "reason": done.headline(&workspace.machine),
        }),
    );
    for refusal in &done.oversize {
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
    for collision in &done.collisions {
        events.emit(
            "workspace.case_collision",
            json!({
                "machine": workspace.machine,
                "paths": collision.paths,
                "reason": collision.to_string(),
            }),
        );
    }
    for skip in &done.skipped {
        events.emit(
            "workspace.skipped",
            json!({"machine": workspace.machine, "path": skip.path, "reason": skip.why}),
        );
    }
    // Named, and left for the next ordinary pass. Nothing was agreed about
    // them, so they are carried the usual way rather than blocking the
    // barrier for ever — the guarantee the bracket owes is that no *guest*
    // state reached the host, and a path that did not land breaks none of it.
    for failure in &done.failures {
        events.emit(
            "workspace.failed",
            json!({"machine": workspace.machine, "path": failure.path, "reason": failure.why}),
        );
    }

    Ok(Report {
        // A restore discards the guest side of the workspace, so whatever the
        // two sides were disagreeing about before it is not a disagreement any
        // more. There is nothing left for a developer to resolve.
        halt: None,
        skipped: done.skipped.into_iter().take(REPORTED).collect(),
        oversize: done.oversize,
        ..Report::default()
    })
}

/// Put the halt marker at the guest's workspace root, or take it away.
///
/// Temp-then-rename in the target's own directory, like every other apply and
/// for the same reason: an editor watching the tree must never see a
/// half-written one. Both names are in the ignore floor, so neither the temp
/// nor the marker itself can become a sync object.
async fn place_marker(guest: &dyn GuestFs, guest_root: &str, body: Option<&str>) -> Result<()> {
    let marker = join_guest(guest_root, halt::MARKER);
    let Some(body) = body else {
        return guest.remove(&marker).await;
    };
    let scratch = tempfile::NamedTempFile::new().context("a host scratch file for the marker")?;
    std::fs::write(scratch.path(), body).context("writing the marker")?;
    let temp = join_guest(guest_root, &format!("{TEMP_PREFIX}halt"));
    guest.push(scratch.path(), &temp).await?;
    guest.rename(&temp, &marker).await
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
        /// How many guest writes had already happened when each watch opened.
        ///
        /// How a test says *the re-seed completed before the watch reopened*
        /// as a fact rather than as a hope: the ordering is the whole of
        /// §19.6's rule, and without this a test could only observe that both
        /// things eventually happened.
        opened_after: std::sync::Mutex<Vec<usize>>,
    }

    impl OneFake {
        fn new(guest: Arc<FakeGuest>) -> Arc<OneFake> {
            let watcher = FakeWatcher::new(guest.clone(), "/src");
            Arc::new(OneFake {
                guest,
                watcher,
                opens: std::sync::Mutex::new(Vec::new()),
                opened_after: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn opens(&self) -> Vec<Vec<String>> {
            self.opens.lock().expect("opens").clone()
        }

        fn opened_after(&self) -> Vec<usize> {
            self.opened_after.lock().expect("opened_after").clone()
        }
    }

    #[async_trait]
    impl GuestSessions for OneFake {
        async fn open(&self) -> Result<Box<dyn GuestFs>> {
            Ok(Box::new(self.guest.clone()))
        }

        async fn watch(&self, _root: &str, prune: Vec<String>) -> Result<Box<dyn GuestWatch>> {
            self.opens.lock().expect("opens").push(prune);
            self.opened_after
                .lock()
                .expect("opened_after")
                .push(self.guest.writes().len());
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

    /// The same, where the question has to be *asked* — a syncer's report
    /// lives behind an async lookup, so `eventually`'s sync closure cannot
    /// reach it.
    async fn eventually_async<F, Fut>(mut check: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..200 {
            if check().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        check().await
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

    /// Start a syncer over the host tree already laid out in `dir`, and wait
    /// until `landed` — a guest path — proves the seed went through.
    ///
    /// Every halt test starts from an *agreed* workspace, because a halt is
    /// about two sides diverging from something they had settled: seeding is
    /// the precondition rather than the subject.
    async fn seeded_lab(
        dir: &std::path::Path,
        state: &std::path::Path,
        landed: &str,
    ) -> (WorkspaceSyncers, Arc<FakeGuest>, Arc<OneFake>) {
        let guest = Arc::new(FakeGuest::new());
        let (events, _rx) = EventLog::recording("lab", state.join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let sessions = OneFake::new(guest.clone());
        syncers
            .start(workspace(dir, state), sessions.clone(), events)
            .await;
        let seeded = {
            let (guest, landed) = (guest.clone(), landed.to_string());
            eventually(move || guest.get(&landed).is_some()).await
        };
        assert!(seeded, "the seed never landed: {:?}", guest.paths());
        (syncers, guest, sessions)
    }

    /// The one-file version, which is every test whose subject is one path
    /// both sides moved.
    async fn halted_lab(
        dir: &std::path::Path,
        state: &std::path::Path,
    ) -> (WorkspaceSyncers, Arc<FakeGuest>, Arc<OneFake>) {
        std::fs::write(dir.join("main.rs"), "agreed").unwrap();
        seeded_lab(dir, state, "/src/main.rs").await
    }

    /// Both sides edit the same file to different content, which is the
    /// anomaly the whole policy is built around.
    fn diverge(dir: &std::path::Path, guest: &FakeGuest, sessions: &OneFake) {
        std::fs::write(dir.join("main.rs"), "the host's version").unwrap();
        guest.file("/src/main.rs", "the guest's version", 500);
        sessions.watcher.mark("main.rs");
    }

    /// Wait for one machine's syncer to report a halt.
    async fn halt_of(syncers: &WorkspaceSyncers, machine: &str) -> Option<Halt> {
        eventually_async(|| async {
            syncers
                .get(machine)
                .await
                .is_some_and(|s| s.report().halt.is_some())
        })
        .await;
        syncers.get(machine).await.and_then(|s| s.report().halt)
    }

    /// **Halt and surface.** Both copies survive untouched, both directions
    /// stop, the paths are named, and no third file is invented anywhere.
    #[tokio::test]
    async fn a_conflict_halts_the_workspace_and_writes_over_neither_copy() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (syncers, guest, sessions) = halted_lab(dir.path(), state.path()).await;

        // A second, uncontested file, so the halt can be seen to stop the
        // whole workspace rather than one path.
        std::fs::write(dir.path().join("other.rs"), "host only").unwrap();
        diverge(dir.path(), &guest, &sessions);

        let halt = halt_of(&syncers, "dev01").await.expect("nothing halted");
        assert_eq!(halt.machine, "dev01");
        assert_eq!(halt.paths(), vec!["main.rs".to_string()]);

        // Neither copy was written and neither was deleted.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("main.rs")).unwrap(),
            "the host's version"
        );
        assert_eq!(
            guest.text("/src/main.rs").as_deref(),
            Some("the guest's version")
        );
        // …and the other direction is stopped too: an uncontested host file
        // does not cross while the workspace is halted.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            guest.get("/src/other.rs").is_none(),
            "the halt was per path, not per workspace: {:?}",
            guest.paths()
        );
        // No conflict copies: the two copies already exist, one per side.
        assert!(
            !guest.paths().iter().any(|p| p.contains("conflict")),
            "{:?}",
            guest.paths()
        );
        assert!(
            !dir.path().join("main.rs.conflict-guest").exists(),
            "a conflict copy was written host-side"
        );
        syncers.stop("dev01").await;
    }

    /// **The guest-side signal.** From inside the guest a halt is otherwise
    /// nothing happening, and ADR-0013 leaves no control path to tell it — so
    /// a marker lands at the workspace root, lists the halted paths, and goes
    /// when the halt does.
    #[tokio::test]
    async fn the_guest_gets_a_marker_naming_the_halted_paths_and_loses_it_on_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (syncers, guest, sessions) = halted_lab(dir.path(), state.path()).await;
        diverge(dir.path(), &guest, &sessions);
        halt_of(&syncers, "dev01").await.expect("nothing halted");

        let marker = format!("/src/{}", halt::MARKER);
        let written = {
            let guest = guest.clone();
            let marker = marker.clone();
            eventually(move || guest.text(&marker).is_some()).await
        };
        assert!(written, "no marker: {:?}", guest.paths());
        let said = guest.text(&marker).unwrap();
        assert!(said.contains("main.rs"), "{said}");
        assert!(said.contains("dev01"), "{said}");
        assert!(said.contains("dev sync resolve"), "{said}");

        // The developer picks a side, host-side, because there is no other
        // side to pick it from.
        let syncer = syncers.get("dev01").await.expect("no syncer");
        let report = syncer
            .resolve(vec!["main.rs".into()], Winner::Guest)
            .await
            .expect("the resolution never completed");
        assert!(report.halt.is_none(), "{:?}", report.halt);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("main.rs")).unwrap(),
            "the guest's version",
            "the winner never reached the canonical copy"
        );
        assert!(
            guest.text(&marker).is_none(),
            "the marker outlived the halt"
        );
        syncers.stop("dev01").await;
    }

    /// The other route: the host copy wins, and the guest's working copy is
    /// overwritten with it.
    #[tokio::test]
    async fn resolving_toward_the_host_carries_the_canonical_copy_into_the_guest() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (syncers, guest, sessions) = halted_lab(dir.path(), state.path()).await;
        diverge(dir.path(), &guest, &sessions);
        halt_of(&syncers, "dev01").await.expect("nothing halted");

        let syncer = syncers.get("dev01").await.expect("no syncer");
        let report = syncer
            .resolve(vec!["main.rs".into()], Winner::Host)
            .await
            .expect("the resolution never completed");
        assert!(report.halt.is_none(), "{:?}", report.halt);
        assert_eq!(
            guest.text("/src/main.rs").as_deref(),
            Some("the host's version")
        );
        syncers.stop("dev01").await;
    }

    /// **A free third route needing no verb**: make the two sides identical by
    /// hand and the next pass adopts them as agreed. Which is also why
    /// ledger-only work survives a halt at all.
    #[tokio::test]
    async fn making_both_sides_identical_by_hand_clears_the_halt() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (syncers, guest, sessions) = halted_lab(dir.path(), state.path()).await;
        diverge(dir.path(), &guest, &sessions);
        halt_of(&syncers, "dev01").await.expect("nothing halted");

        std::fs::write(dir.path().join("main.rs"), "settled by hand").unwrap();
        guest.file("/src/main.rs", "settled by hand", 900);
        sessions.watcher.mark("main.rs");

        let cleared = eventually_async(|| async {
            syncers
                .get("dev01")
                .await
                .is_some_and(|s| s.report().halt.is_none())
        })
        .await;
        assert!(cleared, "the halt outlived the disagreement");
        assert!(
            guest.text(&format!("/src/{}", halt::MARKER)).is_none(),
            "the marker outlived the halt"
        );
        syncers.stop("dev01").await;
    }

    /// **The guards on deletion are asymmetric on purpose.** The guest is
    /// reconstructible and the host is not, so mass arriving from the guest
    /// stops the workspace rather than being replicated onto the one copy
    /// nothing re-derives.
    #[tokio::test]
    async fn a_guest_side_mass_deletion_halts_before_it_reaches_the_canonical_copy() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        for i in 0..40 {
            std::fs::write(dir.path().join(format!("f{i:02}.rs")), "content").unwrap();
        }
        let (syncers, guest, sessions) = seeded_lab(dir.path(), state.path(), "/src/f39.rs").await;

        // `rm -rf *` in the guest.
        for i in 0..40 {
            guest.unlink(&format!("/src/f{i:02}.rs"));
            sessions.watcher.mark(&format!("f{i:02}.rs"));
        }

        let halt = halt_of(&syncers, "dev01")
            .await
            .expect("the mass deletion was not caught");
        assert!(halt.bulk_delete.is_some(), "{halt:?}");
        assert_eq!(halt.paths().len(), 40);
        assert!(
            dir.path().join("f00.rs").exists() && dir.path().join("f39.rs").exists(),
            "the canonical copy was deleted anyway"
        );

        // …and `--all --guest` is the way out, because wanting the deletion is
        // a perfectly ordinary thing to want.
        let syncer = syncers.get("dev01").await.expect("no syncer");
        syncer
            .resolve(halt.paths(), Winner::Guest)
            .await
            .expect("the resolution never completed");
        assert!(
            !dir.path().join("f00.rs").exists(),
            "the deletion never went"
        );
        syncers.stop("dev01").await;
    }

    /// **Never sync `*.lock`, and defer the mutable set while one is held** —
    /// which is *timing*, not a conflict: nothing is reported as needing a
    /// developer, and it clears itself.
    #[tokio::test]
    async fn a_held_git_lock_defers_the_mutable_set_without_halting() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        std::fs::write(dir.path().join(".git/index"), "agreed").unwrap();
        std::fs::write(dir.path().join("main.rs"), "code").unwrap();

        let (syncers, guest, sessions) =
            seeded_lab(dir.path(), state.path(), "/src/.git/index").await;

        // Guest-side git takes the lock and starts rewriting.
        guest.file("/src/.git/index.lock", "pid", 600);
        sessions.watcher.mark(".git/index.lock");
        std::fs::write(
            dir.path().join(".git/index"),
            "a host-side fetch wrote this",
        )
        .unwrap();

        let syncer = syncers.get("dev01").await.expect("no syncer");
        let deferred = {
            let syncer = syncer.clone();
            eventually(move || syncer.report().deferred.iter().any(|p| p == ".git/index")).await
        };
        assert!(
            deferred,
            "the mutable set was not deferred: {:?}",
            syncer.report()
        );
        assert!(
            syncer.report().halt.is_none(),
            "a deferral is not a halt: {:?}",
            syncer.report()
        );
        assert_eq!(
            guest.text("/src/.git/index").as_deref(),
            Some("agreed"),
            "the mutable set crossed mid-rewrite"
        );
        // The lock itself never syncs, whatever else happens.
        assert!(!dir.path().join(".git/index.lock").exists());

        // git lets go, and the deferral clears itself with no developer in it.
        guest.unlink("/src/.git/index.lock");
        let flowed = {
            let guest = guest.clone();
            eventually(move || {
                guest.text("/src/.git/index").as_deref() == Some("a host-side fetch wrote this")
            })
            .await
        };
        assert!(flowed, "the deferral never cleared");
        syncers.stop("dev01").await;
    }

    /// A resolution is carried out **while the workspace is still halted on
    /// other paths**, or per-path `--host`/`--guest` would be a flag that does
    /// nothing until the last one — and it lands even where the directory it
    /// belongs in went with the rest of the guest's deletion, because both
    /// sides' applies make the parents they need.
    #[tokio::test]
    async fn one_resolution_lands_while_the_rest_of_the_batch_is_still_halted() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("pkg")).unwrap();
        std::fs::write(dir.path().join("pkg/a.rs"), "agreed").unwrap();
        std::fs::write(dir.path().join("other.rs"), "agreed").unwrap();

        let (syncers, guest, sessions) =
            seeded_lab(dir.path(), state.path(), "/src/pkg/a.rs").await;

        // The guest drops the whole directory while the host edits what was
        // in it; and a second path diverges, so the halt outlives the first
        // resolution.
        std::fs::write(dir.path().join("pkg/a.rs"), "the host's version").unwrap();
        std::fs::write(dir.path().join("other.rs"), "host").unwrap();
        guest.unlink("/src/pkg/a.rs");
        guest.unlink("/src/pkg");
        guest.file("/src/other.rs", "guest", 500);
        for path in ["pkg", "pkg/a.rs", "other.rs"] {
            sessions.watcher.mark(path);
        }
        let halt = halt_of(&syncers, "dev01").await.expect("nothing halted");
        assert!(halt.paths().contains(&"pkg/a.rs".to_string()), "{halt:?}");

        let syncer = syncers.get("dev01").await.expect("no syncer");
        let report = syncer
            .resolve(vec!["pkg/a.rs".into()], Winner::Host)
            .await
            .expect("the resolution never completed");
        assert_eq!(
            guest.text("/src/pkg/a.rs").as_deref(),
            Some("the host's version"),
            "the resolution never landed: {:?}",
            guest.paths()
        );
        // …and the workspace is still stopped on the path nobody answered for.
        let halt = report.halt.expect("the rest of the batch resumed unasked");
        assert_eq!(halt.paths(), vec!["other.rs".to_string()]);
        syncers.stop("dev01").await;
    }

    /// **Entering scope is a conflict**, and the halt says the rules changed.
    ///
    /// The files most likely to be un-ignored are `.env`, local certs and
    /// `appsettings.Development.json`, where the two sides differing is the
    /// *normal* situation — so picking a winner silently would overwrite a
    /// working local config with a stale one, and the halt has to explain
    /// itself or it reads as vmlab breaking for no reason.
    #[tokio::test]
    async fn un_ignoring_a_populated_path_halts_and_says_the_rules_changed() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), ".env\n").unwrap();
        std::fs::write(dir.path().join(".env"), "HOST=canonical").unwrap();
        std::fs::write(dir.path().join("app.rs"), "code").unwrap();

        let (syncers, guest, _sessions) = seeded_lab(dir.path(), state.path(), "/src/app.rs").await;
        // Guest-owned, so the guest has been holding its own all along.
        assert!(guest.get("/src/.env").is_none());
        guest.file("/src/.env", "GUEST=the one that works here", 400);

        // The developer wants it guest-side after all.
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();

        let halt = halt_of(&syncers, "dev01")
            .await
            .expect("entering scope did not halt");
        assert_eq!(halt.paths(), vec![".env".to_string()]);
        assert!(halt.rules_changed, "{halt:?}");
        assert!(
            halt.headline().contains("ignore rules changed"),
            "{}",
            halt.headline()
        );

        // Neither local config was overwritten with the other.
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".env")).unwrap(),
            "HOST=canonical"
        );
        assert_eq!(
            guest.text("/src/.env").as_deref(),
            Some("GUEST=the one that works here")
        );

        // …and the halt keeps saying so pass after pass, because the ledger
        // does not record rules the two sides have not agreed under.
        let syncer = syncers.get("dev01").await.expect("no syncer");
        let again = syncer.pass_now().await.expect("no pass");
        assert!(
            again.halt.is_some_and(|halt| halt.rules_changed),
            "the explanation was lost on the next pass"
        );
        syncers.stop("dev01").await;
    }

    /// **One halt per machine.** Two dev machines may share one host
    /// workspace, because the host is a hub rather than a peer — so A halting
    /// on its own divergence must leave B syncing.
    #[tokio::test]
    async fn two_machines_on_one_workspace_halt_independently() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "agreed").unwrap();

        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        let syncers = WorkspaceSyncers::default();
        let a_guest = Arc::new(FakeGuest::new());
        let a = OneFake::new(a_guest.clone());
        let b_guest = Arc::new(FakeGuest::new());
        let b = OneFake::new(b_guest.clone());
        syncers
            .start(
                workspace(dir.path(), state.path()),
                a.clone(),
                events.clone(),
            )
            .await;
        syncers
            .start(
                Workspace {
                    machine: "dev02".into(),
                    ledger_path: Ledger::path(state.path(), "dev02"),
                    ..workspace(dir.path(), state.path())
                },
                b.clone(),
                events,
            )
            .await;
        let seeded = {
            let (a_guest, b_guest) = (a_guest.clone(), b_guest.clone());
            eventually(move || {
                a_guest.text("/src/main.rs").is_some() && b_guest.text("/src/main.rs").is_some()
            })
            .await
        };
        assert!(seeded, "one of the two never seeded");

        // A diverges from the host; B is untouched.
        std::fs::write(dir.path().join("main.rs"), "the host's version").unwrap();
        a_guest.file("/src/main.rs", "dev01's version", 500);
        a.watcher.mark("main.rs");

        let halted = halt_of(&syncers, "dev01")
            .await
            .expect("dev01 never halted");
        assert_eq!(
            halted.machine, "dev01",
            "the halt does not name the machine"
        );
        assert!(
            syncers
                .get("dev02")
                .await
                .expect("no dev02 syncer")
                .report()
                .halt
                .is_none(),
            "dev01's divergence stopped dev02"
        );
        // …and B kept following the host through it.
        let followed = {
            let b_guest = b_guest.clone();
            eventually(move || {
                b_guest.text("/src/main.rs").as_deref() == Some("the host's version")
            })
            .await
        };
        assert!(followed, "dev02 stopped syncing because dev01 halted");
        syncers.stop("dev01").await;
        syncers.stop("dev02").await;
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

    // ---- the snapshot bracket (§19.6) -------------------------------------

    /// Roll the guest back the way a snapshot restore does: the tree it was
    /// holding at capture time, complete with an older clock and work the host
    /// has never seen.
    fn rewind(guest: &FakeGuest) {
        guest.file("/src/main.rs", "the version in the snapshot", 1);
        guest.file("/src/experiment.rs", "never synced anywhere", 1);
    }

    /// The sequence `Lab::restore` performs around a rewind (§19.6): take the
    /// syncer off the workspace and wait for it, *then* note the rewind on the
    /// ledger, rewind, and put the syncer back.
    ///
    /// Spelled out here rather than hidden behind a helper on the syncers,
    /// because the ordering **is** the safety argument and a test that took a
    /// convenient order would prove nothing about the real one. The suspend
    /// comes first for two reasons at once: no pass may be in flight while the
    /// guest is rewound, and a pass that finished after the note was written
    /// would save its own in-memory ledger over it — leaving a resumed syncer
    /// to stat-walk a rolled-back tree with no idea anything had happened.
    async fn restore_bracket<T>(
        syncers: &WorkspaceSyncers,
        ws: &Workspace,
        rewind_guest: impl FnOnce() -> T,
    ) -> T {
        let bracket = syncers
            .suspend(&ws.machine)
            .await
            .expect("a workspace to hold");
        assert!(!syncers.is_running(&ws.machine).await);
        syncers.mark_rewound(ws).await.unwrap();
        let observed = rewind_guest();
        syncers.resume(bracket).await;
        observed
    }

    /// The whole hazard in one test. A restore rewinds the guest by files the
    /// syncer cannot tell from edits; the bracket makes the tree re-converge
    /// **from the host**, and the canonical copy is not touched at all.
    #[tokio::test]
    async fn a_restore_re_converges_the_guest_and_never_writes_to_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "the canonical version").unwrap();
        let (syncers, guest, sessions) = seeded_lab(dir.path(), state.path(), "/src/main.rs").await;

        // vmlab performs the restore, so it brackets it: the syncer comes off
        // the workspace first, and no pass can be in flight while the guest is
        // rewound.
        let ws = workspace(dir.path(), state.path());
        restore_bracket(&syncers, &ws, || rewind(&guest)).await;

        let converged = {
            let guest = guest.clone();
            eventually(move || {
                guest.text("/src/main.rs").as_deref() == Some("the canonical version")
                    && guest.get("/src/experiment.rs").is_none()
            })
            .await
        };
        assert!(
            converged,
            "the guest was left rolled back: {:?}",
            guest.paths()
        );

        // **Nothing flows guest→host.** The rolled-back copy and the guest's
        // own unsynced file both end here, and neither reached the tree
        // nothing re-derives.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("main.rs")).unwrap(),
            "the canonical version",
        );
        assert!(!dir.path().join("experiment.rs").exists());
        syncers.stop("dev01").await;
        let _ = sessions;
    }

    /// **The re-seed completes before the watch reopens**, or the syncer's own
    /// writes fill a fresh dirty set with tens of thousands of self-inflicted
    /// paths — and **a restore takes the bracket's re-seed rather than a
    /// stat-walk**, because vmlab already knows what is in that tree.
    #[tokio::test]
    async fn the_re_seed_finishes_before_the_watch_reopens_and_takes_no_stat_walk() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "the canonical version").unwrap();
        let (syncers, guest, sessions) = seeded_lab(dir.path(), state.path(), "/src/main.rs").await;

        let opens_before = sessions.opens().len();
        let ws = workspace(dir.path(), state.path());
        let writes_before = restore_bracket(&syncers, &ws, || {
            rewind(&guest);
            guest.writes().len()
        })
        .await;

        let converged = {
            let guest = guest.clone();
            eventually(move || {
                guest.text("/src/main.rs").as_deref() == Some("the canonical version")
            })
            .await
        };
        assert!(converged);
        let reopened = {
            let sessions = sessions.clone();
            eventually(move || sessions.opens().len() > opens_before).await
        };
        assert!(reopened, "the watch never came back");

        // The watch that follows the restore opened only after the re-seed had
        // already written the tree back.
        let after = sessions.opened_after();
        assert!(
            after[opens_before] > writes_before,
            "the watch reopened before the re-seed wrote anything: {after:?} against \
             {writes_before} writes",
        );

        // And no walk was taken for it. A rescan is what answers *we do not
        // know what the guest did*; here vmlab does.
        let syncer = syncers.get("dev01").await.expect("still running");
        let settled = eventually_async(|| async { syncer.report().passes > 0 }).await;
        assert!(settled);
        assert_eq!(
            syncer.report().rescans,
            0,
            "a restore must not answer itself with a stat-walk",
        );
        assert!(syncer.report().reseed.is_none(), "the barrier never lifted");
        syncers.stop("dev01").await;
    }

    /// A restore discards the guest side by design, so the halt it was
    /// carrying goes with it: a developer who asked for the restore has
    /// already answered the question the halt was asking.
    #[tokio::test]
    async fn a_restore_clears_the_halt_it_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (syncers, guest, sessions) = halted_lab(dir.path(), state.path()).await;
        diverge(dir.path(), &guest, &sessions);
        halt_of(&syncers, "dev01").await.expect("nothing halted");

        // Refused while the halt stands…
        let refused = syncers
            .before_restore("dev01", false, None)
            .await
            .expect_err("a halted workspace refuses an unasked restore");
        assert!(
            refused
                .to_string()
                .contains(super::super::bracket::DISCARD_FLAG),
            "{refused:#}"
        );
        // …and allowed once it is asked for by name.
        syncers.before_restore("dev01", true, None).await.unwrap();

        let ws = workspace(dir.path(), state.path());
        restore_bracket(&syncers, &ws, || {}).await;

        let cleared = eventually_async(|| async {
            syncers
                .get("dev01")
                .await
                .is_some_and(|s| s.report().passes > 0 && s.report().halt.is_none())
        })
        .await;
        assert!(cleared, "the halt outlived the restore that discarded it");
        assert_eq!(
            guest.text("/src/main.rs").as_deref(),
            Some("the host's version"),
            "the guest kept the copy the restore was told to throw away",
        );
        syncers.stop("dev01").await;
    }

    /// **Capture refuses with no escape** when the guest holds work the
    /// canonical copy has never seen — and lets an in-step workspace through,
    /// which is the case that must stay cheap.
    #[tokio::test]
    async fn a_capture_flushes_first_and_refuses_on_unsynced_guest_work() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (syncers, guest, sessions) = halted_lab(dir.path(), state.path()).await;

        // In step: the pre-flight flush is the whole of the check, and it
        // passes.
        syncers.before_capture("dev01", None).await.unwrap();

        // A guest-side save that has not drained yet. The flush carries it,
        // which is the other half of the bracket: flushing before capture is
        // what makes the snapshot coherent with the host tree.
        guest.file("/src/main.rs", "typed just now", 900);
        sessions.watcher.mark("main.rs");
        syncers.before_capture("dev01", None).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("main.rs")).unwrap(),
            "typed just now",
            "the pre-flight flush did not carry the guest's work",
        );

        // A halt, which the flush cannot clear: refused, and with no flag on
        // offer.
        diverge(dir.path(), &guest, &sessions);
        halt_of(&syncers, "dev01").await.expect("nothing halted");
        let refused = syncers
            .before_capture("dev01", None)
            .await
            .expect_err("a halted workspace is not a coherent capture");
        let said = format!("{refused:#}");
        assert!(said.contains("no flag for this"), "{said}");
        assert!(said.contains(super::super::bracket::NOT_A_BACKUP), "{said}");
        syncers.stop("dev01").await;
    }

    /// A machine that is not a dev machine, or has no workspace, has nothing
    /// to bracket — and snapshotting one must not grow a dev machine's costs.
    #[tokio::test]
    async fn a_machine_with_no_workspace_brackets_nothing() {
        let syncers = WorkspaceSyncers::default();
        syncers.before_capture("plain-vm", None).await.unwrap();
        syncers
            .before_restore("plain-vm", false, None)
            .await
            .unwrap();
        assert!(syncers.suspend("plain-vm").await.is_none());
    }

    /// **A restore does not need a running machine.** `vmlab down` takes the
    /// syncer with it, so the fact that the guest was rewound rides the
    /// ledger — otherwise the next `up` would start a syncer that stat-walks a
    /// rolled-back tree, read five hundred old files as guest-side edits, and
    /// carry them onto the canonical copy. Which is the whole hazard.
    #[tokio::test]
    async fn a_restore_of_a_stopped_machine_still_owes_the_re_seed_when_it_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "the canonical version").unwrap();
        let (syncers, guest, sessions) = seeded_lab(dir.path(), state.path(), "/src/main.rs").await;

        // The machine goes down, taking its syncer with it.
        syncers.stop("dev01").await;
        assert!(syncers.suspend("dev01").await.is_none());

        // …and is restored while stopped. All the restore can do is leave the
        // note, which is exactly what it does.
        let ws = workspace(dir.path(), state.path());
        syncers.mark_rewound(&ws).await.unwrap();
        rewind(&guest);

        // `up` again.
        let (events, _rx) = EventLog::recording("lab", state.path().join("events.jsonl"));
        syncers.start(ws.clone(), sessions.clone(), events).await;

        let converged = {
            let guest = guest.clone();
            eventually(move || {
                guest.text("/src/main.rs").as_deref() == Some("the canonical version")
                    && guest.get("/src/experiment.rs").is_none()
            })
            .await
        };
        assert!(
            converged,
            "the note on the ledger bought nothing: {:?}",
            guest.paths()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("main.rs")).unwrap(),
            "the canonical version",
            "the rolled-back copy reached the canonical tree",
        );
        // …and the note is spent, so an ordinary restart does not re-seed
        // again.
        let syncer = syncers.get("dev01").await.expect("running");
        assert!(eventually_async(|| async { syncer.report().passes > 0 }).await);
        assert!(
            !Ledger::load(&ws.ledger_path, &ws.host_root, &ws.guest_root).reseed_owed,
            "the re-seed never cleared its own note",
        );
        syncers.stop("dev01").await;
    }

    /// The mirror case: a halt must not be lost by stopping the machine, or
    /// the very next restore destroys the guest copy of every conflicting path
    /// unasked.
    #[tokio::test]
    async fn a_halt_survives_the_machine_stopping_so_a_restore_still_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (syncers, guest, sessions) = halted_lab(dir.path(), state.path()).await;
        diverge(dir.path(), &guest, &sessions);
        halt_of(&syncers, "dev01").await.expect("nothing halted");
        syncers.stop("dev01").await;

        let ws = workspace(dir.path(), state.path());
        let refused = syncers
            .before_restore("dev01", false, Some(&ws))
            .await
            .expect_err("a halt outlives the machine that was holding it");
        let said = format!("{refused:#}");
        assert!(said.contains("main.rs"), "{said}");
        assert!(said.contains(super::super::bracket::DISCARD_FLAG), "{said}");
        // A capture of a stopped machine refuses on the same recorded halt —
        // it cannot flush one, but it can still decline to freeze a
        // disagreement.
        assert!(
            syncers.before_capture("dev01", Some(&ws)).await.is_err(),
            "a stopped, halted workspace is not a coherent capture",
        );
        // …and the flag answers it, as it does for a running one.
        syncers
            .before_restore("dev01", true, Some(&ws))
            .await
            .unwrap();
    }

    /// A stopped machine whose workspace agrees with itself is snapshottable.
    /// Refusing every down dev machine would be a bigger obstruction than the
    /// incoherence it guards against.
    #[tokio::test]
    async fn a_stopped_machine_in_step_is_still_snapshottable() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "agreed").unwrap();
        let (syncers, _guest, _sessions) =
            seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
        syncers.stop("dev01").await;

        let ws = workspace(dir.path(), state.path());
        syncers.before_capture("dev01", Some(&ws)).await.unwrap();
        syncers
            .before_restore("dev01", false, Some(&ws))
            .await
            .unwrap();
    }

    /// **The note cannot be written under a running syncer**, and the refusal
    /// is a check rather than a comment.
    ///
    /// A running loop holds the ledger in memory and saves it whole, so a note
    /// written while a pass was still in flight is erased by that pass when it
    /// completes — and nothing says so: the resumed syncer simply stat-walks a
    /// rolled-back tree and carries every file in it onto the canonical copy.
    /// A comment would have been one edit away from being wrong, and the
    /// failure it guards is silent, which is the combination §19.6 refuses.
    #[tokio::test]
    async fn the_rewind_note_refuses_to_be_written_under_a_running_syncer() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "the canonical version").unwrap();
        let (syncers, _guest, _sessions) =
            seeded_lab(dir.path(), state.path(), "/src/main.rs").await;

        let ws = workspace(dir.path(), state.path());
        let refused = syncers
            .mark_rewound(&ws)
            .await
            .expect_err("the syncer is still running");
        let said = format!("{refused:#}");
        assert!(said.contains("still running"), "{said}");
        assert!(said.contains("Suspend first"), "{said}");
        assert!(
            !Ledger::load(&ws.ledger_path, &ws.host_root, &ws.guest_root).reseed_owed,
            "the note was written anyway",
        );

        // …and it lands once the syncer is off, which is the order the bracket
        // takes.
        let bracket = syncers.suspend("dev01").await.expect("a workspace to hold");
        syncers.mark_rewound(&ws).await.unwrap();
        assert!(Ledger::load(&ws.ledger_path, &ws.host_root, &ws.guest_root).reseed_owed);
        syncers.resume(bracket).await;
        syncers.stop("dev01").await;
    }

    /// A machine that owes a re-seed holds neither version whole, so capturing
    /// it would freeze a tree mid-rewrite. The `up` that runs the re-seed is
    /// the way out, and the refusal says so rather than offering a flag.
    #[tokio::test]
    async fn a_stopped_machine_that_still_owes_a_re_seed_refuses_a_capture() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "agreed").unwrap();
        let (syncers, _guest, _sessions) =
            seeded_lab(dir.path(), state.path(), "/src/main.rs").await;
        syncers.stop("dev01").await;

        let ws = workspace(dir.path(), state.path());
        syncers.mark_rewound(&ws).await.unwrap();

        let refused = syncers
            .before_capture("dev01", Some(&ws))
            .await
            .expect_err("a tree that has not re-converged is not a coherent capture");
        let said = format!("{refused:#}");
        assert!(said.contains("has not been carried back"), "{said}");
        assert!(said.contains("no flag for this"), "{said}");

        // …and a *restore* is not refused for it: it is about to ask for
        // another re-seed, and the second answers the first.
        syncers
            .before_restore("dev01", false, Some(&ws))
            .await
            .unwrap();
    }
}
