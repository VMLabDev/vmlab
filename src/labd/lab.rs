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
use super::vm::{PowerState, StopReason, VmDirs, VmInstance};
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
    /// Runs per VM after boot but before any provision script — template
    /// builds install the vmlab-agent here, so it lands even when the last
    /// provision generalizes/shuts the guest down (Windows sysprep). Std
    /// lock: set once before `up`, cloned out, never held across await.
    pub pre_provision: std::sync::RwLock<Option<PreProvisionHook>>,
    /// Host config loaded once at build (config-weave binary dir, …).
    pub host_cfg: crate::config::host::HostConfig,
    /// In-flight config-weave runs, one per machine (`up` and on-demand
    /// check/apply claim through the same registry).
    pub playbook_ops: crate::labd::playbook::PlaybookOps,
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
            Arc<VmInstance>,
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

            let first_boot_script = meta.as_ref().and_then(|m| m.first_boot_script.clone());
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
                    first_boot_script,
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

        Ok(Arc::new(LabRuntime {
            name,
            root,
            lab_local,
            config,
            vms,
            containers,
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
    fn note_pull_progress(&self, machine: &str, progress: PullProgress) {
        let events = self.pulls.lock_recover().progress(machine, progress);
        self.emit_pulls(events);
    }

    /// Retire a batch in the ledger and emit whatever it says about it.
    fn finish_pull(&self, batch: &PullBatch, outcome: PullOutcome) {
        let events = self
            .pulls
            .lock_recover()
            .finish(&batch.machines[0], outcome);
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
        let key = batch.machines[0].clone();
        let ref_s = reference.clone();
        let arch_s = arch.clone();
        let task = tokio::spawn(async move {
            let mut progress = |p: crate::oci::PullProgress| {
                me.note_pull_progress(
                    &key,
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
            self.vm(vm_name)?.set_template(super::vm::TemplateParts {
                resolved: resolved_vm,
                backing: Some(resolved.disk_path.clone()),
                disk_size: None,
                first_boot_script: resolved.meta.first_boot_script.clone(),
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
        let key = batch.machines[0].clone();
        let ref_s = reference.clone();
        let arch_s = arch.clone();
        let task = tokio::spawn(async move {
            let mut progress = |p: crate::oci::image::ImagePullProgress| {
                me.note_pull_progress(
                    &key,
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
        let mut labsmb =
            crate::smb::LabSmb::plan(&self.name, &self.lab_local, smb.host_port, &sharing);
        let config = labsmb.build_config();
        let port = match labsmb.spawn(config) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("SMB server failed to start: {e}");
                output(format!(
                    "WARNING: SMB server failed to start — shares will not mount: {e}\n"
                ));
                self.events
                    .emit("smb.failed", json!({"error": e.to_string()}));
                return;
            }
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

        // Hand each volume-declaring container its mount coordinates; the
        // spec sent over ctl carries them (cinit mounts CIFS after net-up).
        for (name, gateway) in smb.volume_gateways {
            let Some(creds) = labsmb.credentials(&name) else {
                continue;
            };
            if let Ok(container) = self.container(&name) {
                container
                    .set_smb(vmlab_cinit_proto::SmbInfo {
                        gateway: gateway.to_string(),
                        username: creds.username.clone(),
                        password: creds.password.clone(),
                    })
                    .await;
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

    /// Mount a VM's shares through the guest agent (PRD §7.5).
    ///
    /// What to run is a [`crate::smb::mount_plan`] — this only drives it, and
    /// only knows how to retry. The commands themselves, and every piece of
    /// guest-OS knowledge behind them, live next to the mount-step type.
    /// XP-era guests without an agent are mounted by provision scripts via
    /// screen automation instead (documented; not attempted here).
    async fn mount_shares(self: &Arc<Self>, vm_name: &str) {
        let cfg = self.config.lab.vms.iter().find(|v| v.name == vm_name);
        let Some(cfg) = cfg else { return };
        if cfg.shares.is_empty() {
            return;
        }
        let Ok(vm) = self.vm(vm_name) else { return };
        let os_hint = crate::smb::guest_os_hint(vm.template().resolved.profile.as_deref());
        let smb_steps = {
            let smb = self.smb.lock().await;
            smb.as_ref()
                .map(|labsmb| labsmb.mount_plan(vm_name, os_hint))
                .unwrap_or_default()
        };
        let plan = crate::smb::mount_plan(os_hint, &vm.virtiofs_mounts().await, smb_steps);
        for note in &plan.unsupported {
            tracing::warn!("{vm_name}: {note}");
        }
        if plan.is_empty() {
            return;
        }
        let Ok(agent) = vm.agent().await else {
            tracing::warn!("{vm_name}: no agent, cannot auto-mount shares");
            return;
        };
        for step in &plan.steps {
            let mut argv = vec![step.command.clone()];
            argv.extend(step.args.iter().cloned());
            let mut last: Option<String> = None;
            for attempt in 0..plan.retry.attempts {
                if attempt > 0 {
                    tokio::time::sleep(plan.retry.delay).await;
                }
                let started = std::time::Instant::now();
                match agent
                    .exec(argv.clone(), vec![], None, None, Duration::from_secs(30))
                    .await
                {
                    Ok(r) if r.exit_code == 0 => {
                        tracing::info!(
                            "{vm_name}: mount step `{}` ok (attempt {attempt}, {:?})",
                            step.command,
                            started.elapsed()
                        );
                        last = None;
                        break;
                    }
                    Ok(r) => {
                        let err = format!(
                            "exited {}: {}",
                            r.exit_code,
                            String::from_utf8_lossy(&r.stderr)
                        );
                        tracing::debug!(
                            "{vm_name}: mount attempt {attempt} ({:?}): {err}",
                            started.elapsed()
                        );
                        last = Some(err);
                    }
                    Err(e) => {
                        tracing::debug!(
                            "{vm_name}: mount attempt {attempt} ({:?}): {e}",
                            started.elapsed()
                        );
                        last = Some(e.to_string());
                    }
                }
            }
            if let Some(err) = last {
                tracing::warn!("{vm_name}: mount step `{}` failed: {err}", step.command);
            }
        }
    }

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

    /// One machine by name, whichever kind it is — the accessor everything
    /// that isn't `start` or `restore` should reach for. Machine names are
    /// unique across both kinds ([`Self::vm`] and [`Self::container`] each
    /// reject the other's names), so there is one namespace to look in.
    pub fn machine(&self, name: &str) -> Result<Arc<dyn Machine>> {
        if let Some(vm) = self.vms.get(name) {
            return Ok(vm.clone() as Arc<dyn Machine>);
        }
        if let Some(c) = self.containers.get(name) {
            return Ok(c.clone() as Arc<dyn Machine>);
        }
        bail!("no vm or container \"{name}\" in lab \"{}\"", self.name)
    }

    /// Every machine in the lab, VMs before containers, each kind in name
    /// order.
    pub fn machines(&self) -> impl Iterator<Item = Arc<dyn Machine>> + '_ {
        self.vms
            .values()
            .map(|v| v.clone() as Arc<dyn Machine>)
            .chain(
                self.containers
                    .values()
                    .map(|c| c.clone() as Arc<dyn Machine>),
            )
    }

    /// Something in the lab waits on this machine's readiness.
    fn has_dependents(&self, name: &str) -> bool {
        self.config
            .lab
            .vms
            .iter()
            .map(|v| &v.depends_on)
            .chain(self.config.lab.containers.iter().map(|c| &c.depends_on))
            .any(|deps| deps.iter().any(|d| d == name))
    }

    /// Verify the external binaries starting `targets` will need are on PATH
    /// (the per-arch QEMU emulator, `qemu-img` for clones, `swtpm` when a VM
    /// wants a TPM), so a missing package surfaces as one clear error before
    /// any clone or boot work begins instead of a spawn failure mid-`up`.
    pub fn preflight_binaries(&self, targets: &[String]) -> Result<()> {
        let mut needed: Vec<String> = vec!["qemu-img".to_string()];
        for name in targets {
            if let Some(c) = self.containers.get(name) {
                let emu = crate::qemu::emulator_binary(&c.resolved.arch);
                if !needed.contains(&emu) {
                    needed.push(emu);
                }
                continue;
            }
            let vm = self.vm(name)?;
            let t = vm.template();
            let emu = crate::qemu::emulator_binary(&t.resolved.arch);
            if !needed.contains(&emu) {
                needed.push(emu);
            }
            if t.resolved.tpm && !needed.iter().any(|b| b == "swtpm") {
                needed.push("swtpm".to_string());
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
            let (os, arch) = if let Some(c) = self.containers.get(name) {
                (playbook::GuestOs::Linux, c.resolved.arch.clone())
            } else {
                let vm = self.vm(name)?;
                let t = vm.template();
                (
                    playbook::guest_os_of(t.resolved.profile.as_deref()),
                    t.resolved.arch.clone(),
                )
            };
            if let Err(e) = playbook::weave_binary(&dir, os, &arch) {
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

    /// Start one VM: wire its NIC sockets into the segment switches, then
    /// boot it with event-emitting callbacks.
    /// Boot one machine, whichever kind it is.
    ///
    /// The last kind-branch in the lab runtime, and deliberately so: a VM
    /// needs its template clone and segment attachments, a container needs its
    /// image spec and restart policy. That difference belongs below a
    /// Hypervisor seam, not in [`Machine`].
    pub async fn start_machine(self: &Arc<Self>, name: &str) -> Result<()> {
        if self.containers.contains_key(name) {
            self.start_container(name).await
        } else {
            self.start_vm(name).await
        }
    }

    /// Stop one machine and delete everything it materialised.
    pub async fn destroy_machine(self: &Arc<Self>, name: &str) -> Result<()> {
        if self.containers.contains_key(name) {
            self.destroy_container(name).await
        } else {
            self.destroy_vm(name).await
        }
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

    pub async fn start_vm(self: &Arc<Self>, name: &str) -> Result<()> {
        let vm = self.vm(name)?.clone();
        if vm.state().await != PowerState::Stopped {
            return Ok(());
        }
        // Safety net for paths that don't pull explicitly (restore, wscript):
        // a no-op unless this VM's template download is still pending.
        self.ensure_pulled(std::slice::from_ref(&name.to_string()), None)
            .await?;
        self.events.emit("vm.starting", json!({"vm": name}));

        std::fs::create_dir_all(&vm.dirs.run)?;
        {
            let mut net = self.network.lock().await;
            let mut attachments = Vec::with_capacity(vm.cfg.nics.len());
            for (i, nic) in vm.cfg.nics.iter().enumerate() {
                let sock = vm.dirs.nic_sock(i);
                let _ = std::fs::remove_file(&sock);
                let seg = net
                    .segment_mut(nic_segment_name(nic))
                    .ok_or_else(|| anyhow!("unknown segment for nic {i}"))?;
                let mac = *vm
                    .macs
                    .get(i)
                    .ok_or_else(|| anyhow!("no persisted MAC for nic {i}"))?;
                attachments.push(seg.attach_nic(&sock, mac, nic.isolated).await?);
            }
            vm.set_nic_attachments(attachments).await;
        }

        let events_exit = self.events.clone();
        let events_ready = self.events.clone();
        let vm_name = name.to_string();
        let vm_name2 = name.to_string();
        vm.start(
            move |reason, status| {
                let payload = json!({"vm": vm_name, "reason": reason, "status": status});
                match reason {
                    StopReason::Crashed => {
                        events_exit.emit("vm.crashed", payload.clone());
                        events_exit.emit("vm.stopped", payload);
                    }
                    _ => events_exit.emit("vm.stopped", payload),
                }
            },
            move || {
                events_ready.emit("vm.ready", json!({"vm": vm_name2}));
            },
        )
        .await
    }

    /// Start one container: wire its NIC sockets into the segment switches
    /// (identically to a VM), then boot its micro-VM with event-emitting
    /// callbacks. Restarts driven by the container's restart policy happen
    /// inside the instance; the callbacks fire again on each attempt.
    pub async fn start_container(self: &Arc<Self>, name: &str) -> Result<()> {
        let container = self.container(name)?.clone();
        if container.state().await != PowerState::Stopped {
            return Ok(());
        }
        // Safety net (see start_vm): no-op unless this image is still pending.
        self.ensure_pulled(std::slice::from_ref(&name.to_string()), None)
            .await?;
        // Volumes mount from the lab's SMB server; make sure it is serving
        // (idempotent — a no-op when `up` already started it).
        if !container.volumes.is_empty() {
            let quiet: crate::scripting::OutputSink = std::sync::Arc::new(|_| {});
            self.ensure_smb(&quiet).await;
        }
        self.events
            .emit("container.starting", json!({"container": name}));

        std::fs::create_dir_all(&container.dirs.run)?;
        {
            let mut net = self.network.lock().await;
            for (i, nic) in container.cfg.nics.iter().enumerate() {
                let sock = container.dirs.nic_sock(i);
                let _ = std::fs::remove_file(&sock);
                let seg = net
                    .segment_mut(nic_segment_name(nic))
                    .ok_or_else(|| anyhow!("unknown segment for nic {i}"))?;
                seg.listen_nic(&sock, nic.isolated).await?;
            }
        }

        let events_exit = self.events.clone();
        let events_ready = self.events.clone();
        let events_health = self.events.clone();
        let me = self.clone();
        let n_exit = name.to_string();
        let n_ready = name.to_string();
        let n_health = name.to_string();
        let n_fwd = name.to_string();
        container
            .start(
                move |reason, exit_code, will_restart| {
                    let payload = json!({
                        "container": n_exit,
                        "reason": reason,
                        "exit_code": exit_code,
                        "restarting": will_restart,
                    });
                    if reason == StopReason::Crashed {
                        events_exit.emit("container.crashed", payload.clone());
                    }
                    if !will_restart {
                        events_exit.emit("container.stopped", payload);
                    }
                },
                move || {
                    events_ready.emit("container.ready", json!({"container": n_ready}));
                    // Forwards target the container's lease; (re-)install on
                    // every readiness so restarts keep them pointed right.
                    let me = me.clone();
                    let n = n_fwd.clone();
                    tokio::spawn(async move {
                        me.install_forwards(std::slice::from_ref(&n)).await;
                    });
                },
                move |healthy| {
                    if !healthy {
                        events_health.emit("container.unhealthy", json!({"container": n_health}));
                    }
                },
            )
            .await
    }

    /// Each machine's lease address and first hardware address — the only
    /// runtime state the [`forward_plan`] needs, gathered once so the plan
    /// itself touches no network. `scope` empty means the whole lab.
    async fn forward_observations(
        &self,
        scope: &[String],
    ) -> (
        std::collections::HashMap<String, std::net::Ipv4Addr>,
        std::collections::HashMap<String, crate::config::model::MacAddr>,
    ) {
        let mut leases = std::collections::HashMap::new();
        let mut macs = std::collections::HashMap::new();
        for m in self.machines() {
            let name = m.name().to_string();
            if !scope.is_empty() && !scope.contains(&name) {
                continue;
            }
            if let Some(mac) = m.macs().first() {
                macs.insert(name.clone(), *mac);
            }
            if let Ok(ip) = m.guest_ip(None).await
                && let Ok(ip) = ip.parse::<std::net::Ipv4Addr>()
            {
                leases.insert(name, ip);
            }
        }
        (leases, macs)
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
        let scope = [machine.to_string()];
        let (leases, macs) = self.forward_observations(&scope).await;
        let rule = forward_plan::web_page(
            &forward_plan::ForwardInputs {
                lab: &self.config.lab,
                machines: &scope,
                leases: &leases,
                macs: &macs,
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
            .vms
            .iter()
            .map(|v| (&v.name, &v.web))
            .chain(self.config.lab.containers.iter().map(|c| (&c.name, &c.web)))
            .find(|(name, _)| name.as_str() == machine)
            .and_then(|(_, pages)| pages.iter().find(|p| p.name == page))
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
                    // Post-start steps only a VM has: shares to mount, a
                    // template first-boot provision to run, and the bake hook
                    // template builds install.
                    if let Ok(vm) = me.vm(&n) {
                        // Detached, so provisions can rely on the shares
                        // (§7.5) without the wave blocking on the mount
                        // retry window.
                        me.spawn_share_mount(&n);
                        // Before this VM can be considered ready (§6.1). A
                        // no-op for templates without one, so leaf-VM timing
                        // is unchanged.
                        me.run_first_boot(&n, &out).await?;
                        // See `LabRuntime::pre_provision`.
                        let hook = me.pre_provision.read().expect("pre_provision lock").clone();
                        if let Some(hook) = hook {
                            hook(vm.clone(), out.clone()).await?;
                        }
                    }
                    // Only gate the wave on readiness when something later
                    // depends on this machine.
                    if me.has_dependents(&n) {
                        let m = me.machine(&n)?;
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

        self.events.emit("lab.up", json!({"vms": targets}));
        Ok(())
    }

    /// Mount a VM's SMB shares in a detached task once its agent answers.
    /// Mounting used to happen at the end of `up`, AFTER the provision
    /// pass — any provision waiting on a share waited on its own tail.
    fn spawn_share_mount(self: &Arc<Self>, name: &str) {
        let has_shares = self
            .config
            .lab
            .vms
            .iter()
            .any(|v| v.name == name && !v.shares.is_empty());
        if !has_shares {
            return;
        }
        let me = self.clone();
        let n = name.to_string();
        tokio::spawn(async move {
            let Ok(vm) = me.vm(&n).cloned() else { return };
            if vm.wait_ready(Duration::from_secs(600)).await.is_ok() {
                me.mount_shares(&n).await;
            }
        });
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
        let (leases, macs) = self.forward_observations(scope).await;
        let plan = forward_plan::plan(&forward_plan::ForwardInputs {
            lab: &self.config.lab,
            machines: scope,
            leases: &leases,
            macs: &macs,
        });
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
            let container = self.containers.contains_key(m);
            if container {
                self.containers[m]
                    .wait_ready(Duration::from_secs(300))
                    .await?;
            } else {
                self.vm(m)?.wait_ready(Duration::from_secs(600)).await?;
            }
            match &step.kind {
                plan::StepKind::Provision(p) => {
                    let script = self.root.join(&p.script);
                    output(format!("provision: {} → {m}\n", p.script.display()));
                    // The script reaches its own machine with `lab.this_vm()`;
                    // containers have no VM handle, so they don't bind one.
                    let owner = (!container).then(|| crate::scripting::ScriptOwner::provision(m));
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

    /// Run the backing template's first-boot provision the first time a clone
    /// is instantiated, before the VM is reported ready (PRD §6.1). For VMs
    /// with no pending first-boot the readiness poller already flips `ready`
    /// (and emits `vm.ready`), so this returns immediately without blocking —
    /// preserving the timing of templates that carry no first-boot script.
    ///
    /// For a pending first-boot it waits for the guest agent, runs the embedded
    /// script scoped to this VM (reached via `lab.this_vm()`), then writes the
    /// run-once sentinel, marks the VM ready, and emits `vm.ready`. Any error or
    /// the overall timeout fails `up` and leaves the VM running for inspection.
    async fn run_first_boot(
        self: &Arc<Self>,
        name: &str,
        output: &crate::scripting::OutputSink,
    ) -> Result<()> {
        let vm = self.vm(name)?.clone();
        if !vm.first_boot_pending() {
            return Ok(());
        }
        let script = vm
            .template()
            .first_boot_script
            .clone()
            .expect("first_boot_pending implies a script");

        output(format!("first-boot: provisioning {name}...\n"));
        vm.wait_agent_up(Duration::from_secs(600))
            .await
            .with_context(|| format!("first-boot {name}: agent did not come up"))?;

        // Hard ceiling: Windows specialize/OOBE can be slow, but a hung guest
        // must not wedge `up` forever.
        let label = format!("first-boot:{name}");
        let run = crate::scripting::run_script_source(
            self.clone(),
            script,
            &label,
            vm.dirs.local.clone(),
            Some(crate::scripting::ScriptOwner::first_boot(name)),
            output.clone(),
        );
        tokio::time::timeout(Duration::from_secs(1800), run)
            .await
            .map_err(|_| anyhow!("first-boot {name}: timed out after 1800s"))?
            .with_context(|| format!("first-boot provision for {name}"))?;

        std::fs::write(vm.dirs.firstboot_sentinel(), b"")
            .with_context(|| format!("writing first-boot sentinel for {name}"))?;
        vm.mark_ready().await;
        self.events.emit("vm.ready", json!({"vm": name}));
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
        self.down(&[], true).await?;
        for m in self.machines() {
            wait_settled(m.as_ref()).await;
        }
        // Removes clones, container overlays, AND named volumes — destroy is
        // the lab-scoped volume lifecycle boundary (PRD §12).
        remove_tree(&self.lab_local).await?;
        let run_dir = crate::paths::lab_runtime_dir(&self.name);
        let _ = remove_tree(&run_dir.join("vms")).await;
        let _ = remove_tree(&run_dir.join("containers")).await;
        Ok(())
    }

    /// Stop one VM and delete its clone and runtime state, leaving the rest of
    /// the lab running. The VM stays in the lab config, so a later `up <vm>`
    /// re-clones it from the template (per-VM analogue of [`destroy`]).
    pub async fn destroy_vm(self: &Arc<Self>, name: &str) -> Result<()> {
        let vm = self.vm(name)?.clone();
        vm.stop(true).await?;
        wait_settled(vm.as_ref()).await;
        remove_tree(&vm.dirs.local).await?;
        let _ = remove_tree(&vm.dirs.run).await;
        self.events
            .emit("vm.destroyed", json!({"vm": name.to_string()}));
        Ok(())
    }

    /// Stop one container and delete its writable overlay, runtime state,
    /// and pinned image digest — the config stays, so a later `up <name>`
    /// re-resolves the image fresh. Named volumes are lab-scoped and
    /// survive; only lab [`destroy`] removes them.
    pub async fn destroy_container(self: &Arc<Self>, name: &str) -> Result<()> {
        let container = self.container(name)?.clone();
        container.stop(true).await?;
        wait_settled(container.as_ref()).await;
        remove_tree(&container.dirs.local).await?;
        let _ = remove_tree(&container.dirs.run).await;
        {
            let mut state = self.state.lock().await;
            let c = state.machine_mut(name);
            c.image_digest = None;
            c.image_ref = None;
            // The scratch qcow2 (which held the snapshot data) is gone.
            c.snapshots.clear();
            state.save(&self.lab_local)?;
        }
        self.events.emit(
            "container.destroyed",
            json!({"container": name.to_string()}),
        );
        Ok(())
    }

    /// The lab status projection (ADR-0004) — produced here, rendered unchanged
    /// by the CLI, the REST surface and the console.
    pub async fn status(&self) -> LabStatus {
        // One projection for both kinds: each adapter fills its own variant
        // (see `Machine::status_detail`), so a machine is described in exactly
        // one place.
        let mut machines = Vec::new();
        for m in self.machines() {
            let mut status = m.status().await;
            // Lab-level, not machine-level: is this machine's template/image
            // download still pending? Drives the "Download" button.
            status.cached = !self.pulls.lock_recover().is_pending(&status.name);
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
        self.vms.values().any(|vm| vm.dirs.local.exists())
            || self.containers.values().any(|c| c.dirs.local.exists())
            || self.lab_local.join("volumes").exists()
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
        for name in self.vms.keys().chain(self.containers.keys()) {
            let online = self.snapshot(name, snap).await?;
            results.push(json!({"vm": name, "online": online}));
        }
        Ok(json!(results))
    }

    pub async fn restore(self: &Arc<Self>, vm_name: &str, snap: &str) -> Result<()> {
        if self.containers.contains_key(vm_name) {
            return self.restore_container(vm_name, snap).await;
        }
        let record = {
            let mut state = self.state.lock().await;
            state.machine_mut(vm_name).snapshots.get(snap).cloned()
        }
        .ok_or_else(|| anyhow!("vm \"{vm_name}\" has no snapshot \"{snap}\""))?;

        let vm = self.vm(vm_name)?.clone();
        // Restoring into a running VM needs NIC listeners only if we must
        // boot QEMU; reuse start_vm's wiring through the callbacks below.
        if record.online && vm.state().await == PowerState::Stopped {
            // Boot paused first via the normal path, then load.
            self.start_vm(vm_name).await?;
        }
        let events_exit = self.events.clone();
        let events_ready = self.events.clone();
        let n1 = vm_name.to_string();
        let n2 = vm_name.to_string();
        vm.restore(
            snap,
            record.online,
            move |reason, status| {
                events_exit.emit(
                    "vm.stopped",
                    json!({"vm": n1, "reason": reason, "status": status}),
                );
            },
            move || events_ready.emit("vm.ready", json!({"vm": n2})),
        )
        .await?;
        self.events.emit(
            "snapshot.restored",
            json!({"vm": vm_name, "name": snap, "online": record.online}),
        );
        Ok(())
    }

    /// Restore a container snapshot with full VM semantics (PRD §18): an
    /// online record boots the micro-VM if needed, loads the snapshot and
    /// resumes exactly where it was; an offline record reverts the scratch
    /// disk and leaves the container stopped. Volume contents are host state
    /// and never roll back.
    async fn restore_container(self: &Arc<Self>, name: &str, snap: &str) -> Result<()> {
        let record = {
            let mut state = self.state.lock().await;
            state.machine_mut(name).snapshots.get(snap).cloned()
        }
        .ok_or_else(|| anyhow!("container \"{name}\" has no snapshot \"{snap}\""))?;

        // The image must be bound before the pin comparison below (a daemon
        // restarted after a cache wipe re-pends the pull).
        self.ensure_pulled(std::slice::from_ref(&name.to_string()), None)
            .await?;
        let container = self.container(name)?.clone();
        // The scratch overlay (and any vmstate) is only valid against the
        // rootfs it was captured over — refuse a changed image pin.
        let current = container.image_digest();
        if let Some(want) = &record.image_digest
            && Some(want) != current.as_ref()
        {
            bail!(
                "container \"{name}\": snapshot \"{snap}\" was taken against image {want}, but \
                 the pinned image is now {} — destroy the container (clearing its snapshots) or \
                 restore the original pin",
                current.as_deref().unwrap_or("<not pulled>")
            );
        }

        if record.online {
            // Ensure a running micro-VM to load into — the normal start path
            // wires NIC listeners and the event callbacks. Whatever the
            // fresh boot writes is rewound by the load.
            if container.state().await == PowerState::Stopped {
                self.start_container(name).await?;
            }
            container.restore_online(snap).await?;
            // Re-point forwards / re-prime the NAT MAC at the restored lease.
            self.install_forwards(&[name.to_string()]).await;
        } else {
            container.restore_offline(snap).await?;
        }
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
