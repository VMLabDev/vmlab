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
use serde::{Deserialize, Serialize};

use crate::config::model::{self, MacAddr};
use crate::net::fastpath::NicAttachment;

pub use super::display::Display;
use super::events::EventLog;
use super::guest_os::GuestOs;
use super::state::MachineState;
use super::vm::PowerState;
use super::vm_agent::AgentHandle;
use crate::sync::LockRecover;

/// The status vocabulary this seam reports in. Declared with the projection
/// (ADR-0004) because it is what reaches the CLI, the REST surface and the
/// console; re-exported here because this is where machines produce it.
pub use crate::status::{MachineDetail, MachineKind, MachineLabel, MachineStatus, NicStatus};

/// How long [`Machine::poweroff`] waits for the emulator to actually go.
pub(super) const POWEROFF_SETTLE: Duration = Duration::from_secs(30);

/// How long a machine that does not say otherwise gets to become ready. A
/// container's entrypoint starts fast and its healthcheck governs the rest.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(300);

/// What a machine can do beyond the universal operations, probed rather than
/// inferred. Drives `machine.capabilities`, which is how a surface decides
/// whether to offer a console, a clipboard or a log view.
///
/// `Deserialize` because a client reads it back, and reading it as the
/// producer's own type is what stops a field renamed here from silently
/// becoming an empty answer there (ADR-0004's lesson).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Agent features negotiated at handshake (`terminal`, `exec`, `fileops`,
    /// `tail`, `metrics`, `clipboard`, `eventlog`, `watch`). Empty
    /// when no agent is answering — which is a live fact, not a property of
    /// the kind.
    pub agent: Vec<String>,
}

/// Where a machine's vmlab-agent came from — and so whether pushing the
/// host's shipped binary into it means anything (PRD §19.4).
///
/// A capability, not a kind: the repair verb asks this rather than asking
/// whether it is holding a container, so a future machine whose agent also
/// ships with the host is answered correctly without a line changing in the
/// verb (ADR-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOrigin {
    /// Sealed into the machine's own artefact at build time (§6.1, §7.4). It
    /// is whatever that build baked, it *can* go stale, and the repair verb
    /// exists so picking up an agent change does not cost a 15–45 minute
    /// Windows rebuild.
    Image,
    /// Shipped with this host's vmlab and loaded at boot — a container
    /// micro-VM's agent lives in the initramfs guest asset. It tracks the
    /// host's installed vmlab and cannot go stale, so there is nothing to
    /// repair: refreshing it means reinstalling the guest asset.
    HostAsset,
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

    /// This machine's agent has answered and it is about to be reported
    /// ready: finish whatever a caller must not race before that happens.
    /// Today that is the agent refresh `up` or `vm start` deferred to this
    /// handshake (§19.4), which restarts the agent a caller would otherwise
    /// already be using.
    ///
    /// Returns when readiness may be reported, success or not, and is bounded:
    /// a machine is never held unready forever on the lab's account. The
    /// default owes nothing.
    async fn before_ready(&self, _machine: &str) {}

    /// The emulator is up with its CPUs still paused and nothing yet drawn
    /// (D20): the one moment a console recorder can attach and be sure of
    /// the first frame. An error here fails the start. The default owes
    /// nothing.
    async fn before_resume(&self, _machine: &str) -> Result<()> {
        Ok(())
    }

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
    /// The guest OS profile this machine effectively runs (§5.3) — a VM's may
    /// have come from its template rather than from its own block, which only
    /// the machine can answer. `None` when nothing named one.
    ///
    /// Asked for by the profile-sourced layer of a resolution the machine
    /// itself takes no part in — the dev defaults in [`crate::dev`] are the
    /// first — so the answer is the profile, not the thing resolved from it.
    fn profile(&self) -> Option<String>;
    /// What the guest clock does across a restore (§7.3): the machine's own
    /// declaration, never inherited.
    fn clock(&self) -> model::Clock {
        model::Clock::default()
    }
    fn nics(&self) -> &[model::Nic];
    fn macs(&self) -> &[MacAddr];
    /// The identities this machine declares (§19.2), in declaration order.
    /// Reported by the machine and not looked up in the lab file: a caller
    /// holding a machine has everything about it.
    fn logins(&self) -> &[model::Login];
    /// Host socket re-exposing one agent terminal session as a raw byte pipe.
    fn term_session_sock(&self, id: u32) -> PathBuf;

    /// The virtiofsd the host running this machine serves shares with
    /// ([`Hypervisor::virtiofsd`](super::hypervisor::Hypervisor::virtiofsd)).
    ///
    /// Exposed here because the decision it feeds is taken twice: once per
    /// machine as it starts (a vhost-user-fs device cannot hotplug, so the
    /// transport is fixed then) and once for the lab, when the share plan
    /// works out what `smbd` must export. Both must read the same host, or a
    /// substituted one disagrees with itself.
    fn virtiofsd(&self) -> Option<crate::qemu::virtiofsd::Virtiofsd>;

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
    /// Every qcow2 the machine writes, in the order its snapshots address
    /// them: a VM's `disk0.qcow2` then each `disk-<name>.qcow2`, a
    /// container's `scratch.qcow2`. What a freeze freezes (§21.1).
    fn disk_paths(&self) -> Vec<PathBuf>;

    /// External binaries that must be on PATH before this machine can boot,
    /// so a missing package surfaces as one clear error rather than a spawn
    /// failure mid-`up`.
    fn required_binaries(&self) -> Vec<String>;

    /// Refuse when the disks this machine already has cannot boot under its
    /// declaration — a VM's clone of a template the lab file no longer names
    /// (§7.1). `up` asks every target before anything downloads or boots, so
    /// the refusal is the whole of what it did. A container has no such rule
    /// (an edited `image =` line just drops its pin, §18), hence the default.
    fn check_disks(&self) -> Result<()> {
        Ok(())
    }

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
    /// The direct seal for guests with no ACPI (DOS, Win 3.x): it skips the
    /// ladder's agent and ACPI rungs, which such a guest ignores, and goes
    /// straight to the clean quit the ladder only reaches after their
    /// timeouts. The default falls back to the
    /// ladder for a machine with no such control channel.
    async fn poweroff(&self) -> Result<()> {
        self.stop(false).await?;
        self.wait_state(PowerState::Stopped, POWEROFF_SETTLE).await
    }

    /// Eject the medium from this machine's install CD-ROM — the drive a
    /// VM's `cdrom =` (or a template build's `source "iso"`) attaches — and
    /// no other drive. Forced, since an installer locks the tray. The medium
    /// stays out until the emulator restarts; a guest reboot inside the same
    /// run keeps it out, which is the point: an installer that reboots from
    /// CD falls through to the disk. Ejecting twice is not an error.
    ///
    /// The default is the refusal for a machine with no such drive.
    async fn eject_cdrom(&self) -> Result<()> {
        Err(no_install_cdrom(self.name()))
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

    /// What `up` says about this machine before it boots: something it was
    /// declared with that this launch will not give it, said rather than
    /// refused (§5.2). Nothing, unless the kind has a reason.
    fn launch_warnings(&self) -> Vec<String> {
        Vec::new()
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
    fn pending_first_boot(&self) -> Option<FirstBootProvision> {
        None
    }

    /// Record that the first-boot provision completed and report ready.
    async fn first_boot_done(&self) -> Result<()> {
        Ok(())
    }

    // ---- agent ------------------------------------------------------------

    /// The vmlab-agent channel, connecting on first use.
    async fn agent(&self) -> Result<AgentHandle>;
    /// The emulator's host process id while it runs — what `machine.stats`
    /// reads the host CPU figure from ([`super::host_cpu`]).
    async fn emulator_pid(&self) -> Option<u32> {
        None
    }
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

    /// Where this machine's agent came from (§19.4). The default is the
    /// ordinary case — an agent sealed into whatever the machine boots — so a
    /// machine whose agent ships with the host says so rather than every other
    /// machine having to say it does not.
    fn agent_origin(&self) -> AgentOrigin {
        AgentOrigin::Image
    }

    /// The agent stamp this machine's artefact sealed at build (§6.1) —
    /// `None` where it sealed none: a scratch machine, a template built with
    /// `agent = false`, or one whose agent is not sealed into anything it
    /// boots. What `up`'s agent refresh (§19.4) falls back on for an agent
    /// whose handshake carries no stamp.
    fn sealed_agent_version(&self) -> Option<String> {
        None
    }

    /// Whether this machine's agent rides the legacy tier's ISA serial
    /// channel (§7.4) — the C or HolyC agent, which nothing can replace over
    /// its own channel.
    fn agent_on_legacy_tier(&self) -> bool {
        false
    }

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

    /// An opaque string that is the same for the whole of one guest boot and
    /// different after any reboot ([`super::boot_id`]). Read over the agent's
    /// `exec`, so it needs the agent answering, and fails by name on a family
    /// with no boot identity to read.
    async fn boot_id(&self) -> Result<String> {
        super::boot_id::read(self.as_machine()).await
    }

    /// Wait until [`boot_id`](Machine::boot_id) answers something other than
    /// `since`, and return it — the reboot signal [`agent_answering`]
    /// (Machine::agent_answering) cannot give, since a guest that reboots
    /// between two pings answers the next one through the new boot's agent.
    /// Failed probes while the guest is down are expected and tolerated.
    async fn wait_rebooted(&self, since: &str, timeout: Duration) -> Result<String> {
        super::boot_id::wait_changed(self.name(), since, timeout, super::boot_id::POLL, || {
            self.boot_id()
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
        retry_handshake(timeout, || self.agent()).await
    }

    // ---- replacing the agent (§19.4) --------------------------------------

    /// Hold this machine's agent channel for a replacement of the agent
    /// itself (§19.4): from here until the returned guard drops, every other
    /// caller of [`agent`](Machine::agent) waits — bounded by
    /// [`AGENT_REPLACEMENT_WAIT`] — instead of being handed a channel the
    /// replacement is about to restart out from under it, or a handshake
    /// with the agent being replaced. The push holds it from before the
    /// binary is sent until the new agent has answered a handshake under its
    /// own instance, so what a waiter gets afterwards is the new agent.
    ///
    /// A second replacement asked for while one is held waits for the first.
    /// The default holds nothing: a machine whose agent is never replaced
    /// (a container's ships with the host) has nothing to gate.
    async fn begin_agent_replacement(&self) -> AgentReplacement {
        AgentReplacement::none()
    }

    /// [`agent`](Machine::agent) for the holder of an [`AgentReplacement`],
    /// which does not wait on the gate it holds. The replacement reaches the
    /// agent it is replacing, and then the one that replaced it, through
    /// this and nothing else.
    async fn agent_during_replacement(&self) -> Result<AgentHandle> {
        self.agent().await
    }

    /// [`wait_agent`](Machine::wait_agent) over
    /// [`agent_during_replacement`](Machine::agent_during_replacement).
    async fn wait_agent_during_replacement(&self, timeout: Duration) -> Result<AgentHandle> {
        retry_handshake(timeout, || self.agent_during_replacement()).await
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

    /// The hypervisor this machine runs on (PRD §22): its own `hypervisor`,
    /// else the host config's default for its kind. Recorded on every
    /// snapshot, because an online one restores only under the hypervisor
    /// that took it. QEMU unless the machine says otherwise.
    fn hypervisor(&self) -> crate::config::hypervisor::HypervisorKind {
        crate::config::hypervisor::HypervisorKind::Qemu
    }

    // ---- host USB passthrough (PRD §5.2.1) ---------------------------------

    /// Whether this machine can take a host USB device now: running, and
    /// booted with the controller one attaches to. The default is the
    /// container's answer — a micro-VM has no USB bus to offer.
    async fn usb_ready(&self) -> Result<()> {
        bail!(crate::usb::container_refusal(self.name()))
    }

    /// Hot-plug the device `plan` decided on, and track it.
    async fn usb_attach(&self, plan: &crate::usb::AttachPlan) -> Result<crate::usb::Attachment> {
        let _ = plan;
        bail!(crate::usb::container_refusal(self.name()))
    }

    /// Unplug the attachment with qdev id `qdev_id` and stop tracking it —
    /// tracking goes even when QEMU no longer knows the device, so a removal
    /// that cannot complete never leaves a phantom attachment behind.
    async fn usb_detach(&self, qdev_id: &str) -> Result<crate::usb::Attachment> {
        let _ = qdev_id;
        bail!(crate::usb::container_refusal(self.name()))
    }

    /// The host devices this machine holds now. Runtime only: a stop clears
    /// them.
    async fn usb_attached(&self) -> Vec<crate::usb::Attachment> {
        Vec::new()
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
    /// window, recording how far it got in `report` for `up` to print. A
    /// no-op for machines whose guest mounts its own folders (a container's
    /// init does it from the spec it was handed).
    async fn mount_shares(
        self: Arc<Self>,
        lab: Arc<dyn LabServices>,
        report: Arc<super::mount_report::MountReport>,
    ) {
        let _ = (lab, report);
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

/// Template payload compiled by the host before a machine becomes ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstBootProvision {
    pub(crate) template: String,
    pub(crate) script: crate::scripting::EmbeddedWscript,
}

/// Blanket helpers that need `Arc<dyn Machine>` rather than `&dyn Machine`.
impl dyn Machine {
    /// Everything this machine can do beyond the universal operations.
    /// Agent features come from a live handshake, so a machine that is up but
    /// not yet answering reports an empty list rather than a guess.
    pub async fn capabilities(self: &Arc<Self>) -> Capabilities {
        Capabilities {
            kind: self.kind(),
            display: self.clone().display().is_some(),
            console_log: self.console_log(1).is_some(),
            reboot: self.can_reboot(),
            healthcheck: self.has_healthcheck(),
            agent: self.agent_features().await,
        }
    }

    /// What this machine's agent advertised at handshake, or nothing at all
    /// when none is answering — which is a live fact about the machine, not a
    /// property of its kind.
    pub async fn agent_features(self: &Arc<Self>) -> Vec<String> {
        match self.agent().await {
            Ok(handle) => handle.info().features,
            Err(_) => Vec::new(),
        }
    }

    /// This machine's line in `status`.
    ///
    /// `cached` and `dev` are left empty here: whether a registry download is
    /// still pending, and which machine is the lab's dev machine, are both
    /// lab-level knowledge, and [`super::lab::LabRuntime::status`] fills them
    /// in.
    pub async fn status(self: &Arc<Self>) -> MachineStatus {
        let ready = self.is_ready().await;
        let state = self.state().await;
        let assigned = if ready {
            self.guest_ips()
                .await
                .unwrap_or_else(|_| vec![None; self.nics().len()])
        } else {
            vec![None; self.nics().len()]
        };
        // After the address lookup, which asks the agent: an agent that
        // restarted in the guest is handshaken again on the way, so the stamp
        // this line reports is the one answering now.
        let detail = self.status_detail().await;
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
            cached: true,
            // Lab-level, like `cached`: `@dev` is per-machine but *the*
            // default dev machine depends on what the rest of the lab
            // declares, so `LabRuntime::status` fills this in (§19.1).
            dev: None,
            // Lab-level too: divergence is recorded in the lab's persisted
            // state, which the machine does not hold (§19.4).
            agent_diverged: false,
            // And the recording: the lab daemon owns it (§11.1).
            recording: None,
            hypervisor: self.hypervisor(),
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

/// [`Machine::eject_cdrom`]'s refusal for a machine that has no install
/// CD-ROM, naming it and saying which machines do.
/// Named by capability, like every other refusal on the interface.
pub(super) fn no_install_cdrom(name: &str) -> anyhow::Error {
    anyhow!(
        "machine `{name}` has no install CD-ROM to eject — only a VM with a \
         `cdrom` attachment, or a template build from `source \"iso\"`, has one"
    )
}

/// [`Machine::poweroff`] for a machine a hypervisor is running: a clean
/// [`Control::quit`](super::hypervisor::Control::quit), then wait for the exit
/// monitor to settle.
///
/// The quit is expected to fail — QEMU exits, so the connection drops
/// mid-request — and a machine that is already down has no channel at all. The
/// power state is the only answer that matters, so it is the only thing
/// checked.
pub(super) async fn quit_and_settle(
    m: &dyn Machine,
    control: Option<Arc<dyn super::hypervisor::Control>>,
) -> Result<()> {
    if let Some(control) = control {
        let _ = control.quit().await;
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

/// How long a caller of [`Machine::agent`] waits for a replacement of the
/// agent in progress ([`Machine::begin_agent_replacement`]) before giving up.
/// A push stages the binary, swaps it, restarts the service and waits up to
/// two minutes for the new agent's handshake; this is that, with room, so a
/// waiter outlasts a slow guest and still fails by name on a wedged one.
pub const AGENT_REPLACEMENT_WAIT: Duration = Duration::from_secs(240);

/// The agent of one machine is being replaced (§19.4) — what
/// [`Machine::begin_agent_replacement`] hands the push, and what every
/// other [`Machine::agent`] call waits on until it drops. A guard rather
/// than a flag: a push that fails, or whose task is gone, reopens the
/// channel by dropping it, so nothing can leave the machine gated for good.
#[must_use = "the agent channel is held only while this is alive"]
pub struct AgentReplacement {
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl AgentReplacement {
    /// Holds nothing — the gate of a machine whose agent is never replaced.
    pub fn none() -> Self {
        Self { _permit: None }
    }
}

/// `connect` retried on transient handshake failures until `timeout` — the
/// loop behind [`Machine::wait_agent`], shared with the replacement's own
/// reconnect so the two cannot drift.
async fn retry_handshake<F, Fut>(timeout: Duration, mut connect: F) -> Result<AgentHandle>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<AgentHandle>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match connect().await {
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

/// What an operation that needs the guest says when the guest is asleep
/// (ACPI S3): the state by name, how to wake it, and how to stop it sleeping.
///
/// A [`Conflict`](crate::proto::ErrorCode::Conflict) — the machine exists and
/// its state forbids the request — carried as a `CommandError` so a plain `?`
/// in a handler keeps the code. Deliberately not an [`AgentUnavailable`]:
/// waiting does not wake a sleeping guest, so nothing should retry it, and
/// [`Machine::wait_agent`] treats what it cannot classify as permanent.
pub fn suspended_error(machine: &str) -> anyhow::Error {
    crate::proto::CommandError::conflict(format!(
        "{machine} is suspended: the guest went to sleep (ACPI S3) and nothing in it answers \
         until it is woken — wake it with `vmlab vm start {machine}`, or set \
         `prevent_sleep = true` on the VM to keep it awake"
    ))
    .into()
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

/// Whether a connect honours the failed-handshake backoff
/// ([`AgentSlot::dial`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backoff {
    /// Answer from a failure in the last 30 s without dialling: spares a
    /// guest with no agent the handshake timeout on every call.
    Honour,
    /// Always dial: a replacement expects its own restart to fail a
    /// handshake, and must see the new agent the moment it answers.
    Skip,
}

/// The cached-connection half of a machine's agent channel, shared by both
/// adapters: one live handle, plus when the last handshake failed so
/// agent-less guests don't pay the timeout on every call.
///
/// The "no agent answering" advice stays with the adapter — a VM wants a
/// template rebuild, a container wants its guest boot asset rebuilt — so
/// [`connect`](Self::connect) takes the hint rather than owning one.
pub(super) struct AgentSlot {
    handle: tokio::sync::Mutex<Option<AgentHandle>>,
    failed_at: tokio::sync::Mutex<Option<std::time::Instant>>,
    /// The version the last successful handshake reported, kept apart from
    /// `handle` so a status read never waits behind a connect in progress.
    answered: std::sync::Mutex<Option<String>>,
    /// One permit, held by a replacement of the agent (§19.4) for as long as
    /// it runs. [`connect`](Self::connect) waits for it to be free before it
    /// hands a channel out, so nobody lands on the agent being replaced or on
    /// the connection the replacement is about to shut down; the replacement
    /// itself reconnects through
    /// [`connect_during_replacement`](Self::connect_during_replacement).
    replacing: Arc<tokio::sync::Semaphore>,
    /// Run on every fresh handshake before the handle is handed out: the
    /// guest-clock check of §7.3 ([`super::boot_clock`]), installed by the
    /// machine at each start.
    on_handshake: std::sync::Mutex<Option<super::boot_clock::HandshakeHook>>,
}

impl Default for AgentSlot {
    fn default() -> Self {
        Self {
            handle: tokio::sync::Mutex::new(None),
            failed_at: tokio::sync::Mutex::new(None),
            answered: std::sync::Mutex::new(None),
            replacing: Arc::new(tokio::sync::Semaphore::new(1)),
            on_handshake: std::sync::Mutex::new(None),
        }
    }
}

impl AgentSlot {
    /// Install what every fresh handshake runs from now on (see
    /// `on_handshake`), replacing the last start's.
    pub fn set_handshake_hook(&self, hook: super::boot_clock::HandshakeHook) {
        *self.on_handshake.lock_recover() = Some(hook);
    }

    /// Run the handshake hook on a handle that just handshook. Called with
    /// the slot's handle lock held, so a concurrent caller waits for it
    /// rather than racing a clock step; the hook talks to the handle it is
    /// given and never back through the slot.
    async fn handshook(&self, handle: &AgentHandle) {
        let hook = self.on_handshake.lock_recover().clone();
        if let Some(hook) = hook {
            hook(handle.clone()).await;
        }
    }

    /// Connect (or reuse), with the token handshake. A guest reboot or an
    /// agent restart leaves the old connection open but talking to a fresh
    /// agent instance, so a live handle is pinged before it is handed out, and
    /// a ping answered by another instance counts as no answer (see
    /// [`AgentHandle::ping`]): the handle is re-handshaken, so the version and
    /// features it reports are the running agent's. A dead one is shut down
    /// explicitly, because QEMU's chardev serves exactly one client and a
    /// half-dead connection would block every future connect.
    ///
    /// Waits first for any replacement of the agent in progress, so what it
    /// hands out afterwards is the agent that replaced it.
    pub async fn connect(&self, name: &str, sock: &Path, hint: &str) -> Result<AgentHandle> {
        self.wait_replaced(name).await?;
        self.dial(name, sock, hint, Backoff::Honour).await
    }

    /// Take the gate every other [`connect`](Self::connect) waits on, for a
    /// replacement of the agent (§19.4). Waits for a replacement already in
    /// progress rather than running beside it.
    pub async fn begin_replacement(&self) -> AgentReplacement {
        let permit = Arc::clone(&self.replacing)
            .acquire_owned()
            .await
            .expect("the replacement semaphore is never closed");
        AgentReplacement {
            _permit: Some(permit),
        }
    }

    /// Whether a replacement of the agent holds the gate right now.
    pub fn replacing(&self) -> bool {
        self.replacing.available_permits() == 0
    }

    /// Wait for a replacement in progress to finish, up to
    /// [`AGENT_REPLACEMENT_WAIT`]; past that, a handshake failure naming
    /// the replacement, which is transient (the replacement may yet end).
    async fn wait_replaced(&self, name: &str) -> Result<()> {
        if !self.replacing() {
            return Ok(());
        }
        match tokio::time::timeout(AGENT_REPLACEMENT_WAIT, self.replacing.acquire()).await {
            Ok(permit) => {
                drop(permit);
                Ok(())
            }
            Err(_) => Err(AgentUnavailable::Handshake {
                machine: name.to_string(),
                message: format!(
                    "its agent is still being replaced after {}s; the replacement did not \
                     come back (`vmlab vm restart {name}` loads whatever is installed)",
                    AGENT_REPLACEMENT_WAIT.as_secs()
                ),
            }
            .into()),
        }
    }

    /// [`connect`](Self::connect) without the wait on the replacement gate:
    /// what the holder of an [`AgentReplacement`] reconnects through.
    ///
    /// It also skips the failed-handshake backoff. A replacement restarts the
    /// agent on purpose, so a handshake that lands in the restart's gap is
    /// expected to fail, and the next one must actually dial: answering it
    /// from the backoff would hold the push on a failure that stopped being
    /// true the moment the new agent came up, until its wait ran out.
    pub async fn connect_during_replacement(
        &self,
        name: &str,
        sock: &Path,
        hint: &str,
    ) -> Result<AgentHandle> {
        self.dial(name, sock, hint, Backoff::Skip).await
    }

    /// Reuse the cached handle if it still answers, else handshake afresh —
    /// unless `backoff` honours a handshake that failed in the last 30 s.
    async fn dial(
        &self,
        name: &str,
        sock: &Path,
        hint: &str,
        backoff: Backoff,
    ) -> Result<AgentHandle> {
        let mut agent = self.handle.lock().await;
        if let Some(handle) = agent.as_ref() {
            if handle.ping(Duration::from_secs(2)).await {
                return Ok(handle.clone());
            }
            if let Some(dead) = agent.take() {
                dead.shutdown().await;
            }
        }
        if backoff == Backoff::Honour {
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
                self.note_answered(&handle);
                *agent = Some(handle.clone());
                self.handshook(&handle).await;
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
        // Not while it is being replaced: a probe that handshook with the
        // agent on its way out would cache a handle the replacement's own
        // reconnect then mistook for the agent that came back.
        if self.replacing() {
            return false;
        }
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
                self.note_answered(&handle);
                *agent = Some(handle.clone());
                self.handshook(&handle).await;
                true
            }
            Err(_) => false,
        }
    }

    fn note_answered(&self, handle: &AgentHandle) {
        *self.answered.lock_recover() = Some(handle.info().agent_version);
    }

    /// The version the agent reported at the last handshake since this
    /// machine started — the agent actually running, which after a refresh or
    /// a repair is not the one its template sealed (§19.4). `None` until an
    /// agent has answered. Never waits.
    pub fn answered_version(&self) -> Option<String> {
        self.answered.lock_recover().clone()
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
        *self.answered.lock_recover() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The failed-handshake backoff spares ordinary callers, never a
    /// replacement.** After a failed handshake, a plain connect inside the
    /// backoff answers with the hint alone and does not dial; a reconnect
    /// during a replacement dials anyway, which its error shows by naming
    /// the socket it tried.
    #[tokio::test]
    async fn a_replacement_reconnect_dials_through_the_handshake_backoff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("absent.sock");
        let slot = AgentSlot::default();
        let hint = "no agent here";

        let first = slot
            .connect("m1", &sock, hint)
            .await
            .err()
            .expect("nothing listens");
        assert!(
            format!("{first:#}").contains("connecting agent socket"),
            "{first:#}"
        );

        let backed_off = slot
            .connect("m1", &sock, hint)
            .await
            .err()
            .expect("backed off");
        assert_eq!(
            format!("{backed_off:#}"),
            format!("m1: {hint}"),
            "answered from the backoff"
        );

        let dialled = slot
            .connect_during_replacement("m1", &sock, hint)
            .await
            .err()
            .expect("still nothing listens");
        assert!(
            format!("{dialled:#}").contains("connecting agent socket"),
            "a replacement dials past the backoff: {dialled:#}"
        );
    }

    /// A caller held by a replacement that never ends is not held for ever:
    /// past [`AGENT_REPLACEMENT_WAIT`] it fails by name, as a handshake
    /// failure — transient, because the replacement may yet come back — that
    /// says what it waited on and what to do about it.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_that_never_ends_fails_its_waiters_by_name() {
        let slot = AgentSlot::default();
        let held = slot.begin_replacement().await;
        assert!(slot.replacing());
        let sock = std::env::temp_dir().join("vmlab-no-such-agent.sock");
        let err = match slot.connect("dc01", &sock, "hint").await {
            Ok(_) => panic!("held past the wait"),
            Err(e) => e,
        };
        let reason = AgentUnavailable::of(&err).expect("typed");
        assert!(reason.is_transient(), "{reason}");
        let msg = format!("{err:#}");
        assert!(msg.contains("dc01"), "{msg}");
        assert!(msg.contains("still being replaced"), "{msg}");
        assert!(msg.contains("vmlab vm restart dc01"), "{msg}");
        drop(held);
        assert!(!slot.replacing(), "released with the guard");
    }

    /// A second replacement asked for while one is held waits for the first
    /// rather than running beside it — two pushes into one guest would race
    /// each other's swap and restart.
    #[tokio::test]
    async fn replacements_of_one_agent_take_turns() {
        let slot = Arc::new(AgentSlot::default());
        let first = slot.begin_replacement().await;
        let second = tokio::spawn({
            let slot = Arc::clone(&slot);
            async move { slot.begin_replacement().await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!second.is_finished(), "waits for the first");
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("takes its turn")
            .expect("task");
        assert!(slot.replacing(), "and holds the gate in turn");
        drop(second);
        assert!(!slot.replacing());
    }

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
