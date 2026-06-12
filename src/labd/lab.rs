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
                    // Pulled on demand by `up` (PRD §6.4) — wired with the
                    // OCI module; for now require a prior explicit pull.
                    bail!(
                        "vm \"{}\": registry template {reference} is not in the local store — \
                         run `vmlab template pull` first",
                        vm_cfg.name
                    );
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

            let vm = VmInstance::new(
                &name,
                vm_cfg.clone(),
                resolved,
                dirs,
                macs,
                backing,
                disk_size,
                cdroms,
                floppy,
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
        }))
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
                handles.push(tokio::spawn(async move {
                    me.start_vm(&n).await?;
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

        self.events.emit("lab.up", json!({"vms": targets}));
        Ok(())
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
