//! Per-container runtime (PRD §18): scratch/config preparation, the micro-VM
//! QEMU spawn, the ctl-channel lifecycle, restart policy, readiness with the
//! healthcheck gate, and the stop ladder. Mirrors [`super::vm`] — a container
//! is "a VM whose OS is the guest asset and whose workload is one OCI image".

// This runtime is consumed by labd's orchestration wiring, which lands in
// the next stage; until then only the tests reach parts of it.
#![allow(clippy::too_many_arguments)]

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{Mutex, RwLock};

use vmlab_cinit_proto::{ContainerSpec, CtlCommand, CtlEvent, HealthSpec, SmbInfo, VolumeMount};

use super::container_ctl::CtlHandle;
use super::vm::{PowerState, StopReason};
use crate::config::model::{self, MacAddr, RestartPolicy, VolumeSource};
use crate::oci::image::model::{ImageConfig, ImageHealthcheck};
use crate::oci::image::pull::PulledImage;
use crate::qemu::container::ContainerVmPaths;
use crate::qemu::{self, Proc};
use crate::qga::GaClient;
use crate::qmp::QmpClient;

/// Scratch (overlay upper layer) qcow2 virtual size. Sparse — real usage is
/// whatever the container writes.
const SCRATCH_SIZE: u64 = 2 << 30;

/// Default micro-VM shape when the config leaves cpus/memory unset.
const DEFAULT_CPUS: u32 = 1;
const DEFAULT_MEMORY: u64 = 256 << 20;

/// PATH handed to `exec` when the merged spec env carries none (matches
/// cinit's fallback in guest/cinit/src/container.rs).
const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Consecutive rapid failures (runs shorter than [`RAPID_RUN`]) after which
/// the restart policy gives up.
const MAX_RAPID_FAILURES: u32 = 5;

/// A run at least this long resets the rapid-failure counter.
const RAPID_RUN: Duration = Duration::from_secs(60);

pub struct ContainerDirs {
    /// `.vmlab/containers/<name>` — scratch qcow2, cfg/ (container.json).
    pub local: PathBuf,
    /// `$XDG_RUNTIME_DIR/vmlab/labs/<lab>/containers/<name>` — sockets.
    pub run: PathBuf,
    /// `~/.local/state/vmlab/labs/<lab>/containers/<name>` — logs.
    pub logs: PathBuf,
}

impl ContainerDirs {
    /// Mirrors [`super::vm::VmDirs`]; the lab-local base honours
    /// `VMLAB_WORK_DIR` via [`crate::paths::lab_local_dir`].
    pub fn new(lab_root: &Path, lab: &str, name: &str) -> Self {
        Self {
            local: crate::paths::lab_local_dir(lab_root)
                .join("containers")
                .join(name),
            run: crate::paths::lab_runtime_dir(lab)
                .join("containers")
                .join(name),
            logs: crate::paths::state_dir()
                .join("labs")
                .join(lab)
                .join("containers")
                .join(name),
        }
    }

    pub fn qmp_sock(&self) -> PathBuf {
        self.run.join("qmp.sock")
    }
    pub fn qga_sock(&self) -> PathBuf {
        self.run.join("qga.sock")
    }
    pub fn ctl_sock(&self) -> PathBuf {
        self.run.join("ctl.sock")
    }
    /// The `vmlab.tty.0` interactive-shell socket (raw PTY bytes).
    pub fn tty_sock(&self) -> PathBuf {
        self.run.join("tty.sock")
    }
    pub fn nic_sock(&self, i: usize) -> PathBuf {
        self.run.join(format!("nic{i}.sock"))
    }
    /// Overlay upper-layer disk (formatted ext4 by cinit on first boot).
    pub fn scratch_disk(&self) -> PathBuf {
        self.local.join("scratch.qcow2")
    }
    /// Debug copy of the merged spec. The guest receives the spec over the
    /// ctl channel (plus the SMB credential, which this copy omits) — this
    /// file exists purely for humans troubleshooting a container.
    pub fn container_json(&self) -> PathBuf {
        self.local.join("container.json")
    }
    /// Serial console log: kernel messages + container stdout/stderr.
    pub fn console_log(&self) -> PathBuf {
        self.logs.join("console.log")
    }
}

/// Host directory backing a named volume. LAB-scoped — `.vmlab/volumes/<name>`
/// under the lab work root ([`crate::paths::lab_local_dir`]) — so containers
/// naming the same volume share data; retained until lab destroy.
pub fn named_volume_dir(lab_local: &Path, volume: &str) -> PathBuf {
    lab_local.join("volumes").join(volume)
}

/// SMB share name for a container's i-th volume. Deterministic from the
/// container name alone, so [`build_spec`] (guest mount source) and
/// [`resolve_volume_hosts`] (host export) agree without plumbing.
pub fn volume_share_name(container: &str, i: usize) -> String {
    format!("vol-{container}-{i}")
}

/// Resolve the config's volume list to concrete SMB exports in declaration
/// order: (share name, host dir, read-only). Relative host binds resolve
/// against the lab root; named volumes land under the lab work dir. The lab
/// serves these from its `smbd` at the segment gateway (PRD §18 — volumes
/// are network mounts so no filesystem device state lands in snapshots).
pub fn resolve_volume_hosts(
    cfg: &model::Container,
    lab_root: &Path,
) -> Vec<(String, PathBuf, bool)> {
    let lab_local = crate::paths::lab_local_dir(lab_root);
    cfg.volumes
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let host = match &v.source {
                VolumeSource::Host(p) if p.is_absolute() => p.clone(),
                VolumeSource::Host(p) => lab_root.join(p),
                VolumeSource::Named(name) => named_volume_dir(&lab_local, name),
            };
            (volume_share_name(&cfg.name, i), host, v.read_only)
        })
        .collect()
}

/// Merge the image config with the lab overrides into the `container.json`
/// document cinit applies verbatim.
///
/// Docker semantics throughout: env overrides replace by key (image order
/// preserved, new keys appended in override order); an entrypoint override
/// with no cmd override *clears* the image cmd rather than inheriting it.
pub fn build_spec(cfg: &model::Container, image: &ImageConfig, hostname: &str) -> ContainerSpec {
    let img = &image.config;

    let entrypoint = cfg
        .entrypoint
        .clone()
        .unwrap_or_else(|| img.entrypoint.clone());
    let cmd = match (&cfg.command, &cfg.entrypoint) {
        (Some(command), _) => command.clone(),
        // Docker: a new entrypoint invalidates the image's cmd.
        (None, Some(_)) => Vec::new(),
        (None, None) => img.cmd.clone(),
    };

    let mut env: Vec<(String, String)> = img
        .env
        .iter()
        .map(|e| match e.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (e.clone(), String::new()),
        })
        .collect();
    for o in &cfg.env {
        match env.iter_mut().find(|(k, _)| *k == o.name) {
            Some(slot) => slot.1 = o.value.clone(),
            None => env.push((o.name.clone(), o.value.clone())),
        }
    }

    let workdir = cfg
        .workdir
        .clone()
        .or_else(|| (!img.working_dir.is_empty()).then(|| img.working_dir.clone()));
    let user = cfg
        .user
        .clone()
        .or_else(|| (!img.user.is_empty()).then(|| img.user.clone()));

    let healthcheck = match &cfg.healthcheck {
        Some(h) => Some(HealthSpec {
            command: h.command.clone(),
            interval_secs: h.interval.as_secs().max(1),
            timeout_secs: h.timeout.as_secs().max(1),
            retries: h.retries,
            start_period_secs: h.start_period.as_secs(),
        }),
        None => img.healthcheck.as_ref().and_then(health_from_image),
    };

    ContainerSpec {
        hostname: hostname.to_string(),
        entrypoint,
        cmd,
        env,
        workdir,
        user,
        // The config has no stop_signal knob; the image's StopSignal wins,
        // None lets cinit default to SIGTERM.
        stop_signal: img.stop_signal.clone(),
        stop_grace_secs: 10,
        volumes: cfg
            .volumes
            .iter()
            .enumerate()
            .map(|(i, v)| VolumeMount {
                share: volume_share_name(&cfg.name, i),
                target: v.target.clone(),
                read_only: v.read_only,
            })
            .collect(),
        // Filled per send from the lab's live SMB server (see
        // `ContainerInstance::spec_for_guest`) — never persisted.
        smb: None,
        nics: cfg.nics.len() as u32,
        healthcheck,
    }
}

/// Convert an image-baked `HEALTHCHECK` (Go-duration nanoseconds, docker
/// defaults) to a [`HealthSpec`]. `["NONE"]`, an empty test, or an unknown
/// probe kind disable the check.
fn health_from_image(hc: &ImageHealthcheck) -> Option<HealthSpec> {
    let command = match hc.test.first().map(String::as_str) {
        Some("CMD") => hc.test[1..].to_vec(),
        Some("CMD-SHELL") => vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            hc.test.get(1).cloned().unwrap_or_default(),
        ],
        _ => return None,
    };
    if command.is_empty() {
        return None;
    }
    // Docker treats 0 (and absence) as "use the default"; sub-second values
    // round up so a positive setting never becomes zero.
    let secs = |ns: Option<i64>, default: u64| match ns {
        Some(n) if n > 0 => (n as u64).div_ceil(1_000_000_000),
        _ => default,
    };
    Some(HealthSpec {
        command,
        interval_secs: secs(hc.interval, 30),
        timeout_secs: secs(hc.timeout, 30),
        retries: hc.retries.filter(|r| *r > 0).unwrap_or(3),
        start_period_secs: secs(hc.start_period, 0),
    })
}

/// The restart decision after one exit, as a pure function so policy is
/// unit-testable. `consecutive_failures` counts rapid failures (runs shorter
/// than [`RAPID_RUN`]) *including* the exit being classified; a run of at
/// least [`RAPID_RUN`] resets it to zero before the call. `None` = stay
/// stopped; `Some(backoff)` = restart after that delay (1s·2ⁿ capped 30s).
pub fn should_restart(
    policy: RestartPolicy,
    stop_requested: bool,
    exit_code: Option<i32>,
    consecutive_failures: u32,
) -> Option<Duration> {
    if stop_requested {
        return None;
    }
    let wants_restart = match policy {
        RestartPolicy::No => false,
        RestartPolicy::Always => true,
        RestartPolicy::OnFailure => exit_code != Some(0),
    };
    if !wants_restart || consecutive_failures >= MAX_RAPID_FAILURES {
        return None;
    }
    let secs = 1u64 << consecutive_failures.saturating_sub(1).min(5);
    Some(Duration::from_secs(secs.min(30)))
}

/// The lifecycle callbacks a `start` installs, shared across restarts.
/// `on_exit(reason, exit_code, will_restart)` fires on every QEMU exit;
/// `on_ready` at most once per `start`; `on_health` on every health
/// transition.
struct Callbacks {
    on_exit: Box<dyn Fn(StopReason, Option<i32>, bool) + Send + Sync>,
    on_ready: Box<dyn Fn() + Send + Sync>,
    on_health: Box<dyn Fn(bool) + Send + Sync>,
    ready_fired: AtomicBool,
}

/// The image-derived half of a container: the pulled image (rootfs squashfs
/// and config) plus the pre-merged spec ([`build_spec`]) cinit receives over
/// the ctl channel. Behind a lock on [`ContainerInstance`] so a deferred
/// registry pull can bind it after the daemon is already up (`None` until
/// then).
pub struct ImageParts {
    pub image: PulledImage,
    pub spec: ContainerSpec,
}

pub struct ContainerInstance {
    pub lab: String,
    pub cfg: model::Container,
    /// Micro-VM architecture (the lab's arch; must match the pulled image).
    pub arch: String,
    pub dirs: ContainerDirs,
    pub macs: Vec<MacAddr>,
    /// Effective MTU of each NIC's segment, in declaration order.
    pub nic_mtus: Vec<u16>,
    /// See [`ImageParts`] — std lock (never held across await); `None` while
    /// the deferred image pull is still pending.
    image: std::sync::RwLock<Option<Arc<ImageParts>>>,
    /// SMB volume exports in declaration order: (share name, host dir,
    /// read-only) — see [`resolve_volume_hosts`].
    pub volumes: Vec<(String, PathBuf, bool)>,
    /// How the guest reaches the lab's SMB server for volume mounts —
    /// set by the lab once its `smbd` is up, before any volume-declaring
    /// start.
    smb: RwLock<Option<SmbInfo>>,

    state: RwLock<PowerState>,
    /// The bundled qemu-ga answers `guest-ping`.
    agent_up: RwLock<bool>,
    /// cinit reported the container process running (ctl `started`).
    started: RwLock<bool>,
    /// Started, agent up, and past the healthcheck gate (first healthy, or
    /// no healthcheck). Gates dependents and provisions.
    ready: RwLock<bool>,
    stop_requested: RwLock<bool>,
    /// Latest healthcheck verdict; `None` until the first report (or when
    /// there is no healthcheck).
    healthy: RwLock<Option<bool>>,
    /// Restarts performed by the policy since the last explicit `start`.
    restarts: AtomicU32,
    /// Consecutive rapid failures feeding [`should_restart`].
    consecutive_failures: AtomicU32,
    /// Exit code from the ctl `exited` event of the last run.
    last_exit: RwLock<Option<i32>>,
    qemu: Mutex<Option<Arc<Proc>>>,
    qmp: Mutex<Option<QmpClient>>,
    qga: Mutex<Option<GaClient>>,
    ctl: Mutex<Option<CtlHandle>>,
}

impl ContainerInstance {
    pub fn new(
        lab: &str,
        cfg: model::Container,
        arch: &str,
        dirs: ContainerDirs,
        macs: Vec<MacAddr>,
        nic_mtus: Vec<u16>,
        image: Option<PulledImage>,
        volumes: Vec<(String, PathBuf, bool)>,
    ) -> Arc<Self> {
        let parts = image.map(|image| {
            let spec = build_spec(&cfg, &image.config, &cfg.name);
            Arc::new(ImageParts { image, spec })
        });
        Arc::new(Self {
            lab: lab.to_string(),
            cfg,
            arch: arch.to_string(),
            dirs,
            macs,
            nic_mtus,
            image: std::sync::RwLock::new(parts),
            volumes,
            smb: RwLock::new(None),
            state: RwLock::new(PowerState::Stopped),
            agent_up: RwLock::new(false),
            started: RwLock::new(false),
            ready: RwLock::new(false),
            stop_requested: RwLock::new(false),
            healthy: RwLock::new(None),
            restarts: AtomicU32::new(0),
            consecutive_failures: AtomicU32::new(0),
            last_exit: RwLock::new(None),
            qemu: Mutex::new(None),
            qmp: Mutex::new(None),
            qga: Mutex::new(None),
            ctl: Mutex::new(None),
        })
    }

    /// The pulled image + merged spec, or an error while the deferred pull
    /// is still pending (`vmlab pull` / `up` binds it).
    pub fn image_parts(&self) -> Result<Arc<ImageParts>> {
        self.image
            .read()
            .expect("image lock")
            .clone()
            .ok_or_else(|| {
                anyhow!(
                    "{}: image not pulled yet — run `vmlab pull` or `vmlab up`",
                    self.cfg.name
                )
            })
    }

    /// The pinned manifest digest, when the image is bound.
    pub fn image_digest(&self) -> Option<String> {
        self.image
            .read()
            .expect("image lock")
            .as_ref()
            .map(|p| p.image.manifest_digest.clone())
    }

    /// Bind the image resolved by a deferred pull (re-merges the spec).
    pub fn set_image(&self, image: PulledImage) {
        let spec = build_spec(&self.cfg, &image.config, &self.cfg.name);
        *self.image.write().expect("image lock") = Some(Arc::new(ImageParts { image, spec }));
    }

    pub async fn state(&self) -> PowerState {
        *self.state.read().await
    }

    pub async fn is_ready(&self) -> bool {
        *self.ready.read().await
    }

    #[allow(dead_code)] // readiness detail; kept public alongside is_ready (mirrors VmInstance)
    pub async fn is_agent_up(&self) -> bool {
        *self.agent_up.read().await
    }

    /// Latest healthcheck verdict (`None`: no check, or no report yet).
    pub async fn health(&self) -> Option<bool> {
        *self.healthy.read().await
    }

    /// Exit code of the last completed run, from the ctl `exited` event.
    pub async fn last_exit(&self) -> Option<i32> {
        *self.last_exit.read().await
    }

    /// Restarts the policy has performed since the last explicit `start`.
    pub fn restart_count(&self) -> u32 {
        self.restarts.load(Ordering::SeqCst)
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

    pub async fn ctl(&self) -> Result<CtlHandle> {
        self.ctl
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("{}: not running", self.cfg.name))
    }

    /// Record how the guest reaches the lab's SMB server (volume mounts).
    pub async fn set_smb(&self, info: SmbInfo) {
        *self.smb.write().await = Some(info);
    }

    /// The spec as cinit receives it: the pre-merged document plus the SMB
    /// coordinates when volumes are declared.
    async fn spec_for_guest(&self) -> Result<ContainerSpec> {
        let mut spec = self.image_parts()?.spec.clone();
        if !spec.volumes.is_empty() {
            spec.smb = Some(self.smb.read().await.clone().ok_or_else(|| {
                anyhow!(
                    "{}: volumes declared but the lab SMB server is not up",
                    self.cfg.name
                )
            })?);
        }
        Ok(spec)
    }

    async fn send_spec(&self, ctl: &CtlHandle) -> Result<()> {
        let spec = self.spec_for_guest().await?;
        ctl.send(&CtlCommand::Spec { spec }).await
    }

    fn cpus(&self) -> u32 {
        self.cfg.cpus.unwrap_or(DEFAULT_CPUS)
    }

    fn memory(&self) -> u64 {
        self.cfg.memory.unwrap_or(DEFAULT_MEMORY)
    }

    fn build_paths(
        &self,
        asset: &crate::guest_asset::GuestAsset,
        parts: &ImageParts,
    ) -> ContainerVmPaths {
        ContainerVmPaths {
            kernel: asset.kernel.clone(),
            initrd: asset.initrd.clone(),
            rootfs_image: parts.image.rootfs_image.clone(),
            scratch_disk: self.dirs.scratch_disk(),
            nics: self
                .macs
                .iter()
                .enumerate()
                .map(|(i, mac)| (*mac, self.dirs.nic_sock(i), self.nic_mtus.get(i).copied()))
                .collect(),
            qmp_sock: self.dirs.qmp_sock(),
            qga_sock: self.dirs.qga_sock(),
            ctl_sock: self.dirs.ctl_sock(),
            tty_sock: self.dirs.tty_sock(),
            serial_log: self.dirs.console_log(),
        }
    }

    /// Spawn the micro-VM paused, connect QMP, release the CPUs, then attach
    /// the ctl channel and guest agent. The caller has already wired the NIC
    /// listener sockets on the segment switches.
    ///
    /// `on_exit(reason, exit_code, will_restart)` runs on every QEMU exit —
    /// `will_restart` marks exits the restart policy follows with a respawn.
    /// `on_ready` fires at most once, when the container is started, the
    /// agent answers, and the healthcheck (if any) first passes. `on_health`
    /// fires on every healthcheck transition for the container's lifetime.
    pub async fn start(
        self: &Arc<Self>,
        on_exit: impl Fn(StopReason, Option<i32>, bool) + Send + Sync + 'static,
        on_ready: impl Fn() + Send + Sync + 'static,
        on_health: impl Fn(bool) + Send + Sync + 'static,
    ) -> Result<()> {
        {
            let mut st = self.state.write().await;
            if *st != PowerState::Stopped {
                bail!("{} is {:?}", self.cfg.name, *st);
            }
            *st = PowerState::Starting;
        }
        *self.stop_requested.write().await = false;
        self.restarts.store(0, Ordering::SeqCst);
        self.consecutive_failures.store(0, Ordering::SeqCst);

        let cbs = Arc::new(Callbacks {
            on_exit: Box::new(on_exit),
            on_ready: Box::new(on_ready),
            on_health: Box::new(on_health),
            ready_fired: AtomicBool::new(false),
        });
        self.clone().start_attempt(cbs).await
    }

    /// Boxed [`start_attempt`](Self::start_attempt) so the exit monitor can
    /// re-invoke the start sequence without a recursive future type.
    fn start_attempt_boxed(
        self: Arc<Self>,
        cbs: Arc<Callbacks>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(self.start_attempt(cbs))
    }

    /// One run of the start sequence. The state must already be `Starting`
    /// (set by [`start`](Self::start) or the exit monitor's restart path);
    /// on failure the state reverts to `Stopped` and handles are torn down.
    async fn start_attempt(self: Arc<Self>, cbs: Arc<Callbacks>) -> Result<()> {
        *self.last_exit.write().await = None;
        *self.healthy.write().await = None;

        // Snapshot the image parts for the whole start sequence; errors out
        // while the deferred pull is still pending.
        let parts = match self.image_parts() {
            Ok(p) => p,
            Err(e) => {
                *self.state.write().await = PowerState::Stopped;
                return Err(e);
            }
        };
        let run = async {
            std::fs::create_dir_all(&self.dirs.local)?;
            std::fs::create_dir_all(&self.dirs.run)?;
            std::fs::create_dir_all(&self.dirs.logs)?;
            // Host dirs behind the SMB volume exports must exist before the
            // guest tries to mount them.
            for (_, host, _) in &self.volumes {
                std::fs::create_dir_all(host)
                    .with_context(|| format!("creating volume dir {}", host.display()))?;
            }
            // Debug copy only (no credential); the guest gets the spec over
            // the ctl channel.
            write_json_atomic(&self.dirs.container_json(), &parts.spec)?;

            if !self.dirs.scratch_disk().exists() {
                crate::template::qimg::create_blank(&self.dirs.scratch_disk(), SCRATCH_SIZE)
                    .await?;
            }

            let asset = crate::guest_asset::ensure_guest_asset(&self.arch)?;
            let accel = qemu::pick_accel(&self.arch);
            if accel == qemu::Accel::Tcg {
                tracing::warn!(
                    "{}: KVM unavailable for {} — falling back to TCG (slow)",
                    self.cfg.name,
                    self.arch
                );
            }
            let args = qemu::container::build_container_args(
                &self.lab,
                &self.cfg.name,
                &self.arch,
                self.cpus(),
                self.memory(),
                &self.build_paths(&asset, &parts),
            )?;
            let proc = Proc::spawn(
                &format!("qemu:{}", self.cfg.name),
                &qemu::emulator_binary(&self.arch),
                &args,
                &self.dirs.logs.join("qemu.log"),
            )
            .await?;
            *self.qemu.lock().await = Some(proc.clone());

            // QMP comes up shortly after spawn (-S leaves CPUs paused).
            let qmp = connect_qmp_retry(&self.dirs.qmp_sock(), &proc).await?;
            qmp.cont().await?;
            *self.qmp.lock().await = Some(qmp);

            // QEMU creates the ctl/qga sockets at startup; retry briefly in
            // case we won the race.
            let ctl = connect_ctl_retry(&self.dirs.ctl_sock(), &proc).await?;
            *self.ctl.lock().await = Some(ctl.clone());
            // cinit blocks its boot on the spec. Send one now (the guest
            // port is usually already open); the ctl watcher re-answers
            // every `boot` announcement in case this one raced the guest.
            self.send_spec(&ctl).await?;
            *self.qga.lock().await = Some(GaClient::connect(&self.dirs.qga_sock()).await?);

            Ok::<_, anyhow::Error>((proc, ctl))
        };

        let (proc, ctl) = match run.await {
            Ok(v) => v,
            Err(e) => {
                *self.state.write().await = PowerState::Stopped;
                self.teardown().await;
                return Err(e);
            }
        };

        *self.state.write().await = PowerState::Running;
        let started_at = tokio::time::Instant::now();

        self.spawn_ctl_watcher(&ctl, &cbs);
        self.spawn_readiness(&cbs);
        self.spawn_exit_monitor(proc, cbs, started_at);
        Ok(())
    }

    /// Track ctl events for the run: `started`, health transitions (with the
    /// `on_health` callback), and the exit code for stop classification.
    fn spawn_ctl_watcher(self: &Arc<Self>, ctl: &CtlHandle, cbs: &Arc<Callbacks>) {
        let me = self.clone();
        let cbs = cbs.clone();
        let ctl = ctl.clone();
        let mut rx = ctl.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    // cinit announces boot every second until the spec lands
                    // (a send can race the guest opening its port, and a
                    // snapshot-restore resync replays `boot` too) — answer
                    // every announcement; cinit ignores duplicates.
                    Ok(CtlEvent::Boot { .. }) => {
                        if let Err(e) = me.send_spec(&ctl).await {
                            tracing::warn!("{}: spec send failed: {e:#}", me.cfg.name);
                        }
                    }
                    Ok(CtlEvent::Started { .. }) => *me.started.write().await = true,
                    Ok(CtlEvent::Health { healthy }) => {
                        let prev = me.healthy.write().await.replace(healthy);
                        if prev != Some(healthy) {
                            (cbs.on_health)(healthy);
                        }
                    }
                    Ok(CtlEvent::Exited { code }) => *me.last_exit.write().await = Some(code),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return, // channel gone: QEMU exited
                }
            }
        });
    }

    /// Readiness (PRD §7.4 shape): ctl `started`, then the healthcheck gate —
    /// ready when the container process runs and (no healthcheck || first
    /// healthy report). Deliberately NOT gated on the bundled agent: the
    /// entrypoint runs regardless (docker semantics), and an agent hiccup
    /// must not wedge readiness. The agent is polled separately below; it
    /// only gates `exec`/`cp`.
    fn spawn_readiness(self: &Arc<Self>, cbs: &Arc<Callbacks>) {
        let me = self.clone();
        let cbs = cbs.clone();
        tokio::spawn(async move {
            // Phase 1: the container process is running.
            loop {
                if me.state().await != PowerState::Running {
                    return;
                }
                if *me.started.read().await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            // Phase 2: the healthcheck gate. (Running implies the image is
            // bound; a missing one just means no healthcheck gate.)
            let has_healthcheck = me
                .image_parts()
                .map(|p| p.spec.healthcheck.is_some())
                .unwrap_or(false);
            if has_healthcheck {
                loop {
                    if me.state().await != PowerState::Running {
                        return;
                    }
                    if *me.healthy.read().await == Some(true) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            *me.ready.write().await = true;
            if !cbs.ready_fired.swap(true, Ordering::SeqCst) {
                (cbs.on_ready)();
            }
        });

        // Agent poller (mirrors the VM poller): tracks `agent_up` for
        // exec/cp availability without holding readiness hostage.
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                if me.state().await != PowerState::Running {
                    return;
                }
                let qga = { me.qga.lock().await.clone() };
                if let Some(qga) = qga
                    && qga.ping(Duration::from_secs(2)).await
                {
                    *me.agent_up.write().await = true;
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    /// Classify why QEMU ended and apply the restart policy. `on_exit` fires
    /// on every exit; `will_restart` is true when a respawn follows.
    fn spawn_exit_monitor(
        self: &Arc<Self>,
        proc: Arc<Proc>,
        cbs: Arc<Callbacks>,
        started_at: tokio::time::Instant,
    ) {
        let me = self.clone();
        tokio::spawn(async move {
            let _ = proc
                .wait_exit(Duration::from_secs(60 * 60 * 24 * 365))
                .await;
            let requested = *me.stop_requested.read().await;
            let exit_code = *me.last_exit.read().await;
            // cinit powers off right after `exited`; no ctl exit at all
            // means the VM died out from under it.
            let reason = if requested {
                StopReason::Requested
            } else if exit_code == Some(0) {
                StopReason::GuestInitiated
            } else {
                StopReason::Crashed
            };
            me.teardown().await;
            *me.state.write().await = PowerState::Stopped;
            *me.agent_up.write().await = false;
            *me.started.write().await = false;
            *me.ready.write().await = false;

            // Rapid-failure accounting: a run of at least RAPID_RUN resets
            // the counter; anything shorter counts one more failure.
            let failures = if started_at.elapsed() >= RAPID_RUN {
                me.consecutive_failures.store(0, Ordering::SeqCst);
                0
            } else {
                me.consecutive_failures.fetch_add(1, Ordering::SeqCst) + 1
            };

            let Some(backoff) = should_restart(me.cfg.restart, requested, exit_code, failures)
            else {
                (cbs.on_exit)(reason, exit_code, false);
                return;
            };

            (cbs.on_exit)(reason, exit_code, true);
            me.restarts.fetch_add(1, Ordering::SeqCst);
            tracing::info!(
                "{}: restarting in {backoff:?} (policy {:?}, exit {exit_code:?})",
                me.cfg.name,
                me.cfg.restart
            );
            tokio::time::sleep(backoff).await;

            // A stop during the backoff cancels the restart.
            if *me.stop_requested.read().await {
                return;
            }
            {
                let mut st = me.state.write().await;
                if *st != PowerState::Stopped {
                    return; // someone else already started it
                }
                *st = PowerState::Starting;
            }
            if let Err(e) = me.clone().start_attempt_boxed(cbs.clone()).await {
                tracing::warn!("{}: restart failed: {e:#}", me.cfg.name);
                (cbs.on_exit)(StopReason::Crashed, None, false);
            }
        });
    }

    async fn teardown(&self) {
        *self.ctl.lock().await = None;
        *self.qmp.lock().await = None;
        *self.qga.lock().await = None;
        if let Some(proc) = self.qemu.lock().await.take()
            && proc.is_running()
        {
            proc.kill().await;
        }
    }

    /// Graceful stop ladder (PRD §7.2 shape): ctl `stop` (in-guest signal +
    /// grace) → guest-agent shutdown → hard kill, each with a timeout.
    /// Setting `stop_requested` first also cancels any pending policy
    /// restart.
    pub async fn stop(&self, force: bool) -> Result<()> {
        *self.stop_requested.write().await = true;
        let proc = { self.qemu.lock().await.clone() };
        let Some(proc) = proc else {
            return Ok(()); // already stopped (or waiting out a backoff)
        };
        *self.state.write().await = PowerState::Stopping;

        if force {
            proc.kill().await;
            let _ = proc.wait_exit(Duration::from_secs(10)).await;
            return self
                .wait_state(PowerState::Stopped, Duration::from_secs(10))
                .await;
        }

        // Rung 1: the ctl channel — cinit signals the container and powers
        // off once it exits (SIGKILL after the grace). A running container
        // always has its image bound; the fallback grace is cinit's default.
        let grace = self
            .image_parts()
            .map(|p| p.spec.stop_grace_secs)
            .unwrap_or(10);
        if let Ok(ctl) = self.ctl().await
            && ctl
                .send(&CtlCommand::Stop { grace_secs: grace })
                .await
                .is_ok()
            && proc
                .wait_exit(Duration::from_secs(grace + 15))
                .await
                .is_ok()
        {
            return self
                .wait_state(PowerState::Stopped, Duration::from_secs(10))
                .await;
        }

        // Rung 2: guest-agent shutdown.
        if let Ok(qga) = self.qga().await {
            let _ = qga.shutdown("powerdown", Duration::from_secs(5)).await;
            if proc.wait_exit(Duration::from_secs(15)).await.is_ok() {
                return self
                    .wait_state(PowerState::Stopped, Duration::from_secs(10))
                    .await;
            }
        }

        // Rung 3: QMP quit, then the hard kill.
        tracing::warn!("{}: graceful stop timed out, killing", self.cfg.name);
        if let Ok(qmp) = self.qmp().await {
            let _ = qmp.quit().await;
        }
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

    // ---- snapshots (PRD §7.3, §18) ------------------------------------------

    /// Take a snapshot of the scratch disk — the container's only writable
    /// state (the rootfs squashfs is immutable, volume contents are host
    /// state outside snapshot scope, §7.5 semantics). Online captures RAM +
    /// device state into the same qcow2, exactly like a VM; returns whether
    /// the capture was online.
    pub async fn snapshot(&self, name: &str) -> Result<bool> {
        super::vm::validate_snapshot_name(name)?;
        match self.state().await {
            PowerState::Running => {
                let qmp = self.qmp().await?;
                qmp.snapshot_save(name, "scratch", &["scratch"]).await?;
                Ok(true)
            }
            PowerState::Stopped => {
                let scratch = self.dirs.scratch_disk();
                if !scratch.exists() {
                    bail!(
                        "{}: no scratch disk yet — the container has never started",
                        self.cfg.name
                    );
                }
                crate::template::qimg::snapshot_create(&scratch, name).await?;
                Ok(false)
            }
            other => bail!("{} is {:?} — wait for it to settle", self.cfg.name, other),
        }
    }

    /// Load an online snapshot into the (already running) micro-VM and
    /// resume it exactly where it was, then ask cinit to replay lifecycle
    /// events — the resumed guest never re-emits `net_up`/`started`/`health`
    /// on its own, and host-side caches need them after a fresh boot.
    pub async fn restore_online(&self, name: &str) -> Result<()> {
        let qmp = self.qmp().await?;
        qmp.stop().await?;
        qmp.snapshot_load(name, "scratch", &["scratch"]).await?;
        // Drop the agent connection BEFORE resuming: the rewound guest
        // replays virtio-serial response bytes the host already consumed,
        // and qga responses carry no request ids — a stale `{"return":…}`
        // silently poisons a later exchange. With no client attached, QEMU
        // discards the replayed bytes; the reconnect below starts clean.
        self.qga.lock().await.take();
        qmp.cont().await?;
        self.reconnect_agent_after_load().await;
        if let Ok(ctl) = self.ctl().await {
            ctl.send(&CtlCommand::Resync).await?;
        }
        Ok(())
    }

    /// Re-establish the guest-agent connection after an online restore
    /// (see [`Self::restore_online`] for why it was dropped), giving the
    /// resumed guest a moment to flush its replayed output into the void
    /// first, then pinging (bounded) so restore hands back a container
    /// whose exec/cp surface demonstrably works.
    async fn reconnect_agent_after_load(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        {
            let mut qga = self.qga.lock().await;
            match GaClient::connect(&self.dirs.qga_sock()).await {
                Ok(c) => *qga = Some(c),
                Err(e) => {
                    tracing::warn!(
                        "{}: agent reconnect after restore failed: {e:#}",
                        self.cfg.name
                    );
                    return;
                }
            }
        }
        if let Ok(qga) = self.qga().await {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while !qga.ping(Duration::from_secs(2)).await {
                if tokio::time::Instant::now() > deadline {
                    tracing::warn!("{}: agent not answering after restore", self.cfg.name);
                    break;
                }
            }
        }
    }

    /// Apply an offline snapshot: power off if needed, revert the scratch
    /// disk, stay off (PRD §7.3 restore semantics). The stop sets
    /// `stop_requested`, so no restart policy fires.
    pub async fn restore_offline(&self, name: &str) -> Result<()> {
        if self.state().await != PowerState::Stopped {
            self.stop(false).await?;
            self.wait_state(PowerState::Stopped, Duration::from_secs(60))
                .await?;
        }
        crate::template::qimg::snapshot_apply(&self.dirs.scratch_disk(), name).await?;
        Ok(())
    }

    pub async fn delete_snapshot(&self, name: &str) -> Result<()> {
        match self.state().await {
            PowerState::Running => {
                let qmp = self.qmp().await?;
                qmp.snapshot_delete(name, &["scratch"]).await?;
            }
            _ => {
                crate::template::qimg::snapshot_delete(&self.dirs.scratch_disk(), name).await?;
            }
        }
        Ok(())
    }

    /// Wait until the container is fully ready: started, agent up, and past
    /// the healthcheck gate.
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

    /// First IPv4 address of the container: the ctl channel's `net_up`
    /// report when available, else the guest agent's interface list
    /// (excluding loopback) — mirroring the VM path.
    pub async fn guest_ip(&self) -> Result<String> {
        if let Some(ctl) = self.ctl.lock().await.clone()
            && let Some(ip) = ctl.ip().await
        {
            return Ok(ip);
        }
        let qga = self.qga().await?;
        let ifaces = qga.network_interfaces(Duration::from_secs(5)).await?;
        for iface in &ifaces {
            for (addr, kind) in &iface.ips {
                if kind == "ipv4" && !addr.starts_with("127.") {
                    return Ok(addr.clone());
                }
            }
        }
        bail!("{}: no IPv4 address reported by agent", self.cfg.name)
    }

    /// Run a command *inside the container rootfs*: qemu-ga lives in the
    /// init namespace, so the command is wrapped as
    /// `/bin/busybox chroot /rootfs <cmd> <args…>` with the container's
    /// merged environment (plus a PATH fallback matching cinit's).
    pub async fn exec(
        &self,
        cmd: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<crate::qga::ExecResult> {
        let qga = self.qga().await?;
        let parts = self.image_parts()?;
        let mut wrapped: Vec<&str> = vec!["chroot", "/rootfs", cmd];
        wrapped.extend_from_slice(args);
        let mut env: Vec<String> = parts
            .spec
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if !parts.spec.env.iter().any(|(k, _)| k == "PATH") {
            env.push(DEFAULT_PATH.to_string());
        }
        Ok(qga
            .exec_env("/bin/busybox", &wrapped, &env, true, timeout)
            .await?)
    }

    /// Copy a host file into the container rootfs (guest paths are
    /// container-relative; the agent sees them under `/rootfs`).
    pub async fn copy_to(&self, host: &Path, guest_path: &str, timeout: Duration) -> Result<()> {
        let data = tokio::fs::read(host)
            .await
            .with_context(|| format!("reading {}", host.display()))?;
        let qga = self.qga().await?;
        qga.file_write(&rootfs_path(guest_path), &data, timeout)
            .await?;
        Ok(())
    }

    /// Copy a file out of the container rootfs to the host.
    pub async fn copy_from(&self, guest_path: &str, host: &Path, timeout: Duration) -> Result<()> {
        let qga = self.qga().await?;
        let data = qga.file_read(&rootfs_path(guest_path), timeout).await?;
        if let Some(parent) = host.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(host, data).with_context(|| format!("writing {}", host.display()))?;
        Ok(())
    }

    /// The last `lines` lines of the container's console log (kernel
    /// messages + the container's stdout/stderr).
    pub fn logs(&self, lines: usize) -> Result<String> {
        let content = match std::fs::read_to_string(self.dirs.console_log()) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading {}", self.dirs.console_log().display()));
            }
        };
        let all: Vec<&str> = content.lines().collect();
        let start = all.len().saturating_sub(lines);
        Ok(all[start..].join("\n"))
    }
}

/// Map a container-relative guest path to where the agent (which runs in
/// the init namespace) sees it.
fn rootfs_path(guest_path: &str) -> String {
    format!("/rootfs/{}", guest_path.trim_start_matches('/'))
}

/// Write a JSON document atomically (temp file + rename) so cinit can never
/// read a torn `container.json`.
fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(value)?;
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming into place: {}", path.display()))?;
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

async fn connect_ctl_retry(sock: &Path, proc: &Arc<Proc>) -> Result<CtlHandle> {
    for _ in 0..100 {
        if !proc.is_running() {
            bail!(
                "QEMU exited during startup: {}",
                proc.exit_status().unwrap_or_default()
            );
        }
        match CtlHandle::connect(sock).await {
            Ok(c) => return Ok(c),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    bail!("ctl socket {} never came up", sock.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{EnvVar, Healthcheck, ImageRef, Volume};
    use crate::oci::image::model::RuntimeDefaults;

    fn container(name: &str) -> model::Container {
        model::Container {
            name: name.into(),
            span: (0, 0),
            image: ImageRef {
                reference: "nginx".into(),
            },
            image_span: (0, 0),
            entrypoint: None,
            command: None,
            workdir: None,
            user: None,
            cpus: None,
            memory: None,
            depends_on: vec![],
            restart: RestartPolicy::No,
            nics: vec![],
            env: vec![],
            volumes: vec![],
            ports: vec![],
            healthcheck: None,
        }
    }

    fn image(defaults: RuntimeDefaults) -> ImageConfig {
        ImageConfig {
            architecture: "amd64".into(),
            os: "linux".into(),
            config: defaults,
            rootfs: Default::default(),
        }
    }

    fn env_var(name: &str, value: &str) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: value.into(),
            span: (0, 0),
        }
    }

    // ---- build_spec: entrypoint/cmd matrix (docker semantics) --------------

    #[test]
    fn spec_entrypoint_cmd_matrix() {
        let img = image(RuntimeDefaults {
            entrypoint: vec!["/entry.sh".into()],
            cmd: vec!["serve".into(), "--all".into()],
            ..Default::default()
        });

        // No overrides: image entrypoint + image cmd.
        let cfg = container("c");
        let spec = build_spec(&cfg, &img, "c");
        assert_eq!(spec.entrypoint, vec!["/entry.sh"]);
        assert_eq!(spec.cmd, vec!["serve", "--all"]);

        // Cmd override only: image entrypoint + new cmd.
        let mut cfg = container("c");
        cfg.command = Some(vec!["debug".into()]);
        let spec = build_spec(&cfg, &img, "c");
        assert_eq!(spec.entrypoint, vec!["/entry.sh"]);
        assert_eq!(spec.cmd, vec!["debug"]);

        // Entrypoint override only: docker CLEARS the image cmd.
        let mut cfg = container("c");
        cfg.entrypoint = Some(vec!["/bin/sh".into(), "-c".into(), "sleep 1".into()]);
        let spec = build_spec(&cfg, &img, "c");
        assert_eq!(spec.entrypoint, vec!["/bin/sh", "-c", "sleep 1"]);
        assert!(spec.cmd.is_empty(), "image cmd must not survive: {spec:?}");

        // Both overridden.
        let mut cfg = container("c");
        cfg.entrypoint = Some(vec!["/e".into()]);
        cfg.command = Some(vec!["run".into()]);
        let spec = build_spec(&cfg, &img, "c");
        assert_eq!(spec.entrypoint, vec!["/e"]);
        assert_eq!(spec.cmd, vec!["run"]);
    }

    // ---- build_spec: env merge ----------------------------------------------

    #[test]
    fn spec_env_overrides_by_key_in_order() {
        let img = image(RuntimeDefaults {
            env: vec![
                "PATH=/usr/bin".into(),
                "LANG=C.UTF-8".into(),
                "FLAGLIKE".into(), // no '=' — key with empty value
            ],
            ..Default::default()
        });
        let mut cfg = container("c");
        cfg.env = vec![env_var("LANG", "en_AU.UTF-8"), env_var("EXTRA", "1")];
        let spec = build_spec(&cfg, &img, "c");
        assert_eq!(
            spec.env,
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                // Overridden in place — image position preserved.
                ("LANG".to_string(), "en_AU.UTF-8".to_string()),
                ("FLAGLIKE".to_string(), String::new()),
                // New keys appended in override order.
                ("EXTRA".to_string(), "1".to_string()),
            ]
        );
    }

    // ---- build_spec: workdir/user/stop_signal/volumes/nics ------------------

    #[test]
    fn spec_scalar_overrides_and_image_fallbacks() {
        let img = image(RuntimeDefaults {
            working_dir: "/srv".into(),
            user: "nginx".into(),
            stop_signal: Some("SIGQUIT".into()),
            ..Default::default()
        });

        let cfg = container("web1");
        let spec = build_spec(&cfg, &img, "web1");
        assert_eq!(spec.hostname, "web1");
        assert_eq!(spec.workdir.as_deref(), Some("/srv"));
        assert_eq!(spec.user.as_deref(), Some("nginx"));
        assert_eq!(spec.stop_signal.as_deref(), Some("SIGQUIT"));
        assert_eq!(spec.stop_grace_secs, 10);

        let mut cfg = container("web1");
        cfg.workdir = Some("/app".into());
        cfg.user = Some("1000:1000".into());
        let spec = build_spec(&cfg, &img, "web1");
        assert_eq!(spec.workdir.as_deref(), Some("/app"));
        assert_eq!(spec.user.as_deref(), Some("1000:1000"));

        // Empty image fields mean "unset", and no StopSignal leaves the
        // spec's None (cinit defaults SIGTERM).
        let spec = build_spec(&container("c"), &image(Default::default()), "c");
        assert!(spec.workdir.is_none());
        assert!(spec.user.is_none());
        assert!(spec.stop_signal.is_none());
    }

    #[test]
    fn spec_volumes_and_nics() {
        let mut cfg = container("db");
        cfg.volumes = vec![
            Volume {
                source: VolumeSource::Host("./data".into()),
                target: "/var/lib/db".into(),
                read_only: false,
                span: (0, 0),
            },
            Volume {
                source: VolumeSource::Named("shared".into()),
                target: "/shared".into(),
                read_only: true,
                span: (0, 0),
            },
        ];
        cfg.nics = vec![
            model::Nic {
                span: (0, 0),
                segment: Some("lan".into()),
                nat: false,
                ip: None,
                mac: None,
                isolated: false,
            },
            model::Nic {
                span: (0, 0),
                segment: Some("dmz".into()),
                nat: false,
                ip: None,
                mac: None,
                isolated: false,
            },
        ];
        let spec = build_spec(&cfg, &image(Default::default()), "db");
        assert_eq!(spec.nics, 2);
        assert_eq!(
            spec.volumes,
            vec![
                VolumeMount {
                    share: "vol-db-0".into(),
                    target: "/var/lib/db".into(),
                    read_only: false,
                },
                VolumeMount {
                    share: "vol-db-1".into(),
                    target: "/shared".into(),
                    read_only: true,
                },
            ]
        );
        // The credential is injected per send, never baked into the spec.
        assert!(spec.smb.is_none());
    }

    // ---- build_spec: healthchecks -------------------------------------------

    #[test]
    fn spec_healthcheck_config_wins_over_image() {
        let img = image(RuntimeDefaults {
            healthcheck: Some(ImageHealthcheck {
                test: vec!["CMD".into(), "/img-check".into()],
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut cfg = container("c");
        cfg.healthcheck = Some(Healthcheck {
            command: vec!["/my-check".into(), "-q".into()],
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(2),
            retries: 4,
            start_period: Duration::from_secs(30),
            span: (0, 0),
        });
        let hc = build_spec(&cfg, &img, "c").healthcheck.unwrap();
        assert_eq!(hc.command, vec!["/my-check", "-q"]);
        assert_eq!(hc.interval_secs, 5);
        assert_eq!(hc.timeout_secs, 2);
        assert_eq!(hc.retries, 4);
        assert_eq!(hc.start_period_secs, 30);
    }

    #[test]
    fn spec_image_healthcheck_conversion() {
        // CMD: argv as-is; nanoseconds → seconds; docker defaults fill gaps.
        let img = image(RuntimeDefaults {
            healthcheck: Some(ImageHealthcheck {
                test: vec!["CMD".into(), "curl".into(), "-f".into(), "http://x/".into()],
                interval: Some(10_000_000_000),
                timeout: None,
                retries: None,
                start_period: Some(5_000_000_000),
            }),
            ..Default::default()
        });
        let hc = build_spec(&container("c"), &img, "c").healthcheck.unwrap();
        assert_eq!(hc.command, vec!["curl", "-f", "http://x/"]);
        assert_eq!(hc.interval_secs, 10);
        assert_eq!(hc.timeout_secs, 30, "docker default");
        assert_eq!(hc.retries, 3, "docker default");
        assert_eq!(hc.start_period_secs, 5);

        // CMD-SHELL wraps in /bin/sh -c.
        let img = image(RuntimeDefaults {
            healthcheck: Some(ImageHealthcheck {
                test: vec!["CMD-SHELL".into(), "curl -f http://x/ || exit 1".into()],
                ..Default::default()
            }),
            ..Default::default()
        });
        let hc = build_spec(&container("c"), &img, "c").healthcheck.unwrap();
        assert_eq!(
            hc.command,
            vec!["/bin/sh", "-c", "curl -f http://x/ || exit 1"]
        );
        assert_eq!(hc.interval_secs, 30);

        // NONE (and empty/unknown) disable the check.
        for test in [vec!["NONE".to_string()], vec![], vec!["WEIRD".to_string()]] {
            let img = image(RuntimeDefaults {
                healthcheck: Some(ImageHealthcheck {
                    test,
                    ..Default::default()
                }),
                ..Default::default()
            });
            assert!(build_spec(&container("c"), &img, "c").healthcheck.is_none());
        }

        // No healthcheck anywhere.
        assert!(
            build_spec(&container("c"), &image(Default::default()), "c")
                .healthcheck
                .is_none()
        );
    }

    #[test]
    fn image_healthcheck_subsecond_rounds_up() {
        let hc = health_from_image(&ImageHealthcheck {
            test: vec!["CMD".into(), "/c".into()],
            interval: Some(500_000_000), // 0.5s
            timeout: Some(1_500_000_000),
            retries: Some(0), // 0 = docker default
            start_period: Some(0),
        })
        .unwrap();
        assert_eq!(hc.interval_secs, 1, "never rounds a positive value to 0");
        assert_eq!(hc.timeout_secs, 2);
        assert_eq!(hc.retries, 3);
        assert_eq!(hc.start_period_secs, 0);
    }

    // ---- restart policy ------------------------------------------------------

    #[test]
    fn restart_policy_decision() {
        use RestartPolicy::*;
        let s = should_restart;

        // `no` never restarts.
        assert_eq!(s(No, false, Some(1), 1), None);
        assert_eq!(s(No, false, Some(0), 0), None);

        // A requested stop never restarts, whatever the policy.
        assert_eq!(s(Always, true, Some(1), 1), None);
        assert_eq!(s(OnFailure, true, Some(1), 1), None);

        // `always` restarts on any exit code (including clean).
        assert!(s(Always, false, Some(0), 1).is_some());
        assert!(s(Always, false, Some(137), 1).is_some());

        // `on-failure` only on non-zero (or unknown) exits.
        assert_eq!(s(OnFailure, false, Some(0), 1), None);
        assert!(s(OnFailure, false, Some(2), 1).is_some());
        assert!(
            s(OnFailure, false, None, 1).is_some(),
            "no ctl exit report = crash = failure"
        );

        // Exponential backoff: 1s·2ⁿ for the nth consecutive rapid failure.
        assert_eq!(s(Always, false, Some(1), 1), Some(Duration::from_secs(1)));
        assert_eq!(s(Always, false, Some(1), 2), Some(Duration::from_secs(2)));
        assert_eq!(s(Always, false, Some(1), 3), Some(Duration::from_secs(4)));
        assert_eq!(s(Always, false, Some(1), 4), Some(Duration::from_secs(8)));

        // Gives up after MAX_RAPID_FAILURES consecutive rapid failures.
        assert_eq!(s(Always, false, Some(1), 5), None);
        assert_eq!(s(Always, false, Some(1), 6), None);

        // A long run reset the counter (0): restart after the base delay.
        assert_eq!(s(Always, false, Some(1), 0), Some(Duration::from_secs(1)));
    }

    // ---- dirs & volumes -------------------------------------------------------

    #[test]
    fn container_dirs_layout() {
        let dirs = ContainerDirs::new(Path::new("/labs/demo"), "demo", "web");
        // The lab-local base honours VMLAB_WORK_DIR (untestable without env
        // mutation) but always ends with containers/<name>.
        assert!(dirs.local.ends_with("containers/web"), "{:?}", dirs.local);
        assert!(
            dirs.run
                .to_string_lossy()
                .contains("vmlab/labs/demo/containers/web")
                || dirs
                    .run
                    .to_string_lossy()
                    .contains("labs/demo/containers/web"),
            "{:?}",
            dirs.run
        );
        assert!(
            dirs.logs.ends_with("vmlab/labs/demo/containers/web")
                || dirs.logs.ends_with("labs/demo/containers/web"),
            "{:?}",
            dirs.logs
        );
        assert_eq!(dirs.qmp_sock(), dirs.run.join("qmp.sock"));
        assert_eq!(dirs.qga_sock(), dirs.run.join("qga.sock"));
        assert_eq!(dirs.ctl_sock(), dirs.run.join("ctl.sock"));
        assert_eq!(dirs.nic_sock(1), dirs.run.join("nic1.sock"));
        assert_eq!(dirs.scratch_disk(), dirs.local.join("scratch.qcow2"));
        assert_eq!(dirs.container_json(), dirs.local.join("container.json"));
        assert_eq!(dirs.console_log(), dirs.logs.join("console.log"));
    }

    #[test]
    fn volume_hosts_resolve_binds_and_named() {
        let mut cfg = container("c");
        cfg.volumes = vec![
            Volume {
                source: VolumeSource::Host("./html".into()),
                target: "/usr/share/nginx/html".into(),
                read_only: true,
                span: (0, 0),
            },
            Volume {
                source: VolumeSource::Host("/abs/certs".into()),
                target: "/etc/certs".into(),
                read_only: true,
                span: (0, 0),
            },
            Volume {
                source: VolumeSource::Named("pgdata".into()),
                target: "/var/lib/postgresql".into(),
                read_only: false,
                span: (0, 0),
            },
        ];
        let vols = resolve_volume_hosts(&cfg, Path::new("/labs/demo"));
        assert_eq!(vols[0].0, "vol-c-0");
        assert_eq!(vols[0].1, PathBuf::from("/labs/demo/./html"));
        assert!(vols[0].2);
        assert_eq!(vols[1].1, PathBuf::from("/abs/certs"));
        assert_eq!(vols[2].0, "vol-c-2");
        // Named volumes are LAB-scoped: under the lab work dir, not the
        // container's own dir.
        assert!(vols[2].1.ends_with("volumes/pgdata"), "{:?}", vols[2].1);
        assert!(!vols[2].2);
    }

    // ---- misc ------------------------------------------------------------------

    #[test]
    fn rootfs_path_prefixes() {
        assert_eq!(
            rootfs_path("/etc/nginx/nginx.conf"),
            "/rootfs/etc/nginx/nginx.conf"
        );
        assert_eq!(rootfs_path("relative/file"), "/rootfs/relative/file");
    }

    #[test]
    fn json_writes_are_atomic_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("container.json");
        let spec = build_spec(&container("c"), &image(Default::default()), "c");
        write_json_atomic(&path, &spec).unwrap();
        let read: ContainerSpec =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, spec);
        assert!(!path.with_extension("json.tmp").exists());
    }
}
