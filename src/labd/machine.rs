//! The Machine seam: one interface over the two things a lab can boot.
//!
//! A machine is a VM or a container ([`CONTEXT.md`]). Both attach to segments,
//! both answer on the same `vmlab.agent.0` channel, both snapshot, and both
//! report the same power states — so callers (the lab runtime, the wscript
//! runtime, playbooks, and the whole `machine.*` command surface) address them
//! through [`Machine`] and never branch on which kind they hold.
//!
//! What genuinely differs is expressed as a **capability**, not as a kind:
//!
//! - a framebuffer is [`Machine::display`], `Some` for VMs and `None` for
//!   containers — the 7 screen/keyboard/pointer/vision operations live on the
//!   concrete [`Display`] rather than on every machine;
//! - a console log is [`Machine::console_log`], `Some` only for containers;
//! - clipboard and Windows event log are agent features, already negotiated at
//!   handshake time and reported through [`Capabilities::agent`].
//!
//! `start` and `restore` are deliberately absent. Bringing a machine up needs
//! setup the two kinds do not share (segment attachment and template clones for
//! VMs; an image spec and a restart policy for containers), and that difference
//! belongs below a Hypervisor seam rather than in this interface. Until then
//! [`super::lab::LabRuntime`] owns those two, and they are the only places left
//! that know a machine's kind.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::model::{self, MacAddr};

use super::vm::PowerState;
use super::vm_agent::AgentHandle;

/// Which of the two kinds a machine is. Reported so a UI can pick an icon —
/// never so a caller can pick a code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    Vm,
    Container,
}

/// What a machine can do beyond the universal operations, probed rather than
/// inferred. Drives `machine.capabilities`, which is how the web console
/// decides whether to offer a console tab, a clipboard button or a log view.
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub kind: MachineKind,
    /// A framebuffer: screenshots, key/pointer input, OCR, image matching.
    pub display: bool,
    /// A console log readable from the host.
    pub console_log: bool,
    /// The guest can reboot in place and come back.
    pub reboot: bool,
    /// Agent features negotiated at handshake (`terminal`, `exec`, `file`,
    /// `tail`, `metrics`, `clipboard`, `eventlog`). Empty when no agent is
    /// answering — which is a live fact, not a property of the kind.
    pub agent: Vec<String>,
}

/// One NIC's addressing as reported to `status`.
#[derive(Debug, Clone, Serialize)]
pub struct NicStatus {
    /// `None` on a NAT-only NIC that joins no segment.
    pub segment: Option<String>,
    pub mac: Option<String>,
    pub static_ip: Option<String>,
    pub ip: Option<String>,
}

/// A guest web page as reported to `status` — no credentials; the browser only
/// needs enough to build a launch link.
#[derive(Debug, Clone, Serialize)]
pub struct WebPageStatus {
    pub name: String,
    pub port: u16,
    pub path: String,
}

/// One machine's line in `status`. The common fields are shared; each adapter
/// contributes its own under `extra` (a VM's template and hardware, a
/// container's image, health and restarts) so this stays one projection
/// instead of two hand-written ones.
#[derive(Debug, Clone, Serialize)]
pub struct MachineStatus {
    pub name: String,
    pub kind: MachineKind,
    pub state: PowerState,
    pub ready: bool,
    pub ip: Option<String>,
    pub nics: Vec<NicStatus>,
    pub web: Vec<WebPageStatus>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Everything a lab can boot, attach to a segment, and drive through the
/// agent.
///
/// Implemented by [`super::vm::VmInstance`] and
/// [`super::container::ContainerInstance`]; obtained from
/// [`super::lab::LabRuntime::machine`].
#[async_trait::async_trait]
pub trait Machine: Send + Sync + 'static {
    // ---- identity ---------------------------------------------------------

    fn name(&self) -> &str;
    fn kind(&self) -> MachineKind;
    /// CPU architecture the machine runs (`x86_64`, `aarch64`, `riscv64`).
    fn arch(&self) -> String;
    /// Guest OS family, for callers that must shape a command line for it.
    fn guest_os(&self) -> super::playbook::GuestOs;
    fn nics(&self) -> &[model::Nic];
    fn macs(&self) -> &[MacAddr];
    fn web_pages(&self) -> &[model::WebPage];
    /// Host socket re-exposing one agent terminal session as a raw byte pipe.
    fn term_session_sock(&self, id: u32) -> PathBuf;

    // ---- power ------------------------------------------------------------

    async fn state(&self) -> PowerState;
    async fn is_ready(&self) -> bool;
    /// Graceful stop ladder, or an immediate kill when `force`.
    async fn stop(&self, force: bool) -> Result<()>;

    /// Wait for the exit monitor to settle the power state.
    async fn wait_state(&self, want: PowerState, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.state().await != want {
            if tokio::time::Instant::now() > deadline {
                bail!(
                    "{}: still {:?} after {timeout:?}",
                    self.name(),
                    self.state().await
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    /// How long `up` waits for this machine to become ready before giving
    /// up. A container's entrypoint starts fast and its healthcheck governs
    /// the rest; a VM may be running a first-boot provision through a
    /// Windows settle reboot.
    fn ready_timeout(&self) -> Duration {
        Duration::from_secs(300)
    }

    /// Wait until the machine is fully usable: agent up and any first-boot
    /// work complete (PRD §10.3).
    async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.is_ready().await {
                return Ok(());
            }
            if self.state().await == PowerState::Stopped {
                bail!("{} stopped while waiting for ready", self.name());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("{}: not ready after {timeout:?}", self.name());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    // ---- agent ------------------------------------------------------------

    /// The vmlab-agent channel, connecting on first use.
    async fn agent(&self) -> Result<AgentHandle>;
    /// Whether the agent answers a ping *right now* — unlike the sticky
    /// [`is_agent_up`](Machine::is_agent_up) this goes false mid-reboot.
    async fn agent_answering(&self) -> bool;

    /// Ask the guest to reboot in place, leaving the machine running.
    ///
    /// Not every machine can: a container micro-VM restarts from a fresh
    /// rootfs, so anything the reboot was meant to make stick would be lost.
    /// The default refuses, and [`Capabilities::reboot`] reports it, so a
    /// caller learns this from the machine rather than from its kind.
    async fn reboot_guest(&self) -> Result<()> {
        bail!(
            "{}: this machine cannot reboot in place (a container micro-VM \
             restarts from a fresh rootfs)",
            self.name()
        )
    }

    /// Whether [`reboot_guest`](Machine::reboot_guest) is available.
    fn can_reboot(&self) -> bool {
        false
    }

    /// [`agent`](Machine::agent), retrying transient handshake failures until
    /// `timeout`. A machine can be momentarily agent-less while claiming to be
    /// up — Windows first-boot ends in a settle reboot, and readiness is sticky
    /// across guest reboots — so one failed handshake must not fail a caller.
    /// Hard failures (stopped, or a vintage guest with no agent channel)
    /// surface immediately.
    async fn wait_agent(&self, timeout: Duration) -> Result<AgentHandle> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.agent().await {
                Ok(agent) => return Ok(agent),
                Err(e) => {
                    let msg = format!("{e:#}");
                    if msg.contains("not running") || msg.contains("no agent channel") {
                        return Err(e);
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Per-NIC IPv4 addresses reported by the agent, matched to the configured
    /// NIC order by resolved MAC in one request.
    async fn guest_ips(&self) -> Result<Vec<Option<String>>> {
        let agent = self.agent().await?;
        let ifaces = agent.net_interfaces(Duration::from_secs(5)).await?;
        let macs: Vec<String> = self.macs().iter().map(ToString::to_string).collect();
        Ok(super::vm_agent::ipv4_by_mac(&ifaces, &macs))
    }

    /// First IPv4 address, or the address of a specific NIC (PRD §10.3).
    async fn guest_ip(&self, nic: Option<usize>) -> Result<String> {
        let ips = self.guest_ips().await?;
        let ip = match nic {
            Some(index) => ips.get(index).and_then(Clone::clone),
            None => ips.into_iter().flatten().next(),
        };
        ip.ok_or_else(|| anyhow!("{}: no IPv4 address reported by agent", self.name()))
    }

    // ---- snapshots (PRD §7.3) ---------------------------------------------

    /// Take a snapshot; returns whether it was online (running) or offline.
    async fn snapshot(&self, name: &str) -> Result<bool>;
    async fn delete_snapshot(&self, name: &str) -> Result<()>;

    /// The artefact identity a snapshot of this machine is only valid
    /// against, recorded at capture and re-checked on restore.
    ///
    /// A container's scratch overlay (and any vmstate) means nothing without
    /// the same read-only rootfs, so it pins the image digest. A VM's
    /// snapshots live inside its own qcow2 chain and are always
    /// self-consistent, so it pins nothing.
    fn snapshot_pin(&self) -> Option<String> {
        None
    }

    // ---- capability surfaces ----------------------------------------------

    /// The machine's framebuffer, if it has one. `None` for containers, which
    /// QEMU starts with no display device at all.
    fn display(self: Arc<Self>) -> Option<Display>;

    /// The last `lines` lines of the machine's console log. `None` for
    /// machines that keep no host-readable console — VMs log to QEMU's serial
    /// file, which is not the same artefact and is served by `logs` at the lab
    /// level.
    fn console_log(&self, lines: usize) -> Option<Result<String>> {
        let _ = lines;
        None
    }

    // ---- projection --------------------------------------------------------

    /// Kind-specific `status` fields, merged into [`MachineStatus::extra`].
    async fn status_extra(&self) -> Map<String, Value>;
}

/// Blanket helpers that need `Arc<dyn Machine>` rather than `&dyn Machine`.
impl dyn Machine {
    /// Everything this machine can do beyond the universal operations.
    /// Agent features come from a live handshake, so a machine that is up but
    /// not yet answering reports an empty list rather than a guess.
    pub async fn capabilities(self: &Arc<Self>) -> Capabilities {
        let agent = match self.agent().await {
            Ok(handle) => handle.info().features,
            Err(_) => Vec::new(),
        };
        Capabilities {
            kind: self.kind(),
            display: self.clone().display().is_some(),
            console_log: self.console_log(1).is_some(),
            reboot: self.can_reboot(),
            agent,
        }
    }

    /// This machine's line in `status`.
    pub async fn status(self: &Arc<Self>) -> MachineStatus {
        let ready = self.is_ready().await;
        let assigned = if ready {
            self.guest_ips()
                .await
                .unwrap_or_else(|_| vec![None; self.nics().len()])
        } else {
            vec![None; self.nics().len()]
        };
        let nics = self
            .nics()
            .iter()
            .enumerate()
            .map(|(i, nic)| NicStatus {
                segment: nic.segment.clone(),
                mac: self.macs().get(i).map(ToString::to_string),
                static_ip: nic.ip.map(|a| a.to_string()),
                ip: assigned.get(i).and_then(Clone::clone),
            })
            .collect();
        MachineStatus {
            name: self.name().to_string(),
            kind: self.kind(),
            state: self.state().await,
            ready,
            ip: assigned.iter().flatten().next().cloned(),
            nics,
            web: self
                .web_pages()
                .iter()
                .map(|w| WebPageStatus {
                    name: w.name.clone(),
                    port: w.port,
                    path: w.path.clone(),
                })
                .collect(),
            extra: self.status_extra().await,
        }
    }
}

// ---- shared implementation details -----------------------------------------

/// The cached-connection half of a machine's agent channel, shared by both
/// adapters: one live handle, plus when the last handshake failed so
/// agent-less guests don't pay the timeout on every call.
///
/// The "no agent answering" advice stays with the adapter — a VM wants a
/// template rebuild, a container wants its guest boot asset rebuilt — so
/// [`connect`](Self::connect) takes the hint rather than owning one.
#[derive(Default)]
pub(super) struct AgentSlot {
    handle: tokio::sync::Mutex<Option<AgentHandle>>,
    failed_at: tokio::sync::Mutex<Option<std::time::Instant>>,
}

impl AgentSlot {
    /// Connect (or reuse), with the token handshake. A guest reboot leaves the
    /// old connection open but talking to a fresh agent instance, so a live
    /// handle is pinged before it is handed out; a dead one is shut down
    /// explicitly, because QEMU's chardev serves exactly one client and a
    /// half-dead connection would block every future connect.
    pub async fn connect(&self, name: &str, sock: &Path, hint: &str) -> Result<AgentHandle> {
        let mut agent = self.handle.lock().await;
        if let Some(handle) = agent.as_ref() {
            if handle.ping(Duration::from_secs(2)).await {
                return Ok(handle.clone());
            }
            if let Some(dead) = agent.take() {
                dead.shutdown().await;
            }
        }
        {
            let failed = self.failed_at.lock().await;
            if let Some(at) = *failed
                && at.elapsed() < Duration::from_secs(30)
            {
                bail!("{name}: {hint}");
            }
        }
        match AgentHandle::connect(sock, Duration::from_secs(5)).await {
            Ok(handle) => {
                *self.failed_at.lock().await = None;
                *agent = Some(handle.clone());
                Ok(handle)
            }
            Err(e) => {
                *self.failed_at.lock().await = Some(std::time::Instant::now());
                Err(anyhow!("{name}: {e:#} — {hint}"))
            }
        }
    }

    /// Whether the agent answers right now, sharing (and populating) the
    /// cached handle. Bypasses the failed-handshake cache — pollers need
    /// prompt discovery, not exec-path backoff — with a shorter timeout.
    pub async fn probe(&self, sock: &Path) -> bool {
        let mut agent = self.handle.lock().await;
        if let Some(handle) = agent.as_ref() {
            if handle.ping(Duration::from_secs(2)).await {
                return true;
            }
            if let Some(dead) = agent.take() {
                dead.shutdown().await;
            }
        }
        match AgentHandle::connect(sock, Duration::from_secs(2)).await {
            Ok(handle) => {
                *self.failed_at.lock().await = None;
                *agent = Some(handle);
                true
            }
            Err(_) => false,
        }
    }

    /// The cached handle without connecting — what the stop ladder uses once
    /// the state has already left `Running`.
    pub async fn cached(&self) -> Option<AgentHandle> {
        self.handle.lock().await.clone()
    }

    /// Forget a recently failed handshake so the next connect retries at once
    /// — used right after installing or starting an agent. Unlike
    /// [`drop`](Self::drop) this never touches a live handle.
    pub async fn clear_failure(&self) {
        *self.failed_at.lock().await = None;
    }

    /// Drop the cached connection with an explicit shutdown: live sessions
    /// hold handle clones, and a half-dead connection blocks the one-client
    /// chardev slot.
    pub async fn drop(&self) {
        if let Some(handle) = self.handle.lock().await.take() {
            handle.shutdown().await;
        }
        *self.failed_at.lock().await = None;
    }
}

// ---- Display ----------------------------------------------------------------

/// A machine's framebuffer, with the operations that read and drive it:
/// screenshots, key chords, pointer input, OCR and image matching.
///
/// Obtained from [`Machine::display`], so a caller that holds one has already
/// established the machine has a screen — there is no "does this support
/// screenshots" check to forget. Concrete rather than a trait: QEMU's
/// framebuffer is the only thing that ever satisfies it, and a seam with one
/// adapter is a seam that has not happened yet.
pub struct Display {
    vm: Arc<super::vm::VmInstance>,
}

impl Display {
    pub(super) fn new(vm: Arc<super::vm::VmInstance>) -> Self {
        Self { vm }
    }

    /// Capture the screen to a PNG at `out`.
    pub async fn screenshot(&self, out: &Path) -> Result<()> {
        crate::scripting::interact::screenshot(&self.vm, out).await
    }

    /// Send a key chord (e.g. `ctrl-alt-delete`).
    pub async fn send_keys(&self, chord: &str) -> Result<()> {
        crate::scripting::interact::send_keys(&self.vm, chord).await
    }

    pub async fn mouse_move(&self, x: i64, y: i64) -> Result<()> {
        crate::scripting::interact::mouse_move(&self.vm, x, y).await
    }

    pub async fn mouse_click(&self, button: &str, at: Option<(i64, i64)>) -> Result<()> {
        crate::scripting::interact::mouse_click(&self.vm, button, at).await
    }

    pub async fn mouse_drag(&self, x1: i64, y1: i64, x2: i64, y2: i64) -> Result<()> {
        crate::scripting::interact::mouse_drag(&self.vm, x1, y1, x2, y2).await
    }

    /// Read text off the screen, optionally within a region.
    pub async fn ocr(&self, region: Option<(u32, u32, u32, u32)>) -> Result<String> {
        crate::scripting::interact::ocr(&self.vm, region).await
    }

    /// Search the screen for the first matching reference image.
    pub async fn find_image(
        &self,
        templates: &[PathBuf],
        opts: &crate::vision::MatchOptions,
    ) -> Result<Option<crate::vision::Match>> {
        crate::scripting::interact::find_image(&self.vm, templates, opts).await
    }
}
