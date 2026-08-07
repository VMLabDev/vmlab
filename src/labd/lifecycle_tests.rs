//! Machine lifecycle, driven through [`Machine`] against the in-memory
//! hypervisor (ADR-0001).
//!
//! Everything here is the code that decides *when* a machine is running — the
//! start ladder, the power-state machine, the exit monitor, readiness gating,
//! teardown ordering and the stop ladder. None
//! of it needs a hypervisor to be correct, and until the seam handed back its
//! own handle types none of it could be reached without one.
//!
//! Two rules hold throughout:
//!
//! - **Assert through `Machine`.** Power state, readiness, which callbacks
//!   fired and in what order, and the error a failed operation returns. Never
//!   how many times the seam was called: that is the implementation, and a
//!   test of it breaks on the next refactor without catching a defect.
//! - **One set of assertions for both kinds.** A VM and a container are set up
//!   differently and then held to the same contract — see [`stops_idempotently`]
//!   and the pairs of tests that call it.
//!
//! Not covered here, deliberately: QMP snapshot save/load, the vhost-user
//! handshake and fast-path offload genuinely need a hypervisor and stay
//! verified against a running lab. The fake does not pretend to implement
//! them — `Machine::snapshot` under it reports "not running", which is the
//! honest answer.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use super::container::{ContainerDirs, ContainerInstance};
use super::events::EventLog;
use super::hypervisor::fake::{FakeHypervisor, Run, Script};
use super::machine::{LabServices, Machine};
use super::vm::{PowerState, StopReason, TemplateParts, VmDirs, VmInstance};
use crate::oci::image::model::{ImageConfig, RuntimeDefaults};
use crate::oci::image::pull::PulledImage;
use crate::profiles::ProfileSet;
use crate::qemu::resolve::testing;

/// Long enough to let a scheduled transition happen, short enough that a test
/// that never gets there fails rather than hangs.
const SETTLE: Duration = Duration::from_secs(5);

/// A machine's own tempdir, with the pieces that exist before a start.
struct Dirs {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Dirs {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        Self { _tmp: tmp, root }
    }
}

// ---- what a caller observes -------------------------------------------------

/// One `on_exit` report, as a consumer receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Exited {
    reason: StopReason,
    /// The emulator's exit status, verbatim.
    status: String,
}

/// The callbacks a `start` installs, recorded. This is the whole of what a
/// consumer of a machine sees happen to it, so it is what the tests assert on.
struct Callbacks {
    exits: mpsc::UnboundedSender<Exited>,
    readies: Arc<AtomicUsize>,
    healths: Arc<std::sync::Mutex<Vec<bool>>>,
}

struct Observed {
    exits: mpsc::UnboundedReceiver<Exited>,
    readies: Arc<AtomicUsize>,
    healths: Arc<std::sync::Mutex<Vec<bool>>>,
}

fn callbacks() -> (Callbacks, Observed) {
    let (tx, rx) = mpsc::unbounded_channel();
    let readies = Arc::new(AtomicUsize::new(0));
    let healths: Arc<std::sync::Mutex<Vec<bool>>> = Arc::default();
    (
        Callbacks {
            exits: tx,
            readies: readies.clone(),
            healths: healths.clone(),
        },
        Observed {
            exits: rx,
            readies,
            healths,
        },
    )
}

async fn collect_events(
    rx: &mut broadcast::Receiver<crate::proto::Event>,
    wanted: &[&str],
    count: usize,
) -> Vec<crate::proto::Event> {
    let mut events = Vec::new();
    while events.len() < count {
        let event = tokio::time::timeout(SETTLE, rx.recv())
            .await
            .expect("no lifecycle event")
            .expect("event stream closed");
        if wanted.contains(&event.event.as_str()) {
            events.push(event);
        }
    }
    events
}

struct TestLab {
    events: Arc<EventLog>,
}

#[async_trait::async_trait]
impl LabServices for TestLab {
    fn events(&self) -> &Arc<EventLog> {
        &self.events
    }

    async fn ensure_pulled(&self, _machine: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn ensure_shares(&self) {}

    async fn attach_nic(
        &self,
        _segment: &str,
        sock: &std::path::Path,
        _mac: crate::config::model::MacAddr,
        _isolated: bool,
        _tap_ok: bool,
    ) -> anyhow::Result<crate::net::fastpath::NicAttachment> {
        Ok(crate::net::fastpath::NicAttachment::Stream {
            sock: sock.to_path_buf(),
        })
    }

    async fn machine_ready(&self, _machine: &str) {}

    async fn smb_mount_plan(
        &self,
        _machine: &str,
        _os: crate::smb::OsHint,
    ) -> Vec<crate::smb::MountStep> {
        Vec::new()
    }
}

impl Observed {
    /// The next exit report, or a failure if none arrives.
    async fn exit(&mut self) -> Exited {
        self.exit_within(SETTLE).await
    }

    /// [`exit`](Self::exit) with an explicit patience. Under paused time the
    /// wait competes with the scripted timers, so it has to outlast them.
    async fn exit_within(&mut self, patience: Duration) -> Exited {
        tokio::time::timeout(patience, self.exits.recv())
            .await
            .expect("no exit reported")
            .expect("exit channel closed")
    }

    fn ready_count(&self) -> usize {
        self.readies.load(Ordering::SeqCst)
    }

    fn health_reports(&self) -> Vec<bool> {
        self.healths.lock().expect("healths").clone()
    }
}

// ---- building the two kinds -------------------------------------------------

/// A VM on a fake hypervisor. `decl` is the `vm "…" { … }` block, so the
/// hardware comes from the real resolution chain rather than from the test
/// (ADR-0008).
///
/// The primary disk is pre-created: preparing disks shells out to `qemu-img`
/// and sits below this test's subject — `ensure_disks` skips what already
/// exists, so nothing here depends on the host's QEMU layout.
fn vm(dirs: &Dirs, decl: &str, script: Script) -> (Arc<VmInstance>, Arc<FakeHypervisor>) {
    let cfg = testing::vm(decl);
    let resolved = crate::qemu::resolve_vm(
        &cfg,
        None,
        &ProfileSet::shipped().expect("shipped profiles"),
    )
    .expect("resolve");
    let vm_dirs = VmDirs {
        local: dirs.root.join("local"),
        run: dirs.root.join("run"),
        logs: dirs.root.join("logs"),
    };
    std::fs::create_dir_all(&vm_dirs.local).expect("local dir");
    std::fs::write(vm_dirs.primary_disk(), b"").expect("disk0");
    let share_hosts: Vec<PathBuf> = cfg
        .shares
        .iter()
        .map(|s| {
            let host = dirs.root.join("shares").join(&s.name);
            std::fs::create_dir_all(&host).expect("share dir");
            host
        })
        .collect();

    let mut vm = VmInstance::new(
        "t",
        cfg,
        vm_dirs,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        None,
        share_hosts,
        TemplateParts {
            resolved,
            backing: None,
            disk_size: Some(10 << 30),
            first_boot: None,
            agent_version: Some("0.1.0".into()),
        },
    );
    let hv = FakeHypervisor::new(script);
    vm.set_hypervisor(hv.clone());
    (vm, hv)
}

/// A container on a fake hypervisor, image already bound. `decl` is the
/// `container "…" { … }` block.
fn container(
    dirs: &Dirs,
    decl: &str,
    script: Script,
) -> (Arc<ContainerInstance>, Arc<FakeHypervisor>) {
    let cfg = testing::container(decl);
    let profiles = ProfileSet::shipped().expect("shipped profiles");
    let resolved = crate::qemu::resolve_container(&cfg, "x86_64", &profiles).expect("resolve");
    let ctr_dirs = ContainerDirs {
        local: dirs.root.join("local"),
        run: dirs.root.join("run"),
        logs: dirs.root.join("logs"),
    };
    std::fs::create_dir_all(&ctr_dirs.local).expect("local dir");
    // As with the VM's disk: the scratch overlay already exists, so no
    // `qemu-img` runs.
    std::fs::write(ctr_dirs.scratch_disk(), b"").expect("scratch");

    let image = PulledImage {
        manifest_digest: "sha256:00".into(),
        config: ImageConfig {
            architecture: "amd64".into(),
            os: "linux".into(),
            config: RuntimeDefaults {
                entrypoint: vec!["/entrypoint".into()],
                ..Default::default()
            },
            rootfs: Default::default(),
        },
        rootfs_image: dirs.root.join("rootfs.sqfs"),
    };
    let mut ctr = ContainerInstance::new(
        "t",
        cfg,
        resolved,
        ctr_dirs,
        Vec::new(),
        Vec::new(),
        Some(image),
        Vec::new(),
    );
    let hv = FakeHypervisor::new(script);
    ctr.set_hypervisor(hv.clone());
    (ctr, hv)
}

/// Start a VM with recorded callbacks.
async fn start_vm(vm: &Arc<VmInstance>, cbs: Callbacks) -> anyhow::Result<()> {
    let Callbacks { exits, readies, .. } = cbs;
    vm.boot(
        move |reason, status| {
            let _ = exits.send(Exited { reason, status });
        },
        move || {
            readies.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await
}

/// Start a container with recorded callbacks.
async fn start_container(ctr: &Arc<ContainerInstance>, cbs: Callbacks) -> anyhow::Result<()> {
    let Callbacks {
        exits,
        readies,
        healths,
    } = cbs;
    ctr.boot(
        move |reason, code| {
            let _ = exits.send(Exited {
                reason,
                // A container's exit status is the code its entrypoint
                // reported over the ctl channel.
                status: format!("{code:?}"),
            });
        },
        move || {
            readies.fetch_add(1, Ordering::SeqCst);
        },
        move |healthy| healths.lock().expect("healths").push(healthy),
    )
    .await
}

/// A plain healthy Linux VM, and the container equivalent.
const LINUX_VM: &str =
    r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB }"#;
const WORKLOAD: &str = r#"container "web" { image = "nginx:1" profile = "container" }"#;

// ---- the contract both kinds are held to ------------------------------------

/// Stopping a machine that is already stopped is a no-op, not an error.
///
/// Teardown runs this on everything it can find, so an error here would turn
/// a partial `down` into a failed one — and a machine that crash-looped its
/// way to Stopped is exactly the case where a caller stops it anyway.
async fn stops_idempotently(m: Arc<dyn Machine>) {
    assert_eq!(m.state().await, PowerState::Stopped);
    m.stop(false)
        .await
        .expect("graceful stop of a stopped machine");
    m.stop(true)
        .await
        .expect("forced stop of a stopped machine");
    assert_eq!(m.state().await, PowerState::Stopped);
}

#[tokio::test]
async fn stopping_a_stopped_vm_is_a_no_op() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    stops_idempotently(vm).await;
}

#[tokio::test]
async fn stopping_a_stopped_container_is_a_no_op() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(&dirs, WORKLOAD, Script::healthy());
    stops_idempotently(ctr).await;
}

// ---- the VM start ladder ----------------------------------------------------

/// The whole point of the seam: a VM's start ladder runs to completion with
/// no KVM, no template and no disk image, and the machine reports itself
/// running and ready through the interface every caller uses.
#[tokio::test]
async fn a_vm_starts_and_becomes_ready_without_a_hypervisor() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let (cbs, observed) = callbacks();

    start_vm(&vm, cbs).await.expect("start");

    let m: Arc<dyn Machine> = vm.clone();
    assert_eq!(m.state().await, PowerState::Running);
    m.wait_ready(SETTLE).await.expect("ready");
    assert_eq!(observed.ready_count(), 1, "on_ready fires exactly once");

    vm.stop(true).await.expect("stop");
}

/// A software TPM that never binds its control socket must fail the start
/// with something a user can act on — not let the emulator spawn and die on a
/// missing chardev, and not leave the VM wedged in Starting.
#[tokio::test]
async fn a_tpm_that_never_binds_fails_the_start() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB tpm = true }"#,
        Script {
            tpm_fails: Some("swtpm never bound its socket".into()),
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();

    let err = start_vm(&vm, cbs).await.expect_err("start must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("dc01"), "names the machine: {msg}");
    assert!(msg.contains("swtpm"), "names what failed: {msg}");
    assert_eq!(
        vm.state().await,
        PowerState::Stopped,
        "a failed start unwinds to Stopped, not Starting"
    );
    assert!(!vm.is_ready().await);
}

/// A filesystem daemon that dies during startup fails the start too — the
/// guest would otherwise boot with a vhost-user chardev nothing is listening
/// on, which QEMU turns into a far less legible failure.
#[tokio::test]
async fn a_virtiofsd_that_dies_fails_the_start() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB
             share "data" { host = "./data" guest = "/mnt/data" transport = "virtiofs" } }"#,
        Script {
            virtiofsd_fails: Some("virtiofsd exited immediately".into()),
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();

    let err = start_vm(&vm, cbs).await.expect_err("start must fail");
    assert!(format!("{err:#}").contains("virtiofsd"), "{err:#}");
    assert_eq!(vm.state().await, PowerState::Stopped);
    assert!(
        !vm.is_ready().await,
        "a machine that never came up is not ready"
    );
    assert!(
        hv.live_helpers().await.is_empty(),
        "a failed start leaves no daemons behind"
    );
}

// ---- the VM exit monitor ----------------------------------------------------

/// An emulator that exits non-zero with nobody having asked is a crash, and
/// the machine settles to Stopped rather than claiming to still be running.
#[tokio::test]
async fn an_emulator_that_exits_nonzero_is_a_crash() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run::dies(Duration::from_millis(20), "exit status: 1")],
            ..Script::healthy()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");

    let exit = observed.exit().await;
    assert_eq!(exit.reason, StopReason::Crashed);
    assert_eq!(exit.status, "exit status: 1");
    vm.wait_state(PowerState::Stopped, SETTLE)
        .await
        .expect("settles to Stopped");
}

/// A guest that powers *itself* off is not a crash, even though nobody asked
/// this daemon for it. `vmlab status` shows the two very differently, so the
/// distinction has to survive the whole path from control channel to callback.
#[tokio::test]
async fn a_guest_initiated_shutdown_is_not_a_crash() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run::guest_powers_off(Duration::from_millis(20))],
            ..Script::healthy()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");

    assert_eq!(observed.exit().await.reason, StopReason::GuestInitiated);
}

/// Teardown has to be complete *before* anyone is told the machine exited: a
/// consumer that reacts to `on_exit` by starting the machine again, or by
/// reporting it, must never see a machine that is simultaneously exited and
/// ready, still agent-up, or still holding its helper daemons.
///
/// Multi-threaded because the assertion runs inside the callback — that is
/// the only place the ordering is observable at all.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_still_up_when_the_exit_callback_fires() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB tpm = true }"#,
        // Stays up until the test kills it, so the agent is demonstrably up
        // first — otherwise "agent-up is cleared" proves nothing.
        Script::healthy(),
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    let seen = vm.clone();
    let seen_hv = hv.clone();
    let handle = tokio::runtime::Handle::current();
    vm.boot(
        move |_reason, _status| {
            let m: Arc<dyn Machine> = seen.clone();
            let hv = seen_hv.clone();
            let snapshot = tokio::task::block_in_place(|| {
                handle.block_on(async {
                    (m.state().await, m.is_ready().await, hv.live_helpers().await)
                })
            });
            let _ = tx.send(snapshot);
        },
        || {},
    )
    .await
    .expect("start");

    vm.wait_agent_up(SETTLE).await.expect("agent up");
    assert!(vm.is_ready().await, "ready before anything ends it");
    assert!(
        !hv.live_helpers().await.is_empty(),
        "the swtpm is running while the machine is"
    );

    vm.stop(true).await.expect("stop");

    let (state, ready, helpers) = tokio::time::timeout(SETTLE, rx.recv())
        .await
        .expect("no exit reported")
        .expect("channel closed");
    assert_eq!(
        state,
        PowerState::Stopped,
        "state settled before the callback"
    );
    assert!(!ready, "a machine cannot be exited and ready at once");
    assert!(
        helpers.is_empty(),
        "the swtpm outlived the machine: {helpers:?}"
    );
    assert!(
        !vm.is_agent_up().await,
        "a stale agent handle must never be handed out after an exit"
    );
}

// ---- the VM stop ladder -----------------------------------------------------

/// The ladder stops at the first rung that works: a guest whose agent answers
/// is asked to shut down and does, so the machine ends cleanly and is never
/// killed. The exit status is how a caller tells those apart.
#[tokio::test]
async fn the_stop_ladder_stops_at_the_first_rung_that_works() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    vm.wait_agent_up(SETTLE).await.expect("agent up");

    vm.stop(false).await.expect("graceful stop");

    let exit = observed.exit().await;
    assert_eq!(exit.reason, StopReason::Requested);
    assert_eq!(
        exit.status, "exit status: 0",
        "the guest shut itself down; nothing had to kill it"
    );
}

/// ...and only falls through when a rung fails. With no agent answering and a
/// guest that ignores ACPI, the ladder must reach the kill rather than give
/// up — a machine an operator asked to stop always stops.
///
/// Time is paused: the middle rung waits 30s for the guest, and the point of
/// the test is the fall-through, not the wait.
#[tokio::test(start_paused = true)]
async fn the_stop_ladder_falls_through_to_the_kill() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            // No agent: rung 1 has nothing to ask.
            agent: false,
            runs: vec![Run {
                ignores_powerdown: true,
                ..Run::forever()
            }],
            ..Script::default()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    assert_eq!(vm.state().await, PowerState::Running);

    vm.stop(false).await.expect("graceful stop");

    let exit = observed.exit().await;
    assert_eq!(exit.reason, StopReason::Requested);
    assert_eq!(
        exit.status, "signal: 9",
        "the guest ignored every graceful rung, so the last one killed it"
    );
}

/// A forced stop skips the graceful rungs outright. That is the whole of the
/// flag's contract: a caller who says `force` is saying "I am not waiting for
/// this guest", and a machine wedged mid-boot has no agent and no ACPI to
/// wait on anyway.
#[tokio::test]
async fn a_forced_stop_skips_the_graceful_rungs() {
    let dirs = Dirs::new();
    // The guest would answer both graceful rungs if it were asked.
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    vm.wait_agent_up(SETTLE).await.expect("agent up");

    vm.stop(true).await.expect("forced stop");

    let exit = observed.exit().await;
    assert_eq!(exit.reason, StopReason::Requested);
    assert_eq!(
        exit.status, "signal: 9",
        "force kills; it does not ask the guest first"
    );
}

// ---- container readiness ----------------------------------------------------

/// A workload container is ready when its entrypoint is running — not when
/// the bundled agent answers. Docker semantics: the entrypoint runs
/// regardless, and an agent hiccup must not wedge readiness.
#[tokio::test]
async fn a_workload_container_is_ready_once_its_entrypoint_runs() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(
        &dirs,
        WORKLOAD,
        Script {
            agent: false,
            runs: vec![Run {
                ctl: vec![(
                    Duration::ZERO,
                    vmlab_cinit_proto::CtlEvent::Started { pid: 7 },
                )],
                ..Run::forever()
            }],
            ..Script::default()
        },
    );
    let (cbs, observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");

    let m: Arc<dyn Machine> = ctr.clone();
    m.wait_ready(SETTLE).await.expect("ready");
    assert_eq!(observed.ready_count(), 1);

    ctr.stop(true).await.expect("stop");
}

/// An idle container has no workload to prove liveness, so its readiness
/// waits for the vmlab-agent instead — its control plane *is* the service.
/// The gate must be the agent and not the healthcheck, which idle mode does
/// not have.
#[tokio::test]
async fn an_idle_container_waits_for_the_agent() {
    let dirs = Dirs::new();
    let decl = r#"container "jump" { image = "alpine:3" profile = "container" mode = :idle }"#;

    // No agent: started, but never ready.
    let no_agent = Dirs::new();
    let (ctr, _hv) = container(
        &no_agent,
        decl,
        Script {
            agent: false,
            runs: vec![Run {
                ctl: vec![(Duration::ZERO, vmlab_cinit_proto::CtlEvent::Idle)],
                ..Run::forever()
            }],
            ..Script::default()
        },
    );
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");
    let err = ctr
        .wait_ready(Duration::from_millis(500))
        .await
        .expect_err("must not be ready without an agent");
    assert!(format!("{err:#}").contains("not ready"), "{err:#}");
    ctr.stop(true).await.expect("stop");

    // Same script, with an agent answering: ready.
    let (ctr, _hv) = container(
        &dirs,
        decl,
        Script {
            agent: true,
            runs: vec![Run {
                ctl: vec![(Duration::ZERO, vmlab_cinit_proto::CtlEvent::Idle)],
                ..Run::forever()
            }],
            ..Script::default()
        },
    );
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");
    ctr.wait_ready(SETTLE)
        .await
        .expect("ready once the agent answers");
    ctr.stop(true).await.expect("stop");
}

/// A healthcheck gates readiness: a container whose check is failing is
/// started but not ready, and the first healthy report opens the gate. Every
/// transition is reported, because a console showing health has to see the
/// unhealthy period too.
#[tokio::test]
async fn a_healthcheck_gates_readiness() {
    use vmlab_cinit_proto::CtlEvent;
    let dirs = Dirs::new();
    let (ctr, _hv) = container(
        &dirs,
        r#"container "web" { image = "nginx:1" profile = "container"
             healthcheck { command = ["/check"] interval = 1s timeout = 1s retries = 3 } }"#,
        Script {
            agent: false,
            runs: vec![Run {
                ctl: vec![
                    (Duration::ZERO, CtlEvent::Started { pid: 7 }),
                    (Duration::ZERO, CtlEvent::Health { healthy: false }),
                    (
                        Duration::from_millis(300),
                        CtlEvent::Health { healthy: true },
                    ),
                ],
                ..Run::forever()
            }],
            ..Script::default()
        },
    );
    let (cbs, observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");

    // Started, unhealthy: the gate holds.
    let err = ctr
        .wait_ready(Duration::from_millis(150))
        .await
        .expect_err("a failing healthcheck keeps the machine unready");
    assert!(format!("{err:#}").contains("not ready"), "{err:#}");

    ctr.wait_ready(SETTLE)
        .await
        .expect("the first healthy report opens the gate");
    assert_eq!(observed.health_reports(), vec![false, true]);

    ctr.stop(true).await.expect("stop");
}

/// Readiness gives up at the timeout it was handed — which is the machine's
/// own [`Machine::ready_timeout`], because a container's entrypoint starts
/// fast while a VM may be running a first-boot provision through a Windows
/// settle reboot.
#[tokio::test]
async fn readiness_times_out_at_the_machines_own_budget() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(&dirs, WORKLOAD, Script::healthy());
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");

    let m: Arc<dyn Machine> = ctr.clone();
    // The two kinds carry different budgets, and a caller reads the budget
    // off the machine rather than guessing from its kind.
    assert_eq!(m.ready_timeout(), Duration::from_secs(300));
    let vm_dirs = Dirs::new();
    let (vm, _hv) = vm(&vm_dirs, LINUX_VM, Script::healthy());
    let vm_m: Arc<dyn Machine> = vm;
    assert_eq!(vm_m.ready_timeout(), Duration::from_secs(600));

    // Nothing ever reports `started`, so the gate never opens.
    let err = m
        .wait_ready(Duration::from_millis(300))
        .await
        .expect_err("must time out");
    let msg = format!("{err:#}");
    assert!(msg.contains("web"), "names the machine: {msg}");
    assert!(msg.contains("not ready"), "{msg}");

    ctr.stop(true).await.expect("stop");
}

// ---- container exit handling ------------------------------------------------

/// A crash is observable but never rescued: the container settles to stopped,
/// and neither lifecycle payload advertises a daemon-owned respawn.
#[tokio::test]
async fn a_crashed_container_stays_stopped_until_explicitly_started() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(
        &dirs,
        WORKLOAD,
        Script {
            agent: false,
            runs: vec![Run::container_exits(Duration::from_millis(20), 1)],
            ..Script::default()
        },
    );
    let (events, mut rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab: Arc<dyn LabServices> = Arc::new(TestLab { events });

    ctr.clone().start(lab.clone()).await.expect("start");

    let exits = collect_events(&mut rx, &["container.crashed", "container.stopped"], 2).await;

    assert_eq!(
        exits
            .iter()
            .map(|event| event.event.as_str())
            .collect::<Vec<_>>(),
        ["container.crashed", "container.stopped"]
    );
    assert!(
        exits
            .iter()
            .all(|event| event.data.get("restarting").is_none()),
        "exit payloads must not promise a daemon-owned respawn: {exits:?}"
    );
    ctr.wait_state(PowerState::Stopped, SETTLE)
        .await
        .expect("stays stopped");

    ctr.clone().start(lab).await.expect("explicit start");
    let after_start = collect_events(
        &mut rx,
        &[
            "container.starting",
            "container.crashed",
            "container.stopped",
        ],
        3,
    )
    .await;
    assert_eq!(
        after_start
            .iter()
            .map(|event| event.event.as_str())
            .collect::<Vec<_>>(),
        [
            "container.starting",
            "container.crashed",
            "container.stopped"
        ]
    );
    ctr.wait_state(PowerState::Stopped, SETTLE)
        .await
        .expect("explicitly started run also settles");
}

/// Two starts cannot both proceed. A second operator or `up` run is refused
/// rather than spawning another emulator against the same sockets and disks.
#[tokio::test]
async fn a_machine_that_is_already_running_refuses_a_second_start() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(&dirs, WORKLOAD, Script::healthy());
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");

    let (cbs, _observed) = callbacks();
    let err = start_container(&ctr, cbs)
        .await
        .expect_err("a second start must be refused");
    assert!(format!("{err:#}").contains("Running"), "{err:#}");

    ctr.stop(true).await.expect("stop");
}

/// A container's stop ladder starts at the ctl channel: cinit signals the
/// container, reaps it, and powers the micro-VM off. Reaching that rung is
/// what makes `vmlab down` a graceful shutdown rather than a power cut.
#[tokio::test]
async fn a_container_stops_through_its_ctl_channel() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(&dirs, WORKLOAD, Script::healthy());
    let (cbs, mut observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");

    ctr.stop(false).await.expect("graceful stop");

    let exit = observed.exit().await;
    assert_eq!(exit.reason, StopReason::Requested);
    assert_eq!(ctr.state().await, PowerState::Stopped);
}

// ---- what the seam does not cover -------------------------------------------

/// The fake does not pretend to be a hypervisor. Snapshots go through QMP,
/// which only real QEMU answers, so a machine running under the fake reports
/// exactly what a stopped one does rather than inventing a result — the
/// honest failure that keeps snapshot coverage where it belongs, against a
/// running lab (ADR-0001).
#[tokio::test]
async fn snapshots_are_out_of_the_fakes_reach_and_say_so() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(&dirs, WORKLOAD, Script::healthy());
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");

    let m: Arc<dyn Machine> = ctr.clone();
    let err = m.snapshot("s1").await.expect_err("no QMP under the fake");
    assert!(format!("{err:#}").contains("not running"), "{err:#}");

    ctr.stop(true).await.expect("stop");
}

/// A container whose host has no guest boot asset cannot start, and says so
/// before anything else is spawned.
#[tokio::test]
async fn a_missing_guest_asset_fails_the_start() {
    let dirs = Dirs::new();
    let (ctr, hv) = container(
        &dirs,
        WORKLOAD,
        Script {
            guest_asset_missing: true,
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();

    let err = start_container(&ctr, cbs).await.expect_err("must fail");
    assert!(format!("{err:#}").contains("guest asset"), "{err:#}");
    assert_eq!(ctr.state().await, PowerState::Stopped);
    assert!(hv.live_helpers().await.is_empty());
}

/// **The repair verb is a VM statement** (§19.4). A container micro-VM's agent
/// lives in the initramfs guest asset this host installed, so it tracks the
/// running vmlab and cannot go stale — and the verb says exactly that, rather
/// than implying a container author has a rebuild to perform or quietly doing
/// nothing.
///
/// Asked of the machine, not of its kind: what the verb reads is
/// [`Machine::agent_origin`], which is why a future machine whose agent also
/// ships with the host is answered correctly without the verb changing.
#[tokio::test]
async fn repairing_a_containers_agent_is_meaningless_and_says_so() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(&dirs, WORKLOAD, Script::healthy());
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");

    let m: Arc<dyn Machine> = ctr.clone();
    let err = super::agent_repair::repair(&m)
        .await
        .expect_err("a container has nothing to repair");
    let msg = format!("{err:#}");
    assert!(msg.contains("guest asset"), "{msg}");
    assert!(msg.contains("cannot go stale"), "{msg}");
    assert!(!msg.contains("rebuild the template"), "{msg}");

    ctr.stop(true).await.expect("stop");
}
