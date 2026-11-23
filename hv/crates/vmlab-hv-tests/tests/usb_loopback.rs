//! Automated USB loopback through Linux's virtual USB hardware.
//!
//! The brief asks for a loopback over dummy_hcd/gadget or usbip "if
//! available without root". Both need root to set up: dummy_hcd is a
//! module (modprobe) and its gadget is configured through configfs, and
//! usbip binds ports through usbip-driver sysfs attributes. Without root,
//! and without a gadget the owner already created, there is nothing to
//! loop back over, so this test skips with the reason. The owner can run
//! it as root:
//!
//! ```sh
//! sudo modprobe dummy_hcd
//! sudo mkdir -p /sys/kernel/config/usb_gadget/vmlab-test
//! # ... configure a function (e.g. g_serial) and bind the UDC ...
//! sudo -E cargo test -p vmlab-hv-tests --test usb_loopback -- --nocapture
//! ```

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use vmlab_hv_devices::usb::{HostUsbDevice, UsbDevice};

/// Skip unless a dummy_hcd-backed gadget device exists that this process
/// can name. Never touches a real host device (host-hardware safety).
fn virtual_gadget() -> Option<(u32, u32)> {
    let probed = Command::new("modprobe")
        .arg("dummy_hcd")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let loaded = matches!(probed, Ok(status) if status.success());
    if !loaded && !Path::new("/sys/module/dummy_hcd").is_dir() {
        eprintln!(
            "skipping: dummy_hcd cannot be loaded without root (modprobe failed) and usbip \
             needs root to bind ports; no virtual USB device to loop back over. \
             Run as root with dummy_hcd and a gadget configured to exercise this test."
        );
        return None;
    }
    // A gadget someone configured: a non-root-hub device whose root hub is
    // driven by dummy_hcd. Only those are candidates; real hardware never
    // hangs off dummy_hcd.
    let devices = Path::new("/sys/bus/usb/devices");
    for entry in std::fs::read_dir(devices).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Interface directories contain ':'; root hubs are `usbN`.
        if name.contains(':') || name.starts_with("usb") || !name.contains('-') {
            continue;
        }
        let root_hub = format!("usb{}", name.split('-').next().unwrap_or(""));
        let root = devices.join(&root_hub);
        let driver = std::fs::read_link(root.join("driver")).ok()?;
        if !driver.to_string_lossy().contains("dummy_hcd") {
            continue;
        }
        let number = |file: &str| -> Option<u32> {
            std::fs::read_to_string(entry.path().join(file))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        return Some((number("busnum")?, number("devnum")?));
    }
    eprintln!(
        "skipping: dummy_hcd is present but no gadget device is configured; create one \
         (configfs usb_gadget) and run again"
    );
    None
}

/// Open the virtual device, take over its interfaces, run one control and
/// one data transfer, and hand it back to the host.
#[test]
#[cfg_attr(
    vmlab_ci_no_host_assets,
    ignore = "requires a configured dummy_hcd USB gadget"
)]
fn usb_loopback_over_virtual_hardware() {
    let Some((bus, devnum)) = virtual_gadget() else {
        return;
    };
    let mut host = match HostUsbDevice::open("usbhost-loopback", bus as u16, devnum as u16) {
        Ok(host) => host,
        Err(e) => {
            eprintln!("skipping: opening the virtual device failed: {e}");
            return;
        }
    };
    // A standard GET_DESCRIPTOR(Device) through the passthrough control
    // path: submitted as an async control URB, NAKed, then reaped.
    let deadline = Instant::now() + Duration::from_secs(5);
    let setup = vmlab_hv_devices::usb::SetupPacket {
        request_type: 0x80,
        request: 6,
        value: 0x0100,
        index: 0,
        length: 18,
    };
    let descriptor = loop {
        match host.control(setup, &[]) {
            vmlab_hv_devices::usb::UsbTransfer::Ack(data) if data.len() >= 2 => break data,
            vmlab_hv_devices::usb::UsbTransfer::Nak if Instant::now() < deadline => {}
            other => panic!("control transfer on the virtual device: {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(descriptor[1], 0x01, "the device descriptor type");
    // Detaching hands the device back; the drop must not fail loudly.
    drop(host);
}

// usbdevfs ioctl numbers on a 64-bit Linux (`linux/usbdevice_fs.h`); the
// device crate keeps its own private.
const USBDEVFS_CLAIMINTERFACE: libc::c_ulong = 0x8004_550f;
const USBDEVFS_RELEASEINTERFACE: libc::c_ulong = 0x8004_5510;
const USBDEVFS_IOCTL: libc::c_ulong = 0xc010_5512;
const USBDEVFS_DISCONNECT: libc::c_int = 0x5516;

/// Claim or release interface 0 of an open usbdevfs file the plain way, as
/// another vmlab-hv process or a libusb program holds it.
fn usbfs_interface_zero(file: &std::fs::File, request: libc::c_ulong) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let interface: libc::c_uint = 0;
    // SAFETY: both ioctls read one c_uint through the pointer, which is
    // valid for the call; the return value is checked.
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), request, &interface) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Unbind the kernel driver of interface 0, if one is bound: the plain
/// claim does not do that itself.
fn usbfs_disconnect_interface_zero(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct UsbdevfsIoctl {
        ifno: libc::c_int,
        ioctl_code: libc::c_int,
        data: *mut libc::c_void,
    }
    let disconnect = UsbdevfsIoctl {
        ifno: 0,
        ioctl_code: USBDEVFS_DISCONNECT,
        data: std::ptr::null_mut(),
    };
    // SAFETY: the ioctl reads one `usbdevfs_ioctl` through the pointer,
    // which is valid for the call. ENODATA (no driver bound) is expected,
    // so the result is not checked; the claim after it is.
    unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_IOCTL, &disconnect) };
}

/// Review usb.md F1: an open must not take the device from another usbdevfs
/// user. The first claim is this test's own file; the kernel must refuse
/// the second claim and leave the first one in place.
#[test]
#[cfg_attr(
    vmlab_ci_no_host_assets,
    ignore = "requires a configured dummy_hcd USB gadget"
)]
fn open_refuses_a_device_another_usbfs_user_holds() {
    let Some((bus, devnum)) = virtual_gadget() else {
        return;
    };
    let node = format!("/dev/bus/usb/{bus:03}/{devnum:03}");
    let holder = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)
    {
        Ok(holder) => holder,
        Err(e) => {
            eprintln!("skipping: opening {node} failed: {e}");
            return;
        }
    };
    usbfs_disconnect_interface_zero(&holder);
    usbfs_interface_zero(&holder, USBDEVFS_CLAIMINTERFACE)
        .expect("the first claim of interface 0 through usbfs");

    let error = match HostUsbDevice::open("usbhost-second", bus as u16, devnum as u16) {
        Ok(_) => panic!("the open took a device another usbfs user holds"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("claiming interface 0 failed"), "{error}");
    assert!(error.contains("held by another program"), "{error}");

    // The first claim is still in place: releasing it succeeds. Had the
    // open taken the interface, the kernel would answer EINVAL here.
    usbfs_interface_zero(&holder, USBDEVFS_RELEASEINTERFACE)
        .expect("the first claim survived the refused open");
}
