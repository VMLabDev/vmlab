//! Named Intel CPU and the MSRs vmlab-hv answers in user space for a macOS guest.

use crate::{error::VmmError, kvm::KvmVm};
use kvm_bindings::{CpuId, KVM_MAX_CPUID_ENTRIES, kvm_cpuid_entry2, kvm_enable_cap};
use kvm_ioctls::{Cap, MsrExitReason, VcpuFd};
use serde::Deserialize;
use vmlab_hv_proto::machine::{MachineDescription, MsrPolicy};

const APIC_BUS_KHZ: u32 = 1_000_000;

#[derive(Deserialize)]
struct Capture {
    leaves: Vec<Leaf>,
}
#[derive(Deserialize)]
struct Leaf {
    function: u32,
    index: u32,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
}

pub(crate) fn skylake_cpuid(id: u8, count: u32) -> Result<CpuId, VmmError> {
    let capture: Capture = serde_json::from_str(include_str!(
        "../../../docs/hwref/q35-macos-tahoe/cpuid.json"
    ))
    .map_err(|e| VmmError::Other(format!("invalid pinned Skylake CPUID: {e}")))?;
    let shift = count.next_power_of_two().trailing_zeros();
    let mut entries: Vec<_> = capture
        .leaves
        .into_iter()
        .map(|leaf| kvm_cpuid_entry2 {
            function: leaf.function,
            index: leaf.index,
            eax: leaf.eax,
            ebx: leaf.ebx,
            ecx: leaf.ecx,
            edx: leaf.edx,
            flags: if matches!(leaf.function, 4 | 7 | 0xb | 0xd) {
                kvm_bindings::KVM_CPUID_FLAG_SIGNIFCANT_INDEX
            } else {
                0
            },
            ..Default::default()
        })
        .collect();
    for entry in &mut entries {
        match entry.function {
            1 => {
                entry.ebx = (entry.ebx & 0xffff) | (count << 16) | (u32::from(id) << 24);
                if count > 1 {
                    entry.edx |= 1 << 28;
                } else {
                    entry.edx &= !(1 << 28);
                }
                // KVM supplies OSXSAVE from the guest's CR4 at execution time.
                entry.ecx &= !(1 << 27);
            }
            4 if entry.eax & 31 != 0 => {
                let sharing = if (entry.eax >> 5) & 7 == 3 {
                    (count - 1) << 14
                } else {
                    0
                };
                entry.eax = (entry.eax & 0x3fff) | sharing | ((count.min(64) - 1) << 26);
            }
            0xb => {
                let (width, cpus, level) = match entry.index {
                    0 => (0, 1, 1),
                    1 => (shift, count, 2),
                    _ => (0, 0, 0),
                };
                entry.eax = width;
                entry.ebx = cpus;
                entry.ecx = level << 8 | entry.index;
                entry.edx = u32::from(id);
            }
            _ => {}
        }
    }
    CpuId::from_entries(&entries).map_err(|e| VmmError::Other(format!("Skylake CPUID: {e}")))
}

pub(crate) fn configure_tsc(
    cpu: &VcpuFd,
    desc: &MachineDescription,
    report: bool,
) -> Result<(), VmmError> {
    if let Some(khz) = desc.tsc_khz {
        cpu.set_tsc_khz(khz)
            .map_err(|e| VmmError::ioctl("KVM_SET_TSC_KHZ", e))?;
    }
    if !desc.invtsc
        && !desc.vmware_cpuid_freq
        && desc.tsc_khz.is_none()
        && desc.cpu_model.as_deref() != Some("Skylake-Client-v4")
    {
        return Ok(());
    }
    let khz = cpu
        .get_tsc_khz()
        .map_err(|e| VmmError::ioctl("KVM_GET_TSC_KHZ", e))?;
    if desc.tsc_khz.is_some_and(|pin| pin != khz) {
        return Err(VmmError::Other(
            "KVM did not retain the requested TSC frequency".into(),
        ));
    }
    if report {
        eprintln!("vmlab-hv: tsc_khz={khz}");
    }
    let mut entries = cpu
        .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VmmError::ioctl("KVM_GET_CPUID2", e))?
        .as_slice()
        .to_vec();
    timing_leaves(&mut entries, khz);
    for entry in &mut entries {
        if entry.function == 1 && desc.vmware_cpuid_freq {
            entry.ecx |= 1 << 31;
        }
        if entry.function == 0x80000007 {
            entry.edx = (entry.edx & !(1 << 8)) | (u32::from(desc.invtsc) << 8);
        }
        if entry.function == 0x40000000 && desc.vmware_cpuid_freq {
            entry.eax = 0x40000010;
        }
    }
    if desc.vmware_cpuid_freq {
        entries.retain(|entry| entry.function != 0x40000010);
        entries.push(kvm_cpuid_entry2 {
            function: 0x40000010,
            eax: khz,
            ebx: APIC_BUS_KHZ,
            ..Default::default()
        });
    }
    let cpuid = CpuId::from_entries(&entries).map_err(|e| VmmError::Other(e.to_string()))?;
    cpu.set_cpuid2(&cpuid)
        .map_err(|e| VmmError::ioctl("KVM_SET_CPUID2", e))
}

fn timing_leaves(entries: &mut [kvm_cpuid_entry2], khz: u32) {
    // Keep the model's advertised leaves; never expose a host crystal ratio
    // that describes a different TSC from the one KVM runs.
    for entry in entries {
        match entry.function {
            0x15 => {
                entry.eax = 1000;
                entry.ebx = khz;
                entry.ecx = 1_000_000;
                entry.edx = 0;
            }
            0x16 => {
                entry.eax = khz / 1000;
                entry.ebx = khz / 1000;
            }
            _ => {}
        }
    }
}

/// How a macOS MSR answers a guest write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MsrWrite {
    /// The register is writable on real hardware; the write succeeds and is
    /// dropped, because the value read back stays 0.
    Drop,
    /// The register is read-only on real hardware; a write raises #GP there
    /// and here.
    Fault,
}

/// One MSR vmlab-hv answers for a macOS guest. Every read returns 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MacosMsr {
    pub(crate) index: u32,
    pub(crate) name: &'static str,
    pub(crate) write: MsrWrite,
}

const fn msr(index: u32, name: &'static str, write: MsrWrite) -> MacosMsr {
    MacosMsr { index, name, write }
}

/// The MSRs XNU touches that KVM has no model for, sorted by index. The
/// reason for each one is in `hv/docs/macos-msrs.md`, and a test keeps that
/// table and this one equal.
pub(crate) const MACOS_MSRS: &[MacosMsr] = &[
    msr(0xe7, "IA32_MPERF", MsrWrite::Drop),
    msr(0xe8, "IA32_APERF", MsrWrite::Drop),
    msr(0x3f8, "MSR_PKG_C3_RESIDENCY", MsrWrite::Fault),
    msr(0x3f9, "MSR_PKG_C6_RESIDENCY", MsrWrite::Fault),
    msr(0x3fa, "MSR_PKG_C7_RESIDENCY", MsrWrite::Fault),
    msr(0x3fc, "MSR_CORE_C3_RESIDENCY", MsrWrite::Fault),
    msr(0x3fd, "MSR_CORE_C6_RESIDENCY", MsrWrite::Fault),
    msr(0x3fe, "MSR_CORE_C7_RESIDENCY", MsrWrite::Fault),
    msr(0x60d, "MSR_PKG_C2_RESIDENCY", MsrWrite::Fault),
    msr(0x61d, "MSR_LLC_FLUSHED_RESIDENCY_TIMER", MsrWrite::Fault),
    msr(0x621, "MSR_RING_PERF_STATUS", MsrWrite::Fault),
    msr(0x630, "MSR_PKG_C8_RESIDENCY", MsrWrite::Fault),
    msr(0x631, "MSR_PKG_C9_RESIDENCY", MsrWrite::Fault),
    msr(0x632, "MSR_PKG_C10_RESIDENCY", MsrWrite::Fault),
    msr(0x64f, "MSR_CORE_PERF_LIMIT_REASONS", MsrWrite::Drop),
    msr(0x6b0, "MSR_GRAPHICS_PERF_LIMIT_REASONS", MsrWrite::Drop),
];

pub(crate) fn configure_msrs(kvm: &KvmVm, policy: MsrPolicy) -> Result<(), VmmError> {
    if policy == MsrPolicy::Fault {
        return Ok(());
    }
    if !kvm.kvm.check_extension(Cap::X86UserSpaceMsr) {
        return Err(VmmError::Other(
            "msr_policy=macos requires KVM_CAP_X86_USER_SPACE_MSR".into(),
        ));
    }
    kvm.vm
        .enable_cap(&kvm_enable_cap {
            cap: Cap::X86UserSpaceMsr as u32,
            args: [u64::from(MsrExitReason::Unknown.bits()), 0, 0, 0],
            ..Default::default()
        })
        .map_err(|e| VmmError::ioctl("KVM_ENABLE_CAP userspace MSRs", e))
}

/// Answers one userspace MSR exit. `Some(value)` completes the access (the
/// value matters to a read only); `None` makes KVM inject #GP.
pub(crate) fn user_msr(
    shared: &crate::vm::VmShared,
    index: u32,
    reason: MsrExitReason,
    write: bool,
) -> Option<u64> {
    let (value, line) = answer(
        *shared.msr_policy.lock(),
        &mut shared.ignored_msrs.lock(),
        index,
        reason,
        write,
    );
    if let Some(line) = line {
        eprintln!("{line}");
    }
    value
}

/// The answer to one access, and the line to log the first time a macOS
/// machine touches that index. `seen` rides the snapshot, so a rewind or a
/// reset does not log an index again.
fn answer(
    policy: MsrPolicy,
    seen: &mut std::collections::BTreeSet<u32>,
    index: u32,
    reason: MsrExitReason,
    write: bool,
) -> (Option<u64>, Option<String>) {
    match msr_action(policy, index, reason, write) {
        MsrAction::Answer(entry) => (
            Some(0),
            seen.insert(index).then(|| {
                format!(
                    "kvm: macOS MSR {index:#010x} ({}) answered in user space",
                    entry.name
                )
            }),
        ),
        MsrAction::Unhandled => (
            None,
            seen.insert(index).then(|| {
                let access = if write { "write" } else { "read" };
                format!("kvm: macOS guest faulted on unhandled MSR {index:#010x} ({access})")
            }),
        ),
        MsrAction::Strict => (None, None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MsrAction {
    /// The table answers the access.
    Answer(MacosMsr),
    /// A macOS machine reached an MSR, or a write, the table does not
    /// handle: #GP, and named once in the log so the table can grow.
    Unhandled,
    /// Not a macOS machine, or not KVM's unknown-MSR reason: #GP.
    Strict,
}

fn msr_action(policy: MsrPolicy, index: u32, reason: MsrExitReason, write: bool) -> MsrAction {
    if policy != MsrPolicy::Macos || reason != MsrExitReason::Unknown {
        return MsrAction::Strict;
    }
    match MACOS_MSRS
        .binary_search_by_key(&index, |entry| entry.index)
        .map(|at| MACOS_MSRS[at])
    {
        Ok(entry) if !write || entry.write == MsrWrite::Drop => MsrAction::Answer(entry),
        _ => MsrAction::Unhandled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::{Bytes, GuestAddress};

    #[test]
    fn cpuid_matches_every_captured_leaf_with_documented_dynamic_fields() {
        let capture: Capture = serde_json::from_str(include_str!(
            "../../../docs/hwref/q35-macos-tahoe/cpuid.json"
        ))
        .unwrap();
        let actual = skylake_cpuid(0, 4).unwrap();
        for expected in capture.leaves {
            let entry = actual
                .as_slice()
                .iter()
                .find(|entry| entry.function == expected.function && entry.index == expected.index)
                .unwrap();
            let ecx = if expected.function == 1 {
                expected.ecx & !(1 << 27)
            } else {
                expected.ecx
            };
            assert_eq!(
                (entry.eax, entry.ebx, entry.ecx, entry.edx),
                (expected.eax, expected.ebx, ecx, expected.edx),
                "leaf {:#x}/{}",
                expected.function,
                expected.index
            );
        }
    }

    #[test]
    fn cpuid_topology_tracks_the_requested_vcpu_count_and_apic_id() {
        for count in [1, 2, 4, 8] {
            for id in 0..count {
                let cpuid = skylake_cpuid(id as u8, count).unwrap();
                let leaf = |function, index| {
                    cpuid
                        .as_slice()
                        .iter()
                        .find(|entry| entry.function == function && entry.index == index)
                        .unwrap()
                };
                assert_eq!(leaf(1, 0).ebx >> 24, id);
                assert_eq!((leaf(1, 0).ebx >> 16) & 255, count);
                assert_eq!(leaf(1, 0).edx & (1 << 28) != 0, count > 1);
                assert_eq!(leaf(0xb, 1).ebx, count);
                assert_eq!(leaf(0xb, 1).edx, id);
                assert_eq!(leaf(0xb, 1).eax, count.trailing_zeros());
            }
        }
    }

    #[test]
    fn macos_table_is_sorted_and_answers_only_its_own_indices() {
        assert!(
            MACOS_MSRS
                .windows(2)
                .all(|pair| pair[0].index < pair[1].index)
        );
        for entry in MACOS_MSRS {
            assert_eq!(
                msr_action(MsrPolicy::Macos, entry.index, MsrExitReason::Unknown, false),
                MsrAction::Answer(*entry),
                "{}",
                entry.name
            );
            let write = msr_action(MsrPolicy::Macos, entry.index, MsrExitReason::Unknown, true);
            match entry.write {
                MsrWrite::Drop => assert_eq!(write, MsrAction::Answer(*entry), "{}", entry.name),
                MsrWrite::Fault => assert_eq!(write, MsrAction::Unhandled, "{}", entry.name),
            }
        }
        // Neighbours of table entries and an arbitrary index are not answered.
        for index in [0xe6, 0xe9, 0x3fb, 0x60e, 0x6b1, 0xdeadbeef] {
            for write in [false, true] {
                assert_eq!(
                    msr_action(MsrPolicy::Macos, index, MsrExitReason::Unknown, write),
                    MsrAction::Unhandled,
                    "{index:#x}"
                );
            }
        }
    }

    #[test]
    fn other_guests_and_other_exit_reasons_stay_strict() {
        for entry in MACOS_MSRS {
            for write in [false, true] {
                for reason in [
                    MsrExitReason::Unknown,
                    MsrExitReason::Inval,
                    MsrExitReason::Filter,
                ] {
                    assert_eq!(
                        msr_action(MsrPolicy::Fault, entry.index, reason, write),
                        MsrAction::Strict
                    );
                }
                for reason in [MsrExitReason::Inval, MsrExitReason::Filter] {
                    assert_eq!(
                        msr_action(MsrPolicy::Macos, entry.index, reason, write),
                        MsrAction::Strict
                    );
                }
            }
        }
    }

    #[test]
    fn document_lists_exactly_the_table() {
        let document = include_str!("../../../docs/macos-msrs.md");
        let rows: Vec<(u32, &str, &str)> = document
            .lines()
            .filter(|line| line.starts_with("| `0x"))
            .map(|line| {
                let cells: Vec<_> = line.split('|').map(str::trim).collect();
                let index = cells[1].trim_matches('`').trim_start_matches("0x");
                (
                    u32::from_str_radix(index, 16).unwrap(),
                    cells[2].trim_matches('`'),
                    cells[4],
                )
            })
            .collect();
        let table: Vec<_> = MACOS_MSRS
            .iter()
            .map(|entry| {
                let write = match entry.write {
                    MsrWrite::Drop => "dropped",
                    MsrWrite::Fault => "#GP",
                };
                (entry.index, entry.name, write)
            })
            .collect();
        assert_eq!(rows, table);
    }

    #[test]
    fn answers_are_zero_and_each_index_is_logged_once() {
        let unknown = MsrExitReason::Unknown;
        let mut seen = std::collections::BTreeSet::new();
        let (value, line) = answer(MsrPolicy::Macos, &mut seen, 0xe7, unknown, false);
        assert_eq!(value, Some(0));
        assert_eq!(
            line.as_deref(),
            Some("kvm: macOS MSR 0x000000e7 (IA32_MPERF) answered in user space")
        );
        assert_eq!(
            answer(MsrPolicy::Macos, &mut seen, 0xe7, unknown, true),
            (Some(0), None)
        );
        // A read-only counter: the read is answered, the write faults.
        assert_eq!(
            answer(MsrPolicy::Macos, &mut seen, 0x3f8, unknown, false).0,
            Some(0)
        );
        assert_eq!(
            answer(MsrPolicy::Macos, &mut seen, 0x3f8, unknown, true),
            (None, None)
        );
        let (value, line) = answer(MsrPolicy::Macos, &mut seen, 0xdeadbeef, unknown, true);
        assert_eq!(value, None);
        assert_eq!(
            line.as_deref(),
            Some("kvm: macOS guest faulted on unhandled MSR 0xdeadbeef (write)")
        );
        assert_eq!(
            answer(MsrPolicy::Macos, &mut seen, 0xdeadbeef, unknown, false),
            (None, None)
        );
        assert_eq!(
            answer(
                MsrPolicy::Macos,
                &mut seen,
                0xe8,
                MsrExitReason::Inval,
                false
            ),
            (None, None)
        );
        assert_eq!(
            seen,
            std::collections::BTreeSet::from([0xe7, 0x3f8, 0xdeadbeef])
        );
        // The set survives a snapshot: a restored machine logs nothing again.
        let saved = serde_json::to_vec(&seen).unwrap();
        let mut restored: std::collections::BTreeSet<u32> = serde_json::from_slice(&saved).unwrap();
        assert_eq!(
            answer(MsrPolicy::Macos, &mut restored, 0xe7, unknown, false),
            (Some(0), None)
        );
        let mut strict = std::collections::BTreeSet::new();
        assert_eq!(
            answer(MsrPolicy::Fault, &mut strict, 0xe7, unknown, false),
            (None, None)
        );
        assert!(strict.is_empty());
    }

    /// Real mode: `rdmsr` of ECX into [0x9000], `wrmsr` of 0x78563412, `rdmsr`
    /// again into [0x9004], then a byte to port 0xe9.
    fn msr_guest(index: u32) -> Vec<u8> {
        let mut code = vec![0x66, 0xb9];
        code.extend(index.to_le_bytes());
        code.extend([
            0x0f, 0x32, 0x66, 0xa3, 0, 0x90, 0x66, 0xb8, 0x12, 0x34, 0x56, 0x78, 0x0f, 0x30, 0x0f,
            0x32, 0x66, 0xa3, 4, 0x90, 0xe6, 0xe9,
        ]);
        code
    }

    fn kvm_msr_exits_usable(test: &str) -> bool {
        if std::fs::read_to_string("/sys/module/kvm/parameters/ignore_msrs")
            .is_ok_and(|value| value.trim() == "Y")
        {
            eprintln!(
                "skipping {test}: host ignore_msrs=Y suppresses unknown-MSR exits; rerun with ignore_msrs=N"
            );
            return false;
        }
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping {test}: /dev/kvm unavailable");
            return false;
        }
        true
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn kvm_macos_msr_reads_zero_drops_the_write_and_completes() {
        if !kvm_msr_exits_usable("macOS MSR exits") {
            return;
        }
        let vm = KvmVm::new().unwrap();
        configure_msrs(&vm, MsrPolicy::Macos).unwrap();
        let _permit = vmlab_hv_tests::process::VmPermit::acquire().unwrap();
        let ram =
            crate::memory::GuestRam::anonymous(crate::layout::MemLayout::new(16).unwrap()).unwrap();
        crate::memory::register_memslots(&vm.vm, &ram).unwrap();
        // MSR_CORE_PERF_LIMIT_REASONS: no KVM model on Intel or AMD hosts,
        // and writable in the table.
        ram.mem
            .write_slice(&msr_guest(0x64f), GuestAddress(0x8000))
            .unwrap();
        ram.mem
            .write_slice(&[0xff; 8], GuestAddress(0x9000))
            .unwrap();
        let mut cpu = vm.create_vcpu(0).unwrap();
        crate::loader::init_real_mode(&mut cpu, 0x8000).unwrap();
        let mut seen = Default::default();
        for write in [false, true, false] {
            match cpu.run().unwrap() {
                kvm_ioctls::VcpuExit::X86Rdmsr(exit) if !write => {
                    assert_eq!(exit.index, 0x64f);
                    let (value, _) =
                        answer(MsrPolicy::Macos, &mut seen, exit.index, exit.reason, false);
                    *exit.error = 0;
                    *exit.data = value.unwrap();
                }
                kvm_ioctls::VcpuExit::X86Wrmsr(exit) if write => {
                    assert_eq!(exit.data, 0x78563412);
                    let (value, _) =
                        answer(MsrPolicy::Macos, &mut seen, exit.index, exit.reason, true);
                    assert!(value.is_some());
                    *exit.error = 0;
                }
                other => panic!("unexpected MSR test exit: {other:?}"),
            }
        }
        assert!(matches!(
            cpu.run().unwrap(),
            kvm_ioctls::VcpuExit::IoOut(0xe9, _)
        ));
        let mut bytes = [0xff; 8];
        ram.mem
            .read_slice(&mut bytes, GuestAddress(0x9000))
            .unwrap();
        assert_eq!(bytes, [0; 8]);
        assert_eq!(seen.len(), 1);
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn kvm_macos_policy_faults_an_msr_outside_the_table() {
        if !kvm_msr_exits_usable("macOS strict MSR") {
            return;
        }
        let _permit = vmlab_hv_tests::process::VmPermit::acquire().unwrap();
        let vm = KvmVm::new().unwrap();
        configure_msrs(&vm, MsrPolicy::Macos).unwrap();
        let ram =
            crate::memory::GuestRam::anonymous(crate::layout::MemLayout::new(16).unwrap()).unwrap();
        crate::memory::register_memslots(&vm.vm, &ram).unwrap();
        ram.mem
            .write_slice(&msr_guest(0xdeadbeef), GuestAddress(0x8000))
            .unwrap();
        // The #GP vector (13) points at 0x8100, which writes 0x42 to port 0xe9.
        ram.mem
            .write_slice(&[0, 0x81, 0, 0], GuestAddress(13 * 4))
            .unwrap();
        ram.mem
            .write_slice(&[0xb0, 0x42, 0xe6, 0xe9], GuestAddress(0x8100))
            .unwrap();
        let mut cpu = vm.create_vcpu(0).unwrap();
        crate::loader::init_real_mode(&mut cpu, 0x8000).unwrap();
        let mut seen = Default::default();
        match cpu.run().unwrap() {
            kvm_ioctls::VcpuExit::X86Rdmsr(exit) => {
                assert_eq!(exit.index, 0xdeadbeef);
                let (value, line) =
                    answer(MsrPolicy::Macos, &mut seen, exit.index, exit.reason, false);
                assert_eq!(value, None);
                assert!(line.unwrap().contains("unhandled MSR 0xdeadbeef (read)"));
                *exit.error = 1;
            }
            other => panic!("unexpected MSR test exit: {other:?}"),
        }
        match cpu.run().unwrap() {
            kvm_ioctls::VcpuExit::IoOut(0xe9, bytes) => assert_eq!(bytes, [0x42]),
            other => panic!("unhandled MSR did not enter the guest #GP handler: {other:?}"),
        }
    }

    #[test]
    fn timing_leaves_describe_the_pin_without_adding_leaves() {
        let mut entries = [
            kvm_cpuid_entry2 {
                function: 0x15,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x16,
                ecx: 100,
                ..Default::default()
            },
        ];
        timing_leaves(&mut entries, 4_291_956);
        assert_eq!(
            u64::from(entries[0].ecx) * u64::from(entries[0].ebx) / u64::from(entries[0].eax),
            4_291_956_000
        );
        assert_eq!(
            (entries[1].eax, entries[1].ebx, entries[1].ecx),
            (4291, 4291, 100)
        );
        timing_leaves(&mut [], 4_291_956);
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn pinned_tsc_preserves_hyperv_leaves_on_every_vcpu() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping pinned Hyper-V TSC: /dev/kvm unavailable");
            return;
        }
        let _permit = vmlab_hv_tests::process::VmPermit::acquire().unwrap();
        let vm = KvmVm::new().unwrap();
        let khz = crate::host::probe().unwrap().tsc_khz.unwrap() / 2;
        let desc: MachineDescription = serde_json::from_value(serde_json::json!({
            "name":"tsc", "machine":"q35", "arch":"x86_64", "accel":"kvm", "vcpus":2, "memory_mib":16,
            "boot":{"kind":"kernel","kernel":"unused","cmdline":""},
            "disks":[],"nics":[],"ports":[],"serial_log":"unused",
            "invtsc":true,"tsc_khz":khz
        }))
        .unwrap();
        for id in 0..2 {
            let cpu = vm
                .create_vcpu_with_model(id, 2, vmlab_hv_proto::machine::CpuPolicy::Host, None)
                .unwrap();
            crate::hyperv::configure_vcpu(&vm.kvm, &cpu, crate::hyperv::WINDOWS_FEATURES).unwrap();
            let before = cpu.get_cpuid2(KVM_MAX_CPUID_ENTRIES).unwrap();
            configure_tsc(&cpu, &desc, false).unwrap();
            assert_eq!(cpu.get_tsc_khz().unwrap(), khz);
            let after = cpu.get_cpuid2(KVM_MAX_CPUID_ENTRIES).unwrap();
            for leaf in before
                .as_slice()
                .iter()
                .filter(|e| (0x40000000..0x50000000).contains(&e.function))
            {
                assert_eq!(
                    after
                        .as_slice()
                        .iter()
                        .find(|e| e.function == leaf.function && e.index == leaf.index),
                    Some(leaf),
                    "pin changed Hyper-V leaf {:#x} on vCPU {id}",
                    leaf.function
                );
            }
        }
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn frequency_leaf_uses_the_pinned_kvm_frequency() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping TSC: /dev/kvm unavailable");
            return;
        }
        let vm = KvmVm::new().unwrap();
        let cpu = vm
            .create_vcpu_with_model(
                0,
                4,
                vmlab_hv_proto::machine::CpuPolicy::Host,
                Some("Skylake-Client-v4"),
            )
            .unwrap();
        let khz = cpu.get_tsc_khz().unwrap() / 2;
        let desc: MachineDescription = serde_json::from_value(serde_json::json!({
            "name":"tsc", "arch":"x86_64", "accel":"kvm", "vcpus":4, "memory_mib":16,
            "boot":{"kind":"kernel","kernel":"unused","cmdline":""},
            "disks":[],"nics":[],"ports":[],"serial_log":"unused",
            "cpu_model":"Skylake-Client-v4","invtsc":true,"vmware_cpuid_freq":true,"tsc_khz":khz
        }))
        .unwrap();
        configure_tsc(&cpu, &desc, true).unwrap();
        let cpuid = cpu.get_cpuid2(KVM_MAX_CPUID_ENTRIES).unwrap();
        let leaf = cpuid
            .as_slice()
            .iter()
            .find(|entry| entry.function == 0x40000010)
            .unwrap();
        assert_eq!((leaf.eax, leaf.ebx), (khz, 1_000_000));
        assert_ne!(
            cpuid
                .as_slice()
                .iter()
                .find(|entry| entry.function == 0x80000007)
                .unwrap()
                .edx
                & 256,
            0
        );
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn kvm_fault_policy_delivers_unknown_msr_gp_to_the_guest() {
        if !std::path::Path::new("/dev/kvm").exists()
            || std::fs::read_to_string("/sys/module/kvm/parameters/ignore_msrs")
                .is_ok_and(|value| value.trim() != "N")
        {
            eprintln!("skipping fault-policy guest: requires /dev/kvm and ignore_msrs=N");
            return;
        }
        let _permit = vmlab_hv_tests::process::VmPermit::acquire().unwrap();
        let vm = KvmVm::new().unwrap();
        configure_msrs(&vm, MsrPolicy::Fault).unwrap();
        let ram =
            crate::memory::GuestRam::anonymous(crate::layout::MemLayout::new(16).unwrap()).unwrap();
        crate::memory::register_memslots(&vm.vm, &ram).unwrap();
        ram.mem
            .write_slice(
                &[0x66, 0xb9, 0xef, 0xbe, 0xad, 0xde, 0x0f, 0x32, 0xf4],
                GuestAddress(0x8000),
            )
            .unwrap();
        ram.mem
            .write_slice(&[0, 0x81, 0, 0], GuestAddress(13 * 4))
            .unwrap();
        ram.mem
            .write_slice(&[0xb0, 0x42, 0xe6, 0xe9], GuestAddress(0x8100))
            .unwrap();
        let mut cpu = vm.create_vcpu(0).unwrap();
        crate::loader::init_real_mode(&mut cpu, 0x8000).unwrap();
        match cpu.run().unwrap() {
            kvm_ioctls::VcpuExit::IoOut(0xe9, bytes) => assert_eq!(bytes, [0x42]),
            other => panic!("unknown MSR did not enter the guest #GP handler: {other:?}"),
        }
    }
}
