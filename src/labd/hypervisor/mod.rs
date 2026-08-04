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
//! Note what is *not* here: building the argv. Resolving hardware into a
//! command line is [`crate::qemu`]'s job and is already pure and tested; this
//! seam is about running machines, not composing them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::guest_asset::GuestAsset;
use crate::qemu::process::ChildFd;
use crate::qmp::QmpClient;

mod qemu;
pub use qemu::Qemu;

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
/// do once it is up, and what it can learn about how the guest is behaving.
///
/// The QMP client sits *behind* this rather than being handed out, so an
/// adapter does not have to stand up a QMP server to be usable.
#[async_trait::async_trait]
pub trait Control: Send + Sync + 'static {
    /// Release the CPUs. The emulator starts paused so the caller can
    /// subscribe to what it needs before the guest runs an instruction.
    async fn resume(&self) -> Result<()>;

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

    /// The live QMP client, when this machine is real QEMU.
    ///
    /// `None` under an in-memory adapter: snapshot save/load, the framebuffer
    /// and scripted input genuinely need a hypervisor (ADR-0001) and stay
    /// verified against a running lab. Callers that need one already report
    /// "not running" when it is absent.
    fn qmp(&self) -> Option<QmpClient> {
        None
    }
}

/// The guest channels a machine answers on once it is up.
///
/// QEMU binds these from the argv, so its adapter ignores them; an in-memory
/// adapter has to stand them up itself. Naming them here rather than leaving
/// them buried in [`LaunchSpec::args`] is what lets the fake speak the real
/// ctl and agent protocols over a socket, the way the agent client's own
/// tests already do.
#[derive(Debug, Clone, Default)]
pub struct GuestChannels {
    /// `vmlab.agent.0` — the vmlab-agent port every machine kind carries.
    pub agent: PathBuf,
    /// `vmlab.ctl.0` — cinit's ndjson control channel. Containers only.
    pub ctl: Option<PathBuf>,
}

/// A prepared emulator invocation.
pub struct LaunchSpec {
    /// Process label for logs and diagnostics (`qemu:dc01`).
    pub label: String,
    /// Emulator binary (`qemu-system-x86_64`).
    pub binary: String,
    pub args: Vec<String>,
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

    /// Whether this host can serve a share over virtiofs at all. A
    /// capability probe rather than an operation, but the same kind of fact
    /// as the rest of this trait: what the host running the machine can do.
    /// Decides whether `transport = "auto"` shares and container volumes
    /// attach as vhost-user-fs devices or fall back to SMB/CIFS.
    fn virtiofsd_available(&self) -> bool;

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

    /// Spawn the emulator and return once it answers control. CPUs stay
    /// paused (`-S`) — the caller resumes them once it has subscribed to the
    /// events it needs.
    async fn start_emulator(&self, spec: LaunchSpec) -> Result<Running>;

    /// The kernel + initramfs this host boots a container micro-VM from
    /// (`guest/build-asset.sh` output). A host probe like the rest of this
    /// trait: where the asset lives is a property of the machine doing the
    /// running, not of the container being run.
    fn guest_asset(&self, arch: &str) -> Result<GuestAsset>;
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
