//! The device tree of the aarch64 `virt` machine (phase 3d, F4). The
//! firmware finds it at the base of RAM and reads its devices from it: the
//! memory, the UART, the interrupt controller, the timer, fw_cfg, the flash
//! banks, the PCI host and PSCI. A kernel that is booted without firmware
//! gets its address in X0.
//!
//! Sources read (phase 3d DESIGN.md, "Sources"): S7, the Devicetree
//! Specification 0.4 and the binding documents of the Linux kernel tree
//! for each node (`arm,gic`, `arm,gic-v2m-frame`, `arm,armv8-timer`,
//! `arm,pl011`, `arm,pl031`, `arm,psci`, `qemu,fw-cfg-mmio`, `cfi-flash`,
//! `virtio,mmio`, `pci-host-ecam-generic`), and S6, the tree that a running
//! QEMU 11.1.3 writes with `-machine virt,dumpdtb=`. No QEMU source was
//! read.

use super::fdt::Fdt;
use super::map;

const PHANDLE_CLOCK: u32 = 1;
const PHANDLE_GIC: u32 = 2;
const PHANDLE_V2M: u32 = 3;

const SPI: u32 = 0;
const PPI: u32 = 1;
const EDGE_RISING: u32 = 1;
const LEVEL_HIGH: u32 = 4;

/// What of a machine its device tree depends on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub vcpus: u8,
    pub ram_size: u64,
    /// The kernel command line, for a kernel that is booted without
    /// firmware.
    pub bootargs: Option<String>,
    /// The first and the last address of an initial RAM disk.
    pub initrd: Option<(u64, u64)>,
}

fn reg(fdt: &mut Fdt, base: u64, size: u64) {
    fdt.cells64("reg", &[base, size]);
}

/// The blob of the tree.
pub fn build(tree: &Tree) -> Vec<u8> {
    let mut fdt = Fdt::new();
    fdt.begin("");
    fdt.cells("interrupt-parent", &[PHANDLE_GIC]);
    fdt.cells("#size-cells", &[2]);
    fdt.cells("#address-cells", &[2]);
    fdt.string("compatible", "linux,dummy-virt");
    fdt.string("model", "vmlab-hv,virt");

    fdt.begin("psci");
    fdt.cells("migrate", &[super::psci::MIGRATE64]);
    fdt.cells("cpu_on", &[super::psci::CPU_ON64]);
    fdt.cells("cpu_off", &[super::psci::CPU_OFF]);
    fdt.cells("cpu_suspend", &[super::psci::CPU_SUSPEND64]);
    fdt.string("method", "hvc");
    fdt.strings_list("compatible", &["arm,psci-1.0", "arm,psci-0.2", "arm,psci"]);
    fdt.end();

    fdt.begin(&format!("memory@{:x}", map::RAM_BASE));
    reg(&mut fdt, map::RAM_BASE, tree.ram_size);
    fdt.string("device_type", "memory");
    fdt.end();

    fdt.begin(&format!("fw-cfg@{:x}", map::FW_CFG_BASE));
    fdt.flag("dma-coherent");
    reg(&mut fdt, map::FW_CFG_BASE, vmlab_hv_firmware::fw_cfg::MMIO_SIZE);
    fdt.string("compatible", "qemu,fw-cfg-mmio");
    fdt.end();

    for slot in 0..map::VIRTIO_MMIO_COUNT {
        let base = map::VIRTIO_MMIO_BASE + slot * map::VIRTIO_MMIO_SIZE;
        fdt.begin(&format!("virtio_mmio@{base:x}"));
        fdt.flag("dma-coherent");
        fdt.cells(
            "interrupts",
            &[SPI, map::VIRTIO_MMIO_SPI + slot as u32, EDGE_RISING],
        );
        reg(&mut fdt, base, map::VIRTIO_MMIO_SIZE);
        fdt.string("compatible", "virtio,mmio");
        fdt.end();
    }

    fdt.begin(&format!("pcie@{:x}", map::PCI_MMIO32_BASE));
    fdt.cells("interrupt-map-mask", &[0x1800, 0, 0, 7]);
    // Each of the four slots that the mask tells apart, with its four pins,
    // on the four lines of the host.
    let mut interrupts = Vec::new();
    for slot in 0..4u32 {
        for pin in 0..4u32 {
            interrupts.extend_from_slice(&[
                slot << 11,
                0,
                0,
                pin + 1,
                PHANDLE_GIC,
                0,
                0,
                SPI,
                map::PCI_INTX_SPI + (slot + pin) % 4,
                LEVEL_HIGH,
            ]);
        }
    }
    fdt.cells("interrupt-map", &interrupts);
    fdt.cells("#interrupt-cells", &[1]);
    let window = |kind: u32, pci: u64, cpu: u64, size: u64| {
        [
            kind,
            (pci >> 32) as u32,
            pci as u32,
            (cpu >> 32) as u32,
            cpu as u32,
            (size >> 32) as u32,
            size as u32,
        ]
    };
    let mut ranges = Vec::new();
    ranges.extend(window(0x0100_0000, 0, map::PCI_PIO_BASE, map::PCI_PIO_SIZE));
    ranges.extend(window(
        0x0200_0000,
        map::PCI_MMIO32_BASE,
        map::PCI_MMIO32_BASE,
        map::PCI_MMIO32_SIZE,
    ));
    ranges.extend(window(
        0x0300_0000,
        map::PCI_MMIO64_BASE,
        map::PCI_MMIO64_BASE,
        map::PCI_MMIO64_SIZE,
    ));
    fdt.cells("ranges", &ranges);
    reg(&mut fdt, map::ECAM_BASE, map::ECAM_SIZE);
    fdt.cells("msi-map", &[0, PHANDLE_V2M, 0, 0x1_0000]);
    fdt.flag("dma-coherent");
    fdt.cells("bus-range", &[0, 0xff]);
    fdt.cells("linux,pci-domain", &[0]);
    fdt.cells("#size-cells", &[2]);
    fdt.cells("#address-cells", &[3]);
    fdt.string("device_type", "pci");
    fdt.string("compatible", "pci-host-ecam-generic");
    fdt.end();

    fdt.begin(&format!("pl031@{:x}", map::RTC_BASE));
    fdt.string("clock-names", "apb_pclk");
    fdt.cells("clocks", &[PHANDLE_CLOCK]);
    fdt.cells("interrupts", &[SPI, map::RTC_SPI, LEVEL_HIGH]);
    reg(&mut fdt, map::RTC_BASE, vmlab_hv_devices::pl031::SIZE);
    fdt.strings_list("compatible", &["arm,pl031", "arm,primecell"]);
    fdt.end();

    fdt.begin(&format!("pl011@{:x}", map::UART_BASE));
    fdt.strings_list("clock-names", &["uartclk", "apb_pclk"]);
    fdt.cells("clocks", &[PHANDLE_CLOCK, PHANDLE_CLOCK]);
    fdt.cells("interrupts", &[SPI, map::UART_SPI, LEVEL_HIGH]);
    reg(&mut fdt, map::UART_BASE, vmlab_hv_devices::pl011::SIZE);
    fdt.strings_list("compatible", &["arm,pl011", "arm,primecell"]);
    fdt.end();

    fdt.begin(&format!("intc@{:x}", map::GICD_BASE));
    fdt.cells("phandle", &[PHANDLE_GIC]);
    fdt.cells64(
        "reg",
        &[map::GICD_BASE, 0x1_0000, map::GICC_BASE, 0x1_0000],
    );
    fdt.string("compatible", "arm,cortex-a15-gic");
    fdt.flag("ranges");
    fdt.cells("#size-cells", &[2]);
    fdt.cells("#address-cells", &[2]);
    fdt.flag("interrupt-controller");
    fdt.cells("#interrupt-cells", &[3]);
    fdt.begin(&format!("v2m@{:x}", map::V2M_BASE));
    fdt.cells("phandle", &[PHANDLE_V2M]);
    reg(&mut fdt, map::V2M_BASE, vmlab_hv_devices::gic::V2M_SIZE);
    fdt.flag("msi-controller");
    fdt.string("compatible", "arm,gic-v2m-frame");
    fdt.end();
    fdt.end();

    fdt.begin("flash@0");
    fdt.cells("bank-width", &[4]);
    fdt.cells64(
        "reg",
        &[
            map::FLASH_BASE,
            map::FLASH_BANK,
            map::FLASH_BASE + map::FLASH_BANK,
            map::FLASH_BANK,
        ],
    );
    fdt.string("compatible", "cfi-flash");
    fdt.end();

    fdt.begin("cpus");
    fdt.cells("#size-cells", &[0]);
    fdt.cells("#address-cells", &[1]);
    for cpu in 0..u32::from(tree.vcpus) {
        fdt.begin(&format!("cpu@{cpu}"));
        fdt.cells("reg", &[cpu]);
        if tree.vcpus > 1 {
            fdt.string("enable-method", "psci");
        }
        fdt.string("compatible", "arm,cortex-a53");
        fdt.string("device_type", "cpu");
        fdt.end();
    }
    fdt.end();

    // The four timers of each CPU, each to the CPUs of the mask.
    let flags = ((1u32 << tree.vcpus) - 1) << 8 | LEVEL_HIGH;
    fdt.begin("timer");
    fdt.cells(
        "interrupts",
        &[
            PPI,
            map::TIMER_SECURE_EL1_GSIV - 16,
            flags,
            PPI,
            map::TIMER_NON_SECURE_EL1_GSIV - 16,
            flags,
            PPI,
            map::TIMER_VIRTUAL_GSIV - 16,
            flags,
            PPI,
            map::TIMER_EL2_GSIV - 16,
            flags,
        ],
    );
    fdt.flag("always-on");
    fdt.strings_list("compatible", &["arm,armv8-timer", "arm,armv7-timer"]);
    fdt.end();

    fdt.begin("apb-pclk");
    fdt.cells("phandle", &[PHANDLE_CLOCK]);
    fdt.string("clock-output-names", "clk24mhz");
    fdt.cells("clock-frequency", &[24_000_000]);
    fdt.cells("#clock-cells", &[0]);
    fdt.string("compatible", "fixed-clock");
    fdt.end();

    fdt.begin("chosen");
    fdt.string("stdout-path", &format!("/pl011@{:x}", map::UART_BASE));
    if let Some(bootargs) = &tree.bootargs {
        fdt.string("bootargs", bootargs);
    }
    if let Some((start, end)) = tree.initrd {
        fdt.cells64("linux,initrd-start", &[start]);
        fdt.cells64("linux,initrd-end", &[end]);
    }
    fdt.end();

    fdt.end();
    fdt.finish()
}

#[cfg(test)]
mod tests {
    use super::super::fdt::property;
    use super::*;

    fn cells(bytes: &[u8]) -> Vec<u32> {
        bytes
            .chunks(4)
            .map(|cell| u32::from_be_bytes(cell.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn the_tree_names_the_devices_at_the_addresses_of_the_map() {
        let blob = build(&Tree {
            vcpus: 2,
            ram_size: 3 << 30,
            bootargs: Some("console=ttyAMA0".into()),
            initrd: Some((0x4800_0000, 0x4800_1000)),
        });
        assert_eq!(
            cells(&property(&blob, "/memory@40000000", "reg").unwrap()),
            [0, 0x4000_0000, 0, 0xc000_0000]
        );
        assert_eq!(
            cells(&property(&blob, "/pl011@9000000", "reg").unwrap()),
            [0, 0x0900_0000, 0, 0x1000]
        );
        assert_eq!(
            cells(&property(&blob, "/pl011@9000000", "interrupts").unwrap()),
            [0, 1, 4]
        );
        assert_eq!(
            cells(&property(&blob, "/fw-cfg@9020000", "reg").unwrap()),
            [0, 0x0902_0000, 0, 0x18]
        );
        assert_eq!(
            cells(&property(&blob, "/intc@8000000", "reg").unwrap()),
            [0, 0x0800_0000, 0, 0x1_0000, 0, 0x0801_0000, 0, 0x1_0000]
        );
        assert_eq!(
            cells(&property(&blob, "/intc@8000000/v2m@8020000", "reg").unwrap()),
            [0, 0x0802_0000, 0, 0x1000]
        );
        assert_eq!(
            cells(&property(&blob, "/pcie@10000000", "reg").unwrap()),
            [0x40, 0x1000_0000, 0, 0x1000_0000]
        );
        // I/O, 32-bit and 64-bit memory.
        assert_eq!(
            cells(&property(&blob, "/pcie@10000000", "ranges").unwrap()),
            [
                0x0100_0000,
                0,
                0,
                0,
                0x3eff_0000,
                0,
                0x1_0000,
                0x0200_0000,
                0,
                0x1000_0000,
                0,
                0x1000_0000,
                0,
                0x2eff_0000,
                0x0300_0000,
                0x80,
                0,
                0x80,
                0,
                0x80,
                0
            ]
        );
        let map = cells(&property(&blob, "/pcie@10000000", "interrupt-map").unwrap());
        assert_eq!(map.len(), 160);
        // Slot 1, pin INTA: the second line of the host.
        assert_eq!(map[40..50], [0x800, 0, 0, 1, 2, 0, 0, 0, 4, 4]);
        // The virtual timer is PPI 11 (INTID 27), to both CPUs.
        let timer = cells(&property(&blob, "/timer", "interrupts").unwrap());
        assert_eq!(timer[6..9], [1, 11, 0x304]);
        assert_eq!(
            property(&blob, "/cpus/cpu@1", "enable-method").unwrap(),
            b"psci\0"
        );
        assert_eq!(property(&blob, "/psci", "method").unwrap(), b"hvc\0");
        assert_eq!(
            cells(&property(&blob, "/psci", "cpu_on").unwrap()),
            [0xc400_0003]
        );
        assert_eq!(
            property(&blob, "/chosen", "bootargs").unwrap(),
            b"console=ttyAMA0\0"
        );
        assert_eq!(
            cells(&property(&blob, "/chosen", "linux,initrd-end").unwrap()),
            [0, 0x4800_1000]
        );
        assert_eq!(
            cells(&property(&blob, "/virtio_mmio@a003e00", "interrupts").unwrap()),
            [0, 47, 1]
        );
    }

    #[test]
    fn one_cpu_has_no_enable_method() {
        let blob = build(&Tree {
            vcpus: 1,
            ram_size: 1 << 30,
            ..Tree::default()
        });
        assert_eq!(property(&blob, "/cpus/cpu@0", "enable-method"), None);
        assert_eq!(property(&blob, "/chosen", "bootargs"), None);
        assert_eq!(
            property(&blob, "/chosen", "stdout-path").unwrap(),
            b"/pl011@9000000\0"
        );
    }
}
