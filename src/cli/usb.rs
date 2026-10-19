//! `vmlab usb` — host USB passthrough (PRD §5.2.1): list what the host has
//! plugged in and who holds it, attach a device to a running VM, detach it.

use anyhow::{Result, bail};
use serde_json::Value;

use super::daemon::remote;
use super::emit;
use super::lab::{lab_client_for, rt, split_vm_ref};
use crate::proto::client::SupClient;
use crate::proto::{LabRequest, SupRequest};
use crate::usb::{Attachment, Listed};

#[derive(clap::Subcommand)]
pub enum UsbCmd {
    /// List the host's USB devices (hubs hidden) and the VM holding each
    List {
        /// Emit a JSON array instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Hot-plug a host USB device into a running VM that has `usb = true`
    ///
    /// SELECTOR is `<vid>:<pid>`, `<vid>:<pid>@<serial>` or
    /// `port:<bus>-<port>` — `vmlab usb list` shows each. The device leaves
    /// the host while attached.
    Attach {
        /// The VM, as `[lab/]vm`
        vm: String,
        selector: String,
        /// Attach a HID keyboard or mouse, which the host is probably using
        #[arg(long)]
        force: bool,
        /// Emit the attachment as JSON
        #[arg(long)]
        json: bool,
    },
    /// Unplug a host USB device from a VM and give it back to the host
    Detach {
        /// The VM, as `[lab/]vm`
        vm: String,
        /// The device, as for `attach`
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        selector: Option<String>,
        /// Detach every device the VM holds
        #[arg(long)]
        all: bool,
        /// Emit what was detached as JSON
        #[arg(long)]
        json: bool,
    },
}

pub fn cmd_usb(cmd: UsbCmd) -> Result<()> {
    match cmd {
        UsbCmd::List { json } => cmd_list(json),
        UsbCmd::Attach {
            vm,
            selector,
            force,
            json,
        } => cmd_attach(&vm, selector, force, json),
        UsbCmd::Detach {
            vm,
            selector,
            all,
            json,
        } => cmd_detach(&vm, selector, all, json),
    }
}

/// The listing comes from the supervisor, which asks every running lab who
/// holds what. With no supervisor there are no labs, so nothing is held and
/// the host's own sysfs is the whole answer — a read-only query does not
/// start one.
fn cmd_list(json: bool) -> Result<()> {
    rt()?.block_on(async {
        let listed = match SupClient::connect(&crate::paths::supervisor_socket()).await {
            Ok(client) => client.send(SupRequest::UsbList {}).await.map_err(remote)?,
            Err(_) => {
                let host = crate::usb::scan(std::path::Path::new(crate::usb::SYSFS_ROOT))?;
                serde_json::to_value(crate::usb::listing(host, &[]))?
            }
        };
        emit(json, &listed, render_list)
    })
}

fn render_list(listed: &Value) -> String {
    let rows: Vec<Listed> = serde_json::from_value(listed.clone()).unwrap_or_default();
    if rows.is_empty() {
        return "no USB devices on this host\n".into();
    }
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|l| {
            let d = &l.device;
            let name = [d.manufacturer.as_deref(), d.product.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            [
                d.port.clone(),
                d.id(),
                d.serial.clone().unwrap_or_else(|| "-".into()),
                d.speed_name().to_string(),
                l.attached_to.clone().unwrap_or_else(|| "-".into()),
                name,
            ]
        })
        .collect();
    let head = ["PORT", "ID", "SERIAL", "SPEED", "ATTACHED TO", "DEVICE"];
    let width = |i: usize| {
        cells
            .iter()
            .map(|c| c[i].len())
            .chain([head[i].len()])
            .max()
            .unwrap_or(0)
    };
    let w: Vec<usize> = (0..5).map(width).collect();
    let line = |c: [&str; 6]| {
        format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {}\n",
            c[0],
            c[1],
            c[2],
            c[3],
            c[4],
            c[5],
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
            w3 = w[3],
            w4 = w[4],
        )
    };
    let mut out = line(head);
    for c in &cells {
        out.push_str(&line([&c[0], &c[1], &c[2], &c[3], &c[4], &c[5]]));
    }
    out
}

fn cmd_attach(vm_ref: &str, selector: String, force: bool, json: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(vm_ref)?;
        let (lab, client) = lab_client_for(lab).await?;
        let reply = client
            .send(LabRequest::MachineUsbAttach {
                machine,
                selector,
                force,
            })
            .await
            .map_err(remote)?;
        emit(json, &reply, |v| {
            match serde_json::from_value::<Attachment>(v.clone()) {
                Ok(a) => format!("attached {} to {lab}/{}\n", a.device.describe(), a.machine),
                Err(_) => format!("{v}\n"),
            }
        })
    })
}

fn cmd_detach(vm_ref: &str, selector: Option<String>, all: bool, json: bool) -> Result<()> {
    if selector.is_none() && !all {
        bail!("name the device to detach, or pass --all");
    }
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(vm_ref)?;
        let (lab, client) = lab_client_for(lab).await?;
        let reply = client
            .send(LabRequest::MachineUsbDetach {
                machine: machine.clone(),
                selector,
                all,
            })
            .await
            .map_err(remote)?;
        emit(json, &reply, |v| {
            let gone: Vec<Attachment> = serde_json::from_value(v.clone()).unwrap_or_default();
            if gone.is_empty() {
                return format!("{lab}/{machine} holds no USB devices\n");
            }
            gone.iter()
                .map(|a| {
                    format!(
                        "detached {} from {lab}/{}\n",
                        a.device.describe(),
                        a.machine
                    )
                })
                .collect()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listing_table_names_the_holder() {
        let listed = serde_json::json!([
            {"port": "1-2.4", "bus": 1, "devnum": 15, "vendor_id": "046d",
             "product_id": "c547", "manufacturer": "Logitech", "product": "USB Receiver",
             "speed": "12", "class": 0, "attached_to": "lab1/dc01"},
            {"port": "1-9", "bus": 1, "devnum": 6, "vendor_id": "0781",
             "product_id": "5581", "serial": "BB22", "speed": "5000", "class": 0},
        ]);
        let out = render_list(&listed);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("PORT"), "{out}");
        assert!(
            lines[1].contains("046d:c547")
                && lines[1].contains("lab1/dc01")
                && lines[1].contains("Logitech USB Receiver"),
            "{out}"
        );
        assert!(
            lines[2].contains("BB22") && lines[2].contains("super"),
            "{out}"
        );
        assert_eq!(
            render_list(&serde_json::json!([])),
            "no USB devices on this host\n"
        );
    }
}
