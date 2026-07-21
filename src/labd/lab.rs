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
use super::network::{LabNetwork, nic_segment_name};
use super::state::{LabState, SnapshotRecord, generate_mac};
use super::vm::{PowerState, StopReason, VmDirs, VmInstance};
use crate::config::LabFile;
use crate::config::model::TemplateRef;
use crate::profiles::ProfileSet;
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
    /// Forward ids installed for container `port {}` blocks, keyed by
    /// container — removed and re-installed when a restart brings a new
    /// lease, so a forward never points at a stale IP.
    container_forwards: Mutex<std::collections::HashMap<String, Vec<(String, u64)>>>,
    /// Loopback forwards backing proxied web pages, keyed by (machine, page).
    /// Revalidated on each `web.forward` (lease IP compare) so restarts and
    /// re-leases self-heal without hooking start events.
    web_forwards: Mutex<std::collections::HashMap<(String, String), WebForward>>,
    /// Kept for post-pull re-resolution (deferred templates fold their meta
    /// into the hardware resolution only once pulled).
    profiles: ProfileSet,
    /// Machines whose template/image is not in the local cache yet — the
    /// deferred-pull work list. [`Self::ensure_pulled`] drains it; `status`
    /// reports it as `template_cached` / `image_cached`.
    pending_pulls: Mutex<BTreeMap<String, PendingPull>>,
    /// Serialises pull runs (concurrent `up` + `pull` + `vm.start` must not
    /// double-download); the loser re-checks the pending list and no-ops.
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

/// One outstanding deferred download.
#[derive(Clone)]
enum PendingPull {
    Template { reference: String, arch: String },
    Image { reference: String },
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

        let mut state = LabState::load(&lab_local);
        let store = TemplateStore::new(crate::paths::template_store_dir());
        let mut network = LabNetwork::build(&config.lab)?;
        let mut pending: BTreeMap<String, PendingPull> = BTreeMap::new();

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
                                PendingPull::Template {
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
            let vm_state = state.vm_mut(&vm_cfg.name);
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
            let media_cache = crate::media::MediaCache::new(lab_local.join("media"));
            for m in &vm_cfg.media {
                let src = root.join(&m.from);
                let built = media_cache
                    .ensure(m.kind, &src, m.label.as_deref())
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
                .map(|s| resolve_share_host(&root, &s.host))
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
                let c_state = state.container_mut(&c_cfg.name);
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
                    pending.insert(c_cfg.name.clone(), PendingPull::Image { reference });
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
                let container = ContainerInstance::new(
                    &name,
                    c_cfg.clone(),
                    arch,
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
                .vms
                .iter()
                .map(|(n, v)| (n.clone(), v.macs.clone()))
                .chain(
                    state
                        .containers
                        .iter()
                        .map(|(n, c)| (n.clone(), c.macs.clone())),
                )
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
            container_forwards: Mutex::new(std::collections::HashMap::new()),
            web_forwards: Mutex::new(std::collections::HashMap::new()),
            profiles: profiles.clone(),
            pending_pulls: Mutex::new(pending),
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
    /// Serialised by `pull_lock`; the work list is re-read under the lock so
    /// a concurrent caller that lost the race finds nothing left to do. A
    /// failed download emits `.error` and fails the caller; the pending
    /// entry survives for retry.
    pub async fn ensure_pulled(
        self: &Arc<Self>,
        targets: &[String],
        output: Option<&crate::scripting::OutputSink>,
    ) -> Result<()> {
        // Cheap common case: nothing pending anywhere.
        if self.pending_pulls.lock().await.is_empty() {
            return Ok(());
        }
        let _guard = self.pull_lock.lock().await;
        let work: Vec<(String, PendingPull)> = self
            .pending_pulls
            .lock()
            .await
            .iter()
            .filter(|(n, _)| targets.is_empty() || targets.contains(n))
            .map(|(n, p)| (n.clone(), p.clone()))
            .collect();
        for (machine, job) in work {
            match job {
                PendingPull::Template { reference, arch } => {
                    self.pull_template(&machine, &reference, &arch, output)
                        .await?;
                }
                PendingPull::Image { reference } => {
                    self.pull_image(&machine, &reference, output).await?;
                }
            }
            self.pending_pulls.lock().await.remove(&machine);
        }
        Ok(())
    }

    /// Pull one registry template, then bind the resolved parts (hardware
    /// re-resolution with the template meta, backing disk, first-boot script)
    /// into the VM instance.
    async fn pull_template(
        self: &Arc<Self>,
        vm_name: &str,
        reference: &str,
        arch: &str,
        output: Option<&crate::scripting::OutputSink>,
    ) -> Result<()> {
        let store = TemplateStore::new(crate::paths::template_store_dir());
        self.events.emit(
            "template.pull.start",
            json!({"vm": vm_name, "reference": reference, "arch": arch}),
        );
        if let Some(out) = output {
            out(format!("pull: {reference} ({arch})\n"));
        }
        let events = self.events.clone();
        let vm_s = vm_name.to_string();
        let ref_s = reference.to_string();
        let mut progress = move |p: crate::oci::PullProgress| {
            let percent = p
                .bytes_done
                .saturating_mul(100)
                .checked_div(p.bytes_total)
                .unwrap_or(0) as u32;
            events.emit(
                "template.pull.progress",
                json!({
                    "vm": vm_s,
                    "reference": ref_s,
                    "chunk": p.chunk,
                    "chunks": p.chunks,
                    "bytes_done": p.bytes_done,
                    "bytes_total": p.bytes_total,
                    "percent": percent,
                }),
            );
        };
        let result =
            crate::oci::ensure_registry_template(reference, arch, &store, &mut progress).await;
        drop(progress);
        match result {
            Ok(resolved) => {
                self.events.emit(
                    "template.pull.done",
                    json!({"vm": vm_name, "reference": reference}),
                );
                if let Some(out) = output {
                    out(format!("pull: {reference} done\n"));
                }
                let vm_cfg = self
                    .config
                    .lab
                    .vms
                    .iter()
                    .find(|v| v.name == vm_name)
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
                Ok(())
            }
            Err(e) => {
                self.events.emit(
                    "template.pull.error",
                    json!({"vm": vm_name, "reference": reference, "error": format!("{e:#}")}),
                );
                Err(e.context(format!("pulling template for vm \"{vm_name}\"")))
            }
        }
    }

    /// Pull one container image, pin its digest into the lab state, and bind
    /// it (re-merging the cinit spec) into the container instance.
    async fn pull_image(
        self: &Arc<Self>,
        name: &str,
        reference: &str,
        output: Option<&crate::scripting::OutputSink>,
    ) -> Result<()> {
        let arch = std::env::consts::ARCH;
        let cache = crate::oci::image::ImageCache::new(crate::paths::oci_cache_dir());
        self.events.emit(
            "container.pull.start",
            json!({"container": name, "reference": reference, "arch": arch}),
        );
        if let Some(out) = output {
            out(format!("pull: {reference}\n"));
        }
        let events = self.events.clone();
        let cn_s = name.to_string();
        let ref_s = reference.to_string();
        let mut progress = move |p: crate::oci::image::ImagePullProgress| {
            let percent = p
                .bytes_done
                .saturating_mul(100)
                .checked_div(p.bytes_total)
                .unwrap_or(0) as u32;
            events.emit(
                "container.pull.progress",
                json!({
                    "container": cn_s,
                    "reference": ref_s,
                    "layer": p.layer,
                    "layers": p.layers,
                    "bytes_done": p.bytes_done,
                    "bytes_total": p.bytes_total,
                    "percent": percent,
                }),
            );
        };
        let result =
            crate::oci::image::ensure_container_image(reference, arch, &cache, &mut progress).await;
        drop(progress);
        match result {
            Ok(image) => {
                self.events.emit(
                    "container.pull.done",
                    json!({"container": name, "reference": reference}),
                );
                if let Some(out) = output {
                    out(format!("pull: {reference} done\n"));
                }
                let container = self.container(name)?;
                {
                    let mut state = self.state.lock().await;
                    let c_state = state.container_mut(name);
                    c_state.image_digest = Some(image.manifest_digest.clone());
                    c_state.image_ref = Some(container.cfg.image.reference.clone());
                    state.save(&self.lab_local)?;
                }
                container.set_image(image);
                Ok(())
            }
            Err(e) => {
                self.events.emit(
                    "container.pull.error",
                    json!({"container": name, "reference": reference, "error": format!("{e:#}")}),
                );
                Err(e.context(format!("pulling image for container \"{name}\"")))
            }
        }
    }

    /// Start the SMB server for the lab's shares — VM `share {}` blocks and
    /// container volumes that fall back to CIFS (PRD §18: volumes ride
    /// virtiofs when the host has a virtiofsd, smbd otherwise)
    /// — and DNAT each relevant segment gateway's port 445 to it (PRD §7.5).
    /// Best-effort: a failure is logged and the rest of the lab still works.
    /// Idempotent; called from `up` and from any individual container start.
    async fn ensure_smb(self: &Arc<Self>, output: &crate::scripting::OutputSink) {
        if self.smb.lock().await.is_some() {
            return; // already serving
        }
        // Collect sharing VMs and volume-declaring containers with their
        // gateway IP (first NIC's segment).
        let mut sharing: Vec<(String, std::net::Ipv4Addr, Vec<crate::config::model::Share>)> =
            Vec::new();
        let mut seg_ports: Vec<String> = Vec::new();
        // Container name → the gateway its volumes mount from, for the
        // post-spawn SmbInfo handout.
        let mut volume_gateways: Vec<(String, std::net::Ipv4Addr)> = Vec::new();
        {
            let net = self.network.lock().await;
            for vm in &self.config.lab.vms {
                if vm.shares.is_empty() {
                    continue;
                }
                // Shares riding virtiofs (§7.5) are served by per-share
                // virtiofsd daemons at VM start — smbd only exports the rest.
                let vfs: std::collections::HashSet<usize> = self
                    .vm(&vm.name)
                    .map(|i| i.virtiofs_share_indices().into_iter().collect())
                    .unwrap_or_default();
                let mut shares: Vec<crate::config::model::Share> = vm
                    .shares
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !vfs.contains(i))
                    .map(|(_, s)| s.clone())
                    .collect();
                if shares.is_empty() {
                    continue;
                }
                let Some(nic) = vm.nics.first() else { continue };
                let seg_name = nic_segment_name(nic);
                let Some(seg) = net.segments.get(seg_name) else {
                    continue;
                };
                for s in &mut shares {
                    s.host = resolve_share_host(&self.root, &s.host);
                }
                sharing.push((vm.name.clone(), seg.service_ip, shares));
                if !seg_ports.contains(&seg_name.to_string()) {
                    seg_ports.push(seg_name.to_string());
                }
            }
            // Containers only need smbd for the CIFS fallback: with a
            // virtiofsd on the host their volumes attach as vhost-user-fs
            // devices instead (spawned per container start, PRD §18).
            let containers_on_cifs = !crate::qemu::virtiofsd::available();
            for (name, container) in &self.containers {
                if container.volumes.is_empty() || !containers_on_cifs {
                    continue;
                }
                // Validated: volumes require a NIC (§5.1).
                let Some(nic) = container.cfg.nics.first() else {
                    continue;
                };
                let seg_name = nic_segment_name(nic);
                let Some(seg) = net.segments.get(seg_name) else {
                    continue;
                };
                // Volume hosts are pre-resolved (resolve_volume_hosts); the
                // guest target rides along for smb.conf comments only. Read
                // targets from the config (1:1 with the resolved exports) so
                // the SMB plan doesn't depend on the image being pulled yet.
                let shares = container
                    .volumes
                    .iter()
                    .zip(container.cfg.volumes.iter())
                    .map(|((share, host, ro), vol)| crate::config::model::Share {
                        span: (0, 0),
                        host: host.clone(),
                        guest: vol.target.clone(),
                        readonly: *ro,
                        smb1: false,
                        name: share.clone(),
                        transport: crate::config::model::ShareTransport::Smb,
                    })
                    .collect();
                sharing.push((name.clone(), seg.service_ip, shares));
                volume_gateways.push((name.clone(), seg.service_ip));
                if !seg_ports.contains(&seg_name.to_string()) {
                    seg_ports.push(seg_name.to_string());
                }
            }
        }
        if sharing.is_empty() {
            return;
        }

        // smbd needs a free localhost port; the gateway DNAT hides the
        // number from guests, so walk upward from a base until one binds
        // (another lab's smbd — or an orphan from an unclean daemon death —
        // may hold the earlier ones).
        let base_port = 14450u16;
        let mut labsmb = None;
        let mut last_err = String::new();
        for port in base_port..base_port + 10 {
            let mut candidate =
                crate::smb::LabSmb::plan(&self.name, &self.lab_local, port, &sharing);
            let config = candidate.build_config();
            match candidate.spawn(config) {
                Ok(p) => {
                    tracing::info!("SMB server for lab {} on 127.0.0.1:{p}", self.name);
                    output(format!(
                        "smb: serving shares on 127.0.0.1:{p} (guest mounts \\\\<gateway>\\<share>; credentials in .vmlab/smb/creds)\n"
                    ));
                    self.events.emit("smb.started", json!({"port": p}));
                    labsmb = Some(candidate);
                    break;
                }
                Err(e) => {
                    tracing::warn!("smbd on port {port} failed: {e}");
                    last_err = e.to_string();
                }
            }
        }
        let Some(labsmb) = labsmb else {
            tracing::warn!("SMB server failed to start: {last_err}");
            output(format!(
                "WARNING: SMB server failed to start — shares will not mount: {last_err}\n"
            ));
            self.events.emit("smb.failed", json!({"error": last_err}));
            return;
        };

        // DNAT gateway:445 → 127.0.0.1:smbd on each sharing segment, so a
        // guest mounting \\<gateway>\<share> reaches the local smbd via NAT.
        {
            let net = self.network.lock().await;
            for seg_name in &seg_ports {
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
        for (name, gateway) in volume_gateways {
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

    /// Mount a VM's SMB shares through the guest agent (PRD §7.5). Linux
    /// guests use cifs; Windows guests use net use / mklink. XP-era guests
    /// without an agent are mounted by provision scripts via screen
    /// automation instead (documented; not attempted here).
    async fn mount_shares(self: &Arc<Self>, vm_name: &str) {
        let cfg = self.config.lab.vms.iter().find(|v| v.name == vm_name);
        let Some(cfg) = cfg else { return };
        if cfg.shares.is_empty() {
            return;
        }

        // Detect the guest OS family from the resolved profile (which folds
        // in template metadata — the lab vm block usually omits `profile`).
        let Ok(vm) = self.vm(vm_name) else { return };
        let os_hint = guest_os_hint(vm.template().resolved.profile.as_deref());

        // virtiofs shares first (§7.5). Linux: mkdir + `mount -t virtiofs`
        // by tag. Windows: register the WinFsp launcher class for
        // virtiofs.exe (idempotent reg adds; virtio-win ships the binary but
        // not the class), then a per-tag launchctl instance mounts the tag
        // at the drive letter. Needs WinFsp in the template — without it
        // the launchctl step fails and surfaces as the usual mount warning.
        let mut steps: Vec<crate::smb::MountStep> = Vec::new();
        let virtiofs_mounts = vm.virtiofs_mounts().await;
        if os_hint == crate::smb::OsHint::Windows && !virtiofs_mounts.is_empty() {
            const CLASS: &str = r"HKLM\Software\WOW6432Node\WinFsp\Services\virtiofs";
            for (value, kind, data) in [
                (
                    "Executable",
                    "REG_SZ",
                    r"C:\Program Files\Virtio-Win\VioFS\virtiofs.exe",
                ),
                ("CommandLine", "REG_SZ", "-t %1 -m %2"),
                ("Security", "REG_SZ", "D:P(A;;RPWPLC;;;WD)"),
                ("JobControl", "REG_DWORD", "1"),
            ] {
                steps.push(crate::smb::MountStep {
                    os_hint,
                    command: "reg".into(),
                    args: vec![
                        "add".into(),
                        CLASS.into(),
                        "/v".into(),
                        value.into(),
                        "/t".into(),
                        kind.into(),
                        "/d".into(),
                        data.into(),
                        "/f".into(),
                    ],
                });
            }
        }
        for m in &virtiofs_mounts {
            if os_hint == crate::smb::OsHint::Windows {
                steps.push(crate::smb::MountStep {
                    os_hint,
                    command: r"C:\Program Files (x86)\WinFsp\bin\launchctl-x64.exe".into(),
                    args: vec![
                        "start".into(),
                        "virtiofs".into(),
                        format!("viofs-{}", m.tag),
                        m.tag.clone(),
                        m.guest.clone(),
                    ],
                });
                continue;
            }
            steps.push(crate::smb::MountStep {
                os_hint,
                command: "mkdir".into(),
                args: vec!["-p".into(), m.guest.clone()],
            });
            let mut args = vec![
                "-t".into(),
                "virtiofs".into(),
                m.tag.clone(),
                m.guest.clone(),
            ];
            if m.readonly {
                args.push("-o".into());
                args.push("ro".into());
            }
            steps.push(crate::smb::MountStep {
                os_hint,
                command: "mount".into(),
                args,
            });
        }

        // Then the SMB plan for whatever smbd serves for this VM.
        {
            let smb = self.smb.lock().await;
            if let Some(labsmb) = smb.as_ref() {
                steps.extend(labsmb.mount_plan(vm_name, os_hint));
            }
        }
        if steps.is_empty() {
            return;
        }
        let Ok(agent) = vm.agent().await else {
            tracing::warn!("{vm_name}: no agent, cannot auto-mount shares");
            return;
        };
        for step in steps {
            let mut argv = vec![step.command.clone()];
            argv.extend(step.args.iter().cloned());
            // Early after boot Windows can't run the mount yet: the agent
            // briefly fails to spawn children, then `net use` returns
            // error 67 until the SMB client service is up (observed ~3-4
            // minutes on Server 2025) — retry across a generous window.
            let mut last: Option<String> = None;
            for attempt in 0..30 {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_secs(10)).await;
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

    /// `depends_on` of a machine (VM or container) by name.
    fn machine_deps(&self, name: &str) -> Option<&[String]> {
        if let Some(v) = self.config.lab.vms.iter().find(|v| v.name == name) {
            return Some(&v.depends_on);
        }
        self.config
            .lab
            .containers
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.depends_on.as_slice())
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
                let emu = crate::qemu::emulator_binary(&c.arch);
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
        let playbooks = &self.config.lab.playbooks;
        if playbooks.is_empty() {
            return Ok(());
        }
        let dir = playbook::default_bin_dir(self.host_cfg.config_weave_bin_dir.as_deref());
        let mut errs: Vec<String> = Vec::new();
        for name in targets {
            let targeted = playbooks
                .iter()
                .any(|p| p.vms.is_empty() || p.vms.iter().any(|v| v == name));
            if !targeted {
                continue;
            }
            let (os, arch) = if let Some(c) = self.containers.get(name) {
                (playbook::GuestOs::Linux, c.arch.clone())
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
                        me.install_container_ports(&n).await;
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

    /// Install a container's `port {}` forwards on its first NIC's segment —
    /// the same NAT forward machinery as segment `forward {}` blocks.
    /// Best-effort, like [`install_declared_forwards`].
    async fn install_container_ports(self: &Arc<Self>, name: &str) {
        let Ok(container) = self.container(name) else {
            return;
        };
        if container.cfg.ports.is_empty() {
            return;
        }
        let Some(nic) = container.cfg.nics.first() else {
            return; // validated: ports require a NIC
        };
        let Ok(ip) = container.guest_ip().await else {
            self.events.emit(
                "forward.skipped",
                json!({"reason": "no lease", "container": name}),
            );
            return;
        };
        let Ok(guest_ip) = ip.parse::<std::net::Ipv4Addr>() else {
            return;
        };
        let seg_name = nic_segment_name(nic).to_string();
        let net = self.network.lock().await;
        // Drop forwards from a previous run/lease before re-installing.
        let stale = self
            .container_forwards
            .lock()
            .await
            .remove(name)
            .unwrap_or_default();
        for (seg, id) in stale {
            if let Some(s) = net.segments.get(&seg).and_then(|s| s.services.as_ref()) {
                s.remove_forward(id);
            }
        }
        let Some(services) = net
            .segments
            .get(&seg_name)
            .and_then(|s| s.services.as_ref())
        else {
            return;
        };
        // Prime the NAT engine with the lease MAC: a container that never
        // originates egress (an idle nginx, say) is otherwise unreachable —
        // the engine would broadcast the SYN and the guest TCP stack drops
        // broadcast-framed segments.
        if let Some(mac) = container.macs.first() {
            services.learn_mac(guest_ip, *mac);
        }
        let mut installed = Vec::new();
        for port in &container.cfg.ports {
            let host_addr =
                std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port.host_port));
            match services.add_forward(host_addr, guest_ip, port.container_port, port.proto) {
                Ok(id) => installed.push((seg_name.clone(), id)),
                Err(e) => {
                    self.events.emit(
                        "forward.skipped",
                        json!({
                            "reason": e.to_string(),
                            "container": name,
                            "host_port": port.host_port,
                        }),
                    );
                }
            }
        }
        self.container_forwards
            .lock()
            .await
            .insert(name.to_string(), installed);
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
        // Locate the declared page + the machine's runtime handle (VM or
        // container share the namespace).
        let (web, macs, ip_res) =
            if let Some(v) = self.config.lab.vms.iter().find(|v| v.name == machine) {
                let inst = self.vm(machine)?;
                let web = v
                    .web
                    .iter()
                    .find(|w| w.name == page)
                    .ok_or_else(|| anyhow!("no web page \"{page}\" on \"{machine}\""))?;
                let first_seg = v.nics.first().map(nic_segment_name).map(str::to_string);
                (
                    (web.clone(), first_seg),
                    inst.macs.clone(),
                    inst.guest_ip(None).await,
                )
            } else if let Some(c) = self
                .config
                .lab
                .containers
                .iter()
                .find(|c| c.name == machine)
            {
                let inst = self.container(machine)?;
                let web = c
                    .web
                    .iter()
                    .find(|w| w.name == page)
                    .ok_or_else(|| anyhow!("no web page \"{page}\" on \"{machine}\""))?;
                let first_seg = c.nics.first().map(nic_segment_name).map(str::to_string);
                (
                    (web.clone(), first_seg),
                    inst.macs.clone(),
                    inst.guest_ip().await,
                )
            } else {
                bail!("no machine \"{machine}\" in lab \"{}\"", self.name);
            };
        let (web, seg_name) = web;
        let seg_name = seg_name.ok_or_else(|| {
            anyhow!("web page \"{page}\" on \"{machine}\" needs a NIC to reach it over")
        })?;
        let ip = ip_res.map_err(|_| {
            anyhow!("\"{machine}\" has no network lease yet — is it running and ready?")
        })?;
        let guest_ip: std::net::Ipv4Addr = ip
            .parse()
            .map_err(|_| anyhow!("machine \"{machine}\" has a non-IPv4 lease"))?;

        let key = (machine.to_string(), page.to_string());
        // Cache hit whose lease still matches → reuse the live forward.
        {
            let cache = self.web_forwards.lock().await;
            if let Some(f) = cache.get(&key)
                && f.guest_ip == guest_ip
            {
                return Ok(json!({
                    "addr": f.addr.to_string(),
                    "guest_ip": guest_ip.to_string(),
                    "port": web.port,
                    "auth": web.auth,
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
        let Some(services) = net
            .segments
            .get(&seg_name)
            .and_then(|s| s.services.as_ref())
        else {
            bail!("segment \"{seg_name}\" has no services — is the lab up?");
        };
        // Prime the NAT engine with the lease MAC (see install_container_ports).
        if let Some(mac) = macs.first() {
            services.learn_mac(guest_ip, *mac);
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| anyhow!("web forward bind failed: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| anyhow!("web forward addr failed: {e}"))?;
        let id = services
            .add_forward_bound(listener, guest_ip, web.port)
            .map_err(|e| {
                anyhow!("web page \"{page}\" needs NAT/egress on segment \"{seg_name}\": {e}")
            })?;
        self.web_forwards.lock().await.insert(
            key,
            WebForward {
                segment: seg_name,
                id,
                addr,
                guest_ip,
            },
        );
        Ok(json!({
            "addr": addr.to_string(),
            "guest_ip": guest_ip.to_string(),
            "port": web.port,
            "auth": web.auth,
        }))
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
        // One machine list spanning VMs and containers — they share the
        // dependency graph and the waves.
        let all_machines: Vec<String> = self
            .config
            .lab
            .vms
            .iter()
            .map(|v| v.name.clone())
            .chain(self.config.lab.containers.iter().map(|c| c.name.clone()))
            .collect();
        let targets: Vec<String> = if subset.is_empty() {
            all_machines.clone()
        } else {
            for s in subset {
                if !self.vms.contains_key(s) && !self.containers.contains_key(s) {
                    bail!("no vm or container \"{s}\" in lab \"{}\"", self.name);
                }
            }
            // Pull in transitive dependencies of the subset.
            let mut wanted: HashSet<String> = HashSet::new();
            let mut stack: Vec<String> = subset.to_vec();
            while let Some(n) = stack.pop() {
                if wanted.insert(n.clone())
                    && let Some(deps) = self.machine_deps(&n)
                {
                    stack.extend(deps.iter().cloned());
                }
            }
            all_machines
                .iter()
                .filter(|n| wanted.contains(*n))
                .cloned()
                .collect()
        };

        // Deferred template/image downloads happen here — before the binary
        // preflight (pulled meta can change the resolved firmware/TPM needs)
        // and before any clone or boot work, streaming progress to both the
        // CLI sink and the event feed.
        self.ensure_pulled(&targets, Some(&output)).await?;

        self.preflight_binaries(&targets)?;

        // Start the SMB server before guests boot so shares are reachable
        // during provisioning (PRD §7.5).
        self.ensure_smb(&output).await;

        let mut remaining: Vec<String> = targets.clone();
        let mut done: HashSet<String> = HashSet::new();
        let steps = merged_up_steps(&self.config.lab);
        let mut next_step = 0usize;
        while !remaining.is_empty() {
            // A wave: every remaining machine whose deps (within the target
            // set) are all done.
            let wave: Vec<String> = remaining
                .iter()
                .filter(|n| {
                    self.machine_deps(n)
                        .unwrap_or(&[])
                        .iter()
                        .all(|d| done.contains(d) || !targets.contains(d))
                })
                .cloned()
                .collect();
            if wave.is_empty() {
                bail!("dependency deadlock among: {}", remaining.join(", "));
            }

            let mut handles = Vec::new();
            for name in &wave {
                let me = self.clone();
                let n = name.clone();
                let out = output.clone();
                handles.push(tokio::spawn(async move {
                    if me.containers.contains_key(&n) {
                        me.start_container(&n).await?;
                        // Only gate the wave on readiness when something
                        // later depends on this container. Container
                        // entrypoints start fast; healthchecks govern the
                        // rest — 300s is generous.
                        if me.has_dependents(&n) {
                            me.container(&n)?
                                .wait_ready(Duration::from_secs(300))
                                .await?;
                        }
                        return Ok::<_, anyhow::Error>(n);
                    }
                    me.start_vm(&n).await?;
                    // Mount the VM's shares as soon as its agent answers —
                    // detached, so provisions can rely on them (§7.5)
                    // without the wave blocking on the mount retry window.
                    me.spawn_share_mount(&n);
                    // Run the template's first-boot provision before this VM
                    // can be considered ready (§6.1). A no-op for templates
                    // without one, so leaf-VM timing is unchanged.
                    me.run_first_boot(&n, &out).await?;
                    // Pre-provision hook (template builds bake the
                    // vmlab-agent here — see `LabRuntime::pre_provision`).
                    let hook = me.pre_provision.read().expect("pre_provision lock").clone();
                    if let Some(hook) = hook {
                        hook(me.vm(&n)?.clone(), out.clone()).await?;
                    }
                    // Only gate the wave on readiness when something later
                    // depends on this VM.
                    if me.has_dependents(&n) {
                        me.vm(&n)?.wait_ready(Duration::from_secs(600)).await?;
                    }
                    Ok::<_, anyhow::Error>(n)
                }));
            }
            for h in handles {
                let n = h.await.map_err(|e| anyhow!("join: {e}"))??;
                done.insert(n.clone());
                remaining.retain(|x| x != &n);
            }

            // Between waves: run (in declaration order) every unrun
            // provision/playbook scoped entirely to already-started VMs,
            // so a VM depending on "dc01" starts only after dc01's
            // configuration steps completed (§7.2).
            self.run_up_steps(&steps, &mut next_step, &done, false, &targets, &output)
                .await?;
        }

        // Final pass: everything left, including unscoped steps.
        self.run_up_steps(&steps, &mut next_step, &done, true, &targets, &output)
            .await?;

        self.install_declared_forwards().await;

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

    /// Wire each segment's declared `forward {}` rules (PRD §9.8) once
    /// machines have leases. Targets resolve against VMs and containers.
    /// Best-effort: a forward to a not-yet-ready machine is skipped.
    async fn install_declared_forwards(self: &Arc<Self>) {
        for seg in &self.config.lab.segments {
            for fwd in &seg.forwards {
                let ip = if let Ok(vm) = self.vm(&fwd.vm) {
                    vm.guest_ip(None).await
                } else if let Ok(c) = self.container(&fwd.vm) {
                    c.guest_ip().await
                } else {
                    continue;
                };
                let Ok(ip) = ip else {
                    self.events.emit(
                        "forward.skipped",
                        json!({"reason": "no lease", "vm": fwd.vm, "host_port": fwd.host_port}),
                    );
                    continue;
                };
                let Ok(guest_ip) = ip.parse::<std::net::Ipv4Addr>() else {
                    continue;
                };
                let host_addr =
                    std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, fwd.host_port));
                let net = self.network.lock().await;
                if let Some(services) = net
                    .segments
                    .get(&seg.name)
                    .and_then(|s| s.services.as_ref())
                {
                    // Container targets: prime the lease MAC so a forward to
                    // an egress-quiet container works from the first SYN
                    // (see install_container_ports).
                    if let Ok(c) = self.container(&fwd.vm)
                        && let Some(mac) = c.macs.first()
                    {
                        services.learn_mac(guest_ip, *mac);
                    }
                    let _ = services.add_forward(host_addr, guest_ip, fwd.guest_port, fwd.proto);
                }
            }
        }
    }

    /// Run configuration steps (provision scripts and playbook applies —
    /// one declaration-ordered queue, see [`merged_up_steps`]) in strict
    /// order starting at `*next`: a scoped step runs once all its machines
    /// are started (waiting for their readiness first); an unscoped step
    /// runs only in the final pass. Stops at the first step that isn't
    /// eligible yet.
    async fn run_up_steps(
        self: &Arc<Self>,
        steps: &[UpStep],
        next: &mut usize,
        started: &HashSet<String>,
        final_pass: bool,
        targets: &[String],
        output: &crate::scripting::OutputSink,
    ) -> Result<()> {
        while *next < steps.len() {
            let step = &steps[*next];
            let scoped = step.vms();
            let eligible = if scoped.is_empty() {
                final_pass
            } else {
                scoped.iter().all(|v| started.contains(v))
            };
            if !eligible {
                return Ok(());
            }
            for m in scoped {
                if let Some(c) = self.containers.get(m) {
                    c.wait_ready(Duration::from_secs(300)).await?;
                } else {
                    self.vm(m)?.wait_ready(Duration::from_secs(600)).await?;
                }
            }
            match step {
                UpStep::Provision(p) => {
                    let script = self.root.join(&p.script);
                    output(format!("provision: {}\n", p.script.display()));
                    crate::scripting::run_script_file(self.clone(), &script, output.clone())
                        .await
                        .with_context(|| format!("provision {}", p.script.display()))?;
                }
                UpStep::Playbook(p) => {
                    // Unscoped playbooks apply to the machines this `up`
                    // actually started (like unscoped provisions run once
                    // everything targeted is up).
                    let machines: Vec<&String> = if scoped.is_empty() {
                        targets.iter().collect()
                    } else {
                        scoped.iter().collect()
                    };
                    for m in machines {
                        if scoped.is_empty() {
                            // Scoped machines were readiness-gated above;
                            // final-pass targets still need the same gate.
                            if let Some(c) = self.containers.get(m) {
                                c.wait_ready(Duration::from_secs(300)).await?;
                            } else {
                                self.vm(m)?.wait_ready(Duration::from_secs(600)).await?;
                            }
                        }
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
            Some(name.to_string()),
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
    pub async fn down(self: &Arc<Self>, subset: &[String], force: bool) -> Result<()> {
        let targets: Vec<String> = if subset.is_empty() {
            self.vms
                .keys()
                .chain(self.containers.keys())
                .cloned()
                .collect()
        } else {
            subset.to_vec()
        };
        let mut handles = Vec::new();
        for name in targets {
            if let Some(c) = self.containers.get(&name) {
                let c = c.clone();
                handles.push(tokio::spawn(async move { c.stop(force).await }));
                continue;
            }
            let vm = self.vm(&name)?.clone();
            handles.push(tokio::spawn(async move { vm.stop(force).await }));
        }
        for h in handles {
            h.await.map_err(|e| anyhow!("join: {e}"))??;
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
        // Wait for exit monitors to settle.
        for vm in self.vms.values() {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while vm.state().await != PowerState::Stopped {
                if tokio::time::Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        for c in self.containers.values() {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while c.state().await != PowerState::Stopped {
                if tokio::time::Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        // Removes clones, container overlays, AND named volumes — destroy is
        // the lab-scoped volume lifecycle boundary (PRD §12).
        if self.lab_local.exists() {
            std::fs::remove_dir_all(&self.lab_local)
                .with_context(|| format!("removing {}", self.lab_local.display()))?;
        }
        let run_dir = crate::paths::lab_runtime_dir(&self.name);
        let _ = std::fs::remove_dir_all(run_dir.join("vms"));
        let _ = std::fs::remove_dir_all(run_dir.join("containers"));
        Ok(())
    }

    /// Stop one VM and delete its clone and runtime state, leaving the rest of
    /// the lab running. The VM stays in the lab config, so a later `up <vm>`
    /// re-clones it from the template (per-VM analogue of [`destroy`]).
    pub async fn destroy_vm(self: &Arc<Self>, name: &str) -> Result<()> {
        let vm = self.vm(name)?.clone();
        vm.stop(true).await?;
        // Wait for the exit monitor to settle before removing its disks.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while vm.state().await != PowerState::Stopped {
            if tokio::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if vm.dirs.local.exists() {
            std::fs::remove_dir_all(&vm.dirs.local)
                .with_context(|| format!("removing {}", vm.dirs.local.display()))?;
        }
        let _ = std::fs::remove_dir_all(&vm.dirs.run);
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
        // Wait for the exit monitor to settle before removing state.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while container.state().await != PowerState::Stopped {
            if tokio::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if container.dirs.local.exists() {
            std::fs::remove_dir_all(&container.dirs.local)
                .with_context(|| format!("removing {}", container.dirs.local.display()))?;
        }
        let _ = std::fs::remove_dir_all(&container.dirs.run);
        {
            let mut state = self.state.lock().await;
            let c = state.container_mut(name);
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

    pub async fn status(&self) -> Value {
        // One cheap map lookup per machine: is its template/image download
        // still pending? Drives the web UI's "Download templates" button.
        let pending = self.pending_pulls.lock().await;
        let mut vms = Vec::new();
        for (name, vm) in &self.vms {
            let state = vm.state().await;
            let ready = vm.is_ready().await;
            let assigned_ips = if ready {
                vm.guest_ips()
                    .await
                    .unwrap_or_else(|_| vec![None; vm.cfg.nics.len()])
            } else {
                vec![None; vm.cfg.nics.len()]
            };
            let ip = assigned_ips.iter().flatten().next().cloned();
            // NICs in declaration order, paired with their resolved MACs — the
            // web UI groups machines by segment and shows MACs from this.
            let nics: Vec<Value> = vm
                .cfg
                .nics
                .iter()
                .enumerate()
                .map(|(i, nic)| {
                    json!({
                        "segment": nic.segment,
                        "mac": vm.macs.get(i).map(|m| m.to_string()),
                        "static_ip": nic.ip.map(|a| a.to_string()),
                        "ip": assigned_ips.get(i).and_then(Clone::clone),
                    })
                })
                .collect();
            // Declared web pages (no credentials — the browser only needs
            // name/port/path to build launch links; the proxy fetches auth
            // separately over the daemon socket).
            let web: Vec<Value> = vm
                .cfg
                .web
                .iter()
                .map(|w| json!({"name": w.name, "port": w.port, "path": w.path}))
                .collect();
            vms.push(json!({
                "name": name,
                "state": state,
                "ready": ready,
                "ip": ip,
                "template_cached": !pending.contains_key(name),
                "template": vm.cfg.template.to_string(),
                "arch": vm.cfg.arch,
                "cpus": vm.cfg.cpus,
                "memory": vm.cfg.memory,
                "nics": nics,
                "web": web,
                // The template carries a baked-in vmlab-agent (terminal
                // support); null on vintage guests and pre-agent templates.
                "agent_version": vm.template().agent_version,
            }));
        }
        let mut containers = Vec::new();
        for (name, c) in &self.containers {
            let state = c.state().await;
            let ready = c.is_ready().await;
            let assigned_ips = if ready {
                c.guest_ips()
                    .await
                    .unwrap_or_else(|_| vec![None; c.cfg.nics.len()])
            } else {
                vec![None; c.cfg.nics.len()]
            };
            let ip = assigned_ips.iter().flatten().next().cloned();
            let nics: Vec<Value> = c
                .cfg
                .nics
                .iter()
                .enumerate()
                .map(|(i, nic)| {
                    json!({
                        "segment": nic.segment,
                        "mac": c.macs.get(i).map(|m| m.to_string()),
                        "static_ip": nic.ip.map(|a| a.to_string()),
                        "ip": assigned_ips.get(i).and_then(Clone::clone),
                    })
                })
                .collect();
            let web: Vec<Value> = c
                .cfg
                .web
                .iter()
                .map(|w| json!({"name": w.name, "port": w.port, "path": w.path}))
                .collect();
            containers.push(json!({
                "name": name,
                "state": state,
                "ready": ready,
                "health": c.health().await,
                "ip": ip,
                "image_cached": !pending.contains_key(name),
                "image": c.cfg.image.reference,
                "digest": c.image_digest(),
                "restarts": c.restart_count(),
                "exit_code": c.last_exit().await,
                "nics": nics,
                "web": web,
            }));
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
            // null = not a global segment (no trunk possible) or supervisor
            // unreachable; bool = live trunk state keyed by segment name, so
            // the accept side (no local `connect`) lights up too.
            let peer_connected = if seg.global {
                trunk_states
                    .as_ref()
                    .map(|m| json!(m.get(&seg.name).copied().unwrap_or(false)))
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            segments.push(json!({
                "name": seg.name,
                "subnet": seg.subnet.to_string(),
                "gateway": seg.gateway_ip.to_string(),
                "nat": seg.nat,
                "dhcp": seg.dhcp,
                "global": seg.global,
                "connect": seg.peer,
                "peer_connected": peer_connected,
            }));
        }
        json!({
            "lab": self.name,
            "vms": vms,
            "containers": containers,
            "segments": segments,
        })
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
        if let Some(container) = self.containers.get(vm_name) {
            let online = container.snapshot(snap).await?;
            {
                let mut state = self.state.lock().await;
                state.container_mut(vm_name).snapshots.insert(
                    snap.to_string(),
                    SnapshotRecord {
                        online,
                        taken_at: chrono::Utc::now(),
                        image_digest: container.image_digest(),
                    },
                );
                state.save(&self.lab_local)?;
            }
            self.events.emit(
                "snapshot.created",
                json!({"vm": vm_name, "name": snap, "online": online}),
            );
            return Ok(online);
        }
        let vm = self.vm(vm_name)?;
        let online = vm.snapshot(snap).await?;
        {
            let mut state = self.state.lock().await;
            state.vm_mut(vm_name).snapshots.insert(
                snap.to_string(),
                SnapshotRecord {
                    online,
                    taken_at: chrono::Utc::now(),
                    image_digest: None,
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
            state.vm_mut(vm_name).snapshots.get(snap).cloned()
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
            state.container_mut(name).snapshots.get(snap).cloned()
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
            self.install_container_ports(name).await;
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
        if let Some(container) = self.containers.get(vm_name) {
            container.delete_snapshot(snap).await?;
            state.container_mut(vm_name).snapshots.remove(snap);
        } else {
            self.vm(vm_name)?.delete_snapshot(snap).await?;
            state.vm_mut(vm_name).snapshots.remove(snap);
        }
        state.save(&self.lab_local)?;
        Ok(())
    }

    pub async fn snapshots(&self, vm_name: &str) -> Result<Value> {
        let state = self.state.lock().await;
        let snaps = if self.containers.contains_key(vm_name) {
            state
                .containers
                .get(vm_name)
                .map(|c| c.snapshots.clone())
                .unwrap_or_default()
        } else {
            state
                .vms
                .get(vm_name)
                .map(|v| v.snapshots.clone())
                .unwrap_or_default()
        };
        Ok(json!(
            snaps
                .into_iter()
                .map(|(name, r)| json!({"name": name, "online": r.online, "taken_at": r.taken_at}))
                .collect::<Vec<_>>()
        ))
    }
}

/// Resolve a share's host path for smb.conf: `~` against $HOME, relative
/// paths against the lab root — smbd's cwd is not the lab's, so a literal
/// `./shared` would canonicalize to `/shared` and fail every tree connect.
fn resolve_share_host(root: &std::path::Path, host: &std::path::Path) -> PathBuf {
    if let Ok(rest) = host.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if host.is_relative() {
        return root.join(host);
    }
    host.to_path_buf()
}

/// Cross-host trunk state per global segment name, from the supervisor's
/// `global.list` (PRD §9.2). `None` = supervisor unreachable; used by
/// [`LabRuntime::status`] to report `peer_connected` per segment.
async fn fetch_global_peer_states() -> Option<std::collections::HashMap<String, bool>> {
    let client = crate::proto::client::Client::connect(&crate::paths::supervisor_socket())
        .await
        .ok()?;
    let list = client.call("global.list", Value::Null).await.ok()?;
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

/// Guess the guest OS family for SMB mount-command selection (PRD §7.5).
/// Heuristic from the resolved profile name; Windows profiles → Windows,
/// the legacy profile → XP-era, everything else → Linux.
fn guest_os_hint(profile: Option<&str>) -> crate::smb::OsHint {
    match profile {
        Some("windows-legacy") => crate::smb::OsHint::WindowsXp,
        Some(p) if p.starts_with("windows") => crate::smb::OsHint::Windows,
        _ => crate::smb::OsHint::Linux,
    }
}

/// One `up`-phase configuration step — provision scripts and playbook
/// applies share a single declaration-ordered queue.
enum UpStep {
    Provision(crate::config::model::Provision),
    Playbook(crate::config::model::Playbook),
}

impl UpStep {
    /// The machines this step is scoped to (empty = unscoped).
    fn vms(&self) -> &[String] {
        match self {
            UpStep::Provision(p) => &p.vms,
            UpStep::Playbook(p) => &p.vms,
        }
    }
}

/// Provisions and playbooks merged back into file declaration order — the
/// model keeps them in separate vecs; block byte spans recover the
/// interleaving.
fn merged_up_steps(lab: &crate::config::model::Lab) -> Vec<UpStep> {
    let mut steps: Vec<UpStep> = lab
        .provisions
        .iter()
        .cloned()
        .map(UpStep::Provision)
        .chain(lab.playbooks.iter().cloned().map(UpStep::Playbook))
        .collect();
    steps.sort_by_key(|s| match s {
        UpStep::Provision(p) => p.span.0,
        UpStep::Playbook(p) => p.span.0,
    });
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_steps_interleave_by_declaration_order() {
        let src = r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" }
  provision "scripts/one.ws" { vms = ["a"] }
  playbook "pb/base" { play = "base" vms = ["a"] }
  provision "scripts/two.ws" { }
}"#;
        let lf = crate::config::load_lab_source(src, "<test>", std::path::Path::new("/tmp"))
            .expect("parse");
        let steps = merged_up_steps(&lf.lab);
        let kinds: Vec<String> = steps
            .iter()
            .map(|s| match s {
                UpStep::Provision(p) => format!("provision:{}", p.script.display()),
                UpStep::Playbook(p) => format!("playbook:{}", p.path.display()),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "provision:scripts/one.ws",
                "playbook:pb/base",
                "provision:scripts/two.ws",
            ]
        );
    }
}
