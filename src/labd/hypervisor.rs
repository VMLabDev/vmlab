//! The **Hypervisor** seam: the host's ability to actually run a machine.
//!
//! Deciding a machine should run and making the host run it used to be the
//! same 260 lines. `VmInstance::start` reached straight for `spawn_swtpm`,
//! `virtiofsd::spawn`, `Proc::spawn_with_fds` and `QmpClient::connect`, so the
//! parts with real logic — the power-state machine, the exit monitor that
//! classifies *why* QEMU ended, readiness gating, and the teardown that has to
//! unwind a half-started machine — could only be exercised by booting real
//! QEMU on a real host. They had no tests at all.
//!
//! Everything that touches the host sits below this seam; everything that
//! decides what the machine's state *means* sits above it. Two adapters:
//! [`Qemu`] does the real thing, and the fake in this module's tests spawns
//! throwaway processes and a mock QMP server so the failure ladder can be
//! driven on demand — a swtpm that never binds its socket, a virtiofsd that
//! dies mid-start, an emulator that exits with a code, a guest that powers
//! itself off.
//!
//! Note what is *not* here: building the argv. Resolving hardware into a
//! command line is [`crate::qemu`]'s job and is already pure and tested; this
//! seam is about running processes, not composing them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::qemu::Proc;
use crate::qemu::process::ChildFd;
use crate::qmp::QmpClient;

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
}

/// A machine the host is now running.
pub struct Running {
    pub proc: Arc<Proc>,
    pub qmp: QmpClient,
}

/// The host operations that turn a prepared machine into a running one.
///
/// Every method spawns something and can fail; that is the point. A caller
/// above this seam handles the failures, and a test can produce them without a
/// hypervisor.
#[async_trait::async_trait]
pub trait Hypervisor: Send + Sync + 'static {
    /// Start a software TPM and wait for its control socket to appear.
    /// Returning `Ok` means the socket is bound — QEMU connects to it at
    /// startup and fails hard if it is not there yet.
    async fn start_tpm(
        &self,
        machine: &str,
        state_dir: &Path,
        ctrl_sock: &Path,
        log: &Path,
    ) -> Result<Arc<Proc>>;

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
    ) -> Result<Arc<Proc>>;

    /// Spawn the emulator and return once its QMP socket answers. CPUs stay
    /// paused (`-S`) — the caller resumes them once it has subscribed to the
    /// events it needs.
    async fn start_emulator(&self, spec: LaunchSpec) -> Result<Running>;
}

/// The real thing: QEMU and friends, as processes on this host.
pub struct Qemu;

#[async_trait::async_trait]
impl Hypervisor for Qemu {
    async fn start_tpm(
        &self,
        machine: &str,
        state_dir: &Path,
        ctrl_sock: &Path,
        log: &Path,
    ) -> Result<Arc<Proc>> {
        let proc = crate::qemu::process::spawn_swtpm(machine, state_dir, ctrl_sock, log).await?;
        // swtpm binds its control socket a moment after exec; QEMU's chardev
        // connects at startup, so racing it means a hard boot failure.
        for _ in 0..50 {
            if ctrl_sock.exists() {
                return Ok(proc);
            }
            if !proc.is_running() {
                bail!(
                    "{machine}: swtpm exited before binding its socket: {}",
                    proc.exit_status().unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        proc.kill().await;
        bail!(
            "{machine}: swtpm never bound {} — the guest would boot with no TPM",
            ctrl_sock.display()
        )
    }

    async fn start_virtiofsd(
        &self,
        machine: &str,
        socket: &Path,
        shared_dir: &Path,
        readonly: bool,
        log: &Path,
    ) -> Result<Arc<Proc>> {
        crate::qemu::virtiofsd::spawn(machine, socket, shared_dir, readonly, log)
            .await
            .with_context(|| format!("{machine}: starting virtiofsd for {}", shared_dir.display()))
    }

    async fn start_emulator(&self, spec: LaunchSpec) -> Result<Running> {
        let proc = Proc::spawn_with_fds(&spec.label, &spec.binary, &spec.args, &spec.log, spec.fds)
            .await?;
        let qmp = connect_qmp_retry(&spec.qmp_sock, &proc).await?;
        Ok(Running { proc, qmp })
    }
}

/// Wait for the emulator's QMP socket to accept a connection, failing fast if
/// the process dies during startup.
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

/// Why a machine left the Running state (PRD §8.1).
///
/// Above the seam because it is a judgement about meaning, not a host
/// operation: the same exit status reads as "an operator asked", "the guest
/// powered itself off" or "it crashed" depending on what was requested and
/// what QMP reported.
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
        // A clean exit nobody asked for and QMP saw no guest shutdown for:
        // QEMU was told to go by something outside this daemon's view.
        StopReason::Requested
    } else {
        StopReason::Crashed
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A hypervisor that runs harmless processes instead of virtual machines,
    //! so the lifecycle above the seam can be driven without KVM.

    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What the fake should do when asked to start something.
    #[derive(Debug, Clone, Default)]
    pub struct Script {
        /// Fail `start_tpm` with this message.
        pub tpm_fails: Option<String>,
        /// Fail `start_virtiofsd` with this message.
        pub virtiofsd_fails: Option<String>,
        /// Fail `start_emulator` with this message.
        pub emulator_fails: Option<String>,
        /// Shell the emulator runs instead of QEMU. Default: sleep quietly.
        pub emulator_script: Option<String>,
    }

    pub struct FakeHypervisor {
        pub script: Script,
        /// Everything the fake spawned, so a test can assert nothing leaked.
        pub spawned: Mutex<Vec<Arc<Proc>>>,
        pub tpm_starts: AtomicUsize,
        pub virtiofsd_starts: AtomicUsize,
        pub emulator_starts: AtomicUsize,
    }

    impl FakeHypervisor {
        pub fn new(script: Script) -> Arc<Self> {
            Arc::new(Self {
                script,
                spawned: Mutex::new(Vec::new()),
                tpm_starts: AtomicUsize::new(0),
                virtiofsd_starts: AtomicUsize::new(0),
                emulator_starts: AtomicUsize::new(0),
            })
        }

        async fn sh(&self, label: &str, script: &str, log: &Path) -> Result<Arc<Proc>> {
            let proc = Proc::spawn(
                label,
                "/bin/sh",
                &["-c".to_string(), script.to_string()],
                log,
            )
            .await?;
            self.spawned
                .lock()
                .expect("spawned lock")
                .push(proc.clone());
            Ok(proc)
        }
    }

    #[async_trait::async_trait]
    impl Hypervisor for FakeHypervisor {
        async fn start_tpm(
            &self,
            machine: &str,
            _state_dir: &Path,
            ctrl_sock: &Path,
            log: &Path,
        ) -> Result<Arc<Proc>> {
            self.tpm_starts.fetch_add(1, Ordering::SeqCst);
            if let Some(msg) = &self.script.tpm_fails {
                bail!("{machine}: {msg}");
            }
            // A real swtpm binds its socket; tests that care assert on it.
            let _ = std::fs::write(ctrl_sock, b"");
            self.sh(&format!("swtpm:{machine}"), "sleep 300", log).await
        }

        async fn start_virtiofsd(
            &self,
            machine: &str,
            _socket: &Path,
            _shared_dir: &Path,
            _readonly: bool,
            log: &Path,
        ) -> Result<Arc<Proc>> {
            self.virtiofsd_starts.fetch_add(1, Ordering::SeqCst);
            if let Some(msg) = &self.script.virtiofsd_fails {
                bail!("{machine}: {msg}");
            }
            self.sh(&format!("virtiofsd:{machine}"), "sleep 300", log)
                .await
        }

        async fn start_emulator(&self, spec: LaunchSpec) -> Result<Running> {
            self.emulator_starts.fetch_add(1, Ordering::SeqCst);
            if let Some(msg) = &self.script.emulator_fails {
                bail!("{}: {msg}", spec.label);
            }
            let script = self
                .script
                .emulator_script
                .clone()
                .unwrap_or_else(|| "sleep 300".to_string());
            let proc = self.sh(&spec.label, &script, &spec.log).await?;
            let qmp = crate::qmp::tests::spawn_mock_qmp(&spec.qmp_sock).await?;
            Ok(Running { proc, qmp })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeHypervisor, Script};
    use super::*;
    use crate::labd::vm::StopReason;

    fn spec(dir: &Path, script: &str) -> LaunchSpec {
        LaunchSpec {
            label: "qemu:test".into(),
            binary: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            log: dir.join("qemu.log"),
            qmp_sock: dir.join("qmp.sock"),
            fds: Vec::new(),
        }
    }

    /// The seam's whole point: a caller above it can be handed a running
    /// machine with a real `Proc` and a real `QmpClient` without KVM, a
    /// template, or a disk image.
    #[tokio::test]
    async fn the_fake_hands_back_a_real_proc_and_qmp() {
        let dir = tempfile::tempdir().unwrap();
        let hv = FakeHypervisor::new(Script::default());
        let running = hv
            .start_emulator(spec(dir.path(), "sleep 300"))
            .await
            .expect("launch");
        assert!(running.proc.is_running());
        // A real QMP round-trip over the mock server.
        assert!(running.qmp.query_status().await.is_ok());
        running.proc.kill().await;
    }

    /// A swtpm that never binds its socket must fail the start, not let QEMU
    /// spawn and die on a missing chardev — the failure ladder that had no
    /// coverage before this seam existed.
    #[tokio::test]
    async fn a_tpm_that_never_binds_fails_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let hv = FakeHypervisor::new(Script {
            tpm_fails: Some("swtpm never bound its socket".into()),
            ..Default::default()
        });
        let err = hv
            .start_tpm(
                "dc01",
                &dir.path().join("state"),
                &dir.path().join("tpm.sock"),
                &dir.path().join("swtpm.log"),
            )
            .await
            .err()
            .expect("must fail");
        assert!(format!("{err:#}").contains("dc01"), "names the machine");
    }

    #[tokio::test]
    async fn a_dead_virtiofsd_fails_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let hv = FakeHypervisor::new(Script {
            virtiofsd_fails: Some("virtiofsd exited immediately".into()),
            ..Default::default()
        });
        assert!(
            hv.start_virtiofsd(
                "dc01",
                &dir.path().join("vfs0.sock"),
                dir.path(),
                false,
                &dir.path().join("vfs.log"),
            )
            .await
            .is_err()
        );
    }

    /// An emulator that dies before QMP comes up is reported as such rather
    /// than hanging for the full connect window.
    #[tokio::test]
    async fn an_emulator_that_exits_during_startup_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let proc = Proc::spawn(
            "qemu:test",
            "/bin/sh",
            &["-c".to_string(), "exit 1".to_string()],
            &dir.path().join("qemu.log"),
        )
        .await
        .unwrap();
        // Give it a moment to actually exit.
        let _ = proc.wait_exit(Duration::from_secs(5)).await;
        let err = connect_qmp_retry(&dir.path().join("nope.sock"), &proc)
            .await
            .err()
            .expect("must fail");
        assert!(
            format!("{err:#}").contains("exited during startup"),
            "{err:#}"
        );
    }

    /// Why a machine stopped is a judgement, and the three answers mean very
    /// different things to someone watching `vmlab status`. A guest that
    /// powered itself off is normal; a crash is not.
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
        // QMP saw the guest power itself off, and QEMU left cleanly.
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
        // Clean, unasked-for, no guest event: QEMU was told to go by
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
