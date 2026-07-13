//! Per-VM runtime: disk preparation, QEMU spawn, the §7.2 stop ladder,
//! readiness, and §7.3 snapshots.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

use crate::config::model::{self, MacAddr};
use crate::net::fastpath::NicAttachment;
use crate::qemu::{self, Proc, VmPaths};
use crate::qga::GaClient;
use crate::qmp::QmpClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

/// Why a VM left the Running state — carried on `vm.stopped` (PRD §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Requested,
    GuestInitiated,
    Crashed,
}

pub struct VmDirs {
    /// `.vmlab/vms/<vm>` — disks, OVMF VARS, TPM state.
    pub local: PathBuf,
    /// `$XDG_RUNTIME_DIR/vmlab/labs/<lab>/vms/<vm>` — sockets.
    pub run: PathBuf,
    /// `~/.local/state/vmlab/labs/<lab>/vms/<vm>` — logs.
    pub logs: PathBuf,
}

impl VmDirs {
    pub fn new(lab: &str, vm: &str, lab_local: &Path) -> Self {
        Self {
            local: lab_local.join("vms").join(vm),
            run: crate::paths::lab_runtime_dir(lab).join("vms").join(vm),
            logs: crate::paths::state_dir()
                .join("labs")
                .join(lab)
                .join("vms")
                .join(vm),
        }
    }

    pub fn qmp_sock(&self) -> PathBuf {
        self.run.join("qmp.sock")
    }
    pub fn qga_sock(&self) -> PathBuf {
        self.run.join("qga.sock")
    }
    /// vmlab-agent channel (`vmlab.agent.0`): terminals/exec/files/metrics.
    pub fn agent_sock(&self) -> PathBuf {
        self.run.join("agent.sock")
    }
    /// Host-side unix socket re-exposing one agent terminal session as a raw
    /// byte pipe (what `vmlab shell` and the web terminal attach to).
    pub fn term_session_sock(&self, id: u32) -> PathBuf {
        self.run.join(format!("term-{id}.sock"))
    }
    pub fn vnc_sock(&self) -> PathBuf {
        self.run.join("vnc.sock")
    }
    pub fn tpm_sock(&self) -> PathBuf {
        self.run.join("tpm.sock")
    }
    pub fn nic_sock(&self, i: usize) -> PathBuf {
        self.run.join(format!("nic{i}.sock"))
    }
    /// vhost-user socket of the i-th share's virtiofsd (§7.5).
    pub fn vfs_sock(&self, i: usize) -> PathBuf {
        self.run.join(format!("vfs{i}.sock"))
    }
    pub fn primary_disk(&self) -> PathBuf {
        self.local.join("disk0.qcow2")
    }
    /// Sentinel marking that the template's first-boot provision has completed
    /// for this clone. Written once first-boot succeeds; gates run-once so a
    /// second boot never waits on a marker that is not re-written (PRD §6.1).
    pub fn firstboot_sentinel(&self) -> PathBuf {
        self.local.join("firstboot.done")
    }
    pub fn extra_disk(&self, name: &str) -> PathBuf {
        self.local.join(format!("disk-{name}.qcow2"))
    }
    pub fn ovmf_vars(&self) -> PathBuf {
        self.local.join("OVMF_VARS.fd")
    }
    pub fn tpm_state(&self) -> PathBuf {
        self.local.join("tpm-state")
    }
}

/// The template-derived half of a VM: hardware resolution, backing disk, and
/// first-boot payload. Held behind a lock on [`VmInstance`] so a deferred
/// registry pull can bind the real parts after the daemon is already up —
/// `build()` installs a meta-less placeholder when the template isn't cached
/// yet, and `LabRuntime::ensure_pulled` swaps in the resolved parts.
pub struct TemplateParts {
    pub resolved: qemu::ResolvedVm,
    /// Backing template disk in the store (None for scratch / not yet pulled).
    pub backing: Option<PathBuf>,
    /// Primary disk virtual size (scratch: from config; clone: template's).
    pub disk_size: Option<u64>,
    /// Embedded first-boot provision script carried by the backing template
    /// (None for scratch / templates without one). Run on first instantiation,
    /// before the VM is reported ready (PRD §6.1).
    pub first_boot_script: Option<String>,
}

/// One share the guest mounts natively over virtiofs this run (§7.5) —
/// the guest agent runs `mount -t virtiofs <tag> <guest>` once ready.
#[derive(Debug, Clone)]
pub struct VirtiofsMount {
    pub tag: String,
    pub guest: String,
    pub readonly: bool,
}

pub struct VmInstance {
    pub lab: String,
    pub cfg: model::Vm,
    pub dirs: VmDirs,
    pub macs: Vec<MacAddr>,
    /// Effective MTU of each NIC's segment, in declaration order. Drives
    /// `host_mtu=` on virtio NICs so the guest matches a jumbo segment.
    pub nic_mtus: Vec<u16>,
    /// CD-ROM image paths (config cdrom + built media), resolved absolute.
    pub cdroms: Vec<PathBuf>,
    pub floppy: Option<PathBuf>,
    /// Absolute host dir per `cfg.shares` entry (relative paths resolved
    /// against the lab root by the lab builder).
    pub share_hosts: Vec<PathBuf>,
    /// See [`TemplateParts`] — std lock (never held across await).
    template: std::sync::RwLock<Arc<TemplateParts>>,

    /// Per-NIC segment attachments for the current run, set by `start_vm`
    /// before [`Self::start`]. Cleared in teardown — tap attachments are
    /// RAII, so clearing detaches them.
    nic_attachments: Mutex<Vec<NicAttachment>>,
    state: RwLock<PowerState>,
    /// The guest agent answers `guest-ping`. Set by the readiness poller.
    agent_up: RwLock<bool>,
    /// The VM is fully provisioned and usable: agent up AND the first-boot
    /// provision (if any) has completed. Gates dependents and provisions.
    ready: RwLock<bool>,
    stop_requested: RwLock<bool>,
    qemu: Mutex<Option<Arc<Proc>>>,
    swtpm: Mutex<Option<Arc<Proc>>>,
    /// Per-share virtiofsd daemons of the current run (§7.5). Killed on
    /// teardown; respawned by every start.
    virtiofsd: Mutex<Vec<Arc<Proc>>>,
    /// The shares this run attached over virtiofs, for the ready-time mount
    /// (see `LabRuntime::mount_shares`).
    virtiofs_mounts: Mutex<Vec<VirtiofsMount>>,
    qmp: Mutex<Option<QmpClient>>,
    qga: Mutex<Option<GaClient>>,
}

impl VmInstance {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        lab: &str,
        cfg: model::Vm,
        dirs: VmDirs,
        macs: Vec<MacAddr>,
        nic_mtus: Vec<u16>,
        cdroms: Vec<PathBuf>,
        floppy: Option<PathBuf>,
        share_hosts: Vec<PathBuf>,
        template: TemplateParts,
    ) -> Arc<Self> {
        Arc::new(Self {
            lab: lab.to_string(),
            cfg,
            dirs,
            macs,
            nic_mtus,
            cdroms,
            floppy,
            share_hosts,
            template: std::sync::RwLock::new(Arc::new(template)),
            nic_attachments: Mutex::new(Vec::new()),
            state: RwLock::new(PowerState::Stopped),
            agent_up: RwLock::new(false),
            ready: RwLock::new(false),
            stop_requested: RwLock::new(false),
            qemu: Mutex::new(None),
            swtpm: Mutex::new(None),
            virtiofsd: Mutex::new(Vec::new()),
            virtiofs_mounts: Mutex::new(Vec::new()),
            qmp: Mutex::new(None),
            qga: Mutex::new(None),
        })
    }

    /// Indices into `cfg.shares` that ride virtiofs (§7.5): explicit
    /// `transport = "virtiofs"` always (a missing host virtiofsd errors at
    /// start rather than silently degrading), `auto` when the host has a
    /// virtiofsd AND the resolved profile says the guest mounts it natively.
    /// `ensure_smb` uses the complement, so a share is served by exactly one
    /// transport.
    pub fn virtiofs_share_indices(&self) -> Vec<usize> {
        let host_has = crate::qemu::virtiofsd::available();
        let guest_ok = self.template().resolved.virtiofs;
        self.cfg
            .shares
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let vfs = match s.transport {
                    crate::config::model::ShareTransport::Smb => false,
                    crate::config::model::ShareTransport::Virtiofs => true,
                    crate::config::model::ShareTransport::Auto => host_has && guest_ok,
                };
                vfs.then_some(i)
            })
            .collect()
    }

    /// The shares the current run attached over virtiofs (empty when
    /// stopped or all-SMB).
    pub async fn virtiofs_mounts(&self) -> Vec<VirtiofsMount> {
        self.virtiofs_mounts.lock().await.clone()
    }

    /// The current template-derived parts (placeholder until a deferred
    /// registry pull binds the real ones).
    pub fn template(&self) -> Arc<TemplateParts> {
        self.template.read().expect("template lock").clone()
    }

    /// Bind the template parts resolved by a deferred pull.
    pub fn set_template(&self, parts: TemplateParts) {
        *self.template.write().expect("template lock") = Arc::new(parts);
    }

    /// Install this run's NIC attachments (wired by `start_vm` just before
    /// [`Self::start`]), one per configured NIC in declaration order.
    pub async fn set_nic_attachments(&self, attachments: Vec<NicAttachment>) {
        *self.nic_attachments.lock().await = attachments;
    }

    pub async fn state(&self) -> PowerState {
        *self.state.read().await
    }

    pub async fn is_ready(&self) -> bool {
        *self.ready.read().await
    }

    /// Whether the guest agent has answered at least once (PRD §2). This is a
    /// weaker signal than [`is_ready`]: it can be true while a first-boot
    /// provision is still running.
    pub async fn is_agent_up(&self) -> bool {
        *self.agent_up.read().await
    }

    /// Mark the VM fully ready. Called by the orchestration layer once the
    /// first-boot provision (if any) has completed.
    pub async fn mark_ready(&self) {
        *self.ready.write().await = true;
    }

    /// Whether a first-boot provision still needs to run for this clone: the
    /// template carries one and no completion sentinel exists yet.
    pub fn first_boot_pending(&self) -> bool {
        self.template().first_boot_script.is_some() && !self.dirs.firstboot_sentinel().exists()
    }

    pub async fn qmp(&self) -> Result<QmpClient> {
        self.qmp
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("{}: not running", self.cfg.name))
    }

    pub async fn qga(&self) -> Result<GaClient> {
        self.qga
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("{}: not running", self.cfg.name))
    }

    /// Create disks on first use (PRD §7.1): linked clone of the template,
    /// or a blank qcow2 for scratch; extra disks blank or FAT-from-folder.
    pub async fn ensure_disks(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dirs.local)?;
        let primary = self.dirs.primary_disk();
        if !primary.exists() {
            let t = self.template();
            match (&t.backing, t.disk_size) {
                (Some(backing), _) => {
                    crate::template::qimg::create_linked_clone(backing, &primary).await?;
                }
                (None, Some(size)) => {
                    crate::template::qimg::create_blank(&primary, size).await?;
                }
                (None, None) => bail!("{}: no backing template and no disk size", self.cfg.name),
            }
        }
        for d in &self.cfg.extra_disks {
            let path = self.dirs.extra_disk(&d.name);
            if path.exists() {
                continue;
            }
            match (&d.from, d.size) {
                (Some(_), _) => {
                    let folder = &d.from.as_ref().expect("checked");
                    fat_disk_from_folder(folder, &path, d.size).await?;
                }
                (None, Some(size)) => {
                    crate::template::qimg::create_blank(&path, size).await?;
                }
                (None, None) => bail!("disk \"{}\": no size and no source folder", d.name),
            }
        }
        Ok(())
    }

    fn all_disk_paths(&self) -> Vec<PathBuf> {
        let mut v = vec![self.dirs.primary_disk()];
        for d in &self.cfg.extra_disks {
            v.push(self.dirs.extra_disk(&d.name));
        }
        v
    }

    fn build_paths(
        &self,
        t: &TemplateParts,
        nics: Vec<qemu::NicSpec>,
        virtiofs_shares: Vec<(String, PathBuf)>,
    ) -> Result<VmPaths> {
        Ok(VmPaths {
            qmp_sock: self.dirs.qmp_sock(),
            qga_sock: self.dirs.qga_sock(),
            agent_sock: self.dirs.agent_sock(),
            vnc_sock: self.dirs.vnc_sock(),
            primary_disk: self.dirs.primary_disk(),
            extra_disks: self
                .cfg
                .extra_disks
                .iter()
                .map(|d| (d.name.clone(), self.dirs.extra_disk(&d.name)))
                .collect(),
            cdroms: self.cdroms.clone(),
            floppy: self.floppy.clone(),
            nics,
            ovmf_vars: (t.resolved.firmware == Some(crate::profiles::FirmwareKind::Ovmf))
                .then(|| self.dirs.ovmf_vars()),
            tpm_sock: t.resolved.tpm.then(|| self.dirs.tpm_sock()),
            serial_log: Some(self.dirs.logs.join("serial.log")),
            virtiofs_shares,
        })
    }

    /// Spawn one virtiofsd per virtiofs share (listening before QEMU
    /// starts) and return the (tag, socket) device list; also records the
    /// ready-time mount plan. Explicit `transport = "virtiofs"` with no
    /// host virtiofsd is a start error.
    async fn start_virtiofsds(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut procs = Vec::new();
        let mut devices = Vec::new();
        let mut mounts = Vec::new();
        for i in self.virtiofs_share_indices() {
            let share = &self.cfg.shares[i];
            if !crate::qemu::virtiofsd::available() {
                bail!(
                    "{}: share \"{}\" demands transport = \"virtiofs\" but no virtiofsd was \
                     found on this host (install one or set VMLAB_VIRTIOFSD)",
                    self.cfg.name,
                    share.name
                );
            }
            let host = self
                .share_hosts
                .get(i)
                .cloned()
                .unwrap_or_else(|| share.host.clone());
            let tag = crate::qemu::virtiofsd::mount_tag(&share.name);
            let sock = self.dirs.vfs_sock(i);
            let proc = crate::qemu::virtiofsd::spawn(
                &format!("{}/{}", self.cfg.name, share.name),
                &sock,
                &host,
                share.readonly,
                &self.dirs.logs.join(format!("virtiofsd{i}.log")),
            )
            .await?;
            procs.push(proc);
            devices.push((tag.clone(), sock));
            mounts.push(VirtiofsMount {
                tag,
                guest: share.guest.clone(),
                readonly: share.readonly,
            });
        }
        *self.virtiofsd.lock().await = procs;
        *self.virtiofs_mounts.lock().await = mounts;
        Ok(devices)
    }

    /// Per-NIC argv specs + the child fd mappings tap attachments need,
    /// derived from the attachments `start_vm` installed for this run.
    async fn nic_specs(&self) -> Result<(Vec<qemu::NicSpec>, Vec<qemu::process::ChildFd>)> {
        let attachments = self.nic_attachments.lock().await;
        if attachments.len() != self.macs.len() {
            bail!(
                "{}: {} nic attachment(s) wired for {} configured nic(s)",
                self.cfg.name,
                attachments.len(),
                self.macs.len()
            );
        }
        let mut specs = Vec::with_capacity(attachments.len());
        let mut fds = Vec::new();
        for (i, (mac, att)) in self.macs.iter().zip(attachments.iter()).enumerate() {
            let mtu = self.nic_mtus.get(i).copied();
            let backend = match att {
                NicAttachment::Stream { sock } => qemu::NicBackend::Stream { sock: sock.clone() },
                NicAttachment::Tap(tap) => {
                    // Fixed, collision-free child numbers past stdio.
                    let child_fd = 10 + i as i32;
                    fds.push(qemu::process::ChildFd {
                        parent: tap.qemu_fd().context("cloning tap fd for qemu")?,
                        child: child_fd,
                    });
                    qemu::NicBackend::Tap { child_fd }
                }
            };
            specs.push(qemu::NicSpec {
                mac: *mac,
                mtu,
                backend,
            });
        }
        Ok((specs, fds))
    }

    /// Spawn QEMU paused, connect QMP, then release the CPUs. The caller has
    /// already wired the NIC listener sockets on the segment switches.
    /// `on_exit` runs when the QEMU process ends (reason classified).
    pub async fn start(
        self: &Arc<Self>,
        on_exit: impl Fn(StopReason, String) + Send + Sync + 'static,
        on_ready: impl Fn() + Send + Sync + 'static,
    ) -> Result<()> {
        {
            let mut st = self.state.write().await;
            if *st != PowerState::Stopped {
                bail!("{} is {:?}", self.cfg.name, *st);
            }
            *st = PowerState::Starting;
        }
        *self.stop_requested.write().await = false;

        // Snapshot the template parts for the whole start sequence (a deferred
        // pull can't swap them mid-boot under us).
        let t = self.template();
        let run = async {
            std::fs::create_dir_all(&self.dirs.run)?;
            std::fs::create_dir_all(&self.dirs.logs)?;
            self.ensure_disks().await?;

            // Per-VM writable OVMF VARS from the firmware template.
            if t.resolved.firmware == Some(crate::profiles::FirmwareKind::Ovmf)
                && !self.dirs.ovmf_vars().exists()
            {
                let fw = match t.resolved.arch.as_str() {
                    "x86_64" => qemu::firmware::ovmf_x86_64(t.resolved.secure_boot)?,
                    "aarch64" => qemu::firmware::uefi_aarch64()?,
                    "riscv64" => qemu::firmware::uefi_riscv64()?,
                    a => bail!("no UEFI firmware for arch {a}"),
                };
                std::fs::copy(&fw.vars_template, self.dirs.ovmf_vars())
                    .context("copying OVMF VARS template")?;
            }

            if t.resolved.tpm {
                let swtpm = qemu::process::spawn_swtpm(
                    &self.cfg.name,
                    &self.dirs.tpm_state(),
                    &self.dirs.tpm_sock(),
                    &self.dirs.logs.join("swtpm.log"),
                )
                .await?;
                // Give swtpm a moment to bind its control socket.
                for _ in 0..50 {
                    if self.dirs.tpm_sock().exists() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                *self.swtpm.lock().await = Some(swtpm);
            }

            let accel = qemu::pick_accel(&t.resolved.arch);
            if accel == qemu::Accel::Tcg {
                tracing::warn!(
                    "{}: KVM unavailable for {} — falling back to TCG (slow)",
                    self.cfg.name,
                    t.resolved.arch
                );
            }
            // virtiofsd daemons must be listening before QEMU spawns (its
            // vhost-user chardevs connect at startup).
            let vfs_devices = self.start_virtiofsds().await?;

            let (nic_specs, nic_fds) = self.nic_specs().await?;
            let args = qemu::build_args(
                &self.lab,
                &t.resolved,
                &self.build_paths(&t, nic_specs, vfs_devices)?,
                accel,
            )?;
            let proc = Proc::spawn_with_fds(
                &format!("qemu:{}", self.cfg.name),
                &qemu::emulator_binary(&t.resolved.arch),
                &args,
                &self.dirs.logs.join("qemu.log"),
                nic_fds,
            )
            .await?;
            *self.qemu.lock().await = Some(proc.clone());

            // QMP comes up shortly after spawn (-S leaves CPUs paused).
            let qmp = connect_qmp_retry(&self.dirs.qmp_sock(), &proc).await?;

            // Track guest-initiated shutdowns via the QMP SHUTDOWN event.
            let mut qmp_events = qmp.subscribe_events();
            let guest_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = guest_shutdown.clone();
            tokio::spawn(async move {
                while let Ok(ev) = qmp_events.recv().await {
                    if ev.event == "SHUTDOWN" {
                        let initiator = ev.data.get("reason").and_then(|r| r.as_str());
                        if initiator == Some("guest-shutdown") || initiator == Some("guest-reset") {
                            flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                }
            });

            qmp.cont().await?;
            *self.qmp.lock().await = Some(qmp);
            *self.qga.lock().await = Some(GaClient::connect(&self.dirs.qga_sock()).await?);

            Ok::<_, anyhow::Error>((proc, guest_shutdown))
        };

        let (proc, guest_shutdown) = match run.await {
            Ok(v) => v,
            Err(e) => {
                *self.state.write().await = PowerState::Stopped;
                self.teardown().await;
                return Err(e);
            }
        };

        *self.state.write().await = PowerState::Running;

        // Exit monitor: classify why QEMU ended (PRD §8.1 stop reasons).
        let me = self.clone();
        tokio::spawn(async move {
            let status = proc
                .wait_exit(Duration::from_secs(60 * 60 * 24 * 365))
                .await
                .unwrap_or_else(|_| "unknown".to_string());
            let requested = *me.stop_requested.read().await;
            let guest = guest_shutdown.load(std::sync::atomic::Ordering::SeqCst);
            let clean = status.contains("exit status: 0");
            let reason = if requested {
                StopReason::Requested
            } else if guest && clean {
                StopReason::GuestInitiated
            } else if clean {
                StopReason::Requested
            } else {
                StopReason::Crashed
            };
            me.teardown().await;
            *me.state.write().await = PowerState::Stopped;
            *me.agent_up.write().await = false;
            *me.ready.write().await = false;
            on_exit(reason, status);
        });

        // Readiness poller: the guest agent answering `guest-ping` makes the VM
        // "agent up" (PRD §2, §7.4). When the template has no pending first-boot
        // provision, agent-up is also full readiness, so set both and fire
        // on_ready. Otherwise leave `ready` for the orchestration layer to flip
        // once the first-boot provision completes.
        let me = self.clone();
        tokio::spawn(async move {
            let defer_ready = me.first_boot_pending();
            loop {
                if me.state().await != PowerState::Running {
                    return;
                }
                let qga = { me.qga.lock().await.clone() };
                if let Some(qga) = qga
                    && qga.ping(Duration::from_secs(2)).await
                {
                    *me.agent_up.write().await = true;
                    if !defer_ready {
                        *me.ready.write().await = true;
                        on_ready();
                    }
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        Ok(())
    }

    async fn teardown(&self) {
        if let Some(tpm) = self.swtpm.lock().await.take() {
            tpm.kill().await;
        }
        // virtiofsd usually exits on its own once QEMU disconnects; kill
        // covers daemons that never got a connection (failed start).
        for proc in self.virtiofsd.lock().await.drain(..) {
            if proc.is_running() {
                proc.kill().await;
            }
        }
        self.virtiofs_mounts.lock().await.clear();
        // RAII: dropping tap attachments detaches their switch ports and
        // XDP state; with QEMU gone, the kernel then destroys the taps.
        self.nic_attachments.lock().await.clear();
        *self.qmp.lock().await = None;
        *self.qga.lock().await = None;
        *self.qemu.lock().await = None;
    }

    /// Graceful stop ladder (PRD §7.2): guest-agent shutdown → ACPI
    /// powerdown → hard kill, each with a timeout.
    pub async fn stop(&self, force: bool) -> Result<()> {
        let proc = { self.qemu.lock().await.clone() };
        let Some(proc) = proc else {
            return Ok(()); // already stopped
        };
        *self.stop_requested.write().await = true;
        *self.state.write().await = PowerState::Stopping;

        if force {
            proc.kill().await;
            let _ = proc.wait_exit(Duration::from_secs(10)).await;
            return self
                .wait_state(PowerState::Stopped, Duration::from_secs(10))
                .await;
        }

        // Rung 1: guest agent shutdown.
        if self.is_agent_up().await
            && let Ok(qga) = self.qga().await
        {
            let _ = qga.shutdown("powerdown", Duration::from_secs(5)).await;
            if proc.wait_exit(Duration::from_secs(30)).await.is_ok() {
                return self
                    .wait_state(PowerState::Stopped, Duration::from_secs(10))
                    .await;
            }
        }

        // Rung 2: ACPI powerdown via QMP.
        if let Ok(qmp) = self.qmp().await {
            let _ = qmp.system_powerdown().await;
            if proc.wait_exit(Duration::from_secs(30)).await.is_ok() {
                return self
                    .wait_state(PowerState::Stopped, Duration::from_secs(10))
                    .await;
            }
        }

        // Rung 3: hard kill.
        tracing::warn!("{}: graceful stop timed out, killing", self.cfg.name);
        proc.kill().await;
        let _ = proc.wait_exit(Duration::from_secs(10)).await;
        self.wait_state(PowerState::Stopped, Duration::from_secs(10))
            .await
    }

    /// Wait for the exit monitor to settle the power state.
    pub async fn wait_state(&self, want: PowerState, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.state().await != want {
            if tokio::time::Instant::now() > deadline {
                bail!(
                    "{}: still {:?} after {timeout:?}",
                    self.cfg.name,
                    self.state().await
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    /// Wait until the VM is fully ready (PRD §10.3 wait_ready): agent up and
    /// any first-boot provision complete.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.is_ready().await {
                return Ok(());
            }
            if self.state().await == PowerState::Stopped {
                bail!("{} stopped while waiting for ready", self.cfg.name);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("{}: not ready after {timeout:?}", self.cfg.name);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Wait until the guest agent first responds, ahead of the first-boot
    /// provision. Weaker than [`wait_ready`].
    pub async fn wait_agent_up(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.is_agent_up().await {
                return Ok(());
            }
            if self.state().await == PowerState::Stopped {
                bail!("{} stopped while waiting for agent", self.cfg.name);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("{}: agent not up after {timeout:?}", self.cfg.name);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    // ---- snapshots (PRD §7.3) ---------------------------------------------

    /// Take a snapshot; returns whether it was online (running) or offline.
    pub async fn snapshot(&self, name: &str) -> Result<bool> {
        validate_snapshot_name(name)?;
        match self.state().await {
            PowerState::Running => {
                let qmp = self.qmp().await?;
                let nodes = disk_nodes(self.all_disk_paths().len());
                let refs: Vec<&str> = nodes.iter().map(String::as_str).collect();
                qmp.snapshot_save(name, "disk0", &refs).await?;
                Ok(true)
            }
            PowerState::Stopped => {
                for disk in self.all_disk_paths() {
                    crate::template::qimg::snapshot_create(&disk, name).await?;
                }
                Ok(false)
            }
            other => bail!("{} is {:?} — wait for it to settle", self.cfg.name, other),
        }
    }

    /// Restore must do the right thing (PRD §7.3): online snapshots resume
    /// running exactly where they were; offline snapshots leave the VM off.
    /// `was_online` comes from the recorded power state at capture.
    pub async fn restore(
        self: &Arc<Self>,
        name: &str,
        was_online: bool,
        on_exit: impl Fn(StopReason, String) + Send + Sync + 'static,
        on_ready: impl Fn() + Send + Sync + 'static,
    ) -> Result<()> {
        if was_online {
            // Ensure a running QEMU to load into.
            if self.state().await == PowerState::Stopped {
                self.start(on_exit, on_ready).await?;
            }
            let qmp = self.qmp().await?;
            qmp.stop().await?;
            let nodes = disk_nodes(self.all_disk_paths().len());
            let refs: Vec<&str> = nodes.iter().map(String::as_str).collect();
            qmp.snapshot_load(name, "disk0", &refs).await?;
            // Drop the agent connection BEFORE resuming: the rewound guest
            // replays virtio-serial response bytes the host already
            // consumed, and qga responses carry no request ids — a stale
            // `{"return":…}` silently poisons a later exchange. With no
            // client attached QEMU discards the replayed bytes; reconnect
            // starts clean (best-effort: agentless guests have no channel
            // to poison).
            self.qga.lock().await.take();
            qmp.cont().await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let Ok(c) = GaClient::connect(&self.dirs.qga_sock()).await {
                *self.qga.lock().await = Some(c);
            }
            Ok(())
        } else {
            // Offline: power off if needed, apply, stay off.
            if self.state().await != PowerState::Stopped {
                self.stop(false).await?;
                // Wait for the exit monitor to settle the state.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
                while self.state().await != PowerState::Stopped {
                    if tokio::time::Instant::now() > deadline {
                        bail!("{} did not stop for restore", self.cfg.name);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            for disk in self.all_disk_paths() {
                crate::template::qimg::snapshot_apply(&disk, name).await?;
            }
            Ok(())
        }
    }

    pub async fn delete_snapshot(&self, name: &str) -> Result<()> {
        match self.state().await {
            PowerState::Running => {
                let qmp = self.qmp().await?;
                let nodes = disk_nodes(self.all_disk_paths().len());
                let refs: Vec<&str> = nodes.iter().map(String::as_str).collect();
                qmp.snapshot_delete(name, &refs).await?;
            }
            _ => {
                for disk in self.all_disk_paths() {
                    crate::template::qimg::snapshot_delete(&disk, name).await?;
                }
            }
        }
        Ok(())
    }

    /// Per-NIC IPv4 addresses reported by the guest agent, matched to the
    /// configured NIC order by resolved MAC address in one QGA request.
    pub async fn guest_ips(&self) -> Result<Vec<Option<String>>> {
        let qga = self.qga().await?;
        let ifaces = qga.network_interfaces(Duration::from_secs(5)).await?;
        let macs: Vec<String> = self.macs.iter().map(ToString::to_string).collect();
        Ok(crate::qga::ipv4_by_mac(&ifaces, &macs))
    }

    /// First IPv4 address, or the address of a specific NIC (PRD §10.3).
    pub async fn guest_ip(&self, nic: Option<usize>) -> Result<String> {
        let ips = self.guest_ips().await?;
        let ip = match nic {
            Some(index) => ips.get(index).and_then(Clone::clone),
            None => ips.into_iter().flatten().next(),
        };
        ip.ok_or_else(|| anyhow::anyhow!("{}: no IPv4 address reported by agent", self.cfg.name))
    }
}

fn disk_nodes(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("disk{i}")).collect()
}

pub(crate) fn validate_snapshot_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("invalid snapshot name `{name}` (alphanumeric, '-', '_', '.')");
    }
    Ok(())
}

async fn connect_qmp_retry(sock: &Path, proc: &Arc<Proc>) -> Result<QmpClient> {
    for _ in 0..100 {
        if !proc.is_running() {
            bail!(
                "QEMU exited during startup: {}",
                proc.exit_status().unwrap_or_default()
            );
        }
        match QmpClient::connect(sock).await {
            Ok(c) => return Ok(c),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    bail!("QMP socket {} never came up", sock.display())
}

/// Build a FAT-formatted qcow2 disk pre-populated from a folder (PRD §5.2).
async fn fat_disk_from_folder(folder: &Path, dest: &Path, size: Option<u64>) -> Result<()> {
    let content: u64 = walk_size(folder)?;
    // FAT32 floor is ~33 MiB; add slack for tables.
    let bytes = size.unwrap_or(0).max(content * 2).max(64 << 20);
    let tmp = dest.with_extension("raw.tmp");
    let _ = std::fs::remove_file(&tmp);

    let kb = bytes.div_ceil(1024);
    run_tool(
        "mkfs.vfat",
        &["-C".into(), tmp.display().to_string(), kb.to_string()],
    )
    .await?;
    let mut entries: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        entries.push(entry?.path().display().to_string());
    }
    if !entries.is_empty() {
        let mut args = vec![
            "-i".to_string(),
            tmp.display().to_string(),
            "-s".to_string(),
        ];
        args.extend(entries);
        args.push("::/".into());
        run_tool("mcopy", &args).await?;
    }
    crate::template::qimg::convert_to_qcow2(&tmp, dest).await?;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

fn walk_size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let md = entry.metadata()?;
        total += if md.is_dir() {
            walk_size(&entry.path())?
        } else {
            md.len()
        };
    }
    Ok(total)
}

async fn run_tool(bin: &str, args: &[String]) -> Result<()> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {bin}"))?;
    if !out.status.success() {
        bail!("{bin} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}
