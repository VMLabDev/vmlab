//! S1: explicit, pre-bound VFIO assignment on the Ubuntu 26.04 template.

use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Ctrl, Event, Outcome, ShutdownReason};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn ubuntu_vfio_enumeration_reset_and_second_start() {
    let Ok(address) = std::env::var("VMLAB_HV_TEST_VFIO_DEVICE") else {
        eprintln!("SKIP VFIO S1: VMLAB_HV_TEST_VFIO_DEVICE is unset; no host device is claimed");
        return;
    };
    assert!(Path::new("/dev/kvm").exists(), "VFIO S1 requires /dev/kvm");
    let (code, vars) = ovmf().expect("VFIO S1 requires matching OVMF CODE and VARS files");
    let template = std::env::var_os("VMLAB_HV_TEST_VM_DISK")
        .map(PathBuf::from)
        .or_else(|| {
            let root = PathBuf::from(std::env::var_os("HOME")?)
                .join(".local/share/vmlab/templates/x86_64/ubuntu-26.04");
            let mut paths: Vec<_> = std::fs::read_dir(root)
                .ok()?
                .flatten()
                .map(|e| e.path().join("disk.qcow2"))
                .filter(|p| p.is_file())
                .collect();
            paths.sort();
            paths.pop()
        });
    let template = template.expect("ubuntu-26.04 template absent; set VMLAB_HV_TEST_VM_DISK");
    let dir = tempfile::tempdir().unwrap();
    let overlay = dir.path().join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &overlay).unwrap();
    for first_start in [true, false] {
        let path = dir.path().join(if first_start { "first" } else { "second" });
        std::fs::create_dir(&path).unwrap();
        run_vm(&path, &overlay, &code, &vars, &address, first_start);
    }
}

fn ovmf() -> Option<(PathBuf, PathBuf)> {
    let directories = std::env::var_os("VMLAB_HV_FIRMWARE_DIR")
        .map(PathBuf::from)
        .into_iter()
        .chain([
            PathBuf::from("/usr/share/OVMF"),
            PathBuf::from("/usr/share/OVMF/x64"),
            PathBuf::from("/usr/share/edk2/x64"),
        ]);
    for dir in directories {
        for (code, vars) in [
            ("OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd"),
            ("OVMF_CODE.4m.fd", "OVMF_VARS.4m.fd"),
        ] {
            let pair = (dir.join(code), dir.join(vars));
            if pair.0.is_file() && pair.1.is_file() {
                return Some(pair);
            }
        }
    }
    None
}

fn run_vm(path: &Path, overlay: &Path, code: &Path, vars: &Path, address: &str, first_start: bool) {
    let vars_copy = path.join("vars.fd");
    std::fs::copy(vars, &vars_copy).unwrap();
    let description: vmlab_hv_proto::machine::MachineDescription = serde_json::from_value(json!({
        "name": "vfio-s1", "machine": "q35", "arch": "x86_64", "accel": "kvm",
        "vcpus": 2, "memory_mib": 4096,
        "boot": {"kind": "firmware", "firmware": "ovmf", "code": code, "vars": vars_copy},
        "console_pci": {"bus": 0, "device": 1, "function": 0},
        "display": {"device": "virtio-vga", "pci": {"bus": 0, "device": 3, "function": 0}},
        "vfio": [{"host": address, "pci": {"bus": 0, "device": 6, "function": 0}}],
        "disks": [{"id": "disk0", "path": overlay, "format": "qcow2", "read_only": false,
            "vmstate": true, "pci": {"bus": 0, "device": 2, "function": 0}}],
        "nics": [], "serial_log": path.join("serial.log"),
        "ports": [{"name": "vmlab.agent.0", "socket": path.join("agent.sock")}]
    }))
    .unwrap();
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec(&description).unwrap(),
    )
    .unwrap();
    let log = std::fs::File::create(path.join("hv.log")).unwrap();
    let mut child = ManagedChild::spawn(
        Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "-q",
                "-p",
                "MemoryMax=24G",
                "-p",
                "MemorySwapMax=0",
                "--",
            ])
            .arg(vmlab_hv_tests::hv_binary())
            .arg("--machine")
            .arg(path.join("machine.json"))
            .arg("--control")
            .arg(path.join("control.sock"))
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log)),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut control = loop {
        if let Ok(client) = Client::connect(path.join("control.sock")) {
            break client;
        }
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "VFIO launch failed: {}",
            std::fs::read_to_string(path.join("hv.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let diagnostics = || {
        format!(
            "VFIO VM in {}\nhypervisor:\n{}\nguest serial:\n{}",
            path.display(),
            std::fs::read_to_string(path.join("hv.log")).unwrap_or_default(),
            std::fs::read_to_string(path.join("serial.log")).unwrap_or_default(),
        )
    };
    let connect_agent = || {
        AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(180))
            .unwrap_or_else(|error| panic!("VFIO agent: {error}\n{}", diagnostics()))
    };
    let mut agent = connect_agent();
    check_pci_identity(&mut agent, address);
    eprintln!(
        "VFIO S1: PCI function {address} enumerated on {}",
        path.display()
    );
    if first_start {
        check_snapshot_refusals(&mut control);
        let boot = boot_id(&mut agent);
        agent.shutdown("reboot", Duration::from_secs(30)).unwrap();
        drop(agent);
        assert_eq!(
            wait_for_lifecycle_event(&mut control),
            Some(Event::Reset),
            "{}",
            diagnostics()
        );
        let mut agent = connect_agent();
        assert_ne!(boot_id(&mut agent), boot, "guest boot ID after reset");
        check_pci_identity(&mut agent, address);
        eprintln!("VFIO S1: guest rebooted and enumerated PCI function {address}");
        agent.shutdown("powerdown", Duration::from_secs(30)).unwrap();
        assert_eq!(
            wait_for_lifecycle_event(&mut control),
            Some(Event::Shutdown {
                reason: ShutdownReason::GuestShutdown,
            }),
            "{}",
            diagnostics()
        );
    } else {
        assert!(matches!(
            control.call(Ctrl::Quit).unwrap(),
            Outcome::Ok { .. }
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}\n{}", diagnostics());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "VFIO VM failed to exit\n{}",
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let log = std::fs::read_to_string(path.join("hv.log")).unwrap();
    for error in [
        "VFIO_GROUP_UNSET_CONTAINER",
        "Could not unbind VFIO group",
        "Could not delete VFIO group",
        "VFIO reset at VM exit",
        "removing VFIO BAR KVM slot",
        "unmapping VFIO BAR",
        "unmapping VFIO guest DMA",
    ] {
        assert!(!log.contains(error), "VFIO teardown: {error}\n{log}");
    }
    eprintln!("VFIO S1: clean exit on {}", path.display());
}

fn wait_for_lifecycle_event(control: &mut Client) -> Option<Event> {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = control.wait_event_timeout(remaining).unwrap()?;
        if matches!(
            event,
            Event::Reset | Event::Shutdown { .. } | Event::Exiting { .. }
        ) {
            return Some(event);
        }
    }
}

fn boot_id(agent: &mut AgentClient) -> Vec<u8> {
    let output = agent
        .exec(
            &["cat", "/proc/sys/kernel/random/boot_id"],
            Duration::from_secs(30),
        )
        .unwrap();
    assert_eq!(output.exit_code, 0);
    assert!(!output.stdout.is_empty());
    output.stdout
}

fn check_pci_identity(agent: &mut AgentClient, address: &str) {
    let output = agent
        .exec(&["lspci", "-n", "-s", "00:06.0"], Duration::from_secs(30))
        .unwrap();
    assert_eq!(output.exit_code, 0);
    let found = String::from_utf8_lossy(&output.stdout);
    assert!(
        found.starts_with("00:06.0 "),
        "missing guest VFIO function: {found}"
    );
    // Only read host identity after the VMM has validated the explicit BDF.
    let host = PathBuf::from("/sys/bus/pci/devices").join(address);
    let vendor = std::fs::read_to_string(host.join("vendor")).unwrap();
    let device = std::fs::read_to_string(host.join("device")).unwrap();
    assert!(
        found.contains(&format!(
            "{}:{}",
            vendor.trim().trim_start_matches("0x"),
            device.trim().trim_start_matches("0x")
        )),
        "host/guest PCI identity mismatch: {found}"
    );
}

fn check_snapshot_refusals(control: &mut Client) {
    for command in [
        Ctrl::SnapshotSave {
            name: "assigned".into(),
        },
        Ctrl::SnapshotLoad {
            name: "assigned".into(),
        },
    ] {
        let outcome = control.call(command).unwrap();
        assert!(
            !matches!(outcome, Outcome::Ok { .. }),
            "snapshot unexpectedly accepted"
        );
        assert!(format!("{outcome:?}").contains("VFIO"));
    }
}
