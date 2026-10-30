//! Lab runtime: owns the VM instances, network fabric, persisted state, and
//! the lifecycle verbs (PRD §7). Lives inside the lab daemon.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::container::{ContainerDirs, ContainerInstance};
use super::events::EventLog;
use super::forward_plan::{self, ForwardPlan, ForwardRule, InstalledForward};
use super::freeze_clone;
use super::machine::Machine;
use super::machine_build::{self, BuildContext, Built, MachineSet};
use super::mount_report::{MOUNT_REPORT_HOLD, MountReport, SettleOnDrop};
use super::network::LabNetwork;
use super::network::nic_segment_name;
use super::plan;
use super::pull_ledger::{
    Cancellation, PullBatch, PullEvent, PullJob, PullLedger, PullOutcome, PullProgress,
};
use super::share_plan;
use super::state::{LabState, SnapshotRecord};
use super::vm::{PowerState, VmDirs, VmInstance};
use crate::config::LabFile;
use crate::profiles::ProfileSet;
use crate::status::{LabStatus, SegmentFrames, SegmentStatus};
use crate::sync::LockRecover;
use crate::template::TemplateStore;
use crate::template::store::Purpose;

mod reload;

pub struct LabRuntime {
    pub name: String,
    pub root: PathBuf,
    pub lab_local: PathBuf,
    /// The lab file as this daemon last loaded it: at start, then at each
    /// reload (PRD §7.1). Read through [`Self::config`], a snapshot a caller
    /// keeps for as long as it needs one consistent declaration.
    config: std::sync::RwLock<Arc<LabFile>>,
    /// Every machine in the lab, and the declaration each was built from. A
    /// reload rebuilds, adds and drops entries; std lock — read for a
    /// lookup, never held across an await.
    machines: std::sync::RwLock<MachineSet>,
    /// What a reload builds a machine as, in a runtime whose machines are
    /// test doubles.
    #[cfg(test)]
    doubles: std::sync::Mutex<Option<reload::MachineDoubles>>,
    /// Held for writing while a reload rebuilds machines, and for reading by
    /// every start across looking its machine up and starting it (PRD §7.1):
    /// a start never boots an instance a reload has just replaced.
    rebuild_gate: tokio::sync::RwLock<()>,
    /// What the running SMB server exports, as [`reload::smb_exports`]
    /// renders it, so a reload can tell whether the file changed them.
    smb_served: std::sync::Mutex<String>,
    pub network: Mutex<LabNetwork>,
    pub state: Mutex<LabState>,
    pub events: Arc<EventLog>,
    /// SMB server for the lab's shares (PRD §7.5); `None` until `up` starts
    /// it (only when some VM declares shares).
    pub smb: Mutex<Option<crate::smb::LabSmb>>,
    /// Forwards installed for each machine — segment `forward {}` blocks
    /// and container `port {}` blocks alike — removed and re-installed when
    /// a restart brings a new lease, so a forward never points at a stale IP.
    /// Each records the port it bound, so an automatic one asks for the same
    /// port again on re-install, and `vmlab status` can say where it is.
    machine_forwards: Mutex<std::collections::HashMap<String, Vec<InstalledForward>>>,
    /// Kept for post-pull re-resolution (deferred templates fold their meta
    /// into the hardware resolution only once pulled).
    profiles: ProfileSet,
    /// Deferred template/image downloads: what is still to fetch and what is
    /// fetching right now (see [`pull_ledger`]). Std lock — the progress
    /// callback is sync, and nothing awaits while it is held.
    pulls: std::sync::Mutex<PullLedger>,
    /// Serialises pull runs (concurrent `up` + `pull` + `vm.start` must not
    /// double-download); the loser re-checks the ledger and no-ops.
    pull_lock: Mutex<()>,
    /// Runs per machine after boot but before any provision script — template
    /// builds install the vmlab-agent here, so it lands even when the last
    /// provision generalizes/shuts the guest down (Windows sysprep). Std
    /// lock: set once before `up`, cloned out, never held across await.
    pub pre_provision: std::sync::RwLock<Option<PreProvisionHook>>,
    /// Whether a provision step waits for its machine to report ready first.
    /// A template build turns this off: its provisions ARE the installer, so
    /// the agent whose handshake readiness means only exists once they have
    /// run — waiting first is a deadlock the machine cannot break. The build
    /// verifies the agent itself either way (`template::build`): blocking
    /// through `pre_provision` for a source that boots an installed OS,
    /// concurrently for one that installs from an ISO.
    pub provisions_wait_ready: std::sync::atomic::AtomicBool,
    /// Whether `up` and `vm start` may refresh a machine's out-of-date guest
    /// agent (§19.4). A template build turns this off: it installs and
    /// verifies the agent itself, and the artefact it seals must carry the
    /// agent the build staged, not one pushed over it mid-build.
    pub agent_updates: std::sync::atomic::AtomicBool,
    /// Machines whose readiness waits on an agent refresh (§19.4), and who
    /// runs it: the machine's own
    /// [`before_ready`](super::machine::LabServices::before_ready) when
    /// nothing in the `up` (or `vm start`) that booted it waits on its agent,
    /// else the `up` that does, with `before_ready` holding readiness until
    /// it is done. Claimed — removed — by whichever comes first, the
    /// machine's `before_ready` or the starter seeing the machine already
    /// ready (or, inline, finishing), so a refresh runs at most once per
    /// start. Only eligible machines are ever entered.
    deferred_refresh: std::sync::Mutex<HashMap<String, OwedRefresh>>,
    /// Host USB passthrough's lab-wide half (§5.2.1): where the host's
    /// devices are read from, the lock serialising attaches, and the
    /// unplug watch.
    pub(super) usb: super::usb::UsbHost,
    /// Where a refresh finds the agent asset this host ships for a machine —
    /// [`agent_repair::shipped_asset`](super::agent_repair::shipped_asset),
    /// replaced only by tests, which must not depend on this host's installed
    /// agent.
    pub(super) shipped_agent: std::sync::RwLock<ShippedAgent>,
    /// Host config loaded once at build (config-weave binary dir, …).
    pub host_cfg: crate::config::host::HostConfig,
    /// In-flight config-weave runs, one per machine (`up` and on-demand
    /// check/apply claim through the same registry).
    pub playbook_ops: crate::labd::playbook::PlaybookOps,
    /// The workspace syncers, one per `@dev` machine carrying a workspace
    /// (§19.6). Owned here rather than by the client that asked for the `up`:
    /// a developer's source tree must not stop syncing because a terminal
    /// closed.
    pub workspaces: crate::labd::workspace::WorkspaceSyncers,
    /// The console recordings this lab owns, one per machine (§11.1).
    pub recordings: super::record::Recordings,
    /// Worktree isolation (§21.4), decided once when the daemon starts:
    /// `Some` for the frozen main lab and every worktree lab built on it.
    pub isolation: Option<super::isolation::IsolationPlan>,
    /// Every machine as the freeze planner compares it against the main
    /// checkout's freeze (§21.2), kept so `status` can plan again and say
    /// whether this daemon's plan still holds.
    freeze_declared: Vec<freeze_clone::Declared>,
    /// Which machines this daemon builds from the main checkout's freeze, as
    /// planned when it was built (§21.2): their MACs and image pins are the
    /// frozen ones already. A clone, once made, is marked built. Held across
    /// a clone, so two starts never clone at once. `None` outside a worktree
    /// lab of a frozen family.
    freeze: Mutex<Option<freeze_clone::FreezePlan>>,
    /// This runtime as its own `Arc`. [`crate::labd::machine::LabServices`] is
    /// a `&self` interface (it has to be, to be object-safe), and the work
    /// behind two of its methods spawns tasks that outlive the call.
    me: std::sync::Weak<LabRuntime>,
}

/// The workspace syncer's route to one machine (§19.6).
///
/// A fresh file session per pass, opened as the machine's **default login** —
/// the one named exception to vmlab's machinery running as the agent identity,
/// because the syncer's whole output is the developer's own source tree, and
/// files it wrote as `SYSTEM` or `root` would leave the developer owning none
/// of it. No flag and no wscript override reaches here: this is not a person
/// invoking a command, so the ladder starts and ends at the declaration.
struct WorkspaceSessions {
    machine: Arc<dyn Machine>,
}

impl WorkspaceSessions {
    /// Who the syncer is: the machine's **default login** (§19.2), the one
    /// named exception to vmlab's machinery running as the agent identity.
    fn logon(&self) -> Result<Option<vmlab_agent_proto::Logon>> {
        crate::labd::identity::resolve(
            self.machine.name(),
            self.machine.logins(),
            self.machine.guest_os(),
            None,
            None,
        )
    }
}

#[async_trait::async_trait]
impl crate::labd::workspace::syncer::GuestSessions for WorkspaceSessions {
    async fn open(&self) -> Result<Box<dyn crate::labd::workspace::guest::GuestFs>> {
        let logon = self.logon()?;
        let agent = self.machine.agent().await?;
        Ok(Box::new(agent.open_fileops(logon).await?))
    }

    /// The one open that carries **no** logon (§19.5): a watcher observes and
    /// produces none of the developer's files, so it runs as the agent
    /// identity — which also makes its coverage a superset of what the login
    /// can traverse, rather than a subtree that silently stops reporting.
    async fn watch(
        &self,
        root: &str,
        prune: Vec<String>,
    ) -> Result<Box<dyn crate::labd::workspace::guest::GuestWatch>> {
        let agent = self.machine.agent().await?;
        Ok(Box::new(agent.open_watch(root.to_string(), prune).await?))
    }

    /// On a Linux-family guest the default root (`/src`) sits under a
    /// root-owned `/`, so the agent identity makes it and hands it to the
    /// login — and on macOS, whose agent is root too, wherever the workspace
    /// was put (BSD `chown` reads `user:` as the owner alone). A Windows login can create `C:\src` itself, and a machine with
    /// no login writes as the agent identity anyway.
    async fn claim_root(&self, root: &str) -> Result<()> {
        if self.machine.guest_os() == crate::labd::guest_os::GuestOs::Windows {
            return Ok(());
        }
        let Some(logon) = self.logon()? else {
            return Ok(());
        };
        let agent = self.machine.agent().await?;
        let argv = [
            "/bin/sh",
            "-c",
            r#"mkdir -p -- "$1" && chown -- "$2:" "$1""#,
            "sh",
            root,
            &logon.user,
        ]
        .map(str::to_string)
        .to_vec();
        let out = agent
            .exec(argv, Vec::new(), None, None, Duration::from_secs(60), None)
            .await?;
        if out.exit_code != 0 {
            anyhow::bail!(
                "creating {root} for {}: exited {}: {}",
                logon.user,
                out.exit_code,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::labd::workspace::windows::GuestRun for WorkspaceSessions {
    /// The Windows preconditions' one command, as the same login the syncer
    /// writes as — because it is that login's `--global` git config a
    /// guest-side checkout will read.
    async fn run(&self, argv: Vec<String>) -> Result<crate::labd::workspace::windows::Ran> {
        let logon = self.logon()?;
        let agent = self.machine.agent().await?;
        let out = agent
            .exec(argv, Vec::new(), None, None, Duration::from_secs(60), logon)
            .await?;
        Ok(crate::labd::workspace::windows::Ran {
            exit_code: out.exit_code,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// See [`LabRuntime::pre_provision`].
pub type PreProvisionHook = Arc<
    dyn Fn(
            Arc<dyn Machine>,
            crate::scripting::OutputSink,
        ) -> futures::future::BoxFuture<'static, Result<()>>
        + Send
        + Sync,
>;

/// Where a refresh finds the agent asset to push — see
/// [`LabRuntime::shipped_agent`].
pub(super) type ShippedAgent =
    Arc<dyn Fn(&dyn Machine) -> Result<crate::agent_asset::AgentAsset> + Send + Sync>;

/// Where a deferred refresh's words go: nobody is reading the `up` or
/// `vm start` that owed it any more.
fn daemon_log(machine: &str) -> crate::scripting::OutputSink {
    let name = machine.to_string();
    Arc::new(move |line: String| {
        tracing::info!(machine = %name, "{}", line.trim_end());
    })
}

/// How long a restore waits for the restored guest's agent before giving up
/// on setting its clock (§7.3). The guest was running when it was captured,
/// so its agent answers within seconds of the load.
const CLOCK_AGENT_WAIT: Duration = Duration::from_secs(60);
/// How long the agent has to answer `set_time`.
pub(super) const CLOCK_SET_WAIT: Duration = Duration::from_secs(10);

/// What an online restore did with the guest clock (§7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClockAfterRestore {
    /// Set to the host's time.
    Set,
    /// Left at capture time: the machine declares `clock = "snapshot"`.
    Kept,
    /// Left alone: the machine declares a fixed clock.
    Fixed,
    /// Should have been set and was not, for this reason.
    NotSet(String),
}

impl ClockAfterRestore {
    /// The `clock` field of the `snapshot.restored` event.
    fn as_str(&self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Kept => "kept",
            Self::Fixed => "fixed",
            Self::NotSet(_) => "not_set",
        }
    }
}

/// What `up`'s join loop heard back: a machine finished starting, or a
/// machine's provisioning finished (§7.2).
enum Joined {
    Started(Result<String>),
    Provisioned(Result<()>),
}

/// A joined task's own result, a panic or abort folded into the error.
fn flatten_join<T>(joined: std::result::Result<Result<T>, tokio::task::JoinError>) -> Result<T> {
    joined.unwrap_or_else(|e| Err(anyhow!("join: {e}")))
}

/// `fut`, abandoned with [`super::provisioning::ProvisionCancelled`] the
/// moment `cancel` fires.
async fn until_cancelled<F: std::future::Future>(
    cancel: &tokio_util::sync::CancellationToken,
    fut: F,
) -> Result<F::Output> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow::Error::new(super::provisioning::ProvisionCancelled)),
        out = fut => Ok(out),
    }
}

/// Fail `up` with `first` (§7.2): abort the machines still starting, cancel
/// every step still running and wait for each to stop, then report the first
/// real failure — never a cancellation it caused.
async fn abandon_up<T: 'static>(
    first: anyhow::Error,
    mut starting: tokio::task::JoinSet<Result<T>>,
    mut provisioning: tokio::task::JoinSet<Result<()>>,
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Error {
    cancel.cancel();
    starting.abort_all();
    while starting.join_next().await.is_some() {}
    let mut held = first;
    while let Some(joined) = provisioning.join_next().await {
        if let Err(e) = flatten_join(joined) {
            held = super::provisioning::first_failure(held, e);
        }
    }
    held
}

/// One machine's share-mount task as `up` started it.
struct ShareMounts {
    machine: String,
    report: Arc<MountReport>,
    task: tokio::task::JoinHandle<()>,
}

/// Wait up to `hold` for every mount task to finish, then say what did not
/// mount (§7.5). A task still running at the deadline keeps running; what it
/// has not settled yet is said as still being tried. A machine whose report
/// a provision or playbook already waited for and printed is not repeated.
async fn report_share_mounts(
    mounts: Vec<ShareMounts>,
    hold: Duration,
    output: &crate::scripting::OutputSink,
) {
    let deadline = tokio::time::Instant::now() + hold;
    for mut m in mounts {
        if !m.report.take_report() {
            continue;
        }
        let finished = tokio::time::timeout_at(deadline, &mut m.task).await.is_ok();
        for line in m.report.lines(&m.machine, finished) {
            output(line);
        }
    }
}

/// The error a cancelled download fails with. A distinct type so the pull
/// paths can tell a cancellation from a transport failure and report the
/// right [`PullOutcome`].
#[derive(Debug)]
struct PullCancelled;

impl std::fmt::Display for PullCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("download cancelled")
    }
}

impl std::error::Error for PullCancelled {}

/// Every machine of the lab file as the freeze planner compares it against
/// the main checkout's freeze (§21.2): what its disk is made from, where it
/// keeps its disks, and which disks and named volumes it uses.
fn freeze_declared(
    config: &LabFile,
    lab: &str,
    lab_local: &std::path::Path,
) -> Vec<freeze_clone::Declared> {
    let vms = config.lab.vms.iter().map(|vm| freeze_clone::Declared {
        name: vm.name.clone(),
        kind: freeze_clone::Kind::Vm,
        source: crate::labd::vm::clone_source(vm),
        local: VmDirs::new(lab, &vm.name, lab_local).local,
        disks: std::iter::once("disk0.qcow2".to_string())
            .chain(
                vm.extra_disks
                    .iter()
                    .map(|d| format!("disk-{}.qcow2", d.name)),
            )
            .collect(),
        volumes: Vec::new(),
    });
    let containers = config
        .lab
        .containers
        .iter()
        .map(|c| freeze_clone::Declared {
            name: c.name.clone(),
            kind: freeze_clone::Kind::Container,
            source: c.image.reference.clone(),
            local: ContainerDirs::new(&config.root, lab, &c.name).local,
            disks: vec!["scratch.qcow2".to_string()],
            volumes: c
                .volumes
                .iter()
                .filter_map(|v| match &v.source {
                    crate::config::model::VolumeSource::Named(n) => Some(n.clone()),
                    crate::config::model::VolumeSource::Host(_) => None,
                })
                .collect(),
        });
    vms.chain(containers).collect()
}

impl LabRuntime {
    pub async fn build(
        config: LabFile,
        events: Arc<EventLog>,
        profiles: &ProfileSet,
    ) -> Result<Arc<LabRuntime>> {
        let name = config.lab.name.clone();
        let root = config.root.clone();
        let lab_local = crate::paths::lab_local_dir(&root);
        std::fs::create_dir_all(&lab_local)?;

        let mut state = LabState::load(&lab_local)?;
        // Before any machine is built: a machine cloned from the main
        // checkout's freeze carries the frozen MACs and image pin (§21.2).
        let freeze_declared = freeze_declared(&config, &name, &lab_local);
        let freeze = freeze_clone::plan(
            &freeze_declared,
            &freeze_clone::observe(&root, &lab_local, &freeze_declared)?,
        );
        let store = TemplateStore::new(crate::paths::template_store_dir());
        let mut network = LabNetwork::build(&config.lab)?;
        // Before any gateway is wired: isolation changes what each NAT
        // engine admits and which global segments the lab joins (§21.4).
        let isolation = super::isolation::decide(&root, &config.lab)?;
        if let Some(plan) = &isolation {
            network.isolate(plan);
        }
        let mut pending: BTreeMap<String, PullJob> = BTreeMap::new();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let host_cfg = crate::config::host::HostConfig::load_default()?;

        let config = Arc::new(config);
        let mtus = network.mtus();
        let mut set = MachineSet::default();
        {
            let cx = BuildContext {
                lab: &name,
                root: &root,
                lab_local: &lab_local,
                profiles,
                store: &store,
                home: home.as_deref(),
                mtus: &mtus,
                freeze: freeze.as_ref(),
                host: &host_cfg,
            };
            for vm_cfg in &config.lab.vms {
                let (vm, pull) = machine_build::build_vm(&cx, vm_cfg, &mut state).await?;
                if let Some(job) = pull {
                    pending.insert(vm_cfg.name.clone(), job);
                }
                set.insert(Built::Vm(vm), config.clone());
            }
            for c_cfg in &config.lab.containers {
                let (container, pull) = machine_build::build_container(&cx, c_cfg, &mut state)?;
                if let Some(job) = pull {
                    pending.insert(c_cfg.name.clone(), job);
                }
                set.insert(Built::Container(container), config.clone());
            }
        }
        state.save(&lab_local)?;

        for (owner, nics) in config
            .lab
            .vms
            .iter()
            .map(|v| (&v.name, &v.nics))
            .chain(config.lab.containers.iter().map(|c| (&c.name, &c.nics)))
        {
            for nic in nics {
                let seg_name = nic_segment_name(nic);
                if network.segment_mut(seg_name).is_none() {
                    bail!("\"{owner}\": nic references unknown segment {seg_name}");
                }
            }
        }

        // Phase 2: gateways with DHCP (reservations from persisted MACs),
        // DNS (auto-registration + statics + sinkholes) per segment. The MAC
        // map spans VMs and containers (one name namespace), so container
        // static IPs and lease-DNS registrations work identically.
        let mut macs_by_vm: std::collections::HashMap<String, Vec<crate::config::model::MacAddr>> =
            state
                .machines
                .iter()
                .map(|(n, m)| (n.clone(), m.macs.clone()))
                .collect();
        // A machine still to be cloned from the freeze reserves the MACs it
        // was built with, not whatever its record here says.
        for m in set.all() {
            if freeze.as_ref().and_then(|f| f.record(m.name())).is_some() {
                macs_by_vm.insert(m.name().to_string(), m.macs().to_vec());
            }
        }
        network.wire_gateways(&config.lab, &macs_by_vm, &host_cfg);

        Ok(Arc::new_cyclic(|me| LabRuntime {
            me: me.clone(),
            name,
            root,
            lab_local,
            config: std::sync::RwLock::new(config),
            machines: std::sync::RwLock::new(set),
            #[cfg(test)]
            doubles: std::sync::Mutex::new(None),
            rebuild_gate: tokio::sync::RwLock::new(()),
            smb_served: std::sync::Mutex::new(String::new()),
            network: Mutex::new(network),
            state: Mutex::new(state),
            events,
            smb: Mutex::new(None),
            machine_forwards: Mutex::new(std::collections::HashMap::new()),
            profiles: profiles.clone(),
            pulls: std::sync::Mutex::new(PullLedger::new(pending)),
            pull_lock: Mutex::new(()),
            pre_provision: std::sync::RwLock::new(None),
            provisions_wait_ready: std::sync::atomic::AtomicBool::new(true),
            agent_updates: std::sync::atomic::AtomicBool::new(true),
            deferred_refresh: std::sync::Mutex::new(HashMap::new()),
            shipped_agent: std::sync::RwLock::new(Arc::new(super::agent_repair::shipped_asset)),
            host_cfg,
            playbook_ops: crate::labd::playbook::PlaybookOps::default(),
            workspaces: crate::labd::workspace::WorkspaceSyncers::default(),
            recordings: super::record::Recordings::default(),
            usb: super::usb::UsbHost::default(),
            isolation,
            freeze_declared,
            freeze: Mutex::new(freeze),
        }))
    }

    /// A runtime whose machines are supplied rather than built from the lab
    /// config — the seam that lets orchestration (waves, readiness gating,
    /// teardown) be driven against doubles, with no hypervisor, template or
    /// network in sight (ADR-0002).
    #[cfg(test)]
    pub(super) fn with_machines(
        config: LabFile,
        machines: Vec<Arc<dyn Machine>>,
    ) -> Result<Arc<LabRuntime>> {
        Self::with_machines_on(config, machines, super::usb::UsbHost::default())
    }

    /// [`Self::with_machines`] reading host USB devices from `usb` — a fake
    /// sysfs tree.
    #[cfg(test)]
    pub(super) fn with_machines_on(
        config: LabFile,
        machines: Vec<Arc<dyn Machine>>,
        usb: super::usb::UsbHost,
    ) -> Result<Arc<LabRuntime>> {
        let name = config.lab.name.clone();
        let root = config.root.clone();
        let lab_local = crate::paths::lab_local_dir(&root);
        std::fs::create_dir_all(&lab_local)?;
        let mut network = LabNetwork::build(&config.lab)?;
        let isolation = super::isolation::decide(&root, &config.lab)?;
        if let Some(plan) = &isolation {
            network.isolate(plan);
        }
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let events = Arc::new(super::events::EventLog::new(&name, tx)?);
        let profiles = ProfileSet::shipped()?;
        let config = Arc::new(config);
        let mut set = MachineSet::default();
        for m in machines {
            set.insert(Built::Double(m), config.clone());
        }
        Ok(Arc::new_cyclic(|me| LabRuntime {
            me: me.clone(),
            name,
            root,
            lab_local: lab_local.clone(),
            config: std::sync::RwLock::new(config),
            machines: std::sync::RwLock::new(set),
            doubles: std::sync::Mutex::new(None),
            rebuild_gate: tokio::sync::RwLock::new(()),
            smb_served: std::sync::Mutex::new(String::new()),
            network: Mutex::new(network),
            state: Mutex::new(LabState::load(&lab_local).unwrap_or_default()),
            events,
            smb: Mutex::new(None),
            machine_forwards: Mutex::new(std::collections::HashMap::new()),
            profiles,
            pulls: std::sync::Mutex::new(PullLedger::new(BTreeMap::new())),
            pull_lock: Mutex::new(()),
            pre_provision: std::sync::RwLock::new(None),
            provisions_wait_ready: std::sync::atomic::AtomicBool::new(true),
            // Off: the real asset lookup reads this host's installed agent,
            // which a test must not depend on. The tests of the refresh turn
            // it on and supply their own asset.
            agent_updates: std::sync::atomic::AtomicBool::new(false),
            deferred_refresh: std::sync::Mutex::new(HashMap::new()),
            shipped_agent: std::sync::RwLock::new(Arc::new(super::agent_repair::shipped_asset)),
            host_cfg: crate::config::host::HostConfig::default(),
            playbook_ops: crate::labd::playbook::PlaybookOps::default(),
            workspaces: crate::labd::workspace::WorkspaceSyncers::default(),
            recordings: super::record::Recordings::default(),
            usb,
            isolation,
            freeze_declared: Vec::new(),
            freeze: Mutex::new(None),
        }))
    }

    /// Download every pending registry template / container image among
    /// `targets` (empty = the whole lab), emitting the same
    /// `template.pull.{start,progress,done,error}` / `container.pull.*`
    /// events the supervisor pre-pull used to stream, so the web UI's
    /// download panel works unchanged (issue #1). Called from `up`, from the
    /// individual start paths, and from the `pull` command — a no-op once
    /// everything is cached, so a fully-cached lab stays offline.
    ///
    /// Serialised by `pull_lock`; the work list is re-read from the ledger
    /// under the lock so a concurrent caller that lost the race finds nothing
    /// left to do. A failed download emits `.error` and fails the caller; the
    /// ledger keeps the job pending for retry.
    pub async fn ensure_pulled(
        self: &Arc<Self>,
        targets: &[String],
        output: Option<&crate::scripting::OutputSink>,
    ) -> Result<()> {
        // Cheap common case: nothing pending anywhere.
        if self.pulls.lock_recover().nothing_pending() {
            return Ok(());
        }
        let _guard = self.pull_lock.lock().await;
        let batches = self.pulls.lock_recover().batches(targets);
        for batch in batches {
            match &batch.job {
                PullJob::Template { .. } => self.pull_template(&batch, output).await?,
                PullJob::Image { .. } => self.pull_image(&batch, output).await?,
            }
        }
        Ok(())
    }

    /// Abort the download `machine` is waiting on; false when it isn't
    /// downloading (queued but not started, already finished, or never
    /// declared). The ledger keeps the job pending, so a later `up`/`pull`
    /// retries from scratch — chunks already fetched live in the store's
    /// `.oci-pull` work dir and are cleaned up by the next successful pull.
    ///
    /// Cancelling only interrupts the download, and takes down every machine
    /// sharing it. The final assemble+install runs on a blocking thread that
    /// cannot be interrupted, so a cancel that lands during install still
    /// leaves a fully verified template behind.
    pub fn cancel_pull(&self, machine: &str) -> bool {
        let ledger = self.pulls.lock_recover();
        match ledger.cancel(machine) {
            Cancellation::Active { handle, .. } => {
                handle.abort();
                true
            }
            Cancellation::Pending | Cancellation::Unknown => false,
        }
    }

    fn emit_pulls(&self, events: Vec<PullEvent>) {
        for e in events {
            self.events.emit(&e.name, e.payload);
        }
    }

    /// Keep the ledger's progress snapshot current (`status` resyncs the web
    /// UI from it) and stream the progress event.
    fn note_pull_progress(&self, batch: &PullBatch, progress: PullProgress) {
        let events = self.pulls.lock_recover().progress(batch, progress);
        self.emit_pulls(events);
    }

    /// Retire a batch in the ledger and emit whatever it says about it.
    fn finish_pull(&self, batch: &PullBatch, outcome: PullOutcome) {
        let events = self.pulls.lock_recover().finish(batch, outcome);
        self.emit_pulls(events);
    }

    /// Register `task` as `batch`'s download, announce it, and await it. An
    /// abort from [`Self::cancel_pull`] surfaces as [`PullCancelled`], so
    /// whatever needed the download fails with a reason the caller can show.
    async fn join_pull<T>(
        self: &Arc<Self>,
        batch: &PullBatch,
        task: tokio::task::JoinHandle<Result<T>>,
    ) -> Result<T> {
        let started = self.pulls.lock_recover().begin(batch, task.abort_handle());
        self.emit_pulls(started);
        match task.await {
            Ok(result) => result,
            Err(e) if e.is_cancelled() => Err(anyhow::Error::new(PullCancelled)),
            Err(e) => bail!("{} download task: {e}", batch.job.kind()),
        }
    }

    /// How a download's error maps onto the ledger: a cancellation already
    /// knows what it is, anything else is a transport failure to report.
    fn pull_outcome(e: &anyhow::Error) -> PullOutcome {
        if e.is::<PullCancelled>() {
            PullOutcome::Cancelled
        } else {
            PullOutcome::Failed(format!("{e:#}"))
        }
    }

    /// Pull one registry template, then bind the resolved parts (hardware
    /// re-resolution with the template meta, backing disk, first-boot script)
    /// into every VM instance waiting on it.
    async fn pull_template(
        self: &Arc<Self>,
        batch: &PullBatch,
        output: Option<&crate::scripting::OutputSink>,
    ) -> Result<()> {
        let store = TemplateStore::new(crate::paths::template_store_dir());
        let reference = batch.job.reference().to_string();
        let arch = batch.job.arch().to_string();
        if let Some(out) = output {
            out(format!("pull: {reference} ({arch})\n"));
        }
        // The batch is keyed by the bare reference; it fetches every variant
        // its machines name between them (§6.6).
        let mut want = crate::oci::client::VariantWant::default();
        for vm_name in &batch.machines {
            if let Some(vm_cfg) = self.config().lab.vms.iter().find(|v| &v.name == vm_name) {
                want.add(vm_cfg.template.variant());
            }
        }
        // The download runs as its own task so `cancel_pull` can abort it.
        let me = Arc::clone(self);
        let owned = batch.clone();
        let ref_s = reference.clone();
        let arch_s = arch.clone();
        let task = tokio::spawn(async move {
            let mut progress = |p: crate::oci::PullProgress| {
                me.note_pull_progress(
                    &owned,
                    PullProgress {
                        unit: p.chunk,
                        units: p.chunks,
                        bytes_done: p.bytes_done,
                        bytes_total: p.bytes_total,
                    },
                );
            };
            crate::oci::ensure_registry_template(&ref_s, &arch_s, &store, &want, &mut progress)
                .await
        });
        match self.join_pull(batch, task).await {
            Ok(resolved) => match self.bind_template(batch, &resolved) {
                Ok(()) => {
                    self.finish_pull(batch, PullOutcome::Done);
                    if let Some(out) = output {
                        out(format!("pull: {reference} done\n"));
                    }
                    Ok(())
                }
                Err(e) => {
                    self.finish_pull(batch, Self::pull_outcome(&e));
                    Err(e)
                }
            },
            Err(e) => {
                self.finish_pull(batch, Self::pull_outcome(&e));
                Err(e.context(format!(
                    "pulling template for vm(s) {}",
                    batch.machines.join(", ")
                )))
            }
        }
    }

    /// Bind a freshly pulled template into each VM waiting on it: the
    /// hardware re-resolves against the template meta the pull just made
    /// available.
    ///
    /// The pull fetched a version; each machine selects its own variant of it
    /// (§6.6), since the batch is keyed by the bare registry reference.
    fn bind_template(
        &self,
        batch: &PullBatch,
        version: &crate::template::store::TemplateVersion,
    ) -> Result<()> {
        for vm_name in &batch.machines {
            // Against the declaration the waiting instance was built from,
            // which a reload may have left behind the lab's (§7.1); a machine
            // a reload dropped has nothing waiting.
            let Some(from) = self
                .machines
                .read()
                .expect("machine set lock")
                .built_from(vm_name)
            else {
                continue;
            };
            let Some(vm_cfg) = from.lab.vms.iter().find(|v| &v.name == vm_name) else {
                continue;
            };
            let resolved = version
                .select(vm_cfg.template.variant(), Purpose::Lab)
                .with_context(|| format!("vm \"{vm_name}\""))?;
            let resolved_vm =
                crate::qemu::resolve_vm(vm_cfg, Some(&resolved.meta), &self.profiles)?;
            // kind-aware: binding a downloaded template needs the concrete
            // type that holds one. Not a decision — this path only ever runs
            // for a machine whose artefact is a template.
            self.vm(vm_name)?.set_template(super::vm::TemplateParts {
                resolved: resolved_vm,
                backing: Some(resolved.disk_path.clone()),
                disk_size: None,
                first_boot: resolved.meta.first_boot(),
                agent_version: resolved.meta.agent_version.clone(),
                variant: resolved.variant.clone(),
                template_vars: crate::template::store::template_vars(&resolved.dir, &resolved.meta),
            });
        }
        Ok(())
    }

    /// Pull one container image, pin its digest into the lab state, and bind
    /// it (re-merging the cinit spec) into every container waiting on it.
    async fn pull_image(
        self: &Arc<Self>,
        batch: &PullBatch,
        output: Option<&crate::scripting::OutputSink>,
    ) -> Result<()> {
        let cache = crate::oci::image::ImageCache::new(crate::paths::oci_cache_dir());
        let reference = batch.job.reference().to_string();
        let arch = batch.job.arch().to_string();
        if let Some(out) = output {
            out(format!("pull: {reference}\n"));
        }
        // As in `pull_template`: a task of its own, so it can be cancelled.
        let me = Arc::clone(self);
        let owned = batch.clone();
        let ref_s = reference.clone();
        let arch_s = arch.clone();
        let task = tokio::spawn(async move {
            let mut progress = |p: crate::oci::image::ImagePullProgress| {
                me.note_pull_progress(
                    &owned,
                    PullProgress {
                        unit: p.layer,
                        units: p.layers,
                        bytes_done: p.bytes_done,
                        bytes_total: p.bytes_total,
                    },
                );
            };
            crate::oci::image::ensure_container_image(&ref_s, &arch_s, &cache, &mut progress).await
        });
        match self.join_pull(batch, task).await {
            Ok(image) => match self.bind_image(batch, image).await {
                Ok(()) => {
                    self.finish_pull(batch, PullOutcome::Done);
                    if let Some(out) = output {
                        out(format!("pull: {reference} done\n"));
                    }
                    Ok(())
                }
                Err(e) => {
                    self.finish_pull(batch, Self::pull_outcome(&e));
                    Err(e)
                }
            },
            Err(e) => {
                self.finish_pull(batch, Self::pull_outcome(&e));
                Err(e.context(format!(
                    "pulling image for container(s) {}",
                    batch.machines.join(", ")
                )))
            }
        }
    }

    /// Pin a freshly pulled image's digest into the lab state and bind it
    /// into each container waiting on it.
    async fn bind_image(
        &self,
        batch: &PullBatch,
        image: crate::oci::image::PulledImage,
    ) -> Result<()> {
        for name in &batch.machines {
            // kind-aware: as in `bind_template` — binding a downloaded image
            // needs the concrete type that holds one; one a reload dropped
            // has nothing waiting.
            let Ok(container) = self.container(name) else {
                continue;
            };
            {
                let mut state = self.state.lock().await;
                let c_state = state.machine_mut(name);
                c_state.image_digest = Some(image.manifest_digest.clone());
                c_state.image_ref = Some(container.cfg.image.reference.clone());
                state.save(&self.lab_local)?;
            }
            container.set_image(image.clone());
        }
        Ok(())
    }

    /// Start the SMB server for the lab's shares — VM `share {}` blocks and
    /// container volumes that fall back to CIFS (PRD §18: volumes ride
    /// virtiofs when the host has a virtiofsd, smbd otherwise) — and DNAT
    /// each relevant segment gateway's port 445 to it (PRD §7.5).
    ///
    /// Which shares those are, which segments need the rule and which host
    /// port the server takes are all decided first, as a [`share_plan`]. This
    /// only carries it out. Best-effort: a failure is logged and the rest of
    /// the lab still works. Idempotent; called from `up` and from any
    /// individual container start.
    async fn ensure_smb(self: &Arc<Self>, output: &crate::scripting::OutputSink) {
        if self.smb.lock().await.is_some() {
            return; // already serving
        }
        let plan = match self.share_plan().await {
            Ok(plan) => plan,
            Err(e) => {
                tracing::warn!("share plan: {e}");
                output(format!("WARNING: shares will not mount: {e}\n"));
                self.events
                    .emit("smb.failed", json!({"error": e.to_string()}));
                return;
            }
        };
        for skip in &plan.skipped {
            tracing::warn!("{}: {}", skip.what, skip.why);
            output(format!("WARNING: {}: {}\n", skip.what, skip.why));
        }
        for warning in &plan.warnings {
            tracing::warn!("{warning}");
            output(format!("WARNING: {warning}\n"));
        }
        // "Why did my share not arrive over virtiofs?" is a support question,
        // and the plan is the only place that knows.
        for placed in plan.placements() {
            tracing::debug!(
                "share {}/{} rides {:?}",
                placed.machine,
                placed.share,
                placed.transport
            );
        }
        let Some(smb) = plan.smb else {
            return; // everything rides virtiofs, or there is nothing to share
        };

        let sharing: Vec<(String, std::net::Ipv4Addr, Vec<crate::config::model::Share>)> = smb
            .exports
            .iter()
            .map(|e| (e.machine.clone(), e.gateway, e.shares.clone()))
            .collect();
        // Work down the plan's free ports: one can be taken between the
        // probe and the bind, and smbd can fail for reasons that have nothing
        // to do with the port at all.
        let mut started = None;
        let mut last_err = String::new();
        for port in &smb.host_ports {
            let mut candidate =
                crate::smb::LabSmb::plan(&self.name, &self.lab_local, *port, &sharing);
            let config = candidate.build_config();
            match candidate.spawn(config) {
                Ok(p) => {
                    started = Some((candidate, p));
                    break;
                }
                Err(e) => {
                    tracing::warn!("smbd on port {port} failed: {e}");
                    last_err = e.to_string();
                }
            }
        }
        let Some((labsmb, port)) = started else {
            tracing::warn!("SMB server failed to start: {last_err}");
            output(format!(
                "WARNING: SMB server failed to start — shares will not mount: {last_err}\n"
            ));
            self.events.emit("smb.failed", json!({"error": last_err}));
            return;
        };
        tracing::info!("SMB server for lab {} on 127.0.0.1:{port}", self.name);
        output(format!(
            "smb: serving shares on 127.0.0.1:{port} (guest mounts \\\\<gateway>\\<share>; credentials in .vmlab/smb/creds)\n"
        ));
        self.events.emit("smb.started", json!({"port": port}));

        // DNAT gateway:445 → 127.0.0.1:smbd on each sharing segment, so a
        // guest mounting \\<gateway>\<share> reaches the local smbd. A
        // segment without `nat` gets no egress, but still gets this.
        {
            let net = self.network.lock().await;
            for seg_name in &smb.gateway_segments {
                if let Some(seg) = net.segments.get(seg_name)
                    && let Some(services) = &seg.services
                    && let Ok(mut rs) = services.rules.lock()
                {
                    use crate::config::model::{HostPort, RedirectRule};
                    rs.add_redirect(RedirectRule {
                        from: HostPort {
                            ip: seg.service_ip,
                            port: Some(445),
                        },
                        to: HostPort {
                            ip: std::net::Ipv4Addr::LOCALHOST,
                            port: Some(labsmb.listen_port()),
                        },
                        proto: None,
                        span: (0, 0),
                    });
                    drop(rs);
                    services.expose_host_service(labsmb.listen_port());
                }
            }
        }

        // Hand each machine whose guest mounts for itself its coordinates.
        // A machine driven from the host instead ignores this.
        *self.smb_served.lock_recover() = reload::smb_exports(&smb);
        for (name, gateway) in smb.volume_gateways {
            let Some(creds) = labsmb.credentials(&name) else {
                continue;
            };
            if let Ok(m) = self.machine(&name) {
                m.smb_ready(gateway, &creds.username, &creds.password).await;
            }
        }

        *self.smb.lock().await = Some(labsmb);
    }

    /// Whether this lab's host can serve virtiofs, asked through the same
    /// [`Hypervisor`](super::hypervisor::Hypervisor) the machines start
    /// against rather than probed again here — every machine in a lab runs on
    /// one host, so any of them answers for all. A lab with no machines has
    /// no shares either, so the fallback never decides anything.
    fn host_virtiofsd(&self) -> Option<crate::qemu::virtiofsd::Virtiofsd> {
        self.machines()
            .next()
            .map_or_else(crate::qemu::virtiofsd::found, |m| m.virtiofsd())
    }

    /// Where every share in this lab goes, computed against the host as it is
    /// right now: the segment gateways the network assembly wired, each VM's
    /// resolved virtiofs capability, and whether the host has a virtiofsd and
    /// a free port at all.
    async fn share_plan(&self) -> Result<share_plan::SharePlan, share_plan::SharePlanError> {
        let gateways: BTreeMap<String, std::net::Ipv4Addr> = {
            let net = self.network.lock().await;
            net.segments
                .iter()
                .map(|(name, seg)| (name.clone(), seg.service_ip))
                .collect()
        };
        // The resolved profile (which folds in template metadata) says whether
        // the guest mounts virtiofs natively.
        let vms = self.machines.read().expect("machine set lock").vms();
        let guest_virtiofs: BTreeMap<String, bool> = vms
            .iter()
            .map(|(name, vm)| (name.clone(), vm.template().resolved.virtiofs))
            .collect();
        let windows_guests: BTreeSet<String> = vms
            .iter()
            .filter(|(_, vm)| {
                crate::smb::guest_os_hint(vm.template().resolved.profile.as_deref())
                    == crate::smb::OsHint::Windows
            })
            .map(|(name, _)| name.clone())
            .collect();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        share_plan::plan(
            &share_plan::ShareInputs {
                lab: &self.config().lab,
                root: &self.root,
                home: home.as_deref(),
                host_virtiofsd: share_plan::HostVirtiofsd::of(self.host_virtiofsd().as_ref()),
                guest_virtiofs: &guest_virtiofs,
                windows_guests: &windows_guests,
                gateways: &gateways,
            },
            &share_plan::BindProbe,
        )
    }

    // ---- BEGIN kind-aware accessors (ADR-0002 exemption) --------------------
    //
    // The two places the lab runtime is allowed to name a kind, and the reason
    // `orchestration_never_branches_on_machine_kind` skips this block:
    //
    //   * they exist *to* reject the other kind's name, so the error a user
    //     reads says "that's a container" rather than "no such machine";
    //   * a deferred pull has to reach a concrete type to bind the template or
    //     image it just downloaded, and the wscript `poweroff` rung needs a
    //     VM's QMP channel.
    //
    // Nothing here decides *behaviour* by kind. If a caller wants one of these
    // to pick a code path, the difference belongs on `Machine` instead.

    pub fn vm(&self, name: &str) -> Result<Arc<VmInstance>> {
        let set = self.machines.read().expect("machine set lock");
        set.vm(name).ok_or_else(|| {
            if set.container(name).is_some() {
                anyhow!("\"{name}\" is a container — use `vmlab container ...`")
            } else {
                anyhow!("no vm \"{name}\" in lab \"{}\"", self.name)
            }
        })
    }

    pub fn container(&self, name: &str) -> Result<Arc<ContainerInstance>> {
        let set = self.machines.read().expect("machine set lock");
        set.container(name).ok_or_else(|| {
            if set.vm(name).is_some() {
                anyhow!("\"{name}\" is a vm — use `vmlab vm ...`")
            } else {
                anyhow!("no container \"{name}\" in lab \"{}\"", self.name)
            }
        })
    }

    // ---- END kind-aware accessors -------------------------------------------

    /// The lab file as this daemon last loaded it (PRD §7.1). A snapshot: a
    /// reload swaps in a new one without disturbing whoever holds this.
    pub fn config(&self) -> Arc<LabFile> {
        self.config.read().expect("config lock").clone()
    }

    /// One machine by name — the only accessor orchestration needs. Machine
    /// names are unique across both kinds, so there is one namespace to look
    /// in.
    pub fn machine(&self, name: &str) -> Result<Arc<dyn Machine>> {
        self.machines
            .read()
            .expect("machine set lock")
            .get(name)
            .ok_or_else(|| anyhow!("no vm or container \"{name}\" in lab \"{}\"", self.name))
    }

    /// Every machine in the lab, in name order, as the set held them when
    /// asked.
    pub fn machines(&self) -> impl Iterator<Item = Arc<dyn Machine>> + use<> {
        self.machines
            .read()
            .expect("machine set lock")
            .all()
            .into_iter()
    }

    /// This runtime as the narrow interface a machine boots against.
    fn services(&self) -> Arc<dyn crate::labd::machine::LabServices> {
        self.arc() as Arc<dyn crate::labd::machine::LabServices>
    }

    /// This runtime's own `Arc`. Infallible in practice: every path that can
    /// reach a `LabRuntime` reached it through one.
    fn arc(&self) -> Arc<Self> {
        self.me.upgrade().expect("lab runtime outlived its Arc")
    }

    /// Something in the lab waits on this machine's readiness.
    fn has_dependents(&self, name: &str) -> bool {
        self.config()
            .lab
            .machines()
            .any(|m| m.depends_on().iter().any(|d| d == name))
    }

    /// Verify the external binaries starting `targets` will need are on PATH
    /// (the per-arch QEMU emulator, `qemu-img` for clones, `swtpm` when a VM
    /// wants a TPM), so a missing package surfaces as one clear error before
    /// any clone or boot work begins instead of a spawn failure mid-`up`.
    pub fn preflight_binaries(&self, targets: &[String]) -> Result<()> {
        let mut needed: Vec<String> = Vec::new();
        for name in targets {
            for binary in self.machine(name)?.required_binaries() {
                if !needed.contains(&binary) {
                    needed.push(binary);
                }
            }
        }
        let missing: Vec<String> = needed
            .into_iter()
            .filter(|b| !crate::qemu::process::binary_on_path(b))
            .collect();
        if !missing.is_empty() {
            bail!(
                "missing required binaries on PATH: {} — install the QEMU/swtpm \
                 packages (PRD §14 lists the runtime dependencies)",
                missing.join(", ")
            );
        }
        self.preflight_playbooks(targets)
    }

    /// The config-weave guest binaries every playbook-targeted machine in
    /// `targets` will need must exist on the host before anything boots.
    /// Also the runtime arch gate for machines whose arch validation could
    /// not see statically (registry templates, containers). A block naming
    /// a guest-resident `guest_bin` needs no host binary, so a machine whose
    /// every block does is not checked.
    fn preflight_playbooks(&self, targets: &[String]) -> Result<()> {
        use crate::labd::playbook;
        let lab = &self.config().lab;
        let dir = playbook::default_bin_dir(self.host_cfg.config_weave_bin_dir.as_deref());
        let mut errs: Vec<String> = Vec::new();
        for name in targets {
            let targeted = playbook::playbooks_of(lab, name)
                .is_some_and(|p| p.iter().any(|pb| pb.guest_bin.is_none()));
            if !targeted {
                continue;
            }
            let m = self.machine(name)?;
            if let Err(e) = playbook::weave_binary(&dir, m.guest_os(), &m.arch()) {
                let msg = format!("\"{name}\": {e}");
                if !errs.contains(&msg) {
                    errs.push(msg);
                }
            }
        }
        if !errs.is_empty() {
            bail!("playbook preflight: {}", errs.join("; "));
        }
        Ok(())
    }

    /// Boot one machine, whichever kind it is.
    pub async fn start_machine(self: &Arc<Self>, name: &str) -> Result<()> {
        self.settle_pending(name).await?;
        self.clone_from_freeze(name).await?;
        // A download is the one long wait a start can make: outside the
        // gate, so a reload is not held up behind it (§7.1).
        self.ensure_pulled(&[name.to_string()], None).await?;
        let _gate = self.rebuild_gate.read().await;
        self.machine(name)?.start(self.services()).await
    }

    /// Build `name` from the main checkout's freeze when this daemon planned
    /// it so and it has no disk yet (§21.2), and copy in the frozen named
    /// volumes it mounts that this lab has not got. Refuses when the freeze
    /// it would come from cannot be read; never falls back to the template.
    /// A no-op for every other machine, and for this one once it is built.
    ///
    /// Before the machine's first boot, so that boot finds a disk to keep,
    /// and the copies a fresh VM would otherwise make from its firmware's
    /// templates already in place.
    async fn clone_from_freeze(self: &Arc<Self>, name: &str) -> Result<()> {
        let mut guard = self.freeze.lock().await;
        let Some(plan) = guard.as_mut() else {
            return Ok(());
        };
        match plan.machines.get(name) {
            Some(freeze_clone::Source::Refused(why)) => {
                bail!("\"{name}\" cannot be built: {why}")
            }
            Some(freeze_clone::Source::Freeze(clone)) => {
                let clone = clone.as_ref().clone();
                self.make_clone(name, &clone, plan).await?;
                plan.machines
                    .insert(name.to_string(), freeze_clone::Source::Built);
            }
            _ => {}
        }
        let (copy, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut plan.volumes)
            .into_iter()
            .partition(|v| v.used_by.iter().any(|m| m == name));
        plan.volumes = keep;
        for v in copy {
            if !v.copy.to.exists() {
                v.run()
                    .await
                    .with_context(|| format!("copying volume \"{}\" from the freeze", v.name))?;
            }
        }
        Ok(())
    }

    /// Execute one machine's part of the freeze plan. Its record is merged
    /// into the lab's state before its primary disk exists, because the
    /// primary disk is what marks it built: interrupted anywhere, the next
    /// `up` plans the same clone again and finishes it.
    async fn make_clone(
        &self,
        name: &str,
        clone: &freeze_clone::MachineClone,
        plan: &freeze_clone::FreezePlan,
    ) -> Result<()> {
        let m = self.machine(name)?;
        clone.stage().await.with_context(|| {
            format!(
                "\"{name}\": cloning from freeze generation {} of {}",
                plan.generation,
                plan.main_root.display()
            )
        })?;
        // An `@dev` machine's guest tree is the main checkout's, as of the
        // freeze: the re-seed carries this worktree's tree in before its
        // watch opens (§19.6), exactly as after a restore.
        if let Some(workspace) = self.workspace_of(name) {
            crate::labd::workspace::ledger::Ledger::mark_rewound(
                &workspace.ledger_path,
                &workspace.host_root,
                &workspace.guest_root,
            )
            .with_context(|| format!("\"{name}\": noting the re-seed its workspace owes"))?;
        }
        {
            let mut state = self.state.lock().await;
            let mut record = clone.record.clone();
            // The MACs this machine was built with: the frozen ones, unless
            // the lab file pins a NIC's MAC itself.
            record.macs = m.macs().to_vec();
            state.machines.insert(name.to_string(), record);
            state.save(&self.lab_local)?;
        }
        clone.finish().await.with_context(|| {
            format!(
                "\"{name}\": cloning from freeze generation {} of {}",
                plan.generation,
                plan.main_root.display()
            )
        })?;
        self.events.emit(
            "machine.cloned_from_freeze",
            json!({
                "machine": name,
                "generation": plan.generation,
                "snapshot": plan.snapshot,
                "main": plan.main_root.display().to_string(),
            }),
        );
        Ok(())
    }

    /// Whether this daemon would build some machine differently, or decide
    /// worktree isolation differently, if it were built now (§21.2, §21.4):
    /// the main checkout was frozen, refrozen or thawed since, or a machine
    /// has lost its disk. A boot verb replaces such a daemon first, as it
    /// does one running an edited lab file. Isolation is decided once, when
    /// the network is assembled, and a freeze or thaw runs inside the main
    /// lab's own daemon, so without this the main lab would keep running
    /// unisolated after its freeze.
    fn freeze_stale(&self, planned: Option<&freeze_clone::FreezePlan>) -> bool {
        let now = freeze_clone::observe(&self.root, &self.lab_local, &self.freeze_declared)
            .ok()
            .and_then(|o| freeze_clone::plan(&self.freeze_declared, &o));
        // An unreadable `worktree.json` counts as stale: the rebuild then
        // fails loudly rather than this daemon running on its old decision.
        let isolation_moved = super::isolation::decide(&self.root, &self.config().lab)
            .map_or(true, |now| now != self.isolation);
        freeze_clone::stale(planned, now.as_ref()) || isolation_moved
    }

    /// Stop one machine, finalising its recording first (D6: unlike the
    /// workspace syncer, a recording ends with the machine it shows).
    pub async fn stop_machine(self: &Arc<Self>, name: &str, force: bool) -> Result<()> {
        let m = self.machine(name)?;
        self.record_finalise_machine(name, "machine stopped").await;
        m.stop(force).await
    }

    /// Stop one machine and delete everything it materialised, leaving the
    /// rest of the lab running. It stays in the lab config, so a later
    /// `up <name>` re-materialises it (per-machine analogue of [`destroy`]).
    pub async fn destroy_machine(self: &Arc<Self>, name: &str) -> Result<()> {
        let m = self.machine(name)?;
        self.workspaces.stop(name).await;
        self.record_finalise_machine(name, "machine destroyed")
            .await;
        m.stop(true).await?;
        // Settle before removing disks out from under a still-running QEMU.
        wait_settled(&*m).await;
        remove_tree(m.local_dir()).await?;
        let _ = remove_tree(m.run_dir()).await;
        // The guest tree goes with the clone, so there is nothing left for
        // the ledger to have agreed with (§19.6). Leaving it would let a
        // re-materialised machine start from an agreement about a tree that
        // no longer exists.
        let _ = std::fs::remove_file(crate::labd::workspace::ledger::Ledger::path(
            &self.lab_local,
            name,
        ));
        {
            let mut state = self.state.lock().await;
            let entry = state.machine_mut(name);
            // The disks a repair changed in place are gone, so the divergence
            // they carried goes with them: what comes back is the artefact's
            // own agent again (§19.4).
            entry.repaired_agent = None;
            m.forget_artefacts(entry).await;
            state.save(&self.lab_local)?;
        }
        self.events.emit(
            &format!("{}.destroyed", m.event_subject()),
            json!({ m.event_subject(): name.to_string() }),
        );
        Ok(())
    }

    /// Record that a machine now runs an agent this host pushed into it, which
    /// is what makes it a **diverged machine** (§19.4).
    ///
    /// Persisted, because divergence outlives this daemon and the machine's
    /// next boot: the template's sealed `agent_version` no longer describes
    /// what is inside the clone, and every surface reporting that machine's
    /// state says so until its disks are destroyed.
    pub async fn record_agent_repair(&self, machine: &str, agent_version: &str) -> Result<()> {
        self.mark_diverged(machine, agent_version).await?;
        self.events.emit(
            "machine.agent_repaired",
            json!({"vm": machine, "machine": machine, "agent_version": agent_version}),
        );
        Ok(())
    }

    /// The one record both a repair and `up`'s refresh leave: `machine` now
    /// runs `agent_version`, pushed by this host, and is diverged until its
    /// disks are destroyed.
    async fn mark_diverged(&self, machine: &str, agent_version: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        state.machine_mut(machine).repaired_agent = Some(agent_version.to_string());
        state.save(&self.lab_local)
    }

    /// `up`'s agent refresh (§19.4): once `m`'s agent has answered and before
    /// any provision runs, push the host's shipped agent where the stamps
    /// differ, and mark the machine diverged exactly as the repair verb does.
    ///
    /// **Never fails the caller.** A machine that is not eligible — opted
    /// out, a host-shipped agent, the legacy tier, nothing sealed — is passed
    /// over silently, as is one with no asset to push; a refresh that fails
    /// says `warning:` with the machine and the reason, emits
    /// `machine.agent_updated` carrying the error, and `up` goes on. The push
    /// stages the binary beside the installed one, so a failure before the
    /// swap leaves the old agent installed and running.
    pub async fn refresh_agent(
        self: &Arc<Self>,
        m: &Arc<dyn Machine>,
        output: &crate::scripting::OutputSink,
    ) {
        let shipped = self
            .shipped_agent
            .read()
            .expect("shipped_agent lock")
            .clone();
        self.refresh_agent_with(m, output, |m: &dyn Machine| shipped(m))
            .await
    }

    /// Start `m` with its agent refresh deferred to its handshake, so the
    /// caller waits on nothing it did not wait on before (§19.4). A machine
    /// that never answers — a guest that cannot — is never refreshed and
    /// never waited for.
    ///
    /// The refresh runs between the handshake and readiness (the machine's
    /// [`before_ready`](super::machine::LabServices::before_ready)), so the
    /// machine is not reported ready — and nothing waiting on its readiness
    /// proceeds — while its agent is being swapped and restarted. A machine
    /// that is not eligible is never entered, so its readiness waits on
    /// nothing; one that is eligible but already current is held only for the
    /// one handshake it takes to find out.
    pub async fn start_with_deferred_refresh(self: &Arc<Self>, name: &str) -> Result<()> {
        self.settle_pending(name).await?;
        let name = name.to_string();
        if self.refresh_could_be_owed(self.machine(&name)?.as_ref()) {
            self.deferred_refresh
                .lock_recover()
                .insert(name.clone(), OwedRefresh::AtHandshake);
        }
        self.start_machine(&name).await?;
        let m = &self.machine(&name)?;
        // Already running and ready, so no handshake is coming: a machine
        // other callers are already using is refreshed beside them, in the
        // background, as `vm start` on a running machine always was.
        if m.is_ready().await && self.deferred_refresh.lock_recover().remove(&name).is_some() {
            let me = self.arc();
            let m = Arc::clone(m);
            tokio::spawn(async move { me.refresh_agent(&m, &daemon_log(&name)).await });
        }
        Ok(())
    }

    /// Whether a refresh of `m` is worth deferring at all — everything that
    /// can be decided without asking its agent: refreshes on, and the machine
    /// eligible (§19.4). The rest — the asset, the handshake, the stamps —
    /// is decided at the handshake.
    fn refresh_could_be_owed(&self, m: &dyn Machine) -> bool {
        self.agent_updates
            .load(std::sync::atomic::Ordering::Relaxed)
            && super::agent_repair::eligible(m, self.config().lab.agent_update_for(m.name()))
                .is_ok()
    }

    /// Hold `m`'s readiness for the agent refresh the caller is about to run
    /// inline (§19.4), when one could be owed: `up` refreshes a machine it
    /// waits on itself, so what the refresh says reaches `up`'s output, and
    /// the machine's handshake must not report it ready while that refresh
    /// is restarting its agent. The hold ends when the returned sender is
    /// dropped — once the refresh is done, or with the task that was running
    /// it — and [`release_inline_refresh`](Self::release_inline_refresh)
    /// clears what the handshake did not claim.
    fn hold_for_inline_refresh(&self, m: &dyn Machine) -> Option<tokio::sync::oneshot::Sender<()>> {
        if !self.refresh_could_be_owed(m) {
            return None;
        }
        let (done, held) = tokio::sync::oneshot::channel();
        self.deferred_refresh
            .lock_recover()
            .insert(m.name().to_string(), OwedRefresh::Inline(held));
        Some(done)
    }

    /// The inline refresh behind a
    /// [`hold_for_inline_refresh`](Self::hold_for_inline_refresh) is done:
    /// readiness may go ahead, and a hold the handshake never claimed — a
    /// machine that was already ready — is not left for its next start.
    fn release_inline_refresh(
        &self,
        machine: &str,
        done: Option<tokio::sync::oneshot::Sender<()>>,
    ) {
        let Some(done) = done else { return };
        drop(done);
        let mut owed = self.deferred_refresh.lock_recover();
        if matches!(owed.get(machine), Some(OwedRefresh::Inline(_))) {
            owed.remove(machine);
        }
    }

    /// Settle the refresh owed to `machine`'s handshake, if one is, and return
    /// once readiness may be reported: when the refresh has finished,
    /// succeeded or failed, or after [`REFRESH_HOLD`] at the latest, the
    /// refresh then carrying on in the background. One owed to the handshake
    /// is run here; nobody is reading an `up`'s output by now, so what it says
    /// goes to the daemon log, and the event and the diverged mark are the
    /// same as an inline refresh's. One `up` is running inline is waited for.
    async fn run_deferred_refresh(&self, machine: &str) {
        let Some(owed) = self.deferred_refresh.lock_recover().remove(machine) else {
            return;
        };
        let finished = match owed {
            OwedRefresh::AtHandshake => {
                let me = self.arc();
                let Ok(m) = me.machine(machine) else {
                    return;
                };
                let log = daemon_log(machine);
                let refresh = tokio::spawn(async move { me.refresh_agent(&m, &log).await });
                tokio::time::timeout(REFRESH_HOLD, refresh).await.is_ok()
            }
            // Resolves when the inline refresh is done, or its task is gone.
            OwedRefresh::Inline(held) => tokio::time::timeout(REFRESH_HOLD, held).await.is_ok(),
        };
        if !finished {
            tracing::warn!(
                machine,
                "agent refresh still running after {}s; reporting the machine ready anyway",
                REFRESH_HOLD.as_secs()
            );
        }
    }

    /// [`refresh_agent`](Self::refresh_agent) with the asset lookup supplied,
    /// which is the seam the tests run through.
    pub(super) async fn refresh_agent_with(
        self: &Arc<Self>,
        m: &Arc<dyn Machine>,
        output: &crate::scripting::OutputSink,
        shipped: impl Fn(&dyn Machine) -> Result<crate::agent_asset::AgentAsset>,
    ) {
        use super::agent_repair::{Refresh, decide, eligible, push_under};
        if !self
            .agent_updates
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let name = m.name().to_string();
        if let Err(why) = eligible(m.as_ref(), self.config().lab.agent_update_for(&name)) {
            tracing::debug!(machine = %name, ?why, "agent refresh: not eligible");
            return;
        }
        let asset = match shipped(m.as_ref()) {
            Ok(asset) => asset,
            Err(e) => {
                tracing::debug!(machine = %name, error = %format!("{e:#}"), "agent refresh: no asset to push");
                return;
            }
        };
        // Only once the agent is up — never while a guest boots, which would
        // hold every `exec` for the boot — but from that moment: the gate is
        // taken before this refresh's own handshake and comparison, so an
        // `exec` arriving the instant `up` returns waits for the decision
        // instead of being handed the channel the push then restarts. A
        // machine found current releases it a handshake later.
        if let Err(e) = m.wait_agent_up(m.ready_timeout()).await {
            self.agent_update_unreached(m.as_ref(), &asset.version, &e, output)
                .await;
            return;
        }
        let replacing = m.begin_agent_replacement().await;
        let handshake = match m
            .wait_agent_during_replacement(Duration::from_secs(60))
            .await
        {
            Ok(agent) => agent.info().agent_version.clone(),
            Err(e) => {
                self.agent_update_unreached(m.as_ref(), &asset.version, &e, output)
                    .await;
                return;
            }
        };
        let recorded = self
            .state
            .lock()
            .await
            .machines
            .get(&name)
            .and_then(|s| s.repaired_agent.clone());
        let sealed = m.sealed_agent_version();
        let (from, to) = match decide(
            &handshake,
            recorded.as_deref(),
            sealed.as_deref(),
            &asset.version,
        ) {
            Refresh::Current => {
                tracing::debug!(machine = %name, stamp = %asset.version, "agent refresh: current");
                return;
            }
            Refresh::Skip(why) => {
                tracing::debug!(machine = %name, %why, "agent refresh: skipped");
                return;
            }
            Refresh::Update { from, to } => (from, to),
        };
        let pushed = match push_under(m, &asset, replacing).await {
            Ok(report) => report.pushed,
            Err(e) => {
                self.agent_update_failed(&name, Some(&from), &to, &e, output);
                return;
            }
        };
        if let Err(e) = self.mark_diverged(&name, &pushed).await {
            // The new agent is running; only the record of it failed.
            output(format!(
                "warning: agent: \"{name}\" runs {pushed} now, but recording it as diverged failed: {e:#}\n"
            ));
        }
        self.events.emit(
            "machine.agent_updated",
            json!({"vm": name, "machine": name, "from": from, "to": pushed, "ok": true}),
        );
        output(format!("agent: updated \"{name}\" ({from} → {pushed})\n"));
    }

    /// A refresh that gave up waiting for the agent, before anything was
    /// pushed: a failure where the machine is still up, but a quiet,
    /// accurate event where it was stopped under the refresh — `ok = false`
    /// with `stopped = true`, nothing on the caller's output, and a line in
    /// the daemon log.
    async fn agent_update_unreached(
        &self,
        m: &dyn Machine,
        to: &str,
        e: &anyhow::Error,
        output: &crate::scripting::OutputSink,
    ) {
        use super::agent_repair::{Unreached, unreached};
        let name = m.name();
        match unreached(m.state().await) {
            Unreached::Failed => self.agent_update_failed(name, None, to, e, output),
            Unreached::Stopped => {
                tracing::info!(machine = %name, "agent refresh: the machine stopped before its agent answered; nothing was pushed");
                self.events.emit(
                    "machine.agent_updated",
                    json!({"vm": name, "machine": name, "from": null, "to": to, "ok": false, "stopped": true,
                           "error": format!("{name} stopped before its agent answered; nothing was pushed")}),
                );
            }
        }
    }

    /// The failed refresh: said, evented, and survived.
    fn agent_update_failed(
        &self,
        name: &str,
        from: Option<&str>,
        to: &str,
        e: &anyhow::Error,
        output: &crate::scripting::OutputSink,
    ) {
        let what = match from {
            Some(from) => format!("({from} → {to})"),
            None => format!("(to {to})"),
        };
        output(format!(
            "warning: agent: could not update \"{name}\" {what}: {e:#}; set agent_update = false \
             on the vm to stop trying\n"
        ));
        self.events.emit(
            "machine.agent_updated",
            json!({"vm": name, "machine": name, "from": from, "to": to, "ok": false, "error": format!("{e:#}")}),
        );
    }

    /// Stop a machine, wait for the exit monitor to settle, and boot it again.
    pub async fn restart_machine(self: &Arc<Self>, name: &str, force: bool) -> Result<()> {
        let m = self.machine(name)?;
        m.stop(force).await?;
        m.wait_state(PowerState::Stopped, Duration::from_secs(60))
            .await
            .map_err(|_| anyhow!("{name} did not stop for restart"))?;
        self.start_machine(name).await
    }

    /// What the running network knows about `scope`'s machines — the only
    /// runtime state the [`forward_plan`] needs, gathered once so the plan
    /// itself touches no network. `scope` empty means the whole lab.
    ///
    /// The hardware address recorded is the one on the NIC actually holding
    /// the lease, not simply the first: priming the NAT engine with a
    /// multi-NIC machine's NIC-0 address for an address leased on NIC 1 would
    /// point the engine at the wrong port.
    async fn forward_observations(&self, scope: &[String]) -> forward_plan::Observed {
        let mut observed = forward_plan::Observed::default();
        for m in self.machines() {
            let name = m.name().to_string();
            if !scope.is_empty() && !scope.contains(&name) {
                continue;
            }
            let Ok(ips) = m.guest_ips().await else {
                continue;
            };
            let leased = ips
                .iter()
                .enumerate()
                .find_map(|(i, ip)| ip.as_ref().map(|ip| (i, ip)));
            let Some((nic, ip)) = leased else { continue };
            let Ok(ip) = ip.parse::<std::net::Ipv4Addr>() else {
                continue;
            };
            if let Some(mac) = m.macs().get(nic) {
                observed.macs.insert(name.clone(), *mac);
            }
            observed.leases.insert(name, ip);
        }
        observed
    }

    /// Install one planned forward on the address it binds
    /// ([`ForwardRule::bind`], which worktree isolation has already made
    /// `127.0.0.1`), returning its id and the host port it bound.
    ///
    /// The single executor behind every forward the lab installs, whether it
    /// came from a segment `forward {}` or a container `port {}`. Priming the
    /// NAT engine with the lease MAC happens for both: a machine that never
    /// originates egress is otherwise unreachable.
    ///
    /// `ports` are tried in order ([`forward_plan::bind_candidates`]): a
    /// fixed rule's one port, or an automatic rule's previous port then 0.
    async fn install_forward(
        &self,
        net: &LabNetwork,
        rule: &ForwardRule,
        ports: &[u16],
    ) -> Result<(u64, u16), String> {
        let services = net
            .segments
            .get(&rule.segment)
            .and_then(|s| s.services.as_ref())
            .ok_or_else(|| {
                format!(
                    "segment \"{}\" has no services — is the lab up?",
                    rule.segment
                )
            })?;
        if let Some(mac) = rule.prime_mac {
            services.learn_mac(rule.guest_ip, mac);
        }
        let mut last = String::from("no host port to bind");
        for &port in ports {
            let host_addr = std::net::SocketAddr::from((rule.bind, port));
            match services.add_forward(host_addr, rule.guest_ip, rule.guest_port, rule.proto) {
                Ok(installed) => return Ok(installed),
                Err(e) => {
                    tracing::debug!("{}: {e}; trying the next candidate", rule.machine);
                    last = e;
                }
            }
        }
        Err(last)
    }

    /// Announce everything a plan left out, and every host port it found
    /// claimed twice. Best-effort installation means these are the only
    /// record that a forward the lab author asked for is not there.
    fn announce_forward_plan(&self, plan: &ForwardPlan) {
        for skip in &plan.skipped {
            self.events.emit(
                "forward.skipped",
                json!({"what": skip.what, "reason": skip.why}),
            );
        }
        for conflict in &plan.conflicts {
            tracing::warn!(
                "host port {} claimed by {}",
                conflict.host_port,
                conflict.claimants.join(" and ")
            );
            self.events.emit(
                "forward.conflict",
                json!({"host_port": conflict.host_port, "claimants": conflict.claimants}),
            );
        }
    }

    /// `vmlab up [vm...]` (PRD §7.2, §10.4): start in `depends_on` waves —
    /// a dependency is satisfied once it is ready, never waiting for its
    /// provisioning — and provision every machine in parallel once it has
    /// started, each running its own steps in declaration order after the
    /// machines its `depends_on_provision` names have finished theirs.
    pub async fn up(
        self: &Arc<Self>,
        subset: &[String],
        output: crate::scripting::OutputSink,
    ) -> Result<()> {
        // Decide the whole schedule before touching anything: which
        // machines a subset drags in, which wave each lands in, and which
        // configuration steps are in scope (see `labd::plan`).
        let plan =
            plan::plan(&self.config().lab, subset, plan::Direction::Up).map_err(|e| anyhow!(e))?;
        // Worktree isolation (§21.4) refuses a worktree lab that peers a
        // global segment with another host, before anything boots.
        if let Some(isolation) = &self.isolation {
            if let Some(why) = isolation.refusal() {
                bail!("{why}");
            }
            output(format!("{}\n", isolation.describe()));
            for warning in &isolation.warnings {
                output(format!("warning: {warning}\n"));
            }
        }
        for skip in &plan.skipped {
            output(format!("{}: {}\n", skip.what, skip.why));
        }
        // A global segment's DHCP is the supervisor's, and it may have had
        // to refuse a static address or an MTU this lab declared (§9.2).
        for warning in self.network.lock().await.global_warnings() {
            output(format!("warning: {warning}\n"));
        }
        let targets: Vec<String> = plan.machines().cloned().collect();

        // `nested = true` asks for VMX/SVM the host may not have to give
        // (§5.2): the machine still boots, so this is said, not refused.
        let config = self.config();
        let nested: Vec<&str> = config
            .lab
            .vms
            .iter()
            .filter(|vm| vm.nested && targets.contains(&vm.name))
            .map(|vm| vm.name.as_str())
            .collect();
        if !nested.is_empty()
            && let Some(problem) = crate::qemu::host_nested_problem()
        {
            output(format!("warning: {}: {problem}\n", nested.join(", ")));
        }

        // A machine an earlier reload left pending, stopped since, boots on
        // the declaration as it is now (§7.1): rebuilt before anything below
        // asks it what it needs.
        for name in &targets {
            self.settle_pending(name).await?;
        }
        // A VM whose clone was made from a template the lab file no longer
        // declares cannot boot it either way — refuse before downloading the
        // new template or starting anything else, so the refusal is the
        // whole of what `up` did.
        for name in &targets {
            self.machine(name)?.check_disks()?;
        }
        // Likewise a machine the main checkout's freeze cannot build (§21.2).
        if let Some(plan) = self.freeze.lock().await.as_ref() {
            for name in &targets {
                if let Some(freeze_clone::Source::Refused(why)) = plan.machines.get(name) {
                    bail!("\"{name}\" cannot be built: {why}");
                }
            }
            for (name, _) in plan
                .clones()
                .filter(|(n, _)| targets.iter().any(|t| t == n))
            {
                output(format!(
                    "{name}: cloning from freeze generation {} (snapshot \"{}\") of {}\n",
                    plan.generation,
                    plan.snapshot,
                    plan.main_root.display()
                ));
            }
        }
        // Before anything downloads or boots, so the waves below see each
        // clone's first-boot marker and the pull its image pin.
        for name in &targets {
            self.clone_from_freeze(name).await?;
        }

        // Deferred template/image downloads happen here — before the binary
        // preflight (pulled meta can change the resolved firmware/TPM needs)
        // and before any clone or boot work, streaming progress to both the
        // CLI sink and the event feed.
        self.ensure_pulled(&targets, Some(&output)).await?;

        // Hardware a machine was declared with that this launch drops — a
        // vm block's `hyperv = true` under TCG or a named `cpu_model` — is
        // said, not refused (§5.2). After the pull, which binds the template
        // layer a model may come from.
        for name in &targets {
            for warning in self.machine(name)?.launch_warnings() {
                output(format!("warning: {warning}\n"));
            }
        }

        self.preflight_binaries(&targets)?;

        // Start the SMB server before guests boot so shares are reachable
        // during provisioning (PRD §7.5).
        self.ensure_smb(&output).await;

        // Every machine's mount task and what it has done so far, reported
        // before the lab is called up (§7.5).
        let mounts: Arc<std::sync::Mutex<Vec<ShareMounts>>> = Arc::default();
        // Provisioning (§7.2) runs beside the waves, not between them: each
        // machine's steps are a task of their own, spawned the moment it has
        // started, gated only on the machines its `depends_on_provision`
        // names. `finished` is each machine's signal that all of its steps
        // are done; `cancel` stops every step still running once one fails.
        let cancel = tokio_util::sync::CancellationToken::new();
        // A caller that drops `up` mid-flight (a cancelled template build)
        // drops the provisioning set with it; the guard stops the scripts
        // that set cannot abort.
        let _cancel_on_drop = cancel.clone().drop_guard();
        let finished: HashMap<String, tokio::sync::watch::Sender<bool>> = plan
            .provisioning
            .iter()
            .map(|p| (p.machine.clone(), tokio::sync::watch::channel(false).0))
            .collect();
        let mut provisioning: tokio::task::JoinSet<Result<()>> = tokio::task::JoinSet::new();
        for wave in &plan.waves {
            let wave = wave.clone();

            // A JoinSet, not loose handles: when one machine in the wave fails
            // the rest are aborted on the spot. Detached tasks used to keep
            // booting VMs and running first-boot scripts long after `up`
            // returned its error, so the reported state and the real state
            // diverged.
            let mut wave_tasks = tokio::task::JoinSet::new();
            for name in &wave {
                let me = self.clone();
                let n = name.clone();
                let out = output.clone();
                let mounts = Arc::clone(&mounts);
                // Whether `up` waits on this machine's agent anyway: a
                // first-boot runs on it, a provision or playbook is scoped to
                // it, another machine's steps wait for its provisioning, or a
                // later wave depends on it. Only those get the agent
                // refresh inline (§19.4); `up` must not start waiting on a
                // machine nothing waits for — one whose guest can never answer
                // would hold it for the whole ready timeout.
                let first_boot = me.machine(&n)?.pending_first_boot().is_some();
                let waited = first_boot
                    || plan
                        .provisioning_of(&n)
                        .is_some_and(|p| !p.steps.is_empty())
                    || plan.provision_awaited(&n)
                    || me.has_dependents(&n);
                wave_tasks.spawn(async move {
                    let m = me.machine(&n)?;
                    // A first-boot machine's readiness is `run_first_boot`'s
                    // to give, after the refresh; any other's is its
                    // handshake's, held here until the refresh below is done
                    // so nobody reaches the agent it is about to restart.
                    let mut hold = None;
                    if waited {
                        if !first_boot {
                            hold = me.hold_for_inline_refresh(m.as_ref());
                        }
                        me.start_machine(&n).await?;
                    } else {
                        me.start_with_deferred_refresh(&n).await?;
                    }
                    // The instance that booted: a reload may have rebuilt the
                    // one looked up above while it was still stopped (§7.1).
                    let m = me.machine(&n)?;
                    // Before this machine can be considered ready (§6.1). A
                    // no-op for machines carrying no first-boot script, so
                    // leaf timing is unchanged. It refreshes the agent itself,
                    // between the script and readiness.
                    me.run_first_boot(&m, &out).await?;
                    // Detached, so the wave does not block on the mount
                    // retry window; this machine's provisions and playbooks
                    // wait for it instead, and `up` reports what it did before
                    // it returns (§7.5). Only now: a first-boot script may run
                    // for as long as half an hour, past the mount task's own
                    // wait for readiness, and may reboot the guest under a
                    // mount made before it. A machine whose guest mounts for
                    // itself does nothing here.
                    let report = Arc::new(MountReport::default());
                    let task = tokio::spawn({
                        let m = Arc::clone(&m);
                        let services = me.services();
                        let guard = SettleOnDrop(Arc::clone(&report));
                        let report = Arc::clone(&report);
                        async move {
                            let _settle = guard;
                            m.mount_shares(services, report).await;
                        }
                    });
                    mounts.lock_recover().push(ShareMounts {
                        machine: n.clone(),
                        report,
                        task,
                    });
                    // After first-boot, before any provision (§19.4): the
                    // first-boot script is the template's and runs under the
                    // agent the template sealed; every provision after it
                    // sees the current one. Never fails the wave.
                    if waited && !first_boot {
                        me.refresh_agent(&m, &out).await;
                        me.release_inline_refresh(&n, hold);
                    }
                    // See `LabRuntime::pre_provision`.
                    let hook = me.pre_provision.read().expect("pre_provision lock").clone();
                    if let Some(hook) = hook {
                        hook(Arc::clone(&m), out.clone()).await?;
                    }
                    // Only gate the wave on readiness when something later
                    // depends on this machine.
                    if me.has_dependents(&n) {
                        m.wait_ready(m.ready_timeout()).await?;
                    }
                    Ok::<_, anyhow::Error>(n)
                });
            }
            // The wave is done when every machine in it has started. Meanwhile
            // the provisioning already under way may fail, which fails `up`
            // there and then rather than after the boots.
            loop {
                let next = tokio::select! {
                    joined = wave_tasks.join_next() => match joined {
                        None => break,
                        Some(joined) => Joined::Started(flatten_join(joined)),
                    },
                    Some(joined) = provisioning.join_next(), if !provisioning.is_empty() => {
                        Joined::Provisioned(flatten_join(joined))
                    }
                };
                match next {
                    Joined::Started(Ok(started)) => {
                        if let Some(work) = plan.provisioning_of(&started) {
                            self.spawn_provisioning(
                                &mut provisioning,
                                work,
                                plan.provision_awaited(&started),
                                &finished,
                                &mounts,
                                &output,
                                &cancel,
                            );
                        }
                    }
                    Joined::Started(Err(e)) | Joined::Provisioned(Err(e)) => {
                        return Err(abandon_up(e, wave_tasks, provisioning, &cancel).await);
                    }
                    Joined::Provisioned(Ok(())) => {}
                }
            }
        }
        // `up` returns once every step has finished (§7.2).
        while let Some(joined) = provisioning.join_next().await {
            if let Err(e) = flatten_join(joined) {
                return Err(abandon_up(
                    e,
                    tokio::task::JoinSet::<Result<String>>::new(),
                    provisioning,
                    &cancel,
                )
                .await);
            }
        }

        for line in self.install_forwards(&targets).await {
            output(format!("{line}\n"));
        }

        // After provisioning, and only now: the syncer writes as the
        // machine's default login, which the provisioning is what creates
        // (§19.6). The task belongs to this daemon, so the `vmlab` process
        // that asked for the `up` may exit without stopping it.
        self.start_workspaces(&targets, &output).await;

        let mounts = std::mem::take(&mut *mounts.lock_recover());
        report_share_mounts(mounts, MOUNT_REPORT_HOLD, &output).await;

        self.events.emit("lab.up", json!({"vms": targets}));
        Ok(())
    }

    /// Whether any forward — a segment `forward {}` or a container
    /// `port {}` — targets `machine`.
    fn has_forwards(&self, machine: &str) -> bool {
        let plan = forward_plan::plan(
            &forward_plan::ForwardInputs {
                lab: &self.config().lab,
                observed: &forward_plan::Observed::default(),
                isolated: self.isolation.is_some(),
            },
            &[machine.to_string()],
        );
        !plan.skipped.is_empty()
    }

    /// Wire up every forward `scope` requires — segment `forward {}` blocks
    /// aimed at those machines and their container `port {}` blocks alike
    /// (PRD §9.8, §18). An empty `scope` means the whole lab.
    ///
    /// Forwards target a machine's lease, so a restart that brings a new one
    /// must drop the old rules first: they are tracked per machine and
    /// removed before re-installing, so a forward never points at a stale IP.
    /// Best-effort — a forward that cannot be installed is announced on the
    /// event feed and the rest of the lab still works.
    ///
    /// Returns a line for each forward a frozen family's port slot bound off
    /// its declared port (§21.7), for `up` to print.
    async fn install_forwards(self: &Arc<Self>, scope: &[String]) -> Vec<String> {
        let observed = self.forward_observations(scope).await;
        let plan = forward_plan::plan(
            &forward_plan::ForwardInputs {
                lab: &self.config().lab,
                observed: &observed,
                isolated: self.isolation.is_some(),
            },
            scope,
        );
        self.announce_forward_plan(&plan);

        let mut installable: BTreeMap<&str, Vec<&ForwardRule>> = BTreeMap::new();
        for rule in plan.rules.iter() {
            installable
                .entry(rule.machine.as_str())
                .or_default()
                .push(rule);
        }
        let net = self.network.lock().await;
        let mut tracked = self.machine_forwards.lock().await;
        // A machine in scope the lab file declares no forward to any more —
        // a reload took its last one away (§7.1) — has what it had removed.
        let bare: Vec<String> = tracked
            .keys()
            .filter(|m| scope.contains(m) && !self.has_forwards(m))
            .cloned()
            .collect();
        for machine in bare {
            for old in tracked.remove(&machine).unwrap_or_default() {
                if let Some(s) = net
                    .segments
                    .get(&old.key.segment)
                    .and_then(|s| s.services.as_ref())
                {
                    s.remove_forward(old.id).await;
                }
            }
        }
        let mut shifted = Vec::new();
        for (machine, rules) in installable {
            // Drop this machine's forwards from a previous run/lease. Their
            // listeners are closed by the time `remove_forward` returns, so
            // an automatic rule can ask for its old port straight back.
            let previous = tracked.remove(machine).unwrap_or_default();
            for old in &previous {
                if let Some(s) = net
                    .segments
                    .get(&old.key.segment)
                    .and_then(|s| s.services.as_ref())
                {
                    s.remove_forward(old.id).await;
                }
            }
            // In a frozen family a fixed port is shifted by the lab's port
            // slot (§21.7), chosen and recorded under the family record's
            // lock, which is held until this machine's binds are done.
            let mut family = match self.family_ports(&rules) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("\"{machine}\": {e:#}");
                    self.events.emit(
                        "forward.skipped",
                        json!({"what": format!("\"{machine}\": forwards"), "reason": format!("{e:#}")}),
                    );
                    continue;
                }
            };
            let mut is_free = crate::freeze::ports::host_free;
            let candidates = {
                let mut fam =
                    family
                        .as_mut()
                        .map(|(_, ports, worktree, slot)| forward_plan::FamilyBind {
                            ports,
                            worktree: worktree.clone(),
                            slot: *slot,
                            is_free: &mut is_free,
                        });
                forward_plan::bind_candidates(&previous, &rules, fam.as_mut())
            };
            let mut installed = Vec::new();
            for (rule, ports) in rules.into_iter().zip(candidates) {
                let what = format!("\"{machine}\": {}", rule.source.describe());
                let bound = match ports {
                    Ok(ports) => self.install_forward(&net, rule, &ports).await,
                    Err(e) => Err(e),
                };
                match bound {
                    Ok((id, host_port)) => {
                        let forward = InstalledForward {
                            key: rule.key(),
                            id,
                            bind: rule.bind,
                            host_port,
                            auto: rule.host.is_auto(),
                            declared: rule.declared_port(),
                            guest_ip: rule.guest_ip,
                        };
                        if let Some(declared) = forward.shifted_from() {
                            shifted.push(format!(
                                "{what} declared on host port {declared} is bound on {host_port} \
                                 (worktree port slot, PRD §21.7)"
                            ));
                        }
                        self.events.emit(
                            "forward.installed",
                            json!({
                                "what": what,
                                "bind": rule.bind.to_string(),
                                "host_port": host_port,
                                "auto": rule.host.is_auto(),
                                "declared_port": forward.shifted_from(),
                                "guest": format!("{}:{}", rule.guest_ip, rule.guest_port),
                            }),
                        );
                        installed.push(forward);
                    }
                    Err(e) => {
                        tracing::warn!("{what}: {e}");
                        self.events
                            .emit("forward.skipped", json!({"what": what, "reason": e}));
                    }
                }
            }
            if let Some((locked, ports, _, _)) = family
                && let Err(e) = locked.save(&ports)
            {
                tracing::warn!("\"{machine}\": recording its ports: {e:#}");
            }
            tracked.insert(machine.to_string(), installed);
        }
        shifted
    }

    /// The frozen family's port record, locked and loaded, with this lab's
    /// name in it and its slot (allocated now if it has none), when the lab
    /// is in a family and `rules` hold a fixed port (§21.7). `None` outside
    /// a family, or when every rule is automatic.
    #[allow(clippy::type_complexity)]
    fn family_ports(
        &self,
        rules: &[&ForwardRule],
    ) -> Result<
        Option<(
            crate::freeze::ports::Locked,
            crate::freeze::ports::FamilyPorts,
            Option<String>,
            u16,
        )>,
    > {
        let Some(isolation) = &self.isolation else {
            return Ok(None);
        };
        if rules.iter().all(|r| r.host.is_auto()) {
            return Ok(None);
        }
        let mut locked = crate::freeze::ports::lock(&isolation.family_local)?;
        let mut ports = locked.load()?;
        let worktree = isolation.port_owner().map(str::to_string);
        let slot = ports.slot(worktree.as_deref());
        Ok(Some((locked, ports, worktree, slot)))
    }

    /// Spawn `work`'s provisioning into `set`, its machine having started.
    /// A machine with no steps that nobody's provisioning waits for needs no
    /// task: it is finished as it starts.
    #[allow(clippy::too_many_arguments)]
    fn spawn_provisioning(
        self: &Arc<Self>,
        set: &mut tokio::task::JoinSet<Result<()>>,
        work: &plan::Provisioning,
        awaited: bool,
        finished: &HashMap<String, tokio::sync::watch::Sender<bool>>,
        mounts: &Arc<std::sync::Mutex<Vec<ShareMounts>>>,
        output: &crate::scripting::OutputSink,
        cancel: &tokio_util::sync::CancellationToken,
    ) {
        let Some(done) = finished.get(&work.machine).cloned() else {
            return;
        };
        if work.steps.is_empty() && !awaited {
            done.send_replace(true);
            return;
        }
        let after: Vec<(String, tokio::sync::watch::Receiver<bool>)> = work
            .after
            .iter()
            .filter_map(|a| finished.get(a).map(|tx| (a.clone(), tx.subscribe())))
            .collect();
        let me = Arc::clone(self);
        let work = work.clone();
        let mounts = Arc::clone(mounts);
        let output = output.clone();
        let cancel = cancel.clone();
        set.spawn(async move {
            let name = work.machine.clone();
            let result = me
                .provision_machine(&work, after, &mounts, &output, &cancel)
                .await;
            match &result {
                Ok(()) => {
                    done.send_replace(true);
                }
                Err(e) if super::provisioning::is_cancelled(e) => {
                    output(format!(
                        "{name}: provisioning cancelled — another machine's step failed\n"
                    ));
                }
                Err(_) => {}
            }
            result
        });
    }

    /// One machine's configuration (§7.2): once it is ready, and once every
    /// machine in `after` has finished its own, its provision and playbook
    /// steps in declaration order, each line of their output led by its name.
    async fn provision_machine(
        self: &Arc<Self>,
        work: &plan::Provisioning,
        after: Vec<(String, tokio::sync::watch::Receiver<bool>)>,
        mounts: &std::sync::Mutex<Vec<ShareMounts>>,
        output: &crate::scripting::OutputSink,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let m = &work.machine;
        // A step may read the machine's shares, so it starts once they
        // have mounted or given up, and says which (§7.5).
        let report = mounts
            .lock_recover()
            .iter()
            .find(|s| s.machine == *m)
            .map(|s| Arc::clone(&s.report));
        if let Some(report) = report.filter(|_| !work.steps.is_empty()) {
            if !report.is_settled() {
                output(format!(
                    "\"{m}\": waiting for its shares to mount before its provisions and \
                     playbooks\n"
                ));
            }
            until_cancelled(cancel, report.settled()).await?;
            if report.take_report() {
                for line in report.lines(m, true) {
                    output(line);
                }
            }
        }
        let machine = self.machine(m)?;
        // The machine's own budget, never a literal: a VM may still be
        // running a first-boot provision, a container's entrypoint starts
        // fast and its healthcheck governs the rest. A build waives the
        // wait entirely — see `provisions_wait_ready`.
        if self
            .provisions_wait_ready
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            until_cancelled(cancel, machine.wait_ready(machine.ready_timeout())).await??;
        }
        let pending: Vec<&str> = after
            .iter()
            .filter(|(_, rx)| !*rx.borrow())
            .map(|(a, _)| a.as_str())
            .collect();
        if !pending.is_empty() {
            output(format!(
                "{m}: waiting for the provisioning of {} (depends_on_provision)\n",
                pending.join(", ")
            ));
        }
        for (_, mut rx) in after {
            // A sender dropped unfinished is a machine whose provisioning
            // failed; `up` is failing for it already.
            until_cancelled(cancel, rx.wait_for(|done| *done))
                .await?
                .map_err(|_| anyhow::Error::new(super::provisioning::ProvisionCancelled))?;
        }
        let lines = super::provisioning::LinePrefix::new(m, output.clone());
        let out = lines.sink();
        for step in &work.steps {
            let ran = self.run_step(step, &out, cancel).await;
            lines.flush();
            ran?;
        }
        Ok(())
    }

    /// One provision or playbook step on its own machine. A provision script
    /// is cancelled from inside, at its next host call, and awaited until it
    /// stops; a playbook apply is abandoned where it stands.
    async fn run_step(
        self: &Arc<Self>,
        step: &plan::Step,
        output: &crate::scripting::OutputSink,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let m = &step.machine;
        output(format!("{}\n", step.describe()));
        match &step.kind {
            plan::StepKind::Provision(p) => {
                let script = self.root.join(&p.script);
                // The script reaches its own machine with `lab.this_vm()`.
                let owner =
                    crate::scripting::ScriptOwner::provision(m).cancelled_by(cancel.clone());
                crate::scripting::run_script_file(
                    self.clone(),
                    &script,
                    Some(owner),
                    None,
                    output.clone(),
                )
                .await
                .with_context(|| format!("{m}: provision {}", p.script.display()))
            }
            plan::StepKind::Playbook(p) => {
                let outcome = until_cancelled(
                    cancel,
                    crate::labd::playbook::run_playbook(
                        self,
                        m,
                        p,
                        crate::labd::playbook::PlaybookMode::Apply,
                        output,
                    ),
                )
                .await?
                .with_context(|| format!("playbook {} play {} on {m}", p.path.display(), p.play))?;
                if outcome.exit_code != 0 {
                    bail!(
                        "playbook {} play {} on {m}: config-weave exited {}",
                        p.path.display(),
                        p.play,
                        outcome.exit_code
                    );
                }
                Ok(())
            }
        }
    }

    /// Run the machine's first-boot provision the first time it is
    /// instantiated, before it is reported ready (PRD §6.1).
    ///
    /// For a machine with nothing pending the readiness poller already flips
    /// `ready` (and emits the ready event), so this returns immediately without
    /// blocking — preserving the timing of everything that carries no
    /// first-boot script.
    ///
    /// For a pending first-boot it waits for the guest agent, runs the embedded
    /// script scoped to this machine (reached via `lab.this_vm()`), then marks
    /// the machine ready and emits the ready event. Any error or the overall
    /// timeout fails `up` and leaves the machine running for inspection.
    async fn run_first_boot(
        self: &Arc<Self>,
        m: &Arc<dyn Machine>,
        output: &crate::scripting::OutputSink,
    ) -> Result<()> {
        let Some(first_boot) = m.pending_first_boot() else {
            return Ok(());
        };
        let name = m.name();

        crate::scripting::ensure_wscript_surface_supported(
            &first_boot.template,
            first_boot.script.surface_version,
        )
        .map_err(anyhow::Error::msg)?;

        output(format!("first-boot: provisioning {name}...\n"));
        m.wait_agent_up(m.ready_timeout())
            .await
            .with_context(|| format!("first-boot {name}: agent did not come up"))?;

        // Hard ceiling: Windows specialize/OOBE can be slow, but a hung guest
        // must not wedge `up` forever.
        let label = format!("first-boot:{name}");
        let run = crate::scripting::run_script_source(
            self.clone(),
            first_boot.script.source,
            &label,
            m.local_dir().to_path_buf(),
            Some(crate::scripting::ScriptOwner::first_boot(name)),
            None,
            output.clone(),
        );
        tokio::time::timeout(Duration::from_secs(1800), run)
            .await
            .map_err(|_| anyhow!("first-boot {name}: timed out after 1800s"))?
            .with_context(|| format!("first-boot provision for {name}"))?;

        // The refresh `up` owes a machine it waits on (§19.4), here so the
        // machine is not reported ready while it restarts the agent. Never
        // fails the first-boot.
        self.refresh_agent(m, output).await;

        m.first_boot_done().await?;
        self.events.emit(
            &format!("{}.ready", m.event_subject()),
            json!({ m.event_subject(): name }),
        );
        super::machine::LabServices::machine_ready(self.as_ref(), name).await;
        output(format!("first-boot: {name} ready\n"));
        Ok(())
    }

    /// Graceful stop; clones retained (PRD §12).
    /// Stop machines in reverse dependency order.
    ///
    /// The mirror of [`up`](Self::up): a subset pulls in its *dependents*, and
    /// the waves run leaves-first, so a domain controller outlives the members
    /// that need it to shut down cleanly. `vmlab down dc01` therefore stops
    /// everything that depends on dc01 as well — before this it stopped dc01
    /// alone and left them running against a dead dependency.
    pub async fn down(self: &Arc<Self>, subset: &[String], force: bool) -> Result<()> {
        // A machine the lab file has dropped while it ran is still this
        // daemon's to stop (§7.1), though no plan over the file names it: a
        // full `down` stops it first, and naming it stops it alone.
        let config = self.config();
        let (declared, dropped): (Vec<String>, Vec<String>) = if subset.is_empty() {
            let dropped = self
                .machines()
                .map(|m| m.name().to_string())
                .filter(|n| config.lab.machine(n).is_none())
                .collect();
            (Vec::new(), dropped)
        } else {
            subset
                .iter()
                .cloned()
                .partition(|n| config.lab.machine(n).is_some() || self.machine(n).is_err())
        };
        let plan = if subset.is_empty() || !declared.is_empty() {
            plan::plan(&config.lab, &declared, plan::Direction::Down).map_err(|e| anyhow!(e))?
        } else {
            plan::LabPlan {
                waves: Vec::new(),
                provisioning: Vec::new(),
                skipped: Vec::new(),
            }
        };
        for skip in &plan.skipped {
            tracing::info!("{}: {}", skip.what, skip.why);
        }
        for name in &dropped {
            self.workspaces.stop(name).await;
            self.record_finalise_machine(name, "machine stopped").await;
            self.machine(name)?.stop(force).await?;
        }

        // Before the machines go: a syncer whose guest has gone would spend
        // its retry window failing to reach one, and a recording is
        // finalised while its last frame is still the machine's (D6).
        for name in plan.machines() {
            self.workspaces.stop(name).await;
            self.record_finalise_machine(name, "machine stopped").await;
        }

        for wave in &plan.waves {
            let mut handles = Vec::new();
            for name in wave {
                // A machine the config declares but this runtime never built
                // is nothing to stop.
                let Ok(m) = self.machine(name) else { continue };
                handles.push(tokio::spawn(async move { m.stop(force).await }));
            }
            for h in handles {
                h.await.map_err(|e| anyhow!("join: {e}"))??;
            }
        }

        // Full lab down: reap smbd too, or it outlives the daemon and holds
        // its port against the next `up`. Partial downs keep shares served.
        if subset.is_empty()
            && let Some(mut labsmb) = self.smb.lock().await.take()
        {
            labsmb.stop();
        }
        self.events.emit("lab.down", Value::Null);
        Ok(())
    }

    /// Stop everything and delete clones, lab-local state, and dynamic net
    /// config (PRD §12).
    pub async fn destroy(self: &Arc<Self>) -> Result<()> {
        // Worktree overlays name the frozen files by absolute path (§21.5).
        // With none depending, the freeze goes with the rest of `.vmlab/`.
        self.refuse_while_depended_on("cannot destroy the main lab")
            .await?;
        self.workspaces.stop_all().await;
        self.record_finalise_all("lab destroyed").await;
        self.down(&[], true).await?;
        // Wait for the exit monitors to settle before removing disks.
        for m in self.machines() {
            wait_settled(&*m).await;
        }
        // Removes clones, container overlays, AND named volumes — destroy is
        // the lab-scoped volume lifecycle boundary (PRD §12).
        remove_tree(&self.lab_local).await?;
        // A worktree lab gives its port slot back to the family (§21.7).
        crate::freeze::ports::release_worktree(&self.root)?;
        // Each machine's run dir, asked for rather than guessed at: the lab's
        // runtime dir also holds the daemon's own control socket, so it is not
        // ours to remove wholesale.
        for m in self.machines() {
            let _ = remove_tree(m.run_dir()).await;
        }
        Ok(())
    }

    /// The lab's dev machines, resolved (§19.1).
    ///
    /// The profile a machine's dev defaults fall back to is its **effective**
    /// one, which for a VM may have come from its template — so it is read off
    /// the already-resolved hardware rather than off the declaration, where a
    /// VM naming no `profile` of its own would land on the floor instead of on
    /// its guest OS's path.
    fn dev_machines(&self) -> Vec<crate::dev::ResolvedDev> {
        crate::dev::machines(&self.config().lab, |cfg| self.effective_profile(cfg.name()))
    }

    /// The guest OS profile `machine` effectively runs, read off its resolved
    /// hardware (a VM's may have come from its template). `None` when the
    /// machine is unknown, names no profile, or names one this host lacks —
    /// every profile-sourced default then falls through to its floor.
    pub(crate) fn effective_profile(&self, machine: &str) -> Option<crate::profiles::Profile> {
        self.machine(machine)
            .ok()
            .and_then(|m| m.profile())
            .and_then(|name| self.profiles.get(&name).cloned())
    }

    /// Start the workspace syncer for every `@dev` machine in `scope` that
    /// declares one (§19.6). An empty `scope` means the whole lab.
    ///
    /// **Called after provisioning, not at machine-ready**: the syncer writes
    /// as the machine's default login, and that account does not exist until
    /// provisioning has created it.
    ///
    /// A workspace whose host directory is not there does not start, loudly.
    /// Creating it would be worse than refusing: an empty canonical tree is a
    /// tree the syncer would then propagate *as* empty.
    async fn start_workspaces(
        self: &Arc<Self>,
        scope: &[String],
        output: &crate::scripting::OutputSink,
    ) {
        for dev in self.dev_machines() {
            if dev.workspace.is_none() {
                continue;
            }
            if !scope.is_empty() && !scope.contains(&dev.name) {
                continue;
            }
            let (Some(workspace), Ok(machine)) =
                (self.workspace_of(&dev.name), self.machine(&dev.name))
            else {
                continue;
            };
            // The syncer is the one piece of vmlab's machinery that runs as
            // the machine's default login (§19.6). With none declared it can
            // only run as the agent identity, and the whole tree lands owned
            // by `root` or `SYSTEM` — which surfaces hours later as
            // permission errors nobody can place, unless it is said here.
            if crate::config::model::default_login(machine.logins()).is_none() {
                self.events.emit(
                    "workspace.identity",
                    json!({
                        "machine": dev.name,
                        "reason": "no login {} is declared, so the workspace is written as the \
                                   agent identity and the tree will not be owned by a developer \
                                   account",
                    }),
                );
            }
            // **Up front, on the terminal that asked for the `up`.** The
            // event log alone would only reach someone already watching it,
            // and the whole point is that both degradations otherwise fail at
            // a random path hours in, looking like a vmlab bug. The syncer
            // emits them too, for whoever attaches later.
            for degradation in workspace.preconditions.degradations() {
                output(format!("warning: {}: {degradation}\n", dev.name));
            }
            self.workspaces
                .start(
                    workspace,
                    Arc::new(WorkspaceSessions { machine }),
                    self.events.clone(),
                )
                .await;
        }
    }

    /// One machine's workspace as declared, resolved against this host — or
    /// `None` for a machine that is not a dev machine carrying one, or whose
    /// canonical directory is not there.
    ///
    /// Resolved in one place because two callers need the same answer for
    /// opposite reasons: [`start_workspaces`](Self::start_workspaces) starts a
    /// syncer with it, and the snapshot bracket (§19.6) needs the ledger's
    /// location for a machine that is **down** — where there is no syncer to
    /// ask and the restore still has to leave its note.
    ///
    /// A workspace whose host directory is not there does not resolve, loudly.
    /// Creating it would be worse than refusing: an empty canonical tree is a
    /// tree the syncer would then propagate *as* empty.
    fn workspace_of(&self, machine: &str) -> Option<crate::labd::workspace::Workspace> {
        let dev = self
            .dev_machines()
            .into_iter()
            .find(|d| d.name == machine)?;
        let declared = self.root.join(dev.workspace.as_ref()?);
        let host_root = match declared.canonicalize() {
            Ok(root) if root.is_dir() => root,
            _ => {
                self.events.emit(
                    "workspace.unavailable",
                    json!({
                        "machine": machine,
                        "workspace": declared.display().to_string(),
                        "reason": "the host workspace directory is not there",
                    }),
                );
                return None;
            }
        };
        // §19.6, resolved from the declaration alone (ADR-0003): the guest
        // family and the default login's elevation are what decide the three
        // Windows actions, and the syncer should never have to ask.
        let handle = self.machine(machine).ok()?;
        Some(crate::labd::workspace::Workspace {
            machine: machine.to_string(),
            ledger_path: crate::labd::workspace::ledger::Ledger::path(&self.lab_local, machine),
            host_root,
            guest_root: dev.workspace_guest.clone(),
            max_file_bytes: self.host_cfg.workspace_max_file,
            preconditions: crate::labd::workspace::preconditions(
                handle.guest_os(),
                handle.logins(),
            ),
        })
    }

    /// Run one machine's sync pass now and answer with what it decided
    /// (§19.6) — `vmlab dev sync flush`.
    pub async fn workspace_flush(
        &self,
        machine: &str,
    ) -> Result<crate::status::WorkspaceSyncStatus> {
        let syncer = self.workspaces.expect(machine).await?;
        Ok(syncer.pass_now().await?.project())
    }

    /// Say which side wins at halted paths and carry it out (§19.6).
    ///
    /// `--all` is expanded **here**, from the halt the daemon is holding,
    /// rather than from a list the client assembled: a client's list is a
    /// snapshot of a projection it polled, and resolving against a stale one
    /// would be a developer answering a question that has since changed.
    pub async fn workspace_resolve(
        &self,
        machine: &str,
        paths: Vec<String>,
        all: bool,
        winner: &str,
    ) -> Result<crate::status::WorkspaceSyncStatus> {
        let winner = crate::labd::workspace::plan::Winner::parse(winner).ok_or_else(|| {
            anyhow::anyhow!(
                "a workspace conflict is resolved toward `host` or `guest`, not {winner:?}"
            )
        })?;
        let syncer = self.workspaces.expect(machine).await?;
        let paths = match all {
            true => syncer.halted_paths(),
            false => paths,
        };
        if paths.is_empty() {
            anyhow::bail!(
                "there is nothing to resolve on \"{machine}\": its workspace is not halted, or the \
                 halt cleared while you were deciding"
            );
        }
        Ok(syncer.resolve(paths, winner).await?.project())
    }

    /// Bring the guest's copy of workspace paths to the host (§19.6).
    ///
    /// Both sides come back together, read by the daemon, because the daemon is
    /// the one thing that knows a workspace's two roots without being told —
    /// which keeps `dev sync diff` from doing path arithmetic a `@dev` edit
    /// could invalidate.
    pub async fn workspace_diff(
        &self,
        machine: &str,
        paths: Vec<String>,
    ) -> Result<crate::labd::workspace::diff::Diff> {
        let syncer = self.workspaces.expect(machine).await?;
        let paths = match paths.is_empty() {
            true => syncer.halted_paths(),
            false => paths,
        };
        if paths.is_empty() {
            anyhow::bail!(
                "\"{machine}\"'s workspace is not halted, so there is nothing to diff — name a \
                 path to compare one anyway"
            );
        }
        // The one thing this layer contributes: the session, as the machine's
        // own default login, so a diff reads what the syncer would write.
        let sessions = WorkspaceSessions {
            machine: self.machine(machine)?,
        };
        let guest = crate::labd::workspace::syncer::GuestSessions::open(&sessions)
            .await
            .context("opening a file session as the machine's default login")?;
        crate::labd::workspace::diff::all(guest.as_ref(), &syncer.workspace, &paths).await
    }

    /// The lab status projection (ADR-0004) — produced here, rendered unchanged
    /// by the CLI, the REST surface and the console.
    pub async fn status(&self) -> LabStatus {
        // One projection for both kinds: each adapter fills its own variant
        // (see `Machine::status_detail`), so a machine is described in exactly
        // one place.
        // Which machines carry `@dev`, and which of them is *the* dev machine
        // (§19.1) — a lab-scoped answer, so it is resolved once here rather
        // than asked of each machine.
        let devs = self.dev_machines();
        // Which machines a repair verb changed in place (§19.4) — recorded in
        // the lab's persisted state, so it is read once here rather than
        // asked of each machine, which does not hold it.
        let diverged: Vec<String> = {
            let state = self.state.lock().await;
            state
                .machines
                .iter()
                .filter(|(_, s)| s.repaired_agent.is_some())
                .map(|(name, _)| name.clone())
                .collect()
        };
        // What each machine's workspace syncer last decided (§19.6) — read
        // once here for the same reason as the two above: a syncer belongs to
        // the lab, not to the machine it writes into.
        let syncing = self.workspaces.reports().await;
        // And what each machine is recording (§11.1), owned by the lab too.
        let recording = self.record_status(None).await;
        let mut machines = Vec::new();
        for m in self.machines() {
            let mut status = m.status().await;
            status.recording = recording.iter().find(|r| r.machine == status.name).cloned();
            // Lab-level, not machine-level: is this machine's template/image
            // download still pending? Drives the "Download" button.
            status.cached = !self.pulls.lock_recover().is_pending(&status.name);
            status.dev = devs
                .iter()
                .find(|d| d.name == status.name)
                .cloned()
                .map(crate::status::DevStatus::from)
                .map(|dev| crate::status::DevStatus {
                    sync: syncing.get(&status.name).map(|report| report.project()),
                    ..dev
                });
            status.agent_diverged = diverged.contains(&status.name);
            machines.push(status);
        }

        let net = self.network.lock().await;
        // Cross-host trunk state lives in the supervisor (it owns the global
        // switches); one best-effort RPC per status when the lab has global
        // segments. None = supervisor unreachable → peer state reads null.
        let trunk_states: Option<std::collections::HashMap<String, bool>> =
            if net.segments.values().any(|s| s.global) {
                fetch_global_peer_states().await
            } else {
                Some(Default::default())
            };
        let mut segments = Vec::new();
        for seg in net.segments.values() {
            // None = not a global segment (no trunk possible) or supervisor
            // unreachable; bool = live trunk state keyed by segment name, so
            // the accept side (no local `connect`) lights up too.
            let peer_connected = if seg.global {
                trunk_states
                    .as_ref()
                    .map(|m| m.get(&seg.global_name).copied().unwrap_or(false))
            } else {
                None
            };
            // Switch counters ride along: a non-zero drop count is the signal
            // that a segment is shedding frames under load (a port's egress
            // queue filling), which otherwise only shows up as mysteriously
            // slow guest transfers.
            let sw = seg.switch.stats();
            segments.push(SegmentStatus {
                name: seg.name.clone(),
                subnet: seg.subnet.to_string(),
                gateway: seg.gateway_ip.to_string(),
                nat: seg.nat,
                dhcp: seg.dhcp,
                global: seg.global,
                connect: seg.peer.clone(),
                peer_connected,
                frames: SegmentFrames {
                    forwarded: sw.frames_forwarded,
                    flooded: sw.frames_flooded,
                    dropped: sw.frames_dropped,
                    offloaded: sw.frames_offloaded,
                },
            });
        }
        let forwards = self.forward_statuses().await;
        let freeze_stale = self.freeze_stale(self.freeze.lock().await.as_ref());
        let pending = self.pending().await;
        // In-flight downloads, so a page load mid-pull still shows progress
        // (the events only reach clients that were already connected).
        LabStatus {
            lab: self.name.clone(),
            machines,
            segments,
            provisioned: self.provisioned(),
            pulls: self.pulls.lock_recover().snapshot(),
            config_digest: crate::config::digest(&self.config()),
            pending,
            forwards,
            isolation: self.isolation.as_ref().map(|p| p.status()),
            freeze_stale,
        }
    }

    /// The installed forwards as `status` reports them: by machine, then in
    /// the order they were installed.
    async fn forward_statuses(&self) -> Vec<crate::status::ForwardStatus> {
        let tracked = self.machine_forwards.lock().await;
        let mut machines: Vec<&String> = tracked.keys().collect();
        machines.sort();
        machines
            .into_iter()
            .flat_map(|m| &tracked[m])
            .map(|f| crate::status::ForwardStatus {
                machine: f.key.machine.clone(),
                segment: f.key.segment.clone(),
                bind: Some(f.bind)
                    .filter(|b| !b.is_unspecified())
                    .map(|b| b.to_string()),
                host_port: f.host_port,
                guest: format!("{}:{}", f.guest_ip, f.key.guest_port),
                proto: match f.key.proto {
                    crate::config::model::Proto::Tcp => "tcp",
                    crate::config::model::Proto::Udp => "udp",
                    crate::config::model::Proto::Both => "both",
                }
                .to_string(),
                auto: f.auto,
                guest_port: f.key.guest_port,
                declared_port: f.shifted_from(),
            })
            .collect()
    }

    /// Whether this lab has materialised anything a destroy would remove:
    /// clones, container overlays or named volumes. Not `.vmlab` itself —
    /// that directory is created the moment the daemon opens the lab, so it
    /// says nothing; the per-machine dirs appear only once a machine starts.
    fn provisioned(&self) -> bool {
        self.machines().any(|m| m.local_dir().exists()) || self.lab_local.join("volumes").exists()
    }

    /// Live per-segment DNS zone snapshots (`dns.table`). Segments without a
    /// local DNS zone — global (supervisor-gatewayed) or `dns { enabled =
    /// false }` — are omitted.
    pub async fn dns_table(&self) -> Value {
        let net = self.network.lock().await;
        let mut segments: Vec<(String, Value)> = Vec::new();
        for seg in net.segments.values() {
            let Some(zone) = seg.gateway.as_ref().and_then(|g| g.dns_zone()) else {
                continue;
            };
            let snapshot = zone.lock_recover().snapshot();
            segments.push((seg.name.clone(), json!(snapshot)));
        }
        segments.sort_by(|(a, _), (b, _)| a.cmp(b));
        let segments: Vec<Value> = segments
            .into_iter()
            .map(|(name, zone)| json!({ "segment": name, "zone": zone }))
            .collect();
        json!({ "segments": segments })
    }

    // ---- snapshots (PRD §7.3; containers §18) --------------------------------

    /// Snapshot one machine — VM or container, same contract. The event and
    /// state record note which; container records also pin the image digest
    /// the capture is valid against.
    ///
    /// **A pre-flight flush brackets capture** (§19.6). A dev machine's
    /// workspace is flushed first, so the snapshot is coherent with the
    /// canonical tree and restoring it later lands somewhere meaningful rather
    /// than mid-transfer — and if the guest is holding work the host has never
    /// seen, this refuses. **With no escape flag**: a snapshot of a tree that
    /// exists nowhere else is not a thing to take against a warning. Every
    /// other machine passes straight through.
    pub async fn snapshot(&self, vm_name: &str, snap: &str) -> Result<bool> {
        let m = self.machine(vm_name)?;
        if let Some(why) = super::worktree::take_refusal(&*self.state.lock().await, vm_name, snap) {
            return Err(crate::proto::CommandError::conflict(why).into());
        }
        self.workspaces
            .before_capture(vm_name, self.workspace_of(vm_name).as_ref())
            .await?;
        let online = m.snapshot(snap).await?;
        {
            let mut state = self.state.lock().await;
            state.machine_mut(vm_name).snapshots.insert(
                snap.to_string(),
                SnapshotRecord {
                    online,
                    taken_at: chrono::Utc::now(),
                    image_digest: m.snapshot_pin(),
                    freeze_point: None,
                    frozen_in: None,
                    hypervisor: m.hypervisor(),
                },
            );
            state.save(&self.lab_local)?;
        }
        self.events.emit(
            "snapshot.created",
            json!({"vm": vm_name, "name": snap, "online": online}),
        );
        Ok(online)
    }

    /// Lab-wide snapshot: every VM and container under one name; consistency
    /// across machines is best-effort, not coordinated (PRD §7.3).
    pub async fn snapshot_all(&self, snap: &str) -> Result<Value> {
        let mut results = Vec::new();
        for m in self.machines() {
            let online = self.snapshot(m.name(), snap).await?;
            results.push(json!({"vm": m.name(), "online": online}));
        }
        Ok(json!(results))
    }

    /// Roll one machine back to a snapshot (PRD §7.3; containers §18).
    ///
    /// The pin rule has one home: a record that names an artefact identity is
    /// only valid against a machine still reporting it
    /// ([`Machine::snapshot_pin`]). A container's scratch overlay means
    /// nothing without the same read-only rootfs; a VM's snapshots live inside
    /// its own qcow2 chain, pin nothing, and pass this untouched.
    ///
    /// **Restore brackets the syncer** (§19.6). A restore rewinds the guest by
    /// hundreds of files at once, which a bidirectional syncer cannot tell from
    /// the developer having edited them — and it would carry them onto the
    /// canonical copy, silently. vmlab performs the restore, so it can bracket
    /// it: the syncer comes off the workspace before the rewind and goes back
    /// on owing a **re-seed**, which carries the tree back host→guest and lets
    /// nothing flow the other way.
    ///
    /// `discard` is the explicit flag a halted workspace needs. Refusing
    /// outright would be obstruction — wanting to throw the guest copy away is
    /// frequently *why* someone restores — but a restore destroys the guest
    /// copy of every conflicting path, so it is asked for by name.
    /// Restore `snap` on machine `name` (§7.3). Answers with what the
    /// restore could not do but did not fail over: the warnings to show.
    pub async fn restore(
        self: &Arc<Self>,
        name: &str,
        snap: &str,
        discard: bool,
    ) -> Result<Vec<String>> {
        let record = {
            let mut state = self.state.lock().await;
            state.machine_mut(name).snapshots.get(snap).cloned()
        }
        .ok_or_else(|| anyhow!("\"{name}\" has no snapshot \"{snap}\""))?;
        if let Some(g) = record.frozen_in {
            return Err(
                crate::proto::CommandError::conflict(super::worktree::locked_refusal(
                    name,
                    snap,
                    g,
                    "restoring",
                ))
                .into(),
            );
        }

        let m = self.machine(name)?;
        // An online snapshot's RAM and device state are the format of the
        // hypervisor that took it, which the machine may no longer run on
        // (§22, D31). Refused before anything moves.
        if let Some(why) = record.hypervisor_refusal(name, snap, m.hypervisor()) {
            return Err(crate::proto::CommandError::conflict(why).into());
        }
        let workspace = self.workspace_of(name);
        // Before anything is rolled back, and before the syncer is taken off
        // the workspace: a refusal has to leave the machine exactly as it was.
        self.workspaces
            .before_restore(name, discard, workspace.as_ref())
            .await?;
        // The pin is compared after any deferred pull the machine's own
        // restore performs, so bind the artefact first.
        self.ensure_pulled(std::slice::from_ref(&name.to_string()), None)
            .await?;
        if let Some(want) = &record.image_digest {
            let current = m.snapshot_pin();
            if Some(want) != current.as_ref() {
                bail!(
                    "\"{name}\": snapshot \"{snap}\" was taken against {want}, but the pinned \
                     artefact is now {} — destroy the machine (clearing its snapshots) or \
                     restore the original pin",
                    current.as_deref().unwrap_or("<not pulled>")
                );
            }
        }

        // **Suspend first**, and it is load-bearing twice over. A pass already
        // scanning would otherwise be free to finish against a guest that gets
        // rewound underneath it — and that pass is the one that carries five
        // hundred rolled-back files onto the canonical copy. It would also
        // *erase the note below*: the loop holds the ledger in memory and
        // saves it whole, so a pass completing after the note was written
        // would put back a ledger that never had it, and the resumed syncer
        // would stat-walk the rewound tree as though nothing had happened.
        // Suspending is a real stop, waited for, which closes both.
        let bracket = self.workspaces.suspend(name).await;

        // An online restore rewinds the guest to a moment it held none of
        // the host USB devices it holds now (capture refuses while any are
        // attached), so they are unplugged first rather than left on a bus
        // the restored device state knows nothing of (§5.2.1). An offline
        // restore stops the machine, which releases them anyway.
        if record.online {
            self.usb_release_for(name, "restore").await;
        }

        // **Every check that can refuse has already run**, so the note goes on
        // before the rewind and stays on if the rewind fails. That ordering is
        // the bracket's whole safety argument: after this point nobody can
        // tell whether the guest was rolled back or not, and the two mistakes
        // are not equally expensive — an unwanted re-seed costs unsynced guest
        // work the developer just asked to roll back, where a missed one costs
        // the canonical copy, which is the thing nothing re-derives. It is
        // also why the note is on the ledger rather than in this process: a
        // daemon that dies mid-restore must not lose it.
        let noted = match &workspace {
            None => Ok(()),
            Some(workspace) => self.workspaces.mark_rewound(workspace).await,
        };
        // The note is the rewind's **precondition**, so a note that could not
        // be written stops the restore rather than being carried on without:
        // a syncer coming back to a rolled-back tree it was never told about
        // is exactly the propagation this bracket exists to prevent.
        let restored = match noted {
            // A freeze point is no internal snapshot of the machine's own
            // disks but the state of the frozen files under them (§21.3).
            Ok(()) => match record.freeze_point {
                Some(generation) => self.restore_freeze_point(&m, snap, generation).await,
                None => {
                    Arc::clone(&m)
                        .restore(self.services(), snap, record.online)
                        .await
                }
            },
            Err(e) => Err(e),
        };
        // Put back on **both** paths. A workspace whose syncer quietly never
        // came back looks exactly like a machine with nothing to sync, which
        // is the silent-divergence failure ADR-0014 exists to rule out.
        if let Some(bracket) = bracket {
            self.workspaces.resume(bracket).await;
        }
        if let (Err(e), Some(workspace)) = (&restored, &workspace) {
            // Said rather than silently undone, for the reason above: the
            // re-seed is still owed, and a developer whose restore failed
            // should hear that their guest tree is about to be re-converged
            // anyway rather than discover it.
            self.events.emit(
                "workspace.reseed_owed",
                json!({
                    "machine": name,
                    "workspace": workspace.host_root.display().to_string(),
                    "reason": format!(
                        "the restore failed ({e:#}) after the workspace was already marked as \
                         rewound, so the re-seed still runs: whether the guest tree was rolled \
                         back is no longer knowable, and re-converging a tree that was not is \
                         cheaper than propagating one that was"
                    ),
                }),
            );
        }
        restored?;
        let mut warnings = Vec::new();
        let mut event = json!({"vm": name, "name": snap, "online": record.online});
        if record.online {
            let clock = self.set_clock_after_restore(&m).await;
            event["clock"] = json!(clock.as_str());
            if let ClockAfterRestore::NotSet(why) = &clock {
                tracing::warn!("{name}: guest clock not set after restore: {why}");
                warnings.push(format!(
                    "\"{name}\": the guest clock was not set to the host's time, so it \
                     resumes at the time the snapshot was taken: {why}"
                ));
            }
        }
        self.events.emit("snapshot.restored", event);
        Ok(warnings)
    }

    /// Bring a guest restored from an online snapshot back to the host's
    /// time (§7.3): it resumes with the clock it was captured with. Never
    /// fails the restore, which has already happened; a clock that could not
    /// be set is said instead.
    async fn set_clock_after_restore(&self, m: &Arc<dyn Machine>) -> ClockAfterRestore {
        use crate::config::model::Clock;
        match m.clock() {
            Clock::Snapshot => return ClockAfterRestore::Kept,
            Clock::Fixed(_) => return ClockAfterRestore::Fixed,
            Clock::Host => {}
        }
        let agent = match m.wait_agent(CLOCK_AGENT_WAIT).await {
            Ok(agent) => agent,
            Err(e) => {
                return ClockAfterRestore::NotSet(format!("its agent did not answer: {e:#}"));
            }
        };
        match agent
            .set_time(std::time::SystemTime::now(), CLOCK_SET_WAIT)
            .await
        {
            Ok(()) => ClockAfterRestore::Set,
            Err(e) => ClockAfterRestore::NotSet(format!("{e:#}")),
        }
    }

    /// Delete one machine's snapshot. A locked one refuses (§21.3), and a
    /// freeze point is a record alone: its state is the frozen files', which
    /// nothing deletes but a thaw, and the overlay's empty mark of it (when a
    /// restore left one) goes with it.
    pub async fn delete_snapshot(&self, vm_name: &str, snap: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let record = state
            .machines
            .get(vm_name)
            .and_then(|m| m.snapshots.get(snap));
        if let Some(g) = record.and_then(|r| r.frozen_in) {
            return Err(
                crate::proto::CommandError::conflict(super::worktree::locked_refusal(
                    vm_name, snap, g, "deleting",
                ))
                .into(),
            );
        }
        if record.is_some_and(|r| r.freeze_point.is_some()) {
            let m = self.machine(vm_name)?;
            for disk in m.disk_paths() {
                let marked = crate::freeze::inspect::disk_facts(&disk)
                    .await?
                    .is_some_and(|f| f.snapshots.iter().any(|s| s.name == snap));
                if marked {
                    crate::template::qimg::snapshot_delete(&disk, snap).await?;
                }
            }
            state.machine_mut(vm_name).snapshots.remove(snap);
            state.save(&self.lab_local)?;
            return Ok(());
        }
        self.machine(vm_name)?.delete_snapshot(snap).await?;
        state.machine_mut(vm_name).snapshots.remove(snap);
        state.save(&self.lab_local)?;
        Ok(())
    }

    pub async fn snapshots(&self, vm_name: &str) -> Result<Value> {
        let state = self.state.lock().await;
        let snaps = state
            .machines
            .get(vm_name)
            .map(|m| m.snapshots.clone())
            .unwrap_or_default();
        Ok(json!(
            snaps
                .into_iter()
                .map(|(name, r)| json!({
                    "name": name,
                    "online": r.online,
                    "taken_at": r.taken_at,
                    // §21.3: stored in a frozen file, so restore and delete
                    // refuse until `vmlab worktree thaw`.
                    "locked": r.frozen_in.is_some(),
                    "frozen_in": r.frozen_in,
                    "freeze_point": r.freeze_point,
                }))
                .collect::<Vec<_>>()
        ))
    }
}

/// How long teardown waits for a machine's exit monitor to settle before
/// removing what the machine was using.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Who runs the agent refresh a machine's readiness waits on (§19.4).
enum OwedRefresh {
    /// The machine's `before_ready`, because nothing in the `up` or
    /// `vm start` that booted it waits on its agent.
    AtHandshake,
    /// The `up` that waits on it, inline; `before_ready` waits for this to
    /// resolve, which it does when that refresh is done or abandoned.
    Inline(tokio::sync::oneshot::Receiver<()>),
}

/// The longest a deferred agent refresh holds its machine unready (§19.4).
/// A backstop only: every step of the refresh carries its own timeout, and
/// together they come to a little over four minutes on a guest that answers
/// none of them. Past this the machine is reported ready on whatever agent
/// answers, and the refresh finishes — or fails — in the background.
const REFRESH_HOLD: Duration = Duration::from_secs(300);

/// How long a machine that just became ready is given to take a DHCP lease
/// before its forwards are planned without one (and skipped, said so).
const LEASE_WAIT: Duration = Duration::from_secs(60);

/// Wait for one machine to come to rest before deleting its disks.
///
/// Best-effort, and deliberately: a machine that will not settle must not
/// wedge a `destroy` forever. It is announced rather than waited on further,
/// because the removal goes ahead either way.
async fn wait_settled(m: &dyn Machine) {
    if let Err(e) = m.wait_state(PowerState::Stopped, SETTLE_TIMEOUT).await {
        tracing::warn!("{e} — tearing it down anyway");
    }
}

/// The lab runtime as the narrow interface a machine boots against.
///
/// Everything here is lab-level work a machine cannot do for itself — reaching
/// the fabric, the event log, the shared-folder server. Nothing here knows
/// what kind of machine is asking.
#[async_trait::async_trait]
impl crate::labd::machine::LabServices for LabRuntime {
    fn events(&self) -> &Arc<EventLog> {
        &self.events
    }

    async fn ensure_pulled(&self, machine: &str) -> Result<()> {
        // The inherent `ensure_pulled` needs `Arc<Self>` — it spawns
        // cancellable download tasks that outlive the call — which a `&self`
        // trait method cannot produce; hence [`LabRuntime::arc`].
        self.arc().ensure_pulled(&[machine.to_string()], None).await
    }

    async fn ensure_shares(&self) {
        let quiet: crate::scripting::OutputSink = Arc::new(|_| {});
        self.arc().ensure_smb(&quiet).await;
    }

    async fn attach_nic(
        &self,
        segment: &str,
        sock: &std::path::Path,
        mac: crate::config::model::MacAddr,
        isolated: bool,
        tap_ok: bool,
    ) -> Result<crate::net::fastpath::NicAttachment> {
        let mut net = self.network.lock().await;
        let seg = net
            .segment_mut(segment)
            .ok_or_else(|| anyhow!("unknown segment \"{segment}\""))?;
        seg.attach_nic(sock, mac, isolated, tap_ok).await
    }

    async fn before_resume(&self, machine: &str) -> Result<()> {
        // A `--record` armed for this boot attaches now, while the CPUs are
        // still paused, so the video starts at the firmware's first frame.
        self.record_at_resume(machine).await
    }

    async fn before_ready(&self, machine: &str) {
        // An agent refresh `up` or `vm start` deferred because nothing waited
        // on this machine (§19.4). Held here, not run beside readiness: the
        // refresh restarts the agent, and a caller let in by `ready` would
        // land on the channel as it closes.
        self.run_deferred_refresh(machine).await;
    }

    async fn machine_ready(&self, machine: &str) {
        let scope = [machine.to_string()];
        // Readiness is the agent's handshake, which a guest can give before
        // its DHCP client has a lease. A machine with forwards waits a while
        // for one rather than having every forward skipped for want of it.
        if self.has_forwards(machine) {
            let deadline = tokio::time::Instant::now() + LEASE_WAIT;
            while !self
                .forward_observations(&scope)
                .await
                .leases
                .contains_key(machine)
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        // `up` may have returned before this machine took its lease, so a
        // shifted port is announced here too; `status` and the
        // `forward.installed` event carry it either way (§21.7).
        for line in self.arc().install_forwards(&scope).await {
            tracing::info!("{line}");
        }
    }

    async fn smb_mount_plan(
        &self,
        machine: &str,
        os: crate::smb::OsHint,
    ) -> Vec<crate::smb::MountStep> {
        match self.smb.lock().await.as_ref() {
            Some(labsmb) => labsmb.mount_plan(machine, os),
            None => Vec::new(),
        }
    }
}

/// Delete a directory tree off the runtime. A lab's clones are tens of GB of
/// qcow2; doing this inline froze the daemon's whole reactor — the network
/// fabric, QMP and agent channels, and the protocol server with it — for as
/// long as the unlink took. Missing is success.
async fn remove_tree(dir: &std::path::Path) -> Result<()> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if !dir.exists() {
            return Ok(());
        }
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))
    })
    .await
    .map_err(|e| anyhow!("remove task: {e}"))?
}

/// Cross-host trunk state per global segment name, from the supervisor's
/// `global.list` (PRD §9.2). `None` = supervisor unreachable; used by
/// [`LabRuntime::status`] to report `peer_connected` per segment.
async fn fetch_global_peer_states() -> Option<std::collections::HashMap<String, bool>> {
    let client = crate::proto::client::SupClient::connect(&crate::paths::supervisor_socket())
        .await
        .ok()?;
    let list = client
        .send(crate::proto::SupRequest::GlobalList {})
        .await
        .ok()?;
    Some(
        list.as_array()?
            .iter()
            .filter_map(|e| {
                Some((
                    e["name"].as_str()?.to_string(),
                    e["peer_connected"].as_bool().unwrap_or(false),
                ))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::machine::{Capabilities, LabServices, MachineKind, MachineStatus};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    /// The property ADR-0002 exists to hold, checked against the source rather
    /// than asserted in a comment.
    ///
    /// The last time this was a comment ("`start` and `restore` … are the only
    /// places left that know a machine's kind") it decayed to seven further
    /// branches before anyone noticed. Orchestration addresses machines through
    /// [`Machine`]; when it needs to know *what a machine can do* it probes a
    /// capability. Asking *what kind it is* is how the duplication came back.
    ///
    /// Reaching a concrete type counts: `if let Ok(vm) = self.vm(n)` decides by
    /// kind just as surely as a match does.
    ///
    /// Two exemptions, both explicit and greppable in the source:
    /// `LabRuntime::vm`/`container` themselves, between `BEGIN`/`END
    /// kind-aware accessors` markers — they exist *to* reject the other kind's
    /// name; and single statements preceded by a `// kind-aware:` comment
    /// giving the reason, for the deferred-pull paths that must reach a
    /// concrete type to bind the artefact they just downloaded. Neither
    /// decides *behaviour* by kind.
    #[test]
    fn orchestration_never_branches_on_machine_kind() {
        let source = include_str!("lab.rs");
        let body = &source[..source
            .find("#[cfg(test)]\nmod tests {")
            .expect("this test module")];
        // One explicit, greppable exemption — see the block it delimits.
        let (before, rest) = body
            .split_once("// ---- BEGIN kind-aware accessors")
            .expect("the exemption markers");
        let (_, after) = rest
            .split_once("// ---- END kind-aware accessors")
            .expect("the exemption end marker");
        let scanned = format!("{before}{after}");
        // Isolated statements that must reach a concrete type to bind an
        // artefact they just downloaded carry a `kind-aware:` marker on the
        // line above, with the reason. Skip those.
        let scanned: String = scanned
            .lines()
            .scan(false, |exempt_next, line| {
                let exempt = *exempt_next;
                *exempt_next = line.contains("// kind-aware:") || (exempt && line.contains("//"));
                Some(if exempt { "" } else { line })
            })
            .collect::<Vec<_>>()
            .join("\n");
        let banned = [
            // A match or comparison on the reported kind.
            "MachineKind::",
            // "is this name a container?" — the shape every one of the seven
            // branches took.
            "containers.contains_key",
            "vms.contains_key",
            "containers.get(",
            "vms.get(",
            // Reaching a concrete type *is* a kind branch: `if let Ok(vm) =
            // self.vm(n)` decides by kind just as surely as a match does, and
            // that is exactly how the `pre_provision` hook stayed VM-only
            // through the first draft of this test.
            "self.vm(",
            "self.container(",
            "me.vm(",
            "me.container(",
        ];
        let offenders: Vec<(usize, &str, &str)> = scanned
            .lines()
            .enumerate()
            .flat_map(|(i, line)| {
                banned
                    .iter()
                    .filter(move |b| line.contains(**b))
                    .map(move |b| (i + 1, *b, line.trim()))
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "the lab runtime branches on machine kind again — express the difference \
             as a capability on `Machine`, or as implementation behind it (ADR-0002):\n{}",
            offenders
                .iter()
                .map(|(n, b, l)| format!("  src/labd/lab.rs:{n}: {b} in `{l}`"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    // ---- machine doubles -----------------------------------------------------

    /// A machine that boots instantly and records what happened to it, so
    /// wave ordering, readiness gating and teardown can be driven without a
    /// hypervisor, a template or a network.
    struct FakeMachine {
        name: String,
        kind: MachineKind,
        ready_timeout: Duration,
        /// How long `start` takes, so a wave's overlap is observable.
        boot: Duration,
        /// How long after `start` returns this machine reports ready.
        settle: Duration,
        state: tokio::sync::RwLock<PowerState>,
        /// The agent has answered — set just before the lab's `before_ready`
        /// runs, as both real adapters do.
        agent_up: tokio::sync::RwLock<bool>,
        ready: tokio::sync::RwLock<bool>,
        starts: AtomicU32,
        stops: AtomicU32,
        /// Global order of events, shared by every machine in the lab.
        log: Arc<Mutex<Vec<String>>>,
        seq: Arc<AtomicUsize>,
        dir: PathBuf,
        /// A guest agent answering on this machine's behalf, for the
        /// questions that are about what the agent said (§19.4). `None` — the
        /// usual case for a double — is a machine with no agent at all.
        agent: Option<super::super::vm_agent::AgentHandle>,
        /// A first-boot script still to run; until it has, the machine is
        /// not ready, as a real one is not (§6.1).
        first_boot: std::sync::Mutex<Option<String>>,
        /// A share "data" that takes this long to mount once the machine is
        /// ready, under the same wait for readiness a VM's mount task makes.
        mount: Option<Duration>,
        /// Snapshots it takes are online ones (RAM included).
        online_snapshots: bool,
        clock: crate::config::model::Clock,
        /// What `boot_id` answers, in turn; the last answer repeats. Empty is
        /// a machine with no boot identity to read.
        boot_ids: std::sync::Mutex<Vec<Result<&'static str, &'static str>>>,
        /// Host USB devices it holds, when it is a VM booted with
        /// `usb = true`; `None` keeps the trait's refusal.
        usb: Option<std::sync::Mutex<Vec<crate::usb::Attachment>>>,
        /// A display whose VNC socket is a fake RFB server, for the console
        /// recording tests (§11.1); `None` is a machine with no display.
        display: Option<Arc<FakeDisplayHost>>,
    }

    /// What a displayed double tells its [`Display`]: a VNC socket (a fake
    /// server's) and an input tap. Its input and frames capabilities are
    /// never reached.
    struct FakeDisplayHost {
        name: String,
        vnc: PathBuf,
        tap: Arc<crate::record::InputTap>,
    }

    #[async_trait::async_trait]
    impl super::super::display::DisplayHost for FakeDisplayHost {
        fn name(&self) -> &str {
            &self.name
        }
        async fn input(&self) -> Result<Arc<dyn super::super::hypervisor::Input>> {
            Err(anyhow!("fake display has no input capability"))
        }
        async fn frames(&self) -> Result<Arc<dyn super::super::hypervisor::Frames>> {
            Err(anyhow!("fake display has no frames capability"))
        }
        fn vnc_sock(&self) -> PathBuf {
            self.vnc.clone()
        }
        fn capture_dir(&self) -> PathBuf {
            self.vnc.parent().unwrap().to_path_buf()
        }
        fn input_transport(&self) -> crate::profiles::InputTransport {
            crate::profiles::InputTransport::Vnc
        }
        fn gl_display(&self) -> bool {
            false
        }
        fn input_tap(&self) -> Arc<crate::record::InputTap> {
            self.tap.clone()
        }
    }

    impl FakeMachine {
        /// A VM double with a display served by the fake RFB server at `vnc`.
        fn displayed(name: &str, shared: &Shared, vnc: &std::path::Path) -> Arc<Self> {
            let mut m =
                Arc::into_inner(Self::new(name, MachineKind::Vm, shared)).expect("sole owner");
            m.display = Some(Arc::new(FakeDisplayHost {
                name: name.to_string(),
                vnc: vnc.to_path_buf(),
                tap: Arc::default(),
            }));
            Arc::new(m)
        }

        fn new(name: &str, kind: MachineKind, shared: &Shared) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
                kind,
                ready_timeout: Duration::from_secs(300),
                boot: Duration::from_millis(0),
                settle: Duration::from_millis(0),
                state: tokio::sync::RwLock::new(PowerState::Stopped),
                agent_up: tokio::sync::RwLock::new(false),
                ready: tokio::sync::RwLock::new(false),
                starts: AtomicU32::new(0),
                stops: AtomicU32::new(0),
                log: shared.log.clone(),
                seq: shared.seq.clone(),
                dir: shared.dir.join(name),
                agent: None,
                first_boot: std::sync::Mutex::new(None),
                mount: None,
                online_snapshots: false,
                clock: crate::config::model::Clock::Host,
                boot_ids: std::sync::Mutex::new(Vec::new()),
                usb: None,
                display: None,
            })
        }

        /// A running VM double with a USB controller.
        fn usb_vm(name: &str, shared: &Shared) -> Arc<Self> {
            let mut m =
                Arc::into_inner(Self::new(name, MachineKind::Vm, shared)).expect("sole owner");
            m.usb = Some(std::sync::Mutex::new(Vec::new()));
            *m.state.get_mut() = PowerState::Running;
            Arc::new(m)
        }

        /// A running double whose snapshots are online, declaring `clock`.
        fn clocked(name: &str, shared: &Shared, clock: crate::config::model::Clock) -> Arc<Self> {
            let mut m =
                Arc::into_inner(Self::new(name, MachineKind::Vm, shared)).expect("sole owner");
            m.online_snapshots = true;
            m.clock = clock;
            Arc::new(m)
        }

        /// A double with a share that takes `mount` to mount, a readiness
        /// budget of `ready_timeout`, and optionally a first-boot script.
        fn sharing(
            name: &str,
            shared: &Shared,
            mount: Duration,
            ready_timeout: Duration,
            first_boot: Option<&str>,
        ) -> Arc<Self> {
            let mut m =
                Arc::into_inner(Self::new(name, MachineKind::Vm, shared)).expect("sole owner");
            m.mount = Some(mount);
            m.ready_timeout = ready_timeout;
            m.first_boot = std::sync::Mutex::new(first_boot.map(str::to_string));
            Arc::new(m)
        }

        /// A double that starts but never reports ready — the guest of an
        /// installer-driven build, which cannot answer a handshake until the
        /// provision that installs the OS has run.
        fn never_ready(name: &str, kind: MachineKind, shared: &Shared) -> Arc<Self> {
            let mut m = Arc::into_inner(Self::new(name, kind, shared)).expect("sole owner");
            m.settle = Duration::from_secs(86_400);
            Arc::new(m)
        }

        async fn note(&self, what: &str) {
            let n = self.seq.fetch_add(1, Ordering::SeqCst);
            self.log
                .lock()
                .await
                .push(format!("{n}:{}:{what}", self.name));
        }
    }

    #[derive(Clone)]
    struct Shared {
        log: Arc<Mutex<Vec<String>>>,
        seq: Arc<AtomicUsize>,
        dir: PathBuf,
    }

    #[async_trait::async_trait]
    impl Machine for FakeMachine {
        fn as_machine(&self) -> &dyn Machine {
            self
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> MachineKind {
            self.kind
        }
        fn arch(&self) -> String {
            "x86_64".into()
        }
        fn guest_os(&self) -> super::super::guest_os::GuestOs {
            super::super::guest_os::GuestOs::Linux
        }
        /// A double runs no guest, so it names no profile and anything
        /// resolved through one lands on its floor.
        fn profile(&self) -> Option<String> {
            None
        }
        fn nics(&self) -> &[crate::config::model::Nic] {
            &[]
        }
        fn macs(&self) -> &[crate::config::model::MacAddr] {
            &[]
        }
        fn logins(&self) -> &[crate::config::model::Login] {
            &[]
        }
        fn term_session_sock(&self, _id: u32) -> PathBuf {
            self.dir.join("term.sock")
        }
        fn nic_sock(&self, i: usize) -> PathBuf {
            self.dir.join(format!("nic{i}.sock"))
        }
        /// A double has no host behind it, and no shares to place either.
        fn virtiofsd(&self) -> Option<crate::qemu::virtiofsd::Virtiofsd> {
            None
        }
        fn event_subject(&self) -> &'static str {
            match self.kind {
                MachineKind::Vm => "vm",
                MachineKind::Container => "container",
            }
        }
        fn local_dir(&self) -> &std::path::Path {
            &self.dir
        }
        fn run_dir(&self) -> &std::path::Path {
            &self.dir
        }
        fn disk_paths(&self) -> Vec<PathBuf> {
            vec![self.dir.join("disk0.qcow2")]
        }
        fn required_binaries(&self) -> Vec<String> {
            // Something every host running the test suite has, so preflight is
            // exercised rather than skipped.
            vec!["sh".to_string()]
        }
        fn ready_timeout(&self) -> Duration {
            self.ready_timeout
        }

        async fn state(&self) -> PowerState {
            *self.state.read().await
        }
        async fn is_ready(&self) -> bool {
            *self.ready.read().await && self.first_boot.lock().unwrap().is_none()
        }
        fn pending_first_boot(&self) -> Option<super::super::machine::FirstBootProvision> {
            let source = self.first_boot.lock().unwrap().clone()?;
            Some(super::super::machine::FirstBootProvision {
                template: "t".into(),
                script: crate::scripting::EmbeddedWscript {
                    source,
                    surface_version: Some(crate::scripting::WSCRIPT_SURFACE_VERSION),
                },
            })
        }
        async fn first_boot_done(&self) -> Result<()> {
            *self.first_boot.lock().unwrap() = None;
            Ok(())
        }
        async fn mount_shares(
            self: Arc<Self>,
            _lab: Arc<dyn LabServices>,
            report: Arc<MountReport>,
        ) {
            let Some(mount) = self.mount else { return };
            report.planned(["data"], &[], 30);
            if self.wait_ready(self.ready_timeout).await.is_err() {
                report.abandoned("the guest never became ready");
                return;
            }
            report.ready();
            tokio::time::sleep(mount).await;
            self.note("mounted").await;
            report.mounted("data");
        }
        async fn stop(&self, _force: bool) -> Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.note("stop").await;
            *self.state.write().await = PowerState::Stopped;
            *self.agent_up.write().await = false;
            *self.ready.write().await = false;
            Ok(())
        }

        /// Readiness the way both adapters give it: the agent answers, the
        /// lab's `before_ready` settles what it owes the boot, then ready.
        async fn start(self: Arc<Self>, lab: Arc<dyn LabServices>) -> Result<()> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.note("start").await;
            *self.state.write().await = PowerState::Starting;
            tokio::time::sleep(self.boot).await;
            // The paused window a real VM offers before its CPUs run (D20).
            if let Err(e) = lab.before_resume(&self.name).await {
                *self.state.write().await = PowerState::Stopped;
                return Err(e);
            }
            let recording = self.display.as_ref().is_some_and(|d| d.tap.attached());
            self.note(&format!("resume:recording={recording}")).await;
            *self.state.write().await = PowerState::Running;
            let me = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(me.settle).await;
                *me.agent_up.write().await = true;
                lab.before_ready(&me.name).await;
                *me.ready.write().await = true;
                me.note("ready").await;
            });
            Ok(())
        }

        async fn restore(
            self: Arc<Self>,
            _lab: Arc<dyn LabServices>,
            snap: &str,
            _online: bool,
        ) -> Result<()> {
            self.note(&format!("restore:{snap}")).await;
            Ok(())
        }

        fn display(self: Arc<Self>) -> Option<super::super::display::Display> {
            let host = self.display.as_ref()?.clone();
            Some(super::super::display::Display::new(host))
        }

        /// A VM double seals an agent, as a template built with one does; a
        /// container double's agent ships with the host, as a real one's.
        fn sealed_agent_version(&self) -> Option<String> {
            (self.kind == MachineKind::Vm).then(|| "agent=sealed".to_string())
        }
        fn agent_origin(&self) -> crate::labd::machine::AgentOrigin {
            match self.kind {
                MachineKind::Vm => crate::labd::machine::AgentOrigin::Image,
                MachineKind::Container => crate::labd::machine::AgentOrigin::HostAsset,
            }
        }
        async fn agent(&self) -> Result<super::super::vm_agent::AgentHandle> {
            self.agent
                .clone()
                .ok_or_else(|| anyhow!("{}: no agent in a double", self.name))
        }
        async fn agent_answering(&self) -> bool {
            *self.agent_up.read().await
        }
        async fn boot_id(&self) -> Result<String> {
            let mut ids = self.boot_ids.lock().unwrap();
            let next = match ids.len() {
                0 => Err("no boot identity in a double"),
                1 => ids[0],
                _ => ids.remove(0),
            };
            next.map(String::from).map_err(|e| anyhow!(e))
        }
        async fn snapshot(&self, _name: &str) -> Result<bool> {
            Ok(self.online_snapshots)
        }
        async fn usb_ready(&self) -> Result<()> {
            if self.usb.is_none() {
                bail!(crate::usb::container_refusal(&self.name));
            }
            if *self.state.read().await != PowerState::Running {
                bail!("\"{}\" is not running", self.name);
            }
            Ok(())
        }
        async fn usb_attach(
            &self,
            plan: &crate::usb::AttachPlan,
        ) -> Result<crate::usb::Attachment> {
            self.usb_ready().await?;
            let a = crate::usb::Attachment {
                machine: self.name.clone(),
                qdev_id: plan.device.qdev_id(),
                device: plan.device.clone(),
            };
            self.usb.as_ref().unwrap().lock().unwrap().push(a.clone());
            self.note(&format!("usb_attach:{}", a.device.port)).await;
            Ok(a)
        }
        async fn usb_detach(&self, qdev_id: &str) -> Result<crate::usb::Attachment> {
            let gone = {
                let mut held = self.usb.as_ref().unwrap().lock().unwrap();
                let i = held
                    .iter()
                    .position(|a| a.qdev_id == qdev_id)
                    .ok_or_else(|| anyhow!("no {qdev_id}"))?;
                held.remove(i)
            };
            self.note(&format!("usb_detach:{}", gone.device.port)).await;
            Ok(gone)
        }
        async fn usb_attached(&self) -> Vec<crate::usb::Attachment> {
            self.usb
                .as_ref()
                .map(|u| u.lock().unwrap().clone())
                .unwrap_or_default()
        }
        fn clock(&self) -> crate::config::model::Clock {
            self.clock
        }
        async fn delete_snapshot(&self, _name: &str) -> Result<()> {
            Ok(())
        }
        async fn status_detail(&self) -> crate::status::MachineDetail {
            crate::status::MachineDetail::Container(crate::status::ContainerStatus {
                image: "double:1".into(),
                digest: None,
                health: None,
                exit_code: None,
            })
        }
    }

    fn shared(dir: &std::path::Path) -> Shared {
        Shared {
            log: Arc::new(Mutex::new(Vec::new())),
            seq: Arc::new(AtomicUsize::new(0)),
            dir: dir.to_path_buf(),
        }
    }

    /// A lab runtime whose machines are doubles. Everything else is real: the
    /// plan, the waves, the readiness gate, the teardown.
    fn lab_of(
        dir: &std::path::Path,
        src: &str,
        machines: Vec<Arc<dyn Machine>>,
    ) -> Arc<LabRuntime> {
        let config = crate::config::load_lab_source(src, "<test>", dir).expect("lab source");
        LabRuntime::with_machines(config, machines).expect("runtime")
    }

    fn quiet() -> crate::scripting::OutputSink {
        Arc::new(|_| {})
    }

    /// A main checkout at `<tmp>/main` frozen at generation 1, and a linked
    /// worktree `feat-x` beside it, laid out as `git worktree add` leaves
    /// them. Returns the two lab roots.
    fn frozen_family(tmp: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let main = tmp.join("main");
        let wt_git = main.join(".git/worktrees/feat-x");
        let wt = tmp.join("feat-x");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        crate::freeze::WorktreeConfig {
            generation: 1,
            snapshot: "base".into(),
        }
        .save(&crate::paths::lab_local_dir(&main))
        .unwrap();
        (main, wt)
    }

    /// A worktree lab peering a global segment with another host is refused
    /// at `up`, naming §21.4 (ADR-0019 amending ADR-0011); the frozen main
    /// lab declaring the same peer is not.
    #[tokio::test]
    async fn a_worktree_lab_with_a_global_peer_is_refused_at_up() {
        let tmp = tempfile::tempdir().unwrap();
        let (main, wt) = frozen_family(tmp.path());
        let src = r#"import <vmlab.wcl>
lab "t" {
  segment "corp" { global = true connect { host = "otherhost:13947" } }
}"#;
        let lab = lab_of(&wt, src, Vec::new());
        let err = lab.up(&[], quiet()).await.expect_err("refused");
        assert!(format!("{err:#}").contains("§21.4"), "{err:#}");
        assert!(format!("{err:#}").contains("\"corp\""), "{err:#}");

        let lab = lab_of(&main, src, Vec::new());
        assert_eq!(lab.isolation.as_ref().unwrap().refusal(), None);
    }

    /// The status projection carries the decision: present in both labs of
    /// a frozen family, the worktree's global segment renamed, absent from a
    /// lab that is not frozen.
    #[tokio::test]
    async fn status_reports_worktree_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        let (main, wt) = frozen_family(tmp.path());
        let src = r#"import <vmlab.wcl>
lab "t" {
  segment "corp" { global = true }
}"#;
        let main_status = lab_of(&main, src, Vec::new()).status().await;
        let iso = main_status.isolation.expect("main lab isolated");
        assert_eq!((iso.role.as_str(), iso.worktree), ("main", None));
        assert_eq!(iso.forward_bind, "127.0.0.1");

        let wt_lab = lab_of(&wt, src, Vec::new());
        assert_eq!(
            wt_lab.network.lock().await.segments["corp"].global_name,
            "corp@feat-x"
        );
        let iso = wt_lab.status().await.isolation.expect("worktree isolated");
        assert_eq!(iso.worktree.as_deref(), Some("feat-x"));

        let plain = tempfile::tempdir().unwrap();
        assert_eq!(
            lab_of(plain.path(), src, Vec::new())
                .status()
                .await
                .isolation,
            None
        );
    }

    /// A freeze or thaw runs inside the main lab's own daemon, which decided
    /// isolation when it started. Its status says it is stale, so the next
    /// boot verb replaces it and the main lab comes up isolated (§21.4).
    #[tokio::test]
    async fn a_freeze_or_thaw_after_start_makes_the_daemon_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let (main, _wt) = frozen_family(tmp.path());
        let src = r#"import <vmlab.wcl>
lab "t" {
  segment "corp" { global = true }
}"#;
        let main_local = crate::paths::lab_local_dir(&main);
        let record = crate::freeze::WorktreeConfig::load(&main_local)
            .unwrap()
            .expect("frozen");

        crate::freeze::WorktreeConfig::remove(&main_local).unwrap();
        let lab = lab_of(&main, src, Vec::new());
        assert_eq!(lab.status().await.isolation, None);
        assert!(!lab.status().await.freeze_stale);

        record.save(&main_local).unwrap();
        assert!(lab.status().await.freeze_stale, "frozen since start");

        let lab = lab_of(&main, src, Vec::new());
        assert!(!lab.status().await.freeze_stale);
        crate::freeze::WorktreeConfig::remove(&main_local).unwrap();
        assert!(lab.status().await.freeze_stale, "thawed since start");
    }

    #[tokio::test]
    async fn crash_handler_can_explicitly_start_a_stopped_container() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let container = FakeMachine::new("web", MachineKind::Container, &sh);
        std::fs::write(
            dir.path().join("restart.ws"),
            r#"use vmlab

fn handle(event: Event, lab: Lab) {
    let Ok(machine) = lab.machine(event.vm) else { return }
    let started = machine.start()
}
"#,
        )
        .unwrap();
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  container "web" { image = "web:1" }
  on "container.crashed" { run = "restart.ws" targets = ["web"] }
}
"#,
            vec![container.clone()],
        );
        let event = crate::proto::Event::new(
            "container.crashed",
            "t",
            json!({"container": "web", "exit_code": 1}),
        );

        let runs = super::super::matching_handler_runs(&lab, &event);
        assert_eq!(runs.len(), 1, "the declared crash handler must match");
        for (script, event) in runs {
            crate::scripting::run_event_handler(lab.clone(), &script, event, quiet()).await;
        }

        assert_eq!(container.starts.load(Ordering::SeqCst), 1);
        assert_eq!(container.state().await, PowerState::Running);
    }

    /// A running, ready double whose boot identity answers `ids` in turn.
    async fn booted(
        name: &str,
        shared: &Shared,
        ids: Vec<Result<&'static str, &'static str>>,
    ) -> Arc<FakeMachine> {
        let m = FakeMachine::new(name, MachineKind::Vm, shared);
        *m.boot_ids.lock().unwrap() = ids;
        *m.state.write().await = PowerState::Running;
        *m.agent_up.write().await = true;
        *m.ready.write().await = true;
        m
    }

    const REBOOT_SCRIPT: &str = r#"use vmlab

fn main(lab: Lab) {
    let Ok(m) = lab.machine("box") else { return }
    let before = m.boot_id().expect("boot id")
    lab.log("before " + before)
    match m.wait_rebooted(before, 4) {
        Ok(_) => lab.log("after " + m.boot_id().expect("boot id")),
        Err(e) => lab.log("not rebooted: " + e),
    }
}
"#;

    async fn run_reboot_script(m: Arc<FakeMachine>) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let lab = lab_of(
            dir.path(),
            "import <vmlab.wcl>\nlab \"t\" { vm \"box\" { template = \"x86_64/t\" } }",
            vec![m],
        );
        let said = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: crate::scripting::OutputSink = {
            let said = said.clone();
            Arc::new(move |s: String| said.lock().unwrap().push(s.trim_end().to_string()))
        };
        crate::scripting::run_script_source(
            lab,
            REBOOT_SCRIPT.into(),
            "reboot.ws",
            dir.path().to_path_buf(),
            None,
            None,
            sink,
        )
        .await
        .expect("script");
        said.lock().unwrap().clone()
    }

    /// `wait_rebooted` rides out a guest still on its old boot and one that
    /// is down, and returns once the identity changes (§10.3).
    #[tokio::test]
    async fn wait_rebooted_sees_the_identity_change_through_a_down_guest() {
        let sh = shared(tempfile::tempdir().unwrap().path());
        let m = booted(
            "box",
            &sh,
            vec![Ok("a"), Ok("a"), Err("agent: not connected"), Ok("b")],
        )
        .await;
        let said = run_reboot_script(m).await;
        assert!(said.iter().any(|s| s == "before a"), "{said:?}");
        assert!(said.iter().any(|s| s == "after b"), "{said:?}");
    }

    /// A guest that never reboots times `wait_rebooted` out, naming the
    /// identity it kept seeing.
    #[tokio::test]
    async fn wait_rebooted_times_out_with_the_last_identity_seen() {
        let sh = shared(tempfile::tempdir().unwrap().path());
        let m = booted("box", &sh, vec![Ok("a")]).await;
        let said = run_reboot_script(m).await;
        assert!(
            said.iter()
                .any(|s| s == "not rebooted: box: no reboot seen within 4s (boot id still `a`)"),
            "{said:?}"
        );
    }

    /// `@dev` reaches every surface through the status projection (§19.1,
    /// ADR-0004): the runtime resolves it once here, for both machine kinds,
    /// and nothing downstream re-reads the lab file to find the dev machine.
    #[tokio::test]
    async fn status_carries_the_dev_machines_and_names_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let dev01 = FakeMachine::new("dev01", MachineKind::Vm, &sh);
        let buildbox = FakeMachine::new("buildbox", MachineKind::Container, &sh);
        let db = FakeMachine::new("db", MachineKind::Container, &sh);
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  @dev(default = true, workspace = "./src") vm "dev01" { template = "x86_64/t" }
  @dev container "buildbox" { image = "sdk:9.0" }
  container "db" { image = "db:1" }
}"#,
            vec![dev01, buildbox, db],
        );

        let status = lab.status().await;
        let dev = |name: &str| {
            status
                .machines
                .iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .dev
                .clone()
        };
        let declared = dev("dev01").expect("dev01 carries @dev");
        assert!(declared.default);
        assert_eq!(declared.workspace.as_deref(), Some("./src"));
        // A double names no profile, so the guest path is vmlab's floor.
        assert_eq!(declared.workspace_guest, crate::dev::WORKSPACE_GUEST_FLOOR);

        let bare = dev("buildbox").expect("buildbox carries @dev");
        assert!(!bare.default, "the declared default wins");
        assert!(bare.workspace.is_none());

        assert!(
            dev("db").is_none(),
            "an undecorated machine is not a dev machine"
        );
        assert_eq!(
            status
                .dev_machines()
                .filter(|(_, d)| d.default)
                .map(|(m, _)| m.name.as_str())
                .collect::<Vec<_>>(),
            ["dev01"],
            "exactly one machine is the lab's default"
        );
    }

    /// Who `up`'s agent refresh asks at all (§19.4), through the real `up`:
    /// a VM double seals an agent but has none answering, so a refresh that
    /// is attempted ends in a warning — and one that is not says nothing.
    /// `agent_update` on the `vm` beats the `lab`'s, a container is never
    /// asked, and `up` succeeds either way.
    #[tokio::test]
    async fn up_refreshes_the_agents_the_lab_file_lets_it_and_never_fails() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let src = r#"import <vmlab.wcl>
lab "t" {
  agent_update = false
  vm "inherits" { template = "x86_64/t" }
  vm "insists" { template = "x86_64/t" agent_update = true }
  container "web" { image = "web:1" }
}"#;
        let lab = lab_of(
            dir.path(),
            src,
            vec![
                FakeMachine::new("inherits", MachineKind::Vm, &sh),
                FakeMachine::new("insists", MachineKind::Vm, &sh),
                FakeMachine::new("web", MachineKind::Container, &sh),
            ],
        );
        lab.agent_updates
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let said = Arc::new(std::sync::Mutex::new(String::new()));
        let sink: crate::scripting::OutputSink = {
            let said = said.clone();
            Arc::new(move |s: String| said.lock().unwrap().push_str(&s))
        };
        let asset = dir.path().join("vmlab-agent");
        std::fs::write(&asset, b"agent").unwrap();
        let shipped = move |_: &dyn Machine| {
            Ok(crate::agent_asset::AgentAsset {
                path: asset.clone(),
                version: "agent=shipped".into(),
            })
        };

        for m in lab.machines().collect::<Vec<_>>() {
            Arc::clone(&m).start(lab.services()).await.unwrap();
            lab.refresh_agent_with(&m, &sink, &shipped).await;
        }
        let out = said.lock().unwrap().clone();
        assert!(
            out.contains("warning: agent: could not update \"insists\""),
            "the vm's own `agent_update = true` beats the lab's: {out}"
        );
        assert!(!out.contains("\"inherits\""), "the lab's opt-out: {out}");
        assert!(
            !out.contains("\"web\""),
            "a container is never asked: {out}"
        );

        // A template build turns the refresh off for everything.
        said.lock().unwrap().clear();
        lab.agent_updates
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let insists = lab.machine("insists").unwrap();
        lab.refresh_agent_with(&insists, &sink, &shipped).await;
        assert_eq!(*said.lock().unwrap(), "");

        // No asset for the guest's os/arch is passed over silently.
        lab.agent_updates
            .store(true, std::sync::atomic::Ordering::Relaxed);
        lab.refresh_agent_with(&insists, &sink, |_: &dyn Machine| {
            Err(anyhow!("no agent asset"))
        })
        .await;
        assert_eq!(*said.lock().unwrap(), "");

        // And the real `up` survives the failed refresh.
        lab.up(&[], sink.clone())
            .await
            .expect("up never fails on a refresh");
    }

    /// `up` does not start waiting on a machine nothing waits for just to
    /// refresh its agent (§19.4): a guest that can never answer — an unsigned
    /// bootloader under secure boot — would otherwise hold `up` for the whole
    /// ready timeout. Its refresh is owed to its handshake instead, and the
    /// machine's `before_ready` claims it. A machine that cannot be refreshed
    /// at all — here a container, whose agent ships with the host — is never
    /// owed one, so nothing ever holds its readiness.
    #[tokio::test]
    async fn a_machine_nothing_waits_on_is_refreshed_at_its_ready_not_by_blocking_up() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  vm "silent" { template = "x86_64/t" }
  container "web" { image = "nginx:1" }
}"#,
            vec![
                FakeMachine::never_ready("silent", MachineKind::Vm, &sh),
                FakeMachine::never_ready("web", MachineKind::Container, &sh),
            ],
        );
        lab.agent_updates
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *lab.shipped_agent.write().unwrap() = Arc::new(|_: &dyn Machine| Err(anyhow!("no asset")));

        tokio::time::timeout(Duration::from_secs(10), lab.up(&[], quiet()))
            .await
            .expect("up returns without waiting on the agent")
            .expect("up");
        assert!(
            lab.deferred_refresh.lock_recover().contains_key("silent"),
            "the refresh is owed to the machine's handshake"
        );
        assert!(
            !lab.deferred_refresh.lock_recover().contains_key("web"),
            "a container is never owed a refresh"
        );

        tokio::time::timeout(
            Duration::from_secs(10),
            super::super::machine::LabServices::before_ready(lab.as_ref(), "silent"),
        )
        .await
        .expect("a refresh that cannot run does not hold readiness");
        assert!(
            !lab.deferred_refresh.lock_recover().contains_key("silent"),
            "the handshake claims it, once"
        );
    }

    /// `up` refreshes the agent of a machine it waits on itself, inline, so
    /// what the refresh says reaches `up`'s output (§19.4). The machine's
    /// handshake must still not report it ready until that refresh is done:
    /// a status poll or a second `exec` let in early lands on the agent the
    /// refresh is restarting.
    #[tokio::test]
    async fn a_machine_up_waits_on_is_not_ready_before_its_inline_refresh_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  vm "db" { template = "x86_64/t" }
  vm "app" { template = "x86_64/t" depends_on = ["db"] }
}"#,
            vec![
                FakeMachine::new("db", MachineKind::Vm, &sh),
                FakeMachine::new("app", MachineKind::Vm, &sh),
            ],
        );
        lab.agent_updates
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let asset = dir.path().join("vmlab-agent");
        std::fs::write(&asset, b"agent").unwrap();
        *lab.shipped_agent.write().unwrap() = Arc::new(move |_: &dyn Machine| {
            Ok(crate::agent_asset::AgentAsset {
                path: asset.clone(),
                version: "agent=shipped".into(),
            })
        });
        // What `up` says, numbered on the same clock as the machines' log.
        let said = Arc::new(std::sync::Mutex::new(Vec::<(usize, String)>::new()));
        let sink: crate::scripting::OutputSink = {
            let said = said.clone();
            let seq = sh.seq.clone();
            Arc::new(move |s: String| {
                let n = seq.fetch_add(1, Ordering::SeqCst);
                said.lock().unwrap().push((n, s));
            })
        };

        tokio::time::timeout(Duration::from_secs(30), lab.up(&[], sink))
            .await
            .expect("up finishes")
            .expect("up never fails on a refresh");

        // The double has no agent to push into, so the refresh ends in its
        // warning — which is as finished as a refresh gets.
        let refreshed = said
            .lock()
            .unwrap()
            .iter()
            .find(|(_, s)| s.contains("could not update \"db\""))
            .map(|(n, _)| *n)
            .expect("up refreshed db inline, into its own output");
        let ready = sh
            .log
            .lock()
            .await
            .iter()
            .find_map(|l| {
                l.strip_suffix(":db:ready")
                    .and_then(|n| n.parse::<usize>().ok())
            })
            .expect("db became ready");
        assert!(
            refreshed < ready,
            "db was reported ready (#{ready}) before its refresh finished (#{refreshed})"
        );
        assert!(
            !lab.deferred_refresh.lock_recover().contains_key("db"),
            "the hold is released once the refresh is done"
        );
    }

    /// The `vm` block's `agent_update` beats the `lab` block's, and the floor
    /// is on.
    #[test]
    fn agent_update_resolves_vm_then_lab_then_on() {
        let cfg = |src: &str| {
            crate::config::load_lab_source(src, "<test>", std::path::Path::new("/tmp"))
                .expect("lab source")
                .lab
        };
        let lab = cfg(r#"import <vmlab.wcl>
lab "t" {
  vm "a" { template = "x86_64/t" }
  vm "b" { template = "x86_64/t" agent_update = false }
}"#);
        assert!(lab.agent_update_for("a"));
        assert!(!lab.agent_update_for("b"));
        let lab = cfg(r#"import <vmlab.wcl>
lab "t" {
  agent_update = false
  vm "a" { template = "x86_64/t" }
  vm "b" { template = "x86_64/t" agent_update = true }
}"#);
        assert!(!lab.agent_update_for("a"));
        assert!(lab.agent_update_for("b"));
    }

    /// A machine the repair verb changed in place is reported diverged
    /// wherever machine state is (§19.4), and the record survives a daemon
    /// that reloads the lab's state — divergence outlives the process that
    /// caused it.
    ///
    /// Destroying the machine's disks forgets it, because the divergence
    /// *was* those disks: what comes back is a clone of the sealed template
    /// again, running the agent that template baked.
    #[tokio::test]
    async fn a_repaired_machine_is_diverged_until_its_disks_are_destroyed() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let dev01 = FakeMachine::new("dev01", MachineKind::Vm, &sh);
        let db = FakeMachine::new("db", MachineKind::Container, &sh);
        let src = r#"import <vmlab.wcl>
lab "t" {
  container "dev01" { image = "a:1" }
  container "db" { image = "b:1" }
}"#;
        let lab = lab_of(dir.path(), src, vec![dev01.clone(), db.clone()]);

        let diverged = |status: &LabStatus, name: &str| {
            status
                .machines
                .iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .agent_diverged
        };
        assert!(
            !diverged(&lab.status().await, "dev01"),
            "nothing diverges by itself"
        );

        lab.record_agent_repair("dev01", "agent=abc")
            .await
            .expect("record the repair");
        let status = lab.status().await;
        assert!(diverged(&status, "dev01"));
        assert!(!diverged(&status, "db"), "one machine, not the lab");

        // A second daemon over the same lab directory reads the same answer.
        let reopened = lab_of(
            dir.path(),
            src,
            vec![
                FakeMachine::new("dev01", MachineKind::Vm, &sh),
                FakeMachine::new("db", MachineKind::Container, &sh),
            ],
        );
        assert!(diverged(&reopened.status().await, "dev01"));

        lab.destroy_machine("dev01").await.expect("destroy");
        assert!(!diverged(&lab.status().await, "dev01"));
    }

    /// `depends_on` gates on readiness identically for both kinds. `web`
    /// depends on `db`, so `db` must be *ready* — not merely started — before
    /// `web` starts, whether `db` is a VM or a container.
    #[tokio::test]
    async fn depends_on_gates_on_readiness_for_both_kinds() {
        for kind in [MachineKind::Vm, MachineKind::Container] {
            let dir = tempfile::tempdir().unwrap();
            let sh = shared(dir.path());
            let db = FakeMachine::new("db", kind, &sh);
            let web = FakeMachine::new("web", MachineKind::Container, &sh);
            let lab = lab_of(
                dir.path(),
                r#"import <vmlab.wcl>
lab "t" {
  container "db" { image = "db:1" }
  container "web" { image = "web:1" depends_on = ["db"] }
}"#,
                vec![db.clone(), web.clone()],
            );
            lab.up(&[], quiet()).await.expect("up");

            let log = sh.log.lock().await.clone();
            let idx = |needle: &str| {
                log.iter()
                    .position(|l| l.ends_with(needle))
                    .unwrap_or_else(|| panic!("{needle} missing from {log:?}"))
            };
            assert!(
                idx("db:ready") < idx("web:start"),
                "web started before db was ready: {log:?}"
            );
            assert_eq!(db.starts.load(Ordering::SeqCst), 1);
            assert_eq!(web.starts.load(Ordering::SeqCst), 1);
        }
    }

    /// A build's provisions ARE the installer, so the build waives the
    /// readiness gate in front of them (`provisions_wait_ready`): the agent
    /// whose handshake readiness means only exists once they have run. With
    /// the gate left on, the same lab waits out the machine's whole budget
    /// and fails — which is what every ISO-installed template used to do.
    #[tokio::test(start_paused = true)]
    async fn a_build_does_not_gate_its_provisions_on_readiness() {
        let src = r#"import <vmlab.wcl>
lab "t" { container "build" { image = "x:1" provision "install.ws" { } } }"#;

        for waived in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("install.ws"), "fn main(lab: Lab) { }\n")
                .expect("provision script");
            let sh = shared(dir.path());
            // Never reports ready — an installer's guest, before the install.
            let build = FakeMachine::never_ready("build", MachineKind::Vm, &sh);
            let lab = lab_of(dir.path(), src, vec![build.clone()]);
            if waived {
                lab.provisions_wait_ready
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
            let result = lab.up(&[], quiet()).await;
            if waived {
                result.expect("the provision runs against a guest that is not ready");
            } else {
                let err = result.expect_err("the gate holds a never-ready machine");
                assert!(format!("{err:#}").contains("not ready"), "{err:#}");
            }
        }
    }

    /// A machine nothing depends on does not gate its wave: `up` returns
    /// without waiting out its readiness budget. Before the interface carried
    /// `ready_timeout` this was the difference between a 300s and a 600s
    /// literal at the call site.
    #[tokio::test(start_paused = true)]
    async fn a_leaf_machine_does_not_gate_its_wave() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let leaf = FakeMachine::new("leaf", MachineKind::Vm, &sh);
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" { container "leaf" { image = "x:1" } }"#,
            vec![leaf.clone()],
        );
        let start = tokio::time::Instant::now();
        lab.up(&[], quiet()).await.expect("up");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "up waited on a machine nothing depends on"
        );
    }

    /// A provision script that says `<tag>-begin`, holds for `hold_ms`, then
    /// says `<tag>-end`.
    fn timed_script(dir: &std::path::Path, tag: &str, hold_ms: u64) {
        std::fs::write(
            dir.join(format!("{tag}.ws")),
            format!(
                "use vmlab\n\nfn main(lab: Lab) {{\n    lab.log(\"{tag}-begin\")\n    \
                 vmlab::sleep_ms({hold_ms})\n    lab.log(\"{tag}-end\")\n}}\n"
            ),
        )
        .unwrap();
    }

    /// Where `needle` first appears in what `up` said, by the shared clock.
    fn said_at(said: &Said, needle: &str) -> usize {
        let said = said.lock().unwrap();
        said.iter()
            .find(|(_, s)| s.contains(needle))
            .map(|(n, _)| *n)
            .unwrap_or_else(|| panic!("{needle} missing from {said:?}"))
    }

    /// Machines nothing orders provision in parallel (§7.2), and every line
    /// of their output is led by the machine's name.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn independent_machines_provision_in_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        timed_script(dir.path(), "a", 400);
        timed_script(dir.path(), "b", 400);
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  container "a" { image = "x:1" provision "a.ws" { } }
  container "b" { image = "x:1" provision "b.ws" { } }
}"#,
            vec![
                FakeMachine::new("a", MachineKind::Container, &sh),
                FakeMachine::new("b", MachineKind::Container, &sh),
            ],
        );
        let (sink, said) = numbered(&sh);
        lab.up(&[], sink).await.expect("up");
        assert!(
            said_at(&said, "a: a-begin") < said_at(&said, "b: b-end")
                && said_at(&said, "b: b-begin") < said_at(&said, "a: a-end"),
            "the two provisions did not overlap: {:?}",
            said.lock().unwrap()
        );
        assert!(said_at(&said, "a: provision a.ws") < said_at(&said, "a: a-begin"));
    }

    /// The F12 defect, regressed: a dependent declared *above* its
    /// dependency. The one file-ordered queue stalled at the dependent's step
    /// (its machine not started yet), so the dependency's provision ran only
    /// after the dependent had started — and then after the dependent's own.
    /// `depends_on_provision` now orders them whatever the file order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dependency_declared_below_provisions_first_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        timed_script(dir.path(), "join", 50);
        timed_script(dir.path(), "dc", 300);
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  container "member" {
    image = "x:1"
    depends_on = ["dc"]
    depends_on_provision = ["dc"]
    provision "join.ws" { }
  }
  container "dc" { image = "x:1" provision "dc.ws" { } }
}"#,
            vec![
                FakeMachine::new("member", MachineKind::Container, &sh),
                FakeMachine::new("dc", MachineKind::Container, &sh),
            ],
        );
        let (sink, said) = numbered(&sh);
        lab.up(&[], sink).await.expect("up");
        assert!(
            said_at(&said, "dc: dc-end") < said_at(&said, "member: join-begin"),
            "member's step ran before dc's had finished: {:?}",
            said.lock().unwrap()
        );
        assert!(
            said_at(&said, "member: waiting for the provisioning of dc")
                < said_at(&said, "dc: dc-end"),
            "the wait was not announced: {:?}",
            said.lock().unwrap()
        );
    }

    /// `depends_on` is start order only (§7.2): the dependent starts once its
    /// dependency is ready, while the dependency is still provisioning.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn depends_on_does_not_wait_for_provisioning() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        timed_script(dir.path(), "dc", 500);
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  container "dc" { image = "x:1" provision "dc.ws" { } }
  container "app" { image = "x:1" depends_on = ["dc"] }
}"#,
            vec![
                FakeMachine::new("dc", MachineKind::Container, &sh),
                FakeMachine::new("app", MachineKind::Container, &sh),
            ],
        );
        let (sink, said) = numbered(&sh);
        lab.up(&[], sink).await.expect("up");
        let started = sh
            .log
            .lock()
            .await
            .iter()
            .find(|l| l.ends_with(":app:start"))
            .and_then(|l| l.split(':').next()?.parse::<usize>().ok())
            .expect("app never started");
        assert!(
            started < said_at(&said, "dc: dc-end"),
            "app waited for dc's provisioning: {:?}",
            said.lock().unwrap()
        );
    }

    /// The first failed step fails `up` with its own error, cancels the steps
    /// still running — a script stops at its next host call — and never
    /// starts the ones waiting on the failed machine; `up` returns once all
    /// of them have stopped (§7.2).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_first_failed_step_cancels_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        std::fs::write(
            dir.path().join("bad.ws"),
            "use vmlab\n\nfn main(lab: Lab) {\n    vmlab::sleep_ms(200)\n    \
             let _m = lab.machine(\"nope\").expect(\"bad-step-failed\")\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("slow.ws"),
            "use vmlab\n\nfn main(lab: Lab) {\n    lab.log(\"slow-begin\")\n    \
             vmlab::sleep_ms(60000)\n    lab.log(\"slow-end\")\n}\n",
        )
        .unwrap();
        timed_script(dir.path(), "after", 0);
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  container "bad" { image = "x:1" provision "bad.ws" { } }
  container "slow" { image = "x:1" provision "slow.ws" { } }
  container "after" { image = "x:1" depends_on_provision = ["bad"] provision "after.ws" { } }
}"#,
            vec![
                FakeMachine::new("bad", MachineKind::Container, &sh),
                FakeMachine::new("slow", MachineKind::Container, &sh),
                FakeMachine::new("after", MachineKind::Container, &sh),
            ],
        );
        let (sink, said) = numbered(&sh);
        let began = std::time::Instant::now();
        let err = tokio::time::timeout(Duration::from_secs(20), lab.up(&[], sink))
            .await
            .expect("up returned once the steps stopped")
            .expect_err("a failed step fails up");
        assert!(began.elapsed() < Duration::from_secs(20));
        let err = format!("{err:#}");
        assert!(err.contains("bad: provision bad.ws"), "{err}");
        assert!(err.contains("bad-step-failed"), "{err}");
        let said = said.lock().unwrap();
        let has = |needle: &str| said.iter().any(|(_, s)| s.contains(needle));
        assert!(has("slow: slow-begin") && !has("slow-end"), "{said:?}");
        assert!(has("slow: provisioning cancelled"), "{said:?}");
        assert!(
            !has("after-begin"),
            "a step waiting on the failure ran: {said:?}"
        );
    }

    /// `down` runs the waves leaves-first: a dependency outlives the machines
    /// that need it to shut down cleanly.
    #[tokio::test]
    async fn down_stops_dependents_before_their_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let db = FakeMachine::new("db", MachineKind::Vm, &sh);
        let web = FakeMachine::new("web", MachineKind::Container, &sh);
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  container "db" { image = "db:1" }
  container "web" { image = "web:1" depends_on = ["db"] }
}"#,
            vec![db.clone(), web.clone()],
        );
        lab.down(&[], false).await.expect("down");

        let log = sh.log.lock().await.clone();
        let idx = |needle: &str| log.iter().position(|l| l.ends_with(needle)).unwrap();
        assert!(
            idx("web:stop") < idx("db:stop"),
            "the dependency was stopped first: {log:?}"
        );
    }

    /// Teardown iterates machines once: both kinds are stopped, and neither
    /// needs the runtime to know which it is holding.
    #[tokio::test]
    async fn destroy_stops_and_clears_every_kind() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let vm = FakeMachine::new("dc01", MachineKind::Vm, &sh);
        let ct = FakeMachine::new("web", MachineKind::Container, &sh);
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  container "dc01" { image = "a:1" }
  container "web" { image = "b:1" }
}"#,
            vec![vm.clone(), ct.clone()],
        );
        lab.destroy_machine("dc01").await.expect("destroy dc01");
        lab.destroy_machine("web").await.expect("destroy web");
        assert_eq!(vm.stops.load(Ordering::SeqCst), 1);
        assert_eq!(ct.stops.load(Ordering::SeqCst), 1);
    }

    /// The budgets the four "wait until ready" implementations used to
    /// disagree about, pinned to the values they converged on. A VM waits 600s
    /// because it may be running a Windows first-boot through a settle reboot;
    /// a container waits 300s because its entrypoint starts fast and its
    /// healthcheck governs the rest.
    #[test]
    fn readiness_budgets_are_per_machine_and_pinned() {
        assert_eq!(
            crate::labd::machine::DEFAULT_READY_TIMEOUT,
            Duration::from_secs(300),
            "a container (and any machine that does not override) waits 300s"
        );
        assert_eq!(
            crate::labd::vm::VM_READY_TIMEOUT,
            Duration::from_secs(600),
            "a VM waits 600s"
        );
    }

    /// A relative `disk { from }` names a folder under the lab root, not the
    /// daemon's working directory; an absolute one is left as written.
    #[test]
    fn disk_from_folders_resolve_against_the_lab_root() {
        let root = std::path::Path::new("/labs/demo");
        let lab = crate::config::load_lab_source(
            r#"import <vmlab.wcl>
            lab "l" {
                vm "v" {
                    template = "scratch"
                    arch = "x86_64"
                    disk = 1GiB
                    disk "rel" { from = "./payload/" }
                    disk "abs" { from = "/srv/payload" }
                    disk "blank" { size = 1GiB }
                }
            }"#,
            "<test>",
            root,
        )
        .expect("parse")
        .lab;
        let cfg = crate::labd::machine_build::rooted_vm_config(&lab.vms[0], root);
        let from: Vec<_> = cfg.extra_disks.iter().map(|d| d.from.clone()).collect();
        assert_eq!(
            from,
            [
                Some(PathBuf::from("/labs/demo/payload/")),
                Some(PathBuf::from("/srv/payload")),
                None,
            ]
        );
    }

    /// Capabilities are probed, and a machine that reports none still answers
    /// the probe — nothing infers them from the kind.
    #[tokio::test]
    async fn capabilities_are_probed_not_inferred() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let m: Arc<dyn Machine> = FakeMachine::new("plain", MachineKind::Vm, &sh);
        let caps: Capabilities = m.capabilities().await;
        assert_eq!(caps.kind, MachineKind::Vm, "kind is reported…");
        assert!(!caps.display, "…but a VM double reports no display");
        assert!(!caps.console_log);
        assert!(!caps.reboot);
        assert!(!caps.healthcheck);
        assert!(caps.agent.is_empty());
        let status: MachineStatus = m.status().await;
        assert_eq!(status.name, "plain");
    }

    /// Every line `up` said, with its place on the machines' clock.
    type Said = Arc<std::sync::Mutex<Vec<(usize, String)>>>;

    /// What `up` says, numbered on the same clock as the machines' log.
    fn numbered(sh: &Shared) -> (crate::scripting::OutputSink, Said) {
        let said = Arc::new(std::sync::Mutex::new(Vec::<(usize, String)>::new()));
        let sink: crate::scripting::OutputSink = {
            let said = said.clone();
            let seq = sh.seq.clone();
            Arc::new(move |s: String| {
                let n = seq.fetch_add(1, Ordering::SeqCst);
                said.lock().unwrap().push((n, s));
            })
        };
        (sink, said)
    }

    /// A first-boot script outlasting the mount task's wait for readiness no
    /// longer costs the machine its shares (#153): the mounts start once the
    /// script is done, so a slow first boot mounts them like any other.
    #[tokio::test]
    async fn a_first_boot_longer_than_the_ready_wait_still_mounts_its_shares() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  vm "win" { template = "x86_64/t" }
}"#,
            vec![FakeMachine::sharing(
                "win",
                &sh,
                Duration::from_millis(100),
                Duration::from_millis(500),
                Some("use vmlab\n\nfn main(lab: Lab) {\n    vmlab::sleep_ms(1500)\n}\n"),
            )],
        );
        let (sink, said) = numbered(&sh);
        tokio::time::timeout(Duration::from_secs(30), lab.up(&[], sink))
            .await
            .expect("up finishes")
            .expect("up");
        assert!(
            sh.log
                .lock()
                .await
                .iter()
                .any(|l| l.ends_with(":win:mounted")),
            "the share never mounted"
        );
        let said = said.lock().unwrap();
        assert!(
            !said.iter().any(|(_, s)| s.contains("will not mount")),
            "{said:?}"
        );
    }

    /// A machine's provisions start once its shares have mounted, not while
    /// the mount is still running (#153): a step may read the share.
    #[tokio::test]
    async fn a_machines_provisions_wait_for_its_shares_to_mount() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        std::fs::write(
            dir.path().join("p.ws"),
            "use vmlab\n\nfn main(lab: Lab) {\n}\n",
        )
        .unwrap();
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  vm "win" {
    template = "x86_64/t"
    provision "p.ws" { }
  }
}"#,
            vec![FakeMachine::sharing(
                "win",
                &sh,
                Duration::from_millis(1500),
                Duration::from_secs(10),
                None,
            )],
        );
        let (sink, said) = numbered(&sh);
        tokio::time::timeout(Duration::from_secs(30), lab.up(&[], sink))
            .await
            .expect("up finishes")
            .expect("up");
        let mounted = sh
            .log
            .lock()
            .await
            .iter()
            .find(|l| l.ends_with(":win:mounted"))
            .and_then(|l| l.split(':').next()?.parse::<usize>().ok())
            .expect("the share never mounted");
        let said = said.lock().unwrap();
        let provision = said
            .iter()
            .find(|(_, s)| s.starts_with("win: provision p.ws"))
            .map(|(n, _)| *n)
            .expect("the provision never ran");
        assert!(
            mounted < provision,
            "the provision started before the share mounted: {said:?}"
        );
        assert!(
            said.iter()
                .any(|(_, s)| s.contains("waiting for its shares to mount")),
            "{said:?}"
        );
    }

    /// An online restore sets the guest clock unless the machine declares it
    /// should not (#157); one it should set but cannot is a warning the
    /// restore answers with, never a failed restore.
    #[tokio::test]
    async fn an_online_restore_sets_the_clock_or_says_why_not() {
        use crate::config::model::Clock;
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let fixed = Clock::Fixed(chrono::DateTime::UNIX_EPOCH);
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  vm "host" { template = "x86_64/t" }
  vm "kept" { template = "x86_64/t" }
  vm "pinned" { template = "x86_64/t" }
}"#,
            vec![
                FakeMachine::clocked("host", &sh, Clock::Host),
                FakeMachine::clocked("kept", &sh, Clock::Snapshot),
                FakeMachine::clocked("pinned", &sh, fixed),
            ],
        );
        for name in ["host", "kept", "pinned"] {
            assert!(lab.snapshot(name, "s1").await.unwrap(), "{name}: online");
        }
        // The double has no agent, so the clock it should set it cannot.
        let warnings =
            tokio::time::timeout(Duration::from_secs(30), lab.restore("host", "s1", false))
                .await
                .expect("restore returns")
                .expect("restore succeeds without the clock");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("\"host\": the guest clock was not set"),
            "{warnings:?}"
        );
        for name in ["kept", "pinned"] {
            let warnings = lab.restore(name, "s1", false).await.unwrap();
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
        }
    }

    /// A snapshot records the hypervisor that took it, and an online one
    /// refuses to restore under another, naming the one that took it; an
    /// offline one restores under either (§22, D31).
    #[tokio::test]
    async fn an_online_snapshot_restores_only_under_the_hypervisor_that_took_it() {
        use crate::config::hypervisor::HypervisorKind;
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  vm "warm" { template = "x86_64/t" clock = "snapshot" }
  vm "cold" { template = "x86_64/t" }
}"#,
            vec![
                FakeMachine::clocked("warm", &sh, crate::config::model::Clock::Snapshot),
                FakeMachine::new("cold", MachineKind::Vm, &sh),
            ],
        );
        assert!(lab.snapshot("warm", "s1").await.unwrap());
        assert!(!lab.snapshot("cold", "s1").await.unwrap());
        // Recorded as the machine's own hypervisor at capture.
        assert_eq!(
            lab.state.lock().await.machines["warm"].snapshots["s1"].hypervisor,
            HypervisorKind::Qemu
        );
        // As if both had been taken while the machines ran on vmlab-hv.
        {
            let mut state = lab.state.lock().await;
            for m in ["warm", "cold"] {
                state
                    .machine_mut(m)
                    .snapshots
                    .get_mut("s1")
                    .unwrap()
                    .hypervisor = HypervisorKind::Vmlab;
            }
        }
        let err = lab.restore("warm", "s1", false).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("taken online under vmlab"), "{msg}");
        assert!(msg.contains("now runs on qemu"), "{msg}");
        assert!(
            err.downcast_ref::<crate::proto::CommandError>().is_some(),
            "a conflict, not an internal error: {msg}"
        );
        lab.restore("cold", "s1", false)
            .await
            .expect("offline restores across");
    }

    /// `up` reports a share that gave up, and one still retrying when its
    /// wait ran out, without waiting for the retrying one to finish.
    #[tokio::test(start_paused = true)]
    async fn up_reports_unmountable_and_still_retrying_shares_within_its_hold() {
        let gave_up = Arc::new(MountReport::default());
        gave_up.planned(["data"], &[], 30);
        gave_up.ready();
        gave_up.gave_up(Some("data"), "`mount` refused: exited 64: not empty");
        let retrying = Arc::new(MountReport::default());
        retrying.planned(["src"], &[], 30);
        retrying.ready();
        retrying.attempt_failed(Some("src"), 0, "exited 1: no such device");
        let mounts = vec![
            ShareMounts {
                machine: "win".into(),
                report: gave_up,
                task: tokio::spawn(async {}),
            },
            ShareMounts {
                machine: "lin".into(),
                report: retrying,
                task: tokio::spawn(std::future::pending()),
            },
        ];
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: crate::scripting::OutputSink = {
            let lines = Arc::clone(&lines);
            Arc::new(move |l| lines.lock().unwrap().push(l))
        };
        let started = tokio::time::Instant::now();
        report_share_mounts(mounts, MOUNT_REPORT_HOLD, &sink).await;
        assert_eq!(
            started.elapsed(),
            MOUNT_REPORT_HOLD,
            "the hold bounds the wait"
        );
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("\"win\": share \"data\" will not mount: `mount` refused"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("\"lin\": share \"src\" not mounted yet, still retrying"),
            "{lines:?}"
        );
    }

    /// When every mount finishes, `up` does not wait out the hold.
    #[tokio::test(start_paused = true)]
    async fn up_does_not_wait_out_the_hold_once_mounts_finish() {
        let report = Arc::new(MountReport::default());
        report.planned(["src"], &[], 30);
        let task = {
            let report = Arc::clone(&report);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                report.ready();
                report.mounted("src");
            })
        };
        let started = tokio::time::Instant::now();
        let mounts = vec![ShareMounts {
            machine: "lin".into(),
            report,
            task,
        }];
        let said = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sink: crate::scripting::OutputSink = {
            let said = Arc::clone(&said);
            Arc::new(move |_| said.store(true, std::sync::atomic::Ordering::Relaxed))
        };
        report_share_mounts(mounts, MOUNT_REPORT_HOLD, &sink).await;
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert!(
            !said.load(std::sync::atomic::Ordering::Relaxed),
            "all mounted says nothing"
        );
    }

    // ---- host USB passthrough (§5.2.1) ---------------------------------------

    /// A lab of two USB-capable VM doubles `a` and `b` and a container `web`,
    /// reading the fake sysfs of `usb::tests::fake_sysfs`, with device nodes
    /// for both flash drives this process can open and a receiver node it
    /// cannot.
    fn usb_lab(dir: &std::path::Path) -> (Arc<LabRuntime>, tempfile::TempDir, Arc<FakeMachine>) {
        use std::os::unix::fs::PermissionsExt;
        let sh = shared(dir);
        let sys = crate::usb::tests::fake_sysfs();
        let dev = dir.join("dev");
        std::fs::create_dir_all(dev.join("001")).unwrap();
        for (n, mode) in [("006", 0o666), ("007", 0o666), ("015", 0o444)] {
            let node = dev.join("001").join(n);
            std::fs::write(&node, b"").unwrap();
            std::fs::set_permissions(&node, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let a = FakeMachine::usb_vm("a", &sh);
        let b = FakeMachine::usb_vm("b", &sh);
        let web = FakeMachine::new("web", MachineKind::Container, &sh);
        let config = crate::config::load_lab_source(
            "import <vmlab.wcl>\nlab \"t\" {\n  vm \"a\" { template = \"x86_64/t\" usb = true }\n  \
             vm \"b\" { template = \"x86_64/t\" usb = true }\n  container \"web\" { image = \"web:1\" }\n}\n",
            "<test>",
            dir,
        )
        .expect("lab source");
        let lab = LabRuntime::with_machines_on(
            config,
            vec![a.clone(), b, web],
            super::super::usb::UsbHost::fake(sys.path().to_path_buf(), dev),
        )
        .expect("runtime");
        (lab, sys, a)
    }

    #[tokio::test]
    async fn usb_attach_refusals_and_detach() {
        let dir = tempfile::tempdir().unwrap();
        let (lab, _sys, _a) = usb_lab(dir.path());
        let mut events = lab.events.subscribe();

        let got = lab.usb_attach("a", "0781:5581@BB22", false).await.unwrap();
        assert_eq!(got.device.port, "1-9");
        assert_eq!(got.qdev_id, "usbhost-1-9");
        let ev = events.recv().await.unwrap();
        assert_eq!(ev.event, "usb.attached");
        assert_eq!(ev.data["vm"], "a");
        assert_eq!(ev.data["port"], "1-9");

        let err = |r: Result<crate::usb::Attachment>| format!("{:#}", r.unwrap_err());
        // Held by another machine of this lab, named as lab/vm.
        let e = err(lab.usb_attach("b", "port:1-9", false).await);
        assert!(e.contains("already attached to t/a"), "{e}");
        // Ambiguous: both drives match.
        let e = err(lab.usb_attach("b", "0781:5581", false).await);
        assert!(e.contains("matches 2 USB devices"), "{e}");
        // A HID receiver wants --force, then a node this process can open.
        let e = err(lab.usb_attach("b", "046d:c547", false).await);
        assert!(e.contains("is a HID keyboard"), "{e}");
        if !nix::unistd::getuid().is_root() {
            let e = err(lab.usb_attach("b", "046d:c547", true).await);
            assert!(e.contains("cannot open") && e.contains("001/015"), "{e}");
            assert!(e.contains("sudo setfacl -m u:"), "{e}");
        }
        let e = err(lab.usb_attach("web", "port:1-10", false).await);
        assert!(e.contains("USB passthrough is for VMs"), "{e}");
        let e = err(lab.usb_attach("b", "mouse", false).await);
        assert!(e.contains("is not a USB selector"), "{e}");

        lab.usb_attach("b", "port:1-10", false).await.unwrap();
        assert_eq!(lab.usb_attached(None).await.unwrap().len(), 2);
        // The container holds nothing, and says so without refusing: the
        // supervisor asks every machine.
        assert!(lab.usb_attached(Some("web")).await.unwrap().is_empty());

        let gone = lab.usb_detach("a", Some("port:1-9")).await.unwrap();
        assert_eq!(gone.len(), 1);
        let e = format!(
            "{:#}",
            lab.usb_detach("a", Some("port:1-9")).await.unwrap_err()
        );
        assert!(e.contains("attached to \"a\""), "{e}");
        let gone = lab.usb_detach("b", None).await.unwrap();
        assert_eq!(gone[0].device.port, "1-10");
        assert!(lab.usb_attached(None).await.unwrap().is_empty());
    }

    /// A device unplugged from the host while a VM holds it leaves the
    /// tracking, with a `usb.detached` that says why; the rest stay.
    #[tokio::test]
    async fn an_unplugged_device_is_swept_out_of_the_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let (lab, sys, a) = usb_lab(dir.path());
        lab.usb_attach("a", "port:1-9", false).await.unwrap();
        lab.usb_attach("b", "port:1-10", false).await.unwrap();
        let mut events = lab.events.subscribe();

        assert!(lab.usb_sweep().await, "nothing has left");
        std::fs::remove_dir_all(sys.path().join("1-9")).unwrap();
        assert!(lab.usb_sweep().await, "b still holds one");
        assert!(a.usb_attached().await.is_empty());
        let ev = events.recv().await.unwrap();
        assert_eq!(ev.event, "usb.detached");
        assert_eq!(ev.data["reason"], "unplugged");
        assert_eq!(ev.data["port"], "1-9");

        // Replugged into the same socket: a new device number, so it is not
        // the device b was given.
        std::fs::write(sys.path().join("1-10").join("devnum"), "8\n").unwrap();
        assert!(!lab.usb_sweep().await, "nothing attached any more");
        assert!(lab.usb_attached(None).await.unwrap().is_empty());
    }

    /// The wscript surface (§5.2.1) over the same runtime calls.
    #[tokio::test]
    async fn usb_from_wscript() {
        let dir = tempfile::tempdir().unwrap();
        let (lab, _sys, _a) = usb_lab(dir.path());
        let said = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: crate::scripting::OutputSink = {
            let said = said.clone();
            Arc::new(move |s: String| said.lock().unwrap().push(s.trim_end().to_string()))
        };
        let script = r#"use vmlab

fn main(lab: Lab) {
    let Ok(a) = lab.machine("a") else { return }
    match a.usb_attach("046d:c547") {
        Ok(_) => lab.log("hid attached unforced"),
        Err(e) => lab.log("refused: " + e),
    }
    let d = a.usb_attach("0781:5581@AA11").expect("attach")
    lab.log("attached " + d.id + " " + d.vendor_id + ":" + d.product_id)
    for dev in lab.usb_devices().expect("list") {
        match dev.attached_to {
            Some(h) => lab.log("held " + dev.port + " by " + h),
            None => lab.log("free " + dev.port),
        }
    }
    lab.log("a holds " + fmt("{}", a.usb_attached().expect("attached").len()))
    a.usb_detach("port:1-10").expect("detach")
    lab.log("a holds " + fmt("{}", a.usb_attached().expect("attached").len()))
}
"#;
        crate::scripting::run_script_source(
            lab,
            script.into(),
            "usb.ws",
            dir.path().to_path_buf(),
            None,
            None,
            sink,
        )
        .await
        .expect("script");
        let said = said.lock().unwrap().clone();
        let has = |line: &str| said.iter().any(|s| s == line);
        assert!(
            said.iter()
                .any(|s| s.starts_with("refused: ") && s.contains("HID keyboard")),
            "{said:?}"
        );
        assert!(has("attached port:1-10 0781:5581"), "{said:?}");
        assert!(has("held 1-10 by t/a"), "{said:?}");
        assert!(has("free 1-9"), "{said:?}");
        assert!(has("a holds 1"), "{said:?}");
        assert!(has("a holds 0"), "{said:?}");
    }

    // ---- console recordings (§11.1) -------------------------------------

    /// A lab with a displayed VM `a` (its VNC socket a fake RFB server), a
    /// VM `b` the same way, and a container `web` with no display.
    async fn record_lab(
        dir: &std::path::Path,
    ) -> Option<(Arc<LabRuntime>, Shared, crate::vnc::fake::FakeRfb)> {
        let avail = crate::record::encoder::probe(None).await;
        if crate::record::encoder::check(&avail, crate::record::Format::Mp4).is_err() {
            eprintln!("skipping: no ffmpeg with libx264 on this host");
            return None;
        }
        let img = crate::vnc::fake::pattern(16, 8);
        let rfb = crate::vnc::fake::spawn(
            16,
            8,
            vec![crate::vnc::fake::update(&[crate::vnc::fake::raw_rect(
                &img, 0, 0, 16, 8,
            )])],
        );
        let sh = shared(dir);
        let a = FakeMachine::displayed("a", &sh, &rfb.path);
        let b = FakeMachine::displayed("b", &sh, &rfb.path);
        let web = FakeMachine::new("web", MachineKind::Container, &sh);
        let lab = lab_of(
            dir,
            r#"import <vmlab.wcl>
lab "t" {
  vm "a" { template = "x86_64/t" }
  vm "b" { template = "x86_64/t" }
  container "web" { image = "web:1" }
}"#,
            vec![a, b, web],
        );
        Some((lab, sh, rfb))
    }

    fn code_of(e: &anyhow::Error) -> Option<crate::proto::ErrorCode> {
        e.downcast_ref::<crate::proto::CommandError>()
            .map(|c| c.code)
    }

    fn small() -> crate::record::RecordOptions {
        crate::record::RecordOptions {
            fps: Some(10),
            size: Some(crate::record::Size::new(32, 16)),
            ..Default::default()
        }
    }

    /// Start, status and stop; the second start is a `conflict` naming the
    /// live file, a container is `unsupported`, a bad path or figure is
    /// `invalid_argument`, and stopping what is not recording is `not_found`.
    #[tokio::test]
    async fn recordings_start_report_stop_and_refuse_by_code() {
        use crate::proto::ErrorCode;
        let dir = tempfile::tempdir().unwrap();
        let Some((lab, _sh, _rfb)) = record_lab(dir.path()).await else {
            return;
        };
        let out = dir.path().join("a.mp4");
        let started = lab
            .record_start("a", Some(out.to_str().unwrap()), &small(), None)
            .await
            .expect("start");
        assert_eq!(started.path, out);
        assert_eq!(started.options.fps, Some(10));

        let status = lab.record_status(None).await;
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].machine, "a");
        let lab_status = lab.status().await;
        let a = lab_status.machines.iter().find(|m| m.name == "a").unwrap();
        assert_eq!(
            a.recording.as_ref().map(|r| r.path.clone()),
            Some(out.clone())
        );
        assert!(
            lab_status
                .machines
                .iter()
                .find(|m| m.name == "b")
                .unwrap()
                .recording
                .is_none()
        );

        let again = lab
            .record_start("a", Some(out.to_str().unwrap()), &small(), None)
            .await
            .unwrap_err();
        assert_eq!(code_of(&again), Some(ErrorCode::Conflict));
        assert!(format!("{again:#}").contains("a.mp4"), "{again:#}");

        let web = lab
            .record_start("web", None, &small(), None)
            .await
            .unwrap_err();
        assert_eq!(code_of(&web), Some(ErrorCode::Unsupported));
        assert!(format!("{web:#}").contains("no display"), "{web:#}");

        let gif = lab
            .record_start("b", Some("/tmp/x.gif"), &small(), None)
            .await
            .unwrap_err();
        assert_eq!(code_of(&gif), Some(ErrorCode::InvalidArgument));
        let rel = lab
            .record_start("b", Some("rel.mp4"), &small(), None)
            .await
            .unwrap_err();
        assert_eq!(code_of(&rel), Some(ErrorCode::InvalidArgument));
        let odd = lab
            .record_start(
                "b",
                None,
                &crate::record::RecordOptions {
                    size: Some(crate::record::Size::new(33, 16)),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(code_of(&odd), Some(ErrorCode::InvalidArgument));

        tokio::time::sleep(Duration::from_millis(300)).await;
        let stopped = lab.record_stop("a").await.expect("stop");
        assert_eq!(stopped.path, out);
        assert!(out.exists());
        assert!(stopped.duration_secs > 0.0);
        assert!(lab.record_status(None).await.is_empty());
        let none = lab.record_stop("a").await.unwrap_err();
        assert_eq!(code_of(&none), Some(ErrorCode::NotFound));

        // The default path lands under the lab's recordings directory.
        let defaulted = lab
            .record_start("b", None, &small(), None)
            .await
            .expect("start");
        assert!(
            defaulted.path.starts_with(lab.lab_local.join("recordings")),
            "{:?}",
            defaulted.path
        );
        lab.record_stop("b").await.expect("stop");
    }

    /// `vm stop`, `down`, a machine's destroy and the lab's destroy finalise
    /// what the machine was recording; nothing is left live.
    #[tokio::test]
    async fn stopping_a_machine_finalises_its_recording() {
        let dir = tempfile::tempdir().unwrap();
        let Some((lab, _sh, _rfb)) = record_lab(dir.path()).await else {
            return;
        };
        let mut events = lab.events.subscribe();
        for (ender, wait) in [
            ("stop", 0),
            ("down", 1),
            ("destroy_machine", 2),
            ("destroy", 3),
        ] {
            let out = dir.path().join(format!("{ender}.mp4"));
            lab.record_start("a", Some(out.to_str().unwrap()), &small(), None)
                .await
                .expect("start");
            assert_eq!(lab.recordings.live_machines().await, vec!["a".to_string()]);
            match wait {
                0 => lab.stop_machine("a", false).await.expect("stop"),
                1 => lab.down(&[], false).await.expect("down"),
                2 => lab.destroy_machine("a").await.expect("destroy a"),
                _ => lab.destroy().await.expect("destroy"),
            }
            assert!(lab.recordings.live_machines().await.is_empty(), "{ender}");
            assert!(out.exists(), "{ender} left a file");
        }
        let mut seen = Vec::new();
        while let Ok(ev) = events.try_recv() {
            seen.push(ev.event.clone());
        }
        assert_eq!(seen.iter().filter(|k| *k == "record.started").count(), 4);
        assert_eq!(seen.iter().filter(|k| *k == "record.stopped").count(), 4);
    }

    /// Recordings a script run started are finalised when the run ends,
    /// whether the script returned or failed.
    #[tokio::test]
    async fn a_run_finalises_the_recordings_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let Some((lab, _sh, _rfb)) = record_lab(dir.path()).await else {
            return;
        };
        for (script, ok) in [
            ("use vmlab\nfn main(lab: Lab) { lab.log(\"fine\") }\n", true),
            (
                "use vmlab\nfn main(lab: Lab) { lab.machine(\"nope\").expect(\"boom\") }\n",
                false,
            ),
        ] {
            let run = lab.recordings.new_run();
            let out = dir.path().join(format!("run-{ok}.mp4"));
            lab.record_start("a", Some(out.to_str().unwrap()), &small(), Some(run))
                .await
                .expect("start");
            // Another run's recording is not this run's to finalise.
            let other = dir.path().join(format!("other-{ok}.mp4"));
            lab.record_start(
                "b",
                Some(other.to_str().unwrap()),
                &small(),
                Some(lab.recordings.new_run()),
            )
            .await
            .expect("start b");
            let result = crate::scripting::run_script_source(
                lab.clone(),
                script.to_string(),
                "<test>",
                dir.path().to_path_buf(),
                None,
                Some(run),
                quiet(),
            )
            .await;
            assert_eq!(result.is_ok(), ok);
            assert_eq!(lab.recordings.live_machines().await, vec!["b".to_string()]);
            assert!(out.exists());
            lab.record_stop("b").await.expect("stop b");
        }
    }

    /// `m.record` from a script: a relative path lands under the lab root, the
    /// handle's `stop` finalises, and a recording left open is finalised when
    /// the run ends.
    #[tokio::test]
    async fn a_script_records_through_the_machine_handle() {
        let dir = tempfile::tempdir().unwrap();
        let Some((lab, _sh, _rfb)) = record_lab(dir.path()).await else {
            return;
        };
        let said = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink: crate::scripting::OutputSink = {
            let said = said.clone();
            Arc::new(move |s: String| said.lock().unwrap().push(s.trim_end().to_string()))
        };
        let script = r#"use vmlab

fn main(lab: Lab) {
    let Ok(a) = lab.machine("a") else { return }
    let r = a.record("out/a.mp4", #{ "fps": "10", "size": "32x16" }).expect("record")
    lab.log("path " + r.path())
    let stopped = r.stop().expect("stop")
    lab.log("stopped " + stopped)
    match r.stop() {
        Ok(_) => lab.log("stopped twice?"),
        Err(e) => lab.log("again: " + e),
    }
    let Ok(b) = lab.machine("b") else { return }
    let open = b.record("", #{ "fps": "10", "size": "32x16" }).expect("record b")
    lab.log("b " + open.path())
    match a.record("x.gif", #{}) {
        Ok(_) => lab.log("gif?"),
        Err(e) => lab.log("refused: " + e),
    }
}
"#;
        crate::scripting::run_script_source(
            lab.clone(),
            script.to_string(),
            "<test>",
            dir.path().to_path_buf(),
            None,
            None,
            sink,
        )
        .await
        .expect("script");
        let said = said.lock().unwrap().clone();
        let expected = lab.root.join("out/a.mp4").display().to_string();
        assert!(
            said.iter().any(|l| *l == format!("path {expected}")),
            "{said:?}"
        );
        assert!(
            said.iter().any(|l| *l == format!("stopped {expected}")),
            "{said:?}"
        );
        assert!(
            said.iter()
                .any(|l| l.starts_with("again: ") && l.contains("not recording")),
            "{said:?}"
        );
        assert!(
            said.iter()
                .any(|l| l.starts_with("refused: ") && l.contains(".gif")),
            "{said:?}"
        );
        assert!(
            lab.recordings.live_machines().await.is_empty(),
            "the run finalised b's"
        );
        let b_path = said.iter().find(|l| l.starts_with("b ")).unwrap();
        assert!(std::path::Path::new(&b_path[2..]).exists(), "{b_path}");
    }

    /// `--record` on a start verb: checked and armed first, started in the
    /// machine's paused window (the recorder is attached before the CPUs
    /// run), and a second request for a machine already recording refused.
    #[tokio::test]
    async fn an_armed_recording_starts_before_the_machine_resumes() {
        use crate::proto::ErrorCode;
        let dir = tempfile::tempdir().unwrap();
        let Some((lab, sh, _rfb)) = record_lab(dir.path()).await else {
            return;
        };
        let out = dir.path().join("boot.mp4");
        let req = crate::record::RecordRequest {
            machine: "a".into(),
            path: Some(out.to_str().unwrap().into()),
            options: small(),
        };
        lab.record_arm(std::slice::from_ref(&req), None)
            .await
            .expect("arm");
        assert!(lab.recordings.is_pending("a"));
        lab.up(&["a".into()], quiet()).await.expect("up");
        lab.record_settle(&["a".into()]).await.expect("settle");
        let log = sh.log.lock().await.clone();
        assert!(
            log.iter().any(|l| l.ends_with("a:resume:recording=true")),
            "the recorder was attached before the CPUs ran: {log:?}"
        );
        assert_eq!(lab.recordings.live_machines().await, vec!["a".to_string()]);

        let again = lab
            .record_arm(std::slice::from_ref(&req), None)
            .await
            .unwrap_err();
        assert_eq!(code_of(&again), Some(ErrorCode::Conflict));

        // A request still armed after the verb — its machine was already
        // running, so nothing booted to consume it — is settled by recording
        // the machine from now.
        lab.record_stop("a").await.expect("stop");
        lab.record_arm(std::slice::from_ref(&req), None)
            .await
            .expect("arm");
        assert!(lab.recordings.is_pending("a"));
        lab.record_settle(&["a".into()]).await.expect("settle");
        assert_eq!(lab.recordings.live_machines().await, vec!["a".to_string()]);
        lab.record_stop("a").await.expect("stop");

        // An arm that fails for one machine arms none.
        let bad = crate::record::RecordRequest {
            machine: "web".into(),
            path: None,
            options: small(),
        };
        let err = lab.record_arm(&[req.clone(), bad], None).await.unwrap_err();
        assert_eq!(code_of(&err), Some(ErrorCode::Unsupported));
        assert!(!lab.recordings.is_pending("a"));
    }

    /// A worktree lab of a frozen main checkout (§21.2), built for real: the
    /// machine in the freeze is built with the frozen MACs, its record is
    /// written only when the clone is made, and the clone keeps the freeze
    /// point as its one snapshot; a machine new in the branch keeps the MACs
    /// its effective name generates.
    #[tokio::test]
    async fn a_worktree_lab_clones_from_the_freeze_with_the_frozen_macs() {
        let have = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("qemu-img").is_file())
        });
        if !have {
            eprintln!("skipping: qemu-img not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let (main, wt) = (tmp.join("main"), tmp.join("wt"));
        let wt_git = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let src = r#"import <vmlab.wcl>
            lab "l" {
                segment "lan" { subnet = "10.71.0.0/24" }
                vm "dc01" {
                    template = "scratch"
                    arch = "x86_64"
                    disk = 1MiB
                    nic { segment = "lan" }
                }
                vm "web" {
                    template = "scratch"
                    arch = "x86_64"
                    disk = 1MiB
                    nic { segment = "lan" }
                }
            }"#;
        std::fs::write(wt.join("vmlab.wcl"), src).unwrap();

        let main_local = crate::paths::lab_local_dir(&main);
        let frozen = crate::freeze::machine_dir(&main_local, 1, "dc01");
        std::fs::create_dir_all(&frozen).unwrap();
        crate::template::qimg::create_blank(&frozen.join("disk0.qcow2"), 1 << 20)
            .await
            .unwrap();
        std::fs::write(frozen.join("clone-source"), "scratch\n").unwrap();
        let mac = crate::config::model::MacAddr([0x52, 0x54, 0, 0xfe, 0xed, 0x01]);
        let mut record = crate::labd::state::MachineState {
            macs: vec![mac],
            ..Default::default()
        };
        for (name, freeze_point) in [("base", Some(1)), ("early", None)] {
            record.snapshots.insert(
                name.into(),
                SnapshotRecord {
                    online: false,
                    taken_at: chrono::Utc::now(),
                    image_digest: None,
                    freeze_point,
                    frozen_in: freeze_point.xor(Some(1)),
                    hypervisor: Default::default(),
                },
            );
        }
        std::fs::write(
            frozen.join("state.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        crate::freeze::WorktreeConfig {
            generation: 1,
            snapshot: "base".into(),
        }
        .save(&main_local)
        .unwrap();

        let config = crate::config::load_lab_source(src, "<test>", &wt).expect("lab source");
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let events = Arc::new(super::super::events::EventLog::new("l-wt", tx).unwrap());
        let lab = LabRuntime::build(config, events, &ProfileSet::shipped().unwrap())
            .await
            .unwrap();
        assert_eq!(lab.vm("dc01").unwrap().macs, vec![mac]);
        assert_eq!(
            lab.vm("web").unwrap().macs,
            vec![crate::labd::state::generate_mac("l", "web", 0)]
        );
        assert!(
            !lab.state.lock().await.machines.contains_key("dc01"),
            "nothing is written for the clone before it is made"
        );
        assert!(!lab.status().await.freeze_stale);

        lab.clone_from_freeze("dc01").await.unwrap();
        lab.clone_from_freeze("web").await.unwrap();
        let local = &lab.vm("dc01").unwrap().dirs.local;
        let info = crate::template::qimg::image_info(&local.join("disk0.qcow2"))
            .await
            .unwrap();
        assert_eq!(info.backing_file, Some(frozen.join("disk0.qcow2")));
        assert_eq!(
            std::fs::read_to_string(local.join("freeze-generation")).unwrap(),
            "1\n"
        );
        assert!(!lab.vm("web").unwrap().dirs.primary_disk().exists());
        let state = LabState::load(&lab.lab_local).unwrap();
        assert_eq!(state.machines["dc01"].macs, vec![mac]);
        assert_eq!(
            state.machines["dc01"].snapshots.keys().collect::<Vec<_>>(),
            ["base"]
        );
        // The disk the boot keeps is the clone, and the daemon's plan holds.
        lab.vm("dc01").unwrap().ensure_disks().await.unwrap();
        assert_eq!(
            crate::template::qimg::image_info(&local.join("disk0.qcow2"))
                .await
                .unwrap()
                .backing_file,
            Some(frozen.join("disk0.qcow2"))
        );
        assert!(!lab.status().await.freeze_stale);

        // A missing generation refuses the machine still to be built.
        std::fs::remove_dir_all(crate::freeze::freeze_root(&main_local)).unwrap();
        let config = crate::config::load_lab_source(src, "<test>", &wt).expect("lab source");
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let events = Arc::new(super::super::events::EventLog::new("l-wt", tx).unwrap());
        let lab = LabRuntime::build(config, events, &ProfileSet::shipped().unwrap())
            .await
            .unwrap();
        let err = lab.clone_from_freeze("web").await.unwrap_err().to_string();
        assert!(err.contains("vmlab worktree freeze"), "{err}");
        lab.clone_from_freeze("dc01").await.unwrap();
    }

    // ---- reload (PRD §7.1) ----------------------------------------------------

    const RELOAD_BASE: &str = r#"import <vmlab.wcl>
lab "t" {
  segment "corp" { }
  container "web" { image = "web:1" nic { segment = "corp" } }
  container "db" { image = "db:1" nic { segment = "corp" } }
}"#;

    /// A lab of two container doubles, `web` and `db`, whose reloads build
    /// fresh doubles — so a machine's instance changing is a rebuild.
    fn reload_lab(dir: &std::path::Path) -> (Arc<LabRuntime>, Shared) {
        let sh = shared(dir);
        let lab = lab_of(
            dir,
            RELOAD_BASE,
            vec![
                FakeMachine::new("web", MachineKind::Container, &sh),
                FakeMachine::new("db", MachineKind::Container, &sh),
            ],
        );
        let doubles = sh.clone();
        *lab.doubles.lock().unwrap() = Some(Arc::new(move |cfg| {
            FakeMachine::new(cfg.name(), MachineKind::Container, &doubles) as Arc<dyn Machine>
        }));
        (lab, sh)
    }

    fn edited(dir: &std::path::Path, src: &str) -> LabFile {
        crate::config::load_lab_source(src, "<test>", dir).expect("lab source")
    }

    fn same(a: &Arc<dyn Machine>, b: &Arc<dyn Machine>) -> bool {
        std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
    }

    /// A stopped machine whose block changed is rebuilt from the new one; a
    /// running one keeps its instance and is pending, named by `status`,
    /// until its next boot rebuilds it — the stop and start of a restart.
    #[tokio::test]
    async fn a_reload_rebuilds_what_is_stopped_and_leaves_what_runs_pending() {
        let dir = tempfile::tempdir().unwrap();
        let (lab, _sh) = reload_lab(dir.path());
        lab.start_machine("web").await.unwrap();
        let (web, db) = (lab.machine("web").unwrap(), lab.machine("db").unwrap());

        let new = RELOAD_BASE
            .replace("\"web:1\"", "\"web:2\"")
            .replace("\"db:1\"", "\"db:2\"");
        let report = lab.reload_with(edited(dir.path(), &new)).await.unwrap();
        assert_eq!(
            report.machines,
            [
                crate::status::MachineReload::Rebuilt {
                    machine: "db".into()
                },
                crate::status::MachineReload::Pending(crate::status::PendingMachine {
                    machine: "web".into(),
                    reason: crate::status::PendingReason::Running,
                }),
            ]
        );
        assert!(same(&lab.machine("web").unwrap(), &web), "web was replaced");
        assert!(
            !same(&lab.machine("db").unwrap(), &db),
            "db was not rebuilt"
        );

        let status = lab.status().await;
        assert_eq!(
            status.config_digest,
            crate::config::digest(&edited(dir.path(), &new))
        );
        assert_eq!(
            status.pending,
            [crate::status::PendingMachine {
                machine: "web".into(),
                reason: crate::status::PendingReason::Running,
            }]
        );

        lab.restart_machine("web", false).await.unwrap();
        let rebuilt = lab.machine("web").unwrap();
        assert!(!same(&rebuilt, &web), "web was not rebuilt by its restart");
        assert_eq!(rebuilt.state().await, PowerState::Running);
        assert_eq!(web.state().await, PowerState::Stopped);
        assert!(lab.status().await.pending.is_empty());
    }

    /// A pending machine that stops by any route — here a guest shutdown,
    /// which no lab verb sees — is named as such, and rebuilt by the next
    /// `up` before it boots.
    #[tokio::test]
    async fn a_pending_machine_that_stopped_is_rebuilt_by_its_next_boot() {
        let dir = tempfile::tempdir().unwrap();
        let (lab, _sh) = reload_lab(dir.path());
        lab.start_machine("web").await.unwrap();
        let web = lab.machine("web").unwrap();
        let new = RELOAD_BASE.replace("\"web:1\"", "\"web:2\"");
        lab.reload_with(edited(dir.path(), &new)).await.unwrap();

        web.stop(false).await.unwrap();
        assert_eq!(
            lab.status().await.pending[0].reason,
            crate::status::PendingReason::Stopped
        );
        lab.up(&["web".to_string()], quiet()).await.unwrap();
        let now = lab.machine("web").unwrap();
        assert!(!same(&now, &web), "booted the old instance");
        assert_eq!(now.state().await, PowerState::Running);
        assert!(lab.status().await.pending.is_empty());
    }

    /// Added machines join; a removed one is dropped while stopped, pending
    /// while running, and still stopped by a full `down`.
    #[tokio::test]
    async fn a_reload_adds_and_drops_machines() {
        let dir = tempfile::tempdir().unwrap();
        let (lab, _sh) = reload_lab(dir.path());
        let new = r#"import <vmlab.wcl>
lab "t" {
  segment "corp" { }
  container "cache" { image = "cache:1" nic { segment = "corp" } }
}"#;
        lab.start_machine("web").await.unwrap();
        let web = lab.machine("web").unwrap();
        let report = lab.reload_with(edited(dir.path(), new)).await.unwrap();
        let lines: Vec<String> = report.machines.iter().map(|m| m.line()).collect();
        assert!(lines[0].starts_with("cache: added"), "{lines:?}");
        assert!(lines[1].starts_with("db: dropped"), "{lines:?}");
        assert!(lines[2].starts_with("web: pending"), "{lines:?}");
        assert!(lab.machine("cache").is_ok());
        assert!(lab.machine("db").is_err());
        assert_eq!(
            lab.status().await.pending[0].reason,
            crate::status::PendingReason::Removed
        );

        lab.down(&[], false).await.unwrap();
        assert_eq!(web.state().await, PowerState::Stopped);
        assert!(lab.status().await.pending.is_empty());
        assert!(lab.machine("web").is_err(), "web outlived its stop");
    }

    /// A network change applies nothing: the declaration, the instances and
    /// the status are the ones from before, and the refusal names the change
    /// and `vmlab down`.
    #[tokio::test]
    async fn a_network_change_is_refused_and_applies_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (lab, _sh) = reload_lab(dir.path());
        lab.start_machine("web").await.unwrap();
        let db = lab.machine("db").unwrap();
        let before = lab.status().await.config_digest;
        let new = RELOAD_BASE
            .replace("segment \"corp\" { }", "segment \"corp\" { mtu = 9000 }")
            .replace("\"db:1\"", "\"db:2\"");
        let err = lab
            .reload_with(edited(dir.path(), &new))
            .await
            .expect_err("refused");
        assert_eq!(err.code, crate::proto::ErrorCode::Conflict);
        assert!(
            err.message.contains("segment \"corp\" changed (mtu)"),
            "{}",
            err.message
        );
        assert!(err.message.contains("vmlab down"), "{}", err.message);
        assert!(same(&lab.machine("db").unwrap(), &db), "db was rebuilt");
        assert_eq!(lab.status().await.config_digest, before);
    }

    /// Handlers added by a reload are the ones the next event runs.
    #[tokio::test]
    async fn a_reload_swaps_the_handlers() {
        let dir = tempfile::tempdir().unwrap();
        let (lab, _sh) = reload_lab(dir.path());
        let new = RELOAD_BASE.replace(
            "  segment \"corp\" { }",
            "  segment \"corp\" { }\n  on \"container.started\" { run = \"hook.ws\" }",
        );
        let report = lab.reload_with(edited(dir.path(), &new)).await.unwrap();
        assert!(report.handlers);
        assert!(report.machines.is_empty());
        let event = crate::proto::Event::new("container.started", "t", json!({"container": "web"}));
        let runs = super::super::matching_handler_runs(&lab, &event);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].0.ends_with("hook.ws"));
    }
}
