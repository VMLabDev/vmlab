//! The aarch64 `virt` machine (phase 3d, F4): the memory map, the device
//! tree, fw_cfg over MMIO, the two flash banks, the GICv2 with its v2m
//! frame, the PL011, the PL031, the GED, the PCI host with its ECAM and the
//! virtio functions, PSCI on `HVC` and `SMC`, reset and power-off. Its vCPUs
//! run on the emulator (`vcpu::emulated`), so the machine needs no KVM.
//!
//! The map is the one of QEMU's `virt` machine, as the ACPI tables of
//! `vmlab-hv-firmware` name it. The firmware starts at the reset vector of
//! the first flash bank, at EL1, and finds the device tree at the base of
//! RAM (DESIGN.md V2).
//!
//! Sources read (phase 3d DESIGN.md, "Sources"): S6, the device tree and the
//! register answers of a running QEMU 11.1.3, and S7, the documents that
//! `tree.rs`, `psci.rs` and the device models name. The Linux arm64 boot
//! protocol (`Documentation/arch/arm64/booting.rst`) for a kernel that is
//! booted without firmware. No QEMU source was read.

pub mod fdt;
pub mod psci;
mod snapshot;
pub mod tree;

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use parking_lot::Mutex;
use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::gic::{self, Gic, V2m};
use vmlab_hv_devices::pflash::cfi::{self, CfiFlash};
use vmlab_hv_devices::pl011::{self, Pl011};
use vmlab_hv_devices::pl031::{self, Pl031};
use vmlab_hv_devices::serial::{HostSocket, SerialOut};
use vmlab_hv_devices::{BusDevice, IrqLine, MsiSink, PciSnapshot, Resettable, ged};
use vmlab_hv_firmware::fw_cfg::{self, FwCfgMmio};
use vmlab_hv_proto::control::ShutdownReason;
use vmlab_hv_proto::machine::{
    Accel, Arch, Boot, DiskBus, FirmwareKind, Machine, MachineDescription, NicAttachment, NicModel,
};
use vmlab_hv_tcg_aarch64::State;
use vmlab_hv_tcg_aarch64::sysreg::CpuModel;
use vmlab_hv_tcg_aarch64::timer::Clock;

use crate::bus::{IoBus, MmioBus};
use crate::error::VmmError;
use crate::factory::{DeviceFactory, PciWiring};
use crate::layout::MemLayout;
use crate::memory::GuestRam;
use crate::pci::{PciEcam, PciRoot};
use crate::snapshot::{DeviceNode, DiskNode, SnapshotRegistry};
use crate::vcpu::emulated::{EmulatedVcpu, TimerLines};
use crate::vcpu::guest_bus::GuestBus;
use crate::vcpu::{Hypercall, HypercallOutcome, VcpuHost};
use crate::vm::{ExitReason, Vm, VmShared};

/// The guest-physical addresses and the interrupts of the machine.
pub mod map {
    pub use vmlab_hv_firmware::virt::map::*;

    /// The two flash banks: the firmware code, then its variables.
    pub const FLASH_BASE: u64 = 0;
    pub const FLASH_BANK: u64 = 64 << 20;
    /// PL031, and its interrupt as an SPI.
    pub const RTC_BASE: u64 = 0x0901_0000;
    pub const RTC_SPI: u32 = 2;
    /// The interrupt of the PL011 as an SPI.
    pub const UART_SPI: u32 = 1;
    /// The generic event device of ACPI: the power button.
    pub const GED_BASE: u64 = 0x0908_0000;
    pub const GED_SPI: u32 = 9;
    /// 32 virtio-mmio transports. The device tree and the DSDT name them,
    /// and this machine plugs no device into one: its virtio devices are
    /// PCI functions.
    pub const VIRTIO_MMIO_BASE: u64 = 0x0a00_0000;
    pub const VIRTIO_MMIO_SIZE: u64 = 0x200;
    pub const VIRTIO_MMIO_COUNT: u64 = 32;
    pub const VIRTIO_MMIO_SPI: u32 = 16;
    /// The windows of the PCI host: 32-bit memory, I/O ports and 64-bit
    /// memory.
    pub const PCI_MMIO32_BASE: u64 = 0x1000_0000;
    pub const PCI_MMIO32_SIZE: u64 = 0x2eff_0000;
    pub const PCI_PIO_BASE: u64 = 0x3eff_0000;
    pub const PCI_PIO_SIZE: u64 = 0x1_0000;
    pub const PCI_MMIO64_BASE: u64 = 0x80_0000_0000;
    pub const PCI_MMIO64_SIZE: u64 = 0x80_0000_0000;
    pub const ECAM_SIZE: u64 = 0x1000_0000;
    /// The four interrupt lines of the PCI host, as SPIs from this one.
    pub const PCI_INTX_SPI: u32 = 3;
    /// The shared interrupts of the GIC.
    pub const SPIS: usize = 256;
    /// The interrupts of the timer of a CPU, as interrupt IDs.
    pub const TIMER_PHYSICAL_INTID: usize = 30;
    pub const TIMER_VIRTUAL_INTID: usize = 27;
}

/// The counts of the system counter in a second: what `CNTFRQ_EL0` reads.
pub const COUNTER_FREQUENCY: u64 = 62_500_000;

/// A kernel without the header of an arm64 image is loaded this far above
/// the base of RAM, as one with the header asks for by default.
const KERNEL_OFFSET: u64 = 0x8_0000;
/// An initial RAM disk and the device tree of a kernel boot sit at most
/// this far above the base of RAM.
const BOOT_DATA_OFFSET: u64 = 128 << 20;
/// The least RAM of the machine.
const MIN_RAM_MIB: u64 = 64;

/// The time of the machine: it stands still while the machine is paused.
struct MachineClock {
    state: Mutex<(bool, u64, Instant)>,
}

impl MachineClock {
    fn new() -> Self {
        Self {
            state: Mutex::new((false, 0, Instant::now())),
        }
    }

    fn now_ns(&self) -> u64 {
        let (running, base, origin) = *self.state.lock();
        if running {
            base.saturating_add(origin.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64)
        } else {
            base
        }
    }

    fn set_running(&self, running: bool) {
        let mut state = self.state.lock();
        if state.0 == running {
            return;
        }
        if !running {
            state.1 = state
                .1
                .saturating_add(state.2.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        }
        state.0 = running;
        state.2 = Instant::now();
    }

    /// Put the time back to `ns`, as a snapshot restore does. The clock
    /// goes on from there when it runs.
    fn set_now_ns(&self, ns: u64) {
        let mut state = self.state.lock();
        state.1 = ns;
        state.2 = Instant::now();
    }

    /// The system counter.
    fn count(&self) -> u64 {
        (u128::from(self.now_ns()) * u128::from(COUNTER_FREQUENCY) / 1_000_000_000) as u64
    }
}

impl pl031::Clock for MachineClock {
    fn now_ns(&self) -> u64 {
        MachineClock::now_ns(self)
    }
}

/// Every byte to each of the writers: the serial log, and the socket of
/// the console.
struct Tee(Vec<Box<dyn std::io::Write + Send>>);

impl std::io::Write for Tee {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut result = Ok(bytes.len());
        for (index, writer) in self.0.iter_mut().enumerate() {
            // The log is the first writer and its error is the error. A
            // client that does not read its socket loses output and stops
            // nothing.
            if let Err(error) = writer.write_all(bytes)
                && index == 0
            {
                result = Err(error);
            }
        }
        result
    }

    fn flush(&mut self) -> std::io::Result<()> {
        for writer in &mut self.0 {
            writer.flush()?;
        }
        Ok(())
    }
}

/// An interrupt line whose far end is made after its near end: the GIC is
/// on the bus before the CPUs exist.
#[derive(Default)]
struct LateLine(OnceLock<Arc<dyn IrqLine>>);

impl IrqLine for LateLine {
    fn set_level(&self, high: bool) {
        if let Some(line) = self.0.get() {
            line.set_level(high);
        }
    }
}

/// The 32 virtio-mmio transports with no device behind them. Each answers
/// with its magic, its version and its vendor, and a device ID of 0, which
/// tells a driver that the slot is empty (VIRTIO 1.2, 4.2.2).
struct EmptyTransports;

impl BusDevice for EmptyTransports {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value: u32 = match (offset % map::VIRTIO_MMIO_SIZE) & !3 {
            0x000 => 0x7472_6976,
            0x004 => 1,
            0x00c => 0x554d_4551,
            _ => 0,
        };
        let bytes = value.to_le_bytes();
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = bytes
                .get((offset as usize & 3) + index)
                .copied()
                .unwrap_or(0);
        }
    }

    fn write(&mut self, _offset: u64, _data: &[u8]) {}
}

/// The function 00:00.0 of the PCI host: a host bridge without a BAR. Its
/// identity and what a guest can write of it are what QEMU's `virt` shows
/// over qtest: the command bits 0x507, the cache line size and the
/// interrupt line.
struct HostBridge {
    registers: [u32; 16],
}

impl HostBridge {
    /// For each register that a guest can write, the bits that it can.
    const WRITABLE: [(usize, u32); 3] = [(1, 0x507), (3, 0xff), (15, 0xff)];

    fn new() -> Self {
        let mut registers = [0; 16];
        // Red Hat's generic PCIe host bridge, class 06 00.
        registers[0] = 0x0008_1b36;
        registers[2] = 0x0600_0000;
        registers[11] = 0x1100_1af4;
        Self { registers }
    }
}

impl vmlab_hv_devices::PciDevice for HostBridge {
    fn config_read(&mut self, reg: usize) -> u32 {
        self.registers.get(reg).copied().unwrap_or(0)
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let Some((_, writable)) = Self::WRITABLE.iter().find(|(index, _)| *index == reg) else {
            return;
        };
        let mut bytes = self.registers[reg].to_le_bytes();
        for (index, byte) in data.iter().enumerate() {
            if let Some(slot) = bytes.get_mut(offset as usize + index) {
                *slot = *byte;
            }
        }
        let value = u32::from_le_bytes(bytes);
        self.registers[reg] = (self.registers[reg] & !writable) | (value & writable);
    }

    fn bars(&self) -> Vec<vmlab_hv_devices::BarInfo> {
        Vec::new()
    }

    fn bar_read(&mut self, _bar: u8, _offset: u64, data: &mut [u8]) {
        data.fill(0xff);
    }

    fn bar_write(&mut self, _bar: u8, _offset: u64, _data: &[u8]) {}
}

impl Resettable for HostBridge {
    fn reset(&mut self) -> Result<(), vmlab_hv_devices::DeviceError> {
        *self = Self::new();
        Ok(())
    }
}

/// What the machine puts into RAM at power-on and at each reset.
struct Images {
    /// (address, bytes): the device tree, and the kernel and the initial
    /// RAM disk of a boot without firmware.
    loads: Vec<(u64, Vec<u8>)>,
    /// Where the first CPU starts, and what it finds in X0.
    entry: u64,
    x0: u64,
}

/// What a caller may replace in a machine: the boot lockstep runs our
/// machine on the firmware inputs of its leader (DESIGN.md O4).
#[derive(Default)]
pub struct Options {
    /// The device tree, in place of the one of `tree.rs`.
    pub dtb: Option<Vec<u8>>,
    /// Wrap the machine as the vCPUs see it: the lockstep takes the device
    /// reads of its leader there.
    pub host: Option<HostWrapper>,
    /// The CPU, in place of the one of the product (DESIGN.md G2): the
    /// lockstep runs the CPU of its leader.
    pub model: Option<&'static CpuModel>,
    /// The interrupt of a PMU in the ACPI tables, for a leader that has
    /// one. The product has none (DESIGN.md V5).
    pub performance_interrupt: Option<u32>,
    /// The fw_cfg files, in directory order, in place of the generated
    /// ones.
    pub fw_cfg_files: Option<Vec<(String, Vec<u8>)>>,
}

pub type HostWrapper = Box<dyn FnOnce(Arc<dyn VcpuHost>) -> Arc<dyn VcpuHost>>;

/// The parts of a built machine whose vCPU threads do not run yet.
pub struct Assembled {
    pub shared: Arc<VmShared>,
    pub vcpus: Vec<Arc<Mutex<EmulatedVcpu>>>,
    pub ram: GuestRam,
}

impl Assembled {
    /// Reset the machine in place, for a caller that runs the vCPUs itself
    /// and holds none of them.
    pub fn reset(&self) -> Result<(), String> {
        let virt = self
            .shared
            .virt
            .get()
            .ok_or("the machine is no virt machine")?;
        virt.reset_with(&self.shared, &self.ram)
    }
}

/// The `virt` half of a machine: what `VmShared` asks it for.
pub struct Virt {
    vcpus: Vec<Arc<Mutex<EmulatedVcpu>>>,
    /// For each CPU, whether it is on.
    powered: Mutex<Vec<bool>>,
    clock: Arc<MachineClock>,
    gic: Arc<Mutex<Gic>>,
    ged: Arc<Mutex<ged::Ged>>,
    rtc: Arc<Mutex<Pl031>>,
    uart: Arc<Mutex<Pl011>>,
    /// The host end of the PL011 as a socket, when the machine has one.
    console: Option<HostSocket>,
    fw_cfg: Arc<Mutex<FwCfgMmio>>,
    /// The bank of the firmware variables.
    flash: Option<Arc<Mutex<CfiFlash>>>,
    bridge: Arc<Mutex<HostBridge>>,
    functions: Vec<Arc<Mutex<dyn PciSnapshot>>>,
    /// The BARs of the PCI root at power-on.
    bars: Vec<(u8, Vec<(u64, bool)>)>,
    images: Images,
    model: &'static CpuModel,
}

fn invalid(message: impl Into<String>) -> VmmError {
    VmmError::InvalidMachine(message.into())
}

/// Static checks of a description of a `virt` machine.
pub fn validate(desc: &MachineDescription) -> Result<(), VmmError> {
    if desc.arch != Arch::Aarch64 || desc.machine != Machine::Virt {
        return Err(invalid(
            "arch aarch64 requires machine=virt, and virt aarch64",
        ));
    }
    if desc.accel != Accel::Tcg {
        return Err(invalid(
            "an aarch64 machine runs on the emulator: accel must be tcg",
        ));
    }
    if desc.vcpus == 0 || desc.vcpus > u32::from(vmlab_hv_firmware::virt::MAX_VCPUS) {
        return Err(invalid("virt requires vcpus in 1..=8 (GICv2)"));
    }
    if desc.memory_mib < MIN_RAM_MIB || desc.memory_mib > vmlab_hv_firmware::virt::MAX_RAM >> 20 {
        return Err(invalid(format!(
            "memory_mib {} is out of range for virt ({MIN_RAM_MIB} MiB to 255 GiB)",
            desc.memory_mib
        )));
    }
    crate::clock::rtc_base(desc.rtc_base.as_deref())?;
    let unsupported = [
        (!desc.vfio.is_empty(), "vfio"),
        (desc.display.is_some(), "display"),
        (desc.virtual_display.is_some(), "virtual_display"),
        (desc.tablet.is_some(), "tablet"),
        (desc.usb.is_some(), "usb"),
        (desc.tpm.is_some(), "tpm"),
        (desc.ahci_pci.is_some(), "ahci_pci"),
        (desc.audio.is_some(), "audio"),
        (desc.floppy.is_some(), "floppy"),
        (desc.applesmc.is_some(), "applesmc"),
        (!desc.hyperv.is_empty(), "hyperv"),
        (!desc.shares.is_empty(), "shares"),
        (desc.cpu_model.is_some(), "cpu_model"),
        (!desc.serial_log_port.is_com1(), "serial_log_port"),
    ];
    if let Some((_, name)) = unsupported.iter().find(|(set, _)| *set) {
        return Err(invalid(format!(
            "`{name}` is not a part of the aarch64 virt machine yet"
        )));
    }
    for disk in &desc.disks {
        if disk.bus != DiskBus::Virtio || disk.cdrom || disk.path.is_none() {
            return Err(invalid(format!(
                "disk `{}`: a virt disk is a virtio-blk disk with an image",
                disk.id
            )));
        }
    }
    for nic in &desc.nics {
        if nic.model != NicModel::Virtio {
            return Err(invalid("a virt NIC is a virtio-net NIC"));
        }
        // A vhost backend writes guest RAM from the kernel, past the write
        // generations that the emulator keeps its translated code against.
        if matches!(
            nic.attachment,
            NicAttachment::Tap { .. } | NicAttachment::TapRaw { .. }
        ) {
            return Err(invalid(
                "a NIC of an emulated machine uses the stream attachment",
            ));
        }
    }
    // One port may be the PL011 itself: `com1` names the first serial port
    // of a machine.
    let mut uarts = desc.ports.iter().filter(|port| !port.transport.is_virtio());
    if uarts
        .next()
        .is_some_and(|port| port.transport.com_port().is_none_or(|com| !com.is_com1()))
        || uarts.next().is_some()
    {
        return Err(invalid(
            "a virt port is a virtio-serial port, or one `com1` port for the PL011",
        ));
    }
    let image = match &desc.boot {
        Boot::Kernel { kernel, initrd, .. } => {
            if !desc.fw_cfg.is_empty() {
                return Err(invalid("fw_cfg files require firmware boot"));
            }
            if let Some(initrd) = initrd
                && !initrd.is_file()
            {
                return Err(invalid(format!(
                    "initrd {} does not exist",
                    initrd.display()
                )));
            }
            kernel
        }
        Boot::Firmware {
            firmware,
            code,
            vars,
            ..
        } => {
            if *firmware != FirmwareKind::Aavmf {
                return Err(invalid("the firmware of virt is aavmf"));
            }
            let vars = vars
                .as_ref()
                .ok_or_else(|| invalid("AAVMF requires a writable vars path"))?;
            if !vars.is_file() {
                return Err(invalid(format!(
                    "firmware vars {} does not exist",
                    vars.display()
                )));
            }
            for file in &desc.fw_cfg {
                fw_cfg::validate_extra_file_name(&file.name).map_err(invalid)?;
            }
            code
        }
    };
    if !image.is_file() {
        return Err(invalid(format!(
            "boot image {} does not exist",
            image.display()
        )));
    }
    Ok(())
}

fn read_image(path: &std::path::Path, what: &str, limit: u64) -> Result<Vec<u8>, VmmError> {
    let data =
        std::fs::read(path).map_err(|e| invalid(format!("{what} {}: {e}", path.display())))?;
    if data.is_empty() || data.len() as u64 > limit {
        return Err(invalid(format!(
            "{what} {} has {} bytes, and 1 to {limit} fit",
            path.display(),
            data.len()
        )));
    }
    Ok(data)
}

/// The Image inside an EFI zboot kernel, which Alpine and Fedora ship for
/// arm64: a PE file whose header says `zimg` at 4, gives the offset and the
/// size of the compressed Image at 8 and 12, and names the compression at
/// 0x18, as the bytes of the guest asset's kernel show. Firmware runs its
/// stub, which unpacks the Image. A
/// boot without firmware unpacks it here, as a boot loader does. Any other
/// image is returned as it is.
fn unpack_zboot(image: Vec<u8>, limit: u64) -> Result<Vec<u8>, VmmError> {
    if image.get(..2) != Some(b"MZ") || image.get(4..8) != Some(b"zimg") {
        return Ok(image);
    }
    let word = |at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap()) as usize;
    let (offset, size) = (word(8), word(12));
    let compression = image.get(0x18..0x20).unwrap_or_default();
    let Some(payload) = offset
        .checked_add(size)
        .and_then(|end| image.get(offset..end))
    else {
        return Err(invalid("the zboot kernel names a payload outside its file"));
    };
    if !compression.starts_with(b"gzip\0") {
        return Err(invalid(format!(
            "the zboot kernel is compressed with {:?}, and only gzip is unpacked",
            String::from_utf8_lossy(compression).trim_end_matches('\0')
        )));
    }
    let mut unpacked = Vec::new();
    use std::io::Read;
    flate2::read::GzDecoder::new(payload)
        .take(limit + 1)
        .read_to_end(&mut unpacked)
        .map_err(|e| invalid(format!("unpacking the zboot kernel: {e}")))?;
    if unpacked.len() as u64 > limit {
        return Err(invalid("the unpacked zboot kernel does not fit the RAM"));
    }
    Ok(unpacked)
}

/// Where a kernel image is loaded, above the base of RAM: an image with the
/// arm64 header says so itself.
fn kernel_offset(image: &[u8]) -> u64 {
    let field = |at: usize| {
        image
            .get(at..at + 8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
    };
    let magic = image.get(0x38..0x3c) == Some(b"ARM\x64");
    match (magic, field(8), field(16)) {
        // A header whose image size is 0 is from before the field counted.
        (true, Some(offset), Some(size)) if size != 0 => offset,
        _ => KERNEL_OFFSET,
    }
}

/// The RAM contents of a boot, and the entry of the first CPU.
fn images(desc: &MachineDescription, dtb: Option<Vec<u8>>) -> Result<Images, VmmError> {
    let ram_size = desc.memory_mib << 20;
    let mut tree = tree::Tree {
        vcpus: desc.vcpus as u8,
        ram_size,
        bootargs: None,
        initrd: None,
    };
    let Boot::Kernel {
        kernel,
        initrd,
        cmdline,
    } = &desc.boot
    else {
        // The firmware finds the tree at the base of RAM.
        let dtb = dtb.unwrap_or_else(|| tree::build(&tree));
        return Ok(Images {
            loads: vec![(map::RAM_BASE, dtb)],
            entry: map::FLASH_BASE,
            x0: 0,
        });
    };
    let kernel = unpack_zboot(read_image(kernel, "kernel", ram_size)?, ram_size)?;
    let kernel_at = map::RAM_BASE + kernel_offset(&kernel);
    let data_at = map::RAM_BASE + (ram_size / 2).min(BOOT_DATA_OFFSET);
    let mut loads = Vec::new();
    let mut next = data_at;
    if let Some(initrd) = initrd {
        let initrd = read_image(initrd, "initrd", ram_size)?;
        tree.initrd = Some((data_at, data_at + initrd.len() as u64));
        next = (data_at + initrd.len() as u64).next_multiple_of(2 << 20);
        loads.push((data_at, initrd));
    }
    tree.bootargs = Some(cmdline.clone());
    let dtb = dtb.unwrap_or_else(|| tree::build(&tree));
    let end = next + dtb.len() as u64;
    if kernel_at + kernel.len() as u64 > data_at || end > map::RAM_BASE + ram_size {
        return Err(invalid(
            "the kernel, the initrd and the device tree do not fit the RAM",
        ));
    }
    loads.push((kernel_at, kernel));
    loads.push((next, dtb));
    Ok(Images {
        loads,
        entry: kernel_at,
        x0: next,
    })
}

/// Build the machine and its vCPU threads. With `paused` the vCPUs stay
/// parked until the `resume` command.
pub fn build(
    desc: &MachineDescription,
    factory: &dyn DeviceFactory,
    paused: bool,
) -> Result<Vm, VmmError> {
    let parts = assemble(desc, factory, Options::default())?;
    let vcpus = parts
        .vcpus
        .into_iter()
        .enumerate()
        .map(|(id, vcpu)| (id as u8, vcpu))
        .collect();
    let mut vm = Vm::from_emulated(parts.shared, vcpus, parts.ram)?;
    if !paused {
        vm.start();
    }
    Ok(vm)
}

/// Build the machine without its vCPU threads.
pub fn assemble(
    desc: &MachineDescription,
    factory: &dyn DeviceFactory,
    options: Options,
) -> Result<Assembled, VmmError> {
    validate(desc)?;
    let cpus = desc.vcpus as usize;
    let ram_size = desc.memory_mib << 20;
    let platform = vmlab_hv_firmware::virt::Platform {
        vcpus: desc.vcpus as u8,
        ram_size,
        uuid: [0; 16],
        performance_interrupt: options.performance_interrupt,
        bootorder: match &desc.boot {
            Boot::Firmware { bootorder, .. } => bootorder.clone(),
            Boot::Kernel { .. } => Vec::new(),
        },
    };

    // The first flash bank is memory that a CPU reads and does not write.
    // The RAM follows at its base.
    let layout = MemLayout {
        regions: vec![
            (map::FLASH_BASE, map::FLASH_BANK),
            (map::RAM_BASE, ram_size),
        ],
        ram: ram_size,
        mmio64_base: map::PCI_MMIO64_BASE,
        mmio64_size: map::PCI_MMIO64_SIZE,
    };
    let ram = GuestRam::anonymous(layout)?;
    let model = options.model;
    let images = images(desc, options.dtb)?;

    let mut mmio_bus = MmioBus::new();
    let mut on_bus = |base: u64, len: u64, device: Arc<Mutex<dyn BusDevice>>| {
        mmio_bus
            .register(base, len, device)
            .map_err(VmmError::Other)
    };

    // The interrupt controller, with the lines of CPUs that come later.
    let late: Vec<[Arc<LateLine>; 2]> = (0..cpus).map(|_| Default::default()).collect();
    let gic = Gic::new(
        "gic",
        late.iter()
            .map(|[irq, fiq]| gic::CpuLines {
                irq: irq.clone(),
                fiq: fiq.clone(),
            })
            .collect(),
        map::SPIS,
    );
    let spi = |spi: u32| Gic::spi_line(&gic, spi as usize);
    on_bus(
        map::GICD_BASE,
        gic::DISTRIBUTOR_SIZE,
        Arc::new(Mutex::new(gic::Distributor(gic.clone()))),
    )?;
    on_bus(
        map::GICC_BASE,
        gic::CPU_INTERFACE_SIZE,
        Arc::new(Mutex::new(gic::CpuInterface(gic.clone()))),
    )?;
    let v2m = V2m::new(
        gic.clone(),
        map::V2M_BASE,
        u32::from(map::V2M_SPI_BASE),
        u32::from(map::V2M_SPI_COUNT),
    );
    on_bus(
        map::V2M_BASE,
        gic::V2M_SIZE,
        Arc::new(Mutex::new(v2m.clone())),
    )?;
    let msi: Arc<dyn MsiSink> = Arc::new(v2m);

    let log =
        crate::serial_log::CappedLog::open(&desc.serial_log, crate::serial_log::SERIAL_LOG_CAP)
            .map_err(|e| VmmError::DeviceUnavailable {
                device: "pl011",
                reason: format!("cannot open serial log {}: {e}", desc.serial_log.display()),
            })?;
    // The PL011 writes the serial log. A `com1` port is its host end as a
    // socket too: the client reads what the log gets, and what it sends
    // the guest receives.
    let mut outputs: Vec<Box<dyn std::io::Write + Send>> = vec![Box::new(log)];
    let mut console = None;
    if let Some(port) = desc.ports.iter().find(|port| !port.transport.is_virtio()) {
        let socket = HostSocket::bind(&port.socket).map_err(|e| {
            VmmError::Other(format!("serial socket {}: {e}", port.socket.display()))
        })?;
        outputs.push(Box::new(socket.output()));
        console = Some(socket);
    }
    let uart = Arc::new(Mutex::new(Pl011::new(
        "pl011",
        SerialOut::new(Box::new(Tee(outputs))),
        spi(map::UART_SPI),
    )));
    on_bus(map::UART_BASE, pl011::SIZE, uart.clone())?;

    let clock = Arc::new(MachineClock::new());
    let rtc = Arc::new(Mutex::new(Pl031::new(
        "pl031",
        crate::clock::rtc_base(desc.rtc_base.as_deref())? as u32,
        clock.clone(),
        spi(map::RTC_SPI),
    )));
    on_bus(map::RTC_BASE, pl031::SIZE, rtc.clone())?;

    let ged = Arc::new(Mutex::new(ged::Ged::new("ged", spi(map::GED_SPI))));
    on_bus(map::GED_BASE, ged::SIZE, ged.clone())?;

    on_bus(
        map::VIRTIO_MMIO_BASE,
        map::VIRTIO_MMIO_COUNT * map::VIRTIO_MMIO_SIZE,
        Arc::new(Mutex::new(EmptyTransports)),
    )?;

    let mut firmware = match options.fw_cfg_files {
        Some(files) => {
            vmlab_hv_firmware::FwCfg::for_virt_files(ram.device_mem.clone(), &platform, files)
        }
        None => vmlab_hv_firmware::FwCfg::for_virt(ram.device_mem.clone(), &platform),
    }
    .map_err(VmmError::InvalidMachine)?;
    crate::builder::add_fw_cfg_files(&mut firmware, desc)?;
    let fw_cfg = Arc::new(Mutex::new(FwCfgMmio(firmware)));
    on_bus(map::FW_CFG_BASE, fw_cfg::MMIO_SIZE, fw_cfg.clone())?;

    // The flash banks of a firmware boot.
    let mut flash = None;
    if let Boot::Firmware { code, vars, .. } = &desc.boot {
        let code = read_image(code, "firmware code", map::FLASH_BANK)?;
        ram.mem
            .write_slice(
                &vec![0xff; map::FLASH_BANK as usize],
                GuestAddress(map::FLASH_BASE),
            )
            .and_then(|()| ram.mem.write_slice(&code, GuestAddress(map::FLASH_BASE)))
            .map_err(|e| VmmError::Memory(format!("loading the firmware code: {e}")))?;
        let vars = vars.as_ref().expect("validated");
        let bank = CfiFlash::open(vars, true).map_err(|e| VmmError::DeviceUnavailable {
            device: "pflash",
            reason: format!("firmware vars {}: {e}", vars.display()),
        })?;
        let bank = Arc::new(Mutex::new(bank));
        on_bus(
            map::FLASH_BASE + map::FLASH_BANK,
            cfi::BANK_SIZE as u64,
            bank.clone(),
        )?;
        flash = Some(bank);
    }

    // The PCI host. The firmware, or the kernel, places the BARs.
    let pci_root = Arc::new(Mutex::new(PciRoot::virt(map::PCI_MMIO64_BASE)));
    pci_root
        .lock()
        .set_mmio64_window(map::PCI_MMIO64_BASE, map::PCI_MMIO64_SIZE);
    on_bus(
        map::ECAM_BASE,
        map::ECAM_SIZE,
        Arc::new(Mutex::new(PciEcam::new(pci_root.clone()))),
    )?;
    let bridge = Arc::new(Mutex::new(HostBridge::new()));
    pci_root
        .lock()
        .add_device_at(0, 0, bridge.clone())
        .map_err(VmmError::Other)?;
    let mut next_slot = 1u8;
    let mut functions: Vec<Arc<Mutex<dyn PciSnapshot>>> = Vec::new();
    let mut place = |address: Option<vmlab_hv_proto::machine::PciAddress>,
                     vectors: u16|
     -> Result<(u8, u8, PciWiring), VmmError> {
        let (slot, function) = match address {
            Some(address) if address.bus == 0 => (address.device, address.function),
            Some(_) => return Err(invalid("a virt function sits on bus 0")),
            None => {
                let slot = next_slot;
                next_slot += 1;
                (slot, 0)
            }
        };
        if slot == 0 || slot >= 32 || function >= 8 {
            return Err(invalid("a virt function takes a slot in 1..=31"));
        }
        next_slot = next_slot.max(slot + 1);
        // Pin INTA of a slot is one of the four lines of the host, by the
        // slot (the `interrupt-map` of the device tree).
        let line = map::PCI_INTX_SPI + u32::from(slot) % 4;
        // QEMU's virt presents a virtio function on its root bus as a
        // transitional one (1af4:1000 to 1af4:103f, with an I/O BAR, read
        // from its monitor). AAVMF's driver of a modern-only function wants
        // a subsystem ID of 0x40 or more, so it would bind none of ours.
        Ok((
            slot,
            function,
            PciWiring {
                msi: msi.clone(),
                intx: Some(spi(line)),
                transitional: true,
                msix_vectors: vectors,
                doorbells: None,
            },
        ))
    };
    let unavailable = |what: String| {
        move |e: crate::error::DeviceUnavailable| VmmError::DeviceUnavailable {
            device: e.device,
            reason: format!("{what}: {}", e.reason),
        }
    };
    let mut disks = Vec::new();
    for disk in &desc.disks {
        let (slot, function, wiring) = place(disk.pci, (desc.vcpus + 1) as u16)?;
        let built = factory
            .virtio_blk(disk, &ram.device_mem, wiring)
            .map_err(unavailable(format!("disk `{}`", disk.id)))?;
        pci_root
            .lock()
            .add_device_at(slot, function, built.pci.function.clone())
            .map_err(VmmError::Other)?;
        functions.extend(built.pci.snapshot);
        disks.push(DiskNode {
            id: disk.id.clone(),
            writable: !disk.read_only,
            vmstate: disk.vmstate,
            image: built.disk,
        });
    }
    let virtio_ports: Vec<_> = desc
        .ports
        .iter()
        .filter(|port| port.transport.is_virtio())
        .cloned()
        .collect();
    if !virtio_ports.is_empty() {
        let (slot, function, wiring) = place(desc.console_pci, 2)?;
        let built = factory
            .virtio_console(&virtio_ports, &ram.device_mem, wiring)
            .map_err(unavailable("virtio-console ports".into()))?;
        pci_root
            .lock()
            .add_device_at(slot, function, built.function.clone())
            .map_err(VmmError::Other)?;
        functions.extend(built.snapshot);
    }
    for nic in &desc.nics {
        let (slot, function, wiring) = place(nic.pci, 2 * nic.attachment.queue_pairs() + 2)?;
        let built = factory
            .virtio_net(nic, &ram.device_mem, wiring)
            .map_err(unavailable(format!("nic {}", nic.mac)))?;
        pci_root
            .lock()
            .add_device_at(slot, function, built.function.clone())
            .map_err(VmmError::Other)?;
        functions.extend(built.snapshot);
    }
    let bars = pci_root.lock().save_bar_states();

    let shared = VmShared::emulated(
        IoBus::new(),
        mmio_bus,
        pci_root,
        (
            (
                map::PCI_MMIO32_BASE,
                map::PCI_MMIO32_BASE + map::PCI_MMIO32_SIZE,
            ),
            (
                map::PCI_MMIO64_BASE,
                map::PCI_MMIO64_BASE + map::PCI_MMIO64_SIZE,
            ),
        ),
    );
    shared.set_reset_description(desc.clone());
    crate::snapshot::remove_stale_checkpoints(&disks).map_err(|error| {
        VmmError::Other(format!("cleaning stale rollback checkpoints: {error}"))
    })?;
    shared.set_snapshot_registry(Arc::new(SnapshotRegistry {
        digest: crate::builder::machine_digest(desc),
        disks,
        nodes: functions.iter().cloned().map(DeviceNode::Virtio).collect(),
    }));

    // The CPUs, on the memory and the bus of the machine.
    let host: Arc<dyn VcpuHost> = shared.clone();
    let host = match options.host {
        Some(wrap) => wrap(host),
        None => host,
    };
    let bus = GuestBus::with_roms(
        ram.mem.clone(),
        host,
        cpus,
        &[(map::FLASH_BASE, map::FLASH_BANK)],
    )
    .map_err(VmmError::Other)?;
    let mut vcpus = Vec::with_capacity(cpus);
    let shootdown = Arc::new(vmlab_hv_tcg_aarch64::shootdown::Shootdown::new(cpus));
    for (id, lines) in late.iter().enumerate() {
        let mut vcpu = EmulatedVcpu::new(
            id as u8,
            bus.clone(),
            State::reset(0),
            Clock::host(0, COUNTER_FREQUENCY),
        );
        if cpus > 1 {
            vcpu.set_shootdown(shootdown.clone());
        }
        let counter = clock.clone();
        vcpu.set_counter(Arc::new(move || counter.count()), COUNTER_FREQUENCY);
        vcpu.set_timer_lines(TimerLines {
            physical: Gic::ppi_line(&gic, id, map::TIMER_PHYSICAL_INTID),
            virtual_: Gic::ppi_line(&gic, id, map::TIMER_VIRTUAL_INTID),
        });
        let cpu_lines = vcpu.lines();
        let _ = lines[0].0.set(cpu_lines.irq);
        let _ = lines[1].0.set(cpu_lines.fiq);
        vcpus.push(Arc::new(Mutex::new(vcpu)));
    }

    let virt = Arc::new(Virt {
        vcpus: vcpus.clone(),
        powered: Mutex::new(vec![false; cpus]),
        clock,
        gic,
        ged,
        rtc,
        uart,
        console,
        fw_cfg,
        flash,
        bridge,
        functions,
        bars,
        images,
        model: model.unwrap_or(&vmlab_hv_tcg_aarch64::sysreg::V8_0),
    });
    virt.power_on(&ram)?;
    shared
        .virt
        .set(virt)
        .map_err(|_| VmmError::Other("the machine has a virt half already".into()))?;
    Ok(Assembled { shared, vcpus, ram })
}

impl Virt {
    pub(crate) fn report_boot_counts(&self) {
        let instructions = std::env::var_os("VMLAB_HV_BOOT_INSTRUCTIONS").is_some();
        if instructions || std::env::var_os("VMLAB_HV_BOOT_BLOCK_COUNTS").is_some() {
            for vcpu in &self.vcpus {
                vcpu.lock().report_boot_counts(instructions);
            }
        }
    }

    pub(crate) fn vcpus(&self) -> usize {
        self.vcpus.len()
    }

    /// The backend of the vCPU `id`, for the debug port.
    pub(crate) fn vcpu(&self, id: u8) -> Option<Arc<Mutex<EmulatedVcpu>>> {
        self.vcpus.get(usize::from(id)).cloned()
    }

    pub(crate) fn powered(&self, id: u8) -> bool {
        self.powered
            .lock()
            .get(usize::from(id))
            .copied()
            .unwrap_or(false)
    }

    /// The state of the CPU `id` when it starts at `entry` with `x0`: EL1
    /// with every interrupt masked and the MMU off.
    fn start_state(&self, id: usize, entry: u64, x0: u64) -> State {
        let mut state = State::reset_as(entry, self.model);
        // The architecture leaves the flags of a reset unknown. The CPU of
        // QEMU starts with Z set (the first record of the boot lockstep),
        // and so does this one (DESIGN.md P2).
        state.set_nzcv(0b0100);
        state.set_x(0, x0);
        // Affinity level 0 is the number of the CPU.
        state.system.regs[vmlab_hv_tcg_aarch64::sysreg::reg::MPIDR_EL1] |= id as u64;
        state
    }

    /// Put the RAM contents of the boot in place, the first CPU at its
    /// entry and every other CPU off. The threads of the vCPUs are parked,
    /// or do not run yet.
    fn power_on(&self, ram: &GuestRam) -> Result<(), VmmError> {
        for (address, bytes) in &self.images.loads {
            ram.mem
                .write_slice(bytes, GuestAddress(*address))
                .map_err(|e| VmmError::Memory(format!("loading the boot images: {e}")))?;
        }
        let mut powered = self.powered.lock();
        for (id, vcpu) in self.vcpus.iter().enumerate() {
            let (entry, x0) = if id == 0 {
                (self.images.entry, self.images.x0)
            } else {
                (0, 0)
            };
            vcpu.lock().reset(self.start_state(id, entry, x0));
            powered[id] = id == 0;
        }
        Ok(())
    }

    /// Reset the machine in place. Every vCPU thread is parked.
    pub(crate) fn reset(&self, shared: &VmShared) -> Result<(), String> {
        let ram = shared.ram().ok_or("reset has no guest RAM")?;
        self.reset_with(shared, &ram)
    }

    fn reset_with(&self, shared: &VmShared, ram: &GuestRam) -> Result<(), String> {
        let device = |e: vmlab_hv_devices::DeviceError| e.to_string();
        self.gic.lock().reset().map_err(device)?;
        self.uart.lock().reset().map_err(device)?;
        self.rtc.lock().reset().map_err(device)?;
        self.ged.lock().reset().map_err(device)?;
        self.fw_cfg.lock().rewind();
        if let Some(flash) = &self.flash {
            flash.lock().reset().map_err(device)?;
        }
        self.bridge.lock().reset().map_err(device)?;
        for function in &self.functions {
            function.lock().reset().map_err(device)?;
        }
        shared
            .pci_root
            .lock()
            .restore_bar_states(&self.bars)
            .map_err(|e| format!("PCI root: {e}"))?;
        self.power_on(ram).map_err(|e| e.to_string())
    }

    pub(crate) fn clock_running(&self, running: bool) {
        self.clock.set_running(running);
    }

    /// What the events thread of the machine does each millisecond.
    pub(crate) fn poll(&self) -> Result<(), String> {
        self.rtc.lock().tick();
        if let Some(console) = &self.console {
            // What the receive FIFO has room for. The rest waits in the
            // socket.
            let mut uart = self.uart.lock();
            let mut bytes = [0u8; 32];
            let room = uart.can_receive().min(bytes.len());
            let count = console.read(&mut bytes[..room]);
            uart.receive(&bytes[..count]);
        }
        if let Some(flash) = &self.flash {
            flash
                .lock()
                .sync_due()
                .map_err(|e| format!("firmware vars: {e}"))?;
        }
        Ok(())
    }

    pub(crate) fn flush(&self) -> Result<(), String> {
        match &self.flash {
            Some(flash) => flash.lock().flush().map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// Press the power button: the guest hears of it through the GED.
    pub(crate) fn power_button(&self) {
        self.ged.lock().power_down();
    }

    /// The offset of `address` in the I/O port window of the PCI host.
    fn pio(address: u64) -> Option<u16> {
        address
            .checked_sub(map::PCI_PIO_BASE)
            .filter(|offset| *offset < map::PCI_PIO_SIZE)
            .map(|offset| offset as u16)
    }

    /// A read of the bus by a CPU. `false`: nothing decodes the address.
    pub(crate) fn mmio_read(&self, shared: &VmShared, address: u64, data: &mut [u8]) -> bool {
        if shared.pci_window(address) {
            // A read that no BAR takes ends in a master abort: all ones.
            if !PciRoot::read_mmio(&shared.pci_root, address, data) {
                data.fill(0xff);
            }
            return true;
        }
        if let Some(port) = Self::pio(address) {
            if !shared.pci_root.lock().dispatch_io_read(port, data) {
                data.fill(0xff);
            }
            return true;
        }
        if shared.mmio_bus().covers(address) {
            shared.mmio_bus().read(address, data);
            return true;
        }
        data.fill(0);
        false
    }

    pub(crate) fn mmio_write(&self, shared: &VmShared, address: u64, data: &[u8]) -> bool {
        if shared.pci_window(address) {
            PciRoot::write_mmio(&shared.pci_root, address, data);
            return true;
        }
        if let Some(port) = Self::pio(address) {
            shared.pci_root.lock().dispatch_io_write(port, data);
            return true;
        }
        if shared.mmio_bus().covers(address) {
            shared.mmio_bus().write(address, data);
            return true;
        }
        false
    }

    /// Serve a call of the guest: PSCI, on both conduits.
    pub(crate) fn hypercall(
        &self,
        shared: &VmShared,
        id: u8,
        call: &Hypercall,
    ) -> HypercallOutcome {
        let function = call.arguments[0] as u32;
        let arguments = [call.arguments[1], call.arguments[2], call.arguments[3]];
        // The call returns in X0 and keeps X1 to X3.
        let returns = |value: u64| {
            HypercallOutcome::Return([value, arguments[0], arguments[1], arguments[2]])
        };
        let mut powered = self.powered.lock();
        match psci::decide(function, arguments, &powered) {
            psci::Action::Return(value) => returns(value),
            psci::Action::Off => {
                powered[usize::from(id)] = false;
                returns(psci::SUCCESS)
            }
            psci::Action::On {
                cpu,
                entry,
                context,
            } => {
                // The CPU is off: its thread is parked, or is on its way
                // there, and holds its backend for no longer than that.
                let target = usize::from(cpu);
                self.vcpus[target]
                    .lock()
                    .reset(self.start_state(target, entry, context));
                powered[target] = true;
                drop(powered);
                shared.wake_parked();
                returns(psci::SUCCESS)
            }
            psci::Action::SystemOff => {
                drop(powered);
                shared.request_exit(ExitReason::Shutdown(ShutdownReason::GuestShutdown));
                returns(psci::SUCCESS)
            }
            psci::Action::SystemReset => {
                drop(powered);
                shared.request_reset();
                returns(psci::SUCCESS)
            }
        }
    }
}

#[cfg(test)]
mod tests;
