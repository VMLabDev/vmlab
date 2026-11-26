//! The `virt` machine without KVM and without host assets: small guests,
//! written here as words, booted as a kernel and as a firmware image.

use std::path::Path;

use vmlab_hv_proto::control::ShutdownReason;
use vmlab_hv_proto::machine::{
    Accel, Arch, Boot, FirmwareKind, Machine, MachineDescription,
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
    let vm = crate::build(desc, &NoDevices, false).unwrap();
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

#[test]
fn a_guest_reset_runs_the_boot_again_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let desc = description(dir.path(), 1, kernel(dir.path(), &reset_once()));
    let (reason, log) = run(&desc);
    assert_eq!(log, "12");
    assert_eq!(reason, OFF);
}

#[test]
fn a_guest_reset_ends_a_machine_that_exits_on_reset() {
    let dir = tempfile::tempdir().unwrap();
    let mut desc = description(dir.path(), 1, kernel(dir.path(), &reset_once()));
    desc.exit_on_reset = true;
    let (reason, log) = run(&desc);
    assert_eq!(log, "1");
    assert_eq!(
        reason,
        ExitReason::Shutdown(ShutdownReason::GuestReset)
    );
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
