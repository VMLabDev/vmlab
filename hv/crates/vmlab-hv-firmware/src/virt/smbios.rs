//! SMBIOS 3.0 structures (DMTF DSP0134 3.0) of `virt`, as QEMU hands them to
//! the firmware: no type 0, which AAVMF supplies, and a 64-bit entry point
//! with no address and no checksum, which AAVMF completes.

use super::{Platform, map};
use crate::smbios::SmbiosTables;

const VERSION: &str = "virt-11.1";

fn put(data: &mut [u8], offset: usize, value: u64, width: usize) {
    data[offset..offset + width].copy_from_slice(&value.to_le_bytes()[..width]);
}

fn structure(kind: u8, length: u8, handle: u16) -> Vec<u8> {
    let mut result = vec![0; usize::from(length)];
    result[0] = kind;
    result[1] = length;
    put(&mut result, 2, handle.into(), 2);
    result
}

fn finish(mut data: Vec<u8>, strings: &[&str]) -> Vec<u8> {
    for string in strings {
        data.extend_from_slice(string.as_bytes());
        data.push(0);
    }
    data.push(0);
    if strings.is_empty() {
        data.push(0);
    }
    data
}

pub fn generate(platform: &Platform) -> Result<SmbiosTables, String> {
    platform.validate()?;
    let cpus = platform.vcpus;
    let ram = platform.ram_size;
    let mut records = Vec::new();

    // Type 1, system. SMBIOS stores the first three UUID fields little-endian.
    let mut system = structure(1, 27, 0x0100);
    system[4..7].copy_from_slice(&[1, 2, 3]);
    let mut uuid = platform.uuid;
    uuid[..4].reverse();
    uuid[4..6].reverse();
    uuid[6..8].reverse();
    system[8..24].copy_from_slice(&uuid);
    system[24] = 6; // wake-up type: power switch
    records.push(finish(system, &["QEMU", "QEMU Virtual Machine", VERSION]));

    // Type 3, chassis.
    let mut chassis = structure(3, 22, 0x0300);
    chassis[4..7].copy_from_slice(&[1, 1, 2]);
    chassis[9..13].copy_from_slice(&[3, 3, 3, 2]);
    records.push(finish(chassis, &["QEMU", VERSION]));

    // Type 4, processor: one socket that holds every CPU as a core.
    let mut cpu = structure(4, 48, 0x0400);
    cpu[4..8].copy_from_slice(&[1, 3, 0xfe, 2]);
    cpu[16] = 3;
    put(&mut cpu, 20, 2000, 2); // maximum speed in MHz
    put(&mut cpu, 22, 2000, 2); // current speed
    cpu[24..26].copy_from_slice(&[0x41, 1]); // populated and enabled; upgrade: other
    cpu[26..32].fill(0xff); // no cache structures
    cpu[35..38].fill(cpus); // cores, enabled cores, threads
    put(&mut cpu, 38, 1 << 1, 2); // 64-bit capable
    put(&mut cpu, 40, 1, 2); // processor family 2
    for offset in [42, 44, 46] {
        put(&mut cpu, offset, cpus.into(), 2);
    }
    records.push(finish(cpu, &["CPU 0", "QEMU", VERSION]));

    // Type 16, the memory array. The capacity is in KiB.
    let mut array = structure(16, 23, 0x1000);
    array[4..7].copy_from_slice(&[1, 3, 6]);
    put(&mut array, 7, ram / 1024, 4);
    put(&mut array, 11, 0xfffe, 2);
    put(&mut array, 13, 1, 2);
    records.push(finish(array, &[]));

    // Type 17, one memory device. From 32 GiB the size is in the extended field.
    let mut dimm = structure(17, 40, 0x1100);
    put(&mut dimm, 4, 0x1000, 2);
    put(&mut dimm, 6, 0xfffe, 2);
    put(&mut dimm, 8, 0xffff, 2);
    put(&mut dimm, 10, 0xffff, 2);
    let megabytes = ram >> 20;
    if megabytes < 0x7fff {
        put(&mut dimm, 12, megabytes, 2);
    } else {
        put(&mut dimm, 12, 0x7fff, 2);
        put(&mut dimm, 28, megabytes, 4);
    }
    dimm[14] = 9; // DIMM
    dimm[16] = 1;
    dimm[18] = 7; // RAM
    put(&mut dimm, 19, 2, 2); // type detail: other
    dimm[23] = 2;
    records.push(finish(dimm, &["DIMM 0", "QEMU"]));

    // Type 19, the address range of the array, in KiB.
    let mut range = structure(19, 31, 0x1300);
    put(&mut range, 4, map::RAM_BASE / 1024, 4);
    put(&mut range, 8, (map::RAM_BASE + ram - 1) / 1024, 4);
    put(&mut range, 12, 0x1000, 2);
    range[14] = 1;
    records.push(finish(range, &[]));

    records.push(finish(structure(32, 11, 0x2000), &[]));
    records.push(finish(structure(127, 4, 0x7f00), &[]));
    let tables: Vec<u8> = records.into_iter().flatten().collect();

    let mut anchor = vec![0; 24];
    anchor[..5].copy_from_slice(b"_SM3_");
    anchor[6] = 24;
    anchor[7] = 3; // SMBIOS 3.0
    anchor[10] = 1; // entry point revision
    put(&mut anchor, 12, tables.len() as u64, 4);
    Ok(SmbiosTables { anchor, tables })
}
