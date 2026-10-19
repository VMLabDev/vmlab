//! Host USB passthrough, the lab daemon's half (PRD §5.2.1).
//!
//! A machine attaches, detaches and tracks its own devices
//! ([`Machine::usb_attach`](super::machine::Machine::usb_attach) and
//! friends); what lives here is what spans the lab: resolving a selector
//! against the host, asking who else holds a device, the `usb.*` events, and
//! the watch that notices a device unplugged from the host while a VM holds
//! it.
//!
//! **An unplug says nothing.** QEMU's `usb-host` device outlives its host
//! device: it detaches from the guest's bus and waits for a match to
//! reappear, with no QMP event either way. The watch reads sysfs instead and
//! removes the `usb-host` device itself — which also stops a *different*
//! device later plugged into that socket from being handed to the guest
//! unasked (the attach pins the device number too, so it would not match,
//! but a lingering device is still a lie in `vmlab usb list`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;

use super::lab::LabRuntime;
use crate::usb::{self, Attachment, HostDevice, Listed, Selector};

/// How often the unplug watch reads sysfs while anything is attached.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);
/// How long the supervisor gets to say who holds what.
const HOLDERS_WAIT: Duration = Duration::from_secs(5);

/// The lab-wide state of USB passthrough.
pub struct UsbHost {
    /// Where the host's devices are listed — `/sys/bus/usb/devices`, or a
    /// fake tree under test.
    pub(super) sysfs: PathBuf,
    /// Where their nodes live — `/dev/bus/usb`.
    pub(super) dev: PathBuf,
    /// Serialises attaches across the lab's machines, so two cannot both
    /// pass the "already attached" check for one device.
    op: tokio::sync::Mutex<()>,
    /// The unplug watch is running.
    watching: AtomicBool,
    /// Whether the supervisor is asked who holds what in other labs. Off
    /// under test, where there is no supervisor to ask.
    ask_supervisor: bool,
}

impl Default for UsbHost {
    fn default() -> Self {
        UsbHost {
            sysfs: usb::SYSFS_ROOT.into(),
            dev: usb::DEV_ROOT.into(),
            op: tokio::sync::Mutex::new(()),
            watching: AtomicBool::new(false),
            ask_supervisor: true,
        }
    }
}

impl UsbHost {
    #[cfg(test)]
    pub(super) fn fake(sysfs: PathBuf, dev: PathBuf) -> Self {
        UsbHost {
            sysfs,
            dev,
            ask_supervisor: false,
            ..UsbHost::default()
        }
    }

    fn scan(&self) -> Result<Vec<HostDevice>> {
        usb::scan(&self.sysfs).map_err(|e| anyhow!("reading {}: {e}", self.sysfs.display()))
    }
}

fn attachment_event(a: &Attachment, reason: Option<&str>) -> serde_json::Value {
    let mut ev = json!({
        "vm": a.machine,
        "port": a.device.port,
        "id": a.device.id(),
        "product": a.device.product,
        "qdev_id": a.qdev_id,
    });
    if let Some(reason) = reason {
        ev["reason"] = json!(reason);
    }
    ev
}

impl LabRuntime {
    /// Every device this lab's machines hold, as `(port, lab/vm)`.
    async fn usb_held_here(&self) -> Vec<(String, String)> {
        let mut held = Vec::new();
        for m in self.machines() {
            for a in m.usb_attached().await {
                held.push((a.device.port, format!("{}/{}", self.name, a.machine)));
            }
        }
        held
    }

    /// Every device any machine on this host holds, as `(port, lab/vm)`:
    /// the supervisor's view, which asks every running lab, merged with
    /// this lab's own — the supervisor may be unreachable, and this lab's
    /// answer is never stale.
    async fn usb_held(&self) -> Vec<(String, String)> {
        let mut held = self.usb_held_here().await;
        if !self.usb.ask_supervisor {
            return held;
        }
        let others = tokio::time::timeout(HOLDERS_WAIT, async {
            let client =
                crate::proto::client::SupClient::connect(&crate::paths::supervisor_socket())
                    .await
                    .ok()?;
            client.send(crate::proto::SupRequest::UsbList {}).await.ok()
        })
        .await
        .ok()
        .flatten();
        let listed: Vec<Listed> = others
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        for l in listed {
            if let Some(holder) = l.attached_to
                && !held.iter().any(|(port, _)| *port == l.device.port)
            {
                held.push((l.device.port, holder));
            }
        }
        held
    }

    /// The host's devices, each with the machine holding it — what
    /// `lab.usb_devices()` returns to a script.
    pub async fn usb_devices(&self) -> Result<Vec<Listed>> {
        let host = self.usb.scan()?;
        Ok(usb::listing(host, &self.usb_held().await))
    }

    /// Attach the host device `selector` names to `machine` (§5.2.1).
    ///
    /// The refusals, in the order they are met: a container; a VM not
    /// running, or booted without `usb = true`; a selector naming no device,
    /// or several; a device another machine holds; a HID keyboard or mouse
    /// without `force`; a device node this process cannot open.
    pub async fn usb_attach(
        self: &Arc<Self>,
        machine: &str,
        selector: &str,
        force: bool,
    ) -> Result<Attachment> {
        let m = self.machine(machine)?;
        m.usb_ready().await?;
        let sel = Selector::parse(selector).map_err(|e| anyhow!(e))?;
        let _op = self.usb.op.lock().await;
        let host = self.usb.scan()?;
        let held = self.usb_held().await;
        let plan = usb::plan_attach(
            &sel,
            &host,
            &held,
            force,
            &self.usb.dev,
            usb::node_accessible,
        )
        .map_err(|e| anyhow!(e))?;
        let attachment = m.usb_attach(&plan).await?;
        self.events
            .emit("usb.attached", attachment_event(&attachment, None));
        self.usb_watch();
        Ok(attachment)
    }

    /// Detach what `selector` names from `machine`, or — `None` — every
    /// device it holds. Answers with what was detached.
    pub async fn usb_detach(
        &self,
        machine: &str,
        selector: Option<&str>,
    ) -> Result<Vec<Attachment>> {
        let m = self.machine(machine)?;
        let attached = m.usb_attached().await;
        let targets: Vec<Attachment> = match selector {
            None => attached,
            Some(s) => {
                let sel = Selector::parse(s).map_err(|e| anyhow!(e))?;
                vec![
                    usb::resolve_attached(&sel, machine, &attached)
                        .map_err(|e| anyhow!(e))?
                        .clone(),
                ]
            }
        };
        let mut done = Vec::new();
        for t in targets {
            let gone = m.usb_detach(&t.qdev_id).await?;
            self.events
                .emit("usb.detached", attachment_event(&gone, None));
            done.push(gone);
        }
        Ok(done)
    }

    /// What `machine` holds — or, `None`, every machine in the lab. A
    /// container holds nothing, so asking one is an empty answer rather
    /// than a refusal: the supervisor asks every machine of every lab.
    pub async fn usb_attached(&self, machine: Option<&str>) -> Result<Vec<Attachment>> {
        match machine {
            Some(name) => Ok(self.machine(name)?.usb_attached().await),
            None => {
                let mut all = Vec::new();
                for m in self.machines() {
                    all.extend(m.usb_attached().await);
                }
                Ok(all)
            }
        }
    }

    /// Drop every device `machine` holds before something rewinds or
    /// replaces its device state — an online restore, whose guest never
    /// saw them. A device that will not detach is said and left; the
    /// restore that follows is what the caller asked for.
    pub(super) async fn usb_release_for(&self, machine: &str, reason: &str) {
        let Ok(m) = self.machine(machine) else {
            return;
        };
        for a in m.usb_attached().await {
            match m.usb_detach(&a.qdev_id).await {
                Ok(gone) => self
                    .events
                    .emit("usb.detached", attachment_event(&gone, Some(reason))),
                Err(e) => tracing::warn!("{machine}: {e:#}"),
            }
        }
    }

    /// One pass of the unplug watch: drop every attachment whose device has
    /// left the host (or come back as a new plug-in). Answers whether
    /// anything is still attached.
    pub(super) async fn usb_sweep(&self) -> bool {
        let host = match self.usb.scan() {
            Ok(host) => host,
            Err(e) => {
                tracing::warn!("usb watch: {e:#}");
                return true;
            }
        };
        let mut remaining = false;
        for m in self.machines() {
            let attached = m.usb_attached().await;
            let departed: Vec<Attachment> = usb::departed(&attached, &host)
                .into_iter()
                .cloned()
                .collect();
            remaining |= attached.len() > departed.len();
            for a in departed {
                // The tracking goes whatever QEMU answers: the host device
                // is gone either way.
                if let Err(e) = m.usb_detach(&a.qdev_id).await {
                    tracing::warn!("{}: removing unplugged {}: {e:#}", a.machine, a.qdev_id);
                }
                self.events
                    .emit("usb.detached", attachment_event(&a, Some("unplugged")));
            }
        }
        remaining
    }

    /// Start the unplug watch unless it runs. It ends itself once nothing is
    /// attached, so a lab that never attaches anything never reads sysfs.
    fn usb_watch(self: &Arc<Self>) {
        if self.usb.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(WATCH_INTERVAL).await;
                let Some(lab) = me.upgrade() else { return };
                if lab.usb_sweep().await {
                    continue;
                }
                lab.usb.watching.store(false, Ordering::SeqCst);
                // An attach that landed between the sweep and the store saw
                // the watch running and did not start one: take it back.
                if lab.usb_attached(None).await.is_ok_and(|a| !a.is_empty())
                    && !lab.usb.watching.swap(true, Ordering::SeqCst)
                {
                    continue;
                }
                return;
            }
        });
    }
}
