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
/// The host's mount table, one line per mount.
pub const MOUNT_TABLE: &str = "/proc/self/mountinfo";
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
    /// The three strings below are the device's own string descriptors, so
    /// whoever made or reprogrammed the device chose them. They are held
    /// cleaned ([`clean_device_string`]) whichever way a `HostDevice` is
    /// made: read from sysfs, or decoded from a daemon's reply.
    #[serde(default, deserialize_with = "clean_device_string_field")]
    pub manufacturer: Option<String>,
    #[serde(default, deserialize_with = "clean_device_string_field")]
    pub product: Option<String>,
    #[serde(default, deserialize_with = "clean_device_string_field")]
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

/// A string descriptor of a device made safe to print: every control
/// character (ESC, a line break, the C1 set), every other kind of white
/// space and every invisible formatting character (bidirectional overrides,
/// zero-width characters) becomes one plain space, and the ends are trimmed.
/// The text comes from outside the host and reaches a terminal through the
/// listing, the refusals, the log and the events. `None` when nothing
/// printable is left.
pub fn clean_device_string(raw: &str) -> Option<String> {
    let unsafe_char = |c: char| {
        c.is_control()
            || c.is_whitespace()
            || matches!(
                c,
                '\u{00ad}'
                    | '\u{061c}'
                    | '\u{180e}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}'
                    | '\u{fff9}'..='\u{fffb}'
                    | '\u{e0000}'..='\u{e007f}'
            )
    };
    let cleaned: String = raw
        .chars()
        .map(|c| if unsafe_char(c) { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

/// [`clean_device_string`] on the way in from a serialised `HostDevice`: a
/// reply of a daemon is not trusted to have cleaned the strings itself.
fn clean_device_string_field<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?
        .as_deref()
        .and_then(clean_device_string))
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
    // A string descriptor: lossy, because sysfs promises no valid UTF-8 and
    // an unreadable string must not hide the device, and cleaned.
    let text = |name: &str| -> Option<String> {
        let raw = std::fs::read(dir.join(name)).ok()?;
        clean_device_string(&String::from_utf8_lossy(&raw))
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
        manufacturer: text("manufacturer"),
        product: text("product"),
        serial: text("serial"),
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

/// A filesystem the host has mounted from a USB device: the block device
/// below the USB device that carries it, and where it is mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMount {
    /// The kernel name of the block device (`sdb1`, or `dm-0` for a mapping
    /// that sits on a partition of the device).
    pub block: String,
    pub mount_point: String,
}

/// How deep the walk below a USB device goes looking for its disks. A disk
/// sits at `<iface>/host<N>/target<..>/<h:c:t:l>/block/<disk>`, five levels
/// down; the bound only keeps a strange tree from being walked for ever.
const BLOCK_WALK_DEPTH: usize = 12;
/// How many layers of holders (dm-crypt, LVM, md) are followed above a disk.
const HOLDER_DEPTH: usize = 8;

/// One block device below a USB device.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Block {
    /// The kernel name (`sdb1`, `dm-0`).
    name: String,
    /// `major:minor`.
    devno: String,
    /// The name of a device-mapper mapping (`/dev/mapper/<name>`).
    dm_name: Option<String>,
}

impl Block {
    /// Whether `source`, the mount source of a mount table line, names this
    /// block device: `/dev/sdb1`, `/dev/mapper/backup`, or one member of a
    /// multi-device list (`/dev/sda1:/dev/sdb1`, as bcachefs writes it).
    fn is_source(&self, source: &str) -> bool {
        source.split(':').any(|path| {
            let Some(below_dev) = path.strip_prefix("/dev/") else {
                return false;
            };
            below_dev == self.name
                || self
                    .dm_name
                    .as_deref()
                    .is_some_and(|dm| below_dev.strip_prefix("mapper/") == Some(dm))
        })
    }
}

/// Every filesystem of `dev` the host has mounted: the block devices sysfs
/// lists below the device at `sysfs_root`, their partitions and whatever
/// holds them (a dm-crypt or LVM mapping), each looked up in `mount_table`
/// (the text of [`MOUNT_TABLE`]) two ways: by its `major:minor`, and by its
/// name as the mount source. The second is the only one that finds a
/// filesystem mounted under an anonymous device number (`0:45`), as btrfs
/// and bcachefs are. Sorted by mount point. An unreadable entry is skipped,
/// as in [`scan`].
pub fn host_mounts(sysfs_root: &Path, dev: &HostDevice, mount_table: &str) -> Vec<HostMount> {
    let mut blocks = Vec::new();
    find_blocks(&sysfs_root.join(&dev.port), BLOCK_WALK_DEPTH, &mut blocks);
    let mut out: Vec<HostMount> = Vec::new();
    for line in mount_table.lines() {
        // `36 35 8:17 / /run/media/u/STICK rw,... - vfat /dev/sdb1 rw`
        // `36 35 0:45 / /mnt/stick rw,... shared:1 - btrfs /dev/sdb1 rw`
        let mut fields = line.split(' ');
        let (Some(devno), Some(mount_point)) = (fields.nth(2), fields.nth(1)) else {
            continue;
        };
        // After the optional fields and their ` - ` end mark: the
        // filesystem type, then the mount source.
        let source = fields
            .skip_while(|f| *f != "-")
            .nth(2)
            .map(unescape_mount)
            .unwrap_or_default();
        if let Some(block) = blocks
            .iter()
            .find(|b| b.devno == devno)
            .or_else(|| blocks.iter().find(|b| b.is_source(&source)))
        {
            let mount = HostMount {
                block: block.name.clone(),
                mount_point: unescape_mount(mount_point),
            };
            if !out.contains(&mount) {
                out.push(mount);
            }
        }
    }
    out.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    out
}

/// Collect every block device below `dir`. Symlinks
/// are not followed on the way down: sysfs is full of them (`subsystem`,
/// `driver`, `device`) and they lead out of the device.
fn find_blocks(dir: &Path, depth: usize, out: &mut Vec<Block>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        if entry.file_name() == "block" {
            if let Ok(disks) = std::fs::read_dir(&path) {
                for disk in disks.flatten() {
                    block_and_parts(&disk.path(), out);
                }
            }
        } else if depth > 0 {
            find_blocks(&path, depth - 1, out);
        }
    }
}

/// One disk directory: the disk, each partition, and the holders of both.
fn block_and_parts(disk: &Path, out: &mut Vec<Block>) {
    block_and_holders(disk, HOLDER_DEPTH, out);
    if let Ok(entries) = std::fs::read_dir(disk) {
        for entry in entries.flatten() {
            let part = entry.path();
            // A partition is a subdirectory with a `dev` of its own.
            if entry.file_type().is_ok_and(|t| t.is_dir()) && part.join("dev").is_file() {
                block_and_holders(&part, HOLDER_DEPTH, out);
            }
        }
    }
}

/// One block device directory, and — through its `holders/` links — every
/// mapping stacked on it.
fn block_and_holders(block: &Path, depth: usize, out: &mut Vec<Block>) {
    let (Some(name), Ok(devno)) = (
        block.file_name().map(|n| n.to_string_lossy().into_owned()),
        std::fs::read_to_string(block.join("dev")),
    ) else {
        return;
    };
    let entry = Block {
        name,
        devno: devno.trim().to_string(),
        dm_name: std::fs::read_to_string(block.join("dm/name"))
            .ok()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
    };
    if out.contains(&entry) {
        return;
    }
    out.push(entry);
    if depth == 0 {
        return;
    }
    if let Ok(holders) = std::fs::read_dir(block.join("holders")) {
        for holder in holders.flatten() {
            block_and_holders(&holder.path(), depth - 1, out);
        }
    }
}

/// A mount point as the mount table spells it, with its octal escapes
/// (`\040` for a space) turned back.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = (b[i] == b'\\' && i + 4 <= b.len())
            .then(|| std::str::from_utf8(&b[i + 1..i + 4]).ok())
            .flatten()
            .and_then(|d| u8::from_str_radix(d, 8).ok());
        match octal {
            Some(c) => {
                out.push(c);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The refusal for a device the host has a filesystem mounted from. The
/// attach disconnects the host's storage driver at once, so what the host
/// has not written yet is lost and the mount is left without its disk.
pub fn mounted_refusal(dev: &HostDevice, mounts: &[HostMount]) -> String {
    format!(
        "{} carries a filesystem the host has mounted ({}) — an attach takes the disk away \
         under it, and data the host has not written yet is lost. Unmount it first, or pass \
         --force (wscript: `usb_attach_force`) if that is what you want",
        dev.describe(),
        mounts
            .iter()
            .map(|m| format!("{} at {}", m.block, m.mount_point))
            .collect::<Vec<_>>()
            .join(", ")
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
/// refuse a HID keyboard or mouse unless `force`, refuse a device that
/// carries a filesystem the host has mounted (`mounted`, [`host_mounts`])
/// unless `force`, and refuse a node this process cannot open
/// (`accessible`). The VM-side refusals — not running,
/// no `usb = true`, a container — come first, from the machine.
pub fn plan_attach(
    sel: &Selector,
    host: &[HostDevice],
    held: &[(String, String)],
    force: bool,
    dev_root: &Path,
    mounted: impl Fn(&HostDevice) -> Vec<HostMount>,
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
    if !force {
        let mounts = mounted(dev);
        if !mounts.is_empty() {
            return Err(mounted_refusal(dev, &mounts));
        }
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
///
/// Not a QEMU limit: 11.1 saves a `usb-host` device's state and loads it
/// back while the device is still attached. Loading it once the device is
/// gone fails ("Unknown section or instance '…/usb-host'"), and nothing
/// guarantees the device is plugged in, at that port, on the day of the
/// restore — so the snapshot is refused rather than taken unrestorable.
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
