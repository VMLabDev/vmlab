//! The aarch64 `virt` machine through the executable's control socket
//! (seam S1; phase 3d, card C10): a guest of a few words that needs no host
//! asset, and AAVMF to its UEFI shell on the serial log. With a display
//! (#1225): the display sockets beside the control socket, and AAVMF to its
//! shell on the virtio GPU and the USB keyboard.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Control, Event, Outcome};
use vmlab_hv_tests::process::ManagedChild;

fn binary() -> PathBuf {
    std::env::var_os("VMLAB_HV_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("vmlab-hv")
        })
}

struct Machine {
    process: ManagedChild,
    client: Client,
    serial: PathBuf,
    stderr: PathBuf,
}

impl Machine {
    /// Start `vmlab-hv --paused` on `description`, which names its serial
    /// log in `dir`, and connect to its control socket.
    fn start(dir: &Path, description: &serde_json::Value) -> Self {
        Self::start_with(dir, description, None)
    }

    /// `start`, with `VMLAB_HV_JIT` set to `1` or `0` when `jit` names it
    /// (3e DESIGN.md K7): the JIT is on by default.
    fn start_with(dir: &Path, description: &serde_json::Value, jit: Option<bool>) -> Self {
        let machine = dir.join("machine.json");
        let control = dir.join("control.sock");
        let stderr = dir.join("stderr.log");
        std::fs::write(&machine, serde_json::to_vec(description).unwrap()).unwrap();
        let mut process = ManagedChild::spawn(
            Command::new(binary())
                .arg("--machine")
                .arg(&machine)
                .arg("--control")
                .arg(&control)
                .arg("--paused")
                .envs(
                    std::env::var("VMLAB_HV_VIRT_LOG")
                        .ok()
                        .map(|filter| ("VMLAB_HV_LOG", filter)),
                )
                .envs(jit.map(|on| ("VMLAB_HV_JIT", if on { "1" } else { "0" })))
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&stderr).unwrap()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let client = loop {
            if let Ok(client) = Client::connect(&control) {
                break client;
            }
            assert!(
                process.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "vmlab-hv did not serve its control socket: {}",
                std::fs::read_to_string(&stderr).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        Self {
            process,
            client,
            serial: dir.join("serial.log"),
            stderr,
        }
    }

    fn serial(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(&self.serial).unwrap_or_default()).into_owned()
    }

    fn diagnostics(&self) -> String {
        let serial = self.serial();
        let tail = serial.len().saturating_sub(4000);
        format!(
            "stderr: {}\nserial (its end): {}",
            std::fs::read_to_string(&self.stderr).unwrap_or_default(),
            &serial[serial.ceil_char_boundary(tail)..]
        )
    }
}

fn description(
    dir: &Path,
    vcpus: u32,
    memory_mib: u64,
    boot: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "name": "virt", "arch": "aarch64", "accel": "tcg", "machine": "virt",
        "vcpus": vcpus, "memory_mib": memory_mib, "boot": boot,
        "disks": [], "nics": [], "ports": [],
        "serial_log": dir.join("serial.log"),
    })
}

/// A kernel of a few words: write to the PL011, then PSCI `SYSTEM_OFF`.
#[test]
fn a_virt_machine_runs_without_kvm_and_powers_off() {
    use vmlab_hv_tcg_aarch64::oracle::asm;
    let dir = tempfile::Builder::new()
        .prefix("hv-virt-")
        .tempdir()
        .unwrap();
    let mut words = asm::load(1, 0x0900_0000).to_vec();
    for byte in *b"S1\n" {
        // movz x2, #byte ; strb w2, [x1]
        words.extend([asm::movz(2, u16::from(byte), 0), 0x3900_0022]);
    }
    words.extend(asm::load(0, 0x8400_0008));
    words.extend([asm::hvc(0), asm::b(0)]);
    let kernel = dir.path().join("kernel");
    std::fs::write(&kernel, asm::bytes(&words)).unwrap();
    let mut machine = Machine::start(
        dir.path(),
        &description(
            dir.path(),
            2,
            64,
            serde_json::json!({"kind": "kernel", "kernel": kernel, "initrd": null, "cmdline": ""}),
        ),
    );
    assert!(matches!(
        machine.client.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let status = machine.process.wait().unwrap();
    assert!(status.success(), "{status}: {}", machine.diagnostics());
    assert_eq!(machine.serial(), "S1\n");
}

/// The AAVMF images of `guest/fetch-firmware.sh aarch64`.
fn aavmf() -> Option<(PathBuf, PathBuf)> {
    let root = std::env::var_os("VMLAB_HV_FIRMWARE_DIR_AARCH64")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../guest/dist/firmware/aarch64")
        });
    let code = root.join("AAVMF_CODE.fd");
    let vars = root.join("AAVMF_VARS.fd");
    (code.is_file() && vars.is_file()).then_some((code, vars))
}

/// The keys that take this AAVMF from its boot manager to its shell: each
/// is sent when the serial log holds the text, after the keys before it.
/// Without a disk the firmware says that it found nothing to boot and waits
/// for a key (QEMU shows the same). The key opens the boot menu, whose
/// second entry is the shell, and the last key ends the wait for
/// `startup.nsh`.
const TO_THE_SHELL: [(&str, &[u8]); 4] = [
    ("Press any key to enter the Boot Manager Menu", b"x"),
    ("EFI Internal Shell", b"\x1b[B"),
    ("EFI Internal Shell", b"\r"),
    ("any other key to continue", b"\r"),
];

/// AAVMF, unchanged, from the reset vector to the prompt of its UEFI shell
/// (the proof of card C10), with the keys of `TO_THE_SHELL` on the PL011.
fn aavmf_reaches_the_shell(vcpus: u32) {
    let Some((code, pristine)) = aavmf() else {
        eprintln!(
            "skipping: AAVMF is absent; run guest/fetch-firmware.sh aarch64 or set VMLAB_HV_FIRMWARE_DIR_AARCH64"
        );
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("hv-virt-")
        .tempdir()
        .unwrap();
    let vars = dir.path().join("AAVMF_VARS.fd");
    std::fs::copy(pristine, &vars).unwrap();
    let console = dir.path().join("console.sock");
    let mut description = description(
        dir.path(),
        vcpus,
        1024,
        serde_json::json!({"kind": "firmware", "firmware": "aavmf", "code": code, "vars": vars}),
    );
    description["ports"] =
        serde_json::json!([{"transport": "com1", "name": "console", "socket": console}]);
    let mut machine = Machine::start(dir.path(), &description);
    let mut keys = std::os::unix::net::UnixStream::connect(&console).unwrap();
    let mut step = 0;
    let mut sent = Instant::now();
    assert!(matches!(
        machine.client.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let timeout = std::env::var("VMLAB_HV_VIRT_TIMEOUT")
        .ok()
        .map(|value| value.parse().unwrap())
        .unwrap_or(600);
    let start = Instant::now();
    let deadline = start + Duration::from_secs(timeout);
    let mut resets = 0;
    while !machine.serial().contains("Shell>") {
        // A key goes out when its text is there and the menu had half a
        // second to draw what the key before it changed.
        if let Some((text, bytes)) = TO_THE_SHELL.get(step)
            && machine.serial().contains(text)
            && sent.elapsed() > Duration::from_millis(500)
        {
            use std::io::Write;
            keys.write_all(bytes).unwrap();
            step += 1;
            sent = Instant::now();
        }
        assert!(
            machine.process.try_wait().unwrap().is_none(),
            "vmlab-hv ended before the UEFI shell: {}",
            machine.diagnostics()
        );
        machine.client.call(Control::Status).unwrap();
        resets += machine
            .client
            .take_events()
            .iter()
            .filter(|event| **event == Event::Reset)
            .count();
        if Instant::now() >= deadline {
            let _ = machine.client.call(Control::Quit);
            let _ = machine.process.wait();
            panic!(
                "no UEFI shell in {timeout} seconds: {}",
                machine.diagnostics()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let serial = machine.serial();
    assert!(serial.contains("UEFI Interactive Shell"), "{serial}");
    eprintln!(
        "AAVMF reached its UEFI shell with {vcpus} vCPU(s) in {:.1} s, after {resets} reset(s) and {} bytes on the serial log",
        start.elapsed().as_secs_f64(),
        serial.len()
    );
    // The firmware wrote its variables through the flash bank.
    assert!(matches!(
        machine.client.call(Control::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    let status = machine.process.wait().unwrap();
    assert!(status.success(), "{status}: {}", machine.diagnostics());
}

#[test]
#[cfg_attr(
    vmlab_ci_no_host_assets,
    ignore = "requires the AAVMF images of guest/fetch-firmware.sh"
)]
fn aavmf_reaches_the_uefi_shell() {
    aavmf_reaches_the_shell(1);
}

#[test]
#[cfg_attr(
    vmlab_ci_no_host_assets,
    ignore = "requires the AAVMF images of guest/fetch-firmware.sh"
)]
fn aavmf_reaches_the_uefi_shell_with_two_cpus() {
    aavmf_reaches_the_shell(2);
}

/// The word that the counting guest of the snapshot tests counts in.
const COUNTER: u64 = vmlab_hv_firmware::virt::map::RAM_BASE + 0x10_0000;

/// A kernel that counts in `COUNTER` for ever.
fn counting_kernel(dir: &Path) -> PathBuf {
    use vmlab_hv_tcg_aarch64::oracle::asm;
    let mut words = asm::load(1, COUNTER).to_vec();
    // ldr x3, [x1] ; add x3, x3, #1 ; str x3, [x1] ; b loop
    words.extend([0xf940_0023, asm::add(3, 3, 1), 0xf900_0023, asm::b(-3)]);
    let kernel = dir.join("kernel");
    std::fs::write(&kernel, asm::bytes(&words)).unwrap();
    kernel
}

fn ok(outcome: Outcome) {
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
}

impl Machine {
    fn debug(
        &mut self,
        request: vmlab_hv_proto::debug::DebugRequest,
    ) -> vmlab_hv_proto::debug::DebugResponse {
        match self.client.call(Control::Debug { request }).unwrap() {
            Outcome::Debug { response } => response,
            other => panic!("{other:?}: {}", self.diagnostics()),
        }
    }

    /// The count of the guest. The machine is paused.
    fn count(&mut self) -> u64 {
        use vmlab_hv_proto::debug::{AddressSpace, DebugRequest, DebugResponse};
        let DebugResponse::Memory { data } = self.debug(DebugRequest::ReadMemory {
            space: AddressSpace::Physical,
            address: COUNTER,
            length: 8,
        }) else {
            panic!("no memory")
        };
        u64::from_le_bytes(data.try_into().unwrap())
    }

    /// Run the guest a while and pause it: the count then.
    fn run_a_while(&mut self) -> u64 {
        ok(self.client.call(Control::Resume).unwrap());
        std::thread::sleep(Duration::from_millis(300));
        ok(self.client.call(Control::Pause).unwrap());
        self.count()
    }

    fn quit(mut self) {
        ok(self.client.call(Control::Quit).unwrap());
        let status = self.process.wait().unwrap();
        assert!(status.success(), "{status}: {}", self.diagnostics());
    }
}

/// D13 at S1 on an emulated aarch64 machine: an online snapshot over the
/// control socket brings the guest back to its moment, in the same process
/// and in a new one on the same disk; an offline snapshot of the disk is
/// refused as a load, and the machine runs on. With the JIT, as a machine
/// runs by default (3e DESIGN.md K7), and with the interpreter.
#[test]
fn snapshots_of_an_emulated_aarch64_machine_online_and_offline() {
    snapshots_online_and_offline(true);
}

#[test]
fn snapshots_of_an_emulated_aarch64_machine_online_and_offline_on_the_interpreter() {
    snapshots_online_and_offline(false);
}

/// The stderr of a machine says how its vCPU runs: the JIT, or the
/// interpreter that `VMLAB_HV_JIT=0` asks for.
fn assert_runs_the_jit(machine: &Machine, jit: bool) {
    let log = std::fs::read_to_string(&machine.stderr).unwrap_or_default();
    let said = if jit {
        "the JIT is on"
    } else {
        "turns the JIT off"
    };
    assert!(log.contains(said), "the log does not say {said:?}: {log}");
}

fn snapshots_online_and_offline(jit: bool) {
    use vmlab_hv_proto::control::{ErrorCode, RunState};
    use vmlab_hv_proto::debug::{DebugRequest, DebugResponse};
    let dir = tempfile::Builder::new()
        .prefix("hv-virt-snap-")
        .tempdir()
        .unwrap();
    let disk = dir.path().join("disk0.qcow2");
    vmlab_disk::ops::create_qcow2(&disk, 16 << 20, None, None).unwrap();
    // An offline snapshot, as vmlab takes one of a stopped machine.
    vmlab_disk::ops::snapshot_create(&disk, "offline").unwrap();
    let kernel = counting_kernel(dir.path());
    let mut desc = description(
        dir.path(),
        1,
        64,
        serde_json::json!({"kind": "kernel", "kernel": kernel, "initrd": null, "cmdline": ""}),
    );
    desc["disks"] = serde_json::json!([{
        "id": "disk0", "path": disk, "format": "qcow2", "read_only": false, "vmstate": true,
    }]);

    let mut machine = Machine::start_with(dir.path(), &desc, Some(jit));
    let at_save = machine.run_a_while();
    assert_runs_the_jit(&machine, jit);
    assert!(
        at_save > 0,
        "the guest did not count: {}",
        machine.diagnostics()
    );
    ok(machine
        .client
        .call(Control::SnapshotSave {
            name: "online".into(),
        })
        .unwrap());
    let later = machine.run_a_while();
    assert!(later > at_save);
    ok(machine
        .client
        .call(Control::SnapshotLoad {
            name: "online".into(),
        })
        .unwrap());
    let Outcome::Ok { status } = machine.client.call(Control::Status).unwrap() else {
        panic!()
    };
    assert_eq!(status, Some(RunState::Paused));
    assert_eq!(machine.count(), at_save);
    let DebugResponse::Aarch64Registers { registers, .. } =
        machine.debug(DebugRequest::ReadRegisters { vcpu: 0 })
    else {
        panic!("no aarch64 registers")
    };
    assert_eq!(registers.x[1], COUNTER);
    assert!(registers.instructions > 0, "the vCPU counts what it runs");
    // It runs on from the save.
    assert!(machine.run_a_while() > at_save);

    // The offline snapshot has no machine state: the load is refused, and
    // the machine is as it was.
    let refused = machine
        .client
        .call(Control::SnapshotLoad {
            name: "offline".into(),
        })
        .unwrap();
    assert!(
        matches!(
            &refused,
            Outcome::Error { code: ErrorCode::SnapshotIncompatible, message } if message.contains("offline")
        ),
        "{refused:?}"
    );
    assert!(machine.run_a_while() > at_save);
    machine.quit();

    let snapshots = vmlab_disk::ops::snapshot_list(&disk).unwrap();
    let size = |name: &str| {
        snapshots
            .iter()
            .find(|snapshot| snapshot.name == name)
            .map(|snapshot| snapshot.vm_state_size)
    };
    assert_eq!(size("offline"), Some(0));
    assert!(size("online").unwrap() > 0);

    // A new process loads the online snapshot.
    std::fs::remove_file(dir.path().join("control.sock")).ok();
    let mut machine = Machine::start_with(dir.path(), &desc, Some(jit));
    ok(machine
        .client
        .call(Control::SnapshotLoad {
            name: "online".into(),
        })
        .unwrap());
    assert_eq!(machine.count(), at_save);
    assert!(machine.run_a_while() > at_save);
    ok(machine
        .client
        .call(Control::SnapshotDelete {
            name: "online".into(),
        })
        .unwrap());
    machine.quit();
}

/// A display at 00:06.0 and the xHCI of the tablet and the keyboard at
/// 00:07.0, the slots that vmlab gives them after a NIC.
fn with_display(description: &mut serde_json::Value) {
    description["display"] = serde_json::json!({
        "device": "virtio-gpu", "pci": {"bus": 0, "device": 6, "function": 0}, "rom": null,
    });
    description["tablet"] = serde_json::json!({"bus": 0, "device": 7, "function": 0});
    description["usb_keyboard"] = serde_json::json!(true);
}

/// The input of a display client: keys, and the tablet.
fn input_client(path: &Path) -> vmlab_hv_display::Client {
    use vmlab_hv_display::{Capabilities, Client};
    Client::connect(path, Capabilities(Capabilities::INPUT)).unwrap()
}

/// Press and release `qcode` over the display socket.
fn tap(client: &vmlab_hv_display::Client, qcode: &str) {
    for down in [true, false] {
        client
            .send(&vmlab_hv_display::Message::Key {
                qcode: qcode.into(),
                down,
            })
            .unwrap();
    }
}

/// #1225 at S1 without a host asset: a `virt` machine with a virtio GPU,
/// the tablet and the USB keyboard says that it has a display and input on
/// its control socket and serves both display sockets beside it. Before a
/// guest driver sets a scanout the screen is the placeholder. Keys and the
/// pointer go in while the guest runs, and an online snapshot of the
/// machine with the two functions saves and loads.
#[test]
fn a_virt_machine_with_a_display_serves_its_screen_and_its_input() {
    use vmlab_hv_display::{FrameFlags, Message};
    use vmlab_hv_proto::control::Capability;
    let dir = tempfile::Builder::new()
        .prefix("hv-virt-gpu-")
        .tempdir()
        .unwrap();
    let kernel = counting_kernel(dir.path());
    let mut desc = description(
        dir.path(),
        1,
        64,
        serde_json::json!({"kind": "kernel", "kernel": kernel, "initrd": null, "cmdline": ""}),
    );
    with_display(&mut desc);
    let disk = dir.path().join("disk0.qcow2");
    vmlab_disk::ops::create_qcow2(&disk, 16 << 20, None, None).unwrap();
    desc["disks"] = serde_json::json!([{
        "id": "disk0", "path": disk, "format": "qcow2", "read_only": false, "vmstate": true,
    }]);
    let mut machine = Machine::start(dir.path(), &desc);
    for capability in [Capability::Display, Capability::Input] {
        assert!(
            machine.client.has_capability(capability.clone()),
            "{capability:?}: {:?}",
            machine.client.capabilities()
        );
    }
    let native = dir.path().join("display.sock");
    assert!(dir.path().join("vnc.sock").exists());
    let screen = crate::display_acceptance::screenshot(&native);
    assert!(screen.flags.has(FrameFlags::PLACEHOLDER));
    assert_eq!((screen.width, screen.height), (640, 480));

    let at_start = machine.run_a_while();
    ok(machine.client.call(Control::Resume).unwrap());
    let input = input_client(&native);
    tap(&input, "a");
    for (x, y) in [(0, 0), (320, 240), (639, 479)] {
        input
            .send(&Message::PointerAbs {
                x,
                y,
                width: 640,
                height: 480,
            })
            .unwrap();
    }
    input.send(&Message::Buttons { mask: 1 }).unwrap();
    input.send(&Message::Buttons { mask: 0 }).unwrap();
    drop(input);
    ok(machine.client.call(Control::Pause).unwrap());
    assert!(
        machine.count() > at_start,
        "the guest stopped: {}",
        machine.diagnostics()
    );
    ok(machine
        .client
        .call(Control::SnapshotSave {
            name: "display".into(),
        })
        .unwrap());
    ok(machine
        .client
        .call(Control::SnapshotLoad {
            name: "display".into(),
        })
        .unwrap());
    let after = machine.run_a_while();
    assert!(after > 0, "{}", machine.diagnostics());
    machine.quit();
}

/// The keys that take this AAVMF from its boot manager to its shell, as in
/// `TO_THE_SHELL`, typed on the USB keyboard as qcodes.
const TO_THE_SHELL_ON_USB: [(&str, &str); 4] = [
    ("Press any key to enter the Boot Manager Menu", "x"),
    ("EFI Internal Shell", "down"),
    ("EFI Internal Shell", "ret"),
    ("any other key to continue", "ret"),
];

/// #1225, the proof at S1: AAVMF, unchanged, drives the virtio GPU and
/// reads the USB keyboard of the xHCI. Keys typed over the display socket
/// take it from its boot manager to the prompt of its UEFI shell, which the
/// serial log shows, and the screen over the same socket is the firmware's
/// console, not the placeholder.
#[test]
#[cfg_attr(
    vmlab_ci_no_host_assets,
    ignore = "requires the AAVMF images of guest/fetch-firmware.sh"
)]
fn aavmf_shows_its_console_on_the_virtio_gpu_and_reads_the_usb_keyboard() {
    use vmlab_hv_display::FrameFlags;
    let Some((code, pristine)) = aavmf() else {
        eprintln!(
            "skipping: AAVMF is absent; run guest/fetch-firmware.sh aarch64 or set VMLAB_HV_FIRMWARE_DIR_AARCH64"
        );
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("hv-virt-gpu-")
        .tempdir()
        .unwrap();
    let vars = dir.path().join("AAVMF_VARS.fd");
    std::fs::copy(pristine, &vars).unwrap();
    let mut description = description(
        dir.path(),
        1,
        1024,
        serde_json::json!({"kind": "firmware", "firmware": "aavmf", "code": code, "vars": vars}),
    );
    with_display(&mut description);
    let mut machine = Machine::start(dir.path(), &description);
    let native = dir.path().join("display.sock");
    let keys = input_client(&native);
    ok(machine.client.call(Control::Resume).unwrap());
    let timeout = std::env::var("VMLAB_HV_VIRT_TIMEOUT")
        .ok()
        .map(|value| value.parse().unwrap())
        .unwrap_or(600);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut step = 0;
    let mut sent = Instant::now();
    while !machine.serial().contains("Shell>") {
        if let Some((text, qcode)) = TO_THE_SHELL_ON_USB.get(step)
            && machine.serial().contains(text)
            && sent.elapsed() > Duration::from_millis(500)
        {
            tap(&keys, qcode);
            step += 1;
            sent = Instant::now();
        }
        assert!(
            machine.process.try_wait().unwrap().is_none(),
            "vmlab-hv ended before the UEFI shell: {}",
            machine.diagnostics()
        );
        if Instant::now() >= deadline {
            let _ = machine.client.call(Control::Quit);
            let _ = machine.process.wait();
            panic!(
                "no UEFI shell in {timeout} seconds, {step} keys typed: {}",
                machine.diagnostics()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let screen = crate::display_acceptance::screenshot(&native);
    assert!(
        !screen.flags.has(FrameFlags::PLACEHOLDER),
        "AAVMF set no scanout: {}",
        machine.diagnostics()
    );
    let lit = screen
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[..3] != [0; 3])
        .count();
    assert!(
        lit > 0,
        "the firmware console of {}x{} is black",
        screen.width,
        screen.height
    );
    if let Some(directory) = std::env::var_os("VMLAB_HV_DISPLAY_ARTIFACTS") {
        crate::display_acceptance::diagnostic_screenshot(
            &native,
            &PathBuf::from(directory).join("aavmf-virt-gpu.ppm"),
        );
    }
    eprintln!(
        "AAVMF reached its shell on the USB keyboard; its console is {}x{} with {lit} lit pixels",
        screen.width, screen.height
    );
    drop(keys);
    machine.quit();
}
