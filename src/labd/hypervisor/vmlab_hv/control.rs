//! `vmlab-hv`'s control protocol as the seam's [`Control`]: the lifecycle
//! commands, the two things only its event stream can answer — did the guest
//! power itself off, is it asleep — and the capabilities its handshake
//! advertised (ADR-0020).
//!
//! The protocol client is synchronous, so this holds **two** connections to
//! the one control socket, as the protocol allows: one carries commands, each
//! run on the blocking pool, and one is read by a thread of its own for
//! events, which the server broadcasts to every connection. The command
//! connection's buffered copies of those events are dropped; the event
//! connection sees every one in order.
//!
//! Kept apart from spawning so it can be driven by a fake server built on
//! `vmlab_hv_proto::server::Server` — no hypervisor binary, no KVM.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::sync::watch;
use vmlab_hv_proto::client::{Client, ClientError};
use vmlab_hv_proto::control::{
    Capability, Command, ErrorCode, Event, Outcome, RunState, ShutdownReason,
};

use super::super::{Control, Frames, Hotplug, Input, Process, Snapshots};
use super::display::HvDisplay;

/// One running `vmlab-hv`, through its control socket.
pub(crate) struct HvControl {
    commands: Arc<Mutex<Client>>,
    capabilities: Vec<Capability>,
    guest_shutdown: Arc<AtomicBool>,
    /// The guest reset itself, which ends vmlab-hv.
    guest_reset: Arc<AtomicBool>,
    asleep: watch::Sender<bool>,
    /// Flips to `true` once the event connection has closed — the
    /// hypervisor has gone and every event it sent has been read.
    events_closed: watch::Receiver<bool>,
    /// The machine's display socket, for a machine launched with a display.
    display: Option<Arc<HvDisplay>>,
    /// The ids of hot-plugged devices `device_deleted` has reported gone
    /// since a detach last asked about them.
    deleted: watch::Sender<BTreeSet<String>>,
}

impl HvControl {
    /// Connect both connections and start reading events. Fails when the
    /// socket does not answer the handshake — not there yet, or a server
    /// speaking another protocol version.
    pub(crate) async fn connect(socket: &Path) -> Result<Self> {
        let path = socket.to_path_buf();
        let (commands, events) = tokio::task::spawn_blocking(move || {
            let commands = Client::connect(&path)?;
            let events = Client::connect(&path)?;
            Ok::<_, ClientError>((commands, events))
        })
        .await
        .context("vmlab-hv control connect task")?
        .with_context(|| format!("vmlab-hv control socket {}", socket.display()))?;

        let capabilities = commands.capabilities().to_vec();
        tracing::debug!(
            hv_version = commands.hv_version(),
            ?capabilities,
            "vmlab-hv control connected"
        );
        let guest_shutdown = Arc::new(AtomicBool::new(false));
        let guest_reset = Arc::new(AtomicBool::new(false));
        let asleep = watch::Sender::new(false);
        let deleted = watch::Sender::new(BTreeSet::new());
        let (closed_tx, events_closed) = watch::channel(false);
        {
            let flag = guest_shutdown.clone();
            let reset = guest_reset.clone();
            let sleep = asleep.clone();
            let gone = deleted.clone();
            std::thread::Builder::new()
                .name("vmlab-hv-events".into())
                .spawn(move || {
                    let mut events = events;
                    // Ends with the stream: the hypervisor exited, or a
                    // message it should never send.
                    while let Ok(Some(event)) = events.wait_event() {
                        if let Event::DeviceDeleted { id } = &event {
                            gone.send_modify(|ids| {
                                ids.insert(id.clone());
                            });
                        }
                        observe(&event, &flag, &reset, &sleep);
                    }
                    closed_tx.send_replace(true);
                })
                .context("spawning the vmlab-hv event reader")?;
        }
        Ok(Self {
            commands: Arc::new(Mutex::new(commands)),
            capabilities,
            guest_shutdown,
            guest_reset,
            asleep,
            events_closed,
            display: None,
            deleted,
        })
    }

    /// The machine has a display, served on `socket`: the frames and input
    /// capabilities are its, where the handshake advertises them.
    pub(crate) fn set_display(&mut self, socket: std::path::PathBuf) {
        self.display = Some(Arc::new(HvDisplay::new(socket)));
    }

    /// Turns `true` once every event the hypervisor sent has been read,
    /// which is after it exits.
    pub(crate) fn drained(&self) -> watch::Receiver<bool> {
        self.events_closed.clone()
    }

    async fn call(&self, command: Command) -> Result<Option<RunState>> {
        call(&self.commands, command).await
    }
}

/// What one event says: a guest that powered itself off (or reset, which
/// QEMU's adapter counts the same way), and whether it is asleep.
fn observe(
    event: &Event,
    guest_shutdown: &AtomicBool,
    guest_reset: &AtomicBool,
    asleep: &watch::Sender<bool>,
) {
    if let Event::Shutdown {
        reason: ShutdownReason::GuestShutdown | ShutdownReason::GuestReset,
    } = event
    {
        guest_shutdown.store(true, Ordering::SeqCst);
    }
    if let Event::Shutdown {
        reason: ShutdownReason::GuestReset,
    } = event
    {
        guest_reset.store(true, Ordering::SeqCst);
    }
    // One table for both hypervisors: the events mean what QEMU's do.
    let name = match event {
        Event::Shutdown { .. } => "SHUTDOWN",
        Event::Reset => "RESET",
        Event::Stopped => "STOP",
        Event::Resumed => "RESUME",
        Event::Suspend => "SUSPEND",
        Event::Wakeup => "WAKEUP",
        Event::DeviceDeleted { .. } => "DEVICE_DELETED",
    };
    if let Some(now) = super::super::sleep_transition(name) {
        asleep.send_if_modified(|was| std::mem::replace(was, now) != now);
    }
}

/// Send one command on the blocking pool and return its outcome as sent.
async fn send(client: &Arc<Mutex<Client>>, command: Command) -> Result<Outcome> {
    let client = client.clone();
    let what = command_name(&command);
    tokio::task::spawn_blocking(move || {
        let mut client = client.lock().unwrap_or_else(|p| p.into_inner());
        let outcome = client.call(command);
        // The event connection reads these; this one's copies go.
        client.take_events();
        outcome
    })
    .await
    .context("vmlab-hv control task")?
    .map_err(|e| anyhow!(e).context(format!("vmlab-hv `{what}`")))
}

/// Send one command and turn its outcome into a result.
async fn call(client: &Arc<Mutex<Client>>, command: Command) -> Result<Option<RunState>> {
    let what = command_name(&command);
    refused(&what, send(client, command).await?)
}

/// An outcome as a result, a refusal naming the command and its code.
fn refused(what: &str, outcome: Outcome) -> Result<Option<RunState>> {
    match outcome {
        Outcome::Ok { status } => Ok(status),
        Outcome::Error { code, message } => Err(anyhow!(
            "vmlab-hv refused `{what}`: {message} ({})",
            serde_json::to_value(code)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{code:?}"))
        )),
    }
}

/// The wire name of a command, for messages.
fn command_name(command: &Command) -> String {
    serde_json::to_value(command)
        .ok()
        .and_then(|v| v.get("command")?.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{command:?}"))
}

#[async_trait::async_trait]
impl Control for HvControl {
    async fn resume(&self) -> Result<()> {
        self.call(Command::Resume).await.map(drop)
    }

    async fn pause(&self) -> Result<()> {
        self.call(Command::Pause).await.map(drop)
    }

    async fn powerdown(&self) -> Result<()> {
        self.call(Command::Powerdown).await.map(drop)
    }

    /// The hypervisor may exit before its reply reaches us; the connection
    /// closing is then the answer, and it is the one asked for.
    async fn quit(&self) -> Result<()> {
        match self.call(Command::Quit).await {
            Ok(_) => Ok(()),
            Err(e)
                if e.chain().any(|c| {
                    matches!(
                        c.downcast_ref::<ClientError>(),
                        Some(ClientError::Closed | ClientError::Io(_))
                    )
                }) =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn guest_shutdown(&self) -> bool {
        self.guest_shutdown.load(Ordering::SeqCst)
    }

    fn guest_reset(&self) -> bool {
        self.guest_reset.load(Ordering::SeqCst)
    }

    fn sleep_watch(&self) -> Option<watch::Receiver<bool>> {
        Some(self.asleep.subscribe())
    }

    /// vmlab-hv has no command of its own for it: `resume` on a machine in
    /// S3 wakes the guest, and it reports `wakeup`.
    async fn wakeup(&self) -> Result<()> {
        self.call(Command::Resume).await.map(drop)
    }

    fn snapshots(&self) -> Option<Arc<dyn Snapshots>> {
        self.capabilities
            .contains(&Capability::Snapshots)
            .then(|| Arc::new(HvSnapshots(self.commands.clone())) as Arc<dyn Snapshots>)
    }

    /// Scripted input rides the display socket (phase 2b), not a control
    /// command.
    fn input(&self) -> Option<Arc<dyn Input>> {
        self.capabilities
            .contains(&Capability::Input)
            .then(|| self.display.clone())
            .flatten()
            .map(|d| d as Arc<dyn Input>)
    }

    fn frames(&self) -> Option<Arc<dyn Frames>> {
        self.capabilities
            .contains(&Capability::Display)
            .then(|| self.display.clone())
            .flatten()
            .map(|d| d as Arc<dyn Frames>)
    }

    /// Host USB passthrough (phase 2f), on a machine launched with the
    /// `usbx` controller.
    fn hotplug(&self) -> Option<Arc<dyn Hotplug>> {
        self.capabilities.contains(&Capability::Hotplug).then(|| {
            Arc::new(HvHotplug {
                commands: self.commands.clone(),
                deleted: self.deleted.clone(),
            }) as Arc<dyn Hotplug>
        })
    }

    // Media: no phase delivered yet has it, so the trait's `None` stands.
}

/// The **hot-plug** capability as `device_add` and `device_del`, the
/// latter confirmed by the `device_deleted` event once the device is back on
/// the host — QEMU's `DEVICE_DELETED`, and waited for the same way.
struct HvHotplug {
    commands: Arc<Mutex<Client>>,
    deleted: watch::Sender<BTreeSet<String>>,
}

#[async_trait::async_trait]
impl Hotplug for HvHotplug {
    /// The plan's arguments are QEMU's `usb-host` ones, which the protocol
    /// carries field for field.
    async fn attach_usb(&self, plan: &crate::usb::AttachPlan) -> Result<()> {
        let mut command = plan.args.clone();
        command["command"] = "device_add".into();
        let command: Command = serde_json::from_value(command)
            .context("the attach plan is not a vmlab-hv `device_add`")?;
        call(&self.commands, command).await.map(drop)
    }

    async fn detach(&self, id: &str, wait: Duration) -> Result<()> {
        // Forget an earlier unplug of the same id, so only this one counts.
        self.deleted.send_modify(|ids| {
            ids.remove(id);
        });
        let mut gone = self.deleted.subscribe();
        let outcome = send(&self.commands, Command::DeviceDel { id: id.into() }).await?;
        match outcome {
            // vmlab-hv has already dropped it — unplugged, or never there.
            Outcome::Error {
                code: ErrorCode::DeviceNotFound,
                ..
            } => return Ok(()),
            other => refused("device_del", other).map(drop)?,
        }
        tokio::time::timeout(wait, gone.wait_for(|ids| ids.contains(id)))
            .await
            .map_err(|_| {
                anyhow!(
                    "vmlab-hv did not report `{id}` gone within {}s of `device_del`",
                    wait.as_secs()
                )
            })?
            .map_err(|_| anyhow!("vmlab-hv exited before reporting `{id}` gone"))?;
        Ok(())
    }
}

/// The **snapshots** capability as the protocol's commands. The hypervisor
/// knows which disks it snapshots and which carries the vmstate — the
/// description said (`vmstate: true`) — so the names the seam passes are the
/// ones it was launched with and need not travel.
struct HvSnapshots(Arc<Mutex<Client>>);

#[async_trait::async_trait]
impl Snapshots for HvSnapshots {
    async fn save(&self, name: &str, _vmstate: &str, _devices: &[&str]) -> Result<()> {
        call(
            &self.0,
            Command::SnapshotSave {
                name: name.to_string(),
            },
        )
        .await
        .map(drop)
    }

    async fn load(&self, name: &str, _vmstate: &str, _devices: &[&str]) -> Result<()> {
        call(
            &self.0,
            Command::SnapshotLoad {
                name: name.to_string(),
            },
        )
        .await
        .map(drop)
    }

    async fn delete(&self, name: &str, _devices: &[&str]) -> Result<()> {
        call(
            &self.0,
            Command::SnapshotDelete {
                name: name.to_string(),
            },
        )
        .await
        .map(drop)
    }
}

/// The hypervisor process, whose exit is only reported once its events have
/// been read: a guest that powers off sends `shutdown` and then exits, and
/// [`super::super::classify_exit`] asks [`Control::guest_shutdown`] the moment
/// the exit is seen — so the event has to have landed first.
pub(crate) struct HvProcess {
    pub(crate) proc: Arc<dyn Process>,
    pub(crate) drained: watch::Receiver<bool>,
}

/// How long an exited hypervisor's event stream gets to close behind it.
const DRAIN_PATIENCE: Duration = Duration::from_secs(2);

#[async_trait::async_trait]
impl Process for HvProcess {
    fn is_running(&self) -> bool {
        self.proc.is_running()
    }

    fn exit_status(&self) -> Option<String> {
        self.proc.exit_status()
    }

    async fn wait_exit(&self, timeout: Duration) -> Result<String> {
        let status = self.proc.wait_exit(timeout).await?;
        let mut drained = self.drained.clone();
        let _ = tokio::time::timeout(DRAIN_PATIENCE, drained.wait_for(|closed| *closed)).await;
        Ok(status)
    }

    async fn kill(&self) {
        self.proc.kill().await;
    }

    fn pid(&self) -> Option<u32> {
        self.proc.pid()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use vmlab_hv_proto::control::ErrorCode;
    use vmlab_hv_proto::server::Server;

    /// A fake vmlab-hv control socket: every command it receives, and a
    /// handle to broadcast events with.
    struct FakeHv {
        _dir: tempfile::TempDir,
        socket: PathBuf,
        seen: mpsc::Receiver<Command>,
        events: Box<dyn Fn(Event) + Send>,
    }

    fn fake_hv(
        capabilities: Vec<Capability>,
        answer: impl Fn(&Command) -> Outcome + Send + 'static,
    ) -> FakeHv {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let (tx, seen) = mpsc::channel();
        let server = Server::new("fake-hv 0.0.0", capabilities, move |command: Command| {
            let outcome = answer(&command);
            let _ = tx.send(command);
            outcome
        });
        let broadcaster = server.clone();
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || server.serve(listener));
        FakeHv {
            _dir: dir,
            socket,
            seen,
            events: Box::new(move |e| broadcaster.broadcast(e)),
        }
    }

    fn ok(_: &Command) -> Outcome {
        Outcome::Ok { status: None }
    }

    #[tokio::test]
    async fn the_lifecycle_commands_reach_the_hypervisor() {
        let hv = fake_hv(vec![], ok);
        let control = HvControl::connect(&hv.socket).await.unwrap();
        control.resume().await.unwrap();
        control.pause().await.unwrap();
        control.powerdown().await.unwrap();
        control.quit().await.unwrap();
        let seen: Vec<_> = hv.seen.try_iter().collect();
        assert_eq!(
            seen,
            vec![
                Command::Resume,
                Command::Pause,
                Command::Powerdown,
                Command::Quit
            ]
        );
    }

    #[tokio::test]
    async fn a_refusal_names_the_command_and_the_code() {
        let hv = fake_hv(vec![], |_| Outcome::Error {
            code: ErrorCode::InvalidState,
            message: "machine is not paused".into(),
        });
        let control = HvControl::connect(&hv.socket).await.unwrap();
        let err = format!("{:#}", control.resume().await.unwrap_err());
        assert!(err.contains("`resume`"), "{err}");
        assert!(err.contains("machine is not paused"), "{err}");
        assert!(err.contains("invalid_state"), "{err}");
    }

    /// The capability is the handshake's word, not an assumption.
    #[tokio::test]
    async fn snapshots_are_offered_only_when_advertised() {
        let without = fake_hv(vec![], ok);
        let control = HvControl::connect(&without.socket).await.unwrap();
        assert!(control.snapshots().is_none());
        assert!(control.input().is_none() && control.frames().is_none());
        assert!(control.hotplug().is_none() && control.media().is_none());

        let with = fake_hv(vec![Capability::Snapshots], |c| match c {
            Command::SnapshotLoad { name } if name == "gone" => Outcome::Error {
                code: ErrorCode::SnapshotNotFound,
                message: "no snapshot `gone`".into(),
            },
            _ => Outcome::Ok { status: None },
        });
        let control = HvControl::connect(&with.socket).await.unwrap();
        let snaps = control.snapshots().expect("advertised");
        snaps.save("s1", "scratch", &["scratch"]).await.unwrap();
        snaps.load("s1", "scratch", &["scratch"]).await.unwrap();
        snaps.delete("s1", &["scratch"]).await.unwrap();
        let err = format!(
            "{:#}",
            snaps
                .load("gone", "scratch", &["scratch"])
                .await
                .unwrap_err()
        );
        assert!(err.contains("snapshot_not_found"), "{err}");
        let seen: Vec<_> = with.seen.try_iter().collect();
        assert_eq!(
            seen,
            vec![
                Command::SnapshotSave { name: "s1".into() },
                Command::SnapshotLoad { name: "s1".into() },
                Command::SnapshotDelete { name: "s1".into() },
                Command::SnapshotLoad {
                    name: "gone".into()
                },
            ]
        );
    }

    async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{what} never happened");
    }

    /// Only the guest's own shutdown counts: a host-requested one is the
    /// stop ladder's, and `classify_exit` must not read it as the guest's.
    #[tokio::test]
    async fn a_guest_shutdown_event_sets_the_flag_and_a_host_one_does_not() {
        let hv = fake_hv(vec![], ok);
        let control = HvControl::connect(&hv.socket).await.unwrap();
        // A request round-trip first, so both connections are registered
        // for broadcasts.
        control.pause().await.unwrap();
        (hv.events)(Event::Shutdown {
            reason: ShutdownReason::HostRequest,
        });
        (hv.events)(Event::Stopped);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!control.guest_shutdown());
        (hv.events)(Event::Shutdown {
            reason: ShutdownReason::GuestShutdown,
        });
        eventually("guest shutdown flag", || control.guest_shutdown()).await;
    }

    #[tokio::test]
    async fn suspend_and_wakeup_drive_the_sleep_watch() {
        let hv = fake_hv(vec![], ok);
        let control = HvControl::connect(&hv.socket).await.unwrap();
        control.pause().await.unwrap();
        let mut asleep = control.sleep_watch().expect("a watch");
        assert!(!*asleep.borrow());
        (hv.events)(Event::Suspend);
        tokio::time::timeout(Duration::from_secs(5), asleep.wait_for(|a| *a))
            .await
            .unwrap()
            .unwrap();
        (hv.events)(Event::Wakeup);
        tokio::time::timeout(Duration::from_secs(5), asleep.wait_for(|a| !*a))
            .await
            .unwrap()
            .unwrap();
        // A reset leaves the guest running whatever state it was in.
        (hv.events)(Event::Suspend);
        tokio::time::timeout(Duration::from_secs(5), asleep.wait_for(|a| *a))
            .await
            .unwrap()
            .unwrap();
        (hv.events)(Event::Reset);
        tokio::time::timeout(Duration::from_secs(5), asleep.wait_for(|a| !*a))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_socket_with_nobody_listening_does_not_connect() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            HvControl::connect(&dir.path().join("control.sock"))
                .await
                .is_err()
        );
    }

    /// Frames and input ride the display socket, where the handshake
    /// advertises them and the machine has one.
    #[tokio::test]
    async fn frames_and_input_ride_the_display_socket() {
        use vmlab_hv_display::Message;
        let display = crate::hvdisplay::fake::spawn(8, 4);
        let hv = fake_hv(vec![Capability::Display, Capability::Input], ok);

        // Advertised, but no display: nothing to offer.
        let bare = HvControl::connect(&hv.socket).await.unwrap();
        assert!(bare.frames().is_none() && bare.input().is_none());

        let mut control = HvControl::connect(&hv.socket).await.unwrap();
        control.set_display(display.path.clone());
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("grab.ppm");
        control
            .frames()
            .expect("advertised")
            .screendump(&shot)
            .await
            .unwrap();
        let img = crate::vision::load_screen(&shot).unwrap();
        assert_eq!((img.width(), img.height()), (8, 4));
        assert_eq!(img.get_pixel(3, 2).0, [3, 2, 3 ^ 2]);

        let input = control.input().expect("advertised");
        input.send_keys(&["ctrl", "alt", "delete"]).await.unwrap();
        input.pointer_to(4, 2, 8, 4).await.unwrap();
        input.button("left", true).await.unwrap();
        input.button("right", true).await.unwrap();
        input.button("left", false).await.unwrap();
        let key = |q: &str, down| Message::Key {
            qcode: q.into(),
            down,
        };
        let want = vec![
            key("ctrl", true),
            key("alt", true),
            key("delete", true),
            key("delete", false),
            key("alt", false),
            key("ctrl", false),
            Message::PointerAbs {
                x: 4,
                y: 2,
                width: 8,
                height: 4,
            },
            Message::Buttons { mask: 1 },
            Message::Buttons { mask: 5 },
            Message::Buttons { mask: 4 },
        ];
        eventually("every input", || {
            display.inputs.lock().unwrap().len() >= want.len()
        })
        .await;
        assert_eq!(*display.inputs.lock().unwrap(), want);

        // Not advertised: not offered, display or none.
        let without = fake_hv(vec![], ok);
        let mut control = HvControl::connect(&without.socket).await.unwrap();
        control.set_display(display.path.clone());
        assert!(control.frames().is_none() && control.input().is_none());
    }

    /// USB passthrough (phase 2f): an attach plan is `device_add` field for
    /// field, a detach is `device_del` that returns once `device_deleted`
    /// names the device — not on another device's — and a device vmlab-hv
    /// no longer has is already detached.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usb_attach_and_detach_ride_device_add_and_device_deleted() {
        let hv = fake_hv(vec![Capability::Hotplug], |c| match c {
            Command::DeviceDel { id } if id == "usbhost-gone" => Outcome::Error {
                code: ErrorCode::DeviceNotFound,
                message: "no device `usbhost-gone`".into(),
            },
            _ => Outcome::Ok { status: None },
        });
        let control = HvControl::connect(&hv.socket).await.unwrap();
        let hotplug = control.hotplug().expect("advertised");
        let plan = crate::usb::AttachPlan {
            device: crate::usb::HostDevice {
                port: "3-1.2".into(),
                bus: 3,
                devnum: 5,
                vendor_id: "1234".into(),
                product_id: "5678".into(),
                manufacturer: None,
                product: None,
                serial: None,
                speed: "480".into(),
                class: 0,
                interfaces: Vec::new(),
            },
            args: serde_json::json!({
                "driver": "usb-host", "id": "usbhost-3-1.2", "bus": "usbx.0",
                "hostbus": 3, "hostaddr": 5, "hostport": "1.2",
            }),
        };
        hotplug.attach_usb(&plan).await.unwrap();
        assert_eq!(
            hv.seen.recv_timeout(Duration::from_secs(5)).unwrap(),
            Command::DeviceAdd {
                id: "usbhost-3-1.2".into(),
                driver: "usb-host".into(),
                bus: Some("usbx.0".into()),
                hostbus: 3,
                hostaddr: 5,
                hostport: "1.2".into(),
            }
        );

        let detach = {
            let hotplug = hotplug.clone();
            tokio::spawn(async move {
                hotplug
                    .detach("usbhost-3-1.2", Duration::from_secs(5))
                    .await
            })
        };
        assert_eq!(
            hv.seen.recv_timeout(Duration::from_secs(5)).unwrap(),
            Command::DeviceDel {
                id: "usbhost-3-1.2".into()
            }
        );
        (hv.events)(Event::DeviceDeleted {
            id: "usbhost-other".into(),
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !detach.is_finished(),
            "another device's event is not this one"
        );
        (hv.events)(Event::DeviceDeleted {
            id: "usbhost-3-1.2".into(),
        });
        tokio::time::timeout(Duration::from_secs(5), detach)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        hotplug
            .detach("usbhost-gone", Duration::from_secs(5))
            .await
            .unwrap();
        let err = hotplug
            .detach("usbhost-slow", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("did not report"), "{err:#}");
    }

    /// A wake is vmlab-hv's `resume`, which takes a guest out of S3.
    #[tokio::test]
    async fn wakeup_is_a_resume() {
        let hv = fake_hv(vec![], ok);
        let control = HvControl::connect(&hv.socket).await.unwrap();
        control.wakeup().await.unwrap();
        assert_eq!(
            hv.seen.try_iter().collect::<Vec<_>>(),
            vec![Command::Resume]
        );
    }
}
