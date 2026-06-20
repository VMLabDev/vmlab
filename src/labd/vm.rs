//! Per-VM runtime: disk preparation, QEMU spawn, the §7.2 stop ladder,
//! readiness, and §7.3 snapshots.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

use crate::config::model::{self, MacAddr};
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
    pub fn vnc_sock(&self) -> PathBuf {
        self.run.join("vnc.sock")
    }
    pub fn tpm_sock(&self) -> PathBuf {
        self.run.join("tpm.sock")
    }
    pub fn nic_sock(&self, i: usize) -> PathBuf {
        self.run.join(format!("nic{i}.sock"))
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

pub struct VmInstance {
    pub lab: String,
    pub cfg: model::Vm,
    pub resolved: qemu::ResolvedVm,
    pub dirs: VmDirs,
    pub macs: Vec<MacAddr>,
    /// Backing template disk in the store (None for scratch).
    pub backing: Option<PathBuf>,
    /// Primary disk virtual size (scratch: from config; clone: template's).
    pub disk_size: Option<u64>,
    /// CD-ROM image paths (config cdrom + built media), resolved absolute.
    pub cdroms: Vec<PathBuf>,
    pub floppy: Option<PathBuf>,
    /// Embedded first-boot provision script carried by the backing template
    /// (None for scratch / templates without one). Run on first instantiation,
    /// before the VM is reported ready (PRD §6.1).
    pub first_boot_script: Option<String>,

    state: RwLock<PowerState>,
    /// The guest agent answers `guest-ping`. Set by the readiness poller.
    agent_up: RwLock<bool>,
    /// The VM is fully provisioned and usable: agent up AND the first-boot
    /// provision (if any) has completed. Gates dependents and provisions.
    ready: RwLock<bool>,
    stop_requested: RwLock<bool>,
    qemu: Mutex<Option<Arc<Proc>>>,
    swtpm: Mutex<Option<Arc<Proc>>>,
    qmp: Mutex<Option<QmpClient>>,
    qga: Mutex<Option<GaClient>>,
}

impl VmInstance {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        lab: &str,
        cfg: model::Vm,
        resolved: qemu::ResolvedVm,
        dirs: VmDirs,
        macs: Vec<MacAddr>,
        backing: Option<PathBuf>,
        disk_size: Option<u64>,
        cdroms: Vec<PathBuf>,
        floppy: Option<PathBuf>,
        first_boot_script: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            lab: lab.to_string(),
            cfg,
            resolved,
            dirs,
            macs,
            backing,
            disk_size,
            cdroms,
            floppy,
            first_boot_script,
            state: RwLock::new(PowerState::Stopped),
            agent_up: RwLock::new(false),
            ready: RwLock::new(false),
            stop_requested: RwLock::new(false),
            qemu: Mutex::new(None),
            swtpm: Mutex::new(None),
            qmp: Mutex::new(None),
            qga: Mutex::new(None),
        })
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
        self.first_boot_script.is_some() && !self.dirs.firstboot_sentinel().exists()
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
            match (&self.backing, self.disk_size) {
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

    fn build_paths(&self) -> Result<VmPaths> {
        Ok(VmPaths {
            qmp_sock: self.dirs.qmp_sock(),
            qga_sock: self.dirs.qga_sock(),
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
            nics: self
                .macs
                .iter()
                .enumerate()
                .map(|(i, mac)| (*mac, self.dirs.nic_sock(i)))
                .collect(),
            ovmf_vars: (self.resolved.firmware == Some(crate::profiles::FirmwareKind::Ovmf))
                .then(|| self.dirs.ovmf_vars()),
            tpm_sock: self.resolved.tpm.then(|| self.dirs.tpm_sock()),
            serial_log: Some(self.dirs.logs.join("serial.log")),
        })
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

        let run = async {
            std::fs::create_dir_all(&self.dirs.run)?;
            std::fs::create_dir_all(&self.dirs.logs)?;
            self.ensure_disks().await?;

            // Per-VM writable OVMF VARS from the firmware template.
            if self.resolved.firmware == Some(crate::profiles::FirmwareKind::Ovmf)
                && !self.dirs.ovmf_vars().exists()
            {
                let fw = match self.resolved.arch.as_str() {
                    "x86_64" => qemu::firmware::ovmf_x86_64(self.resolved.secure_boot)?,
                    "aarch64" => qemu::firmware::uefi_aarch64()?,
                    "riscv64" => qemu::firmware::uefi_riscv64()?,
                    a => bail!("no UEFI firmware for arch {a}"),
                };
                std::fs::copy(&fw.vars_template, self.dirs.ovmf_vars())
                    .context("copying OVMF VARS template")?;
            }

            if self.resolved.tpm {
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

            let accel = qemu::pick_accel(&self.resolved.arch);
            if accel == qemu::Accel::Tcg {
                tracing::warn!(
                    "{}: KVM unavailable for {} — falling back to TCG (slow)",
                    self.cfg.name,
                    self.resolved.arch
                );
            }
            let args = qemu::build_args(&self.lab, &self.resolved, &self.build_paths()?, accel)?;
            let proc = Proc::spawn(
                &format!("qemu:{}", self.cfg.name),
                &qemu::emulator_binary(&self.resolved.arch),
                &args,
                &self.dirs.logs.join("qemu.log"),
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
            qmp.cont().await?;
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

    /// First IPv4 address reported by the guest agent (excluding loopback),
    /// or per-NIC when `nic` is given (PRD §10.3 vm.ip()).
    pub async fn guest_ip(&self, nic: Option<usize>) -> Result<String> {
        let qga = self.qga().await?;
        let ifaces = qga.network_interfaces(Duration::from_secs(5)).await?;
        let want_mac = nic.and_then(|i| self.macs.get(i)).map(|m| m.to_string());
        for iface in &ifaces {
            if let Some(want) = &want_mac
                && iface.hardware_address.as_deref() != Some(want.as_str())
            {
                continue;
            }
            for (addr, kind) in &iface.ips {
                if kind == "ipv4" && !addr.starts_with("127.") {
                    return Ok(addr.clone());
                }
            }
        }
        bail!("{}: no IPv4 address reported by agent", self.cfg.name)
    }
}

fn disk_nodes(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("disk{i}")).collect()
}

fn validate_snapshot_name(name: &str) -> Result<()> {
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
