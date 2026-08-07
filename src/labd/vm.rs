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
use crate::qemu::{self, VmPaths};
use crate::qmp::QmpClient;
use crate::smb::VirtiofsMount;

use super::hypervisor::{Control, Hypervisor, Process};

/// How long a VM gets to become ready: it may be running a template first-boot
/// provision through a Windows specialize/OOBE pass and a settle reboot.
pub const VM_READY_TIMEOUT: Duration = Duration::from_secs(600);

/// What to tell a user whose guest has no answering vmlab-agent.
const NO_AGENT_HINT: &str = "the guest has no running vmlab-agent (the template likely predates \
     agent support) — rebuild it with `vmlab template build`";

/// A machine's power state. Declared with the status projection, which is what
/// puts it on the wire (ADR-0004), and re-exported here because this is where
/// the daemon's callers reach for it.
pub use crate::status::PowerState;

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
    /// Embedded first-boot provision and its host-surface contract. `None` for
    /// scratch / templates without one. Run on first instantiation before the
    /// VM is reported ready (PRD §6.1).
    pub first_boot: Option<crate::scripting::EmbeddedWscript>,
    /// vmlab-agent stamp the template build baked in (`None` = the template
    /// predates agent support — no terminal, exec, copy or readiness).
    pub agent_version: Option<String>,
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
    qemu: Mutex<Option<Arc<dyn Process>>>,
    swtpm: Mutex<Option<Arc<dyn Process>>>,
    /// Per-share virtiofsd daemons of the current run (§7.5). Killed on
    /// teardown; respawned by every start.
    virtiofsd: Mutex<Vec<Arc<dyn Process>>>,
    /// The shares this run attached over virtiofs, for the ready-time mount
    /// (see `LabRuntime::mount_shares`).
    virtiofs_mounts: Mutex<Vec<VirtiofsMount>>,
    /// The running machine's control channel (see [`Control`]); `None` while
    /// stopped.
    control: Mutex<Option<Arc<dyn Control>>>,
    /// Lazy vmlab-agent connection (terminals/exec/files — §"agent channel").
    agent: super::machine::AgentSlot,
    /// How this machine reaches the host to actually run (see
    /// [`super::hypervisor`]). Real QEMU in production; a fake in tests, which
    /// is what makes the start ladder and the exit monitor testable.
    hv: Arc<dyn Hypervisor>,
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
            control: Mutex::new(None),
            agent: super::machine::AgentSlot::default(),
            hv: Arc::new(super::hypervisor::Qemu),
        })
    }

    /// Run this VM against a different hypervisor — the injection point
    /// ADR-0001 introduced so the start ladder and the exit monitor can be
    /// driven without KVM. `labd::lifecycle_tests` drives both kinds through
    /// it; orchestration is driven a level up, against whole-machine doubles
    /// (see `labd::lab`'s tests).
    #[cfg(test)]
    pub(crate) fn set_hypervisor(self: &mut Arc<Self>, hv: Arc<dyn Hypervisor>) {
        Arc::get_mut(self).expect("sole owner").hv = hv;
    }

    /// Indices into `cfg.shares` that ride virtiofs (§7.5).
    ///
    /// Both halves of the decision are shared with the share plan: the rule
    /// is [`crate::labd::share_plan::transport_of`], and the host fact comes
    /// from this machine's hypervisor, which is what the plan asks too. So
    /// the plan's complement really is what `smbd` exports, and a share is
    /// served by exactly one transport — under a substituted host as much as
    /// a real one.
    pub fn virtiofs_share_indices(&self) -> Vec<usize> {
        use crate::labd::share_plan::{Transport, transport_of};
        let host_has = self.hv.virtiofsd_available();
        let guest_ok = self.template().resolved.virtiofs;
        self.cfg
            .shares
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                (transport_of(s.transport, host_has, guest_ok) == Transport::Virtiofs).then_some(i)
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

    async fn power_state(&self) -> PowerState {
        *self.state.read().await
    }

    async fn ready_flag(&self) -> bool {
        *self.ready.read().await
    }

    /// Whether the guest agent has answered at least once (PRD §2). This is a
    /// weaker signal than [`is_ready`]: it can be true while a first-boot
    /// provision is still running.
    async fn agent_up_flag(&self) -> bool {
        *self.agent_up.read().await
    }

    /// Whether the guest agent answers a ping right now. Unlike the sticky
    /// [`is_agent_up`] flag, this goes false while the guest is down or
    /// mid-reboot — what a first-boot provision needs to watch its own guest
    /// restart (QEMU stays up, so power state never changes).
    async fn agent_is_answering(&self) -> bool {
        self.agent_probe().await
    }

    /// Mark the VM fully ready. Called by the orchestration layer once the
    /// first-boot provision (if any) has completed.
    pub async fn mark_ready(&self) {
        *self.ready.write().await = true;
    }

    /// Whether a first-boot provision still needs to run for this clone: the
    /// template carries one and no completion sentinel exists yet.
    pub fn first_boot_pending(&self) -> bool {
        self.template().first_boot.is_some() && !self.dirs.firstboot_sentinel().exists()
    }

    /// The live QMP client, for the operations that genuinely need a
    /// hypervisor — snapshots, the framebuffer, scripted input. Absent both
    /// when the VM is stopped and when it is running under an in-memory
    /// adapter, which is why those stay verified against a running lab
    /// (ADR-0001).
    pub async fn qmp(&self) -> Result<QmpClient> {
        self.control
            .lock()
            .await
            .as_ref()
            .and_then(|c| c.qmp())
            .ok_or_else(|| anyhow!("{}: not running", self.cfg.name))
    }

    /// The vmlab-agent channel, connecting (with the token handshake) on
    /// first use. A failed handshake is remembered briefly so agent-less
    /// guests don't pay the timeout on every exec that would prefer the
    /// agent transport.
    async fn agent_handle(&self) -> Result<super::vm_agent::AgentHandle> {
        if self.power_state().await != PowerState::Running {
            return Err(super::machine::AgentUnavailable::NotRunning(self.cfg.name.clone()).into());
        }
        if !self.template().resolved.agent_channel {
            return Err(super::machine::AgentUnavailable::NoChannel(self.cfg.name.clone()).into());
        }
        self.agent
            .connect(&self.cfg.name, &self.dirs.agent_sock(), NO_AGENT_HINT)
            .await
    }

    /// Whether the vmlab-agent answers right now, sharing (and populating)
    /// the cached handle.
    async fn agent_probe(&self) -> bool {
        if self.power_state().await != PowerState::Running
            || !self.template().resolved.agent_channel
        {
            return false;
        }
        self.agent.probe(&self.dirs.agent_sock()).await
    }

    /// Drop the cached agent connection (teardown, snapshot restore).
    pub async fn drop_agent(&self) {
        self.agent.drop().await;
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

    /// The host's UEFI CODE/VARS pair for this VM, or `None` under SeaBIOS.
    /// Firmware discovery probes the host filesystem, so it happens here —
    /// where the runtime paths are assembled — rather than inside the argv
    /// builder, which is a pure function of what it is handed (ADR-0008).
    fn uefi_firmware(&self, t: &TemplateParts) -> Result<Option<qemu::firmware::UefiFirmware>> {
        if t.resolved.firmware != Some(crate::profiles::FirmwareKind::Ovmf) {
            return Ok(None);
        }
        let arch = qemu::qemu_arch(&t.resolved.arch);
        Ok(Some(qemu::firmware::lookup(arch, t.resolved.secure_boot)?))
    }

    fn build_paths(
        &self,
        t: &TemplateParts,
        firmware: Option<&qemu::firmware::UefiFirmware>,
        nics: Vec<qemu::NicSpec>,
        virtiofs_shares: Vec<(String, PathBuf)>,
    ) -> Result<VmPaths> {
        Ok(VmPaths {
            qmp_sock: self.dirs.qmp_sock(),
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
            firmware_code: firmware.map(|fw| fw.code.clone()),
            ovmf_vars: firmware.map(|_| self.dirs.ovmf_vars()),
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
            if !self.hv.virtiofsd_available() {
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
            let proc = self
                .hv
                .start_virtiofsd(
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
    ///
    /// The callback-level entry point [`Machine::start`](super::machine::Machine::start)
    /// wraps: it turns these callbacks into the lab's lifecycle events and
    /// wires the NICs first. `labd::lifecycle_tests` drives this directly,
    /// because what it asserts on — the classified exit reason, how many times
    /// readiness fired, every healthcheck transition — is exactly what the
    /// callbacks carry and the event projection flattens. Not a second route
    /// for consumers: `pub(super)`, and nothing outside this module tree can
    /// reach it (ADR-0002).
    pub(super) async fn boot(
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
            let firmware = self.uefi_firmware(&t)?;
            if let Some(fw) = &firmware
                && !self.dirs.ovmf_vars().exists()
            {
                std::fs::copy(&fw.vars_template, self.dirs.ovmf_vars())
                    .context("copying OVMF VARS template")?;
            }

            if t.resolved.tpm {
                let swtpm = self
                    .hv
                    .start_tpm(
                        &self.cfg.name,
                        &self.dirs.tpm_state(),
                        &self.dirs.tpm_sock(),
                        &self.dirs.logs.join("swtpm.log"),
                    )
                    .await?;
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
                &self.build_paths(&t, firmware.as_ref(), nic_specs, vfs_devices)?,
                accel,
            )?;
            // The machine answers control shortly after spawn (-S leaves CPUs
            // paused); the hypervisor returns once it does.
            let super::hypervisor::Running { proc, control } = self
                .hv
                .start_emulator(super::hypervisor::LaunchSpec {
                    label: format!("qemu:{}", self.cfg.name),
                    binary: qemu::emulator_binary(&t.resolved.arch),
                    args,
                    log: self.dirs.logs.join("qemu.log"),
                    qmp_sock: self.dirs.qmp_sock(),
                    fds: nic_fds,
                    channels: super::hypervisor::GuestChannels {
                        agent: self.dirs.agent_sock(),
                        ctl: None,
                    },
                })
                .await?;
            *self.qemu.lock().await = Some(proc.clone());

            control.resume().await?;
            *self.control.lock().await = Some(control.clone());

            Ok::<_, anyhow::Error>((proc, control))
        };

        let (proc, control) = match run.await {
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
            let reason = super::hypervisor::classify_exit(
                *me.stop_requested.read().await,
                control.guest_shutdown(),
                &status,
            );
            me.teardown().await;
            *me.state.write().await = PowerState::Stopped;
            *me.agent_up.write().await = false;
            *me.ready.write().await = false;
            on_exit(reason, status);
        });

        // Readiness poller: the vmlab-agent answering its handshake makes the
        // VM "agent up" (PRD §2, §7.4). When the template has no pending
        // first-boot provision, agent-up is also full readiness, so set both
        // and fire on_ready. Otherwise leave `ready` for the orchestration
        // layer to flip once the first-boot provision completes.
        let me = self.clone();
        tokio::spawn(async move {
            let defer_ready = me.first_boot_pending();
            loop {
                if me.power_state().await != PowerState::Running {
                    return;
                }
                if me.agent_probe().await {
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
        *self.control.lock().await = None;
        self.drop_agent().await;
        *self.qemu.lock().await = None;
    }

    /// Graceful stop ladder (PRD §7.2): guest-agent shutdown → ACPI
    /// powerdown → hard kill, each with a timeout.
    async fn stop_ladder(&self, force: bool) -> Result<()> {
        let proc = { self.qemu.lock().await.clone() };
        let Some(proc) = proc else {
            return Ok(()); // already stopped
        };
        *self.stop_requested.write().await = true;
        *self.state.write().await = PowerState::Stopping;

        if force {
            proc.kill().await;
            let _ = proc.wait_exit(Duration::from_secs(10)).await;
            return self.settle_stopped(Duration::from_secs(10)).await;
        }

        // Rung 1: guest agent shutdown. The state is already Stopping so
        // `agent()`'s Running gate can't be used: take the cached handle, or
        // make one quick connect attempt on guests with an agent channel.
        if self.agent_up_flag().await {
            let agent = self.agent.cached().await;
            let agent = match agent {
                Some(a) => Some(a),
                None if self.template().resolved.agent_channel => {
                    super::vm_agent::AgentHandle::connect(
                        &self.dirs.agent_sock(),
                        Duration::from_secs(2),
                    )
                    .await
                    .ok()
                }
                None => None,
            };
            if let Some(agent) = agent
                && agent
                    .shutdown_guest(
                        super::vm_agent::ShutdownMode::Powerdown,
                        Duration::from_secs(5),
                    )
                    .await
                    .is_ok()
                && proc.wait_exit(Duration::from_secs(30)).await.is_ok()
            {
                return self.settle_stopped(Duration::from_secs(10)).await;
            }
        }

        // Rung 2: ACPI powerdown. Delivery succeeding says nothing about the
        // guest acting on it — a guest with no ACPI daemon, or one sitting at
        // a "really shut down?" dialog, ignores it entirely.
        if let Some(control) = self.control.lock().await.clone() {
            let _ = control.powerdown().await;
            if proc.wait_exit(Duration::from_secs(30)).await.is_ok() {
                return self.settle_stopped(Duration::from_secs(10)).await;
            }
        }

        // Rung 3: hard kill.
        tracing::warn!("{}: graceful stop timed out, killing", self.cfg.name);
        proc.kill().await;
        let _ = proc.wait_exit(Duration::from_secs(10)).await;
        self.settle_stopped(Duration::from_secs(10)).await
    }

    /// Wait for the exit monitor to settle the power state — the interface's
    /// one implementation, reached explicitly because the inherent method that
    /// used to shadow it is gone.
    async fn settle_stopped(&self, timeout: Duration) -> Result<()> {
        super::machine::Machine::wait_state(self, PowerState::Stopped, timeout).await
    }

    // ---- snapshots (PRD §7.3) ---------------------------------------------

    /// Take a snapshot; returns whether it was online (running) or offline.
    async fn take_snapshot(&self, name: &str) -> Result<bool> {
        validate_snapshot_name(name)?;
        match self.power_state().await {
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
    async fn load_snapshot(
        self: &Arc<Self>,
        name: &str,
        was_online: bool,
        on_exit: impl Fn(StopReason, String) + Send + Sync + 'static,
        on_ready: impl Fn() + Send + Sync + 'static,
    ) -> Result<()> {
        if was_online {
            // Ensure a running QEMU to load into.
            if self.power_state().await == PowerState::Stopped {
                self.boot(on_exit, on_ready).await?;
            }
            let qmp = self.qmp().await?;
            qmp.stop().await?;
            let nodes = disk_nodes(self.all_disk_paths().len());
            let refs: Vec<&str> = nodes.iter().map(String::as_str).collect();
            qmp.snapshot_load(name, "disk0", &refs).await?;
            // Drop the agent connection BEFORE resuming: the rewound guest
            // replays virtio-serial response bytes the host already
            // consumed. With no client attached QEMU discards the replayed
            // bytes; the lazy reconnect on next use re-handshakes with a
            // fresh token, and the frame magic resyncs any mid-frame
            // garbage — see guest/agent-proto.
            self.drop_agent().await;
            qmp.cont().await?;
            Ok(())
        } else {
            // Offline: power off if needed, apply, stay off.
            if self.power_state().await != PowerState::Stopped {
                self.stop_ladder(false).await?;
                self.settle_stopped(Duration::from_secs(60))
                    .await
                    .with_context(|| format!("{} did not stop for restore", self.cfg.name))?;
            }
            for disk in self.all_disk_paths() {
                crate::template::qimg::snapshot_apply(&disk, name).await?;
            }
            Ok(())
        }
    }

    async fn drop_snapshot(&self, name: &str) -> Result<()> {
        match self.power_state().await {
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
}

/// A VM's display is QEMU's: screendumps over QMP, input over QMP or RFB
/// depending on what the guest actually listens to.
#[async_trait::async_trait]
impl super::display::DisplayHost for VmInstance {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    async fn qmp(&self) -> Result<QmpClient> {
        VmInstance::qmp(self).await
    }

    fn vnc_sock(&self) -> PathBuf {
        self.dirs.vnc_sock()
    }

    fn capture_dir(&self) -> PathBuf {
        self.dirs.run.clone()
    }

    fn input_transport(&self) -> crate::profiles::InputTransport {
        self.template().resolved.input_transport
    }
}

#[async_trait::async_trait]
impl super::machine::Machine for VmInstance {
    fn virtiofsd_available(&self) -> bool {
        self.hv.virtiofsd_available()
    }

    fn as_machine(&self) -> &dyn super::machine::Machine {
        self
    }

    fn name(&self) -> &str {
        &self.cfg.name
    }

    fn kind(&self) -> super::machine::MachineKind {
        super::machine::MachineKind::Vm
    }

    fn arch(&self) -> String {
        self.template().resolved.arch.clone()
    }

    fn guest_os(&self) -> super::guest_os::GuestOs {
        super::guest_os::guest_os_of(self.template().resolved.profile.as_deref())
    }

    fn nics(&self) -> &[model::Nic] {
        &self.cfg.nics
    }

    fn macs(&self) -> &[MacAddr] {
        &self.macs
    }

    fn web_pages(&self) -> &[model::WebPage] {
        &self.cfg.web
    }

    fn logins(&self) -> &[model::Login] {
        &self.cfg.logins
    }

    fn term_session_sock(&self, id: u32) -> PathBuf {
        self.dirs.term_session_sock(id)
    }

    fn nic_sock(&self, i: usize) -> PathBuf {
        self.dirs.nic_sock(i)
    }

    /// A VM's argv carries pre-opened descriptors, so its NICs can ride the
    /// afxdp tap fast path.
    fn takes_tap_fds(&self) -> bool {
        true
    }

    fn event_subject(&self) -> &'static str {
        "vm"
    }

    fn local_dir(&self) -> &Path {
        &self.dirs.local
    }

    fn run_dir(&self) -> &Path {
        &self.dirs.run
    }

    /// The per-arch emulator, `qemu-img` for the clone, and `swtpm` when the
    /// resolved hardware wants a TPM.
    fn required_binaries(&self) -> Vec<String> {
        let t = self.template();
        let mut needed = vec![
            "qemu-img".to_string(),
            qemu::emulator_binary(&t.resolved.arch),
        ];
        if t.resolved.tpm {
            needed.push("swtpm".to_string());
        }
        needed
    }

    async fn state(&self) -> PowerState {
        self.power_state().await
    }

    async fn is_ready(&self) -> bool {
        self.ready_flag().await
    }

    async fn stop(&self, force: bool) -> Result<()> {
        self.stop_ladder(force).await
    }

    async fn poweroff(&self) -> Result<()> {
        super::machine::quit_and_settle(self, VmInstance::qmp(self).await).await
    }

    /// Wire this VM's NICs into the lab fabric — taps on the fast path where
    /// available, since a VM's argv can carry pre-opened descriptors — then
    /// spawn QEMU with event-emitting callbacks.
    async fn start(self: Arc<Self>, lab: Arc<dyn super::machine::LabServices>) -> Result<()> {
        if self.power_state().await != PowerState::Stopped {
            return Ok(());
        }
        // Safety net for paths that don't pull explicitly (restore, wscript):
        // a no-op unless this VM's template download is still pending.
        lab.ensure_pulled(&self.cfg.name).await?;
        let events = lab.events().clone();
        events.emit("vm.starting", serde_json::json!({"vm": self.cfg.name}));

        std::fs::create_dir_all(&self.dirs.run)?;
        let attachments = super::machine::attach_all_nics(&*self, &*lab).await?;
        self.set_nic_attachments(attachments).await;

        let events_exit = events.clone();
        let vm_name = self.cfg.name.clone();
        let vm_name2 = self.cfg.name.clone();
        self.boot(
            move |reason, status| {
                let payload =
                    serde_json::json!({"vm": vm_name, "reason": reason, "status": status});
                match reason {
                    StopReason::Crashed => {
                        events_exit.emit("vm.crashed", payload.clone());
                        events_exit.emit("vm.stopped", payload);
                    }
                    _ => events_exit.emit("vm.stopped", payload),
                }
            },
            move || {
                events.emit("vm.ready", serde_json::json!({"vm": vm_name2}));
            },
        )
        .await
    }

    /// An online snapshot boots the VM (through the normal start path, so the
    /// NIC wiring and event callbacks are the usual ones) and loads into it;
    /// an offline one reverts the qcow2 chain and leaves it stopped.
    async fn restore(
        self: Arc<Self>,
        lab: Arc<dyn super::machine::LabServices>,
        snap: &str,
        online: bool,
    ) -> Result<()> {
        // Restoring into a running VM needs NIC listeners only if QEMU must be
        // booted; go through `start` for that so the wiring stays in one place.
        if online && self.power_state().await == PowerState::Stopped {
            Arc::clone(&self).start(Arc::clone(&lab)).await?;
        }
        let events_exit = lab.events().clone();
        let events_ready = lab.events().clone();
        let n1 = self.cfg.name.clone();
        let n2 = self.cfg.name.clone();
        self.load_snapshot(
            snap,
            online,
            move |reason, status| {
                events_exit.emit(
                    "vm.stopped",
                    serde_json::json!({"vm": n1, "reason": reason, "status": status}),
                );
            },
            move || events_ready.emit("vm.ready", serde_json::json!({"vm": n2})),
        )
        .await
    }

    /// A VM may be running a template first-boot provision through a Windows
    /// specialize/OOBE pass and a settle reboot.
    fn ready_timeout(&self) -> Duration {
        VM_READY_TIMEOUT
    }

    fn pending_first_boot(&self) -> Option<super::machine::FirstBootProvision> {
        if !self.first_boot_pending() {
            return None;
        }
        let template = self.template();
        Some(super::machine::FirstBootProvision {
            template: self.cfg.template.to_string(),
            script: template.first_boot.clone()?,
        })
    }

    async fn first_boot_done(&self) -> Result<()> {
        std::fs::write(self.dirs.firstboot_sentinel(), b"")
            .with_context(|| format!("writing first-boot sentinel for {}", self.cfg.name))?;
        self.mark_ready().await;
        Ok(())
    }

    async fn agent(&self) -> Result<super::vm_agent::AgentHandle> {
        self.agent_handle().await
    }

    async fn agent_answering(&self) -> bool {
        self.agent_is_answering().await
    }

    async fn is_agent_up(&self) -> bool {
        self.agent_up_flag().await
    }

    fn has_agent_channel(&self) -> bool {
        self.template().resolved.agent_channel
    }

    async fn clear_agent_failure(&self) {
        self.agent.clear_failure().await;
    }

    async fn snapshot(&self, name: &str) -> Result<bool> {
        self.take_snapshot(name).await
    }

    async fn delete_snapshot(&self, name: &str) -> Result<()> {
        self.drop_snapshot(name).await
    }

    fn display(self: Arc<Self>) -> Option<super::display::Display> {
        Some(super::display::Display::new(self))
    }

    fn can_reboot(&self) -> bool {
        true
    }

    async fn reboot_guest(&self) -> Result<()> {
        self.agent_handle()
            .await?
            .shutdown_guest(
                super::vm_agent::ShutdownMode::Reboot,
                Duration::from_secs(10),
            )
            .await
            .map_err(|e| anyhow!("rebooting {}: {e}", self.cfg.name))
    }

    /// Mount this VM's shares through the guest agent (PRD §7.5).
    ///
    /// What to run is a [`crate::smb::mount_plan`] — this only drives it, and
    /// only knows how to retry. The commands themselves, and every piece of
    /// guest-OS knowledge behind them, live next to the mount-step type.
    /// XP-era guests without an agent are mounted by provision scripts via
    /// screen automation instead (documented; not attempted here).
    ///
    /// Waits for its own readiness rather than making the wave wait on the
    /// retry window — Windows needs minutes before `net use` stops returning
    /// error 67.
    async fn mount_shares(self: Arc<Self>, lab: Arc<dyn super::machine::LabServices>) {
        if self.cfg.shares.is_empty() {
            return;
        }
        if self.wait_ready(self.ready_timeout()).await.is_err() {
            return;
        }
        let vm_name = &self.cfg.name;
        let os_hint = crate::smb::guest_os_hint(self.template().resolved.profile.as_deref());
        let smb_steps = lab.smb_mount_plan(vm_name, os_hint).await;
        let plan = crate::smb::mount_plan(os_hint, &self.virtiofs_mounts().await, smb_steps);
        // A share the guest cannot mount is the author's to fix, so it goes
        // on the event feed the console watches, not only into the log.
        for note in &plan.unsupported {
            tracing::warn!("{vm_name}: {note}");
            lab.events().emit(
                "share.unmountable",
                serde_json::json!({"vm": vm_name, "reason": note}),
            );
        }
        if plan.is_empty() {
            return;
        }
        let Ok(agent) = self.agent_handle().await else {
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

    /// QEMU's serial file, which is where a VM's guest console output lands.
    fn console_log(&self, lines: usize) -> Option<Result<String>> {
        Some(tail_file(&self.dirs.logs.join("serial.log"), lines))
    }

    async fn status_detail(&self) -> super::machine::MachineDetail {
        super::machine::MachineDetail::Vm(crate::status::VmStatus {
            template: self.cfg.template.to_string(),
            arch: self.cfg.arch.clone(),
            cpus: self.cfg.cpus,
            memory: self.cfg.memory,
            // The template carries a baked-in vmlab-agent (terminal support);
            // null on vintage guests and pre-agent templates.
            agent_version: self.template().agent_version.clone(),
        })
    }
}

/// The last `lines` lines of a log file; a file that was never written is
/// empty rather than an error (the machine may not have started yet).
pub(super) fn tail_file(path: &Path, lines: usize) -> Result<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(lines);
    Ok(all[start..].join("\n"))
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
