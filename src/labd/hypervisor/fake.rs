//! An entirely in-memory hypervisor: no KVM, no subprocess, no disk image.
//!
//! The previous fake handed back a real `Proc` and a real `QmpClient`, so it
//! had to spawn `/bin/sh` and stand up a mock QMP server for every machine —
//! expensive enough that the lifecycle tests it existed for were never
//! written (ADR-0001). Now that the seam owns its handle types, a machine is
//! a `watch` channel and two flags.
//!
//! What it *does* stand up is the guest channels, because those carry real
//! protocols the code under test speaks: a fake cinit on `vmlab.ctl.0` and a
//! fake vmlab-agent on `vmlab.agent.0`, both over unix sockets in a tempdir,
//! following the agent client's own tests. A scenario is then a [`Script`]:
//! what fails, what the guest reports, and how the machine ends.
//!
//! Every failure modelled here has been seen against real QEMU — a swtpm that
//! never binds its control socket, a virtiofsd that dies during startup, an
//! emulator that exits before QMP answers, a guest that ignores ACPI, a
//! container entrypoint that exits immediately and keeps doing so.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, watch};

use vmlab_agent_proto::{
    AgentMsg, Frame, FrameDecoder, FrameKind, HostMsg, PROTO_VERSION as AGENT_PROTO, encode_ctrl,
};
use vmlab_cinit_proto::{CtlCommand, CtlEvent, PROTO_VERSION as CTL_PROTO};

use super::{Control, GuestAsset, GuestChannels, Hypervisor, LaunchSpec, Process, Running};

/// How a scripted machine ends by itself.
#[derive(Debug, Clone)]
pub struct Exit {
    /// How long after coming up.
    pub after: Duration,
    /// The status string [`Process::exit_status`] reports.
    pub status: String,
    /// The guest powered itself off rather than the emulator dying — what
    /// separates a clean shutdown from a crash in [`super::classify_exit`].
    pub guest_initiated: bool,
}

/// One start attempt, as the fake plays it.
#[derive(Debug, Clone, Default)]
pub struct Run {
    /// The emulator never comes up: `start_emulator` fails with this.
    pub start_fails: Option<String>,
    /// What the guest's init reports on the ctl channel once it has the
    /// spec, each after a delay from the previous one. A scripted
    /// [`CtlEvent::Exited`] also ends the machine, exactly as cinit powers
    /// the micro-VM off once the container process is gone.
    pub ctl: Vec<(Duration, CtlEvent)>,
    /// The machine ends on its own this long after coming up — the emulator
    /// dying out from under the guest.
    pub exits: Option<Exit>,
    /// The guest ignores an ACPI powerdown. The request still succeeds (QEMU
    /// accepts it); nothing happens, which is what forces a stop ladder onto
    /// its next rung.
    pub ignores_powerdown: bool,
}

impl Run {
    /// Comes up and stays up until something stops it.
    pub fn forever() -> Self {
        Self::default()
    }

    /// Comes up, then the emulator dies with `status`.
    pub fn dies(after: Duration, status: &str) -> Self {
        Self {
            exits: Some(Exit {
                after,
                status: status.to_string(),
                guest_initiated: false,
            }),
            ..Self::default()
        }
    }

    /// Comes up, then the guest powers itself off cleanly.
    pub fn guest_powers_off(after: Duration) -> Self {
        Self {
            exits: Some(Exit {
                after,
                status: "exit status: 0".into(),
                guest_initiated: true,
            }),
            ..Self::default()
        }
    }

    /// A container whose entrypoint starts and then exits with `code`.
    pub fn container_exits(after: Duration, code: i32) -> Self {
        Self {
            ctl: vec![
                (Duration::ZERO, CtlEvent::Started { pid: 1 }),
                (after, CtlEvent::Exited { code }),
            ],
            ..Self::default()
        }
    }
}

/// A scenario, declaratively.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// `start_tpm` fails with this message.
    pub tpm_fails: Option<String>,
    /// `start_virtiofsd` fails with this message.
    pub virtiofsd_fails: Option<String>,
    /// This host has no virtiofsd, so shares and volumes fall back to SMB.
    pub no_virtiofsd: bool,
    /// No guest boot asset is installed for this architecture.
    pub guest_asset_missing: bool,
    /// The guest runs a vmlab-agent that answers the handshake.
    pub agent: bool,
    /// One entry per start attempt, in order. The last repeats once the list
    /// is exhausted — a crash-looping container keeps crash-looping.
    pub runs: Vec<Run>,
}

impl Script {
    /// A machine that comes up and stays up, with a live guest agent.
    pub fn healthy() -> Self {
        Self {
            agent: true,
            runs: vec![Run::forever()],
            ..Self::default()
        }
    }
}

pub struct FakeHypervisor {
    script: Script,
    /// Which start attempt the next `start_emulator` is.
    attempts: AtomicUsize,
    /// Every helper the fake started (TPMs, filesystem daemons), so a test
    /// can assert teardown left nothing behind.
    helpers: Mutex<Vec<Arc<FakeProc>>>,
}

impl FakeHypervisor {
    pub fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            attempts: AtomicUsize::new(0),
            helpers: Mutex::new(Vec::new()),
        })
    }

    /// Helper processes this fake started that are still alive. Teardown is
    /// meant to leave none: a leaked virtiofsd holds its vhost-user socket
    /// and a leaked swtpm holds the TPM state directory.
    pub async fn live_helpers(&self) -> Vec<String> {
        self.helpers
            .lock()
            .await
            .iter()
            .filter(|p| p.is_running())
            .map(|p| p.name.clone())
            .collect()
    }

    /// The script entry for this start attempt; the last one repeats.
    fn next_run(&self) -> Run {
        let n = self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.script.runs.is_empty() {
            return Run::forever();
        }
        let i = n.min(self.script.runs.len() - 1);
        self.script.runs[i].clone()
    }

    async fn helper(&self, name: String) -> Arc<dyn Process> {
        let proc = Arc::new(FakeProc::new(name));
        self.helpers.lock().await.push(proc.clone());
        proc
    }
}

#[async_trait::async_trait]
impl Hypervisor for FakeHypervisor {
    async fn start_tpm(
        &self,
        machine: &str,
        _state_dir: &Path,
        ctrl_sock: &Path,
        _log: &Path,
    ) -> Result<Arc<dyn Process>> {
        if let Some(msg) = &self.script.tpm_fails {
            bail!("{machine}: {msg}");
        }
        // A real swtpm binds its socket before the adapter returns Ok.
        if let Some(parent) = ctrl_sock.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(ctrl_sock, b"")?;
        Ok(self.helper(format!("swtpm:{machine}")).await)
    }

    fn virtiofsd_available(&self) -> bool {
        !self.script.no_virtiofsd
    }

    async fn start_virtiofsd(
        &self,
        machine: &str,
        _socket: &Path,
        _shared_dir: &Path,
        _readonly: bool,
        _log: &Path,
    ) -> Result<Arc<dyn Process>> {
        if let Some(msg) = &self.script.virtiofsd_fails {
            bail!("{machine}: {msg}");
        }
        Ok(self.helper(format!("virtiofsd:{machine}")).await)
    }

    async fn start_emulator(&self, spec: LaunchSpec) -> Result<Running> {
        let run = self.next_run();
        if let Some(msg) = &run.start_fails {
            bail!("{}: {msg}", spec.label);
        }
        let machine = Arc::new(FakeProc::new(spec.label.clone()));
        serve_guest(&spec.channels, &machine, &run, self.script.agent)?;

        if let Some(exit) = run.exits.clone() {
            let machine = machine.clone();
            tokio::spawn(async move {
                tokio::time::sleep(exit.after).await;
                if exit.guest_initiated {
                    machine.guest_shutdown.store(true, Ordering::SeqCst);
                }
                machine.end(&exit.status);
            });
        }

        Ok(Running {
            control: Arc::new(FakeControl {
                machine: machine.clone(),
                ignores_powerdown: run.ignores_powerdown,
            }),
            proc: machine,
        })
    }

    fn guest_asset(&self, arch: &str) -> Result<GuestAsset> {
        if self.script.guest_asset_missing {
            bail!("no micro-VM guest asset for {arch}");
        }
        // Paths only ever reach the argv builder, which the fake never runs.
        Ok(GuestAsset {
            kernel: PathBuf::from("/fake/vmlinuz"),
            initrd: PathBuf::from("/fake/initramfs.img"),
            version: "fake".into(),
        })
    }
}

// ---- the machine itself -----------------------------------------------------

/// An in-memory process: a `watch` channel that goes from "running" to an
/// exit status exactly once, plus the guest-shutdown flag its control channel
/// reports.
pub(crate) struct FakeProc {
    name: String,
    exited: watch::Sender<Option<String>>,
    guest_shutdown: AtomicBool,
}

impl FakeProc {
    fn new(name: String) -> Self {
        Self {
            name,
            exited: watch::Sender::new(None),
            guest_shutdown: AtomicBool::new(false),
        }
    }

    /// End the machine, if it has not already ended. The first status wins,
    /// so a kill racing a scheduled exit cannot rewrite history.
    fn end(&self, status: &str) {
        self.exited.send_if_modified(|slot| {
            if slot.is_some() {
                return false;
            }
            *slot = Some(status.to_string());
            true
        });
    }

    /// The guest powered itself off — a clean exit nobody has to explain.
    fn guest_powers_off(&self) {
        self.guest_shutdown.store(true, Ordering::SeqCst);
        self.end("exit status: 0");
    }

    /// Resolves once the machine has ended. The guest-channel servers race
    /// their sockets against this so a finished machine's listeners go with
    /// it, rather than parking on `accept` for the rest of the test binary.
    async fn ended(&self) {
        let mut rx = self.exited.subscribe();
        while rx.borrow_and_update().is_none() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[async_trait::async_trait]
impl Process for FakeProc {
    fn is_running(&self) -> bool {
        self.exited.borrow().is_none()
    }

    fn exit_status(&self) -> Option<String> {
        self.exited.borrow().clone()
    }

    async fn wait_exit(&self, timeout: Duration) -> Result<String> {
        let mut rx = self.exited.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(status) = rx.borrow_and_update().clone() {
                return Ok(status);
            }
            tokio::time::timeout_at(deadline, rx.changed())
                .await
                .map_err(|_| anyhow::anyhow!("{} did not exit within {timeout:?}", self.name))?
                .map_err(|_| anyhow::anyhow!("process watcher gone"))?;
        }
    }

    async fn kill(&self) {
        self.end("signal: 9");
    }
}

struct FakeControl {
    machine: Arc<FakeProc>,
    ignores_powerdown: bool,
}

#[async_trait::async_trait]
impl Control for FakeControl {
    async fn resume(&self) -> Result<()> {
        Ok(())
    }

    async fn powerdown(&self) -> Result<()> {
        // QEMU accepts the request either way; whether the guest acts on it
        // is the guest's business, and a guest that does not is what the
        // next rung of the stop ladder exists for.
        if !self.ignores_powerdown {
            self.machine.guest_powers_off();
        }
        Ok(())
    }

    async fn quit(&self) -> Result<()> {
        self.machine.end("exit status: 0");
        Ok(())
    }

    fn guest_shutdown(&self) -> bool {
        self.machine.guest_shutdown.load(Ordering::SeqCst)
    }
}

// ---- the guest channels -----------------------------------------------------

/// Bind and serve whichever guest channels this machine exposes.
///
/// Bound synchronously, before the caller is told the machine is up: the
/// lifecycle connects to both the moment `start_emulator` returns, exactly as
/// it does against QEMU's `server=on,wait=off` chardevs.
fn serve_guest(
    channels: &GuestChannels,
    machine: &Arc<FakeProc>,
    run: &Run,
    agent: bool,
) -> Result<()> {
    if let Some(ctl) = &channels.ctl {
        let listener = bind(ctl)?;
        serve_ctl(listener, machine.clone(), run.ctl.clone());
    }
    if agent && !channels.agent.as_os_str().is_empty() {
        let listener = bind(&channels.agent)?;
        serve_agent(listener, machine.clone());
    }
    Ok(())
}

/// Bind a guest-channel socket, replacing one left by a previous run — a
/// restarted machine rebinds its ports just as a respawned QEMU does.
fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    Ok(UnixListener::bind(path)?)
}

/// A fake cinit on `vmlab.ctl.0`: announce boot, block on the spec, then
/// report the scripted lifecycle. A `stop` command signals the container and
/// powers the micro-VM off, which is the whole of the container stop ladder's
/// first rung.
fn serve_ctl(listener: UnixListener, machine: Arc<FakeProc>, timeline: Vec<(Duration, CtlEvent)>) {
    tokio::spawn(async move {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = machine.ended() => return,
        };
        let Ok((stream, _)) = accepted else {
            return;
        };
        let (read_half, write_half) = stream.into_split();
        let out = Arc::new(Mutex::new(write_half));
        let send = |ev: CtlEvent| {
            let out = out.clone();
            async move {
                let mut line = serde_json::to_string(&ev).expect("ctl event");
                line.push('\n');
                let _ = out.lock().await.write_all(line.as_bytes()).await;
            }
        };

        // cinit announces boot until the spec lands.
        send(CtlEvent::Boot {
            proto_version: CTL_PROTO,
        })
        .await;

        let mut specced = false;
        let mut lines = BufReader::new(read_half).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(cmd) = serde_json::from_str::<CtlCommand>(line.trim()) else {
                continue;
            };
            match cmd {
                // The first spec unblocks the boot; cinit ignores duplicates.
                CtlCommand::Spec { .. } if !specced => {
                    specced = true;
                    let machine = machine.clone();
                    let out = out.clone();
                    let timeline = timeline.clone();
                    tokio::spawn(async move {
                        for (delay, ev) in timeline {
                            tokio::time::sleep(delay).await;
                            let mut line = serde_json::to_string(&ev).expect("ctl event");
                            line.push('\n');
                            if out.lock().await.write_all(line.as_bytes()).await.is_err() {
                                return;
                            }
                            // cinit powers the micro-VM off once the
                            // container process is gone.
                            if matches!(ev, CtlEvent::Exited { .. }) {
                                machine.end("exit status: 0");
                                return;
                            }
                        }
                    });
                }
                CtlCommand::Stop { .. } => {
                    send(CtlEvent::Exited { code: 0 }).await;
                    machine.end("exit status: 0");
                    return;
                }
                _ => {}
            }
        }
    });
}

/// A fake vmlab-agent on `vmlab.agent.0`: enough of the frame protocol to
/// complete the handshake, answer pings, and act on a shutdown request. Loops
/// on accept because the host reconnects whenever its cached handle dies.
fn serve_agent(listener: UnixListener, machine: Arc<FakeProc>) {
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = machine.ended() => return,
            };
            let Ok((stream, _)) = accepted else {
                return;
            };
            agent_session(stream, machine.clone()).await;
        }
    });
}

async fn agent_session(stream: UnixStream, machine: Arc<FakeProc>) {
    let (mut rx, mut tx) = stream.into_split();
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match rx.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        dec.push(&buf[..n]);
        while let Some(Frame { kind, payload, .. }) = dec.next_frame() {
            if kind != FrameKind::Ctrl {
                continue;
            }
            let Ok(msg) = serde_json::from_slice::<HostMsg>(&payload) else {
                continue;
            };
            let reply = match msg {
                HostMsg::Hello { token, .. } => AgentMsg::Hello {
                    proto_version: AGENT_PROTO,
                    agent_version: "0.0.0-fake".into(),
                    os: "linux".into(),
                    features: vec!["exec".into(), "file".into(), "terminal".into()],
                    token,
                },
                HostMsg::Ping => AgentMsg::Pong,
                HostMsg::NetInfo => AgentMsg::NetInfo {
                    interfaces: Vec::new(),
                },
                HostMsg::Shutdown { mode } => AgentMsg::ShuttingDown { mode },
                _ => continue,
            };
            let shutting_down = matches!(reply, AgentMsg::ShuttingDown { .. });
            if tx.write_all(&encode_ctrl(&reply)).await.is_err() {
                return;
            }
            if shutting_down {
                machine.guest_powers_off();
                return;
            }
        }
    }
}
