//! A template build's lab, served for inspection (PRD §6.1).
//!
//! A build boots its VM in a synthetic one-machine lab that the supervisor
//! runs in-process, not under a lab daemon — so nothing listened on that
//! lab's control socket, and every machine verb answered "lab … is not
//! running" about a VM that was plainly on screen. Whoever develops a
//! screen-driven template needs to see it, and whoever finds a build stuck at
//! a prompt needs to press the key it waits for.
//!
//! So while the build runs, the supervisor binds the lab's control socket
//! itself and answers the part of the lab vocabulary that watches or types
//! into the build VM. Nothing that changes the lab's shape is served: the
//! build owns its VM's power, disks and provisions, and a `vmlab vm stop` or
//! a snapshot taken under it would seal something nobody built. Which
//! commands those are is [`serves`], a value the tests pin (ADR-0003).

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use anyhow::Result;
use serde_json::Value;

use super::LabdHandler;
use super::lab::LabRuntime;
use crate::proto::server::{Handler, Server, Streamer};
use crate::proto::{CommandError, LabRequest, WireRequest as _};

/// Whether a template build's lab answers `req`.
///
/// Exhaustive on purpose: a command added to the lab vocabulary fails to
/// compile here until someone decides whether a build may serve it.
pub fn serves(req: &LabRequest) -> bool {
    match req {
        // What a CLI verb asks before it drives a daemon, and what the lab
        // and its machine look like.
        LabRequest::Ping {}
        | LabRequest::Version {}
        | LabRequest::Status {}
        | LabRequest::MachineCapabilities { .. }
        | LabRequest::MachineIp { .. }
        | LabRequest::MachineLogs { .. }
        // The screen: read it, and drive it by hand.
        | LabRequest::MachineScreenshot { .. }
        | LabRequest::MachineOcr { .. }
        | LabRequest::MachineFindImage { .. }
        | LabRequest::MachineSendKeys { .. }
        | LabRequest::MachineMouseMove { .. }
        | LabRequest::MachineMouseClick { .. }
        | LabRequest::MachineMouseDrag { .. } => true,

        // The lab's shape and the VM's power: the build's to decide.
        LabRequest::DnsTable {}
        | LabRequest::Up { .. }
        | LabRequest::Pull { .. }
        | LabRequest::Reload {}
        | LabRequest::PullCancel { .. }
        | LabRequest::Run { .. }
        | LabRequest::Down { .. }
        | LabRequest::Destroy {}
        | LabRequest::MachineStart { .. }
        | LabRequest::MachineStop { .. }
        | LabRequest::MachineRestart { .. }
        | LabRequest::MachineDestroy { .. }
        | LabRequest::Shutdown {}
        // State the sealed image would carry, or a build would roll back.
        | LabRequest::SnapshotTake { .. }
        | LabRequest::SnapshotRestore { .. }
        | LabRequest::SnapshotDelete { .. }
        | LabRequest::SnapshotList { .. }
        | LabRequest::WorktreeFreeze { .. }
        | LabRequest::WorktreeThaw { .. }
        | LabRequest::MachineUsbAttach { .. }
        | LabRequest::MachineUsbDetach { .. }
        | LabRequest::MachineHeldUsb { .. }
        | LabRequest::MachineRepairAgent { .. }
        | LabRequest::MachineRecordStart { .. }
        | LabRequest::MachineRecordStop { .. }
        | LabRequest::MachineRecordStatus { .. }
        | LabRequest::PlaybookList {}
        | LabRequest::PlaybookCheck { .. }
        | LabRequest::PlaybookApply { .. }
        | LabRequest::WorkspaceFlush { .. }
        | LabRequest::WorkspaceResolve { .. }
        | LabRequest::WorkspaceDiff { .. }
        // The agent: a build installs and verifies the one it seals, and
        // the guest it would reach is half-installed.
        | LabRequest::MachineExec { .. }
        | LabRequest::MachineOsInfo { .. }
        | LabRequest::MachineTtyOpen { .. }
        | LabRequest::MachineTtyResize { .. }
        | LabRequest::MachineAppOpen { .. }
        | LabRequest::MachinePushFile { .. }
        | LabRequest::MachinePullFile { .. }
        | LabRequest::MachineTail { .. }
        | LabRequest::MachineEventLog { .. }
        | LabRequest::MachineStats { .. }
        | LabRequest::MachineClipboardGet { .. }
        | LabRequest::MachineClipboardSet { .. } => false,
    }
}

/// The refusal a build's lab answers a command it does not serve with.
fn refusal(req: &LabRequest) -> CommandError {
    CommandError::unsupported(format!(
        "`{}` is not served for a template build's lab: it answers inspection and \
         input only (screenshot, console, sendkeys, mouse, ocr, find-image) — \
         stop the build with `vmlab template stop`",
        req.cmd()
    ))
}

/// Holds the runtime weakly: the build drops its runtime to free the lab's
/// directories before a verification boot reuses them, and a socket being
/// served must not keep it alive past that.
struct BuildLabHandler {
    lab: Weak<LabRuntime>,
    tasks: Arc<crate::lifecycle::TaskGroup>,
}

#[async_trait::async_trait]
impl Handler<LabRequest> for BuildLabHandler {
    async fn handle(&self, req: LabRequest, stream: &Streamer) -> Result<Value, CommandError> {
        if !serves(&req) {
            return Err(refusal(&req));
        }
        let lab = self.lab.upgrade().ok_or_else(|| {
            CommandError::conflict("the template build's VM is no longer running")
        })?;
        let inner = LabdHandler {
            lab,
            tasks: self.tasks.clone(),
        };
        inner.handle(req, stream).await
    }
}

/// The build lab's control socket, bound for as long as this is held.
pub struct BuildLabServer {
    _server: Server<LabRequest>,
    sock: PathBuf,
}

impl Drop for BuildLabServer {
    fn drop(&mut self) {
        // The accept loop stops with the server; the socket file would
        // otherwise outlive it and turn "not running" into a refused connect.
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// Bind `runtime`'s lab control socket for a build, or `None` when a live
/// daemon already answers there — a lab of that name the user runs, which
/// the build must not take the socket from.
pub async fn serve(runtime: &Arc<LabRuntime>) -> Result<Option<BuildLabServer>> {
    let name = runtime.config().lab.name.clone();
    let sock = crate::paths::lab_socket(&name);
    if crate::proto::client::LabClient::connect(&sock)
        .await
        .is_ok()
    {
        return Ok(None);
    }
    let handler: Arc<dyn Handler<LabRequest>> = Arc::new(BuildLabHandler {
        lab: Arc::downgrade(runtime),
        tasks: Arc::new(crate::lifecycle::TaskGroup::new()),
    });
    let server = Server::bind(&sock, handler).await?;
    Ok(Some(BuildLabServer {
        _server: server,
        sock,
    }))
}

#[cfg(test)]
mod tests {
    use super::{refusal, serves};
    use crate::proto::{LabRequest, WireRequest as _};
    use serde_json::json;

    fn req(cmd: &str, args: serde_json::Value) -> LabRequest {
        LabRequest::from_wire(cmd, args).expect("a well-formed request")
    }

    #[test]
    fn a_build_lab_serves_the_screen_and_its_input() {
        for (cmd, args) in [
            ("ping", json!({})),
            ("version", json!({})),
            ("status", json!({})),
            (
                "machine.screenshot",
                json!({"machine": "build", "path": "/x.png"}),
            ),
            (
                "machine.sendkeys",
                json!({"machine": "build", "keys": "ret"}),
            ),
            (
                "machine.mouse_move",
                json!({"machine": "build", "x": 1, "y": 2}),
            ),
            (
                "machine.mouse_click",
                json!({"machine": "build", "button": "left"}),
            ),
            ("machine.ocr", json!({"machine": "build"})),
            ("machine.capabilities", json!({"machine": "build"})),
        ] {
            assert!(serves(&req(cmd, args)), "{cmd} should be served");
        }
    }

    #[test]
    fn a_build_lab_refuses_what_would_change_the_build() {
        for (cmd, args) in [
            ("up", json!({})),
            ("down", json!({})),
            ("destroy", json!({})),
            ("shutdown", json!({})),
            ("machine.stop", json!({"machine": "build"})),
            ("machine.restart", json!({"machine": "build"})),
            ("snapshot.take", json!({"name": "s"})),
            (
                "machine.exec",
                json!({"machine": "build", "cmd": "true", "args": []}),
            ),
        ] {
            let r = req(cmd, args);
            assert!(!serves(&r), "{cmd} should be refused");
            let e = refusal(&r);
            assert_eq!(e.code, crate::proto::ErrorCode::Unsupported);
            assert!(e.message.contains(cmd), "{}", e.message);
        }
    }
}
