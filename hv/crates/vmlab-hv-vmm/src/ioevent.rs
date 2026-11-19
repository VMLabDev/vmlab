//! KVM ioeventfd registrations for PCI notify events.
//!
//! A registered guest write never reaches user space: KVM adds one count to
//! the event and resumes the guest. The registration matches a write of any
//! width and value, which also lets KVM skip decoding the instruction.

use kvm_ioctls::{IoEventAddress, NoDatamatch, VmFd};
use std::io;
use std::sync::Arc;
use vmm_sys_util::eventfd::EventFd;

use crate::pci::NotifyRegistrar;

pub(crate) struct KvmNotify(pub(crate) Arc<VmFd>);

impl NotifyRegistrar for KvmNotify {
    fn register(&mut self, address: u64, event: &EventFd) -> io::Result<()> {
        self.0
            .register_ioevent(event, &IoEventAddress::Mmio(address), NoDatamatch)
            .map_err(io::Error::other)
    }

    fn unregister(&mut self, address: u64, event: &EventFd) -> io::Result<()> {
        self.0
            .unregister_ioevent(event, &IoEventAddress::Mmio(address), NoDatamatch)
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_ioctls::VcpuExit;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend};

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn registered_write_counts_on_the_event_and_never_exits_as_mmio() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("SKIP notify event KVM registration: /dev/kvm missing");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let memory =
            vmlab_hv_devices::GuestMemory::from_ranges(&[(GuestAddress(0), 0x2000)]).unwrap();
        let registration = kvm_bindings::kvm_userspace_memory_region {
            slot: 0,
            guest_phys_addr: 0,
            memory_size: 0x2000,
            userspace_addr: memory.get_host_address(GuestAddress(0)).unwrap() as u64,
            flags: 0,
        };
        // SAFETY: this test retains memory until after the vCPU is dropped.
        unsafe { kvm.vm.set_user_memory_region(registration) }.unwrap();
        let mut cpu = kvm.vm.create_vcpu(0).unwrap();
        let mut segments = cpu.get_sregs().unwrap();
        segments.cs.base = 0;
        segments.cs.selector = 0;
        segments.ds.limit = u32::MAX;
        segments.ds.g = 1;
        cpu.set_sregs(&segments).unwrap();

        // A virtio driver's kick: a 16-bit write of the queue index to the
        // queue's notify address, then a port write that ends the run.
        const NOTIFY: u64 = 0x9000_3004;
        let mut code = vec![0x67, 0xc7, 0x05];
        code.extend((NOTIFY as u32).to_le_bytes());
        code.extend(1u16.to_le_bytes());
        code.extend([0xe6, 0xf4]);
        memory.write_slice(&code, GuestAddress(0x1000)).unwrap();
        let run = |cpu: &mut kvm_ioctls::VcpuFd| {
            cpu.set_regs(&kvm_bindings::kvm_regs {
                rip: 0x1000,
                rflags: 2,
                ..Default::default()
            })
            .unwrap();
            match cpu.run().unwrap() {
                VcpuExit::IoOut(0xf4, _) => None,
                VcpuExit::MmioWrite(address, data) => Some((address, data.to_vec())),
                exit => panic!("unexpected exit: {exit:?}"),
            }
        };

        let event = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let mut notify = KvmNotify(kvm.vm.clone());
        notify.register(NOTIFY, &event).unwrap();
        assert_eq!(run(&mut cpu), None);
        assert_eq!(run(&mut cpu), None);
        assert_eq!(event.read().unwrap(), 2);

        notify.unregister(NOTIFY, &event).unwrap();
        assert_eq!(run(&mut cpu), Some((NOTIFY, vec![1, 0])));
        assert_eq!(event.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }
}
