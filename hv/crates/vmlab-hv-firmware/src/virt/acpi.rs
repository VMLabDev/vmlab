//! The ACPI tables of `virt` as QEMU hands them to the firmware: one
//! `etc/acpi/tables` file with file-relative pointers and zero checksums, the
//! RSDP, and the loader program that relocates and sums them.
//!
//! Every table but the DSDT is built field by field from its specification.
//! The static AML of the DSDT is the hardware-reference capture
//! (`docs/hwref/virt-fwcfg/smp1-2g/acpi/DSDT`), an observation of a running
//! QEMU and not QEMU source. Only its processor devices depend on the
//! machine, and they are rebuilt.

use super::{Platform, map};
use crate::acpi::{
    AcpiTables, RSDP_FILE, TABLES_FILE, append, checksum, integer, package, put, table,
};
use crate::loader::{TableLoader, Zone};

const DSDT: &[u8] = include_bytes!("../../../../docs/hwref/virt-fwcfg/smp1-2g/acpi/DSDT");
/// QEMU pads `etc/acpi/tables` to this size.
const TABLES_SIZE: usize = 0x20000;
const HEADER: usize = 36;

/// Build the tables of F4 and the loader program for them.
pub fn generate(platform: &Platform) -> Result<AcpiTables, String> {
    platform.validate()?;
    let mut tables = Vec::new();
    let mut loader = TableLoader::default();
    loader.allocate(RSDP_FILE, 16, Zone::Fseg);
    loader.allocate(TABLES_FILE, 64, Zone::High);

    let dsdt = append(&mut tables, &dsdt(platform.vcpus)?);
    checksum(&mut tables, &mut loader, dsdt);

    let mut fadt = fadt();
    put(&mut fadt, 140, dsdt as u64, 8);
    let fadt = append(&mut tables, &fadt);
    loader.add_pointer(TABLES_FILE, TABLES_FILE, (fadt + 140) as u32, 8);
    checksum(&mut tables, &mut loader, fadt);

    let mut entries = vec![fadt];
    let pptt = pptt(platform.vcpus);
    for (index, body) in [madt(platform), pptt, gtdt(), mcfg(), spcr(), dbg2(), iort()]
        .iter()
        .enumerate()
    {
        let start = append(&mut tables, body);
        checksum(&mut tables, &mut loader, start);
        entries.push(start);
        if index == 1 {
            // QEMU 11.1 lists the PPTT twice in the XSDT. A guest sees the
            // same table at two entries, and vmlab-hv presents the same.
            entries.push(start);
        }
    }

    let mut xsdt = table(*b"XSDT", 1, entries.len() * 8);
    for (index, address) in entries.iter().enumerate() {
        put(&mut xsdt, HEADER + index * 8, *address as u64, 8);
    }
    let xsdt = append(&mut tables, &xsdt);
    for index in 0..entries.len() {
        loader.add_pointer(
            TABLES_FILE,
            TABLES_FILE,
            (xsdt + HEADER + index * 8) as u32,
            8,
        );
    }
    checksum(&mut tables, &mut loader, xsdt);

    // RSDP of ACPI 2.0: no RSDT, a 64-bit pointer to the XSDT, two checksums.
    let mut rsdp = vec![0; 36];
    rsdp[..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(b"BOCHS ");
    rsdp[15] = 2;
    put(&mut rsdp, 20, 36, 4);
    put(&mut rsdp, 24, xsdt as u64, 8);
    loader.add_checksum(RSDP_FILE, 8, 0, 20);
    loader.add_pointer(RSDP_FILE, TABLES_FILE, 24, 8);
    loader.add_checksum(RSDP_FILE, 32, 0, 36);

    if tables.len() > TABLES_SIZE {
        return Err("virt ACPI tables exceed the size of their fw_cfg file".into());
    }
    tables.resize(TABLES_SIZE, 0);
    Ok(AcpiTables {
        tables,
        rsdp,
        loader: loader.into_records(),
    })
}

/// Read an AML PkgLength: its value and how many bytes encode it.
fn package_length(aml: &[u8], offset: usize) -> Option<(usize, usize)> {
    let lead = *aml.get(offset)?;
    let more = usize::from(lead >> 6);
    if more == 0 {
        return Some((usize::from(lead & 0x3f), 1));
    }
    let mut length = usize::from(lead & 0x0f);
    for index in 0..more {
        length |= usize::from(*aml.get(offset + 1 + index)?) << (4 + 8 * index);
    }
    Some((length, more + 1))
}

/// `Device (Cnnn) { Name (_HID, "ACPI0007") Name (_UID, n) }`.
fn processor(id: u8) -> Vec<u8> {
    let mut body = format!("C{id:03X}").into_bytes();
    body.extend_from_slice(b"\x08_HID\x0dACPI0007\0\x08_UID");
    body.extend(integer(id));
    package(&[0x5b, 0x82], &body)
}

/// The captured DSDT with one processor device per vCPU. The devices are the
/// first objects of the `\_SB` scope, which is the first object of the table.
fn dsdt(vcpus: u8) -> Result<Vec<u8>, String> {
    let malformed = || "the captured virt DSDT has an unexpected layout".to_string();
    if DSDT.get(HEADER) != Some(&0x10) {
        return Err(malformed());
    }
    let (length, width) = package_length(DSDT, HEADER + 1).ok_or_else(malformed)?;
    let scope_end = HEADER + 1 + length;
    let name_end = HEADER + 1 + width + 5;
    let captured_cpu = processor(0);
    let rest = name_end + captured_cpu.len();
    if scope_end > DSDT.len()
        || DSDT.get(HEADER + 1 + width..name_end) != Some(b"\\_SB_".as_slice())
        || DSDT.get(name_end..rest) != Some(captured_cpu.as_slice())
    {
        return Err(malformed());
    }
    let mut scope = b"\\_SB_".to_vec();
    for id in 0..vcpus {
        scope.extend(processor(id));
    }
    scope.extend_from_slice(&DSDT[rest..scope_end]);
    let mut dsdt = DSDT[..HEADER].to_vec();
    dsdt.extend(package(&[0x10], &scope));
    dsdt.extend_from_slice(&DSDT[scope_end..]);
    let length = dsdt.len() as u32;
    dsdt[4..8].copy_from_slice(&length.to_le_bytes());
    dsdt[9] = 0;
    Ok(dsdt)
}

/// FADT of ACPI 6.3 for a hardware-reduced platform that starts its CPUs
/// through PSCI on the HVC conduit.
fn fadt() -> Vec<u8> {
    let mut b = table(*b"FACP", 6, 276 - HEADER);
    // Flags: HW_REDUCED_ACPI.
    put(&mut b, 112, 1 << 20, 4);
    // ARM boot architecture flags: PSCI_COMPLIANT and PSCI_USE_HVC.
    put(&mut b, 129, 0b11, 2);
    // FADT minor version.
    b[131] = 3;
    // Hypervisor vendor identity.
    b[268..272].copy_from_slice(b"QEMU");
    b
}

/// MADT: the GICv2 distributor, one GICC per CPU, and the GICv2m frame.
fn madt(platform: &Platform) -> Vec<u8> {
    let mut b = table(*b"APIC", 4, 8);
    // GICD: type 0x0c, length 24.
    let mut gicd = vec![0u8; 24];
    gicd[..2].copy_from_slice(&[0x0c, 24]);
    put(&mut gicd, 8, map::GICD_BASE, 8);
    gicd[20] = 2;
    b.extend(gicd);
    for cpu in 0..u64::from(platform.vcpus) {
        // GICC: type 0x0b, length 80.
        let mut gicc = vec![0u8; 80];
        gicc[..2].copy_from_slice(&[0x0b, 80]);
        put(&mut gicc, 4, cpu, 4); // CPU interface number
        put(&mut gicc, 8, cpu, 4); // ACPI processor UID
        put(&mut gicc, 12, 1, 4); // flags: enabled
        put(
            &mut gicc,
            20,
            platform.performance_interrupt.unwrap_or(0).into(),
            4,
        );
        put(&mut gicc, 32, map::GICC_BASE, 8);
        put(&mut gicc, 40, map::GICV_BASE, 8);
        put(&mut gicc, 48, map::GICH_BASE, 8);
        put(&mut gicc, 68, cpu, 8); // MPIDR: affinity level 0 is the index
        b.extend(gicc);
    }
    // GIC MSI frame: type 0x0d, length 24. Flag 1: the SPI range below
    // replaces what the frame's MSI_TYPER says.
    let mut frame = vec![0u8; 24];
    frame[..2].copy_from_slice(&[0x0d, 24]);
    put(&mut frame, 8, map::V2M_BASE, 8);
    put(&mut frame, 16, 1, 4);
    put(&mut frame, 20, map::V2M_SPI_COUNT.into(), 2);
    put(&mut frame, 22, map::V2M_SPI_BASE.into(), 2);
    b.extend(frame);
    finish(b)
}

/// PPTT: one package, one container under it, and one leaf per CPU.
fn pptt(vcpus: u8) -> Vec<u8> {
    let mut b = table(*b"PPTT", 2, 0);
    let node = |b: &mut Vec<u8>, flags: u32, parent: usize, id: u32| -> usize {
        let start = b.len();
        let mut n = vec![0u8; 20];
        n[1] = 20;
        put(&mut n, 4, flags.into(), 4);
        put(&mut n, 8, parent as u64, 4);
        put(&mut n, 12, id.into(), 4);
        b.extend(n);
        start
    };
    // Flags 0x11: a physical package whose children are identical.
    let package = node(&mut b, 0x11, 0, 0);
    let container = node(&mut b, 0x11, package, 0);
    for cpu in 0..u32::from(vcpus) {
        // Flags 0x0a: the ACPI processor ID is valid, and the node is a leaf.
        node(&mut b, 0x0a, container, cpu);
    }
    finish(b)
}

/// GTDT: the four timer interrupts of the generic timer, no memory-mapped
/// counter frame and no platform timer.
fn gtdt() -> Vec<u8> {
    let mut b = table(*b"GTDT", 3, 104 - HEADER);
    put(&mut b, 36, u64::MAX, 8); // CntControlBase: none
    put(&mut b, 48, map::TIMER_SECURE_EL1_GSIV.into(), 4);
    put(&mut b, 56, map::TIMER_NON_SECURE_EL1_GSIV.into(), 4);
    put(&mut b, 60, 1 << 2, 4); // always-on capability
    put(&mut b, 64, map::TIMER_VIRTUAL_GSIV.into(), 4);
    put(&mut b, 72, map::TIMER_EL2_GSIV.into(), 4);
    put(&mut b, 80, u64::MAX, 8); // CntReadBase: none
    b
}

fn mcfg() -> Vec<u8> {
    let mut b = table(*b"MCFG", 1, 24);
    put(&mut b, 44, map::ECAM_BASE, 8);
    b[55] = 255; // last bus
    b
}

/// A Generic Address Structure for the 32-bit registers of the PL011.
fn uart_address() -> [u8; 12] {
    let mut gas = [0u8; 12];
    gas[1] = 32; // register bit width
    gas[3] = 3; // access size: dword
    gas[4..].copy_from_slice(&map::UART_BASE.to_le_bytes());
    gas
}

/// SPCR revision 2: the PL011 as the serial console.
fn spcr() -> Vec<u8> {
    let mut b = table(*b"SPCR", 2, 80 - HEADER);
    b[36] = 3; // interface type: ARM PL011
    b[40..52].copy_from_slice(&uart_address());
    b[52] = 1 << 3; // interrupt type: ARM GIC
    put(&mut b, 54, map::UART_GSIV.into(), 4);
    b[58] = 3; // 9600 baud
    b[60] = 1; // one stop bit
    b[61] = 1 << 1; // flow control: RTS/CTS
    put(&mut b, 64, 0xffff, 2); // not a PCI device
    put(&mut b, 66, 0xffff, 2);
    b
}

/// DBG2: the PL011 as the one debug port.
fn dbg2() -> Vec<u8> {
    const NAME: &[u8] = b"COM0\0";
    let mut b = table(*b"DBG2", 0, 8);
    put(&mut b, 36, 44, 4); // offset of the device information
    put(&mut b, 40, 1, 4); // one device
    let mut device = vec![0u8; 22];
    device[3] = 1; // one generic address register
    put(&mut device, 4, NAME.len() as u64, 2);
    put(&mut device, 6, 38, 2); // offset of the namespace string
    put(&mut device, 12, 0x8000, 2); // port type: serial
    put(&mut device, 14, 3, 2); // port subtype: ARM PL011
    put(&mut device, 18, 22, 2); // offset of the address register
    put(&mut device, 20, 34, 2); // offset of the address size
    device.extend_from_slice(&uart_address());
    device.extend_from_slice(&map::UART_SIZE.to_le_bytes());
    device.extend_from_slice(NAME);
    let length = device.len() as u64;
    put(&mut device, 1, length, 2);
    b.extend(device);
    finish(b)
}

/// IORT revision E.b: one PCI root complex node, with no ID mapping because
/// a GICv2m machine has neither an ITS nor an SMMU.
fn iort() -> Vec<u8> {
    let mut b = table(*b"IORT", 5, 12);
    put(&mut b, 36, 1, 4); // one node
    put(&mut b, 40, 48, 4); // offset of the node array
    let mut node = vec![0u8; 36];
    node[0] = 2; // root complex
    put(&mut node, 1, 36, 2);
    node[3] = 3; // node revision
    put(&mut node, 12, 36, 4); // offset of the (empty) ID mapping array
    put(&mut node, 16, 1, 4); // cache coherent
    node[23] = 3; // memory access flags: coherent path, DACS
    node[32] = 64; // memory address size limit in bits
    b.extend(node);
    finish(b)
}

fn finish(mut table: Vec<u8>) -> Vec<u8> {
    let length = table.len() as u32;
    table[4..8].copy_from_slice(&length.to_le_bytes());
    table
}
