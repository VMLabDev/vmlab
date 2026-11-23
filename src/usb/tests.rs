//! Host USB listing, selectors and the attach decision, over a fake sysfs.

use std::path::Path;

use super::*;

/// One device directory in a fake sysfs tree: `attrs` are its files,
/// `ifaces` its `(class, subclass, protocol)` interfaces.
fn device(root: &Path, port: &str, attrs: &[(&str, &str)], ifaces: &[(u8, u8, u8)]) {
    let dir = root.join(port);
    std::fs::create_dir_all(&dir).unwrap();
    for (k, v) in attrs {
        std::fs::write(dir.join(k), format!("{v}\n")).unwrap();
    }
    for (i, (c, s, p)) in ifaces.iter().enumerate() {
        let iface = dir.join(format!("{port}:1.{i}"));
        std::fs::create_dir_all(&iface).unwrap();
        std::fs::write(iface.join("bInterfaceClass"), format!("{c:02x}\n")).unwrap();
        std::fs::write(iface.join("bInterfaceSubClass"), format!("{s:02x}\n")).unwrap();
        std::fs::write(iface.join("bInterfaceProtocol"), format!("{p:02x}\n")).unwrap();
        // sysfs also lists each interface at the top level.
        std::os::unix::fs::symlink(&iface, root.join(format!("{port}:1.{i}"))).unwrap();
    }
}

/// A root hub, a hub, a HID receiver (keyboard + mouse interfaces) at
/// `1-2.4` (`/dev/bus/usb/001/015`), and two identical flash drives told
/// apart by serial: `AA11` at `1-10` (`001/007`), `BB22` at `1-9`
/// (`001/006`).
pub(crate) fn fake_sysfs() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    device(
        root,
        "usb1",
        &[
            ("busnum", "1"),
            ("devnum", "1"),
            ("idVendor", "1d6b"),
            ("idProduct", "0002"),
            ("bDeviceClass", "09"),
            ("speed", "480"),
        ],
        &[(9, 0, 0)],
    );
    device(
        root,
        "1-2",
        &[
            ("busnum", "1"),
            ("devnum", "2"),
            ("idVendor", "05e3"),
            ("idProduct", "0610"),
            ("bDeviceClass", "09"),
            ("product", "USB2.1 Hub"),
            ("speed", "480"),
        ],
        &[(9, 0, 0)],
    );
    device(
        root,
        "1-2.4",
        &[
            ("busnum", "1"),
            ("devnum", "15"),
            ("idVendor", "046D"),
            ("idProduct", "c547"),
            ("bDeviceClass", "00"),
            ("manufacturer", "Logitech"),
            ("product", "USB Receiver"),
            ("speed", "12"),
        ],
        &[(3, 1, 2), (3, 1, 1), (3, 0, 0)],
    );
    for (port, devnum, serial) in [("1-10", "7", "AA11"), ("1-9", "6", "BB22")] {
        device(
            root,
            port,
            &[
                ("busnum", "1"),
                ("devnum", devnum),
                ("idVendor", "0781"),
                ("idProduct", "5581"),
                ("bDeviceClass", "00"),
                ("manufacturer", "SanDisk"),
                ("product", "Ultra"),
                ("serial", serial),
                ("speed", "5000"),
            ],
            &[(8, 6, 0x50)],
        );
    }
    dir
}

#[test]
fn scan_reads_devices_and_hides_hubs_and_interfaces() {
    let sys = fake_sysfs();
    let devs = scan(sys.path()).unwrap();
    let ports: Vec<&str> = devs.iter().map(|d| d.port.as_str()).collect();
    // No root hub, no hub, no interface entries; ports in numeric order.
    assert_eq!(ports, ["1-2.4", "1-9", "1-10"]);

    let rx = &devs[0];
    assert_eq!(rx.id(), "046d:c547", "vendor id lowercased");
    assert_eq!(rx.bus, 1);
    assert_eq!(rx.devnum, 15);
    assert_eq!(rx.manufacturer.as_deref(), Some("Logitech"));
    assert_eq!(rx.product.as_deref(), Some("USB Receiver"));
    assert_eq!(rx.serial, None);
    assert_eq!(rx.speed_name(), "full");
    assert_eq!(rx.interfaces.len(), 3);
    assert_eq!(rx.hostport(), "2.4");
    assert_eq!(rx.qdev_id(), "usbhost-1-2.4");
    assert_eq!(
        rx.node(Path::new("/dev/bus/usb")),
        Path::new("/dev/bus/usb/001/015")
    );

    let flash = &devs[1];
    assert_eq!(flash.serial.as_deref(), Some("BB22"));
    assert_eq!(flash.speed_name(), "super");
    assert_eq!(flash.host_input(), None);
}

#[test]
fn scan_of_a_missing_root_is_empty() {
    let sys = tempfile::tempdir().unwrap();
    assert!(scan(&sys.path().join("nope")).unwrap().is_empty());
}

#[test]
fn selectors_parse_and_reject() {
    assert_eq!(
        Selector::parse("046D:C547").unwrap(),
        Selector::Id {
            vendor: "046d".into(),
            product: "c547".into(),
            serial: None
        }
    );
    assert_eq!(
        Selector::parse("0781:5581@AA11").unwrap().to_string(),
        "0781:5581@AA11"
    );
    assert_eq!(
        Selector::parse("port:1-2.1.4").unwrap(),
        Selector::Port("1-2.1.4".into())
    );
    for bad in [
        "046d",
        "46d:c547",
        "046d:c547@",
        "port:1",
        "port:x-1",
        "port:1-2.",
        "mouse",
    ] {
        let e = Selector::parse(bad).unwrap_err();
        assert!(e.contains("is not a USB selector"), "{bad}: {e}");
    }
}

#[test]
fn resolution_is_exact_or_refused() {
    let sys = fake_sysfs();
    let devs = scan(sys.path()).unwrap();
    let sel = |s: &str| Selector::parse(s).unwrap();

    assert_eq!(resolve(&sel("046d:c547"), &devs).unwrap().port, "1-2.4");
    assert_eq!(resolve(&sel("0781:5581@AA11"), &devs).unwrap().port, "1-10");
    assert_eq!(resolve(&sel("port:1-9"), &devs).unwrap().port, "1-9");

    // Two identical drives: ambiguous, and every candidate listed by port.
    let e = resolve(&sel("0781:5581"), &devs).unwrap_err();
    assert!(e.contains("matches 2 USB devices"), "{e}");
    assert!(e.contains("port:1-9") && e.contains("port:1-10"), "{e}");

    let e = resolve(&sel("dead:beef"), &devs).unwrap_err();
    assert_eq!(e, "no USB device matches `dead:beef` on this host");
    assert!(resolve(&sel("0781:5581@ZZ"), &devs).is_err());
}

fn plan(sel: &str, held: &[(String, String)], force: bool, ok: bool) -> Result<AttachPlan, String> {
    let sys = fake_sysfs();
    let devs = scan(sys.path()).unwrap();
    plan_attach(
        &Selector::parse(sel).unwrap(),
        &devs,
        held,
        force,
        Path::new("/dev/bus/usb"),
        |_| Vec::new(),
        |_| ok,
    )
}

/// Give the fake device at `port` a USB mass-storage disk, the way sysfs
/// nests one: `<iface>/host6/target6:0:0/6:0:0:0/block/<disk>`, with one
/// partition. `disk` and `part` are `(name, major:minor)`.
fn storage(root: &Path, port: &str, disk: (&str, &str), part: (&str, &str)) -> std::path::PathBuf {
    let disk_dir = root
        .join(port)
        .join(format!("{port}:1.0/host6/target6:0:0/6:0:0:0/block"))
        .join(disk.0);
    let part_dir = disk_dir.join(part.0);
    std::fs::create_dir_all(&part_dir).unwrap();
    std::fs::write(disk_dir.join("dev"), format!("{}\n", disk.1)).unwrap();
    std::fs::write(part_dir.join("dev"), format!("{}\n", part.1)).unwrap();
    // What every sysfs device directory has, and what the walk must not
    // follow: a link back up the tree.
    std::os::unix::fs::symlink(root.join(port), disk_dir.join("device")).unwrap();
    part_dir
}

/// A mount table in the format of `/proc/self/mountinfo`: the root on an
/// NVMe disk, and one line per `(major:minor, mount point)` given.
fn mount_table(mounts: &[(&str, &str)]) -> String {
    let mut t = String::from("29 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n");
    for (i, (devno, at)) in mounts.iter().enumerate() {
        t.push_str(&format!(
            "{} 29 {devno} / {at} rw,nosuid,nodev shared:{} - vfat /dev/x rw\n",
            100 + i,
            200 + i
        ));
    }
    t
}

fn plan_mounted(sys: &Path, sel: &str, table: &str, force: bool) -> Result<AttachPlan, String> {
    let devs = scan(sys).unwrap();
    plan_attach(
        &Selector::parse(sel).unwrap(),
        &devs,
        &[],
        force,
        Path::new("/dev/bus/usb"),
        |d| host_mounts(sys, d, table),
        |_| true,
    )
}

/// Review `docs/reviews/usb.md` F2: the claim disconnects the host's
/// storage driver at once, so a mounted filesystem loses its disk under its
/// dirty pages.
#[test]
fn plan_attach_refuses_a_mounted_storage_device_without_force() {
    let sys = fake_sysfs();
    storage(sys.path(), "1-9", ("sdb", "8:16"), ("sdb1", "8:17"));
    // The other, identical drive has a disk too, and nothing mounted.
    storage(sys.path(), "1-10", ("sdc", "8:32"), ("sdc1", "8:33"));
    let table = mount_table(&[("8:17", "/run/media/u/MY\\040STICK")]);

    let e = plan_mounted(sys.path(), "port:1-9", &table, false).unwrap_err();
    assert!(e.contains("0781:5581"), "{e}");
    assert!(e.contains("/run/media/u/MY STICK"), "{e}");
    assert!(e.contains("sdb1"), "{e}");
    assert!(
        e.contains("--force") && e.contains("usb_attach_force"),
        "{e}"
    );

    // Forced, it attaches, as a HID keyboard does.
    assert!(plan_mounted(sys.path(), "port:1-9", &table, true).is_ok());
    // A disk nothing is mounted from attaches unforced: the other drive,
    // and this one once the host has unmounted it.
    assert!(plan_mounted(sys.path(), "port:1-10", &table, false).is_ok());
    assert!(plan_mounted(sys.path(), "port:1-9", &mount_table(&[]), false).is_ok());
}

#[test]
fn every_mount_of_the_device_is_named() {
    let sys = fake_sysfs();
    let part = storage(sys.path(), "1-9", ("sdb", "8:16"), ("sdb1", "8:17"));
    // A second partition, and a dm-crypt mapping on the first: sysfs links
    // the mapping from the partition's `holders/`.
    let part2 = part.parent().unwrap().join("sdb2");
    std::fs::create_dir_all(&part2).unwrap();
    std::fs::write(part2.join("dev"), "8:18\n").unwrap();
    let dm = sys.path().join("virtual-block-dm-0");
    std::fs::create_dir_all(&dm).unwrap();
    std::fs::write(dm.join("dev"), "254:0\n").unwrap();
    std::fs::create_dir_all(part.join("holders")).unwrap();
    std::os::unix::fs::symlink(&dm, part.join("holders/dm-0")).unwrap();
    // 8:17 itself is not mounted: the filesystem is on the mapping. 8:18 is
    // mounted twice at one place and once at another.
    let table = mount_table(&[
        ("8:18", "/mnt/b"),
        ("254:0", "/mnt/a"),
        ("8:18", "/mnt/b"),
        ("8:180", "/mnt/not-ours"),
    ]);
    let devs = scan(sys.path()).unwrap();
    let dev = resolve(&Selector::parse("port:1-9").unwrap(), &devs).unwrap();
    let got: Vec<(String, String)> = host_mounts(sys.path(), dev, &table)
        .into_iter()
        .map(|m| (m.block, m.mount_point))
        .collect();
    assert_eq!(
        got,
        [
            ("dm-0".to_string(), "/mnt/a".to_string()),
            ("sdb2".to_string(), "/mnt/b".to_string()),
        ]
    );
    let e = plan_mounted(sys.path(), "port:1-9", &table, false).unwrap_err();
    assert!(e.contains("/mnt/a") && e.contains("/mnt/b"), "{e}");
    assert!(!e.contains("not-ours"), "{e}");
    // A device with no disk at all has no mounts.
    let hid = resolve(&Selector::parse("046d:c547").unwrap(), &devs).unwrap();
    assert!(host_mounts(sys.path(), hid, &table).is_empty());
}

#[test]
fn a_hid_keyboard_or_mouse_needs_force() {
    let e = plan("046d:c547", &[], false, true).unwrap_err();
    // The receiver has both; the keyboard is what the refusal names.
    assert!(e.contains("is a HID keyboard"), "{e}");
    assert!(e.contains("--force"), "{e}");

    let p = plan("046d:c547", &[], true, true).unwrap();
    assert_eq!(
        p.args,
        serde_json::json!({
            "driver": "usb-host",
            "id": "usbhost-1-2.4",
            "bus": "usbx.0",
            "hostbus": 1,
            "hostaddr": 15,
            "hostport": "2.4",
        })
    );
    // A flash drive is not host input.
    assert!(plan("port:1-9", &[], false, true).is_ok());
}

#[test]
fn mouse_only_device_is_named_a_mouse() {
    let d = HostDevice {
        port: "1-1".into(),
        bus: 1,
        devnum: 3,
        vendor_id: "1234".into(),
        product_id: "5678".into(),
        manufacturer: None,
        product: None,
        serial: None,
        speed: "12".into(),
        class: 0,
        interfaces: vec![Interface {
            class: 3,
            subclass: 1,
            protocol: 2,
        }],
    };
    assert_eq!(d.host_input(), Some("mouse"));
}

#[test]
fn a_held_device_is_refused_naming_the_holder() {
    let held = vec![("1-9".to_string(), "lab1/dc01".to_string())];
    let e = plan("port:1-9", &held, true, true).unwrap_err();
    assert!(e.contains("already attached to lab1/dc01"), "{e}");
    // Holding one drive does not hold the other.
    assert!(plan("port:1-10", &held, true, true).is_ok());
}

#[test]
fn an_unopenable_node_is_refused_with_both_fixes() {
    let e = plan("port:1-9", &[], false, false).unwrap_err();
    assert!(e.contains("cannot open /dev/bus/usb/001/006"), "{e}");
    assert!(
        e.contains(":rw /dev/bus/usb/001/006") && e.contains("sudo setfacl -m u:"),
        "{e}"
    );
    assert!(
        e.contains("udev rule") && e.contains("ATTR{idVendor}==\"0781\""),
        "{e}"
    );
}

#[test]
fn the_permission_check_reads_the_real_mode() {
    let dir = tempfile::tempdir().unwrap();
    let node = dir.path().join("015");
    std::fs::write(&node, b"").unwrap();
    assert!(node_accessible(&node));
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o444)).unwrap();
    // Root can write anything; the check is only meaningful for a user.
    if !nix::unistd::getuid().is_root() {
        assert!(!node_accessible(&node));
    }
    assert!(!node_accessible(&dir.path().join("missing")));
}

#[test]
fn departed_attachments_are_the_unplugged_and_the_replugged() {
    let sys = fake_sysfs();
    let devs = scan(sys.path()).unwrap();
    let attach = |d: &HostDevice| Attachment {
        machine: "dc01".into(),
        qdev_id: d.qdev_id(),
        device: d.clone(),
    };
    let mut gone = devs[1].clone();
    gone.port = "1-7".into();
    let mut replugged = devs[0].clone();
    replugged.devnum = 99;
    let attached = vec![attach(&devs[2]), attach(&gone), attach(&replugged)];
    let ports: Vec<&str> = departed(&attached, &devs)
        .iter()
        .map(|a| a.device.port.as_str())
        .collect();
    assert_eq!(ports, ["1-7", "1-2.4"]);

    let e =
        resolve_attached(&Selector::parse("0781:5581").unwrap(), "dc01", &attached).unwrap_err();
    assert!(
        e.contains("matches 2 USB devices attached to \"dc01\""),
        "{e}"
    );
}
