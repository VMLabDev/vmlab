//! Host USB passthrough (PRD §5.2.1): what the host has plugged in, how a
//! selector names one device, and every refusal an attach can meet before
//! QEMU is asked to do anything.
//!
//! Pure over its inputs. The device list is read from sysfs under a root the
//! caller passes ([`scan`]), and an attach's every decision —
//! [`plan_attach`] — is a value computed before the QMP `device_add` it
//! leads to (ADR-0003). The lab daemon executes the plan
//! ([`crate::labd::vm`]); the supervisor's `usb.list` folds in which machine
//! holds each device.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the kernel lists USB devices.
pub const SYSFS_ROOT: &str = "/sys/bus/usb/devices";
/// Where the device nodes QEMU opens live.
pub const DEV_ROOT: &str = "/dev/bus/usb";
/// The qdev id of the xHCI controller a `usb = true` VM boots with; every
/// hot-plugged device rides its bus, `usbx.0`.
pub const CONTROLLER_ID: &str = "usbx";

/// USB interface class: human interface device.
const CLASS_HID: u8 = 0x03;
/// USB device class: hub.
const CLASS_HUB: u8 = 0x09;

/// One interface of a host device — enough to say what the device is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

impl Interface {
    /// A HID boot keyboard (protocol 1) or boot mouse (protocol 2).
    fn host_input(&self) -> Option<&'static str> {
        match (self.class, self.protocol) {
            (CLASS_HID, 1) => Some("keyboard"),
            (CLASS_HID, 2) => Some("mouse"),
            _ => None,
        }
    }
}

/// One USB device plugged into the host, as sysfs reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDevice {
    /// The kernel's port path, `<bus>-<port>[.<port>...]` — stable for as
    /// long as the device stays in the same socket, which is why it is the
    /// identity everything else keys on.
    pub port: String,
    pub bus: u32,
    /// The device number on its bus; changes on every replug.
    pub devnum: u32,
    /// Four lowercase hex digits.
    pub vendor_id: String,
    pub product_id: String,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub serial: Option<String>,
    /// Negotiated speed in Mbit/s, as sysfs spells it (`1.5`, `12`, `480`,
    /// `5000`, …).
    pub speed: String,
    pub class: u8,
    #[serde(default)]
    pub interfaces: Vec<Interface>,
}

impl HostDevice {
    /// `vvvv:pppp`.
    pub fn id(&self) -> String {
        format!("{}:{}", self.vendor_id, self.product_id)
    }

    /// The selector that names exactly this device.
    pub fn port_selector(&self) -> String {
        format!("port:{}", self.port)
    }

    /// The device node QEMU opens.
    pub fn node(&self, dev_root: &Path) -> PathBuf {
        dev_root
            .join(format!("{:03}", self.bus))
            .join(format!("{:03}", self.devnum))
    }

    /// The port path below the bus — what QEMU's `usb-host` calls
    /// `hostport` (`1-2.1.4` → `2.1.4`).
    pub fn hostport(&self) -> &str {
        self.port
            .split_once('-')
            .map(|(_, p)| p)
            .unwrap_or(&self.port)
    }

    /// The qdev id an attachment of this device carries: derived from the
    /// port path, so one port is one id on any VM.
    pub fn qdev_id(&self) -> String {
        qdev_id(&self.port)
    }

    /// What the host is likely using this device as — a HID boot keyboard
    /// or mouse — or `None`.
    pub fn host_input(&self) -> Option<&'static str> {
        // Keyboard first: a combined receiver is more dangerous to lose as
        // a keyboard than as a mouse, so that is what the refusal names.
        let kinds: Vec<_> = self
            .interfaces
            .iter()
            .filter_map(Interface::host_input)
            .collect();
        kinds
            .iter()
            .find(|k| **k == "keyboard")
            .or(kinds.first())
            .copied()
    }

    /// How a message names this device: `046d:c547 Logitech USB Receiver at
    /// port 1-2.1.4`.
    pub fn describe(&self) -> String {
        let mut s = self.id();
        let label: Vec<&str> = [self.manufacturer.as_deref(), self.product.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        if !label.is_empty() {
            s.push(' ');
            s.push_str(&label.join(" "));
        }
        s.push_str(" at port ");
        s.push_str(&self.port);
        s
    }

    /// Speed as people say it.
    pub fn speed_name(&self) -> &'static str {
        match self.speed.as_str() {
            "1.5" => "low",
            "12" => "full",
            "480" => "high",
            "5000" => "super",
            "10000" | "20000" => "super+",
            _ => "unknown",
        }
    }
}

/// The qdev id an attachment at `port` carries.
pub fn qdev_id(port: &str) -> String {
    format!("usbhost-{port}")
}

/// Every non-hub device under `root` (normally [`SYSFS_ROOT`]), sorted by
/// port. An entry that cannot be read is skipped rather than failing the
/// listing: a device unplugged mid-scan is not an error.
pub fn scan(root: &Path) -> std::io::Result<Vec<HostDevice>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        // No USB at all — a container without sysfs, a host without a bus.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `usbN` is a root hub, `1-2:1.0` an interface; a device is
        // `<bus>-<port>[.<port>...]`.
        if name.contains(':') || !name.contains('-') {
            continue;
        }
        if let Some(dev) = read_device(&root.join(&name), &name)
            && dev.class != CLASS_HUB
        {
            out.push(dev);
        }
    }
    out.sort_by_key(|d| port_key(&d.port));
    Ok(out)
}

/// Sort key: bus, then each port number — so `1-10` follows `1-9`.
fn port_key(port: &str) -> Vec<u32> {
    port.split(['-', '.'])
        .map(|p| p.parse().unwrap_or(u32::MAX))
        .collect()
}

fn read_device(dir: &Path, port: &str) -> Option<HostDevice> {
    let attr = |name: &str| -> Option<String> {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let hex = |name: &str| attr(name).and_then(|s| u8::from_str_radix(&s, 16).ok());
    let mut interfaces = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        let prefix = format!("{port}:");
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&prefix))
            .collect();
        names.sort();
        for n in names {
            let iface = dir.join(&n);
            let read = |a: &str| {
                std::fs::read_to_string(iface.join(a))
                    .ok()
                    .and_then(|s| u8::from_str_radix(s.trim(), 16).ok())
            };
            if let (Some(class), Some(subclass), Some(protocol)) = (
                read("bInterfaceClass"),
                read("bInterfaceSubClass"),
                read("bInterfaceProtocol"),
            ) {
                interfaces.push(Interface {
                    class,
                    subclass,
                    protocol,
                });
            }
        }
    }
    Some(HostDevice {
        port: port.to_string(),
        bus: attr("busnum")?.parse().ok()?,
        devnum: attr("devnum")?.parse().ok()?,
        vendor_id: attr("idVendor")?.to_lowercase(),
        product_id: attr("idProduct")?.to_lowercase(),
        manufacturer: attr("manufacturer"),
        product: attr("product"),
        serial: attr("serial"),
        speed: attr("speed").unwrap_or_default(),
        class: hex("bDeviceClass").unwrap_or(0),
        interfaces,
    })
}

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

/// How a user names a device (PRD §5.2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// `vvvv:pppp`, optionally `@serial`.
    Id {
        vendor: String,
        product: String,
        serial: Option<String>,
    },
    /// `port:<bus>-<port>[.<port>...]`.
    Port(String),
}

impl Selector {
    pub fn parse(s: &str) -> Result<Selector, String> {
        let bad = || {
            format!(
                "`{s}` is not a USB selector — use `<vid>:<pid>`, `<vid>:<pid>@<serial>` or \
                 `port:<bus>-<port>` (`vmlab usb list` shows all three)"
            )
        };
        if let Some(port) = s.strip_prefix("port:") {
            let ok = port.split_once('-').is_some_and(|(bus, ports)| {
                bus.parse::<u32>().is_ok()
                    && !ports.is_empty()
                    && ports.split('.').all(|p| p.parse::<u32>().is_ok())
            });
            return if ok {
                Ok(Selector::Port(port.to_string()))
            } else {
                Err(bad())
            };
        }
        let (ids, serial) = match s.split_once('@') {
            Some((ids, serial)) if !serial.is_empty() => (ids, Some(serial.to_string())),
            Some(_) => return Err(bad()),
            None => (s, None),
        };
        let (vendor, product) = ids.split_once(':').ok_or_else(bad)?;
        let hex4 = |v: &str| v.len() == 4 && v.chars().all(|c| c.is_ascii_hexdigit());
        if !hex4(vendor) || !hex4(product) {
            return Err(bad());
        }
        Ok(Selector::Id {
            vendor: vendor.to_lowercase(),
            product: product.to_lowercase(),
            serial,
        })
    }

    fn matches(&self, port: &str, vendor: &str, product: &str, serial: Option<&str>) -> bool {
        match self {
            Selector::Port(p) => p == port,
            Selector::Id {
                vendor: v,
                product: p,
                serial: s,
            } => v == vendor && p == product && s.as_deref().is_none_or(|s| Some(s) == serial),
        }
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Selector::Port(p) => write!(f, "port:{p}"),
            Selector::Id {
                vendor,
                product,
                serial: Some(s),
            } => write!(f, "{vendor}:{product}@{s}"),
            Selector::Id {
                vendor, product, ..
            } => write!(f, "{vendor}:{product}"),
        }
    }
}

/// Exactly one item `sel` names, or why not: nothing matched, or several
/// did and the message lists them by the selector that names each alone.
fn pick_one<'a, T>(
    sel: &Selector,
    items: &'a [T],
    key: impl Fn(&T) -> (&str, &str, &str, Option<&str>),
    describe: impl Fn(&T) -> String,
    where_: &str,
) -> Result<&'a T, String> {
    let hits: Vec<&T> = items
        .iter()
        .filter(|i| {
            let (port, v, p, s) = key(i);
            sel.matches(port, v, p, s)
        })
        .collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no USB device matches `{sel}` {where_}")),
        many => Err(format!(
            "`{sel}` matches {} USB devices {where_} — name one by port:\n{}",
            many.len(),
            many.iter()
                .map(|d| format!("  port:{}  {}", key(d).0, describe(d)))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

/// The one host device `sel` names.
pub fn resolve<'a>(sel: &Selector, devices: &'a [HostDevice]) -> Result<&'a HostDevice, String> {
    pick_one(
        sel,
        devices,
        |d| {
            (
                d.port.as_str(),
                d.vendor_id.as_str(),
                d.product_id.as_str(),
                d.serial.as_deref(),
            )
        },
        HostDevice::describe,
        "on this host",
    )
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

/// A host device a running VM holds. Runtime only: a stop releases it and
/// nothing is persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// The machine holding it.
    pub machine: String,
    /// The qdev id the `usb-host` device carries.
    pub qdev_id: String,
    pub device: HostDevice,
}

/// One row of the host listing (`usb.list`): a device, and the machine
/// holding it as `lab/vm`, if one does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    #[serde(flatten)]
    pub device: HostDevice,
    #[serde(default)]
    pub attached_to: Option<String>,
}

/// The host's devices with each holder folded in. `held` maps a port to
/// `lab/vm`; a held port the host no longer has is not listed — the holder
/// is about to drop it.
pub fn listing(host: Vec<HostDevice>, held: &[(String, String)]) -> Vec<Listed> {
    host.into_iter()
        .map(|device| Listed {
            attached_to: held
                .iter()
                .find(|(port, _)| *port == device.port)
                .map(|(_, holder)| holder.clone()),
            device,
        })
        .collect()
}

/// The one attachment `sel` names on `machine`.
pub fn resolve_attached<'a>(
    sel: &Selector,
    machine: &str,
    attached: &'a [Attachment],
) -> Result<&'a Attachment, String> {
    pick_one(
        sel,
        attached,
        |a| {
            (
                a.device.port.as_str(),
                a.device.vendor_id.as_str(),
                a.device.product_id.as_str(),
                a.device.serial.as_deref(),
            )
        },
        |a| a.device.describe(),
        &format!("attached to \"{machine}\""),
    )
}

/// Attachments whose device is no longer the one plugged in at that port —
/// unplugged, or replugged (a new device number). QEMU keeps a `usb-host`
/// device whose host device left, detached from the guest's bus and with no
/// event said; these are what the tracking has to drop.
pub fn departed<'a>(attached: &'a [Attachment], host: &[HostDevice]) -> Vec<&'a Attachment> {
    attached
        .iter()
        .filter(|a| {
            !host.iter().any(|d| {
                d.port == a.device.port && d.bus == a.device.bus && d.devnum == a.device.devnum
            })
        })
        .collect()
}

/// Whether this process can open `node` read-write — what QEMU's `usb-host`
/// needs, and what a stock `root:root 0664` node does not grant.
pub fn node_accessible(node: &Path) -> bool {
    use nix::unistd::{AccessFlags, access};
    access(node, AccessFlags::R_OK | AccessFlags::W_OK).is_ok()
}

/// The refusal for a node this process cannot open, with both fixes.
pub fn permission_refusal(dev: &HostDevice, node: &Path) -> String {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| nix::unistd::getuid().to_string());
    format!(
        "cannot open {} for {} — vmlab runs as {user} and needs read-write access to it.\n\
         \x20 for now:   sudo setfacl -m u:{user}:rw {}\n\
         \x20 for good:  a udev rule, e.g. SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{}\", \
         ATTR{{idProduct}}==\"{}\", TAG+=\"uaccess\" in /etc/udev/rules.d/70-vmlab-usb.rules",
        node.display(),
        dev.describe(),
        node.display(),
        dev.vendor_id,
        dev.product_id,
    )
}

/// Everything an attach decided before QEMU is asked: which device, and the
/// `device_add` arguments that pin it.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachPlan {
    pub device: HostDevice,
    pub args: serde_json::Value,
}

/// Decide an attach (PRD §5.2.1): resolve `sel` among `host`, refuse a
/// device another machine holds (`held` maps a port to its holder, `lab/vm`),
/// refuse a HID keyboard or mouse unless `force`, and refuse a node this
/// process cannot open (`accessible`). The VM-side refusals — not running,
/// no `usb = true`, a container — come first, from the machine.
pub fn plan_attach(
    sel: &Selector,
    host: &[HostDevice],
    held: &[(String, String)],
    force: bool,
    dev_root: &Path,
    accessible: impl Fn(&Path) -> bool,
) -> Result<AttachPlan, String> {
    let dev = resolve(sel, host)?;
    if let Some((_, holder)) = held.iter().find(|(port, _)| *port == dev.port) {
        return Err(format!(
            "{} is already attached to {holder} — `vmlab usb detach {holder} port:{}` first",
            dev.describe(),
            dev.port
        ));
    }
    if !force && let Some(kind) = dev.host_input() {
        return Err(format!(
            "{} is a HID {kind} — the host is probably using it, and it leaves the host while \
             attached. Pass --force (wscript: `usb_attach_force`) if that is what you want",
            dev.describe()
        ));
    }
    let node = dev.node(dev_root);
    if !accessible(&node) {
        return Err(permission_refusal(dev, &node));
    }
    Ok(AttachPlan {
        device: dev.clone(),
        args: serde_json::json!({
            "driver": "usb-host",
            "id": dev.qdev_id(),
            "bus": format!("{CONTROLLER_ID}.0"),
            "hostbus": dev.bus,
            // The device number pins this plug-in: a different device later
            // plugged into the same socket does not match, so QEMU's
            // auto-attach cannot hand it to the guest unasked.
            "hostaddr": dev.devnum,
            "hostport": dev.hostport(),
        }),
    })
}

/// The refusal for a VM booted without the controller.
pub fn no_controller_refusal(vm: &str) -> String {
    format!("\"{vm}\" has no USB controller to attach to — set `usb = true` on {vm} and restart it")
}

/// The refusal for a container.
pub fn container_refusal(name: &str) -> String {
    format!("\"{name}\" is a container — USB passthrough is for VMs")
}

/// The refusal for an online snapshot while devices are attached.
pub fn snapshot_refusal(vm: &str, attached: &[Attachment]) -> String {
    format!(
        "\"{vm}\" holds host USB devices ({}) — an online snapshot of it could only be \
         restored with the same devices attached at the same ports; detach them first \
         (`vmlab usb detach {vm} --all`)",
        attached
            .iter()
            .map(|a| a.device.describe())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
pub(crate) mod tests;
