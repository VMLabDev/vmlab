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
//! Not covered here, deliberately: what QMP snapshot save/load actually does
//! to a guest, the vhost-user handshake and fast-path offload genuinely need
//! a hypervisor and stay verified against a running lab. What *is* covered is
//! the caller's side of each capability (ADR-0020): a machine whose
//! hypervisor reports none refuses exactly as a stopped one does, and one the
//! script grants it to gets through to the in-memory double.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use super::container::{ContainerDirs, ContainerInstance};
use super::events::EventLog;
use super::hypervisor::fake::{Capabilities, FakeHypervisor, Run, Script};
use super::machine::{LabServices, Machine};
use super::vm::{Floppy, PowerState, StopReason, TemplateParts, VmDirs, VmInstance};
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
    /// Every `machine_ready` the lab was told of, in order.
    readied: Arc<std::sync::Mutex<Vec<String>>>,
    /// Every `before_resume` and `machine_ready`, interleaved as
    /// `resume:<name>` and `ready:<name>`, so the order of the two shows.
    hooks: Arc<std::sync::Mutex<Vec<String>>>,
    /// Make `before_resume` fail with this, as a recorder that could not
    /// attach would (§11.1, D20).
    refuse_resume: std::sync::Mutex<Option<String>>,
}

impl TestLab {
    fn new(events: Arc<EventLog>) -> Self {
        Self {
            events,
            readied: Arc::default(),
            hooks: Arc::default(),
            refuse_resume: std::sync::Mutex::new(None),
        }
    }

    fn readied(&self) -> Vec<String> {
        self.readied.lock().expect("readied").clone()
    }

    fn hooks(&self) -> Vec<String> {
        self.hooks.lock().expect("hooks").clone()
    }
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

    async fn attach_nics(
        &self,
        machine: &dyn Machine,
    ) -> anyhow::Result<Vec<crate::net::fastpath::NicAttachment>> {
        Ok((0..machine.nics().len())
            .map(|i| crate::net::fastpath::NicAttachment::Stream {
                sock: machine.nic_sock(i),
            })
            .collect())
    }

    async fn before_resume(&self, machine: &str) -> anyhow::Result<()> {
        self.hooks
            .lock()
            .expect("hooks")
            .push(format!("resume:{machine}"));
        match self.refuse_resume.lock().expect("refuse").clone() {
            Some(why) => Err(anyhow::anyhow!(why)),
            None => Ok(()),
        }
    }

    async fn machine_ready(&self, machine: &str) {
        self.readied
            .lock()
            .expect("readied")
            .push(machine.to_string());
        self.hooks
            .lock()
            .expect("hooks")
            .push(format!("ready:{machine}"));
    }

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
    vm_with_floppy(dirs, decl, script, None)
}

/// [`vm`], with `floppy` in drive A.
fn vm_with_floppy(
    dirs: &Dirs,
    decl: &str,
    script: Script,
    floppy: Option<Floppy>,
) -> (Arc<VmInstance>, Arc<FakeHypervisor>) {
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
        floppy,
        share_hosts,
        TemplateParts {
            resolved,
            backing: None,
            disk_size: Some(10 << 30),
            first_boot: None,
            agent_version: Some("0.1.0".into()),
            logins: Vec::new(),
            variant: None,
            template_vars: None,
        },
    );
    let on_vmlab = script.vmlab_hv;
    vm.set_host_config(crate::config::host::HostConfig::parse(
        "import <vmlab-host.wcl>\nhost { apple_smc_osk = \"TESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTEST\" }",
        "<test>",
    ).unwrap());
    let hv = FakeHypervisor::new(script);
    if on_vmlab {
        vm = vm.on_hypervisor(crate::config::hypervisor::HypervisorKind::Vmlab, hv.clone());
        // The fake runs anywhere: a KVM host, whatever runs the test.
        vm.set_host_facts(kvm_host());
    } else {
        vm.set_hypervisor(hv.clone());
    }
    (vm, hv)
}

/// An x86_64 host with a usable `/dev/kvm`, as vmlab-hv's refusals read it.
fn kvm_host() -> crate::config::hypervisor::HostFacts {
    crate::config::hypervisor::HostFacts {
        arch: "x86_64".into(),
        kvm: true,
    }
}

#[test]
fn automatic_hypervisor_selection_follows_a_deferred_templates_hardware() {
    use crate::config::hypervisor::HypervisorKind;
    use crate::net::framing::FramingMode;
    let dirs = Dirs::new();
    let (mut machine, _) = vm(&dirs, LINUX_VM, Script::healthy());
    machine.set_host_facts(kvm_host());
    let machine = machine.with_automatic_hypervisor(Some(Default::default()));
    assert_eq!(machine.hypervisor(), HypervisorKind::Vmlab);
    assert!(machine.takes_tap_fds());
    assert_eq!(
        machine.net_framing(),
        FramingMode::Vnet {
            queue_pairs: crate::net::framing::queue_pairs(machine.template().resolved.cpus),
        }
    );
    let mut parts = (*machine.template()).clone();
    parts.resolved.display_device = Some("qxl-vga".into());
    machine.set_template(parts);
    // vmlab-hv presents no QXL, and QEMU runs no x86 guest: the VM stays on
    // vmlab-hv, and its start refuses the display.
    assert_eq!(machine.hypervisor(), HypervisorKind::Vmlab);
    assert_eq!(machine.hypervisor_fallback(), None);
    let why = crate::config::hypervisor::vmlab_refusals(
        crate::config::model::MachineCfg::Vm(&machine.cfg),
        &kvm_host(),
        Some(&machine.template().resolved),
    );
    assert!(why.iter().any(|w| w.contains("qxl-vga")), "{why:?}");
}

#[tokio::test]
async fn qemu_identity_migrates_once_and_keeps_its_snapshots() {
    use super::state::{LabState, SnapshotRecord};
    use crate::config::hypervisor::{HypervisorKind, SelectionSource};

    let dirs = Dirs::new();
    let (machine, _) = vm(&dirs, LINUX_VM, on_vmlab_hv(Script::healthy()));
    let machine = machine.with_selection_source(SelectionSource::QemuMigration);
    let disk = machine.dirs.primary_disk();
    std::fs::remove_file(&disk).unwrap();
    crate::template::qimg::create_blank(&disk, 1 << 20)
        .await
        .unwrap();
    crate::template::qimg::snapshot_create(&disk, "cold")
        .await
        .unwrap();
    let local = crate::paths::lab_local_dir(&dirs.root);
    let mut state = LabState::default();
    let saved = state.machine_mut(machine.name());
    saved.hypervisor = Some(HypervisorKind::Qemu);
    for (name, online) in [("cold", false), ("warm", true)] {
        saved.snapshots.insert(
            name.into(),
            SnapshotRecord {
                online,
                taken_at: chrono::Utc::now(),
                image_digest: None,
                frozen_in: None,
                freeze_point: None,
                hypervisor: HypervisorKind::Qemu,
            },
        );
    }
    state.save(&local).unwrap();
    let config = crate::config::load_lab_source(
        &format!("import <vmlab.wcl>\nlab \"t\" {{\n{LINUX_VM}\n}}\n"),
        "<test>",
        &dirs.root,
    )
    .unwrap();
    let lab = super::lab::LabRuntime::with_machines(config, vec![machine.clone()]).unwrap();
    let (tx, mut lines) = mpsc::unbounded_channel();
    let output: crate::scripting::OutputSink = Arc::new(move |line| tx.send(line).unwrap());
    let name = machine.name();
    lab.start_machine_with_output(name, Some(&output))
        .await
        .unwrap();
    assert_eq!(
        lines.try_recv().unwrap(),
        "info: dc01: migrated from QEMU to vmlab-hv\n"
    );
    assert_eq!(
        LabState::load(&local).unwrap().machines[name].hypervisor,
        Some(HypervisorKind::Vmlab)
    );
    let listed = lab.snapshots(name).await.unwrap();
    assert!(listed[0]["unrestorable"].is_null(), "{listed}");
    assert_eq!(listed[1]["name"], "warm");
    let why = listed[1]["unrestorable"].as_str().unwrap();
    assert!(
        why.contains("\"warm\"") && why.contains("convert it to offline"),
        "{why}"
    );
    let error = lab.restore(name, "warm", false).await.unwrap_err();
    assert_eq!(error.to_string(), why);
    lab.stop_machine(name, true).await.unwrap();
    machine
        .wait_state(PowerState::Stopped, SETTLE)
        .await
        .unwrap();
    lab.restore(name, "cold", false).await.unwrap();
    assert_eq!(
        LabState::load(&local).unwrap().machines[name].snapshots,
        state.machines[name].snapshots
    );
    lab.start_machine_with_output(name, Some(&output))
        .await
        .unwrap();
    assert!(lines.try_recv().is_err());
    lab.stop_machine(name, true).await.unwrap();
}

#[tokio::test]
async fn refused_qemu_migration_keeps_the_disk_identity() {
    use super::state::LabState;
    use crate::config::hypervisor::{HypervisorKind, SelectionSource};

    let dirs = Dirs::new();
    let (machine, _) = vm(&dirs, LINUX_VM, on_vmlab_hv(Script::healthy()));
    let machine = machine.with_selection_source(SelectionSource::QemuMigration);
    let mut parts = (*machine.template()).clone();
    parts.resolved.display_device = Some("qxl-vga".into());
    machine.set_template(parts);
    let local = crate::paths::lab_local_dir(&dirs.root);
    let mut state = LabState::default();
    let name = machine.name();
    state.machine_mut(name).hypervisor = Some(HypervisorKind::Qemu);
    state.save(&local).unwrap();
    let config = crate::config::load_lab_source(
        &format!("import <vmlab.wcl>\nlab \"t\" {{\n{LINUX_VM}\n}}\n"),
        "<test>",
        &dirs.root,
    )
    .unwrap();
    let lab = super::lab::LabRuntime::with_machines(config, vec![machine.clone()]).unwrap();
    let error = lab.start_machine(name).await.unwrap_err().to_string();
    assert!(error.contains("qxl-vga"), "{error}");
    assert_eq!(machine.state().await, PowerState::Stopped);
    assert_eq!(
        LabState::load(&local).unwrap().machines[name].hypervisor,
        Some(HypervisorKind::Qemu)
    );
}

/// A legacy pc VM on vmlab-hv gets its e1000 or PCnet on a plain stream:
/// vmlab-hv's emulated pc NICs take no TAP, and the same VM on QEMU does.
#[test]
fn a_pc_nic_on_vmlab_hv_takes_a_stream_not_a_tap() {
    use crate::config::hypervisor::HypervisorKind;
    use crate::net::framing::FramingMode;
    let dirs = Dirs::new();
    for (hypervisor, kind, tap) in [
        ("vmlab", HypervisorKind::Vmlab, false),
        ("qemu", HypervisorKind::Qemu, true),
    ] {
        let (mut machine, _) = vm(
            &dirs,
            &format!(
                r#"vm "w7" {{ template = "scratch" arch = "x86_64" profile = "windows-legacy" disk = 10GiB hypervisor = "{hypervisor}" }}"#
            ),
            Script::healthy(),
        );
        machine.set_host_facts(kvm_host());
        // Selection reads the declaration only on the automatic path.
        let machine = machine.with_automatic_hypervisor(Some(Default::default()));
        assert_eq!(machine.template().resolved.nic_model, "e1000");
        assert_eq!(machine.hypervisor(), kind);
        assert_eq!(machine.takes_tap_fds(), tap, "{hypervisor}");
        assert_eq!(machine.net_framing(), FramingMode::Raw, "{hypervisor}");
    }
}

/// A container on a fake hypervisor, image already bound. `decl` is the
/// `container "…" { … }` block.
fn container(
    dirs: &Dirs,
    decl: &str,
    script: Script,
) -> (Arc<ContainerInstance>, Arc<FakeHypervisor>) {
    container_with_volumes(dirs, decl, script, Vec::new())
}

/// [`container`] with resolved volumes: (share name, host dir, read-only).
fn container_with_volumes(
    dirs: &Dirs,
    decl: &str,
    script: Script,
    volumes: Vec<(String, PathBuf, bool)>,
) -> (Arc<ContainerInstance>, Arc<FakeHypervisor>) {
    container_on_image(dirs, decl, script, volumes, "sha256:00")
}

/// [`container_with_volumes`] bound to the image with this manifest digest.
/// A scratch overlay `dirs` already holds is kept as it is, so a second
/// call over the same `dirs` is the machine rebuilt.
fn container_on_image(
    dirs: &Dirs,
    decl: &str,
    script: Script,
    volumes: Vec<(String, PathBuf, bool)>,
    digest: &str,
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
    if !ctr_dirs.scratch_disk().exists() {
        std::fs::write(ctr_dirs.scratch_disk(), b"").expect("scratch");
    }

    let image = PulledImage {
        manifest_digest: digest.into(),
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
    // A script that speaks vmlab-hv's control protocol is a machine on it.
    let kind = if script.vmlab_hv {
        crate::config::hypervisor::HypervisorKind::Vmlab
    } else {
        crate::config::hypervisor::HypervisorKind::Qemu
    };
    let hv = FakeHypervisor::new(script);
    let mut ctr = ContainerInstance::new(
        "t",
        cfg,
        resolved,
        ctr_dirs,
        Vec::new(),
        Vec::new(),
        Some(image),
        volumes,
        kind,
        hv.clone(),
    );
    ctr.set_host_facts(kvm_host());
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
        std::future::ready(()),
        std::future::ready(Ok(())),
        |_| {},
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

/// `eject_cdrom` on a machine with no install CD-ROM — a VM declaring no
/// `cdrom`, or any container — refuses by naming the missing drive, before
/// asking the emulator anything (neither is running here).
#[tokio::test]
async fn ejecting_a_cdrom_that_is_not_there_says_so() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let (ctr, _hv) = container(&dirs, WORKLOAD, Script::healthy());
    for m in [vm as Arc<dyn Machine>, ctr as Arc<dyn Machine>] {
        let err = m.eject_cdrom().await.expect_err("no install CD-ROM");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("machine `{}` has no install CD-ROM", m.name())),
            "{msg}"
        );
    }
}

#[tokio::test]
async fn deferred_template_logins_reach_selectors_and_status_without_a_default() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB }"#,
        Script::healthy(),
    );
    let m: Arc<dyn Machine> = vm.clone();
    assert!(m.logins().is_empty());
    let mut template = crate::config::load_template_source(
        r#"import <vmlab.wcl>
template "t" { arch = "x86_64" version = "1" source "scratch" {}
login "stock" { user = "dev" password = "never-in-status" default = true }
}"#,
        "<test>",
        std::path::Path::new("/tmp"),
    )
    .unwrap();
    let parts = vm.template();
    vm.set_template(TemplateParts {
        resolved: parts.resolved.clone(),
        backing: parts.backing.clone(),
        disk_size: parts.disk_size,
        first_boot: parts.first_boot.clone(),
        agent_version: parts.agent_version.clone(),
        logins: template.templates.remove(0).logins,
        variant: parts.variant.clone(),
        template_vars: parts.template_vars.clone(),
    });
    let logins = m.logins();
    assert!(crate::config::model::default_login(&logins).is_none());
    assert!(
        super::identity::resolve(m.name(), &logins, m.guest_os(), None, None)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        super::identity::resolve(m.name(), &logins, m.guest_os(), Some("stock"), None)
            .unwrap()
            .unwrap()
            .user,
        "dev"
    );
    let status = serde_json::to_value(m.status().await).unwrap();
    assert_eq!(status["logins"][0]["label"], "stock");
    assert_eq!(status["logins"][0]["origin"], "template");
    assert_eq!(status["logins"][0]["default"], false);
    assert!(!status.to_string().contains("never-in-status"));
    assert!(status["logins"][0].get("password").is_none());
}

/// A VM reports the hardware it resolved to (VM > template > profile), not
/// what it declared: one that leaves its arch or size to its template must
/// not show a dash for each.
#[tokio::test]
async fn a_vms_status_reports_its_resolved_hardware() {
    let dirs = Dirs::new();
    // Declares no cpus or memory; the arch is then taken from the template,
    // which is what a registry template's metadata does.
    let (vm, _hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB }"#,
        Script::healthy(),
    );
    let parts = vm.template();
    let mut resolved = parts.resolved.clone();
    resolved.arch = "aarch64".into();
    vm.set_template(TemplateParts {
        resolved,
        backing: parts.backing.clone(),
        disk_size: parts.disk_size,
        first_boot: None,
        agent_version: parts.agent_version.clone(),
        logins: parts.logins.clone(),
        variant: None,
        template_vars: None,
    });

    let m: Arc<dyn Machine> = vm.clone();
    let crate::status::MachineDetail::Vm(detail) = m.status_detail().await else {
        panic!("a VM reports VM detail");
    };
    let resolved = &vm.template().resolved;
    assert_eq!(detail.arch.as_deref(), Some("aarch64"));
    assert_eq!(detail.cpus, Some(resolved.cpus));
    assert_eq!(detail.memory, Some(resolved.memory));
}

/// Review finding F3 (docs/reviews/firmware.md): QEMU's `virt` machine has no
/// secure world, so an aarch64 guest's kernel can program the flash that
/// holds PK, KEK, db and dbx. `secure_boot = true` there must say so before
/// the VM boots, under either key store, and stay silent for a VM without
/// secure boot and for an x86_64 one, whose store is written from SMM only.
#[tokio::test]
async fn aarch64_secure_boot_says_its_key_store_is_guest_writable() {
    let warnings = |arch: &str, extra: &str| {
        let dirs = Dirs::new();
        let (vm, _hv) = vm(
            &dirs,
            &format!(
                "vm \"sb\" {{ template = \"scratch\" arch = \"{arch}\" profile = \"linux-modern\" \
                 disk = 1GiB firmware = \"ovmf\" {extra} }}"
            ),
            Script::healthy(),
        );
        let m: Arc<dyn Machine> = vm;
        m.launch_warnings()
    };
    let about_the_store = |lines: &[String]| -> Vec<String> {
        lines
            .iter()
            .filter(|line| line.contains("secure_boot = true"))
            .cloned()
            .collect()
    };

    for extra in [
        "secure_boot = true",
        "secure_boot = true secure_boot_keys = \"setup\"",
    ] {
        let said = about_the_store(&warnings("aarch64", extra));
        assert_eq!(said.len(), 1, "{extra}: {said:?}");
        let line = &said[0];
        assert!(line.starts_with("vm \"sb\": secure_boot = true"), "{line}");
        assert!(line.contains("the guest's own kernel can write"), "{line}");
    }
    assert_eq!(
        about_the_store(&warnings("aarch64", "secure_boot = false")),
        Vec::<String>::new()
    );
    assert_eq!(
        about_the_store(&warnings("x86_64", "secure_boot = true")),
        Vec::<String>::new()
    );
}

/// Host USB passthrough's machine-side refusals (§5.2.1): a container
/// never; a VM only while running, and only when it booted with the
/// controller; a stop forgets nothing it did not hold.
#[tokio::test]
async fn usb_attaches_only_to_a_running_vm_with_the_controller() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(&dirs, WORKLOAD, Script::healthy());
    let e = format!("{:#}", ctr.usb_ready().await.unwrap_err());
    assert_eq!(e, "\"web\" is a container — USB passthrough is for VMs");

    let (plain, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let e = format!("{:#}", plain.usb_ready().await.unwrap_err());
    assert!(e.contains("is not running"), "{e}");
    let (cbs, _observed) = callbacks();
    start_vm(&plain, cbs).await.expect("start");
    let e = format!("{:#}", plain.usb_ready().await.unwrap_err());
    assert_eq!(
        e,
        "\"dc01\" has no USB controller to attach to — set `usb = true` on dc01 and restart it"
    );
    plain.stop(true).await.expect("stop");

    let (usb, _hv) = vm(
        &dirs,
        r#"vm "dc02" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB usb = true }"#,
        Script::healthy(),
    );
    let (cbs, _observed) = callbacks();
    start_vm(&usb, cbs).await.expect("start");
    usb.usb_ready().await.expect("running with a controller");
    assert!(usb.usb_attached().await.is_empty());
    usb.stop(true).await.expect("stop");
}

// ---- the VM start ladder ----------------------------------------------------

#[tokio::test]
async fn machines_choose_vnet_framing_only_for_vmlab_virtio_nics() {
    use crate::net::framing::FramingMode;
    let dirs = Dirs::new();
    let (qemu, _) = vm(&dirs, LINUX_VM, Script::healthy());
    assert_eq!(qemu.net_framing(), FramingMode::Raw);
    let mut script = Script::healthy();
    script.vmlab_hv = true;
    let (native, _) = vm(&dirs, LINUX_VM, script.clone());
    assert!(native.takes_tap_fds());
    let count = crate::net::framing::queue_pairs(native.template().resolved.cpus);
    assert_eq!(
        native.net_framing(),
        FramingMode::Vnet { queue_pairs: count }
    );
    let (container, _) = container(
        &dirs,
        r#"container "web" { image = "alpine" cpus = 3 memory = 256MiB }"#,
        script,
    );
    assert_eq!(
        container.net_framing(),
        FramingMode::Vnet { queue_pairs: 3 }
    );
    assert!(!container.takes_tap_fds());
}

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

/// F1 (docs/reviews/appshare-media.md): a floppy built from a `media {}`
/// folder is an entry of the media cache, which every machine with that folder
/// is handed. vmlab-hv opens drive A writable and takes no image lock, so no
/// two machines may launch with one path, and none with the cache entry: what
/// one guest writes must reach neither the cache nor the other guest.
#[tokio::test]
async fn two_machines_do_not_share_a_writable_floppy() {
    let cache = tempfile::tempdir().expect("cache dir");
    let entry = cache.path().join("0123456789abcdef.img");
    std::fs::write(&entry, b"as built").expect("cache entry");

    let mut launched = Vec::new();
    for _ in 0..2 {
        let dirs = Dirs::new();
        let (vm, hv) = vm_with_floppy(
            &dirs,
            LINUX_VM,
            Script::healthy(),
            Some(Floppy::Built(entry.clone())),
        );
        let (cbs, _observed) = callbacks();
        start_vm(&vm, cbs).await.expect("start");
        let floppy = hv
            .launched_floppies()
            .pop()
            .flatten()
            .expect("the launch carries the floppy");
        assert_eq!(
            std::fs::read(&floppy).expect("launched floppy"),
            b"as built",
            "the guest reads what was built"
        );
        // What a guest write to drive A does to the image it was given.
        std::fs::write(&floppy, b"written by a guest").expect("guest write");
        vm.stop(true).await.expect("stop");
        launched.push((dirs, floppy));
    }

    assert_ne!(
        launched[0].1, entry,
        "a machine launched with the cache entry"
    );
    assert_ne!(
        launched[1].1, entry,
        "a machine launched with the cache entry"
    );
    assert_ne!(
        launched[0].1, launched[1].1,
        "two machines launched with one writable floppy image"
    );
    assert_eq!(
        std::fs::read(&entry).expect("cache entry"),
        b"as built",
        "a guest wrote into the shared media cache"
    );
}

/// F4 (docs/reviews/config.md): a disk's image is `disk-<name>.qcow2`, so
/// two `disk` blocks with one name are one file. A configuration that reaches
/// the lab daemon that way must never launch with that file as two drives:
/// vmlab-hv takes no image lock, and writes through both corrupt it.
#[tokio::test]
async fn two_disks_with_one_name_never_launch_as_two_drives() {
    let dirs = Dirs::new();
    let decl = r#"vm "dc01" {
        template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB
        disk "data" { size = 1GiB }
        disk "data" { size = 1GiB }
    }"#;
    let (vm, hv) = vm(&dirs, decl, Script::healthy());
    // As for the primary disk: present, so nothing shells out to qemu-img.
    std::fs::write(dirs.root.join("local").join("disk-data.qcow2"), b"").expect("disk-data");
    let (cbs, _observed) = callbacks();

    let started = start_vm(&vm, cbs).await;

    for disks in hv.launched_disks() {
        let mut seen = std::collections::HashSet::new();
        for d in &disks {
            assert!(
                seen.insert(d),
                "one image launched as two drives: {disks:?}"
            );
        }
    }
    let err = started.expect_err("a VM with one disk name twice must not start");
    assert!(
        format!("{err:#}").contains("disk \"data\" is declared twice"),
        "the refusal names the duplicate: {err:#}"
    );
}

/// Readiness is when the lab installs whatever is keyed on the machine's
/// lease — a segment `forward {}` aimed at it above all (PRD §9.8). The VM
/// must tell the lab exactly as a container does, or its forwards stay
/// skipped from `up` until the next one.
#[tokio::test]
async fn a_vm_that_becomes_ready_tells_the_lab() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let (events, _rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events));

    vm.clone()
        .start(lab.clone() as Arc<dyn LabServices>, None)
        .await
        .expect("start");
    vm.wait_ready(SETTLE).await.expect("ready");
    tokio::time::timeout(SETTLE, async {
        while lab.readied().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the lab was never told the VM is ready");
    assert_eq!(lab.readied(), ["dc01"]);

    vm.stop(true).await.expect("stop");
}

/// An agent that restarts inside a running guest answers pings on the
/// channel the host already holds. The host must notice it is a different
/// agent and handshake again, or every surface goes on reporting the version
/// and features of one that is gone.
#[tokio::test]
async fn an_agent_restarted_in_the_guest_is_handshaken_again() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run {
                agent_restarts: Some((Duration::from_secs(1), "0.0.0-fake-2".into())),
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();
    m.wait_ready(SETTLE).await.expect("ready");
    let before = m.agent().await.expect("agent").info();
    assert_eq!(before.agent_version, "0.0.0-fake");

    let after = tokio::time::timeout(SETTLE, async {
        loop {
            let info = m.agent().await.expect("agent").info();
            if info.instance != before.instance {
                return info;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the restarted agent was never handshaken");
    assert_eq!(after.agent_version, "0.0.0-fake-2");
    let crate::status::MachineDetail::Vm(detail) = m.status_detail().await else {
        panic!("a VM reports VM detail");
    };
    assert_eq!(detail.agent_version.as_deref(), Some("0.0.0-fake-2"));

    vm.stop(true).await.expect("stop");
}

/// **An agent call made while the agent is being replaced waits, and lands
/// on the agent that replaced it** (§19.4). The push restarts the agent and
/// shuts its own connection down; before this, `vmlab exec` issued right
/// after `up` — whose refresh of an un-waited machine runs on after `up`
/// returns — was handed that very connection and died with it, as "agent
/// channel closed during exec", or was killed in the restart (137), or
/// handshook with the agent on its way out and was cut off when the new
/// one answered its ping as somebody else.
///
/// The fake's agent restarts a second in, as a new instance with a new
/// version. The gate is held across that: a caller waiting on it is still
/// waiting once the restart has happened, the machine's agent is "not
/// answering" to a probe meanwhile, the replacement's own reconnect reaches
/// the new instance, and the waiter — released only then — gets a handle
/// that handshook with the new instance, never the old one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_call_during_a_replacement_waits_for_the_agent_that_replaced_it() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run {
                agent_restarts: Some((Duration::from_secs(1), "0.0.0-fake-2".into())),
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();
    m.wait_ready(SETTLE).await.expect("ready");
    let before = m.agent().await.expect("agent").info();
    assert_eq!(before.agent_version, "0.0.0-fake");

    // The push takes the gate while the old agent is still the one answering.
    let replacing = m.begin_agent_replacement().await;
    let waiter = tokio::spawn({
        let m = Arc::clone(&m);
        async move {
            m.agent()
                .await
                .expect("the agent after the replacement")
                .info()
        }
    });
    // Past the scripted restart: the new instance is up, and the waiter is
    // still held, because the replacement has not said it is done.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!waiter.is_finished(), "held until the replacement is done");
    assert!(
        !m.agent_answering().await,
        "not answering anybody while it is being replaced"
    );

    // The replacement's own reconnect goes through the gate it holds, and
    // finds the new instance.
    let came_back = m
        .wait_agent_during_replacement(SETTLE)
        .await
        .expect("the replacement reaches the new agent")
        .info();
    assert_ne!(
        came_back.instance, before.instance,
        "a new process answered"
    );
    assert_eq!(came_back.agent_version, "0.0.0-fake-2");
    assert!(
        !waiter.is_finished(),
        "still held: the reconnect is not the release"
    );

    drop(replacing);
    let after = tokio::time::timeout(SETTLE, waiter)
        .await
        .expect("released with the replacement")
        .expect("the waiter's task");
    assert_eq!(
        after.instance, came_back.instance,
        "the waiter got the new agent"
    );
    assert_eq!(after.agent_version, "0.0.0-fake-2");
    assert!(m.agent_answering().await, "answering again once released");

    vm.stop(true).await.expect("stop");
}

/// **A reconnect that lands in the restart's gap is retried, not believed**
/// (§19.4). A Windows service bounce leaves seconds in which nothing
/// answers; a handshake attempted then fails. That failure must not stand
/// for the rest of the wait: the failed-handshake backoff that spares an
/// agent-less guest repeated timeouts would otherwise answer every retry
/// from its cache, and the push would report an agent that came back as
/// one that never did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_inside_the_restart_gap_does_not_end_the_wait() {
    let dirs = Dirs::new();
    let (vm1, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run {
                agent_restarts: Some((Duration::from_secs(4), "0.0.0-fake-2".into())),
                agent_restart_gap: Some(Duration::from_secs(8)),
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();
    start_vm(&vm1, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm1.clone();
    m.wait_ready(SETTLE).await.expect("ready");
    let before = m.agent().await.expect("agent").info();
    let plan = super::agent_repair::plan(super::guest_os::GuestOs::Linux);

    // Held as a real push holds it: the status poller's probe stands down
    // while it is taken, so nothing else clears a failed handshake for the
    // push.
    let replacing = m.begin_agent_replacement().await;
    let after = super::agent_repair::came_back(
        &m,
        "0.0.0-fake-2",
        before.instance.as_deref(),
        &plan,
        Duration::from_secs(30),
    )
    .await
    .expect("the agent that came back after the gap answers within the wait")
    .info();
    assert_ne!(after.instance, before.instance);
    assert_eq!(after.agent_version, "0.0.0-fake-2");
    drop(replacing);
    vm1.stop(true).await.expect("stop");
}

/// **A reconnect answered by the agent told to restart is a restart not yet
/// taken effect, and is waited for** (§19.4). On Windows the service bounce
/// lands seconds after the restart command returned, so the push's first
/// reconnect was routinely the old agent's: the refresh was reported failed
/// ("the agent answering is still agent=f1e4515 — the service did not
/// restart onto it"), the machine went ready, and the bounce then landed
/// under the first `exec` after `up`.
///
/// The fake's agent restarts a second in, as a new instance with a new
/// version: the push's wait outlasts that and lands on the new instance. A
/// guest whose agent never restarts is reported, by name, only once the
/// wait is up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_push_waits_for_a_restart_that_has_not_taken_effect_yet() {
    let dirs = Dirs::new();
    let (vm1, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run {
                agent_restarts: Some((Duration::from_secs(1), "0.0.0-fake-2".into())),
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();
    start_vm(&vm1, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm1.clone();
    m.wait_ready(SETTLE).await.expect("ready");
    let before = m.agent().await.expect("agent").info();
    let plan = super::agent_repair::plan(super::guest_os::GuestOs::Linux);

    // Asked straight away, the agent answering is still the one that was
    // told to restart — and the wait outlasts the restart.
    let after = super::agent_repair::came_back(
        &m,
        "0.0.0-fake-2",
        before.instance.as_deref(),
        &plan,
        Duration::from_secs(10),
    )
    .await
    .expect("the restarted agent answers within the wait")
    .info();
    assert_ne!(after.instance, before.instance);
    assert_eq!(after.agent_version, "0.0.0-fake-2");
    vm1.stop(true).await.expect("stop");

    // No restart ever: reported once the wait is up, naming what answered.
    let dirs = Dirs::new();
    let (vm2, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let (cbs, _observed) = callbacks();
    start_vm(&vm2, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm2.clone();
    m.wait_ready(SETTLE).await.expect("ready");
    let before = m.agent().await.expect("agent").info();
    let started = tokio::time::Instant::now();
    let err = match super::agent_repair::came_back(
        &m,
        "agent=new",
        before.instance.as_deref(),
        &plan,
        Duration::from_secs(3),
    )
    .await
    {
        Ok(_) => panic!("the old agent is not the pushed one"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "waited the wait out"
    );
    assert!(err.contains("still 0.0.0-fake"), "{err}");
    assert!(err.contains("did not restart onto it within 3s"), "{err}");
    assert!(err.contains("vmlab vm restart dc01"), "{err}");
    vm2.stop(true).await.expect("stop");
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

/// A clone made from one template is never booted under a declaration that
/// names another: the boot refuses, says how to recreate the disk, and
/// leaves both the disk and the record of what it was cloned from alone.
#[tokio::test]
async fn a_clone_of_another_template_refuses_to_boot() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    std::fs::write(vm.dirs.clone_source(), "x86_64/alpine-3.23\n").expect("record");
    let (cbs, _observed) = callbacks();

    let err = start_vm(&vm, cbs).await.expect_err("start must refuse");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("x86_64/alpine-3.23"),
        "names the old template: {msg}"
    );
    assert!(msg.contains("\"scratch\""), "names the declared one: {msg}");
    assert!(
        msg.contains("vmlab vm destroy dc01"),
        "says the way out: {msg}"
    );
    assert_eq!(vm.state().await, PowerState::Stopped);
    assert!(vm.dirs.primary_disk().exists(), "the guest's disk is kept");
    assert_eq!(
        std::fs::read_to_string(vm.dirs.clone_source()).expect("record"),
        "x86_64/alpine-3.23\n"
    );
}

/// A container's writable layer is never booted over another image than the
/// one it was made on (§18, docs/reviews/containers.md F3): after the
/// `image =` line and the pin change, the start refuses, names both images
/// and the way out, and leaves the layer alone.
#[tokio::test]
async fn a_changed_image_does_not_reuse_the_old_scratch_layer() {
    let dirs = Dirs::new();
    let (old, _hv) =
        container_on_image(&dirs, WORKLOAD, Script::healthy(), Vec::new(), "sha256:aa");
    let (cbs, _observed) = callbacks();
    start_container(&old, cbs)
        .await
        .expect("start on the old image");
    old.stop(true).await.expect("stop");
    let scratch = old.dirs.scratch_disk();
    let marker = b"written while nginx:1 ran";
    std::fs::write(&scratch, marker).expect("marker");
    drop(old);

    let (new, hv) = container_on_image(
        &dirs,
        r#"container "web" { image = "nginx:2" profile = "container" }"#,
        Script::healthy(),
        Vec::new(),
        "sha256:bb",
    );
    let (cbs, _observed) = callbacks();
    let err = start_container(&new, cbs)
        .await
        .expect_err("the old layer must not boot over the new image");
    let msg = format!("{err:#}");
    assert!(msg.contains("sha256:aa"), "names the old image: {msg}");
    assert!(msg.contains("sha256:bb"), "names the new image: {msg}");
    assert!(msg.contains("nginx:2"), "names the declared image: {msg}");
    assert!(
        msg.contains("vmlab container destroy web"),
        "says the way out: {msg}"
    );
    assert_eq!(new.state().await, PowerState::Stopped);
    assert_eq!(hv.starts(), 0, "nothing was launched");
    assert_eq!(
        std::fs::read(&scratch).expect("scratch"),
        marker,
        "the layer is kept"
    );
    assert_eq!(
        std::fs::read_to_string(new.dirs.scratch_image()).expect("record"),
        "sha256:aa",
        "and so is the record of its image"
    );
}

/// The same image keeps its writable layer across a rebuild of the machine,
/// and a layer made before its image was recorded is taken to be of the
/// image pinned now.
#[tokio::test]
async fn an_unchanged_image_keeps_its_scratch_layer() {
    let dirs = Dirs::new();
    for _ in 0..2 {
        let (ctr, _hv) =
            container_on_image(&dirs, WORKLOAD, Script::healthy(), Vec::new(), "sha256:aa");
        let (cbs, _observed) = callbacks();
        start_container(&ctr, cbs).await.expect("start");
        ctr.stop(true).await.expect("stop");
        assert_eq!(
            std::fs::read_to_string(ctr.dirs.scratch_image()).expect("record"),
            "sha256:aa"
        );
    }
}

/// A clone made before its source was recorded boots, and is taken to be of
/// the template declared now — which is then what it is checked against.
#[tokio::test]
async fn a_clone_with_no_recorded_source_boots_and_is_recorded() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    assert!(!vm.dirs.clone_source().exists());
    let (cbs, _observed) = callbacks();

    start_vm(&vm, cbs).await.expect("start");
    assert_eq!(
        std::fs::read_to_string(vm.dirs.clone_source()).expect("record"),
        "scratch"
    );
    vm.stop(true).await.expect("stop");
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

/// A virtiofsd older than `--migration-mode` (Ubuntu 24.04 ships 1.10.0)
/// still serves a share: the VM starts, and only an online snapshot — which
/// would need the daemon's state — is refused, naming the release that has
/// it. The same VM on a current virtiofsd gets past the check to QMP, which
/// the fake does not answer.
#[tokio::test]
async fn an_old_virtiofsd_starts_the_vm_and_refuses_online_snapshots() {
    const SHARED: &str = r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB
             share "data" { host = "./data" guest = "/mnt/data" transport = "virtiofs" } }"#;
    for (old, refused) in [(true, true), (false, false)] {
        let dirs = Dirs::new();
        let (vm, _hv) = vm(
            &dirs,
            SHARED,
            Script {
                old_virtiofsd: old,
                ..Script::healthy()
            },
        );
        let (cbs, _observed) = callbacks();
        start_vm(&vm, cbs)
            .await
            .expect("an old virtiofsd still starts");

        let m: Arc<dyn Machine> = vm.clone();
        let err = format!("{:#}", m.snapshot("s1").await.expect_err("no QMP"));
        assert_eq!(
            err.contains("virtiofsd 1.11.0 or later"),
            refused,
            "old={old}: {err}"
        );
        if refused {
            assert!(
                err.contains("virtiofsd 1.10.0"),
                "names what it found: {err}"
            );
        }
        vm.stop(true).await.expect("stop");
    }
}

/// An online restore is refused before anything moves — a stopped VM is not
/// booted only to have the load fail after its disks were rewound.
#[tokio::test]
async fn an_old_virtiofsd_refuses_an_online_restore_without_booting() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB
             share "data" { host = "./data" guest = "/mnt/data" transport = "virtiofs" } }"#,
        Script {
            old_virtiofsd: true,
            ..Script::healthy()
        },
    );
    let (events, _rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events)) as Arc<dyn LabServices>;
    let m: Arc<dyn Machine> = vm.clone();
    let err = m.restore(lab, "s1", true).await.expect_err("refused");
    assert!(format!("{err:#}").contains("virtiofsd 1.11.0"), "{err:#}");
    assert_eq!(vm.state().await, PowerState::Stopped, "never booted");
    assert!(hv.live_helpers().await.is_empty());
}

/// A read-only share on a virtiofsd with no `--readonly` fails the start
/// naming the release that added it, instead of exporting it writable.
#[tokio::test]
async fn a_readonly_share_on_a_virtiofsd_without_readonly_fails_the_start() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB
             share "data" { host = "./data" guest = "/mnt/data" transport = "virtiofs" readonly = true } }"#,
        Script {
            old_virtiofsd: true,
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();
    let err = format!("{:#}", start_vm(&vm, cbs).await.expect_err("refused"));
    assert!(err.contains("--readonly"), "{err}");
    assert!(err.contains("virtiofsd 1.13.0"), "{err}");
    assert_eq!(vm.state().await, PowerState::Stopped);
    assert!(hv.live_helpers().await.is_empty());
}

/// A container's virtiofs volumes are held to the same rule as a VM's shares.
#[tokio::test]
async fn an_old_virtiofsd_refuses_a_containers_online_snapshot() {
    let dirs = Dirs::new();
    let data = dirs.root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let (ctr, _hv) = container_with_volumes(
        &dirs,
        WORKLOAD,
        Script {
            old_virtiofsd: true,
            ..Script::healthy()
        },
        vec![("data".into(), data, false)],
    );
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");

    let m: Arc<dyn Machine> = ctr.clone();
    let err = m.snapshot("s1").await.expect_err("refused");
    assert!(format!("{err:#}").contains("virtiofsd 1.11.0"), "{err:#}");
    ctr.stop(true).await.expect("stop");
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
        std::future::ready(()),
        std::future::ready(Ok(())),
        |_| {},
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

// ---- a guest that sleeps (ACPI S3) ------------------------------------------

/// The VM block the sleep tests boot: the plain Linux VM, keeping itself
/// awake or not.
fn sleepy_vm(prevent_sleep: bool) -> String {
    format!(
        r#"vm "dc01" {{ template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB prevent_sleep = {prevent_sleep} }}"#
    )
}

/// A guest that suspends to RAM leaves QEMU alive and nothing answering. It
/// is reported as `suspended` — not stopped, not running — with an event to
/// say so; anything that needs its agent is refused by name, with the way
/// out; and a start wakes it rather than booting a second emulator.
#[tokio::test]
async fn a_guest_that_sleeps_is_suspended_until_a_start_wakes_it() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        &sleepy_vm(false),
        Script {
            runs: vec![Run::guest_sleeps(Duration::from_millis(300))],
            ..Script::healthy()
        },
    );
    let (events, mut rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events)) as Arc<dyn LabServices>;
    let m: Arc<dyn Machine> = vm.clone();

    m.clone().start(lab.clone(), None).await.expect("start");
    m.wait_ready(SETTLE).await.expect("ready");
    m.wait_state(PowerState::Suspended, SETTLE)
        .await
        .expect("the guest went to sleep");

    let slept = collect_events(&mut rx, &["vm.suspended"], 1).await;
    assert_eq!(slept[0].data["vm"], "dc01");
    assert_eq!(slept[0].data["prevent_sleep"], false);

    let status = m.status().await;
    assert_eq!(status.state, PowerState::Suspended);
    assert_eq!(status.label.text, "suspended");

    // Refused at once, by name, with the remedy — not a handshake timeout.
    let err = m
        .agent()
        .await
        .err()
        .expect("a sleeping guest has no agent");
    let coded = crate::proto::CommandError::from(err);
    assert_eq!(coded.code, crate::proto::ErrorCode::Conflict);
    assert!(coded.message.contains("suspended"), "{}", coded.message);
    assert!(
        coded.message.contains("vmlab vm start dc01"),
        "{}",
        coded.message
    );
    assert!(coded.message.contains("prevent_sleep"), "{}", coded.message);
    // And never retried: waiting does not wake a guest.
    let started = tokio::time::Instant::now();
    assert!(m.wait_agent(Duration::from_secs(60)).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(5));

    // A start wakes it — the same emulator, so nothing reports a stop.
    m.clone().start(lab.clone(), None).await.expect("wake");
    assert_eq!(m.state().await, PowerState::Running);
    let woke = collect_events(&mut rx, &["vm.woken", "vm.stopped", "vm.starting"], 1).await;
    assert_eq!(woke[0].event, "vm.woken");
    assert_eq!(woke[0].data["cause"], "start");
    m.agent()
        .await
        .expect("the agent answers once the guest is awake");

    vm.stop(true).await.expect("stop");
}

/// `prevent_sleep` wakes the guest the moment it suspends, and says so: the
/// machine is back to running on its own and the event log records that the
/// guest tried to sleep and was woken.
#[tokio::test]
async fn prevent_sleep_wakes_the_guest_and_records_it() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        &sleepy_vm(true),
        Script {
            runs: vec![Run::guest_sleeps(Duration::from_millis(300))],
            ..Script::healthy()
        },
    );
    let (events, mut rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events)) as Arc<dyn LabServices>;
    let m: Arc<dyn Machine> = vm.clone();

    m.clone().start(lab, None).await.expect("start");
    let seen = collect_events(&mut rx, &["vm.suspended", "vm.woken"], 2).await;
    assert_eq!(seen[0].event, "vm.suspended");
    assert_eq!(seen[0].data["prevent_sleep"], true);
    assert_eq!(seen[1].event, "vm.woken");
    assert_eq!(seen[1].data["cause"], "prevent_sleep");
    assert!(
        seen[1].data["message"]
            .as_str()
            .is_some_and(|m| m.contains("tried to sleep")),
        "{:?}",
        seen[1].data
    );
    m.wait_state(PowerState::Running, SETTLE)
        .await
        .expect("awake again");
    m.agent().await.expect("the agent answers");

    vm.stop(true).await.expect("stop");
}

/// A graceful stop of a sleeping guest wakes it first, so the agent rung can
/// ask it to shut down: it ends cleanly rather than at the kill.
#[tokio::test]
async fn a_graceful_stop_wakes_a_sleeping_guest_first() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        &sleepy_vm(false),
        Script {
            runs: vec![Run::guest_sleeps(Duration::from_millis(300))],
            ..Script::healthy()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    vm.wait_agent_up(SETTLE).await.expect("agent up");
    vm.wait_state(PowerState::Suspended, SETTLE)
        .await
        .expect("asleep");

    vm.stop(false).await.expect("graceful stop");

    let exit = observed.exit().await;
    assert_eq!(exit.reason, StopReason::Requested);
    assert_eq!(exit.status, "exit status: 0", "shut down, not killed");
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
                ignores_quit: true,
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

/// A guest that ignores ACPI — Windows 9x and DOS halt at "It's now safe to
/// turn off your computer" — is ended by the emulator quitting, not by a kill:
/// QEMU flushes its block caches on `quit`, where a SIGKILL would drop the
/// qcow2 metadata it holds and leave the disk reading as empty.
#[tokio::test(start_paused = true)]
async fn a_guest_that_ignores_acpi_ends_at_a_clean_quit() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
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

    vm.stop(false).await.expect("graceful stop");

    let exit = observed.exit().await;
    assert_eq!(exit.reason, StopReason::Requested);
    assert_eq!(exit.status, "exit status: 0", "quit, not killed");
}

#[tokio::test(start_paused = true)]
async fn a_seal_fails_when_the_guest_does_not_power_off_by_itself() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run {
                ignores_agent_shutdown: true,
                ignores_powerdown: true,
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let seal = crate::template::build::shutdown_for_seal(&vm);
    tokio::pin!(seal);
    // Keep Tokio from advancing past the agent handshake while the fake's
    // socket I/O is pending. Once the request arrives, let the timers run.
    let result = tokio::select! {
        result = &mut seal => result,
        () = async {
            while hv.journal().is_empty() {
                tokio::task::yield_now().await;
            }
        } => seal.await,
    };

    assert_eq!(hv.journal(), ["agent shutdown ignored"]);
    assert_eq!(observed.exit().await.status, "exit status: 0");
    let error = result.expect_err("emulator quit must fail the seal");
    assert!(error.to_string().contains("quit rung"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn a_legacy_seal_allows_quit_but_rejects_kill() {
    for ignores_quit in [false, true] {
        let dirs = Dirs::new();
        let (vm, _) = vm(
            &dirs,
            r#"vm "dos" { template = "scratch" arch = "x86_64" profile = "windows-9x" disk = 1GiB }"#,
            Script {
                agent: false,
                runs: vec![Run {
                    ignores_powerdown: true,
                    ignores_quit,
                    ..Run::forever()
                }],
                ..Script::default()
            },
        );
        let (cbs, mut observed) = callbacks();
        start_vm(&vm, cbs).await.expect("start");
        let result = crate::template::build::shutdown_for_seal(&vm).await;
        let exit = observed.exit().await;
        if ignores_quit {
            assert_eq!(exit.status, "signal: 9");
            let error = result.expect_err("kill must fail the seal");
            assert!(error.to_string().contains("kill rung"), "{error}");
        } else {
            assert_eq!(exit.status, "exit status: 0");
            result.expect("windows-9x allows sealing after emulator quit");
        }
    }
}

/// A booting guest whose agent has not answered yet is not power-cut: the
/// stop waits for the agent its template sealed and asks it, rather than
/// skipping to ACPI and the quit — which is what `vmlab down` right after
/// `vmlab up` used to do. The guest here ignores ACPI and the emulator
/// ignores `quit`, so only the agent rung ends it cleanly.
#[tokio::test]
async fn a_booting_guest_is_stopped_through_its_agent_once_it_answers() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run {
                agent_answers_after: Some(Duration::from_secs(2)),
                ignores_powerdown: true,
                ignores_quit: true,
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    assert!(!vm.is_agent_up().await, "still booting");

    vm.stop(false).await.expect("graceful stop");

    let exit = observed.exit().await;
    assert_eq!(exit.reason, StopReason::Requested);
    assert_eq!(exit.status, "exit status: 0", "the agent shut it down");
}

/// The wait is bounded where ACPI would work: an agent that takes longer
/// than [`BOOTING_AGENT_WAIT`] is given up on, and the ladder goes on down.
///
/// [`BOOTING_AGENT_WAIT`]: super::stop_plan::BOOTING_AGENT_WAIT
#[tokio::test(start_paused = true)]
async fn a_booting_guest_waits_only_so_long_for_its_agent() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run {
                agent_answers_after: Some(Duration::from_secs(300)),
                ignores_powerdown: true,
                ignores_quit: true,
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");

    vm.stop(false).await.expect("graceful stop");

    let exit = observed.exit().await;
    assert_eq!(exit.status, "signal: 9", "gave up on the agent and killed");
}

/// A guest that ignores ACPI (`acpi_shutdown = false`, macOS) is stopped
/// through its agent too, booting or not; how much longer it is waited for
/// than a guest the power button would stop is the plan's
/// (`stop_plan::tests`).
#[tokio::test]
async fn a_booting_guest_that_ignores_acpi_is_stopped_through_its_agent() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        r#"vm "mac" { template = "scratch" arch = "x86_64" profile = "macos" disk = 10GiB }"#,
        Script {
            runs: vec![Run {
                agent_answers_after: Some(Duration::from_secs(2)),
                ignores_powerdown: true,
                ignores_quit: true,
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    assert!(!vm.template().resolved.acpi_shutdown);
    // The profile pins the TSC; a pin from an earlier start spares the probe,
    // which only vmlab-hv has now (§22.4).
    crate::qemu::tsc::write_pin(&vm.dirs.tsc_pin(), 3_000_000_000).expect("pin");
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");

    vm.stop(false).await.expect("graceful stop");

    let exit = observed.exit().await;
    assert_eq!(exit.status, "exit status: 0", "the agent shut it down");
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

/// A clone of a template that baked no agent is ready once it is running.
/// Nothing in that guest can ever answer a handshake, so holding it unready
/// only spends the caller's whole budget waiting for a signal that cannot
/// come — how an agent-less template used to fail its own build.
#[tokio::test]
async fn an_agentless_clone_is_ready_once_it_is_running() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            agent: false,
            ..Script::healthy()
        },
    );
    let parts = vm.template();
    vm.set_template(TemplateParts {
        resolved: parts.resolved.clone(),
        backing: Some(dirs.root.join("template.qcow2")),
        disk_size: parts.disk_size,
        first_boot: parts.first_boot.clone(),
        agent_version: None,
        variant: None,
        template_vars: None,
        logins: parts.logins.clone(),
    });

    let m: Arc<dyn Machine> = vm.clone();
    assert!(!m.is_ready().await, "not ready before it is running");

    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    m.wait_ready(SETTLE)
        .await
        .expect("an agent-less clone is ready once running");

    vm.stop(true).await.expect("stop");
}

/// The same clone with an agent baked in keeps the handshake as its gate: a
/// guest that *can* answer must, or readiness would mean nothing.
#[tokio::test]
async fn a_clone_with_an_agent_still_waits_for_the_handshake() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            agent: false,
            ..Script::healthy()
        },
    );
    let parts = vm.template();
    vm.set_template(TemplateParts {
        resolved: parts.resolved.clone(),
        backing: Some(dirs.root.join("template.qcow2")),
        disk_size: parts.disk_size,
        first_boot: parts.first_boot.clone(),
        agent_version: parts.agent_version.clone(), // an agent IS baked in
        logins: parts.logins.clone(),
        variant: None,
        template_vars: None,
    });

    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();
    let err = m
        .wait_ready(Duration::from_millis(300))
        .await
        .expect_err("no agent answered, so it is not ready");
    assert!(format!("{err:#}").contains("not ready"), "{err:#}");

    vm.stop(true).await.expect("stop");
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
    let lab: Arc<dyn LabServices> = Arc::new(TestLab::new(events));

    ctr.clone().start(lab.clone(), None).await.expect("start");

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

    ctr.clone().start(lab, None).await.expect("explicit start");
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

#[tokio::test]
async fn an_offline_capture_excludes_a_concurrent_start() {
    use crate::template::qimg::{self, Backend, testing};

    let _tool = testing::select(Backend::VmlabDisk).unwrap();
    let dirs = Dirs::new();
    let (vm, _) = vm(&dirs, LINUX_VM, Script::healthy());
    qimg::create_blank(&vm.dirs.primary_disk(), 1 << 20)
        .await
        .unwrap();

    let (entered, release) = testing::pause_next("snapshot");
    let machine = vm.clone();
    let capture = tokio::spawn(async move { machine.snapshot("cold").await });
    tokio::time::timeout(SETTLE, entered)
        .await
        .expect("capture did not reach the image tool")
        .unwrap();

    let (cbs, _observed) = callbacks();
    let start = tokio::time::timeout(SETTLE, start_vm(&vm, cbs)).await;
    release.send(()).unwrap();
    assert!(!capture.await.unwrap().expect("offline capture"));
    let error = start
        .expect("start must refuse promptly")
        .expect_err("start must refuse while an offline snapshot operation holds the disk");
    assert!(error.to_string().contains("snapshot"), "{error:#}");
    assert_eq!(vm.state().await, PowerState::Stopped);

    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start after capture");
    vm.stop(true).await.unwrap();
}

#[tokio::test]
async fn offline_snapshot_writes_exclude_start_on_both_machine_kinds() {
    use crate::template::qimg::{self, Backend, testing};

    let _tool = testing::select(Backend::VmlabDisk).unwrap();
    let vm_dirs = Dirs::new();
    let ctr_dirs = Dirs::new();
    let (vm, _) = vm(&vm_dirs, LINUX_VM, Script::healthy());
    let (ctr, _) = container(&ctr_dirs, WORKLOAD, Script::healthy());
    let vm_disk = vm.dirs.primary_disk();
    let ctr_disk = ctr.dirs.scratch_disk();
    let machines: [(Arc<dyn Machine>, PathBuf); 2] = [(vm, vm_disk), (ctr, ctr_disk)];

    for (machine, disk) in machines {
        qimg::create_blank(&disk, 1 << 20).await.unwrap();
        let (events, _) = EventLog::recording("t", disk.with_extension("events.jsonl"));
        let lab: Arc<dyn LabServices> = Arc::new(TestLab::new(events));
        for op in ["capture", "restore", "delete"] {
            let (entered, release) = testing::pause_next("snapshot");
            let work = {
                let machine = machine.clone();
                let lab = lab.clone();
                tokio::spawn(async move {
                    match op {
                        "capture" => {
                            assert!(!machine.snapshot("cold").await?);
                            Ok(())
                        }
                        "restore" => machine.restore(lab, "cold", false).await,
                        _ => machine.delete_snapshot("cold").await,
                    }
                })
            };
            tokio::time::timeout(SETTLE, entered)
                .await
                .unwrap_or_else(|_| panic!("{op} did not reach the image tool"))
                .unwrap();

            let start =
                tokio::time::timeout(SETTLE, machine.clone().start(lab.clone(), None)).await;
            release.send(()).unwrap();
            work.await.unwrap().unwrap();
            let error = start
                .expect("start must refuse promptly")
                .expect_err("start must refuse during an offline snapshot operation");
            assert!(error.to_string().contains("snapshot"), "{op}: {error:#}");
            assert_eq!(machine.state().await, PowerState::Stopped);
        }
        machine.clone().start(lab, None).await.unwrap();
        machine.stop(true).await.unwrap();
    }
}

/// A `firmware_dir` laid out like the bundled x86_64 set, each file holding
/// its own name: the plain and secure-boot CODE builds, the blank VARS
/// template and the one with keys enrolled. The four are the same length, as
/// the real templates are, so a size check tells them apart no better here
/// than it does in production.
fn fake_firmware(dirs: &Dirs) -> crate::config::host::HostConfig {
    let arch = dirs.root.join("firmware/x86_64");
    std::fs::create_dir_all(&arch).expect("firmware dir");
    for (name, bytes) in [
        ("OVMF_CODE_4M.fd", b"code:plain"),
        ("OVMF_CODE_4M.secboot.fd", b"code:secbt"),
        ("OVMF_VARS_4M.fd", b"vars:blank"),
        ("OVMF_VARS_4M.ms.fd", b"vars:keyed"),
    ] {
        std::fs::write(arch.join(name), bytes).expect("firmware file");
    }
    crate::config::host::HostConfig {
        firmware_dir: Some(dirs.root.join("firmware")),
        ..Default::default()
    }
}

/// The flash device opens the VM's copy read-write even when its template is read-only.
#[tokio::test]
async fn the_vars_copy_is_writable_whatever_the_template_is() {
    use std::os::unix::fs::PermissionsExt;

    let dirs = Dirs::new();
    let firmware = fake_firmware(&dirs);
    let template = dirs.root.join("firmware/x86_64/OVMF_VARS_4M.fd");
    std::fs::set_permissions(&template, std::fs::Permissions::from_mode(0o444)).unwrap();

    let (events, _) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab: Arc<dyn LabServices> = Arc::new(TestLab::new(events));
    let (mut vm, _) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB firmware = "ovmf" secure_boot = false }"#,
        Script::healthy(),
    );
    vm.set_host_config(firmware);
    let store = vm.dirs.ovmf_vars();
    let machine: Arc<dyn Machine> = vm;

    machine.clone().start(lab, None).await.unwrap();
    machine.stop(true).await.unwrap();

    let mode = std::fs::metadata(&store).unwrap().permissions().mode();
    assert_ne!(mode & 0o200, 0, "VARS store mode is {mode:o}");
    assert_eq!(std::fs::read(&store).unwrap(), b"vars:blank");
    assert_eq!(
        std::fs::metadata(&template).unwrap().permissions().mode() & 0o777,
        0o444
    );
}

/// A failed first copy must leave the next start free to copy a good template
/// (firmware review F5). Linux exposes /proc/self/mem as a regular file, but a
/// read at offset zero fails after the destination has been opened.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_failed_vars_copy_leaves_no_store() {
    let dirs = Dirs::new();
    let firmware = fake_firmware(&dirs);
    let template = dirs.root.join("firmware/x86_64/OVMF_VARS_4M.fd");
    std::fs::remove_file(&template).unwrap();
    std::os::unix::fs::symlink("/proc/self/mem", &template).unwrap();

    let (events, _) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab: Arc<dyn LabServices> = Arc::new(TestLab::new(events));
    let (mut vm, hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB firmware = "ovmf" secure_boot = false }"#,
        Script::healthy(),
    );
    vm.set_host_config(firmware);
    let store = vm.dirs.ovmf_vars();
    let machine: Arc<dyn Machine> = vm;

    let error = machine.clone().start(lab.clone(), None).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("copying OVMF VARS template"),
        "expected the copy to fail: {error:#}"
    );
    assert_eq!(hv.starts(), 0);
    assert!(!store.exists(), "failed copy left {}", store.display());

    std::fs::remove_file(&template).unwrap();
    std::fs::write(&template, b"vars:blank").unwrap();
    machine.clone().start(lab, None).await.unwrap();
    machine.stop(true).await.unwrap();
    assert_eq!(std::fs::read(&store).unwrap(), b"vars:blank");
}

/// A VM started without secure boot has a variable store nothing vouches
/// for: it was copied from the blank template, and the guest could write it
/// while the flash was unprotected. `secure_boot = true` on that VM must not
/// boot the secure-boot firmware on that store, where it would sit in setup
/// mode and verify nothing, or enforce keys the guest chose (firmware review
/// F2). The start is refused by name and the store is left as it was.
#[tokio::test]
async fn turning_secure_boot_on_does_not_boot_an_unenrolled_store() {
    const PLAIN: &str = r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB firmware = "ovmf" secure_boot = false }"#;
    const SECURE: &str = r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB firmware = "ovmf" secure_boot = true }"#;
    let dirs = Dirs::new();
    let firmware = fake_firmware(&dirs);
    let (events, _) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab: Arc<dyn LabServices> = Arc::new(TestLab::new(events));

    let (mut first, _) = vm(&dirs, PLAIN, Script::healthy());
    first.set_host_config(firmware.clone());
    let store = first.dirs.ovmf_vars();
    let machine: Arc<dyn Machine> = first;
    machine.clone().start(lab.clone(), None).await.unwrap();
    machine.stop(true).await.unwrap();
    assert_eq!(std::fs::read(&store).unwrap(), b"vars:blank");
    drop(machine);

    // The operator's edit: the same VM, the same directory, secure boot on.
    let (mut second, hv) = vm(&dirs, SECURE, Script::healthy());
    second.set_host_config(firmware);
    let machine: Arc<dyn Machine> = second;
    let outcome = machine.clone().start(lab.clone(), None).await;
    let enrolled = std::fs::read(&store).unwrap() == b"vars:keyed";
    match outcome {
        Ok(()) => {
            machine.stop(true).await.unwrap();
            assert!(
                enrolled,
                "secure boot started on the store the VM had without it"
            );
        }
        Err(error) => {
            let said = format!("{error:#}");
            assert!(
                said.contains("dc01")
                    && said.contains("secure_boot")
                    && said.contains("OVMF_VARS.fd"),
                "the refusal names the machine, the setting and the store: {said}"
            );
            assert_eq!(hv.starts(), 0, "a refused start launches nothing");
            assert_eq!(std::fs::read(&store).unwrap(), b"vars:blank");
            assert_eq!(machine.state().await, PowerState::Stopped);
        }
    }

    // A VM that never ran without secure boot is not in the way of this:
    // its first start copies the enrolled template and every later one
    // keeps that store.
    let fresh = Dirs::new();
    let firmware = fake_firmware(&fresh);
    for _ in 0..2 {
        let (mut vm, _) = vm(&fresh, SECURE, Script::healthy());
        vm.set_host_config(firmware.clone());
        let store = vm.dirs.ovmf_vars();
        let machine: Arc<dyn Machine> = vm;
        machine.clone().start(lab.clone(), None).await.unwrap();
        machine.stop(true).await.unwrap();
        assert_eq!(std::fs::read(&store).unwrap(), b"vars:keyed");
    }
}

/// An offline snapshot is the whole powered-off machine: the disks, the UEFI
/// variable store and the TPM's permanent state. A restore that rewound the
/// disks alone would leave boot entries, enrolled keys and whatever the guest
/// sealed to the TPM at the present, against a disk from the past.
#[tokio::test]
async fn an_offline_restore_returns_the_firmware_variables() {
    use crate::template::qimg::{self, Backend, testing};

    let _tool = testing::select(Backend::VmlabDisk).unwrap();
    let dirs = Dirs::new();
    let (vm, _) = vm(&dirs, LINUX_VM, Script::healthy());
    qimg::create_blank(&vm.dirs.primary_disk(), 1 << 20)
        .await
        .unwrap();
    let vars = vm.dirs.ovmf_vars();
    let tpm = vm.dirs.tpm_state();
    let permall = tpm.join("tpm2-00.permall");
    std::fs::write(&vars, b"vars at capture").unwrap();
    std::fs::create_dir_all(&tpm).unwrap();
    std::fs::write(&permall, b"tpm at capture").unwrap();

    let (events, _) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab: Arc<dyn LabServices> = Arc::new(TestLab::new(events));
    let machine: Arc<dyn Machine> = vm.clone();
    assert!(!machine.snapshot("cold").await.expect("offline capture"));

    // What a boot after the capture does: the guest rewrites a variable,
    // re-seals to the TPM, and leaves a file the capture never saw.
    std::fs::write(&vars, b"vars after capture").unwrap();
    std::fs::write(&permall, b"tpm after capture").unwrap();
    std::fs::write(tpm.join("later"), b"made after capture").unwrap();

    machine
        .clone()
        .restore(lab.clone(), "cold", false)
        .await
        .expect("offline restore");
    assert_eq!(std::fs::read(&vars).unwrap(), b"vars at capture");
    assert_eq!(std::fs::read(&permall).unwrap(), b"tpm at capture");
    assert!(
        !tpm.join("later").exists(),
        "the TPM state is replaced whole"
    );

    // A second restore gives the same answer: the capture is not consumed.
    std::fs::write(&vars, b"vars after restore").unwrap();
    machine
        .clone()
        .restore(lab, "cold", false)
        .await
        .expect("second offline restore");
    assert_eq!(std::fs::read(&vars).unwrap(), b"vars at capture");

    machine.delete_snapshot("cold").await.expect("delete");
    assert!(
        !vm.dirs.snapshot_state().join("snap-cold").exists(),
        "deleting the snapshot deletes what it held beside the disks"
    );
}

/// A machine whose hypervisor reports no snapshots capability — the fake's
/// default — reports exactly what a stopped one does rather than inventing a
/// result: the honest failure that keeps what a snapshot does to a guest
/// verified where it belongs, against a running lab (ADR-0001, ADR-0020).
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

// ---- capabilities (ADR-0020) ------------------------------------------------

/// A running machine whose hypervisor reports none of the snapshots, input,
/// frames, hot-plug or media capabilities refuses every operation that needs
/// one with the message it always gave — `<name>: not running` — and nothing
/// is half-done: the VM is still running, holds no USB device, and has no
/// screenshot on disk.
#[tokio::test]
async fn a_missing_capability_is_refused_as_not_running() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB usb = true cdrom = "./install.iso" }"#,
        Script::healthy(),
    );
    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let (events, _rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events)) as Arc<dyn LabServices>;
    let m: Arc<dyn Machine> = vm.clone();
    let refused = |e: anyhow::Error| {
        let e = format!("{e:#}");
        assert!(e.contains("dc01: not running"), "{e}");
    };

    refused(m.snapshot("s1").await.expect_err("no snapshots").into());
    refused(
        m.clone()
            .restore(lab, "s1", true)
            .await
            .expect_err("no snapshots"),
    );
    refused(m.usb_attach(&usb_plan()).await.expect_err("no hot-plug"));
    refused(m.eject_cdrom().await.expect_err("no media"));
    let display = m.clone().display().expect("a VM reports a display");
    let shot = dirs.root.join("shot.png");
    refused(display.screenshot(&shot).await.expect_err("no frames"));
    refused(
        display
            .send_keys("ctrl-alt-delete")
            .await
            .expect_err("no input"),
    );

    assert_eq!(vm.state().await, PowerState::Running);
    assert!(m.usb_attached().await.is_empty());
    assert!(!shot.exists());
    vm.stop(true).await.expect("stop");
}

/// The same operations on a machine whose hypervisor reports each
/// capability get through to it: the snapshot is an online one, restoring it
/// leaves the VM running, the device is held until detached, and the
/// screenshot lands.
#[tokio::test]
async fn a_reported_capability_is_used() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB usb = true cdrom = "./install.iso" }"#,
        Script {
            capabilities: Capabilities::all(),
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let (events, _rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events)) as Arc<dyn LabServices>;
    let m: Arc<dyn Machine> = vm.clone();

    assert!(m.snapshot("s1").await.expect("snapshot"), "an online one");
    m.clone()
        .restore(lab, "s1", true)
        .await
        .expect("online restore");
    assert_eq!(vm.state().await, PowerState::Running);

    let plan = usb_plan();
    let held = m.usb_attach(&plan).await.expect("attach");
    assert_eq!(m.usb_attached().await, vec![held.clone()]);
    m.usb_detach(&held.qdev_id).await.expect("detach");
    assert!(m.usb_attached().await.is_empty());

    m.eject_cdrom().await.expect("eject");
    let display = m.clone().display().expect("a VM reports a display");
    let shot = dirs.root.join("shot.png");
    display.screenshot(&shot).await.expect("screenshot");
    assert!(shot.exists());
    display.send_keys("ctrl-alt-delete").await.expect("keys");

    vm.stop(true).await.expect("stop");
}

#[tokio::test]
async fn a_failed_online_snapshot_load_resumes_the_vm() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB }"#,
        Script {
            capabilities: Capabilities::all(),
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.unwrap();
    let (events, _rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events)) as Arc<dyn LabServices>;
    let machine: Arc<dyn Machine> = vm.clone();
    let error = machine.restore(lab, "missing", true).await.unwrap_err();
    assert!(format!("{error:#}").contains("does not exist"));
    assert!(!hv.paused(), "failed load left CPUs paused");
    assert_eq!(vm.state().await, PowerState::Running);
    vm.stop(true).await.unwrap();
}

#[tokio::test]
async fn a_failed_container_load_resumes_the_container() {
    let dirs = Dirs::new();
    let (ctr, hv) = container(
        &dirs,
        WORKLOAD,
        Script {
            capabilities: Capabilities {
                snapshots: true,
                ..Capabilities::default()
            },
            ..Script::healthy()
        },
    );
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");
    let (events, _rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events));

    let error = ctr
        .clone()
        .restore(lab, "missing", true)
        .await
        .expect_err("no such snapshot");
    assert!(format!("{error:#}").contains("does not exist"), "{error:#}");
    assert!(!hv.paused(), "failed load left CPUs paused");
    ctr.stop(true).await.expect("stop");
}

/// A container's online snapshot rides the same capability: absent, the
/// refusal it always gave; present, an online capture it can restore.
#[tokio::test]
async fn a_containers_online_snapshot_rides_the_snapshots_capability() {
    for granted in [false, true] {
        let dirs = Dirs::new();
        let (ctr, _hv) = container(
            &dirs,
            WORKLOAD,
            Script {
                capabilities: Capabilities {
                    snapshots: granted,
                    ..Capabilities::default()
                },
                ..Script::healthy()
            },
        );
        let (cbs, _observed) = callbacks();
        start_container(&ctr, cbs).await.expect("start");
        let m: Arc<dyn Machine> = ctr.clone();
        match m.snapshot("s1").await {
            Ok(online) => {
                assert!(granted && online, "granted={granted}");
                let (events, _rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
                let lab = Arc::new(TestLab::new(events)) as Arc<dyn LabServices>;
                m.clone()
                    .restore(lab, "s1", true)
                    .await
                    .expect("online restore");
                assert_eq!(ctr.state().await, PowerState::Running);
            }
            Err(e) => {
                assert!(!granted, "{e:#}");
                assert!(format!("{e:#}").contains("web: not running"), "{e:#}");
            }
        }
        ctr.stop(true).await.expect("stop");
    }
}

/// `poweroff` asks the hypervisor to quit through [`Control`] and waits for
/// the exit — a requested stop, not a crash.
#[tokio::test]
async fn poweroff_quits_through_the_control_channel() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();
    m.poweroff().await.expect("poweroff");
    assert_eq!(vm.state().await, PowerState::Stopped);
    assert_eq!(observed.exit().await.reason, StopReason::Requested);
}

/// A forced stop gives a held host USB device back before it kills the
/// hypervisor (§5.2.1: "A VM stop releases everything it held"). The device
/// returns to its host driver only when the hypervisor drops its backend, and
/// a SIGKILL drops nothing: the kernel then releases the claims and binds no
/// driver, so the host has a device it cannot use until a replug.
#[tokio::test]
async fn a_forced_stop_detaches_usb_devices_before_the_kill() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        USB_VM,
        Script {
            capabilities: Capabilities::all(),
            ..Script::healthy()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();
    let held = m.usb_attach(&usb_plan()).await.expect("attach");

    m.stop(true).await.expect("forced stop");

    assert_eq!(observed.exit().await.status, "signal: 9");
    assert_eq!(
        hv.journal(),
        vec![
            format!("attach {}", held.qdev_id),
            format!("detach {}", held.qdev_id),
            "kill".to_string(),
        ],
        "the device goes back to the host while the hypervisor still runs"
    );
    assert!(m.usb_attached().await.is_empty());
}

/// The same for a graceful stop that ends at the kill rung: a guest that
/// ignores the shutdown request and a hypervisor that ignores `quit`.
///
/// Time is paused: the rungs before the kill wait for the guest.
#[tokio::test(start_paused = true)]
async fn a_stop_that_ends_at_the_kill_detaches_usb_devices_first() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        USB_VM,
        Script {
            agent: false,
            capabilities: Capabilities::all(),
            runs: vec![Run {
                ignores_powerdown: true,
                ignores_quit: true,
                ..Run::forever()
            }],
            ..Script::default()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();
    let held = m.usb_attach(&usb_plan()).await.expect("attach");

    m.stop(false).await.expect("graceful stop");

    assert_eq!(observed.exit().await.status, "signal: 9");
    assert_eq!(
        hv.journal(),
        vec![
            format!("attach {}", held.qdev_id),
            format!("detach {}", held.qdev_id),
            "kill".to_string(),
        ]
    );
}

/// A hypervisor that never answers the detach is killed all the same: the
/// forced stop is what ends a wedged machine, so the detach has a limit.
///
/// Time is paused: the limit is the one the stop itself keeps.
#[tokio::test(start_paused = true)]
async fn a_forced_stop_kills_a_hypervisor_that_never_answers_the_detach() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        USB_VM,
        Script {
            capabilities: Capabilities::all(),
            runs: vec![Run {
                ignores_detach: true,
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();
    let held = m.usb_attach(&usb_plan()).await.expect("attach");

    m.stop(true).await.expect("forced stop");

    assert_eq!(observed.exit().await.status, "signal: 9");
    assert_eq!(
        hv.journal(),
        vec![
            format!("attach {}", held.qdev_id),
            format!("detach {}", held.qdev_id),
            "kill".to_string(),
        ]
    );
    assert_eq!(vm.state().await, PowerState::Stopped);
}

/// A stop that ends before the kill rung detaches nothing: the hypervisor
/// exits by itself and gives its devices back as it drops them.
#[tokio::test]
async fn a_stop_that_ends_before_the_kill_leaves_the_detach_to_the_hypervisor() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        USB_VM,
        Script {
            capabilities: Capabilities::all(),
            ..Script::healthy()
        },
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();
    let held = m.usb_attach(&usb_plan()).await.expect("attach");

    m.stop(false).await.expect("graceful stop");

    assert_eq!(observed.exit().await.status, "exit status: 0");
    assert_eq!(hv.journal(), vec![format!("attach {}", held.qdev_id)]);
    assert!(m.usb_attached().await.is_empty());
}

/// A VM with the hot-plug USB controller.
const USB_VM: &str = r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB usb = true }"#;

/// A host USB device the hot-plug tests attach: an ordinary flash drive.
fn usb_plan() -> crate::usb::AttachPlan {
    let device = crate::usb::HostDevice {
        port: "3-2".into(),
        bus: 3,
        devnum: 7,
        vendor_id: "0781".into(),
        product_id: "5567".into(),
        manufacturer: None,
        product: Some("Cruzer Blade".into()),
        serial: None,
        speed: "480".into(),
        class: 0,
        interfaces: vec![crate::usb::Interface {
            class: 8,
            subclass: 6,
            protocol: 80,
        }],
    };
    crate::usb::AttachPlan {
        args: serde_json::json!({"driver": "usb-host", "id": device.qdev_id()}),
        device,
    }
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

/// **`up` refreshes a stale agent, and a refresh that fails never fails
/// `up`** (§19.4). Driven against a VM whose agent answers: its handshake is
/// not a stamp (`0.0.0-fake`), so what it is falls back to the stamp its
/// template sealed (`0.1.0`).
///
/// A shipped asset carrying that stamp is current, and nothing is said. One
/// carrying another is pushed — and the fake agent refuses the file session,
/// so the push fails the way a real refusal does: a `warning:` naming the
/// machine and both stamps, `machine.agent_updated` with the error, the old
/// agent still answering, and the machine not marked diverged.
#[tokio::test]
async fn a_stale_agent_is_refreshed_and_a_failed_refresh_only_warns() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let config = crate::config::load_lab_source(
        &format!("import <vmlab.wcl>\nlab \"t\" {{\n{LINUX_VM}\n}}\n"),
        "<test>",
        &dirs.root,
    )
    .expect("lab source");
    let lab = super::lab::LabRuntime::with_machines(config, vec![vm.clone()]).expect("runtime");
    lab.agent_updates.store(true, Ordering::Relaxed);
    let mut events = lab.events.subscribe();
    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();

    let binary = dirs.root.join("vmlab-agent");
    std::fs::write(&binary, b"a newer agent").expect("asset");
    let shipped = |version: &'static str| {
        let path = binary.clone();
        move |_: &dyn Machine| {
            Ok(crate::agent_asset::AgentAsset {
                path: path.clone(),
                version: version.to_string(),
            })
        }
    };
    let said = Arc::new(std::sync::Mutex::new(String::new()));
    let sink: crate::scripting::OutputSink = {
        let said = said.clone();
        Arc::new(move |s: String| said.lock().expect("said").push_str(&s))
    };

    lab.refresh_agent_with(&m, &sink, shipped("0.1.0")).await;
    assert_eq!(*said.lock().unwrap(), "", "a current agent is left alone");

    lab.refresh_agent_with(&m, &sink, shipped("agent=new"))
        .await;
    let out = said.lock().unwrap().clone();
    assert!(
        out.contains("warning: agent: could not update \"dc01\" (0.1.0 → agent=new)"),
        "{out}"
    );
    let event = collect_events(&mut events, &["machine.agent_updated"], 1)
        .await
        .remove(0);
    assert_eq!(event.data["machine"], "dc01");
    assert_eq!(event.data["ok"], false);
    assert!(
        event.data["error"]
            .as_str()
            .is_some_and(|e| e.contains("file session")),
        "{}",
        event.data
    );
    assert!(
        m.agent_answering().await,
        "the old agent is still answering"
    );
    let status = lab.status().await;
    assert!(
        !status.machines.iter().any(|s| s.agent_diverged),
        "nothing was replaced, so nothing diverged"
    );

    vm.stop(true).await.expect("stop");
}

/// **A refresh cut short by a stop is said quietly and accurately.** The
/// refresh waits for the agent; the machine is stopped before it answers.
/// Nothing was pushed and nothing went wrong, so `up`'s output carries no
/// `warning:`, and `machine.agent_updated` says the machine stopped rather
/// than reporting the wait's own error as a failure.
#[tokio::test]
async fn a_refresh_cut_short_by_a_stop_is_a_quiet_event() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        LINUX_VM,
        Script {
            runs: vec![Run {
                agent_answers_after: Some(Duration::from_secs(3600)),
                ..Run::forever()
            }],
            ..Script::healthy()
        },
    );
    let config = crate::config::load_lab_source(
        &format!("import <vmlab.wcl>\nlab \"t\" {{\n{LINUX_VM}\n}}\n"),
        "<test>",
        &dirs.root,
    )
    .expect("lab source");
    let lab = super::lab::LabRuntime::with_machines(config, vec![vm.clone()]).expect("runtime");
    lab.agent_updates.store(true, Ordering::Relaxed);
    let mut events = lab.events.subscribe();
    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();

    let binary = dirs.root.join("vmlab-agent");
    std::fs::write(&binary, b"a newer agent").expect("asset");
    let said = Arc::new(std::sync::Mutex::new(String::new()));
    let sink: crate::scripting::OutputSink = {
        let said = said.clone();
        Arc::new(move |s: String| said.lock().expect("said").push_str(&s))
    };
    let refresh = {
        let (lab, m) = (lab.clone(), m.clone());
        tokio::spawn(async move {
            lab.refresh_agent_with(&m, &sink, move |_: &dyn Machine| {
                Ok(crate::agent_asset::AgentAsset {
                    path: binary.clone(),
                    version: "agent=new".into(),
                })
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    vm.stop(true).await.expect("stop");
    refresh.await.expect("refresh task");

    assert_eq!(*said.lock().unwrap(), "", "no warning for a stop");
    let event = collect_events(&mut events, &["machine.agent_updated"], 1)
        .await
        .remove(0);
    assert_eq!(event.data["ok"], false, "{}", event.data);
    assert_eq!(event.data["stopped"], true, "{}", event.data);
    assert!(event.data["from"].is_null(), "{}", event.data);
    assert!(
        event.data["error"]
            .as_str()
            .is_some_and(|e| e.contains("stopped before its agent answered")),
        "{}",
        event.data
    );
}

// ---- readiness and the deferred agent refresh (§19.4) -------------------------

/// A lab runtime over one fake-hypervisor VM, with agent refreshes on and the
/// shipped asset supplied by the test. `vm_decl` is the lab file's `vm` block,
/// which is where `agent_update` is read from.
fn refreshing_lab(
    dirs: &Dirs,
    vm: &Arc<VmInstance>,
    vm_decl: &str,
    shipped: super::lab::ShippedAgent,
) -> Arc<super::lab::LabRuntime> {
    let config = crate::config::load_lab_source(
        &format!("import <vmlab.wcl>\nlab \"t\" {{\n{vm_decl}\n}}\n"),
        "<test>",
        &dirs.root,
    )
    .expect("lab source");
    let lab = super::lab::LabRuntime::with_machines(config, vec![vm.clone()]).expect("runtime");
    lab.agent_updates.store(true, Ordering::Relaxed);
    *lab.shipped_agent.write().expect("shipped_agent") = shipped;
    lab
}

/// **A machine owed an agent refresh is not ready until the refresh has
/// run** (§19.4). The refresh restarts the agent, so a machine reported ready
/// first hands whoever was waiting on it a channel about to close — which is
/// what `vmlab exec` met live, as "agent channel closed during exec".
///
/// The refresh here is held at its first step until the test lets it go, and
/// then fails (the fake agent refuses the file session): readiness waits for
/// it, and a failed refresh still ends with the machine ready on its old
/// agent, `vm.ready` arriving only after the refresh said how it went.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_owed_a_refresh_is_not_ready_until_the_refresh_has_run() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let binary = dirs.root.join("vmlab-agent");
    std::fs::write(&binary, b"a newer agent").expect("asset");
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    let lab = refreshing_lab(
        &dirs,
        &vm,
        LINUX_VM,
        Arc::new(move |_: &dyn Machine| {
            // Off the reactor: a worker blocked here would strand whatever
            // it had queued, the fake agent's own answers among them.
            let _ = tokio::task::block_in_place(|| held.lock().expect("held").recv());
            Ok(crate::agent_asset::AgentAsset {
                path: binary.clone(),
                version: "agent=new".into(),
            })
        }),
    );
    let mut events = lab.events.subscribe();
    let m: Arc<dyn Machine> = vm.clone();

    lab.start_with_deferred_refresh(m.name(), None)
        .await
        .expect("start");
    m.wait_agent_up(SETTLE).await.expect("the agent answered");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !m.is_ready().await,
        "the agent answered, but the refresh it is owed has not run"
    );

    release.send(()).expect("release the refresh");
    m.wait_ready(SETTLE)
        .await
        .expect("a failed refresh still ends with the machine ready");
    let seen = collect_events(&mut events, &["machine.agent_updated", "vm.ready"], 2).await;
    assert_eq!(seen[0].event, "machine.agent_updated", "{seen:?}");
    assert_eq!(seen[0].data["ok"], false);
    assert_eq!(
        seen[1].event, "vm.ready",
        "ready only once the refresh said"
    );
    assert!(m.agent_answering().await, "on its old agent");

    vm.stop(true).await.expect("stop");
}

/// The other half: a machine with **no** refresh owed is not delayed by one.
/// An agent already current is held only for the handshake that shows it; a
/// machine opted out with `agent_update = false` is never even asked about
/// its asset.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_owed_no_refresh_is_ready_at_its_handshake() {
    // Current: the shipped stamp is the one the template sealed.
    let dirs = Dirs::new();
    let (vm1, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let binary = dirs.root.join("vmlab-agent");
    let lab = refreshing_lab(
        &dirs,
        &vm1,
        LINUX_VM,
        Arc::new(move |_: &dyn Machine| {
            Ok(crate::agent_asset::AgentAsset {
                path: binary.clone(),
                version: "0.1.0".into(),
            })
        }),
    );
    let mut events = lab.events.subscribe();
    let m: Arc<dyn Machine> = vm1.clone();
    lab.start_with_deferred_refresh(m.name(), None)
        .await
        .expect("start");
    m.wait_ready(SETTLE).await.expect("ready");
    collect_events(&mut events, &["vm.ready"], 1).await;
    vm1.stop(true).await.expect("stop");

    // Opted out: the asset lookup is never reached.
    let dirs = Dirs::new();
    let opted_out = r#"vm "dc01" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 10GiB agent_update = false }"#;
    let (vm2, _hv) = vm(&dirs, opted_out, Script::healthy());
    let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let lab = refreshing_lab(&dirs, &vm2, opted_out, {
        let asked = asked.clone();
        Arc::new(move |_: &dyn Machine| {
            asked.store(true, Ordering::SeqCst);
            anyhow::bail!("never asked")
        })
    });
    let m: Arc<dyn Machine> = vm2.clone();
    lab.start_with_deferred_refresh(m.name(), None)
        .await
        .expect("start");
    m.wait_ready(SETTLE).await.expect("ready");
    assert!(
        !asked.load(Ordering::SeqCst),
        "an opted-out machine owes no refresh"
    );
    vm2.stop(true).await.expect("stop");
}

/// **A VM's status names the agent actually running** (§19.4). After a
/// refresh or a repair that is not the stamp its template sealed, and
/// reporting the sealed one would say the push never happened. The template's
/// stamp stands in only until an agent has answered, and again once the
/// machine has stopped.
#[tokio::test]
async fn a_vms_status_names_the_agent_that_answered_not_the_sealed_one() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let m: Arc<dyn Machine> = vm.clone();
    let agent = |detail: crate::status::MachineDetail| match detail {
        crate::status::MachineDetail::Vm(vm) => vm.agent_version,
        _ => panic!("a VM reports VM detail"),
    };
    assert_eq!(
        agent(m.status_detail().await).as_deref(),
        Some("0.1.0"),
        "nothing has answered, so the sealed stamp"
    );

    let (cbs, _observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    m.wait_ready(SETTLE).await.expect("ready");
    assert_eq!(
        agent(m.status_detail().await).as_deref(),
        Some("0.0.0-fake"),
        "the handshake's own version"
    );

    vm.stop(true).await.expect("stop");
    assert_eq!(agent(m.status_detail().await).as_deref(), Some("0.1.0"));
}

/// The paused window (§11.1, D20): the lab's `before_resume` runs once the
/// emulator is up — the VM is `Starting`, control answers — and before the
/// CPUs are released, so a recorder attached there catches the first frame;
/// readiness follows it. An error there fails the start and leaves the VM
/// stopped.
#[tokio::test]
async fn before_resume_runs_in_the_paused_window_and_can_refuse_the_start() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, Script::healthy());
    let (events, _rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events));
    let m: Arc<dyn Machine> = vm.clone();

    m.clone()
        .start(lab.clone() as Arc<dyn LabServices>, None)
        .await
        .expect("start");
    m.wait_ready(SETTLE).await.expect("ready");
    let seen = lab.hooks();
    let resume = seen
        .iter()
        .position(|s| s == "resume:dc01")
        .expect("before_resume ran");
    let ready = seen
        .iter()
        .position(|s| s == "ready:dc01")
        .expect("machine_ready ran");
    assert!(resume < ready, "{seen:?}");
    m.stop(true).await.expect("stop");
    m.wait_state(PowerState::Stopped, SETTLE)
        .await
        .expect("stopped");

    *lab.refuse_resume.lock().unwrap() = Some("recorder could not attach".into());
    let err = m
        .clone()
        .start(lab.clone() as Arc<dyn LabServices>, None)
        .await
        .expect_err("a refused resume fails the start");
    assert!(
        format!("{err:#}").contains("recorder could not attach"),
        "{err:#}"
    );
    m.wait_state(PowerState::Stopped, SETTLE)
        .await
        .expect("the failed start leaves the VM stopped");
}

// ---- vmlab-hv's control protocol (§22, ADR-0021) ----------------------------
//
// The same machines, controlled through the vmlab-hv adapter's real protocol
// client against a fake control server built on `vmlab_hv_proto`'s `Server`
// (`Script::vmlab_hv`): what the lifecycle does with resume, the stop ladder,
// a guest's own power-off and online snapshots, when the answers arrive the
// way vmlab-hv sends them.

fn on_vmlab_hv(script: Script) -> Script {
    Script {
        vmlab_hv: true,
        ..script
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_container_on_vmlab_hv_starts_becomes_ready_and_stops() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(
        &dirs,
        WORKLOAD,
        on_vmlab_hv(Script {
            runs: vec![Run {
                ctl: vec![(
                    Duration::ZERO,
                    vmlab_cinit_proto::CtlEvent::Started { pid: 7 },
                )],
                ..Run::forever()
            }],
            ..Script::healthy()
        }),
    );
    assert_eq!(
        Machine::hypervisor(&*ctr),
        crate::config::hypervisor::HypervisorKind::Vmlab
    );
    let (cbs, mut observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");
    let m: Arc<dyn Machine> = ctr.clone();
    m.wait_ready(SETTLE).await.expect("ready");
    assert_eq!(observed.ready_count(), 1);

    ctr.stop(false).await.expect("graceful stop");
    assert_eq!(observed.exit().await.reason, StopReason::Requested);
    assert_eq!(ctr.state().await, PowerState::Stopped);
}

/// An online snapshot of a container on vmlab-hv goes over its protocol:
/// captured, restored into the running micro-VM, and a name it does not have
/// refused in vmlab-hv's own words.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_container_on_vmlab_hv_snapshots_online_over_its_protocol() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(
        &dirs,
        WORKLOAD,
        on_vmlab_hv(Script {
            capabilities: Capabilities {
                snapshots: true,
                ..Capabilities::default()
            },
            ..Script::healthy()
        }),
    );
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");
    let (events, _rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events)) as Arc<dyn LabServices>;
    let m: Arc<dyn Machine> = ctr.clone();

    assert!(m.snapshot("s1").await.expect("snapshot"), "an online one");
    m.clone()
        .restore(lab.clone(), "s1", true)
        .await
        .expect("online restore");
    assert_eq!(ctr.state().await, PowerState::Running);
    let err = m
        .clone()
        .restore(lab, "nope", true)
        .await
        .expect_err("no such snapshot");
    assert!(format!("{err:#}").contains("snapshot_not_found"), "{err:#}");
    m.delete_snapshot("s1").await.expect("delete");
    ctr.stop(true).await.expect("stop");
}

/// Without the capability in the handshake, the refusal a machine without
/// snapshots always gave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vmlab_hv_that_does_not_advertise_snapshots_is_refused_them() {
    let dirs = Dirs::new();
    let (ctr, _hv) = container(&dirs, WORKLOAD, on_vmlab_hv(Script::healthy()));
    let (cbs, _observed) = callbacks();
    start_container(&ctr, cbs).await.expect("start");
    let m: Arc<dyn Machine> = ctr.clone();
    let err = m.snapshot("s1").await.expect_err("not advertised");
    assert!(format!("{err:#}").contains("web: not running"), "{err:#}");
    ctr.stop(true).await.expect("stop");
}

/// `classify_exit` reads the guest-shutdown flag the moment the exit is
/// seen; on vmlab-hv that flag comes from a `shutdown` event, so a guest
/// powering itself off still reads as the guest's doing, and the emulator
/// dying as a crash. Driven through a VM, whose exit monitor is the one
/// that classifies — the adapter's control logic is the same for both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guest_power_off_over_vmlab_hvs_events_is_classified_as_the_guests() {
    let dirs = Dirs::new();
    let (vm1, _hv) = vm(
        &dirs,
        LINUX_VM,
        on_vmlab_hv(Script {
            runs: vec![Run::guest_powers_off(Duration::from_millis(300))],
            ..Script::healthy()
        }),
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm1, cbs).await.expect("start");
    assert_eq!(observed.exit().await.reason, StopReason::GuestInitiated);

    let dirs = Dirs::new();
    let (vm2, _hv) = vm(
        &dirs,
        LINUX_VM,
        on_vmlab_hv(Script {
            runs: vec![Run::dies(Duration::from_millis(300), "signal: 11")],
            ..Script::healthy()
        }),
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm2, cbs).await.expect("start");
    assert_eq!(observed.exit().await.reason, StopReason::Crashed);
}

/// S3 over vmlab-hv's protocol: the guest's `suspend` is seen, and a
/// `prevent_sleep` VM is woken with vmlab-hv's `resume`, which it answers
/// with `wakeup`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prevent_sleep_wakes_a_vmlab_hv_guest_with_resume() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(
        &dirs,
        &sleepy_vm(true),
        on_vmlab_hv(Script {
            runs: vec![Run::guest_sleeps(Duration::from_millis(300))],
            ..Script::healthy()
        }),
    );
    let (events, mut rx) = EventLog::recording("t", dirs.root.join("events.jsonl"));
    let lab = Arc::new(TestLab::new(events)) as Arc<dyn LabServices>;
    let m: Arc<dyn Machine> = vm.clone();

    m.clone().start(lab, None).await.expect("start");
    let seen = collect_events(&mut rx, &["vm.suspended", "vm.woken"], 2).await;
    assert_eq!(seen[0].event, "vm.suspended");
    assert_eq!(seen[1].event, "vm.woken");
    assert_eq!(seen[1].data["cause"], "prevent_sleep");
    m.wait_state(PowerState::Running, SETTLE)
        .await
        .expect("awake again");
    vm.stop(true).await.expect("stop");
}

/// The start-time refusal itself: on a host whose `/dev/kvm` is not usable a
/// VM on vmlab-hv is refused before anything spawns, naming KVM — vmlab-hv
/// has no emulation to fall back on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vmlab_hv_vm_on_a_host_without_kvm_is_refused_at_start() {
    let dirs = Dirs::new();
    let (mut vm, hv) = vm(&dirs, LINUX_VM, on_vmlab_hv(Script::healthy()));
    vm.set_host_facts(crate::config::hypervisor::HostFacts {
        kvm: false,
        ..kvm_host()
    });
    let (cbs, _observed) = callbacks();
    let err = start_vm(&vm, cbs).await.expect_err("refused");
    assert!(format!("{err:#}").contains("/dev/kvm"), "{err:#}");
    assert_eq!(hv.starts(), 0, "nothing was started");
    assert_eq!(vm.state().await, PowerState::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_container_without_kvm_is_refused_before_launch_on_either_hypervisor() {
    for script in [Script::healthy(), on_vmlab_hv(Script::healthy())] {
        let dirs = Dirs::new();
        let (mut ctr, hv) = container(&dirs, WORKLOAD, script);
        ctr.set_host_facts(crate::config::hypervisor::HostFacts {
            kvm: false,
            ..kvm_host()
        });
        let (cbs, _observed) = callbacks();
        let err = start_container(&ctr, cbs).await.expect_err("refused");
        assert!(format!("{err:#}").contains("KVM is required"), "{err:#}");
        assert_eq!(hv.starts(), 0, "nothing was started");
        assert_eq!(ctr.state().await, PowerState::Stopped);
    }
}

#[tokio::test]
async fn apple_smc_without_a_host_key_is_refused_before_launch() {
    let dirs = Dirs::new();
    let (mut vm, _hv) = vm(
        &dirs,
        r#"vm "mac" { template = "scratch" arch = "x86_64" profile = "macos" disk = 10GiB }"#,
        Script::healthy(),
    );
    vm.set_host_config(Default::default());
    let error = vm.check_host_config().unwrap_err();
    assert_eq!(
        error.to_string(),
        crate::config::host::APPLE_SMC_OSK_REQUIRED
    );
    let (cbs, _observed) = callbacks();
    let error = start_vm(&vm, cbs).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        crate::config::host::APPLE_SMC_OSK_REQUIRED
    );
    assert_eq!(vm.state().await, PowerState::Stopped);
    assert!(!vm.run_dir().exists());
}

#[tokio::test]
async fn apple_smc_preflight_refuses_the_whole_lab_before_any_boot() {
    let linux_dirs = Dirs::new();
    let mac_dirs = Dirs::new();
    let mac_decl =
        r#"vm "mac" { template = "scratch" arch = "x86_64" profile = "macos" disk = 10GiB }"#;
    let (linux, _) = vm(&linux_dirs, LINUX_VM, Script::healthy());
    let (mut mac, _) = vm(&mac_dirs, mac_decl, Script::healthy());
    mac.set_host_config(Default::default());
    let source = format!("import <vmlab.wcl>\nlab \"t\" {{ {LINUX_VM} {mac_decl} }}");
    let config = crate::config::load_lab_source(&source, "<test>", &linux_dirs.root).unwrap();
    let lab =
        super::lab::LabRuntime::with_machines(config, vec![linux.clone(), mac.clone()]).unwrap();
    let error = lab
        .preflight_binaries(&["dc01".into(), "mac".into()])
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        crate::config::host::APPLE_SMC_OSK_REQUIRED
    );
    assert!(!linux.run_dir().exists());
    assert!(!mac.run_dir().exists());
}

/// A guest reset on vmlab-hv happens in place (PRD §22): the same process
/// runs on, so to the lab it is one run — no second start, no exit, no second
/// `ready` — but readiness drops while the guest reboots and comes back once
/// the rebooted guest's agent has been handshaken, and a later stop still
/// ends it as asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guest_reset_on_vmlab_hv_happens_in_place_as_one_run() {
    let dirs = Dirs::new();
    let (vm, hv) = vm(
        &dirs,
        LINUX_VM,
        on_vmlab_hv(Script {
            runs: vec![Run::guest_resets(
                Duration::from_millis(600),
                Duration::from_millis(800),
            )],
            ..Script::healthy()
        }),
    );
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    let m: Arc<dyn Machine> = vm.clone();
    m.wait_ready(SETTLE).await.expect("ready");
    let before = m.agent().await.expect("agent").info();

    // Readiness drops with the reset...
    tokio::time::timeout(SETTLE, async {
        while m.is_ready().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("readiness never dropped for the reset");
    assert_eq!(
        vm.state().await,
        PowerState::Running,
        "a reset is not a stop"
    );
    // ...and returns with the rebooted guest's agent.
    m.wait_ready(SETTLE).await.expect("ready again");
    let agent = m.agent().await.expect("agent");
    let after = agent.info();
    assert_ne!(
        after.instance, before.instance,
        "the new agent was handshaken"
    );
    let output = agent
        .exec(
            vec!["echo".into(), "rebooted".into()],
            vec![],
            None,
            None,
            SETTLE,
            None,
        )
        .await
        .expect("exec as soon as ready");
    assert_eq!(output.exit_code, 0);
    assert_eq!(hv.starts(), 1, "the reset happened in place");
    assert_eq!(observed.ready_count(), 1, "one run, announced once");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), observed.exit())
            .await
            .is_err(),
        "a reset is not an exit"
    );

    vm.stop(false).await.expect("stop");
    assert_eq!(observed.exit().await.reason, StopReason::Requested);
}

/// The stop ladder's ACPI rung over vmlab-hv's `powerdown`: the guest acts
/// on it, sends `shutdown`, exits — a requested stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stop_ladder_powers_down_over_vmlab_hv() {
    let dirs = Dirs::new();
    let (vm, _hv) = vm(&dirs, LINUX_VM, on_vmlab_hv(Script::healthy()));
    let (cbs, mut observed) = callbacks();
    start_vm(&vm, cbs).await.expect("start");
    vm.stop(false).await.expect("stop");
    assert_eq!(observed.exit().await.reason, StopReason::Requested);
    assert_eq!(vm.state().await, PowerState::Stopped);
}
