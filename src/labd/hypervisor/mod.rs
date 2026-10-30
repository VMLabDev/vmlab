//! The **Hypervisor** seam: the host's ability to actually run a machine.
//!
//! Deciding a machine should run and making the host run it used to be the
//! same 260 lines. `VmInstance::start` reached straight for `spawn_swtpm`,
//! `virtiofsd::spawn`, `Proc::spawn_with_fds` and `QmpClient::connect`, so the
//! parts with real logic — the power-state machine, the exit monitor that
//! classifies *why* the emulator ended, readiness gating and the stop ladder —
//! could only be exercised by booting real QEMU on a real host.
//!
//! Everything that touches the host sits below this seam; everything that
//! decides what the machine's state *means* sits above it. The seam is stated
//! in terms of **what running means** — the machine is up ([`Process`]), it
//! answers control ([`Control`]), it exited for this reason
//! ([`classify_exit`]) — rather than which executable was launched, and it
//! hands back handle types it owns rather than the QEMU module's concrete
//! `Proc` and `QmpClient`. That is what lets an adapter be entirely
//! in-memory (ADR-0001).
//!
//! Two adapters, both live: [`Qemu`] runs the real thing in production, and
//! [`fake::FakeHypervisor`] runs a scripted machine with no subprocess at
//! all, which is what every lifecycle test drives.
//!
//! What a running machine can do beyond the lifecycle — online snapshots,
//! scripted input, framebuffer capture, device hot-plug — is a set of
//! **capabilities** [`Control`] reports, each `None` where the adapter has
//! none. A caller asks for the one it needs and refuses when it is absent; it
//! never reaches past the seam for a protocol client (ADR-0020).
//!
//! The seam receives a typed [`Launch`] — the resolved machine and the
//! runtime paths its caller computed — and each adapter composes its own
//! launch from it. QEMU's composes an argv by calling the same pure builders
//! in [`crate::qemu`] (ADR-0003, ADR-0008); resolving hardware stays above
//! the seam, and only turning the result into a command line moved below it
//! (ADR-0020, amending ADR-0001).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::guest_asset::GuestAsset;
use crate::qemu::container::ContainerVmPaths;
use crate::qemu::process::{ChildEnv, ChildFd};
use crate::qemu::{Accel, ResolvedContainer, ResolvedVm, VmPaths};

mod qemu;
pub use qemu::Qemu;
#[cfg(test)]
pub(crate) use qemu::QmpCapabilities;

#[cfg(test)]
pub(crate) mod fake;

/// Something the host is running on the machine's behalf: the emulator
/// itself, a software TPM, a filesystem daemon.
///
/// Only what the lifecycle above the seam actually needs — is it up, what did
/// it exit with, wait for that, end it. Deliberately not [`crate::qemu::Proc`]:
/// tying the seam to a real OS process is what forced the previous fake to
/// spawn `/bin/sh`.
#[async_trait::async_trait]
pub trait Process: Send + Sync + 'static {
    fn is_running(&self) -> bool;

    /// The exit status once there is one, in the shape
    /// [`std::process::ExitStatus`] prints (`exit status: 0`, `signal: 9`).
    /// Free text, because that is all [`classify_exit`] reads and all a user
    /// ever sees.
    fn exit_status(&self) -> Option<String>;

    /// Wait for exit. `Ok(status)` on exit, `Err` on timeout.
    async fn wait_exit(&self, timeout: Duration) -> Result<String>;

    /// End it now — the last rung of every stop ladder.
    async fn kill(&self);
}

/// A running machine's control channel: what the lifecycle asks a machine to
/// do once it is up, what it can learn about how the guest is behaving, and
/// which capabilities this machine has.
///
/// The QMP client sits *behind* this rather than being handed out, so an
/// adapter does not have to stand up a QMP server to be usable.
#[async_trait::async_trait]
pub trait Control: Send + Sync + 'static {
    /// Release the CPUs. The emulator starts paused so the caller can
    /// subscribe to what it needs before the guest runs an instruction.
    async fn resume(&self) -> Result<()>;

    /// Stop the CPUs, leaving the machine up — what an online restore does
    /// before it loads a snapshot over the running guest.
    async fn pause(&self) -> Result<()>;

    /// Ask the guest to power itself down (ACPI) — a stop-ladder rung, not a
    /// kill. Success means the request was delivered, not that the guest
    /// obeyed; the caller waits on [`Process::wait_exit`] for that.
    async fn powerdown(&self) -> Result<()>;

    /// Tell the emulator itself to go.
    async fn quit(&self) -> Result<()>;

    /// Whether the guest has powered itself off since this machine came up.
    /// Feeds [`classify_exit`], where the same exit status reads as a clean
    /// shutdown or a crash depending on this answer.
    fn guest_shutdown(&self) -> bool;

    /// Whether the guest is asleep — suspended to RAM (ACPI S3), the emulator
    /// alive with its vCPUs stopped — as a channel that changes on every
    /// transition. QEMU's q35 machine advertises S3, and a Windows client
    /// edition takes it after an idle timeout.
    ///
    /// `None` from an adapter whose guests cannot sleep. Subscribed before
    /// [`resume`](Control::resume) like the shutdown watch, so a guest that
    /// sleeps the moment it runs is not missed.
    fn sleep_watch(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        None
    }

    /// Wake a sleeping guest (`system_wakeup`). Success means the request was
    /// delivered; the guest is awake once [`sleep_watch`](Control::sleep_watch)
    /// says so.
    async fn wakeup(&self) -> Result<()> {
        anyhow::bail!("this machine cannot sleep, so there is nothing to wake")
    }

    /// Online snapshots, when this machine can take them. `None` leaves the
    /// caller's refusal standing; it does not fall back to anything.
    fn snapshots(&self) -> Option<Arc<dyn Snapshots>> {
        None
    }

    /// Scripted keyboard and pointer input over the control channel.
    fn input(&self) -> Option<Arc<dyn Input>> {
        None
    }

    /// Framebuffer capture over the control channel.
    fn frames(&self) -> Option<Arc<dyn Frames>> {
        None
    }

    /// Hot-plugging host devices into the running machine.
    fn hotplug(&self) -> Option<Arc<dyn Hotplug>> {
        None
    }

    /// Removable media in the running machine's drives.
    fn media(&self) -> Option<Arc<dyn Media>> {
        None
    }
}

/// The **snapshots** capability: save, load and delete a snapshot of a
/// running machine — RAM and device state into `vmstate`, every disk in
/// `devices` captured at the same instant (PRD §7.3).
///
/// The names are the block-device nodes the machine was launched with
/// (`disk0`…, a container's `scratch`). Each operation completes when it is
/// done; there is no job to poll above the seam.
#[async_trait::async_trait]
pub trait Snapshots: Send + Sync {
    async fn save(&self, name: &str, vmstate: &str, devices: &[&str]) -> Result<()>;

    /// Load over the running guest. The caller pauses the machine first and
    /// resumes it after, through [`Control`], so it can act in between.
    async fn load(&self, name: &str, vmstate: &str, devices: &[&str]) -> Result<()>;

    async fn delete(&self, name: &str, devices: &[&str]) -> Result<()>;
}

/// The **input** capability: keyboard and absolute pointer, in QEMU `qcode`
/// key names and pixel coordinates against a `width`×`height` screen.
#[async_trait::async_trait]
pub trait Input: Send + Sync {
    /// Press and release `keys` together, as one chord.
    async fn send_keys(&self, keys: &[&str]) -> Result<()>;

    async fn pointer_to(&self, x: u32, y: u32, width: u32, height: u32) -> Result<()>;

    /// Press (`down`) or release a mouse button (`left`, `middle`, `right`).
    async fn button(&self, button: &str, down: bool) -> Result<()>;
}

/// The **frames** capability: the current framebuffer, written as an image
/// file at a path the caller chose and reads back.
#[async_trait::async_trait]
pub trait Frames: Send + Sync {
    async fn screendump(&self, to: &Path) -> Result<()>;
}

/// The **hot-plug** capability: host devices attached to and detached from
/// the running machine (PRD §5.2.1).
#[async_trait::async_trait]
pub trait Hotplug: Send + Sync {
    /// Attach the host USB device `plan` resolved.
    async fn attach_usb(&self, plan: &crate::usb::AttachPlan) -> Result<()>;

    /// Detach the device with id `id`, waiting up to `wait` for the guest to
    /// let go. A device the machine no longer has is already detached, so
    /// that is success too.
    async fn detach(&self, id: &str, wait: Duration) -> Result<()>;
}

/// The **media** capability: removable media in a running machine's drives.
#[async_trait::async_trait]
pub trait Media: Send + Sync {
    /// Eject the install CD-ROM a VM with a `cdrom` attachment boots from,
    /// even while the guest holds it, and leave the tray closed and empty.
    async fn eject_install_cdrom(&self) -> Result<()>;
}

/// The guest channels a machine answers on once it is up.
///
/// QEMU binds these from the argv, so its adapter ignores them; an in-memory
/// adapter has to stand them up itself. Naming them here rather than leaving
/// them buried in the composed launch is what lets the fake speak the real
/// ctl and agent protocols over a socket, the way the agent client's own
/// tests already do.
#[derive(Debug, Clone, Default)]
pub struct GuestChannels {
    /// `vmlab.agent.0` — the vmlab-agent port every machine kind carries.
    pub agent: PathBuf,
    /// `vmlab.ctl.0` — cinit's ndjson control channel. Containers only.
    pub ctl: Option<PathBuf>,
}

/// What to launch: the resolved machine and the runtime paths its caller
/// computed for this start — never a command line. Each adapter composes its
/// own launch from it (ADR-0020).
pub enum Launch {
    /// A full VM (§5).
    Vm {
        lab: String,
        resolved: Box<ResolvedVm>,
        paths: Box<VmPaths>,
        /// The accelerator [`crate::qemu::plan_accel`] decided on.
        accel: Accel,
        /// The host GPU's own emulator build, run in place of the stock one
        /// (`gpu {}`'s `qemu_binary`, §5.2). `None` runs the stock emulator.
        emulator: Option<PathBuf>,
    },
    /// A container micro-VM (§18), booting the host's guest asset.
    Container {
        lab: String,
        resolved: Box<ResolvedContainer>,
        paths: Box<ContainerVmPaths>,
    },
}

/// A prepared machine start.
pub struct LaunchSpec {
    /// Process label for logs and diagnostics (`qemu:dc01`).
    pub label: String,
    /// The machine to launch.
    pub machine: Launch,
    /// The emulator's environment changes — the Vulkan loader variables a
    /// `gpu {}` owns (`qemu::hostgpu::emulator_env`).
    pub env: ChildEnv,
    /// Where the emulator's stdout/stderr goes.
    pub log: PathBuf,
    /// The QMP socket the emulator will bind, connected once it is up.
    pub qmp_sock: PathBuf,
    /// Pre-opened descriptors installed at fixed numbers in the child (tap
    /// NIC fds).
    pub fds: Vec<ChildFd>,
    /// The guest channels this machine exposes — see [`GuestChannels`].
    pub channels: GuestChannels,
}

/// A machine the host is now running.
pub struct Running {
    pub proc: Arc<dyn Process>,
    pub control: Arc<dyn Control>,
}

/// The host operations that turn a prepared machine into a running one.
///
/// Every method reaches the host and can fail; that is the point. A caller
/// above this seam handles the failures, and a test can produce them without
/// a hypervisor.
#[async_trait::async_trait]
pub trait Hypervisor: Send + Sync + 'static {
    /// Start a software TPM and wait for its control socket to appear.
    /// Returning `Ok` means the socket is bound — the emulator connects to it
    /// at startup and fails hard if it is not there yet.
    async fn start_tpm(
        &self,
        machine: &str,
        state_dir: &Path,
        ctrl_sock: &Path,
        log: &Path,
    ) -> Result<Arc<dyn Process>>;

    /// The virtiofsd this host serves shares with, and what it accepts —
    /// `None` when it has no usable one. A capability probe rather than an
    /// operation, but the same kind of fact as the rest of this trait: what
    /// the host running the machine can do. Decides whether
    /// `transport = "auto"` shares and container volumes attach as
    /// vhost-user-fs devices or fall back to SMB/CIFS, and whether a machine
    /// with those devices can be snapshotted online.
    fn virtiofsd(&self) -> Option<crate::qemu::virtiofsd::Virtiofsd>;

    /// Start one virtiofsd exporting `shared_dir` on `socket`. These are
    /// vhost-user backends, so they must be listening before the emulator
    /// spawns.
    async fn start_virtiofsd(
        &self,
        machine: &str,
        socket: &Path,
        shared_dir: &Path,
        readonly: bool,
        log: &Path,
    ) -> Result<Arc<dyn Process>>;

    /// Compose the launch [`LaunchSpec::machine`] describes, spawn it, and
    /// return once it answers control. CPUs stay paused — the caller resumes
    /// them once it has subscribed to the events it needs. A launch that
    /// cannot be composed fails here, before anything is spawned.
    async fn start_emulator(&self, spec: LaunchSpec) -> Result<Running>;

    /// The kernel + initramfs this host boots a container micro-VM from
    /// (`guest/build-asset.sh` output). A host probe like the rest of this
    /// trait: where the asset lives is a property of the machine doing the
    /// running, not of the container being run.
    fn guest_asset(&self, arch: &str) -> Result<GuestAsset>;
}

/// What one QMP event says about whether the guest is asleep: `Some(true)`
/// as it suspends, `Some(false)` for anything that means its vCPUs run again,
/// `None` for an event that says nothing either way.
///
/// `WAKEUP` is the ordinary way out of S3. A reset or a `cont` also leaves
/// the guest running whatever state it was in, and QEMU emits no `WAKEUP` for
/// either, so they count as waking too — a watch that missed them would
/// report a running guest asleep for the rest of its life.
pub fn sleep_transition(event: &str) -> Option<bool> {
    match event {
        "SUSPEND" => Some(true),
        "WAKEUP" | "RESET" | "RESUME" => Some(false),
        _ => None,
    }
}

/// Why a machine left the Running state (PRD §8.1).
///
/// Above the seam because it is a judgement about meaning, not a host
/// operation: the same exit status reads as "an operator asked", "the guest
/// powered itself off" or "it crashed" depending on what was requested and
/// what the machine's control channel reported.
pub fn classify_exit(
    stop_requested: bool,
    guest_shutdown: bool,
    status: &str,
) -> super::vm::StopReason {
    use super::vm::StopReason;
    let clean = status.contains("exit status: 0");
    if stop_requested {
        StopReason::Requested
    } else if guest_shutdown && clean {
        StopReason::GuestInitiated
    } else if clean {
        // A clean exit nobody asked for and the control channel saw no guest
        // shutdown for: the emulator was told to go by something outside this
        // daemon's view.
        StopReason::Requested
    } else {
        StopReason::Crashed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::vm::StopReason;

    /// The sleep watch is only as good as this table: a missed way out of S3
    /// leaves a running guest reported `suspended` for good, and a missed way
    /// in is the bug the watch exists to fix — a sleeping guest read as hung.
    #[test]
    fn sleep_transitions() {
        assert_eq!(sleep_transition("SUSPEND"), Some(true));
        assert_eq!(sleep_transition("WAKEUP"), Some(false));
        // Neither emits WAKEUP, and both leave the vCPUs running.
        assert_eq!(sleep_transition("RESET"), Some(false));
        assert_eq!(sleep_transition("RESUME"), Some(false));
        // Suspend-to-disk powers the guest off; the exit monitor owns that.
        assert_eq!(sleep_transition("SUSPEND_DISK"), None);
        assert_eq!(sleep_transition("SHUTDOWN"), None);
        assert_eq!(sleep_transition("STOP"), None);
    }

    /// Why a machine stopped is a judgement, and the three answers mean very
    /// different things to someone watching `vmlab status`. A guest that
    /// powered itself off is normal; a crash is not.
    ///
    /// The lifecycle tests prove the *caller* acts on this correctly; this
    /// pins the table itself.
    #[test]
    fn exit_classification() {
        // An operator asked — never a crash, whatever the exit status.
        assert_eq!(
            classify_exit(true, false, "exit status: 137"),
            StopReason::Requested
        );
        assert_eq!(
            classify_exit(true, true, "signal: 9"),
            StopReason::Requested
        );
        // The control channel saw the guest power itself off, and the
        // emulator left cleanly.
        assert_eq!(
            classify_exit(false, true, "exit status: 0"),
            StopReason::GuestInitiated
        );
        // A guest shutdown that ended in a dirty exit is still a crash — the
        // shutdown started, something went wrong finishing it.
        assert_eq!(
            classify_exit(false, true, "signal: 11"),
            StopReason::Crashed
        );
        // Clean, unasked-for, no guest event: the emulator was told to go by
        // something outside this daemon's view.
        assert_eq!(
            classify_exit(false, false, "exit status: 0"),
            StopReason::Requested
        );
        // Anything else is a crash.
        assert_eq!(
            classify_exit(false, false, "exit status: 1"),
            StopReason::Crashed
        );
    }
}
