//! KVM's binary statistics (Linux KVM API section 4.133, `KVM_GET_STATS_FD`):
//! the kernel's own exit, halt and interrupt counters of a VM and of each
//! vCPU. Diagnostic reports carry them, so the exit rate of an idle guest can
//! be measured without debugfs or perf.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::FileExt;

// _IO(KVMIO, 0xce)
const KVM_GET_STATS_FD: libc::c_ulong = 0xaece;
const TYPE_MASK: u32 = 0xf;
const TYPE_CUMULATIVE: u32 = 0;
const TYPE_INSTANT: u32 = 1;
const TYPE_PEAK: u32 = 2;
const HEADER_LEN: usize = 24;
const DESCRIPTOR_LEN: usize = 16;

struct Counter {
    name: String,
    offset: u64,
}

/// One statistics file and the scalar counters its descriptors name.
/// Histograms are left out.
pub(crate) struct KvmStats {
    file: File,
    data_offset: u64,
    counters: Vec<Counter>,
}

impl KvmStats {
    /// Open the statistics of a VM or vCPU descriptor. A vCPU's call takes
    /// the vCPU mutex in the kernel, so it is made before the vCPU first
    /// runs. Reads of the file afterwards take no lock.
    pub(crate) fn open(fd: RawFd) -> std::io::Result<Self> {
        // SAFETY: fd is a live KVM VM or vCPU descriptor. The ioctl takes no
        // argument and returns a new descriptor that this function owns.
        let stats = unsafe { libc::ioctl(fd, KVM_GET_STATS_FD, 0) };
        if stats < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the kernel returned a fresh descriptor owned by nothing else.
        let file = unsafe { File::from_raw_fd(stats) };
        Self::parse(file)
    }

    fn parse(file: File) -> std::io::Result<Self> {
        let invalid = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what);
        let mut header = [0u8; HEADER_LEN];
        file.read_exact_at(&mut header, 0)?;
        let word = |i: usize| u32::from_le_bytes(header[i * 4..i * 4 + 4].try_into().unwrap());
        let (name_size, count, desc_offset, data_offset) =
            (word(1) as usize, word(2) as usize, word(4) as u64, word(5) as u64);
        if count > 4096 || name_size > 4096 {
            return Err(invalid("KVM statistics header out of range"));
        }
        let stride = DESCRIPTOR_LEN + name_size;
        let mut descriptors = vec![0u8; stride * count];
        file.read_exact_at(&mut descriptors, desc_offset)?;
        let mut counters = Vec::new();
        for raw in descriptors.chunks_exact(stride) {
            let flags = u32::from_le_bytes(raw[0..4].try_into().unwrap());
            let size = u16::from_le_bytes(raw[6..8].try_into().unwrap());
            let offset = u32::from_le_bytes(raw[8..12].try_into().unwrap());
            let name = &raw[DESCRIPTOR_LEN..];
            let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
            if size == 1
                && matches!(flags & TYPE_MASK, TYPE_CUMULATIVE | TYPE_INSTANT | TYPE_PEAK)
            {
                counters.push(Counter {
                    name: String::from_utf8_lossy(name).into_owned(),
                    offset: offset as u64,
                });
            }
        }
        Ok(Self {
            file,
            data_offset,
            counters,
        })
    }

    /// The current value of every scalar counter, by name.
    pub(crate) fn read(&self) -> std::io::Result<BTreeMap<String, u64>> {
        let end = self.counters.iter().map(|c| c.offset + 8).max().unwrap_or(0);
        let mut data = vec![0u8; end as usize];
        self.file.read_exact_at(&mut data, self.data_offset)?;
        Ok(self
            .counters
            .iter()
            .map(|c| {
                let at = c.offset as usize;
                (
                    c.name.clone(),
                    u64::from_le_bytes(data[at..at + 8].try_into().unwrap()),
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn vm_and_vcpu_counters_are_read_and_a_vcpu_exit_counts() {
        let Ok(kvm) = kvm_ioctls::Kvm::new() else {
            eprintln!("skipping: no /dev/kvm");
            return;
        };
        let vm = kvm.create_vm().unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();
        let (vm_stats, vcpu_stats) = match (
            KvmStats::open(vm.as_raw_fd()),
            KvmStats::open(vcpu.as_raw_fd()),
        ) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) if e.raw_os_error() == Some(libc::ENOTTY) => {
                eprintln!("skipping: KVM_GET_STATS_FD unsupported");
                return;
            }
            (Err(e), _) | (_, Err(e)) => panic!("{e}"),
        };
        assert!(vm_stats.read().unwrap().contains_key("remote_tlb_flush"));
        let before = vcpu_stats.read().unwrap();
        assert!(before.contains_key("exits"), "{before:?}");
        assert!(before.contains_key("halt_exits"), "{before:?}");
        assert!(!before.keys().any(|k| k == "halt_poll_success_hist"));
        // A vCPU in real mode at 0x1000 runs one OUT and exits to here.
        const SIZE: usize = 0x10000;
        // SAFETY: a fresh anonymous mapping, unmapped at the end of the test
        // after the VM that maps it is gone.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(host, libc::MAP_FAILED);
        // SAFETY: 0x1003 lies inside the mapping made above.
        unsafe {
            std::ptr::copy_nonoverlapping([0xe6u8, 0x80, 0xf4].as_ptr(), host.cast::<u8>().add(0x1000), 3);
            vm.set_user_memory_region(kvm_bindings::kvm_userspace_memory_region {
                slot: 0,
                guest_phys_addr: 0,
                memory_size: SIZE as u64,
                userspace_addr: host as u64,
                flags: 0,
            })
            .unwrap();
        }
        let mut sregs = vcpu.get_sregs().unwrap();
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        vcpu.set_sregs(&sregs).unwrap();
        let mut regs = vcpu.get_regs().unwrap();
        regs.rip = 0x1000;
        regs.rflags = 2;
        vcpu.set_regs(&regs).unwrap();
        let mut vcpu = vcpu;
        assert!(matches!(
            vcpu.run().unwrap(),
            kvm_ioctls::VcpuExit::IoOut(0x80, _)
        ));
        let after = vcpu_stats.read().unwrap();
        assert!(after["io_exits"] > before["io_exits"], "{after:?}");
        assert!(after["exits"] > before["exits"], "{after:?}");
        drop((vcpu, vm));
        // SAFETY: the VM and vCPU that mapped the region are closed.
        unsafe { libc::munmap(host, SIZE) };
    }
}
