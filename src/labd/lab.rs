//! Lab runtime: owns the VM instances, network fabric, persisted state, and
//! the lifecycle verbs (PRD §7). Lives inside the lab daemon.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::events::EventLog;
use super::network::{LabNetwork, nic_segment_name};
use super::state::{LabState, SnapshotRecord, generate_mac};
use super::vm::{PowerState, StopReason, VmDirs, VmInstance};
use crate::config::LabFile;
use crate::config::model::TemplateRef;
use crate::profiles::ProfileSet;
use crate::template::TemplateStore;

pub struct LabRuntime {
    pub name: String,
    pub root: PathBuf,
    pub lab_local: PathBuf,
    pub config: LabFile,
    pub vms: BTreeMap<String, Arc<VmInstance>>,
    pub network: Mutex<LabNetwork>,
    pub state: Mutex<LabState>,
    pub events: Arc<EventLog>,
    /// SMB server for the lab's shares (PRD §7.5); `None` until `up` starts
    /// it (only when some VM declares shares).
    pub smb: Mutex<Option<crate::smb::LabSmb>>,
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
                    // Templates live at `host/owner/[group/]name:VERSION` — the
                    // store name is the last repository path component. The tag
                    // may be a concrete version or a moving alias (`latest` /
                    // `latest-prerelease`): a concrete version already cached is
                    // used offline; otherwise resolve the alias to its real
                    // version (a cheap manifest fetch) and only pull the disk
                    // when that version isn't already in the store.
                    let arch = vm_cfg.arch.clone().ok_or_else(|| {
                        anyhow!(
                            "vm \"{}\": registry template needs an explicit arch",
                            vm_cfg.name
                        )
                    })?;
                    let registry = crate::oci::Registry::new(reference)?;
                    let store_name = registry
                        .repository()
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    let resolved = match store.resolve(&arch, &store_name, Some(registry.tag())) {
                        // Tag is a concrete, already-cached version: stay offline.
                        Ok(r) => r,
                        Err(_) => {
                            // Not cached. Resolve the requested tag against the
                            // registry's published tags first: a build-counter
                            // prefix (e.g. 26100.1742) resolves to the latest
                            // 26100.1742.<N>; a moving alias or exact tag is used
                            // as-is. Then resolve that to its real version without
                            // downloading the disk, and only pull when absent.
                            let pull_tag = match registry.list_tags().await {
                                Ok(tags) => crate::template::store::resolve_version_pin(
                                    &tags,
                                    registry.tag(),
                                )
                                .unwrap_or_else(|| registry.tag().to_string()),
                                Err(_) => registry.tag().to_string(),
                            };
                            let registry = if pull_tag.as_str() == registry.tag() {
                                registry
                            } else {
                                crate::oci::Registry::new(&crate::oci::with_version_tag(
                                    reference, &pull_tag,
                                )?)?
                            };
                            let real = registry
                                .resolve_version(Some(&arch))
                                .await
                                .with_context(|| format!("resolving {reference}"))?;
                            match store.resolve(&arch, &store_name, Some(&real)) {
                                // Resolved tag points at an already-cached version.
                                Ok(r) => r,
                                Err(_) => {
                                    let work = crate::paths::template_store_dir().join(".oci-pull");
                                    std::fs::create_dir_all(&work)?;
                                    let meta = registry
                                        .pull(Some(&arch), &store, &work, false)
                                        .await
                                        .with_context(|| format!("pulling {reference}"))?;
                                    let _ = std::fs::remove_dir_all(&work);
                                    store.resolve(&meta.arch, &meta.name, Some(&meta.version))?
                                }
                            }
                        }
                    };
                    (Some(resolved.disk_path.clone()), Some(resolved.meta), None)
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
            let vm = VmInstance::new(
                &name,
                vm_cfg.clone(),
                resolved,
                dirs,
                macs,
                nic_mtus,
                backing,
                disk_size,
                cdroms,
                floppy,
                first_boot_script,
            );
            vms.insert(vm_cfg.name.clone(), vm);
        }
        state.save(&lab_local)?;

        for vm_cfg in &config.lab.vms {
            for nic in &vm_cfg.nics {
                let seg_name = nic_segment_name(nic);
                if network.segment_mut(seg_name).is_none() {
                    bail!("nic references unknown segment {seg_name}");
                }
            }
        }

        // Phase 2: gateways with DHCP (reservations from persisted MACs),
        // DNS (auto-registration + statics + sinkholes) per segment.
        let host_cfg = crate::config::host::HostConfig::load_default()?;
        let macs_by_vm: std::collections::HashMap<String, Vec<crate::config::model::MacAddr>> =
            state
                .vms
                .iter()
                .map(|(n, v)| (n.clone(), v.macs.clone()))
                .collect();
        network.wire_gateways(&config.lab, &macs_by_vm, &host_cfg);

        Ok(Arc::new(LabRuntime {
            name,
            root,
            lab_local,
            config,
            vms,
            network: Mutex::new(network),
            state: Mutex::new(state),
            events,
            smb: Mutex::new(None),
        }))
    }

    /// Start the SMB server for the lab's shares and DNAT each relevant
    /// segment gateway's port 445 to it (PRD §7.5). Best-effort: a failure
    /// is logged and the rest of the lab still works. Called from `up`.
    async fn ensure_smb(self: &Arc<Self>, output: &crate::scripting::OutputSink) {
        // Collect sharing VMs with their gateway IP (first NIC's segment).
        let mut sharing: Vec<(String, std::net::Ipv4Addr, Vec<crate::config::model::Share>)> =
            Vec::new();
        let mut seg_ports: Vec<String> = Vec::new();
        {
            let net = self.network.lock().await;
            for vm in &self.config.lab.vms {
                if vm.shares.is_empty() {
                    continue;
                }
                let Some(nic) = vm.nics.first() else { continue };
                let seg_name = nic_segment_name(nic);
                let Some(seg) = net.segments.get(seg_name) else {
                    continue;
                };
                let mut shares = vm.shares.clone();
                for s in &mut shares {
                    s.host = resolve_share_host(&self.root, &s.host);
                }
                sharing.push((vm.name.clone(), seg.gateway_ip, shares));
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
                            ip: seg.gateway_ip,
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
        let smb = self.smb.lock().await;
        let Some(labsmb) = smb.as_ref() else { return };

        // Detect the guest OS family from the resolved profile (which folds
        // in template metadata — the lab vm block usually omits `profile`).
        let Ok(vm) = self.vm(vm_name) else { return };
        let os_hint = guest_os_hint(vm.resolved.profile.as_deref());
        let steps = labsmb.mount_plan(vm_name, os_hint);
        let Ok(qga) = vm.qga().await else {
            tracing::warn!("{vm_name}: no agent, cannot auto-mount shares");
            return;
        };
        for step in steps {
            let args: Vec<&str> = step.args.iter().map(String::as_str).collect();
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
                match qga
                    .exec(&step.command, &args, true, Duration::from_secs(30))
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
        self.vms
            .get(name)
            .ok_or_else(|| anyhow!("no vm \"{name}\" in lab \"{}\"", self.name))
    }

    /// Start one VM: wire its NIC sockets into the segment switches, then
    /// boot it with event-emitting callbacks.
    pub async fn start_vm(self: &Arc<Self>, name: &str) -> Result<()> {
        let vm = self.vm(name)?.clone();
        if vm.state().await != PowerState::Stopped {
            return Ok(());
        }
        self.events.emit("vm.starting", json!({"vm": name}));

        std::fs::create_dir_all(&vm.dirs.run)?;
        {
            let mut net = self.network.lock().await;
            for (i, nic) in vm.cfg.nics.iter().enumerate() {
                let sock = vm.dirs.nic_sock(i);
                let _ = std::fs::remove_file(&sock);
                let seg = net
                    .segment_mut(nic_segment_name(nic))
                    .ok_or_else(|| anyhow!("unknown segment for nic {i}"))?;
                seg.listen_nic(&sock, nic.isolated).await?;
            }
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

    /// `vmlab up [vm...]` (PRD §7.2, §10.4): start in depends_on waves and
    /// run provision scripts in declaration order. A dependency is
    /// satisfied when its VM is ready and the provisions scoped to it have
    /// completed.
    pub async fn up(
        self: &Arc<Self>,
        subset: &[String],
        output: crate::scripting::OutputSink,
    ) -> Result<()> {
        let targets: Vec<String> = if subset.is_empty() {
            self.config.lab.vms.iter().map(|v| v.name.clone()).collect()
        } else {
            for s in subset {
                self.vm(s)?;
            }
            // Pull in transitive dependencies of the subset.
            let mut wanted: HashSet<String> = HashSet::new();
            let mut stack: Vec<String> = subset.to_vec();
            while let Some(n) = stack.pop() {
                if wanted.insert(n.clone())
                    && let Some(cfg) = self.config.lab.vms.iter().find(|v| v.name == n)
                {
                    stack.extend(cfg.depends_on.iter().cloned());
                }
            }
            self.config
                .lab
                .vms
                .iter()
                .map(|v| v.name.clone())
                .filter(|n| wanted.contains(n))
                .collect()
        };

        // Start the SMB server before guests boot so shares are reachable
        // during provisioning (PRD §7.5).
        self.ensure_smb(&output).await;

        let mut remaining: Vec<String> = targets.clone();
        let mut done: HashSet<String> = HashSet::new();
        let mut next_provision = 0usize;
        while !remaining.is_empty() {
            // A wave: every remaining VM whose deps (within the target set)
            // are all done.
            let wave: Vec<String> = remaining
                .iter()
                .filter(|n| {
                    let cfg = self.config.lab.vms.iter().find(|v| &v.name == *n).unwrap();
                    cfg.depends_on
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
                    me.start_vm(&n).await?;
                    // Mount the VM's shares as soon as its agent answers —
                    // detached, so provisions can rely on them (§7.5)
                    // without the wave blocking on the mount retry window.
                    me.spawn_share_mount(&n);
                    // Run the template's first-boot provision before this VM
                    // can be considered ready (§6.1). A no-op for templates
                    // without one, so leaf-VM timing is unchanged.
                    me.run_first_boot(&n, &out).await?;
                    // Only gate the wave on readiness when something later
                    // depends on this VM.
                    let dependents = me.config.lab.vms.iter().any(|v| v.depends_on.contains(&n));
                    if dependents {
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
            // provision scoped entirely to already-started VMs, so a VM
            // depending on "dc01" starts only after dc01's provisions
            // completed (§7.2).
            self.run_provisions(&mut next_provision, &done, false, &output)
                .await?;
        }

        // Final pass: everything left, including unscoped scripts.
        self.run_provisions(&mut next_provision, &done, true, &output)
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

    /// Wire each segment's declared `forward {}` rules (PRD §9.8) once VMs
    /// have leases. Best-effort: a forward to a not-yet-ready VM is skipped.
    async fn install_declared_forwards(self: &Arc<Self>) {
        for seg in &self.config.lab.segments {
            for fwd in &seg.forwards {
                let Ok(vm) = self.vm(&fwd.vm) else { continue };
                let Ok(ip) = vm.guest_ip(None).await else {
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
                    let _ = services.add_forward(host_addr, guest_ip, fwd.guest_port, fwd.proto);
                }
            }
        }
    }

    /// Run provision scripts in strict declaration order starting at
    /// `*next`: a scoped script runs once all its VMs are started (waiting
    /// for their readiness first); an unscoped script runs only in the
    /// final pass. Stops at the first script that isn't eligible yet.
    async fn run_provisions(
        self: &Arc<Self>,
        next: &mut usize,
        started: &HashSet<String>,
        final_pass: bool,
        output: &crate::scripting::OutputSink,
    ) -> Result<()> {
        let provisions = self.config.lab.provisions.clone();
        while *next < provisions.len() {
            let p = &provisions[*next];
            let eligible = if p.vms.is_empty() {
                final_pass
            } else {
                p.vms.iter().all(|v| started.contains(v))
            };
            if !eligible {
                return Ok(());
            }
            for vm in &p.vms {
                self.vm(vm)?.wait_ready(Duration::from_secs(600)).await?;
            }
            let script = self.root.join(&p.script);
            output(format!("provision: {}\n", p.script.display()));
            crate::scripting::run_script_file(self.clone(), &script, output.clone())
                .await
                .with_context(|| format!("provision {}", p.script.display()))?;
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
            self.vms.keys().cloned().collect()
        } else {
            subset.to_vec()
        };
        let mut handles = Vec::new();
        for name in targets {
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
        if self.lab_local.exists() {
            std::fs::remove_dir_all(&self.lab_local)
                .with_context(|| format!("removing {}", self.lab_local.display()))?;
        }
        let run_dir = crate::paths::lab_runtime_dir(&self.name);
        let _ = std::fs::remove_dir_all(run_dir.join("vms"));
        Ok(())
    }

    pub async fn status(&self) -> Value {
        let mut vms = Vec::new();
        for (name, vm) in &self.vms {
            let state = vm.state().await;
            let ready = vm.is_ready().await;
            let ip = if ready {
                vm.guest_ip(None).await.ok()
            } else {
                None
            };
            vms.push(json!({
                "name": name,
                "state": state,
                "ready": ready,
                "ip": ip,
                "template": vm.cfg.template.to_string(),
            }));
        }
        let net = self.network.lock().await;
        let mut segments = Vec::new();
        for seg in net.segments.values() {
            segments.push(json!({
                "name": seg.name,
                "subnet": seg.subnet.to_string(),
                "gateway": seg.gateway_ip.to_string(),
                "nat": seg.nat,
                "dhcp": seg.dhcp,
            }));
        }
        json!({"lab": self.name, "vms": vms, "segments": segments})
    }

    // ---- snapshots (PRD §7.3) ----------------------------------------------

    pub async fn snapshot(&self, vm_name: &str, snap: &str) -> Result<bool> {
        let vm = self.vm(vm_name)?;
        let online = vm.snapshot(snap).await?;
        {
            let mut state = self.state.lock().await;
            state.vm_mut(vm_name).snapshots.insert(
                snap.to_string(),
                SnapshotRecord {
                    online,
                    taken_at: chrono::Utc::now(),
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

    /// Lab-wide snapshot: all VMs under one name; consistency across VMs is
    /// best-effort, not coordinated (PRD §7.3).
    pub async fn snapshot_all(&self, snap: &str) -> Result<Value> {
        let mut results = Vec::new();
        for name in self.vms.keys() {
            let online = self.snapshot(name, snap).await?;
            results.push(json!({"vm": name, "online": online}));
        }
        Ok(json!(results))
    }

    pub async fn restore(self: &Arc<Self>, vm_name: &str, snap: &str) -> Result<()> {
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

    pub async fn delete_snapshot(&self, vm_name: &str, snap: &str) -> Result<()> {
        let vm = self.vm(vm_name)?;
        vm.delete_snapshot(snap).await?;
        let mut state = self.state.lock().await;
        state.vm_mut(vm_name).snapshots.remove(snap);
        state.save(&self.lab_local)?;
        Ok(())
    }

    pub async fn snapshots(&self, vm_name: &str) -> Result<Value> {
        let state = self.state.lock().await;
        let snaps = state
            .vms
            .get(vm_name)
            .map(|v| v.snapshots.clone())
            .unwrap_or_default();
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
