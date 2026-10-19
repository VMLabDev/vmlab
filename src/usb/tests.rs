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
        |_| ok,
    )
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
