//! The **pull ledger**: the lifecycle a deferred template or container-image
//! download moves through, held as a value.
//!
//! Build never downloads (PRD §6.4): a registry template or container image
//! that is not in the local cache becomes a *pending* job, and `up` / `start` /
//! `pull` drain the list. The state each job moves through — pending, active,
//! progress, done, error, cancelled — the arithmetic feeding the console's
//! progress bar, and the events the web UI's download panel listens for all
//! used to live inside the runtime, reachable only by pointing a lab at a
//! registry.
//!
//! Here they are a value. The ledger performs no I/O: it decides *what* to
//! download and *what to say about it*, and returns [`PullEvent`]s for the
//! caller to emit. The cancellation handle is a type parameter for the same
//! reason — the ledger says a download is cancellable and hands its handle
//! back; aborting the task is the executor's job.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::status::{PullKind, PullStatus};

/// One outstanding deferred download.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PullJob {
    /// A registry template backing a VM's disk.
    Template { reference: String, arch: String },
    /// A container image.
    Image { reference: String, arch: String },
}

impl PullJob {
    pub fn reference(&self) -> &str {
        match self {
            PullJob::Template { reference, .. } | PullJob::Image { reference, .. } => reference,
        }
    }

    pub fn arch(&self) -> &str {
        match self {
            PullJob::Template { arch, .. } | PullJob::Image { arch, .. } => arch,
        }
    }

    /// What kind of artefact this is — the vocabulary `status` reports in and
    /// the prefix of the events it emits, both from [`PullKind`] so the two
    /// cannot name the same download differently (ADR-0004).
    pub fn kind(&self) -> PullKind {
        match self {
            PullJob::Template { .. } => PullKind::Template,
            PullJob::Image { .. } => PullKind::Container,
        }
    }

    /// What the unit counters in a progress event are called: a template
    /// arrives in chunks, an image in layers.
    fn unit_fields(&self) -> (&'static str, &'static str) {
        match self {
            PullJob::Template { .. } => ("chunk", "chunks"),
            PullJob::Image { .. } => ("layer", "layers"),
        }
    }
}

/// One download, and every machine waiting on it. Two VMs declaring the same
/// registry template share a batch, so the work is performed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullBatch {
    pub job: PullJob,
    /// The machines this download satisfies, in name order. Never empty.
    pub machines: Vec<String>,
}

/// How far a download has got, in the units the transport reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PullProgress {
    /// Which chunk/layer is in flight, and how many there are in total.
    pub unit: usize,
    pub units: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// How a download ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullOutcome {
    Done,
    /// Aborted through [`PullLedger::cancel`]; the pending entry survives so a
    /// later `up`/`pull` retries.
    Cancelled,
    /// Failed, with the reason the console shows.
    Failed(String),
}

/// What cancelling a machine's download can mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cancellation<H> {
    /// A download is running: abort this handle. `machines` is everything the
    /// batch was serving, all of which lose it.
    Active { handle: H, machines: Vec<String> },
    /// Queued but not started — there is no task to abort, and the job stays
    /// pending.
    Pending,
    /// Nothing queued and nothing running for that machine.
    Unknown,
}

/// An event the caller emits verbatim on the lab's event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullEvent {
    pub name: String,
    pub payload: Value,
}

/// A download in flight.
#[derive(Debug, Clone)]
struct ActivePull<H> {
    job: PullJob,
    machines: Vec<String>,
    bytes_done: u64,
    bytes_total: u64,
    percent: u32,
    handle: H,
}

/// The deferred-download work list plus whatever is running against it.
///
/// `H` is the executor's cancellation handle — `tokio::task::AbortHandle` in
/// the daemon, anything at all in a test.
#[derive(Debug)]
pub struct PullLedger<H = tokio::task::AbortHandle> {
    pending: BTreeMap<String, PullJob>,
    active: Vec<ActivePull<H>>,
}

impl<H> PullLedger<H> {
    /// A ledger over the jobs `LabRuntime::build` deferred, keyed by machine.
    pub fn new(pending: BTreeMap<String, PullJob>) -> Self {
        Self {
            pending,
            active: Vec::new(),
        }
    }

    /// Nothing is waiting to be downloaded anywhere — the common case, and
    /// the one that keeps a fully-cached lab offline.
    pub fn nothing_pending(&self) -> bool {
        self.pending.is_empty()
    }

    /// Is this machine still waiting on a download? Drives `status`'s
    /// `cached` flag (and so the console's Download button).
    pub fn is_pending(&self, machine: &str) -> bool {
        self.pending.contains_key(machine)
    }

    /// The downloads `targets` needs, one entry per distinct artefact.
    /// An empty `targets` means the whole lab.
    ///
    /// Machines wanting the same reference at the same arch are grouped, so
    /// a lab of ten VMs off one registry template downloads it once rather
    /// than ten times.
    pub fn batches(&self, targets: &[String]) -> Vec<PullBatch> {
        let mut by_job: BTreeMap<&PullJob, Vec<String>> = BTreeMap::new();
        for (machine, job) in &self.pending {
            if targets.is_empty() || targets.iter().any(|t| t == machine) {
                by_job.entry(job).or_default().push(machine.clone());
            }
        }
        by_job
            .into_iter()
            .map(|(job, machines)| PullBatch {
                job: job.clone(),
                machines,
            })
            .collect()
    }

    /// Register `batch` as running under `handle` and announce it — one
    /// `<kind>.pull.start` per waiting machine, because the console keys its
    /// download panel by machine.
    pub fn begin(&mut self, batch: &PullBatch, handle: H) -> Vec<PullEvent> {
        self.active.push(ActivePull {
            job: batch.job.clone(),
            machines: batch.machines.clone(),
            bytes_done: 0,
            bytes_total: 0,
            percent: 0,
            handle,
        });
        let job = &batch.job;
        batch
            .machines
            .iter()
            .map(|m| PullEvent {
                name: format!("{}.pull.start", job.kind()),
                payload: json!({
                    job.kind().subject(): m,
                    "reference": job.reference(),
                    "arch": job.arch(),
                }),
            })
            .collect()
    }

    /// Record a progress report against the batch `machine` belongs to and
    /// announce it. Empty when nothing is running for that machine.
    pub fn progress(&mut self, machine: &str, p: PullProgress) -> Vec<PullEvent> {
        let percent = pull_percent(p.bytes_done, p.bytes_total);
        let Some(active) = self.active.iter_mut().find(|a| holds(a, machine)) else {
            return Vec::new();
        };
        active.bytes_done = p.bytes_done;
        active.bytes_total = p.bytes_total;
        active.percent = percent;
        let job = &active.job;
        let (unit, units) = job.unit_fields();
        active
            .machines
            .iter()
            .map(|m| PullEvent {
                name: format!("{}.pull.progress", job.kind()),
                payload: json!({
                    job.kind().subject(): m,
                    "reference": job.reference(),
                    unit: p.unit,
                    units: p.units,
                    "bytes_done": p.bytes_done,
                    "bytes_total": p.bytes_total,
                    "percent": percent,
                }),
            })
            .collect()
    }

    /// Retire the batch `machine` belongs to. A [`PullOutcome::Done`] clears
    /// the pending entries it satisfied; anything else leaves them for retry.
    /// Empty when nothing is running for that machine.
    pub fn finish(&mut self, machine: &str, outcome: PullOutcome) -> Vec<PullEvent> {
        let Some(at) = self.active.iter().position(|a| holds(a, machine)) else {
            return Vec::new();
        };
        let active = self.active.remove(at);
        let job = &active.job;
        if outcome == PullOutcome::Done {
            for m in &active.machines {
                self.pending.remove(m);
            }
        }
        let suffix = match &outcome {
            PullOutcome::Done => "done",
            PullOutcome::Cancelled => "cancelled",
            PullOutcome::Failed(_) => "error",
        };
        active
            .machines
            .iter()
            .map(|m| {
                let mut payload = json!({
                    job.kind().subject(): m,
                    "reference": job.reference(),
                });
                if let PullOutcome::Failed(err) = &outcome {
                    payload["error"] = json!(err);
                }
                PullEvent {
                    name: format!("{}.pull.{suffix}", job.kind()),
                    payload,
                }
            })
            .collect()
    }

    /// What cancelling `machine`'s download means right now. The ledger does
    /// not abort anything itself — it hands back the handle and says who else
    /// the abort takes with it.
    pub fn cancel(&self, machine: &str) -> Cancellation<&H> {
        if let Some(active) = self.active.iter().find(|a| holds(a, machine)) {
            return Cancellation::Active {
                handle: &active.handle,
                machines: active.machines.clone(),
            };
        }
        if self.pending.contains_key(machine) {
            return Cancellation::Pending;
        }
        Cancellation::Unknown
    }

    /// Every in-flight download, one row per waiting machine in name order —
    /// what `status` reports (ADR-0004) so a surface connecting mid-pull still
    /// shows progress rather than a machine that looks stuck.
    pub fn snapshot(&self) -> Vec<PullStatus> {
        let mut rows: Vec<PullStatus> = self
            .active
            .iter()
            .flat_map(|a| {
                a.machines.iter().map(|m| PullStatus {
                    machine: m.clone(),
                    kind: a.job.kind(),
                    reference: a.job.reference().to_string(),
                    bytes_done: a.bytes_done,
                    bytes_total: a.bytes_total,
                    percent: a.percent,
                })
            })
            .collect();
        rows.sort_by(|a, b| a.machine.cmp(&b.machine));
        rows
    }
}

fn holds<H>(active: &ActivePull<H>, machine: &str) -> bool {
    active.machines.iter().any(|m| m == machine)
}

/// Percent complete, saturating and total-safe: a transport that has not
/// worked out its byte total yet reports zero rather than dividing by it.
pub fn pull_percent(bytes_done: u64, bytes_total: u64) -> u32 {
    bytes_done
        .saturating_mul(100)
        .checked_div(bytes_total)
        .unwrap_or(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(reference: &str) -> PullJob {
        PullJob::Template {
            reference: reference.to_string(),
            arch: "x86_64".to_string(),
        }
    }

    fn image(reference: &str) -> PullJob {
        PullJob::Image {
            reference: reference.to_string(),
            arch: "x86_64".to_string(),
        }
    }

    fn ledger(jobs: &[(&str, PullJob)]) -> PullLedger<u32> {
        PullLedger::new(
            jobs.iter()
                .map(|(m, j)| (m.to_string(), j.clone()))
                .collect(),
        )
    }

    fn names(events: &[PullEvent]) -> Vec<&str> {
        events.iter().map(|e| e.name.as_str()).collect()
    }

    /// The whole lifecycle as a value: pending → active → progress → done,
    /// with no registry anywhere near it.
    #[test]
    fn a_download_runs_its_full_lifecycle() {
        let mut led = ledger(&[("web", template("reg/t:1"))]);
        assert!(!led.nothing_pending());
        assert!(led.is_pending("web"));

        let batch = led.batches(&[]).remove(0);
        assert_eq!(names(&led.begin(&batch, 1)), ["template.pull.start"]);
        assert_eq!(
            names(&led.progress(
                "web",
                PullProgress {
                    unit: 1,
                    units: 4,
                    bytes_done: 50,
                    bytes_total: 200,
                }
            )),
            ["template.pull.progress"]
        );
        assert_eq!(led.snapshot()[0].percent, 25);
        assert_eq!(
            names(&led.finish("web", PullOutcome::Done)),
            ["template.pull.done"]
        );
        assert!(led.snapshot().is_empty(), "no longer downloading");
        assert!(led.nothing_pending(), "a completed pull clears its job");
    }

    /// A transport that has not sized the artefact yet must not reach the
    /// console's progress bar through a division by zero.
    #[test]
    fn progress_with_no_total_is_zero_percent() {
        assert_eq!(pull_percent(0, 0), 0);
        assert_eq!(pull_percent(4096, 0), 0);
        assert_eq!(pull_percent(u64::MAX, 0), 0);
        // And a terabyte-scale artefact scales without overflowing.
        assert_eq!(pull_percent(512 << 30, 1024 << 30), 50);
    }

    /// Two VMs off one registry template download it once.
    #[test]
    fn machines_wanting_the_same_artefact_share_one_download() {
        let led = ledger(&[
            ("app", template("reg/t:1")),
            ("web", template("reg/t:1")),
            ("db", template("reg/other:1")),
        ]);
        let batches = led.batches(&[]);
        assert_eq!(batches.len(), 2, "one per distinct artefact: {batches:#?}");
        let shared = batches
            .iter()
            .find(|b| b.job.reference() == "reg/t:1")
            .unwrap();
        assert_eq!(shared.machines, ["app", "web"]);
    }

    /// Same reference, different arch, is a different artefact.
    #[test]
    fn a_differing_arch_is_not_shared() {
        let led = PullLedger::<u32>::new(BTreeMap::from([
            ("x".to_string(), template("reg/t:1")),
            (
                "a".to_string(),
                PullJob::Template {
                    reference: "reg/t:1".into(),
                    arch: "aarch64".into(),
                },
            ),
        ]));
        assert_eq!(led.batches(&[]).len(), 2);
    }

    /// A shared download announces itself to every machine waiting on it, so
    /// the console's per-machine panel lights up for all of them.
    #[test]
    fn a_shared_download_reports_to_every_waiting_machine() {
        let mut led = ledger(&[("app", template("reg/t:1")), ("web", template("reg/t:1"))]);
        let batch = led.batches(&[]).remove(0);
        let started = led.begin(&batch, 1);
        assert_eq!(started.len(), 2);
        let subjects: Vec<&str> = started
            .iter()
            .map(|e| e.payload["vm"].as_str().unwrap())
            .collect();
        assert_eq!(subjects, ["app", "web"]);
        assert_eq!(led.finish("app", PullOutcome::Done).len(), 2);
        assert!(
            led.nothing_pending(),
            "one download satisfied both machines"
        );
    }

    /// A subset only drains its own machines' work.
    #[test]
    fn a_subset_leaves_other_machines_pending() {
        let led = ledger(&[("web", template("reg/t:1")), ("db", image("redis"))]);
        let batches = led.batches(&["db".into()]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].machines, ["db"]);
    }

    /// Cancelling a queued download is not the same as cancelling a running
    /// one: there is no task to abort, and nothing to report.
    #[test]
    fn cancelling_pending_and_active_are_distinguishable() {
        let mut led = ledger(&[("web", template("reg/t:1")), ("db", image("redis"))]);
        assert_eq!(led.cancel("web"), Cancellation::Pending);
        assert_eq!(led.cancel("nobody"), Cancellation::Unknown);

        let batch = led
            .batches(&[])
            .into_iter()
            .find(|b| b.machines == ["web"])
            .unwrap();
        led.begin(&batch, 7);
        assert_eq!(
            led.cancel("web"),
            Cancellation::Active {
                handle: &7,
                machines: vec!["web".to_string()],
            }
        );
        // The other machine's job never started and is still only queued.
        assert_eq!(led.cancel("db"), Cancellation::Pending);
    }

    /// Cancelling one machine of a shared download takes the rest with it,
    /// and says so.
    #[test]
    fn cancelling_a_shared_download_names_everyone_it_takes() {
        let mut led = ledger(&[("app", template("reg/t:1")), ("web", template("reg/t:1"))]);
        let batch = led.batches(&[]).remove(0);
        led.begin(&batch, 9);
        assert_eq!(
            led.cancel("web"),
            Cancellation::Active {
                handle: &9,
                machines: vec!["app".to_string(), "web".to_string()],
            }
        );
        let events = led.finish("web", PullOutcome::Cancelled);
        assert_eq!(
            names(&events),
            ["template.pull.cancelled", "template.pull.cancelled"]
        );
        assert!(
            led.is_pending("app") && led.is_pending("web"),
            "a cancelled pull stays pending for retry"
        );
    }

    /// A failure names the artefact and the reason, so the console can show
    /// both.
    #[test]
    fn a_failure_reports_the_artefact_and_why() {
        let mut led = ledger(&[("db", image("redis:7"))]);
        let batch = led.batches(&[]).remove(0);
        led.begin(&batch, 1);
        let events = led.finish("db", PullOutcome::Failed("connection reset".into()));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "container.pull.error");
        assert_eq!(events[0].payload["container"], "db");
        assert_eq!(events[0].payload["reference"], "redis:7");
        assert_eq!(events[0].payload["error"], "connection reset");
        assert!(
            led.is_pending("db"),
            "a failed pull stays pending for retry"
        );
    }

    /// The payload shape the console's download panel is written against —
    /// a VM download names its machine `vm` and counts chunks; a container
    /// download names it `container` and counts layers.
    #[test]
    fn each_kind_keeps_its_own_event_vocabulary() {
        let mut led = ledger(&[("web", template("reg/t:1")), ("db", image("redis:7"))]);
        for batch in led.batches(&[]) {
            led.begin(&batch, 1);
        }
        let p = PullProgress {
            unit: 2,
            units: 5,
            bytes_done: 1,
            bytes_total: 4,
        };
        let vm = led.progress("web", p).remove(0);
        assert_eq!(vm.name, "template.pull.progress");
        assert_eq!(vm.payload["vm"], "web");
        assert_eq!(vm.payload["chunk"], 2);
        assert_eq!(vm.payload["chunks"], 5);
        assert_eq!(vm.payload["percent"], 25);

        let container = led.progress("db", p).remove(0);
        assert_eq!(container.name, "container.pull.progress");
        assert_eq!(container.payload["container"], "db");
        assert_eq!(container.payload["layer"], 2);
        assert_eq!(container.payload["layers"], 5);
    }

    /// Progress and completion for a machine with nothing running are
    /// no-ops, not panics — a cancel can land between the two.
    #[test]
    fn reports_for_an_idle_machine_are_ignored() {
        let mut led = ledger(&[("web", template("reg/t:1"))]);
        assert!(led.progress("web", PullProgress::default()).is_empty());
        assert!(led.finish("web", PullOutcome::Done).is_empty());
        assert!(led.is_pending("web"), "nothing ran, so nothing completed");
    }

    /// `status` lists one row per waiting machine, in name order.
    #[test]
    fn status_lists_a_row_per_waiting_machine() {
        let mut led = ledger(&[("web", template("reg/t:1")), ("app", template("reg/t:1"))]);
        let batch = led.batches(&[]).remove(0);
        led.begin(&batch, 1);
        led.progress(
            "web",
            PullProgress {
                unit: 1,
                units: 2,
                bytes_done: 30,
                bytes_total: 60,
            },
        );
        let rows = led.snapshot();
        assert_eq!(
            rows.iter().map(|r| r.machine.as_str()).collect::<Vec<_>>(),
            ["app", "web"]
        );
        assert!(
            rows.iter()
                .all(|r| r.percent == 50 && r.kind == PullKind::Template)
        );
    }
}
