//! The hwref diff of aarch64 `virt` (phase 3 D9, phase 3d card C11).
//!
//! The reference is `docs/hwref/virt-fwcfg`: what QEMU 11.1.3 hands a
//! firmware through fw_cfg, captured over qtest. The candidate is what
//! `vmlab_hv_firmware::virt` generates for the same machine. Both sides are
//! compared byte for byte: before a firmware runs there is no relocated
//! pointer and no checksum to normalise. A difference must be on the
//! allow-list `scripts/hwref/approved-differences-virt.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;
use vm_memory::GuestAddress;
use vmlab_hv_devices::{BusDevice, GuestMemory};
use vmlab_hv_firmware::FwCfg;
use vmlab_hv_firmware::virt::{self, Platform, map};

const TABLES: &str = "etc/acpi/tables";
const RSDP: &str = "etc/acpi/rsdp";
const LOADER: &str = "etc/table-loader";

fn hv_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One captured configuration of QEMU.
struct Capture {
    name: String,
    record: Value,
    /// File name to (selector, content), as the fw_cfg directory lists them.
    files: BTreeMap<String, (u16, Vec<u8>)>,
}

impl Capture {
    fn load(name: &str) -> Self {
        let root = hv_root().join("docs/hwref/virt-fwcfg").join(name);
        let record: Value =
            serde_json::from_slice(&std::fs::read(root.join("capture.json")).unwrap()).unwrap();
        let mut files = BTreeMap::new();
        for entry in record["files"].as_array().unwrap() {
            let file = entry["name"].as_str().unwrap();
            let size = entry["size"].as_u64().unwrap() as usize;
            let mut data = if entry.get("stored_as").is_some() {
                // The file is its tables in the recorded order, then zeros.
                let mut data = Vec::new();
                for table in record["acpi_tables"].as_array().unwrap() {
                    assert_eq!(table["offset"].as_u64().unwrap() as usize, data.len());
                    data.extend(
                        std::fs::read(root.join("acpi").join(table["signature"].as_str().unwrap()))
                            .unwrap(),
                    );
                }
                data
            } else {
                std::fs::read(root.join("fwcfg").join(file)).unwrap()
            };
            assert!(data.len() <= size, "{name}: {file} is longer than recorded");
            data.resize(size, 0);
            let select = entry["select"].as_u64().unwrap() as u16;
            files.insert(file.to_string(), (select, data));
        }
        Self {
            name: name.into(),
            record,
            files,
        }
    }

    fn all() -> Vec<Self> {
        let mut names: Vec<_> = std::fs::read_dir(hv_root().join("docs/hwref/virt-fwcfg"))
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.path().join("capture.json").is_file())
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names.len(), 16, "the captures of virt: {names:?}");
        names.iter().map(|name| Self::load(name)).collect()
    }

    /// QEMU presents a PMU unless the capture was made with `pmu=off`.
    fn has_pmu(&self) -> bool {
        !self.record["cpu"].as_str().unwrap().contains("pmu=off")
    }

    fn uuid(&self) -> [u8; 16] {
        let argv: Vec<_> = self.record["qemu_argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|word| word.as_str().unwrap())
            .collect();
        let mut uuid = [0; 16];
        if let Some(index) = argv.iter().position(|word| *word == "-uuid") {
            let digits: String = argv[index + 1].chars().filter(|c| *c != '-').collect();
            for (index, byte) in uuid.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&digits[index * 2..index * 2 + 2], 16).unwrap();
            }
        }
        uuid
    }

    /// The machine of this capture. `pmu` says what the candidate presents.
    fn platform(&self, pmu: bool) -> Platform {
        Platform {
            vcpus: self.record["smp"].as_u64().unwrap() as u8,
            ram_size: self.record["memory_mib"].as_u64().unwrap() << 20,
            uuid: self.uuid(),
            performance_interrupt: pmu.then_some(map::QEMU_PMU_GSIV),
            bootorder: Vec::new(),
        }
    }

    fn contents(&self) -> BTreeMap<String, Vec<u8>> {
        self.files
            .iter()
            .map(|(name, (_, data))| (name.clone(), data.clone()))
            .collect()
    }
}

fn candidate(platform: &Platform) -> BTreeMap<String, Vec<u8>> {
    virt::files(platform).unwrap().into_iter().collect()
}

/// One run of differing bytes, or a file that one side lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Difference {
    file: String,
    /// The ACPI table and the offset inside it, for `etc/acpi/tables`.
    table: Option<(String, usize)>,
    offset: usize,
    reference: Vec<u8>,
    candidate: Vec<u8>,
}

/// The tables of an `etc/acpi/tables` file: (signature, offset, length).
fn acpi_tables(file: &[u8]) -> Vec<(String, usize, usize)> {
    let mut tables = Vec::new();
    let mut offset = 0;
    while offset + 36 <= file.len() && file[offset..offset + 4] != [0; 4] {
        let length = u32::from_le_bytes(file[offset + 4..offset + 8].try_into().unwrap()) as usize;
        assert!(length >= 36 && offset + length <= file.len());
        let signature = String::from_utf8(file[offset..offset + 4].to_vec()).unwrap();
        tables.push((signature, offset, length));
        offset += length;
    }
    assert!(file[offset..].iter().all(|byte| *byte == 0));
    tables
}

fn diff(
    reference: &BTreeMap<String, Vec<u8>>,
    candidate: &BTreeMap<String, Vec<u8>>,
) -> Vec<Difference> {
    let mut result = Vec::new();
    let names: std::collections::BTreeSet<_> = reference.keys().chain(candidate.keys()).collect();
    for name in names {
        let (Some(a), Some(b)) = (reference.get(name), candidate.get(name)) else {
            result.push(Difference {
                file: name.clone(),
                table: None,
                offset: 0,
                reference: reference.get(name).cloned().unwrap_or_default(),
                candidate: candidate.get(name).cloned().unwrap_or_default(),
            });
            continue;
        };
        let tables = if name == TABLES {
            acpi_tables(a)
        } else {
            Vec::new()
        };
        let length = a.len().max(b.len());
        let mut offset = 0;
        while offset < length {
            if a.get(offset) == b.get(offset) {
                offset += 1;
                continue;
            }
            let start = offset;
            while offset < length && a.get(offset) != b.get(offset) {
                offset += 1;
            }
            result.push(Difference {
                file: name.clone(),
                table: tables
                    .iter()
                    .find(|(_, base, size)| (*base..base + size).contains(&start))
                    .map(|(signature, base, _)| (signature.clone(), start - base)),
                offset: start,
                reference: a.get(start..offset.min(a.len())).unwrap_or_default().to_vec(),
                candidate: b.get(start..offset.min(b.len())).unwrap_or_default().to_vec(),
            });
        }
    }
    result
}

/// The interrupt controller structures of a MADT: (type, offset in the table).
fn madt_structures(madt: &[u8]) -> Vec<(u8, usize)> {
    let mut result = Vec::new();
    let mut offset = 44;
    while offset < madt.len() {
        result.push((madt[offset], offset));
        offset += usize::from(madt[offset + 1]);
    }
    result
}

fn allow_list() -> Value {
    let path = hv_root().join("scripts/hwref/approved-differences-virt.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// Whether one rule of the allow-list covers one difference exactly: the
/// file, the table, the structure type, the field and both values.
fn rule_covers(rule: &Value, difference: &Difference, reference: &BTreeMap<String, Vec<u8>>) -> bool {
    let Some((signature, table_offset)) = &difference.table else {
        return false;
    };
    if rule["file"] != difference.file.as_str() || rule["table"] != signature.as_str() {
        return false;
    }
    let file = &reference[&difference.file];
    let (_, base, length) = acpi_tables(file)
        .into_iter()
        .find(|(name, _, _)| name == signature)
        .unwrap();
    let table = &file[base..base + length];
    let field = rule["field_offset"].as_u64().unwrap() as usize;
    let width = rule["field_width"].as_u64().unwrap() as usize;
    let kind = rule["structure_type"].as_u64().unwrap() as u8;
    let Some((_, structure)) = madt_structures(table)
        .into_iter()
        .find(|(found, start)| *found == kind && (start + field..start + field + width).contains(table_offset))
    else {
        return false;
    };
    // The whole field must hold the two approved values, not only the bytes
    // that differ.
    let value = |bytes: &[u8]| {
        let mut wide = [0u8; 8];
        wide[..width].copy_from_slice(&bytes[..width]);
        u64::from_le_bytes(wide)
    };
    let start = base + structure + field;
    let mut patched = file[start..start + width].to_vec();
    let inside = difference.offset - start;
    if inside + difference.candidate.len() > width
        || difference.candidate.len() != difference.reference.len()
    {
        return false;
    }
    patched[inside..inside + difference.candidate.len()].copy_from_slice(&difference.candidate);
    value(&file[start..start + width]) == rule["reference"].as_u64().unwrap()
        && value(&patched) == rule["candidate"].as_u64().unwrap()
}

/// Split differences into those no rule covers, and count the use of each rule.
fn apply_allow_list(
    differences: &[Difference],
    reference: &BTreeMap<String, Vec<u8>>,
    allow: &Value,
) -> (Vec<Difference>, BTreeMap<String, usize>) {
    let mut unapproved = Vec::new();
    let mut used = BTreeMap::new();
    for difference in differences {
        let rule = allow["differences"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule_covers(rule, difference, reference));
        match rule {
            Some(rule) => *used.entry(rule["id"].as_str().unwrap().to_string()).or_default() += 1,
            None => unapproved.push(difference.clone()),
        }
    }
    (unapproved, used)
}

#[test]
fn generator_reproduces_every_qemu_capture_byte_for_byte() {
    // With the PMU interrupt that the captured QEMU CPU has, nothing differs.
    for capture in Capture::all() {
        let platform = capture.platform(capture.has_pmu());
        let differences = diff(&capture.contents(), &candidate(&platform));
        assert!(differences.is_empty(), "{}: {differences:x?}", capture.name);
    }
}

#[test]
fn product_tables_differ_from_qemu_only_inside_the_allow_list() {
    // vmlab-hv presents no PMU (V5). Against QEMU's default CPU the diff is
    // one field per CPU, and the allow-list covers exactly that.
    let allow = allow_list();
    let mut total = BTreeMap::new();
    for capture in Capture::all() {
        let reference = capture.contents();
        let differences = diff(&reference, &candidate(&capture.platform(false)));
        let (unapproved, used) = apply_allow_list(&differences, &reference, &allow);
        assert!(unapproved.is_empty(), "{}: {unapproved:x?}", capture.name);
        let cpus = capture.record["smp"].as_u64().unwrap() as usize;
        if capture.has_pmu() {
            assert_eq!(differences.len(), cpus, "{}", capture.name);
            assert_eq!(used.get("V5"), Some(&cpus), "{}", capture.name);
        } else {
            // QEMU's own answer for a CPU without a PMU is our answer.
            assert!(differences.is_empty(), "{}: {differences:x?}", capture.name);
        }
        for (id, count) in used {
            *total.entry(id).or_insert(0) += count;
        }
    }
    // No rule is on the list without a difference that needs it.
    for rule in allow["differences"].as_array().unwrap() {
        let id = rule["id"].as_str().unwrap();
        assert!(total.contains_key(id), "allow-list rule {id} covers nothing");
    }
}

#[test]
fn allow_list_keeps_every_other_difference() {
    let allow = allow_list();
    let capture = Capture::load("smp2-2g");
    let reference = capture.contents();
    let good = candidate(&capture.platform(false));
    let (_, madt, _) = acpi_tables(&good[TABLES])
        .into_iter()
        .find(|(signature, _, _)| signature == "APIC")
        .unwrap();
    let gicc = madt + 44 + 24;
    let spcr = acpi_tables(&good[TABLES])
        .into_iter()
        .find(|(signature, _, _)| signature == "SPCR")
        .unwrap()
        .1;
    // (file, offset, new byte): none of these is the approved difference.
    let cases = [
        (TABLES, gicc + 20, 22),      // a PMU interrupt other than "none"
        (TABLES, gicc + 21, 1),       // the same field, a value that is not 0
        (TABLES, gicc + 32, 0xff),    // the base of the CPU interface
        (TABLES, gicc + 12, 0),       // a disabled CPU
        (TABLES, madt + 44 + 20, 3),  // the GIC version
        (TABLES, spcr + 54, 34),      // the interrupt of the console
        (RSDP, 15, 0),                // the revision of the RSDP
        (LOADER, 4, b'x'),            // a loader record
        ("etc/smbios/smbios-tables", 4, 9),
    ];
    for (file, offset, byte) in cases {
        let mut changed = good.clone();
        assert_ne!(changed.get_mut(file).unwrap()[offset], byte);
        changed.get_mut(file).unwrap()[offset] = byte;
        let (unapproved, _) = apply_allow_list(&diff(&reference, &changed), &reference, &allow);
        assert_eq!(unapproved.len(), 1, "{file} at {offset:#x}: {unapproved:x?}");
        assert_eq!(unapproved[0].file, file);
    }
    // A file that one side lacks, and a file of another size, are differences.
    let mut missing = good.clone();
    missing.remove("etc/tpm/log");
    missing.insert("etc/extra".into(), vec![1]);
    let (unapproved, _) = apply_allow_list(&diff(&reference, &missing), &reference, &allow);
    assert_eq!(unapproved.len(), 2);
    let mut longer = good.clone();
    longer.get_mut(RSDP).unwrap().push(0);
    let (unapproved, _) = apply_allow_list(&diff(&reference, &longer), &reference, &allow);
    assert_eq!(unapproved.len(), 1);
}

#[test]
fn cpu_model_changes_no_firmware_input() {
    // G2: the CPU that the guest sees is not QEMU's `max`. QEMU's own files
    // for `-cpu max` and `-cpu cortex-a53` are the same, so G2 needs no rule
    // at this level.
    let allow = allow_list();
    let max = Capture::load("smp2-2g");
    let a53 = Capture::load("a53-smp2-2g");
    assert_eq!(a53.record["cpu"], "cortex-a53");
    assert!(diff(&max.contents(), &a53.contents()).is_empty());
    let noted: Vec<_> = allow["no_difference"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(noted, ["G2"]);
}

fn select(device: &mut FwCfg, key: u16) {
    device.write(0, &key.to_le_bytes());
}

fn read(device: &mut FwCfg, length: usize) -> Vec<u8> {
    (0..length)
        .map(|_| {
            let mut byte = [0];
            device.read(1, &mut byte);
            byte[0]
        })
        .collect()
}

#[test]
fn fw_cfg_device_presents_the_directory_and_items_of_qemu() {
    for capture in Capture::all() {
        let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let platform = capture.platform(capture.has_pmu());
        let mut device = FwCfg::for_virt(memory, &platform).unwrap();
        select(&mut device, 0x19);
        let count = u32::from_be_bytes(read(&mut device, 4).try_into().unwrap()) as usize;
        assert_eq!(count, capture.files.len(), "{}", capture.name);
        let directory = read(&mut device, count * 64);
        for (entry, (name, (key, data))) in directory.chunks(64).zip(&capture.files) {
            let end = entry[8..].iter().position(|byte| *byte == 0).unwrap();
            assert_eq!(std::str::from_utf8(&entry[8..8 + end]).unwrap(), name);
            assert_eq!(u16::from_be_bytes(entry[4..6].try_into().unwrap()), *key);
            assert_eq!(
                u32::from_be_bytes(entry[..4].try_into().unwrap()) as usize,
                data.len()
            );
            select(&mut device, *key);
            assert!(read(&mut device, data.len()) == *data, "{}: {name}", capture.name);
        }
        // The numeric items a firmware reads. An item QEMU leaves unset
        // reads as zeros on both sides.
        for (name, key) in [
            ("signature", 0x00),
            ("id", 0x01),
            ("uuid", 0x02),
            ("ram_size", 0x03),
            ("nographic", 0x04),
            ("nb_cpus", 0x05),
            ("boot_menu", 0x0e),
            ("max_cpus", 0x0f),
        ] {
            let expected = capture.record["numeric"][name].as_str().unwrap();
            select(&mut device, key);
            let actual: String = read(&mut device, expected.len() / 2)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(actual, expected, "{}: {name}", capture.name);
        }
    }
}

/// What a firmware does with `etc/table-loader`: place each file, add the
/// base of the file a pointer names, then fill each checksum.
fn run_loader(files: &BTreeMap<String, Vec<u8>>) -> (BTreeMap<String, (u64, Vec<u8>)>, usize) {
    let text = |field: &[u8]| {
        let end = field.iter().position(|byte| *byte == 0).unwrap();
        String::from_utf8(field[..end].to_vec()).unwrap()
    };
    let word = |record: &[u8], offset: usize| {
        u32::from_le_bytes(record[offset..offset + 4].try_into().unwrap()) as usize
    };
    let mut placed: BTreeMap<String, (u64, Vec<u8>)> = BTreeMap::new();
    let mut next = 0x1_3c00_0000u64;
    let mut commands = 0;
    for record in files[LOADER].chunks(128) {
        assert_eq!(record.len(), 128);
        commands += 1;
        match word(record, 0) {
            1 => {
                let name = text(&record[4..60]);
                let alignment = word(record, 60) as u64;
                assert!(alignment.is_power_of_two());
                assert!(matches!(record[64], 1 | 2), "zone of {name}");
                next = next.next_multiple_of(alignment);
                let data = files[&name].clone();
                let size = data.len() as u64;
                assert!(placed.insert(name, (next, data)).is_none());
                next += size;
            }
            2 => {
                let source = placed[&text(&record[60..116])].0;
                let (_, data) = placed.get_mut(&text(&record[4..60])).unwrap();
                let (offset, size) = (word(record, 116), usize::from(record[120]));
                let mut wide = [0u8; 8];
                wide[..size].copy_from_slice(&data[offset..offset + size]);
                let value = u64::from_le_bytes(wide).wrapping_add(source);
                data[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
            }
            3 => {
                let (_, data) = placed.get_mut(&text(&record[4..60])).unwrap();
                let (offset, start, length) = (word(record, 60), word(record, 64), word(record, 68));
                assert_eq!(data[offset], 0);
                let sum = data[start..start + length]
                    .iter()
                    .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
                data[offset] = 0u8.wrapping_sub(sum);
            }
            other => panic!("loader command {other}"),
        }
    }
    (placed, commands)
}

#[test]
fn loader_program_yields_tables_a_guest_can_walk() {
    for vcpus in 1..=virt::MAX_VCPUS {
        let platform = Platform {
            vcpus,
            ..Platform::default()
        };
        let (placed, commands) = run_loader(&candidate(&platform));
        assert_eq!(commands, 25);
        let (rsdp_base, rsdp) = &placed[RSDP];
        let (base, tables) = &placed[TABLES];
        assert_eq!(rsdp_base % 16, 0);
        assert_eq!(base % 64, 0);
        let sum = |bytes: &[u8]| bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        assert_eq!(&rsdp[..8], b"RSD PTR ");
        assert_eq!(sum(&rsdp[..20]), 0);
        assert_eq!(sum(rsdp), 0);
        let table_at = |address: u64| {
            let offset = (address - base) as usize;
            let length =
                u32::from_le_bytes(tables[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let table = &tables[offset..offset + length];
            assert_eq!(sum(table), 0, "checksum of {:?}", &table[..4]);
            table
        };
        let xsdt = table_at(u64::from_le_bytes(rsdp[24..32].try_into().unwrap()));
        assert_eq!(&xsdt[..4], b"XSDT");
        let signatures: Vec<_> = xsdt[36..]
            .chunks(8)
            .map(|entry| table_at(u64::from_le_bytes(entry.try_into().unwrap()))[..4].to_vec())
            .collect();
        let expected: [&[u8; 4]; 9] = [
            b"FACP", b"APIC", b"PPTT", b"PPTT", b"GTDT", b"MCFG", b"SPCR", b"DBG2", b"IORT",
        ];
        assert!(signatures.iter().map(Vec::as_slice).eq(expected.iter().map(|s| s.as_slice())));
        let fadt = table_at(u64::from_le_bytes(xsdt[36..44].try_into().unwrap()));
        let dsdt = table_at(u64::from_le_bytes(fadt[140..148].try_into().unwrap()));
        assert_eq!(&dsdt[..4], b"DSDT");
        // One processor device and one GICC per CPU, with matching UIDs.
        for cpu in 0..vcpus {
            let name = format!("C{cpu:03X}\x08_HID\x0dACPI0007");
            assert_eq!(
                dsdt.windows(name.len()).filter(|w| *w == name.as_bytes()).count(),
                1
            );
        }
        assert!(!dsdt.windows(4).any(|w| w == format!("C{vcpus:03X}").as_bytes()));
        let madt = table_at(u64::from_le_bytes(xsdt[44..52].try_into().unwrap()));
        let giccs: Vec<_> = madt_structures(madt)
            .into_iter()
            .filter(|(kind, _)| *kind == 0x0b)
            .map(|(_, offset)| u32::from_le_bytes(madt[offset + 8..offset + 12].try_into().unwrap()))
            .collect();
        assert!(giccs.iter().copied().eq(0..u32::from(vcpus)));
    }
}

#[test]
fn product_tables_state_psci_over_hvc_and_no_pmu() {
    let files = candidate(&Platform::default());
    let tables = acpi_tables(&files[TABLES]);
    let table = |signature: &str| {
        let (_, base, length) = tables.iter().find(|(name, _, _)| name == signature).unwrap();
        &files[TABLES][*base..base + length]
    };
    let fadt = table("FACP");
    // Hardware-reduced ACPI, PSCI compliant, the HVC conduit (V1).
    assert_eq!(u32::from_le_bytes(fadt[112..116].try_into().unwrap()), 1 << 20);
    assert_eq!(u16::from_le_bytes(fadt[129..131].try_into().unwrap()), 0b11);
    let madt = table("APIC");
    for (kind, offset) in madt_structures(madt) {
        if kind == 0x0b {
            // No PMU interrupt (V5).
            assert_eq!(madt[offset + 20..offset + 24], [0; 4]);
        }
    }
    let gtdt = table("GTDT");
    assert_eq!(u32::from_le_bytes(gtdt[64..68].try_into().unwrap()), 27);
}

#[test]
fn machines_outside_virt_are_refused() {
    for platform in [
        Platform {
            vcpus: 0,
            ..Platform::default()
        },
        Platform {
            vcpus: virt::MAX_VCPUS + 1,
            ..Platform::default()
        },
        Platform {
            ram_size: 0,
            ..Platform::default()
        },
        Platform {
            ram_size: (1 << 30) + 4096,
            ..Platform::default()
        },
        Platform {
            ram_size: virt::MAX_RAM + (1 << 20),
            ..Platform::default()
        },
        Platform {
            bootorder: vec!["relative".into()],
            ..Platform::default()
        },
    ] {
        assert!(virt::files(&platform).is_err(), "{platform:?}");
    }
    let bootorder = Platform {
        bootorder: vec!["/pci@i0cf8/scsi@2/disk@0,0".into(), "/pci@i0cf8/usb@5".into()],
        ..Platform::default()
    };
    let files = candidate(&bootorder);
    assert_eq!(
        files["bootorder"],
        b"/pci@i0cf8/scsi@2/disk@0,0\n/pci@i0cf8/usb@5\0"
    );
}
