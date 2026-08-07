//! Lab runtime: owns the VM instances, network fabric, persisted state, and
//! the lifecycle verbs (PRD §7). Lives inside the lab daemon.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::container::{ContainerDirs, ContainerInstance, resolve_volume_hosts};
use super::events::EventLog;
use super::forward_plan::{self, ForwardPlan, ForwardRule, HostBinding};
use super::machine::Machine;
use super::network::LabNetwork;
use super::network::nic_segment_name;
use super::plan;
use super::pull_ledger::{
    Cancellation, PullBatch, PullEvent, PullJob, PullLedger, PullOutcome, PullProgress,
};
use super::share_plan;
use super::state::{LabState, SnapshotRecord, generate_mac};
use super::vm::{PowerState, VmDirs, VmInstance};
use crate::config::LabFile;
use crate::config::model::TemplateRef;
use crate::profiles::ProfileSet;
use crate::status::{LabStatus, SegmentFrames, SegmentStatus};
use crate::sync::LockRecover;
use crate::template::TemplateStore;

pub struct LabRuntime {
    pub name: String,
    pub root: PathBuf,
    pub lab_local: PathBuf,
    pub config: LabFile,
    pub vms: BTreeMap<String, Arc<VmInstance>>,
    pub containers: BTreeMap<String, Arc<ContainerInstance>>,
    /// Every machine in the lab under one name namespace, in the order
    /// `machines()` reports them. The same `Arc`s as `vms`/`containers`; those
    /// two stay for the handful of places that legitimately want a concrete
    /// type (binding a deferred pull's template or image).
    machines: BTreeMap<String, Arc<dyn Machine>>,
    pub network: Mutex<LabNetwork>,
    pub state: Mutex<LabState>,
    pub events: Arc<EventLog>,
    /// SMB server for the lab's shares (PRD §7.5); `None` until `up` starts
    /// it (only when some VM declares shares).
    pub smb: Mutex<Option<crate::smb::LabSmb>>,
    /// Forward ids installed for each machine — segment `forward {}` blocks
    /// and container `port {}` blocks alike — removed and re-installed when
    /// a restart brings a new lease, so a forward never points at a stale IP.
    machine_forwards: Mutex<std::collections::HashMap<String, Vec<(String, u64)>>>,
    /// Loopback forwards backing proxied web pages, keyed by (machine, page).
    /// Revalidated on each `web.forward` (lease IP compare) so restarts and
    /// re-leases self-heal without hooking start events.
    web_forwards: Mutex<std::collections::HashMap<(String, String), WebForward>>,
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

/// A live loopback forward backing a proxied web page.
struct WebForward {
    segment: String,
    id: u64,
    addr: std::net::SocketAddr,
    guest_ip: std::net::Ipv4Addr,
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
        let store = TemplateStore::new(crate::paths::template_store_dir());
        let mut network = LabNetwork::build(&config.lab)?;
        let mut pending: BTreeMap<String, PullJob> = BTreeMap::new();
        let home = std::env::var_os("HOME").map(PathBuf::from);

        let mut vms = BTreeMap::new();
        for vm_cfg in &config.lab.vms {
            // Backing template + recorded hardware.
            let (backing, meta, disk_size) = match &vm_cfg.template {
                TemplateRef::Scratch => (None, None, vm_cfg.disk),
                TemplateRef::Store {
                    arch,
                    name: tname,
                    version,
                } => {
                    let resolved = store
                        .resolve(arch, tname, version.as_deref())
                        .with_context(|| format!("vm \"{}\"", vm_cfg.name))?;
                    (Some(resolved.disk_path.clone()), Some(resolved.meta), None)
                }
                TemplateRef::Registry { reference } => {
                    // A registry reference is pulled on first `up` if absent
                    // from the store, never re-pulled implicitly (PRD §6.4).
                    // Build NEVER downloads: an uncached template becomes a
                    // pending pull (placeholder hardware resolution below) so
                    // the daemon starts instantly; `ensure_pulled` binds the
                    // real parts at up/start/`pull` time, with progress.
                    let arch = vm_cfg.arch.clone().ok_or_else(|| {
                        anyhow!(
                            "vm \"{}\": registry template needs an explicit arch",
                            vm_cfg.name
                        )
                    })?;
                    match crate::oci::cached_registry_template(reference, &arch, &store)? {
                        Some(resolved) => {
                            (Some(resolved.disk_path.clone()), Some(resolved.meta), None)
                        }
                        None => {
                            pending.insert(
                                vm_cfg.name.clone(),
                                PullJob::Template {
                                    reference: reference.clone(),
                                    arch,
                                },
                            );
                            (None, None, None)
                        }
                    }
                }
            };

            let resolved = crate::qemu::resolve_vm(vm_cfg, meta.as_ref(), profiles)?;

            // Stable MACs: explicit > persisted > generated (PRD §9.4).
            let vm_state = state.machine_mut(&vm_cfg.name);
            let mut macs = Vec::new();
            for (i, nic) in vm_cfg.nics.iter().enumerate() {
                let mac = nic
                    .mac
                    .or_else(|| vm_state.macs.get(i).copied())
                    .unwrap_or_else(|| generate_mac(&name, &vm_cfg.name, i));
                macs.push(mac);
            }
            vm_state.macs = macs.clone();

            let dirs = VmDirs::new(&name, &vm_cfg.name, &lab_local);
            let mut cdroms = Vec::new();
            if let Some(c) = &vm_cfg.cdrom {
                cdroms.push(root.join(c));
            }
            let mut floppy = vm_cfg.floppy.as_ref().map(|f| root.join(f));

            // media {} blocks: ISO/floppy images built from folders,
            // content-addressed in .vmlab/media (PRD §6.3).
            let media_dir = lab_local.join("media");
            for m in &vm_cfg.media {
                let src = root.join(&m.from);
                // xorriso/mtools under a blocking task: building an ISO from a
                // folder takes seconds, and this runs on the daemon's runtime.
                let (kind, label, dir) = (m.kind, m.label.clone(), media_dir.clone());
                let built = tokio::task::spawn_blocking(move || {
                    crate::media::MediaCache::new(dir).ensure(kind, &src, label.as_deref())
                })
                .await
                .map_err(|e| anyhow!("media build task: {e}"))?
                .with_context(|| format!("building media for vm \"{}\"", vm_cfg.name))?;
                match m.kind {
                    crate::config::model::MediaKind::Iso => cdroms.push(built),
                    crate::config::model::MediaKind::Floppy => {
                        if floppy.is_some() {
                            bail!(
                                "vm \"{}\": both a floppy attachment and floppy media declared — \
                                 a VM has one floppy drive",
                                vm_cfg.name
                            );
                        }
                        floppy = Some(built);
                    }
                }
            }

            let first_boot = meta.as_ref().and_then(|m| m.first_boot());
            let agent_version = meta.as_ref().and_then(|m| m.agent_version.clone());
            // Each NIC inherits its segment's effective MTU (jumbo on NAT/global
            // by default); drives `host_mtu=` on virtio NICs in the cmdline.
            let nic_mtus: Vec<u16> = vm_cfg
                .nics
                .iter()
                .map(|nic| {
                    network
                        .segments
                        .get(nic_segment_name(nic))
                        .map_or(crate::labd::network::STANDARD_MTU, |s| s.effective_mtu())
                })
                .collect();
            let share_hosts = vm_cfg
                .shares
                .iter()
                .map(|s| share_plan::resolve_share_host(&root, home.as_deref(), &s.host))
                .collect();
            let vm = VmInstance::new(
                &name,
                vm_cfg.clone(),
                dirs,
                macs,
                nic_mtus,
                cdroms,
                floppy,
                share_hosts,
                crate::labd::vm::TemplateParts {
                    resolved,
                    backing,
                    disk_size,
                    first_boot,
                    agent_version,
                },
            );
            vms.insert(vm_cfg.name.clone(), vm);
        }

        // Containers: bind each image offline when it is already cached (the
        // digest pin makes previously-pulled images hit); an uncached image
        // becomes a pending pull, mirroring registry templates — build never
        // downloads, `ensure_pulled` does (with progress) and pins the digest
        // so `up` never re-pulls implicitly (PRD §6.4 semantics).
        let mut containers = BTreeMap::new();
        if !config.lab.containers.is_empty() {
            // Micro-VM containers run the host architecture (v1).
            let arch = std::env::consts::ARCH;
            let cache = crate::oci::image::ImageCache::new(crate::paths::oci_cache_dir());
            for c_cfg in &config.lab.containers {
                let c_state = state.machine_mut(&c_cfg.name);
                if c_state.image_ref.as_deref() != Some(c_cfg.image.reference.as_str()) {
                    // The `image =` line changed — drop the stale pin.
                    c_state.image_digest = None;
                }
                let reference = match &c_state.image_digest {
                    // `name:tag@digest` is valid and the digest wins; a
                    // reference already carrying a digest equals its pin.
                    Some(d) if !c_cfg.image.reference.contains('@') => {
                        format!("{}@{}", c_cfg.image.reference, d)
                    }
                    _ => c_cfg.image.reference.clone(),
                };
                let image = crate::oci::image::cached_container_image(&reference, &cache)
                    .with_context(|| {
                        format!("container \"{}\": image {}", c_cfg.name, c_cfg.image)
                    })?;
                if let Some(image) = &image {
                    c_state.image_digest = Some(image.manifest_digest.clone());
                    c_state.image_ref = Some(c_cfg.image.reference.clone());
                } else {
                    pending.insert(
                        c_cfg.name.clone(),
                        PullJob::Image {
                            reference,
                            arch: arch.to_string(),
                        },
                    );
                }

                // Stable MACs: explicit > persisted > generated — the unified
                // name namespace keeps the hash inputs collision-free.
                let mut macs = Vec::new();
                for (i, nic) in c_cfg.nics.iter().enumerate() {
                    let mac = nic
                        .mac
                        .or_else(|| c_state.macs.get(i).copied())
                        .unwrap_or_else(|| generate_mac(&name, &c_cfg.name, i));
                    macs.push(mac);
                }
                c_state.macs = macs.clone();

                let nic_mtus: Vec<u16> = c_cfg
                    .nics
                    .iter()
                    .map(|nic| {
                        network
                            .segments
                            .get(nic_segment_name(nic))
                            .map_or(crate::labd::network::STANDARD_MTU, |s| s.effective_mtu())
                    })
                    .collect();
                let dirs = ContainerDirs::new(&root, &name, &c_cfg.name);
                let volumes = resolve_volume_hosts(c_cfg, &root);
                // Same resolver as the VMs above: declaration > profile
                // (a container has no template layer). §5.1 validation has
                // already rejected a container no layer sizes.
                let resolved = crate::qemu::resolve_container(c_cfg, arch, profiles)?;
                let container = ContainerInstance::new(
                    &name,
                    c_cfg.clone(),
                    resolved,
                    dirs,
                    macs,
                    nic_mtus,
                    image,
                    volumes,
                );
                containers.insert(c_cfg.name.clone(), container);
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
        let host_cfg = crate::config::host::HostConfig::load_default()?;
        let macs_by_vm: std::collections::HashMap<String, Vec<crate::config::model::MacAddr>> =
            state
                .machines
                .iter()
                .map(|(n, m)| (n.clone(), m.macs.clone()))
                .collect();
        network.wire_gateways(&config.lab, &macs_by_vm, &host_cfg);

        let machines: BTreeMap<String, Arc<dyn Machine>> = vms
            .iter()
            .map(|(n, v)| (n.clone(), v.clone() as Arc<dyn Machine>))
            .chain(
                containers
                    .iter()
                    .map(|(n, c)| (n.clone(), c.clone() as Arc<dyn Machine>)),
            )
            .collect();

        Ok(Arc::new_cyclic(|me| LabRuntime {
            me: me.clone(),
            name,
            root,
            lab_local,
            config,
            vms,
            containers,
            machines,
            network: Mutex::new(network),
            state: Mutex::new(state),
            events,
            smb: Mutex::new(None),
            machine_forwards: Mutex::new(std::collections::HashMap::new()),
            web_forwards: Mutex::new(std::collections::HashMap::new()),
            profiles: profiles.clone(),
            pulls: std::sync::Mutex::new(PullLedger::new(pending)),
            pull_lock: Mutex::new(()),
            pre_provision: std::sync::RwLock::new(None),
            host_cfg,
            playbook_ops: crate::labd::playbook::PlaybookOps::default(),
            workspaces: crate::labd::workspace::WorkspaceSyncers::default(),
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
        let name = config.lab.name.clone();
        let root = config.root.clone();
        let lab_local = crate::paths::lab_local_dir(&root);
        std::fs::create_dir_all(&lab_local)?;
        let network = LabNetwork::build(&config.lab)?;
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let events = Arc::new(super::events::EventLog::new(&name, tx)?);
        let machines: BTreeMap<String, Arc<dyn Machine>> = machines
            .into_iter()
            .map(|m| (m.name().to_string(), m))
            .collect();
        let profiles = ProfileSet::shipped()?;
        Ok(Arc::new_cyclic(|me| LabRuntime {
            me: me.clone(),
            name,
            root,
            lab_local: lab_local.clone(),
            config,
            vms: BTreeMap::new(),
            containers: BTreeMap::new(),
            machines,
            network: Mutex::new(network),
            state: Mutex::new(LabState::load(&lab_local).unwrap_or_default()),
            events,
            smb: Mutex::new(None),
            machine_forwards: Mutex::new(std::collections::HashMap::new()),
            web_forwards: Mutex::new(std::collections::HashMap::new()),
            profiles,
            pulls: std::sync::Mutex::new(PullLedger::new(BTreeMap::new())),
            pull_lock: Mutex::new(()),
            pre_provision: std::sync::RwLock::new(None),
            host_cfg: crate::config::host::HostConfig::default(),
            playbook_ops: crate::labd::playbook::PlaybookOps::default(),
            workspaces: crate::labd::workspace::WorkspaceSyncers::default(),
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
            crate::oci::ensure_registry_template(&ref_s, &arch_s, &store, &mut progress).await
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
    fn bind_template(
        &self,
        batch: &PullBatch,
        resolved: &crate::template::store::ResolvedTemplate,
    ) -> Result<()> {
        for vm_name in &batch.machines {
            let vm_cfg = self
                .config
                .lab
                .vms
                .iter()
                .find(|v| &v.name == vm_name)
                .ok_or_else(|| anyhow!("no vm \"{vm_name}\" in the lab config"))?;
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
            // needs the concrete type that holds one.
            let container = self.container(name)?;
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
        // guest mounting \\<gateway>\<share> reaches the local smbd via NAT.
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
                }
            }
        }

        // Hand each machine whose guest mounts for itself its coordinates.
        // A machine driven from the host instead ignores this.
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
    fn host_virtiofsd(&self) -> bool {
        self.machines()
            .next()
            .map(|m| m.virtiofsd_available())
            .unwrap_or_else(crate::qemu::virtiofsd::available)
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
        let guest_virtiofs: BTreeMap<String, bool> = self
            .vms
            .iter()
            .map(|(name, vm)| (name.clone(), vm.template().resolved.virtiofs))
            .collect();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        share_plan::plan(
            &share_plan::ShareInputs {
                lab: &self.config.lab,
                root: &self.root,
                home: home.as_deref(),
                host_virtiofsd: self.host_virtiofsd(),
                guest_virtiofs: &guest_virtiofs,
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

    pub fn vm(&self, name: &str) -> Result<&Arc<VmInstance>> {
        self.vms.get(name).ok_or_else(|| {
            if self.containers.contains_key(name) {
                anyhow!("\"{name}\" is a container — use `vmlab container ...`")
            } else {
                anyhow!("no vm \"{name}\" in lab \"{}\"", self.name)
            }
        })
    }

    pub fn container(&self, name: &str) -> Result<&Arc<ContainerInstance>> {
        self.containers.get(name).ok_or_else(|| {
            if self.vms.contains_key(name) {
                anyhow!("\"{name}\" is a vm — use `vmlab vm ...`")
            } else {
                anyhow!("no container \"{name}\" in lab \"{}\"", self.name)
            }
        })
    }

    // ---- END kind-aware accessors -------------------------------------------

    /// One machine by name — the only accessor orchestration needs. Machine
    /// names are unique across both kinds, so there is one namespace to look
    /// in.
    pub fn machine(&self, name: &str) -> Result<Arc<dyn Machine>> {
        self.machines
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("no vm or container \"{name}\" in lab \"{}\"", self.name))
    }

    /// Every machine in the lab, in name order.
    pub fn machines(&self) -> impl Iterator<Item = Arc<dyn Machine>> + '_ {
        self.machines.values().cloned()
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
        self.config
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
    /// not see statically (registry templates, containers).
    fn preflight_playbooks(&self, targets: &[String]) -> Result<()> {
        use crate::labd::playbook;
        let lab = &self.config.lab;
        let dir = playbook::default_bin_dir(self.host_cfg.config_weave_bin_dir.as_deref());
        let mut errs: Vec<String> = Vec::new();
        for name in targets {
            let targeted = playbook::playbooks_of(lab, name).is_some_and(|p| !p.is_empty());
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
        self.machine(name)?.start(self.services()).await
    }

    /// Stop one machine and delete everything it materialised, leaving the
    /// rest of the lab running. It stays in the lab config, so a later
    /// `up <name>` re-materialises it (per-machine analogue of [`destroy`]).
    pub async fn destroy_machine(self: &Arc<Self>, name: &str) -> Result<()> {
        let m = self.machine(name)?;
        self.workspaces.stop(name).await;
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
        {
            let mut state = self.state.lock().await;
            state.machine_mut(machine).repaired_agent = Some(agent_version.to_string());
            state.save(&self.lab_local)?;
        }
        self.events.emit(
            "machine.agent_repaired",
            json!({"vm": machine, "machine": machine, "agent_version": agent_version}),
        );
        Ok(())
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

    /// Install one planned forward, returning its id and — for an ephemeral
    /// binding — the loopback address it landed on.
    ///
    /// The single executor behind every forward the lab installs, whether it
    /// came from a segment `forward {}`, a container `port {}` or a `web {}`
    /// page. Priming the NAT engine with the lease MAC happens for all three:
    /// a machine that never originates egress is otherwise unreachable.
    async fn install_forward(
        &self,
        net: &LabNetwork,
        rule: &ForwardRule,
    ) -> Result<(u64, Option<std::net::SocketAddr>), String> {
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
        match rule.host {
            HostBinding::Port(port) => {
                let host_addr = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
                let id =
                    services.add_forward(host_addr, rule.guest_ip, rule.guest_port, rule.proto)?;
                Ok((id, None))
            }
            HostBinding::Ephemeral => {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .map_err(|e| format!("web forward bind failed: {e}"))?;
                let addr = listener
                    .local_addr()
                    .map_err(|e| format!("web forward addr failed: {e}"))?;
                let id = services.add_forward_bound(listener, rule.guest_ip, rule.guest_port)?;
                Ok((id, Some(addr)))
            }
        }
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

    /// Ensure a loopback forward exists for a declared web page and return
    /// its bound host address, the guest IP, port, and the page's auth spec.
    /// The forward is cached per (machine, page) and revalidated against the
    /// current lease so restarts self-heal. Errors (unknown page, no lease,
    /// no NAT) are surfaced to the proxy, which maps them to a 502.
    pub async fn ensure_web_forward(
        self: &Arc<Self>,
        machine: &str,
        page: &str,
    ) -> Result<serde_json::Value> {
        let observed = self.forward_observations(&[machine.to_string()]).await;
        let rule = forward_plan::web_page(
            &forward_plan::ForwardInputs {
                lab: &self.config.lab,
                observed: &observed,
            },
            machine,
            page,
        )
        .map_err(|e| anyhow!(e))?;
        let auth = self.web_page_auth(machine, page);

        let key = (machine.to_string(), page.to_string());
        // Cache hit whose lease still matches → reuse the live forward.
        {
            let cache = self.web_forwards.lock().await;
            if let Some(f) = cache.get(&key)
                && f.guest_ip == rule.guest_ip
            {
                return Ok(json!({
                    "addr": f.addr.to_string(),
                    "guest_ip": rule.guest_ip.to_string(),
                    "port": rule.guest_port,
                    "auth": auth,
                }));
            }
        }

        let net = self.network.lock().await;
        // Drop a stale forward (lease moved / machine restarted).
        if let Some(old) = self.web_forwards.lock().await.remove(&key)
            && let Some(s) = net
                .segments
                .get(&old.segment)
                .and_then(|s| s.services.as_ref())
        {
            s.remove_forward(old.id);
        }
        let (id, addr) = self.install_forward(&net, &rule).await.map_err(|e| {
            anyhow!(
                "web page \"{page}\" needs NAT/egress on segment \"{}\": {e}",
                rule.segment
            )
        })?;
        let addr = addr.expect("an ephemeral binding reports its address");
        self.web_forwards.lock().await.insert(
            key,
            WebForward {
                segment: rule.segment,
                id,
                addr,
                guest_ip: rule.guest_ip,
            },
        );
        Ok(json!({
            "addr": addr.to_string(),
            "guest_ip": rule.guest_ip.to_string(),
            "port": rule.guest_port,
            "auth": auth,
        }))
    }

    /// The credentials the console's proxy injects for a declared page. Not
    /// part of the forward — the rule says where to send bytes, this says
    /// how to log in once they arrive.
    fn web_page_auth(&self, machine: &str, page: &str) -> Option<crate::config::model::WebAuth> {
        self.config
            .lab
            .machine(machine)?
            .web()
            .iter()
            .find(|p| p.name == page)
            .and_then(|p| p.auth.clone())
    }

    /// `vmlab up [vm...]` (PRD §7.2, §10.4): start in depends_on waves and
    /// run provision scripts in declaration order. A dependency is
    /// satisfied when its VM is ready and the provisions scoped to it have
    /// completed.
    pub async fn up(
        self: &Arc<Self>,
        subset: &[String],
        output: crate::scripting::OutputSink,
    ) -> Result<()> {
        // Decide the whole schedule before touching anything: which
        // machines a subset drags in, which wave each lands in, and which
        // configuration steps are in scope (see `labd::plan`).
        let plan =
            plan::plan(&self.config.lab, subset, plan::Direction::Up).map_err(|e| anyhow!(e))?;
        for skip in &plan.skipped {
            output(format!("{}: {}\n", skip.what, skip.why));
        }
        let targets: Vec<String> = plan.machines().cloned().collect();

        // Deferred template/image downloads happen here — before the binary
        // preflight (pulled meta can change the resolved firmware/TPM needs)
        // and before any clone or boot work, streaming progress to both the
        // CLI sink and the event feed.
        self.ensure_pulled(&targets, Some(&output)).await?;

        self.preflight_binaries(&targets)?;

        // Start the SMB server before guests boot so shares are reachable
        // during provisioning (PRD §7.5).
        self.ensure_smb(&output).await;

        let steps = plan.steps.clone();
        let mut next_step = 0usize;
        let mut done: HashSet<String> = HashSet::new();
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
                wave_tasks.spawn(async move {
                    me.start_machine(&n).await?;
                    let m = me.machine(&n)?;
                    // Detached, so provisions can rely on the shares (§7.5)
                    // without the wave blocking on the mount retry window. A
                    // machine whose guest mounts for itself does nothing here.
                    tokio::spawn(Arc::clone(&m).mount_shares(me.services()));
                    // Before this machine can be considered ready (§6.1). A
                    // no-op for machines carrying no first-boot script, so
                    // leaf timing is unchanged.
                    me.run_first_boot(&m, &out).await?;
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
            while let Some(joined) = wave_tasks.join_next().await {
                let started = match joined {
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => {
                        wave_tasks.abort_all();
                        return Err(e);
                    }
                    Err(e) => {
                        wave_tasks.abort_all();
                        return Err(anyhow!("join: {e}"));
                    }
                };
                done.insert(started);
            }

            // Between waves: run (in declaration order) every unrun
            // provision/playbook whose machine has already started, so a VM
            // depending on "dc01" starts only after dc01's configuration
            // steps completed (§7.2).
            self.run_up_steps(&steps, &mut next_step, &done, &output)
                .await?;
        }

        // Final pass: the last wave's steps.
        self.run_up_steps(&steps, &mut next_step, &done, &output)
            .await?;

        self.install_forwards(&targets).await;

        self.warn_unattachable(&targets, &output).await;

        // After provisioning, and only now: the syncer writes as the
        // machine's default login, which the provisioning is what creates
        // (§19.6). The task belongs to this daemon, so the `vmlab` process
        // that asked for the `up` may exit without stopping it.
        self.start_workspaces(&targets, &output).await;

        self.events.emit("lab.up", json!({"vms": targets}));
        Ok(())
    }

    /// §19.4's middle rung: `up` warns about a machine whose agent cannot
    /// serve an attach, and never fails over it.
    ///
    /// Here rather than at `validate` because the handshake is part of
    /// readiness, so by now the features are *probed* rather than inferred
    /// from a sealed version string; and a warning rather than an error
    /// because the SSH facade is a general capability — a machine nothing can
    /// attach to is still a perfectly good machine, and its shell still works.
    ///
    /// Only machines whose agent actually answered are considered. A machine
    /// still booting, or one whose guest profile has no agent channel at all,
    /// has told us nothing about its features, and guessing from silence is
    /// the inference this ladder exists to avoid.
    ///
    /// **Every machine, not only the `@dev` ones.** Scoping it to dev machines
    /// was the quieter reading and is wrong: the SSH facade is a *general*
    /// capability of every machine (§19.3), so "nothing can attach to this"
    /// is news about any of them. It also self-clears — the warning exists for
    /// a machine whose agent answered and is old, which stops the moment its
    /// template is rebuilt.
    async fn warn_unattachable(&self, targets: &[String], output: &crate::scripting::OutputSink) {
        for name in targets {
            let Ok(m) = self.machine(name) else { continue };
            let Ok(agent) = m.agent().await else { continue };
            let Some(warning) = crate::attach::warning(name, &agent.info().features) else {
                continue;
            };
            output(format!("{warning}\n"));
            self.events.emit(
                "machine.not_attachable",
                json!({"vm": name, "machine": name, "reason": warning}),
            );
        }
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
    async fn install_forwards(self: &Arc<Self>, scope: &[String]) {
        let observed = self.forward_observations(scope).await;
        let plan = forward_plan::plan(
            &forward_plan::ForwardInputs {
                lab: &self.config.lab,
                observed: &observed,
            },
            scope,
        );
        self.announce_forward_plan(&plan);

        // Web pages bind on demand (`ensure_web_forward`) — the console needs
        // the ephemeral port back, and nothing wants one until it is opened.
        let mut installable: BTreeMap<&str, Vec<&ForwardRule>> = BTreeMap::new();
        for rule in plan
            .rules
            .iter()
            .filter(|r| r.host != HostBinding::Ephemeral)
        {
            installable
                .entry(rule.machine.as_str())
                .or_default()
                .push(rule);
        }
        if installable.is_empty() {
            return;
        }

        let net = self.network.lock().await;
        let mut tracked = self.machine_forwards.lock().await;
        for (machine, rules) in installable {
            // Drop this machine's forwards from a previous run/lease.
            for (seg, id) in tracked.remove(machine).unwrap_or_default() {
                if let Some(s) = net.segments.get(&seg).and_then(|s| s.services.as_ref()) {
                    s.remove_forward(id);
                }
            }
            let mut installed = Vec::new();
            for rule in rules {
                match self.install_forward(&net, rule).await {
                    Ok((id, _)) => installed.push((rule.segment.clone(), id)),
                    Err(e) => self.events.emit(
                        "forward.skipped",
                        json!({
                            "what": format!("\"{machine}\": {}", rule.source.describe()),
                            "reason": e,
                        }),
                    ),
                }
            }
            tracked.insert(machine.to_string(), installed);
        }
    }

    /// Run configuration steps (provision scripts and playbook applies —
    /// one declaration-ordered queue, see [`plan`]) in strict
    /// order starting at `*next`. A step runs once the machine that declares
    /// it has started, waiting for its readiness first; the queue stops at
    /// the first step whose machine isn't up yet.
    async fn run_up_steps(
        self: &Arc<Self>,
        steps: &[plan::Step],
        next: &mut usize,
        started: &HashSet<String>,
        output: &crate::scripting::OutputSink,
    ) -> Result<()> {
        while *next < steps.len() {
            let step = &steps[*next];
            let m = &step.machine;
            if !started.contains(m) {
                return Ok(());
            }
            let machine = self.machine(m)?;
            // The machine's own budget, never a literal: a VM may still be
            // running a first-boot provision, a container's entrypoint starts
            // fast and its healthcheck governs the rest.
            machine.wait_ready(machine.ready_timeout()).await?;
            match &step.kind {
                plan::StepKind::Provision(p) => {
                    let script = self.root.join(&p.script);
                    output(format!("provision: {} → {m}\n", p.script.display()));
                    // The script reaches its own machine with `lab.this_vm()`.
                    let owner = Some(crate::scripting::ScriptOwner::provision(m));
                    crate::scripting::run_script_file(self.clone(), &script, owner, output.clone())
                        .await
                        .with_context(|| format!("provision {}", p.script.display()))?;
                }
                plan::StepKind::Playbook(p) => {
                    output(format!(
                        "playbook: {} play {} → {m}\n",
                        p.path.display(),
                        p.play
                    ));
                    let outcome = crate::labd::playbook::run_playbook(
                        self,
                        m,
                        p,
                        crate::labd::playbook::PlaybookMode::Apply,
                        output,
                    )
                    .await
                    .with_context(|| {
                        format!("playbook {} play {} on {m}", p.path.display(), p.play)
                    })?;
                    if outcome.exit_code != 0 {
                        bail!(
                            "playbook {} play {} on {m}: config-weave exited {}",
                            p.path.display(),
                            p.play,
                            outcome.exit_code
                        );
                    }
                }
            }
            *next += 1;
        }
        Ok(())
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
            output.clone(),
        );
        tokio::time::timeout(Duration::from_secs(1800), run)
            .await
            .map_err(|_| anyhow!("first-boot {name}: timed out after 1800s"))?
            .with_context(|| format!("first-boot provision for {name}"))?;

        m.first_boot_done().await?;
        self.events.emit(
            &format!("{}.ready", m.event_subject()),
            json!({ m.event_subject(): name }),
        );
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
        let plan =
            plan::plan(&self.config.lab, subset, plan::Direction::Down).map_err(|e| anyhow!(e))?;
        for skip in &plan.skipped {
            tracing::info!("{}: {}", skip.what, skip.why);
        }

        // Before the machines go: a syncer whose guest has gone would spend
        // its retry window failing to reach one.
        for name in plan.machines() {
            self.workspaces.stop(name).await;
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
        self.workspaces.stop_all().await;
        self.down(&[], true).await?;
        // Wait for the exit monitors to settle before removing disks.
        for m in self.machines() {
            wait_settled(&*m).await;
        }
        // Removes clones, container overlays, AND named volumes — destroy is
        // the lab-scoped volume lifecycle boundary (PRD §12).
        remove_tree(&self.lab_local).await?;
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
        crate::dev::machines(&self.config.lab, |cfg| {
            self.machine(cfg.name())
                .ok()
                .and_then(|m| m.profile())
                .and_then(|name| self.profiles.get(&name).cloned())
        })
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
            let Some(workspace) = dev.workspace.clone() else {
                continue;
            };
            if !scope.is_empty() && !scope.contains(&dev.name) {
                continue;
            }
            let Ok(machine) = self.machine(&dev.name) else {
                continue;
            };
            let declared = self.root.join(&workspace);
            let host_root = match declared.canonicalize() {
                Ok(root) if root.is_dir() => root,
                _ => {
                    self.events.emit(
                        "workspace.unavailable",
                        json!({
                            "machine": dev.name,
                            "workspace": declared.display().to_string(),
                            "reason": "the host workspace directory is not there",
                        }),
                    );
                    continue;
                }
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
            // §19.6, resolved from the declaration alone (ADR-0003): the
            // guest family and the default login's elevation are what decide
            // the three Windows actions, and the syncer should never have to
            // ask.
            let preconditions =
                crate::labd::workspace::preconditions(machine.guest_os(), machine.logins());
            // **Up front, on the terminal that asked for the `up`.** The
            // event log alone would only reach someone already watching it,
            // and the whole point is that both degradations otherwise fail at
            // a random path hours in, looking like a vmlab bug. The syncer
            // emits them too, for whoever attaches later.
            for degradation in preconditions.degradations() {
                output(format!("warning: {}: {degradation}\n", dev.name));
            }
            self.workspaces
                .start(
                    crate::labd::workspace::Workspace {
                        machine: dev.name.clone(),
                        ledger_path: crate::labd::workspace::ledger::Ledger::path(
                            &self.lab_local,
                            &dev.name,
                        ),
                        host_root,
                        guest_root: dev.workspace_guest.clone(),
                        max_file_bytes: self.host_cfg.workspace_max_file,
                        preconditions,
                    },
                    Arc::new(WorkspaceSessions { machine }),
                    self.events.clone(),
                )
                .await;
        }
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
        let mut machines = Vec::new();
        for m in self.machines() {
            let mut status = m.status().await;
            // Lab-level, not machine-level: is this machine's template/image
            // download still pending? Drives the "Download" button.
            status.cached = !self.pulls.lock_recover().is_pending(&status.name);
            status.dev = devs
                .iter()
                .find(|d| d.name == status.name)
                .cloned()
                .map(Into::into);
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
                    .map(|m| m.get(&seg.name).copied().unwrap_or(false))
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
        // In-flight downloads, so a page load mid-pull still shows progress
        // (the events only reach clients that were already connected).
        LabStatus {
            lab: self.name.clone(),
            machines,
            segments,
            provisioned: self.provisioned(),
            pulls: self.pulls.lock_recover().snapshot(),
        }
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
    pub async fn snapshot(&self, vm_name: &str, snap: &str) -> Result<bool> {
        let m = self.machine(vm_name)?;
        let online = m.snapshot(snap).await?;
        {
            let mut state = self.state.lock().await;
            state.machine_mut(vm_name).snapshots.insert(
                snap.to_string(),
                SnapshotRecord {
                    online,
                    taken_at: chrono::Utc::now(),
                    image_digest: m.snapshot_pin(),
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
    pub async fn restore(self: &Arc<Self>, name: &str, snap: &str) -> Result<()> {
        let record = {
            let mut state = self.state.lock().await;
            state.machine_mut(name).snapshots.get(snap).cloned()
        }
        .ok_or_else(|| anyhow!("\"{name}\" has no snapshot \"{snap}\""))?;

        let m = self.machine(name)?;
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

        m.restore(self.services(), snap, record.online).await?;
        self.events.emit(
            "snapshot.restored",
            json!({"vm": name, "name": snap, "online": record.online}),
        );
        Ok(())
    }

    pub async fn delete_snapshot(&self, vm_name: &str, snap: &str) -> Result<()> {
        let mut state = self.state.lock().await;
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
                .map(|(name, r)| json!({"name": name, "online": r.online, "taken_at": r.taken_at}))
                .collect::<Vec<_>>()
        ))
    }
}

/// How long teardown waits for a machine's exit monitor to settle before
/// removing what the machine was using.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

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

    async fn machine_ready(&self, machine: &str) {
        self.arc()
            .install_forwards(std::slice::from_ref(&machine.to_string()))
            .await;
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
    }

    impl FakeMachine {
        fn new(name: &str, kind: MachineKind, shared: &Shared) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
                kind,
                ready_timeout: Duration::from_secs(300),
                boot: Duration::from_millis(0),
                settle: Duration::from_millis(0),
                state: tokio::sync::RwLock::new(PowerState::Stopped),
                ready: tokio::sync::RwLock::new(false),
                starts: AtomicU32::new(0),
                stops: AtomicU32::new(0),
                log: shared.log.clone(),
                seq: shared.seq.clone(),
                dir: shared.dir.join(name),
                agent: None,
            })
        }

        /// The same double, with a guest agent answering for it.
        fn with_agent(
            name: &str,
            kind: MachineKind,
            shared: &Shared,
            agent: super::super::vm_agent::AgentHandle,
        ) -> Arc<Self> {
            let mut me = Arc::into_inner(Self::new(name, kind, shared)).expect("sole owner");
            me.agent = Some(agent);
            Arc::new(me)
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
        fn web_pages(&self) -> &[crate::config::model::WebPage] {
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
        fn virtiofsd_available(&self) -> bool {
            false
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
            *self.ready.read().await
        }
        async fn stop(&self, _force: bool) -> Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.note("stop").await;
            *self.state.write().await = PowerState::Stopped;
            *self.ready.write().await = false;
            Ok(())
        }

        async fn start(self: Arc<Self>, _lab: Arc<dyn LabServices>) -> Result<()> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.note("start").await;
            *self.state.write().await = PowerState::Starting;
            tokio::time::sleep(self.boot).await;
            *self.state.write().await = PowerState::Running;
            let me = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(me.settle).await;
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

        async fn agent(&self) -> Result<super::super::vm_agent::AgentHandle> {
            self.agent
                .clone()
                .ok_or_else(|| anyhow!("{}: no agent in a double", self.name))
        }
        async fn agent_answering(&self) -> bool {
            *self.ready.read().await
        }
        async fn snapshot(&self, _name: &str) -> Result<bool> {
            Ok(false)
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

    /// An output sink that keeps what was printed, for the tests that are
    /// about what `up` *said*.
    fn recording() -> (
        crate::scripting::OutputSink,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = lines.clone();
        (
            Arc::new(move |line: String| sink.lock().expect("sink lock").push(line)),
            lines,
        )
    }

    /// A guest agent that answers a handshake advertising `features`, and a
    /// ping, and nothing else — which is every question §19.4 asks of one.
    async fn mock_agent(sock: PathBuf, features: &[&str]) -> super::super::vm_agent::AgentHandle {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use vmlab_agent_proto::{
            AgentMsg, Frame, FrameDecoder, FrameKind, HostMsg, PROTO_VERSION, encode_ctrl,
        };

        let listener = tokio::net::UnixListener::bind(&sock).expect("bind the mock agent");
        let features: Vec<String> = features.iter().map(|f| f.to_string()).collect();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut dec = FrameDecoder::new();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => dec.push(&buf[..n]),
                }
                while let Some(Frame { kind, payload, .. }) = dec.next_frame() {
                    if kind != FrameKind::Ctrl {
                        continue;
                    }
                    let Ok(msg) = serde_json::from_slice::<HostMsg>(&payload) else {
                        continue;
                    };
                    let reply = match msg {
                        HostMsg::Hello { token, .. } => AgentMsg::Hello {
                            proto_version: PROTO_VERSION,
                            agent_version: "mock".into(),
                            os: "linux".into(),
                            features: features.clone(),
                            token,
                        },
                        HostMsg::Ping => AgentMsg::Pong,
                        // A guest on no segment still answers the question,
                        // and answering it is what keeps a caller that asks
                        // (the forward plan does) from waiting out its
                        // timeout.
                        HostMsg::NetInfo => AgentMsg::NetInfo {
                            interfaces: Vec::new(),
                        },
                        _ => continue,
                    };
                    if stream.write_all(&encode_ctrl(&reply)).await.is_err() {
                        return;
                    }
                }
            }
        });
        super::super::vm_agent::AgentHandle::connect(&sock, Duration::from_secs(5))
            .await
            .expect("connect the mock agent")
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

    /// §19.4's middle rung. `up` warns about a machine whose agent cannot
    /// serve an attach — by then the features are probed rather than inferred
    /// — and **brings the lab up anyway**: the facade is a general capability,
    /// so a machine nothing can attach to is still a perfectly good machine.
    ///
    /// The lab holds one of each on purpose: the machine that can serve an
    /// attach must draw no warning at all, or the warning would be noise the
    /// developer learns to skip.
    #[tokio::test]
    async fn up_warns_about_a_machine_that_cannot_serve_an_attach() {
        let dir = tempfile::tempdir().unwrap();
        let sh = shared(dir.path());
        let stale = FakeMachine::with_agent(
            "stale",
            MachineKind::Vm,
            &sh,
            mock_agent(dir.path().join("stale.sock"), &["terminal", "exec"]).await,
        );
        let current = FakeMachine::with_agent(
            "dev01",
            MachineKind::Vm,
            &sh,
            mock_agent(
                dir.path().join("dev01.sock"),
                &["terminal", "exec", "tunnel", "fileops"],
            )
            .await,
        );
        let lab = lab_of(
            dir.path(),
            r#"import <vmlab.wcl>
lab "t" {
  container "stale" { image = "a:1" }
  container "dev01" { image = "b:1" }
}"#,
            vec![stale.clone(), current.clone()],
        );

        let (sink, printed) = recording();
        lab.up(&[], sink)
            .await
            .expect("a stale agent must not fail `up`");

        let said = printed.lock().unwrap().join("");
        let warned: Vec<&str> = said.lines().filter(|l| l.starts_with("warning:")).collect();
        assert_eq!(warned.len(), 1, "one warning, for one machine: {said:?}");
        assert!(warned[0].contains("\"stale\""), "{said:?}");
        assert!(
            warned[0].contains("serves no `tunnel` and `fileops`"),
            "{said:?}"
        );
        assert!(warned[0].contains("repair-agent stale"), "{said:?}");
        assert!(
            !said.contains("dev01"),
            "the attachable machine drew a warning: {said:?}"
        );
        // And it really did come up.
        assert_eq!(stale.starts.load(Ordering::SeqCst), 1);
        assert_eq!(current.starts.load(Ordering::SeqCst), 1);
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
}
