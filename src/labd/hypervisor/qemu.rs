//! The production adapter: QEMU and friends, as processes on this host.
//!
//! Everything here is the mapping layer the seam's own handle types buy —
//! `Proc` becomes a [`Process`], the QMP client plus its SHUTDOWN-event watch
//! become a [`Control`]. Nothing above the seam sees either concrete type
//! again, which is what lets [`super::fake`] be in-memory.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{Control, GuestAsset, Hypervisor, LaunchSpec, Process, Running};
use crate::qemu::Proc;
use crate::qmp::QmpClient;

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
    ) -> Result<Arc<dyn Process>> {
        let proc = crate::qemu::process::spawn_swtpm(machine, state_dir, ctrl_sock, log).await?;
        // swtpm binds its control socket a moment after exec; QEMU's chardev
        // connects at startup, so racing it means a hard boot failure.
        for _ in 0..50 {
            if ctrl_sock.exists() {
                return Ok(host_process(proc));
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

    fn virtiofsd(&self) -> Option<crate::qemu::virtiofsd::Virtiofsd> {
        crate::qemu::virtiofsd::found()
    }

    async fn start_virtiofsd(
        &self,
        machine: &str,
        socket: &Path,
        shared_dir: &Path,
        readonly: bool,
        log: &Path,
    ) -> Result<Arc<dyn Process>> {
        let proc = crate::qemu::virtiofsd::spawn(machine, socket, shared_dir, readonly, log)
            .await
            .with_context(|| {
                format!("{machine}: starting virtiofsd for {}", shared_dir.display())
            })?;
        Ok(host_process(proc))
    }

    async fn start_emulator(&self, spec: LaunchSpec) -> Result<Running> {
        let log_from = crate::qemu::process::log_offset(&spec.log);
        let proc = Proc::spawn_with_fds(&spec.label, &spec.binary, &spec.args, &spec.log, spec.fds)
            .await?;
        let qmp = connect_qmp_retry(&spec.qmp_sock, &proc, &spec.log, log_from).await?;
        // QEMU binds the guest channels itself, from the argv — this adapter
        // has nothing to stand up, only to say where they landed. It is the
        // first thing anyone wants when a terminal will not attach.
        tracing::debug!(
            machine = %spec.label,
            agent = %spec.channels.agent.display(),
            ctl = ?spec.channels.ctl,
            "emulator up"
        );
        Ok(Running {
            proc: host_process(proc),
            control: Arc::new(QemuControl::new(qmp)),
        })
    }

    fn guest_asset(&self, arch: &str) -> Result<GuestAsset> {
        crate::guest_asset::ensure_guest_asset(arch)
    }
}

/// Present a spawned host process as the seam's [`Process`].
fn host_process(proc: Arc<Proc>) -> Arc<dyn Process> {
    Arc::new(HostProcess(proc))
}

struct HostProcess(Arc<Proc>);

#[async_trait::async_trait]
impl Process for HostProcess {
    fn is_running(&self) -> bool {
        self.0.is_running()
    }

    fn exit_status(&self) -> Option<String> {
        self.0.exit_status()
    }

    async fn wait_exit(&self, timeout: Duration) -> Result<String> {
        self.0.wait_exit(timeout).await
    }

    async fn kill(&self) {
        self.0.kill().await;
    }
}

/// QMP as the seam's [`Control`], plus the one piece of state that only the
/// event stream can answer: whether the guest powered *itself* off.
struct QemuControl {
    qmp: QmpClient,
    guest_shutdown: Arc<AtomicBool>,
    /// Whether the guest is suspended to RAM, from `SUSPEND`/`WAKEUP` (see
    /// [`super::sleep_transition`]).
    asleep: tokio::sync::watch::Sender<bool>,
}

impl QemuControl {
    /// Subscribing here rather than above the seam matters: the caller has
    /// not been handed the client yet, so no SHUTDOWN or SUSPEND event can be
    /// missed between connect and subscribe.
    fn new(qmp: QmpClient) -> Self {
        let guest_shutdown = Arc::new(AtomicBool::new(false));
        let flag = guest_shutdown.clone();
        let asleep = tokio::sync::watch::Sender::new(false);
        let sleep = asleep.clone();
        let mut events = qmp.subscribe_events();
        tokio::spawn(async move {
            loop {
                let ev = match events.recv().await {
                    Ok(ev) => ev,
                    // A burst of events overran the channel: carry on, since
                    // the next sleep transition still lands.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if ev.event == "SHUTDOWN" {
                    let initiator = ev.data.get("reason").and_then(|r| r.as_str());
                    if initiator == Some("guest-shutdown") || initiator == Some("guest-reset") {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                if let Some(now) = super::sleep_transition(&ev.event) {
                    sleep.send_if_modified(|was| std::mem::replace(was, now) != now);
                }
            }
        });
        Self {
            qmp,
            guest_shutdown,
            asleep,
        }
    }
}

#[async_trait::async_trait]
impl Control for QemuControl {
    async fn resume(&self) -> Result<()> {
        Ok(self.qmp.cont().await?)
    }

    async fn powerdown(&self) -> Result<()> {
        Ok(self.qmp.system_powerdown().await?)
    }

    async fn quit(&self) -> Result<()> {
        Ok(self.qmp.quit().await?)
    }

    fn guest_shutdown(&self) -> bool {
        self.guest_shutdown.load(Ordering::SeqCst)
    }

    fn sleep_watch(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        Some(self.asleep.subscribe())
    }

    async fn wakeup(&self) -> Result<()> {
        Ok(self.qmp.system_wakeup().await?)
    }

    fn qmp(&self) -> Option<QmpClient> {
        Some(self.qmp.clone())
    }
}

/// Wait for the emulator's QMP socket to accept a connection, failing fast if
/// the process dies during startup — quoting what it wrote to `log` past
/// `log_from`, which is where QEMU says why.
async fn connect_qmp_retry(
    sock: &Path,
    proc: &Arc<Proc>,
    log: &Path,
    log_from: u64,
) -> Result<QmpClient> {
    for _ in 0..100 {
        if !proc.is_running() {
            return Err(crate::qemu::process::exited_during_startup(
                "QEMU",
                &proc.exit_status().unwrap_or_default(),
                log,
                log_from,
            ));
        }
        match QmpClient::connect(sock).await {
            Ok(c) => return Ok(c),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    bail!("QMP socket {} never came up", sock.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An emulator that dies before QMP comes up is reported as such rather
    /// than hanging for the full connect window — the failure a missing
    /// firmware file or a bad argv actually produces.
    #[tokio::test]
    async fn an_emulator_that_exits_during_startup_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("qemu.log");
        let log_from = crate::qemu::process::log_offset(&log);
        let proc = Proc::spawn(
            "qemu:test",
            "/bin/sh",
            &[
                "-c".to_string(),
                "echo 'qemu-system-x86_64: Could not access KVM kernel module' >&2; exit 1"
                    .to_string(),
            ],
            &log,
        )
        .await
        .unwrap();
        // Give it a moment to actually exit.
        let _ = proc.wait_exit(Duration::from_secs(5)).await;
        let err = connect_qmp_retry(&dir.path().join("nope.sock"), &proc, &log, log_from)
            .await
            .err()
            .expect("must fail");
        let err = format!("{err:#}");
        assert!(err.contains("exited during startup"), "{err}");
        // The reason QEMU gave, not only its exit status.
        assert!(err.contains("Could not access KVM kernel module"), "{err}");
    }
}
