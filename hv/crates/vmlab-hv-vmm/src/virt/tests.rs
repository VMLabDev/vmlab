//! The `virt` machine without KVM and without host assets: small guests,
//! written here as words, booted as a kernel and as a firmware image.

use std::path::Path;

use vmlab_hv_proto::control::ShutdownReason;
use vmlab_hv_proto::machine::{
    Accel, Arch, Boot, DisplayDevice, FirmwareKind, Machine, MachineDescription,
};
use vmlab_hv_tcg_aarch64::oracle::asm;

use super::{map, psci};
use crate::factory::NoDevices;
use crate::vm::ExitReason;

const KERNEL: u64 = map::RAM_BASE + 0x8_0000;

fn strb(rt: u32, rn: u32) -> u32 {
    0x3900_0000 | rn << 5 | rt
}

fn ldr32(rt: u32, rn: u32) -> u32 {
    0xb940_0000 | rn << 5 | rt
}

fn str32(rt: u32, rn: u32) -> u32 {
    0xb900_0000 | rn << 5 | rt
}

fn ldr64(rt: u32, rn: u32) -> u32 {
    0xf940_0000 | rn << 5 | rt
}

fn str64(rt: u32, rn: u32) -> u32 {
    0xf900_0000 | rn << 5 | rt
}

/// `STRH Wt, [Xn, #offset]`.
fn strh(rt: u32, rn: u32, offset: u32) -> u32 {
    0x7900_0000 | (offset / 2) << 10 | rn << 5 | rt
}

/// `MOV Xd, Xm`.
fn mov(rd: u32, rm: u32) -> u32 {
    0xaa00_03e0 | rm << 16 | rd
}

/// A program with branches to a place that comes later.
#[derive(Default)]
struct Program {
    words: Vec<u32>,
    /// The `CBNZ` words that go to the failure exit, by index.
    failures: Vec<usize>,
}

impl Program {
    fn push(&mut self, words: impl IntoIterator<Item = u32>) -> &mut Self {
        self.words.extend(words);
        self
    }

    /// X1 holds the UART: write `text`. X2 is used.
    fn print(&mut self, text: &str) -> &mut Self {
        for byte in text.bytes() {
            self.push([asm::movz(2, u16::from(byte), 0), strb(2, 1)]);
        }
        self
    }

    /// Go to the failure exit unless X`rt` is `value`. X9 is used.
    fn expect(&mut self, rt: u32, value: u64) -> &mut Self {
        self.push(asm::load(9, value));
        self.push([asm::eor(rt, rt, 9)]);
        self.failures.push(self.words.len());
        self.push([0]);
        self
    }

    fn psci(&mut self, function: u32) -> &mut Self {
        self.push(asm::load(0, u64::from(function)));
        self.push([asm::hvc(0)])
    }

    /// End the program: power off, and the failure exit after it.
    fn finish(mut self) -> Vec<u32> {
        self.psci(psci::SYSTEM_OFF);
        self.push([asm::b(0)]);
        let failure = self.words.len();
        self.print("!");
        self.psci(psci::SYSTEM_OFF);
        self.push([asm::b(0)]);
        for at in std::mem::take(&mut self.failures) {
            // The register that `expect` compared, and left as the
            // difference.
            let rt = self.words[at - 1] & 31;
            self.words[at] = asm::cbnz(rt, (failure - at) as i32);
        }
        self.words
    }
}

fn description(dir: &Path, vcpus: u32, boot: Boot) -> MachineDescription {
    let mut desc: MachineDescription = serde_json::from_value(serde_json::json!({
        "name": "virt", "arch": "aarch64", "accel": "tcg", "machine": "virt",
        "vcpus": vcpus, "memory_mib": 64,
        "boot": {"kind": "kernel", "kernel": "", "initrd": null, "cmdline": ""},
        "disks": [], "nics": [], "ports": [],
        "serial_log": dir.join("serial.log"),
    }))
    .unwrap();
    desc.boot = boot;
    desc
}

fn kernel(dir: &Path, words: &[u32]) -> Boot {
    let path = dir.join("kernel");
    std::fs::write(&path, asm::bytes(words)).unwrap();
    Boot::Kernel {
        kernel: path,
        initrd: None,
        cmdline: "test".into(),
    }
}

/// Run the machine to its end: why it ended, and its serial log.
fn run(desc: &MachineDescription) -> (ExitReason, String) {
    run_vm(crate::build(desc, &NoDevices, false).unwrap(), desc)
}

/// The machine of `desc` with its vCPUs on the JIT or on the interpreter
/// (3e DESIGN.md K7), not started.
fn machine(
    desc: &MachineDescription,
    jit: bool,
) -> (
    crate::vm::Vm,
    Vec<std::sync::Arc<parking_lot::Mutex<crate::vcpu::emulated::EmulatedVcpu>>>,
) {
    let parts = super::assemble(desc, &NoDevices, super::Options::default()).unwrap();
    for vcpu in &parts.vcpus {
        let mut vcpu = vcpu.lock();
        vcpu.cpu().set_jit(jit).unwrap();
        assert_eq!(vcpu.cpu().jit_enabled(), jit);
    }
    let vcpus = parts.vcpus.clone();
    let numbered = parts
        .vcpus
        .into_iter()
        .enumerate()
        .map(|(id, vcpu)| (id as u8, vcpu))
        .collect();
    let vm = crate::vm::Vm::from_emulated(parts.shared, numbered, parts.ram).unwrap();
    (vm, vcpus)
}

/// The runs of compiled code of the vCPUs.
fn compiled_runs(
    vcpus: &[std::sync::Arc<parking_lot::Mutex<crate::vcpu::emulated::EmulatedVcpu>>],
) -> u64 {
    vcpus
        .iter()
        .map(|vcpu| vcpu.lock().cpu().jit_counts().compiled_runs)
        .sum()
}

/// `run` with the vCPUs on the JIT or on the interpreter, and the runs of
/// compiled code they made.
fn run_with(desc: &MachineDescription, jit: bool) -> (ExitReason, String, u64) {
    let (mut vm, vcpus) = machine(desc, jit);
    vm.start();
    let (reason, log) = run_vm(vm, desc);
    (reason, log, compiled_runs(&vcpus))
}

fn run_vm(vm: crate::vm::Vm, desc: &MachineDescription) -> (ExitReason, String) {
    let handle = vm.handle();
    let watchdog = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while handle.status() != vmlab_hv_proto::control::RunState::ShuttingDown {
            if std::time::Instant::now() > deadline {
                handle.runtime_error("the test guest did not end in 60 seconds".into());
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
    let reason = vm.wait();
    watchdog.join().unwrap();
    let log = std::fs::read(&desc.serial_log).unwrap();
    (reason, String::from_utf8_lossy(&log).into_owned())
}

const OFF: ExitReason = ExitReason::Shutdown(ShutdownReason::GuestShutdown);

#[test]
fn a_kernel_finds_the_device_tree_in_x0_and_powers_off() {
    let dir = tempfile::tempdir().unwrap();
    let mut program = Program::default();
    program.push([mov(19, 0)]);
    program.push(asm::load(1, map::UART_BASE));
    // The magic of a device tree, as a little-endian load reads it.
    program.push([ldr32(3, 19)]);
    program.expect(3, 0xedfe_0dd0);
    // The kernel runs at EL1 with SP_EL1: CurrentEL is 4.
    program.push([asm::mrs(3, 0xc212)]);
    program.expect(3, 4);
    program.print("virt\n");
    let desc = description(dir.path(), 1, kernel(dir.path(), &program.finish()));
    let (reason, log) = run(&desc);
    assert_eq!(log, "virt\n");
    assert_eq!(reason, OFF);
}

#[test]
fn psci_turns_a_second_cpu_on_and_it_turns_itself_off() {
    let dir = tempfile::tempdir().unwrap();
    const SECONDARY: usize = 0x80;
    let mut program = Program::default();
    program.push(asm::load(1, map::UART_BASE));
    program.push([asm::mrs(3, 0xc005)]);
    program.expect(3, 0x8000_0000);
    program.psci(psci::VERSION);
    program.expect(0, 0x1_0001);
    // CPU_ON of the CPU 1, with 'B' as its context.
    program.push(asm::load(1, 1));
    program.push(asm::load(2, KERNEL + 4 * SECONDARY as u64));
    program.push(asm::load(3, u64::from(b'B')));
    program.psci(psci::CPU_ON64);
    program.expect(0, 0);
    // A second CPU_ON of a CPU that is on, or that was: both answers are
    // right, so wait for AFFINITY_INFO to say off.
    let wait = program.words.len();
    program.push(asm::load(1, 1));
    program.push(asm::load(2, 0));
    program.psci(psci::AFFINITY_INFO64);
    program.push([asm::eor(0, 0, 1)]);
    let back = wait as i32 - program.words.len() as i32;
    program.push([asm::cbnz(0, back)]);
    program.push(asm::load(1, map::UART_BASE));
    program.print("A\n");
    let mut words = program.finish();
    assert!(words.len() <= SECONDARY);
    words.resize(SECONDARY, asm::NOP);
    // The second CPU: X0 is the context.
    words.push(mov(19, 0));
    words.extend(asm::load(1, map::UART_BASE));
    words.push(strb(19, 1));
    // Its MPIDR names it.
    words.push(asm::mrs(3, 0xc005));
    words.push(asm::movz(2, u16::from(b'0'), 0));
    words.push(0x8b03_0042); // add x2, x2, x3
    words.push(strb(2, 1));
    words.extend(asm::load(0, u64::from(psci::CPU_OFF)));
    words.extend([asm::hvc(0), asm::b(0)]);
    let desc = description(dir.path(), 2, kernel(dir.path(), &words));
    let (reason, log) = run(&desc);
    assert_eq!(log, "B1A\n");
    assert_eq!(reason, OFF);
}

fn reset_once() -> Vec<u32> {
    // A word of RAM that neither boot image covers tells the two boots
    // apart: a reset keeps the RAM.
    let mut program = Program::default();
    program.push(asm::load(1, map::UART_BASE));
    program.push(asm::load(5, map::RAM_BASE + 0x10_0000));
    program.push([ldr64(6, 5)]);
    let branch = program.words.len();
    program.push([0]);
    program.print("1");
    program.push([str64(5, 5)]);
    program.psci(psci::SYSTEM_RESET);
    program.push([asm::b(0)]);
    let second = program.words.len();
    program.words[branch] = asm::cbnz(6, (second - branch) as i32);
    program.print("2");
    program.finish()
}

/// A reset in place runs the boot again, with the JIT (3e DESIGN.md K7:
/// the reset drops the compiled code of the first boot) and on the
/// interpreter.
#[test]
fn a_guest_reset_runs_the_boot_again_in_place() {
    for jit in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let desc = description(dir.path(), 1, kernel(dir.path(), &reset_once()));
        let (reason, log, compiled) = run_with(&desc, jit);
        assert_eq!(log, "12", "jit {jit}");
        assert_eq!(reason, OFF, "jit {jit}");
        assert_eq!(compiled > 0, jit, "{compiled} runs of compiled code");
    }
}

#[test]
fn a_guest_reset_ends_a_machine_that_exits_on_reset() {
    let dir = tempfile::tempdir().unwrap();
    let mut desc = description(dir.path(), 1, kernel(dir.path(), &reset_once()));
    desc.exit_on_reset = true;
    let (reason, log) = run(&desc);
    assert_eq!(log, "1");
    assert_eq!(reason, ExitReason::Shutdown(ShutdownReason::GuestReset));
}

#[test]
fn a_firmware_runs_from_flash_and_reads_fw_cfg_the_tree_and_its_variables() {
    let dir = tempfile::tempdir().unwrap();
    let mut program = Program::default();
    program.push(asm::load(1, map::UART_BASE));
    // The device tree is at the base of RAM.
    program.push(asm::load(5, map::RAM_BASE));
    program.push([ldr32(3, 5)]);
    program.expect(3, 0xedfe_0dd0);
    // fw_cfg: select the signature and read it from the data register.
    program.push(asm::load(5, map::FW_CFG_BASE));
    program.push([strh(31, 5, 8), ldr32(3, 5)]);
    program.expect(3, u64::from(u32::from_le_bytes(*b"QEMU")));
    // The variables: program a word, and read the array back.
    program.push(asm::load(6, map::FLASH_BASE + map::FLASH_BANK));
    for value in [0x0040_0040u64, 0x1234_5678, 0x00ff_00ff] {
        program.push(asm::load(7, value));
        program.push([str32(7, 6)]);
    }
    program.push([ldr32(3, 6)]);
    program.expect(3, 0x1234_5678);
    // A store into the code bank changes nothing.
    program.push(asm::load(6, map::FLASH_BASE));
    program.push([ldr32(10, 6), str32(31, 6), ldr32(3, 6)]);
    program.push([asm::eor(3, 3, 10)]);
    program.expect(3, 0);
    // An address that nothing decodes is a bus error: the firmware has no
    // vector table, so it is not tried here.
    program.print("OK\n");
    let code = dir.path().join("code.fd");
    let vars = dir.path().join("vars.fd");
    std::fs::write(&code, asm::bytes(&program.finish())).unwrap();
    std::fs::write(&vars, vec![0xff; 256 << 10]).unwrap();
    let desc = description(
        dir.path(),
        1,
        Boot::Firmware {
            boot_menu: false,
            firmware: FirmwareKind::Aavmf,
            code,
            vars: Some(vars.clone()),
            secure_boot: false,
            bootorder: Vec::new(),
        },
    );
    let (reason, log) = run(&desc);
    assert_eq!(log, "OK\n");
    assert_eq!(reason, OFF);
    let vars = std::fs::read(vars).unwrap();
    assert_eq!(vars[..4], 0x1234_5678u32.to_le_bytes());
    assert_eq!(vars.len(), 256 << 10);
}

#[test]
fn a_description_that_is_not_a_virt_machine_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let good = description(dir.path(), 1, kernel(dir.path(), &[asm::b(0)]));
    assert!(crate::validate(&good).is_ok());
    let refused = |change: &dyn Fn(&mut MachineDescription), word: &str| {
        let mut desc = good.clone();
        change(&mut desc);
        let error = crate::validate(&desc).unwrap_err().to_string();
        assert!(error.contains(word), "{error}");
    };
    refused(&|d| d.accel = Accel::Kvm, "tcg");
    refused(&|d| d.machine = Machine::Q35, "virt");
    refused(&|d| d.arch = Arch::Riscv64, "virt");
    refused(&|d| d.vcpus = 9, "1..=8");
    refused(&|d| d.memory_mib = 32, "memory_mib");
    refused(&|d| d.cpu_model = Some("max".into()), "cpu_model");
    refused(
        &|d| {
            d.boot = Boot::Firmware {
                boot_menu: false,
                firmware: FirmwareKind::Ovmf,
                code: dir.path().join("kernel"),
                vars: None,
                secure_boot: false,
                bootorder: Vec::new(),
            }
        },
        "aavmf",
    );
    refused(
        &|d| {
            d.boot = Boot::Kernel {
                kernel: dir.path().join("none"),
                initrd: None,
                cmdline: String::new(),
            }
        },
        "does not exist",
    );
    // The display is a 2D virtio-gpu, and the keyboard rides the xHCI of
    // the tablet (#1225).
    let at = |device| vmlab_hv_proto::machine::PciAddress {
        bus: 0,
        device,
        function: 0,
    };
    let gpu = vmlab_hv_proto::machine::Display {
        device: DisplayDevice::VirtioGpu,
        gpu: None,
        vram_bytes: vmlab_hv_proto::machine::default_gpu_vram_bytes(),
        render_node: None,
        pci: at(6),
        rom: None,
        socket: None,
        vnc_socket: None,
    };
    let mut shown = good.clone();
    shown.display = Some(gpu.clone());
    shown.tablet = Some(at(7));
    shown.usb_keyboard = true;
    assert!(crate::validate(&shown).is_ok());
    let vga = vmlab_hv_proto::machine::Display {
        device: DisplayDevice::VirtioVga,
        ..gpu.clone()
    };
    refused(&|d| d.display = Some(vga.clone()), "2D virtio-gpu");
    let rom = vmlab_hv_proto::machine::Display {
        rom: Some(dir.path().join("rom")),
        ..gpu.clone()
    };
    refused(&|d| d.display = Some(rom.clone()), "no ROM");
    refused(&|d| d.usb_keyboard = true, "usb_keyboard");
    refused(&|d| d.usb = Some(at(7)), "`usb`");
    // A TAP NIC and an in-process share are a part of virt (#1227); a
    // share of an external virtiofsd is not.
    let mut attached = good.clone();
    attached.nics.push(vmlab_hv_proto::machine::Nic {
        model: vmlab_hv_proto::machine::NicModel::Virtio,
        rom: None,
        pci: Some(at(3)),
        mac: "52:54:00:00:00:01".into(),
        mtu: None,
        attachment: vmlab_hv_proto::machine::NicAttachment::TapRaw { queue_pairs: 1 },
    });
    attached.shares.push(vmlab_hv_proto::machine::FsShare {
        pci: Some(at(4)),
        tag: "work".into(),
        socket: None,
        path: Some(dir.path().to_path_buf()),
        read_only: false,
    });
    assert!(crate::validate(&attached).is_ok());
    let external = vmlab_hv_proto::machine::FsShare {
        pci: Some(at(4)),
        tag: "work".into(),
        socket: Some(dir.path().join("virtiofsd.sock")),
        path: None,
        read_only: false,
    };
    refused(&|d| d.shares = vec![external.clone()], "external virtiofsd");
    // An x86 machine takes no virtio-gpu without VGA.
    let mut q35 = shown.clone();
    q35.arch = Arch::X86_64;
    q35.accel = Accel::Kvm;
    q35.machine = Machine::Q35;
    let error = crate::validate(&q35).unwrap_err().to_string();
    assert!(error.contains("virtio-vga"), "{error}");
    // An x86 machine does not take the machine of aarch64.
    let mut x86 = good.clone();
    x86.arch = Arch::X86_64;
    x86.accel = Accel::Kvm;
    assert!(crate::validate(&x86).is_err());
}

#[test]
fn a_kernel_with_the_arm64_header_is_loaded_where_it_asks() {
    let mut image = vec![0u8; 0x40];
    assert_eq!(super::kernel_offset(&image), 0x8_0000);
    image[0x38..0x3c].copy_from_slice(b"ARM\x64");
    // An image size of 0: the offset field is not to be trusted.
    image[8..16].copy_from_slice(&0x20_0000u64.to_le_bytes());
    assert_eq!(super::kernel_offset(&image), 0x8_0000);
    image[16..24].copy_from_slice(&0x1000u64.to_le_bytes());
    assert_eq!(super::kernel_offset(&image), 0x20_0000);
    assert_eq!(super::kernel_offset(&image[..0x20]), 0x8_0000);
}

/// `MSR`s of the counting guest: TPIDR_EL1, CNTV_CVAL_EL0 and CNTV_CTL_EL0.
const TPIDR_EL1: u16 = 0xc684;
const CNTV_CVAL_EL0: u16 = 0xdf1a;
const CNTV_CTL_EL0: u16 = 0xdf19;
/// The word that the counting guest counts in.
const COUNTER: u64 = map::RAM_BASE + 0x10_0000;

/// A machine whose one CPU arms its virtual timer, so that its line is high
/// at the GIC, and then counts in RAM for ever, with the count also in
/// TPIDR_EL1. Its vmstate disk is a qcow2 file in `dir`. Its CPU runs the
/// JIT or the interpreter.
fn counting(
    dir: &Path,
    jit: bool,
) -> (
    crate::vm::Vm,
    Vec<std::sync::Arc<parking_lot::Mutex<crate::vcpu::emulated::EmulatedVcpu>>>,
) {
    let mut words = asm::load(1, COUNTER).to_vec();
    words.extend(asm::load(4, 1));
    words.extend([asm::msr(CNTV_CVAL_EL0, 4), asm::msr(CNTV_CTL_EL0, 4)]);
    words.extend([
        ldr64(3, 1),
        asm::add(3, 3, 1),
        str64(3, 1),
        asm::msr(TPIDR_EL1, 3),
        asm::b(-4),
    ]);
    let desc = description(dir, 1, kernel(dir, &words));
    let parts = super::assemble(&desc, &NoDevices, super::Options::default()).unwrap();
    for vcpu in &parts.vcpus {
        vcpu.lock().cpu().set_jit(jit).unwrap();
    }
    let path = dir.join("vmstate.qcow2");
    vmlab_disk::ops::create_qcow2(&path, 16 << 20, None, None).unwrap();
    let image: crate::factory::SharedImage = std::sync::Arc::new(parking_lot::Mutex::new(
        vmlab_disk::open(&path, vmlab_disk::Format::Qcow2, false).unwrap(),
    ));
    let registry = parts.shared.snapshot_registry().unwrap();
    parts
        .shared
        .set_snapshot_registry(std::sync::Arc::new(crate::snapshot::SnapshotRegistry {
            digest: registry.digest,
            disks: vec![crate::snapshot::DiskNode {
                id: "vmstate".into(),
                writable: true,
                vmstate: true,
                image,
            }],
            nodes: Vec::new(),
        }));
    let vcpus = parts.vcpus.clone();
    let numbered = parts
        .vcpus
        .into_iter()
        .enumerate()
        .map(|(id, vcpu)| (id as u8, vcpu))
        .collect();
    let vm = crate::vm::Vm::from_emulated(parts.shared, numbered, parts.ram).unwrap();
    (vm, vcpus)
}

fn count(handle: &crate::vm::VmHandle) -> u64 {
    use vm_memory::Bytes;
    handle
        .shared()
        .ram()
        .unwrap()
        .mem
        .read_obj::<u64>(vm_memory::GuestAddress(COUNTER))
        .unwrap()
}

/// Wait until the guest counted past `at`.
fn counted_past(handle: &crate::vm::VmHandle, at: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while count(handle) <= at {
        assert!(
            std::time::Instant::now() < deadline,
            "the guest does not count: {} after {at}",
            count(handle)
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn cpu_state(
    vcpu: &parking_lot::Mutex<crate::vcpu::emulated::EmulatedVcpu>,
) -> vmlab_hv_tcg_aarch64::State {
    use crate::vcpu::VcpuBackend;
    match vcpu.lock().state().unwrap() {
        crate::vcpu::VcpuState::Aarch64(state) => *state,
        crate::vcpu::VcpuState::X86 { .. } => unreachable!(),
    }
}

fn gic_state(handle: &crate::vm::VmHandle) -> Vec<u8> {
    use vmlab_hv_devices::Snapshot;
    handle
        .shared()
        .virt
        .get()
        .unwrap()
        .gic
        .lock()
        .save_state()
        .unwrap()
}

/// D13 on an emulated machine: a load puts RAM, the CPU with its system
/// registers and timers, and the GIC back to the save, keeps the run state
/// of the save, and the CPU runs on from there on caches that it built
/// again. With the JIT (3e DESIGN.md K7: the load drops the compiled code,
/// the fast TLB and the jump cache) and on the interpreter.
#[test]
fn a_snapshot_of_an_emulated_machine_brings_its_cpu_gic_timer_and_ram_back() {
    for jit in [true, false] {
        a_snapshot_brings_the_machine_back(jit);
    }
}

fn a_snapshot_brings_the_machine_back(jit: bool) {
    use vmlab_hv_proto::control::RunState;
    let dir = tempfile::tempdir().unwrap();
    let (mut vm, vcpus) = counting(dir.path(), jit);
    let handle = vm.handle();
    vm.start();
    counted_past(&handle, 1000);
    handle.pause().unwrap();
    // The vCPU ends its slice before it parks: its lock waits for that.
    let cpu_at_save = cpu_state(&vcpus[0]);
    let at_save = count(&handle);
    let gic_at_save = gic_state(&handle);
    // The pause can fall between the store and the MSR.
    let tpidr = cpu_at_save.system.regs[vmlab_hv_tcg_aarch64::sysreg::index("TPIDR_EL1")];
    assert!(
        tpidr == at_save || tpidr + 1 == at_save,
        "TPIDR_EL1 {tpidr} follows the count {at_save}"
    );
    assert_eq!(cpu_at_save.system.timers[1].compare, 1);
    crate::snapshot::save(&handle, "counted").unwrap();
    assert_eq!(handle.status(), RunState::Paused);

    handle.resume().unwrap();
    counted_past(&handle, at_save + 100_000);
    crate::snapshot::load(&handle, "counted").unwrap();
    // The save was of a paused machine.
    assert_eq!(handle.status(), RunState::Paused);
    assert_eq!(count(&handle), at_save);
    assert_eq!(
        super::snapshot::SavedCpu::of(&cpu_state(&vcpus[0])),
        super::snapshot::SavedCpu::of(&cpu_at_save)
    );
    assert_eq!(gic_state(&handle), gic_at_save);
    assert_eq!(vcpus[0].lock().cpu().tlb_entries(), 0);
    let compiled_at_load = compiled_runs(&vcpus);
    assert_eq!(
        compiled_at_load > 0,
        jit,
        "{compiled_at_load} runs of compiled code"
    );

    // The CPU runs on from the save, on compiled code again with the JIT.
    handle.resume().unwrap();
    counted_past(&handle, at_save + 1000);
    handle.pause().unwrap();
    assert_eq!(compiled_runs(&vcpus) > compiled_at_load, jit);
    assert_eq!(vcpus[0].lock().cpu().jit_enabled(), jit);
    handle.resume().unwrap();
    handle.quit();
    vm.wait();
}

/// A load of a name that no disk has, and a load of a snapshot that is
/// not of this machine, change nothing.
#[test]
fn a_snapshot_that_does_not_fit_the_emulated_machine_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (mut vm, _vcpus) = counting(dir.path(), true);
    let handle = vm.handle();
    vm.start();
    counted_past(&handle, 10);
    assert!(
        crate::snapshot::load(&handle, "none")
            .unwrap_err()
            .is_not_found()
    );
    crate::snapshot::save(&handle, "kept").unwrap();
    crate::snapshot::delete(&handle, "kept").unwrap();
    assert!(crate::snapshot::load(&handle, "kept").is_err());
    let at = count(&handle);
    counted_past(&handle, at + 10);
    handle.quit();
    vm.wait();
}

/// The debug port of an emulated machine: pause, the registers, a step,
/// a write of a register that the CPU runs on, memory by virtual address,
/// and a detach that resumes. With the JIT (3e DESIGN.md K7: a pause stops
/// the CPU at a block head, and a step runs one instruction) and on the
/// interpreter.
#[test]
fn the_debug_port_reads_steps_and_writes_an_emulated_cpu() {
    for jit in [true, false] {
        the_debug_port_on(jit);
    }
}

fn the_debug_port_on(jit: bool) {
    use vmlab_hv_proto::control::{Outcome, RunState};
    use vmlab_hv_proto::debug::{AddressSpace, DebugRequest, DebugResponse};
    let dir = tempfile::tempdir().unwrap();
    let (mut vm, vcpus) = counting(dir.path(), jit);
    let handle = vm.handle();
    vm.start();
    counted_past(&handle, 100);
    let debug = |request| match crate::debug::handle(handle.shared(), request) {
        Outcome::Debug { response } => response,
        other => panic!("{other:?}"),
    };
    let DebugResponse::Status { status } = debug(DebugRequest::Pause) else {
        panic!()
    };
    assert_eq!(status.state, RunState::Paused);
    assert_eq!(status.vcpus, vec![0]);
    let read = |debug: &dyn Fn(DebugRequest) -> DebugResponse| {
        let DebugResponse::Aarch64Registers { registers, .. } =
            debug(DebugRequest::ReadRegisters { vcpu: 0 })
        else {
            panic!()
        };
        registers
    };
    let before = read(&debug);
    assert_eq!(before.x[1], COUNTER);
    // EL1 with SP_EL1 and every interrupt masked.
    assert_eq!(before.cpsr & 0x3cf, 0x3c5);
    let DebugResponse::Status { status } = debug(DebugRequest::Step { vcpu: 0 }) else {
        panic!()
    };
    assert_eq!(status.state, RunState::Paused);
    let stop = status.stop.unwrap();
    let after = read(&debug);
    assert_eq!(stop.pc, after.pc);
    assert_ne!(after.pc, before.pc);

    // The count goes on from a value that the debugger wrote: step to the
    // load, then set X3 after it.
    let mut steps = 0;
    while read(&debug).pc != KERNEL + 4 * 10 {
        debug(DebugRequest::Step { vcpu: 0 });
        steps += 1;
        assert!(steps < 8, "the loop has five instructions");
    }
    debug(DebugRequest::Step { vcpu: 0 });
    debug(DebugRequest::SetRegisters {
        vcpu: 0,
        values: [("x3".to_string(), 1 << 40)].into(),
    });
    for _ in 0..3 {
        debug(DebugRequest::Step { vcpu: 0 });
    }
    assert_eq!(count(&handle), (1 << 40) + 1);
    let DebugResponse::Memory { data } = debug(DebugRequest::ReadMemory {
        space: AddressSpace::Virtual { vcpu: 0 },
        address: COUNTER,
        length: 8,
    }) else {
        panic!()
    };
    assert_eq!(data, ((1u64 << 40) + 1).to_le_bytes());
    assert!(matches!(
        crate::debug::handle(handle.shared(), DebugRequest::ReadRegisters { vcpu: 1 }),
        Outcome::Error { .. }
    ));
    debug(DebugRequest::Detach { resume: true });
    assert_eq!(handle.status(), RunState::Running);
    counted_past(&handle, (1 << 40) + 100);
    handle.pause().unwrap();
    assert_eq!(compiled_runs(&vcpus) > 0, jit);
    handle.resume().unwrap();
    handle.quit();
    vm.wait();
}

/// A kernel of the EFI zboot format boots without firmware: the machine
/// unpacks the Image inside it.
#[test]
fn a_zboot_kernel_is_unpacked_and_boots() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let mut program = Program::default();
    program.push(asm::load(1, map::UART_BASE));
    program.print("z\n");
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&asm::bytes(&program.finish())).unwrap();
    let payload = gzip.finish().unwrap();
    let mut image = vec![0u8; 0x40];
    image[..8].copy_from_slice(b"MZ\0\0zimg");
    image[8..12].copy_from_slice(&0x40u32.to_le_bytes());
    image[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    image[0x18..0x1d].copy_from_slice(b"gzip\0");
    image.extend(&payload);
    let path = dir.path().join("vmlinuz");
    std::fs::write(&path, &image).unwrap();
    let boot = Boot::Kernel {
        kernel: path.clone(),
        initrd: None,
        cmdline: String::new(),
    };
    let (reason, log) = run(&description(dir.path(), 1, boot));
    assert_eq!(log, "z\n");
    assert_eq!(reason, OFF);
    // Another compression is named in the refusal.
    image[0x18..0x1d].copy_from_slice(b"zstd\0");
    std::fs::write(&path, &image).unwrap();
    let boot = Boot::Kernel {
        kernel: path,
        initrd: None,
        cmdline: String::new(),
    };
    let error = crate::build(&description(dir.path(), 1, boot), &NoDevices, true)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("zstd"), "{error}");
}
