//! The Machine seam: one interface over the two things a lab can boot.
//!
//! A machine is a VM or a container ([`CONTEXT.md`]). Both attach to segments,
//! both answer on the same `vmlab.agent.0` channel, both snapshot, and both
//! report the same power states — so callers (the lab runtime, the wscript
//! runtime, playbooks, and the whole `machine.*` command surface) address them
//! through [`Machine`] and never branch on which kind they hold.
//!
//! **There is no second route.** `start` and `restore` are on this interface
//! too: booting a VM needs a template clone and segment attachments, booting a
//! container needs an image spec, and that difference is implementation — not
//! a branch in a caller. The lab runtime supplies what both need through
//! [`LabServices`], and asks the machine to do the rest.
//!
//! What genuinely differs is expressed as a **capability**, not as a kind:
//!
//! - a framebuffer is [`Machine::display`] — the screen/keyboard/pointer/
//!   vision operations live on the concrete [`Display`], and absence is
//!   *reported*, never inferred: no container reports one today, and one
//!   running a display server would report one without a line changing here;
//! - a console log is [`Machine::console_log`];
//! - a healthcheck verdict is [`Machine::health`];
//! - in-place reboot is [`Machine::reboot_guest`], gated by
//!   [`Machine::can_reboot`];
//! - clipboard and Windows event log are agent features, already negotiated at
//!   handshake time and reported through [`Capabilities::agent`].
//!
//! This module used to claim the lab runtime "owns `start` and `restore`, and
//! they are the only places left that know a machine's kind". That decayed to
//! seven further branches before anyone noticed, because nothing enforced it
//! (ADR-0002). The claim is now enforced rather than asserted:
//! `orchestration_never_branches_on_machine_kind` in [`super::lab`] fails the
//! build if the lab runtime learns to ask what kind it is holding.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use crate::config::model::{self, MacAddr};
use crate::net::fastpath::NicAttachment;

pub use super::display::Display;
use super::events::EventLog;
use super::guest_os::GuestOs;
use super::state::MachineState;
use super::vm::PowerState;
use super::vm_agent::AgentHandle;

/// The status vocabulary this seam reports in. Declared with the projection
/// (ADR-0004) because it is what reaches the CLI, the REST surface and the
/// console; re-exported here because this is where machines produce it.
pub use crate::status::{
    MachineDetail, MachineKind, MachineLabel, MachineStatus, NicStatus, WebPageStatus,
};

/// How long [`Machine::poweroff`] waits for the emulator to actually go.
pub(super) const POWEROFF_SETTLE: Duration = Duration::from_secs(30);

/// How long a machine that does not say otherwise gets to become ready. A
/// container's entrypoint starts fast and its healthcheck governs the rest.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(300);

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
    /// The machine declares a healthcheck, so [`Machine::health`] carries a
    /// verdict rather than only "is it ready".
    pub healthcheck: bool,
    /// Agent features negotiated at handshake (`terminal`, `exec`, `file`,
    /// `tail`, `metrics`, `clipboard`, `eventlog`). Empty when no agent is
    /// answering — which is a live fact, not a property of the kind.
    pub agent: Vec<String>,
}

/// What booting a machine needs from the lab around it.
///
/// Deliberately narrow. [`super::lab::LabRuntime`] implements it, but a
/// machine implementation is handed this and not the runtime, so it cannot
/// reach for the rest of the lab — and a test can boot a machine against a
/// stub instead of a fabric.
#[async_trait::async_trait]
pub trait LabServices: Send + Sync + 'static {
    /// The event log lifecycle transitions are reported on.
    fn events(&self) -> &Arc<EventLog>;

    /// Download this machine's template or image if it is not cached yet. A
    /// no-op once cached, so a fully-cached lab stays offline.
    async fn ensure_pulled(&self, machine: &str) -> Result<()>;

    /// Make sure the lab's shared-folder server is serving. Idempotent.
    async fn ensure_shares(&self);

    /// Wire one NIC into `segment`'s switch, returning how the machine should
    /// speak to it.
    ///
    /// `tap_ok` says this machine can consume a pre-opened tap fd; a machine
    /// that can only take a stream socket (a container micro-VM builds its
    /// argv without fd passing) passes `false` and always gets
    /// [`NicAttachment::Stream`].
    async fn attach_nic(
        &self,
        segment: &str,
        sock: &Path,
        mac: MacAddr,
        isolated: bool,
        tap_ok: bool,
    ) -> Result<NicAttachment>;

    /// This machine reached readiness — (re-)install anything keyed on its
    /// network lease, which moves across restarts.
    async fn machine_ready(&self, machine: &str);

    /// The guest-side commands that mount whatever the lab's smbd exports for
    /// `machine`. Empty when no SMB server is running.
    async fn smb_mount_plan(
        &self,
        machine: &str,
        os: crate::smb::OsHint,
    ) -> Vec<crate::smb::MountStep>;
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
    fn guest_os(&self) -> GuestOs;
    fn nics(&self) -> &[model::Nic];
    fn macs(&self) -> &[MacAddr];
    fn web_pages(&self) -> &[model::WebPage];
    /// Host socket re-exposing one agent terminal session as a raw byte pipe.
    fn term_session_sock(&self, id: u32) -> PathBuf;

    /// Whether the host running this machine can serve a share over virtiofs
    /// ([`Hypervisor::virtiofsd_available`](super::hypervisor::Hypervisor::virtiofsd_available)).
    ///
    /// Exposed here because the decision it feeds is taken twice: once per
    /// machine as it starts (a vhost-user-fs device cannot hotplug, so the
    /// transport is fixed then) and once for the lab, when the share plan
    /// works out what `smbd` must export. Both must read the same host, or a
    /// substituted one disagrees with itself.
    fn virtiofsd_available(&self) -> bool;

    /// Host socket for the i-th NIC of the current run.
    fn nic_sock(&self, i: usize) -> PathBuf;

    /// Whether this machine can consume a pre-opened tap fd (the afxdp fast
    /// path), or only a stream socket. A hardware fact about how its command
    /// line is built, not a preference.
    fn takes_tap_fds(&self) -> bool {
        false
    }

    /// The word this machine's events name it under (`vm.stopped` vs
    /// `container.stopped`). Reported, like [`kind`](Machine::kind), so the
    /// event vocabulary stays what clients already parse without a caller
    /// asking what it is holding.
    fn event_subject(&self) -> &'static str;

    /// `.vmlab/<kind>s/<name>` — the disks, overlays and firmware state a
    /// destroy removes.
    fn local_dir(&self) -> &Path;
    /// `$XDG_RUNTIME_DIR/...` — this run's sockets.
    fn run_dir(&self) -> &Path;

    /// External binaries that must be on PATH before this machine can boot,
    /// so a missing package surfaces as one clear error rather than a spawn
    /// failure mid-`up`.
    fn required_binaries(&self) -> Vec<String>;

    /// This machine as the interface, so the default methods can share one
    /// polling loop. `impl Machine for T { fn as_machine(&self) -> &dyn Machine { self } }`
    /// — trait upcasting would make it unnecessary, and this goes when it lands.
    fn as_machine(&self) -> &dyn Machine;

    // ---- power ------------------------------------------------------------

    async fn state(&self) -> PowerState;
    async fn is_ready(&self) -> bool;
    /// Graceful stop ladder, or an immediate kill when `force`.
    async fn stop(&self, force: bool) -> Result<()>;

    /// Exit the emulator *gracefully*, flushing block-device caches first.
    ///
    /// The only safe seal for guests with no ACPI (DOS, Win 3.x): the stop
    /// ladder's bottom rung is a SIGKILL, which can drop unflushed qcow2
    /// writes and leave the disk unbootable. The default falls back to the
    /// ladder for a machine with no such control channel.
    async fn poweroff(&self) -> Result<()> {
        self.stop(false).await?;
        self.wait_state(PowerState::Stopped, POWEROFF_SETTLE).await
    }

    /// Boot this machine, wiring its NICs into the lab fabric and reporting
    /// its lifecycle on the lab's event log. A no-op when it is already up.
    ///
    /// The kind-specific work — a template clone and segment attachments for a
    /// VM, an image spec for a container — is implementation. What both need
    /// from the lab arrives through [`LabServices`].
    async fn start(self: Arc<Self>, lab: Arc<dyn LabServices>) -> Result<()>;

    /// Roll this machine back to `snap` (PRD §7.3).
    ///
    /// `online` is the power state recorded at capture, and it is the whole
    /// contract: an online snapshot resumes running exactly where it was
    /// (booting the machine first if it is off), an offline one leaves it
    /// stopped. The caller has already checked [`snapshot_pin`](Machine::snapshot_pin).
    async fn restore(
        self: Arc<Self>,
        lab: Arc<dyn LabServices>,
        snap: &str,
        online: bool,
    ) -> Result<()>;

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

    /// How long a caller waits for this machine to become ready before giving
    /// up. A container's entrypoint starts fast and its healthcheck governs
    /// the rest; a VM may be running a first-boot provision through a Windows
    /// settle reboot.
    ///
    /// Readiness policy lives here and nowhere else: callers pass
    /// `m.ready_timeout()`, never a literal, so a VM cannot wait one budget
    /// through one path and another through the next.
    fn ready_timeout(&self) -> Duration {
        DEFAULT_READY_TIMEOUT
    }

    /// Wait until the machine is fully usable: agent up and any first-boot
    /// work complete (PRD §10.3).
    ///
    /// The only implementation. There used to be four, with three different
    /// timeout budgets, and which one ran depended on whether the caller held
    /// a concrete machine or this interface — see [`ready_timeout`](Machine::ready_timeout).
    async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        wait_until(self.as_machine(), timeout, "ready", |m| {
            Box::pin(m.is_ready())
        })
        .await
    }

    // ---- first boot (PRD §6.1) ---------------------------------------------

    /// The provision script this machine must run once, before it can be
    /// reported ready. `None` when it carries none, or when it already ran for
    /// this instantiation.
    fn pending_first_boot(&self) -> Option<String> {
        None
    }

    /// Record that the first-boot provision completed and report ready.
    async fn first_boot_done(&self) -> Result<()> {
        Ok(())
    }

    // ---- agent ------------------------------------------------------------

    /// The vmlab-agent channel, connecting on first use.
    async fn agent(&self) -> Result<AgentHandle>;
    /// Whether this machine has an agent channel at all. `false` for a
    /// vintage guest whose profile predates virtio-serial — no terminal, exec,
    /// copy or readiness is possible, and no amount of waiting changes that.
    fn has_agent_channel(&self) -> bool {
        true
    }

    /// Forget a recently failed handshake so the next [`agent`](Machine::agent)
    /// call reconnects immediately — used right after installing or starting an
    /// agent. Unlike dropping the connection this never touches a live handle
    /// another task may be using.
    async fn clear_agent_failure(&self) {}

    /// Whether the agent answers a ping *right now* — unlike the sticky
    /// [`is_agent_up`](Machine::is_agent_up) this goes false mid-reboot.
    async fn agent_answering(&self) -> bool;

    /// Whether the agent has answered at least once since this machine
    /// started. Weaker than [`is_ready`](Machine::is_ready), which also waits
    /// for first-boot work, and stickier than
    /// [`agent_answering`](Machine::agent_answering), which drops mid-reboot.
    async fn is_agent_up(&self) -> bool {
        self.agent_answering().await
    }

    /// Wait until the agent has answered at least once.
    async fn wait_agent_up(&self, timeout: Duration) -> Result<()> {
        wait_until(self.as_machine(), timeout, "agent-up", |m| {
            Box::pin(m.is_agent_up())
        })
        .await
    }

    /// Wait until the agent answers a live ping — what a first-boot script
    /// that rebooted its own guest waits on, since QEMU never left `Running`
    /// and the sticky flags never dropped.
    async fn wait_agent_answering(&self, timeout: Duration) -> Result<()> {
        wait_until(self.as_machine(), timeout, "agent-answering", |m| {
            Box::pin(m.agent_answering())
        })
        .await
    }

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
                    // A permanent reason will not improve with waiting.
                    // Anything untyped is treated as permanent too: guessing
                    // that an unknown failure is retryable is how a caller
                    // ends up blocked for the whole timeout on a real fault.
                    let transient =
                        AgentUnavailable::of(&e).is_some_and(AgentUnavailable::is_transient);
                    if !transient || tokio::time::Instant::now() >= deadline {
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

    /// The machine's framebuffer, if it reports one.
    ///
    /// Absence is a fact about this machine, not about its kind: no container
    /// reports a display today because QEMU starts container micro-VMs with no
    /// display device, and one running a display server would report one by
    /// overriding this — with every screen operation working unchanged. The
    /// default is `None`, so a new machine kind is display-less until it says
    /// otherwise.
    fn display(self: Arc<Self>) -> Option<Display> {
        None
    }

    /// The last `lines` lines of the machine's console log.
    fn console_log(&self, lines: usize) -> Option<Result<String>> {
        let _ = lines;
        None
    }

    /// The latest healthcheck verdict, when the machine declares one.
    /// `None` means "no check, or no report yet" — a machine with no
    /// healthcheck is healthy once it is ready.
    async fn health(&self) -> Option<bool> {
        None
    }

    /// Whether this machine declares a healthcheck at all. Distinct from
    /// [`health`](Machine::health) returning `None`, which also covers "declared,
    /// but has not reported yet".
    fn has_healthcheck(&self) -> bool {
        false
    }

    /// Whether this machine is healthy right now: its healthcheck's latest
    /// verdict, or plain readiness when it declares none.
    ///
    /// A machine with no healthcheck answers `true` once it is ready rather
    /// than reporting the capability missing. That is deliberate and predates
    /// this interface: `is_healthy` is a gate lab authors write `if
    /// m.is_healthy()` around, and failing it on every machine that declares
    /// no check would break every script that does. [`Capabilities::healthcheck`]
    /// is how a caller learns whether the answer means anything.
    async fn is_healthy(&self) -> bool {
        match self.health().await {
            Some(healthy) => healthy,
            None => self.is_ready().await,
        }
    }

    // ---- shared folders (PRD §7.5, §18) -------------------------------------

    /// Hand the machine the credentials its guest mounts the lab's SMB server
    /// with. Called once smbd is up, for machines whose guest does the mount
    /// itself rather than being driven from the host.
    async fn smb_ready(&self, gateway: std::net::Ipv4Addr, username: &str, password: &str) {
        let _ = (gateway, username, password);
    }

    /// Mount this machine's shares from inside the guest, once it is ready.
    ///
    /// Spawned per machine by `up` and expected to take its time — Windows
    /// needs minutes before `net use` stops returning error 67 — so it waits
    /// for readiness itself rather than making the wave wait on the retry
    /// window. A no-op for machines whose guest mounts its own folders (a
    /// container's init does it from the spec it was handed).
    async fn mount_shares(self: Arc<Self>, lab: Arc<dyn LabServices>) {
        let _ = lab;
    }

    // ---- teardown ------------------------------------------------------------

    /// Forget the persisted artefacts a destroy invalidated. The lab has
    /// already removed this machine's directories; anything it recorded about
    /// them in lab state is the machine's to clear.
    async fn forget_artefacts(&self, state: &mut MachineState) {
        let _ = state;
    }

    // ---- projection --------------------------------------------------------

    /// This machine's kind-specific half of [`MachineStatus`] — the variant
    /// only this adapter can fill (ADR-0004).
    async fn status_detail(&self) -> MachineDetail;
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
            healthcheck: self.has_healthcheck(),
            agent,
        }
    }

    /// This machine's line in `status`.
    ///
    /// `cached` is left true here: whether a registry download is still pending
    /// is lab-level knowledge, and [`super::lab::LabRuntime::status`] fills it
    /// in.
    pub async fn status(self: &Arc<Self>) -> MachineStatus {
        let ready = self.is_ready().await;
        let state = self.state().await;
        let detail = self.status_detail().await;
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
            label: MachineLabel::derive(state, ready, &detail),
            state,
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
            cached: true,
            detail,
        }
    }
}

// ---- shared implementation details -----------------------------------------

/// Wire every one of `m`'s NICs into the lab fabric, in declaration order.
///
/// One implementation for both adapters: the segment, socket and MAC come from
/// the machine, and whether it can take a tap fd is
/// [`takes_tap_fds`](Machine::takes_tap_fds). A machine that ignores the
/// returned attachments (a container micro-VM connects to the sockets itself)
/// simply drops them.
pub(super) async fn attach_all_nics(
    m: &dyn Machine,
    lab: &dyn LabServices,
) -> Result<Vec<NicAttachment>> {
    let tap_ok = m.takes_tap_fds();
    let mut attachments = Vec::with_capacity(m.nics().len());
    for (i, nic) in m.nics().iter().enumerate() {
        let sock = m.nic_sock(i);
        let _ = std::fs::remove_file(&sock);
        let mac = *m
            .macs()
            .get(i)
            .ok_or_else(|| anyhow!("{}: no persisted MAC for nic {i}", m.name()))?;
        attachments.push(
            lab.attach_nic(
                super::network::nic_segment_name(nic),
                &sock,
                mac,
                nic.isolated,
                tap_ok,
            )
            .await?,
        );
    }
    Ok(attachments)
}

/// [`Machine::poweroff`] for a machine QEMU is running: a clean QMP `quit`,
/// then wait for the exit monitor to settle.
///
/// The QMP call is expected to fail — QEMU exits, so the connection drops
/// mid-request — and a machine that is already down has no channel at all. The
/// power state is the only answer that matters, so it is the only thing
/// checked.
pub(super) async fn quit_and_settle(
    m: &dyn Machine,
    qmp: Result<crate::qmp::QmpClient>,
) -> Result<()> {
    if let Ok(qmp) = qmp {
        let _ = qmp.quit().await;
    }
    m.wait_state(PowerState::Stopped, POWEROFF_SETTLE).await
}

/// One polling wait, behind every "wait until …" on this interface: settle at
/// 250ms, give up early when the machine stopped underneath (nothing it is
/// waiting on can happen now), and name the machine and the budget on
/// timeout.
async fn wait_until(
    m: &dyn Machine,
    timeout: Duration,
    what: &str,
    probe: impl for<'a> Fn(&'a dyn Machine) -> futures::future::BoxFuture<'a, bool>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if probe(m).await {
            return Ok(());
        }
        if m.state().await == PowerState::Stopped {
            bail!("{} stopped while waiting for {what}", m.name());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("{}: not {what} after {timeout:?}", m.name());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Why a machine's agent channel could not be reached.
///
/// The distinction that matters to a caller is whether waiting could change
/// the answer. A stopped machine and a vintage guest with no agent channel
/// will never answer; a failed handshake often will, because a machine can be
/// briefly agent-less while claiming to be up — Windows first-boot ends in a
/// settle reboot, and readiness is sticky across guest reboots.
///
/// Carried inside `anyhow::Error`, so the 21 call sites that only want a
/// message are untouched and the few that must branch use
/// [`AgentUnavailable::of`].
#[derive(Debug, thiserror::Error)]
pub enum AgentUnavailable {
    #[error("{0}: not running")]
    NotRunning(String),
    #[error(
        "{0}: this guest profile has no agent channel (vintage guest) — \
         no interactive terminal is possible"
    )]
    NoChannel(String),
    #[error("{machine}: {message}")]
    Handshake { machine: String, message: String },
}

impl AgentUnavailable {
    /// Whether retrying could succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Handshake { .. })
    }

    /// Recover the typed reason from an error returned by
    /// [`Machine::agent`], if that is what it is.
    pub fn of(err: &anyhow::Error) -> Option<&AgentUnavailable> {
        err.downcast_ref::<AgentUnavailable>()
    }
}

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
                return Err(AgentUnavailable::Handshake {
                    machine: name.to_string(),
                    message: hint.to_string(),
                }
                .into());
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
                Err(AgentUnavailable::Handshake {
                    machine: name.to_string(),
                    message: format!("{e:#} — {hint}"),
                }
                .into())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `wait_agent` retries a handshake and gives up immediately on a reason
    /// that will never improve. Before this was typed it decided by searching
    /// the error's text for "not running" — so rewording a message, or an
    /// unrelated error that happened to contain the phrase, silently changed
    /// whether callers blocked for the full timeout.
    #[test]
    fn only_a_failed_handshake_is_worth_retrying() {
        let stopped = AgentUnavailable::NotRunning("dc01".into());
        let vintage = AgentUnavailable::NoChannel("dos622".into());
        let handshake = AgentUnavailable::Handshake {
            machine: "dc01".into(),
            message: "timed out".into(),
        };
        assert!(!stopped.is_transient(), "a stopped machine never answers");
        assert!(
            !vintage.is_transient(),
            "a vintage guest has no agent at all"
        );
        assert!(
            handshake.is_transient(),
            "a machine can be briefly agent-less across a settle reboot"
        );
    }

    /// The reason survives the trip through `anyhow`, which is how it reaches
    /// `wait_agent` without changing 21 call sites that only want a message.
    #[test]
    fn the_reason_survives_anyhow() {
        let err: anyhow::Error = AgentUnavailable::NotRunning("dc01".into()).into();
        let found = AgentUnavailable::of(&err).expect("recoverable");
        assert!(matches!(found, AgentUnavailable::NotRunning(n) if n == "dc01"));
        assert!(!found.is_transient());
        // And it still reads well for a human.
        assert_eq!(format!("{err}"), "dc01: not running");
    }

    /// An error from somewhere else must not be mistaken for a retryable
    /// handshake — treating unknown failures as transient would block a
    /// caller for the whole timeout on a real fault.
    #[test]
    fn an_unrelated_error_is_not_transient() {
        let err = anyhow::anyhow!("disk full while creating the clone");
        assert!(AgentUnavailable::of(&err).is_none());
    }
}
