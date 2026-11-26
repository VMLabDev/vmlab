//! The KVM backend of a vCPU: `KVM_RUN`, and the exits that only an x86 KVM
//! guest has. Port I/O, the MSR exits, SMIs, Hyper-V hypercalls and the
//! debug exit are served here. MMIO, halt, shutdown and reset go to the
//! loop as [`Exit`]s.

use std::sync::Arc;

use kvm_ioctls::{VcpuExit, VcpuFd};
use parking_lot::Mutex;

use super::{Exit, Kick, VcpuBackend, VcpuHost, VcpuState};
use crate::vm::{ExitReason, VmShared};

fn io_dispatch_width(port: u16, width: usize, length: usize) -> usize {
    // IDE data ports are byte FIFOs, including across sector boundaries.
    // Keep the complete REP payload so each dword does not poll every UART.
    if matches!(port, 0x1f0 | 0x170) {
        length.max(width)
    } else {
        width
    }
}

/// One KVM vCPU, made on its own thread. The fd sits in a mutex shared with
/// the snapshot path: this backend holds it across `KVM_RUN` and the exits
/// it serves itself, so a snapshot can take it once the thread parks.
pub struct KvmVcpu<'a> {
    id: u8,
    vcpu: Arc<Mutex<VcpuFd>>,
    shared: &'a VmShared,
    sync_supported: bool,
    sync_regs: bool,
    recorder: Option<crate::diagnostics::flight::ExitWriter<'a>>,
    /// I/O, MMIO and Hyper-V hypercalls finish on the next KVM_RUN. Their
    /// pending state is not included in GET_REGS or migration state (KVM
    /// API section 5).
    pending_completion: bool,
}

impl<'a> KvmVcpu<'a> {
    pub fn new(id: u8, vcpu: Arc<Mutex<VcpuFd>>, shared: &'a VmShared) -> Self {
        let sync_supported = shared
            .kvm
            .kvm
            .check_extension_int(kvm_ioctls::Cap::SyncRegs)
            & kvm_bindings::KVM_SYNC_X86_REGS as i32
            != 0;
        Self {
            id,
            vcpu,
            shared,
            sync_supported,
            sync_regs: false,
            recorder: None,
            pending_completion: false,
        }
    }
}

impl VcpuBackend for KvmVcpu<'_> {
    /// One `KVM_RUN`. With `running` false it completes the access that the
    /// last exit left open, without executing another instruction. A split
    /// MMIO access can exit again, so the loop keeps calling until done.
    fn run(&mut self, running: bool) -> Exit {
        let id = self.id;
        let shared = self.shared;
        // The exit borrows the fd's kvm_run area, so the guard lives to the
        // end of this call. Nothing else takes this mutex while the thread
        // runs: the snapshot path waits for the thread to park first.
        let mut guard = self.vcpu.lock();
        if running && !self.pending_completion && shared.has_pending_smi(id) {
            if let Err(e) = crate::smm::inject_smi(&guard) {
                return Exit::Fail(crate::vm::ExitReason::Error(e.to_string()));
            }
            shared.take_pending_smi(id);
            shared.wait_smi_delivery();
        }
        guard.set_kvm_immediate_exit(u8::from(!running));
        let mode = crate::diagnostics::report::exit_recorder_mode();
        if mode != crate::diagnostics::report::RECORD_OFF && self.recorder.is_none() {
            self.recorder = Some(
                shared.exit_rings[id as usize]
                    .get_or_init(|| Arc::new(crate::diagnostics::flight::ExitRing::default()))
                    .writer()
                    .expect("one exit-history writer per vCPU"),
            );
        }
        sync_exit_rip(
            guard.get_kvm_run(),
            &mut self.sync_regs,
            self.sync_supported && mode == crate::diagnostics::report::RECORD_RIP,
        );
        let run_ptr = guard.get_kvm_run() as *const kvm_bindings::kvm_run;
        let permit = shared.enter_kvm();
        let result = guard.run();
        drop(permit);
        // Metadata is disjoint from the exit's borrowed data slice. Only the
        // RIP mode requests a register copy; the default skips recording.
        if mode != crate::diagnostics::report::RECORD_OFF {
            // SAFETY: run_ptr is the live KVM mapping; the exit reason selects the union.
            self.recorder.as_mut().unwrap().record(unsafe {
                exit_record(
                    run_ptr,
                    result.as_ref().err().map(|e| e.errno()),
                    self.sync_regs,
                )
            });
        }
        // Deliver buffered VGA writes before reads, register changes, or parking.
        shared.drain_coalesced_vga();
        self.pending_completion = matches!(
            &result,
            Ok(VcpuExit::IoIn(..)
                | VcpuExit::IoOut(..)
                | VcpuExit::MmioRead(..)
                | VcpuExit::MmioWrite(..)
                | VcpuExit::X86Rdmsr(..)
                | VcpuExit::X86Wrmsr(..))
        );
        let width = if matches!(&result, Ok(VcpuExit::IoIn(..) | VcpuExit::IoOut(..))) {
            // SAFETY: KVM_EXIT_IO selects this union member. run_ptr names
            // the stable vCPU mmap; size is metadata disjoint from the I/O
            // payload slice borrowed by VcpuExit.
            unsafe { (*run_ptr).__bindgen_anon_1.io.size as usize }
        } else {
            1
        };
        if !matches!(width, 1 | 2 | 4) {
            return Exit::Fail(crate::vm::ExitReason::Error(format!(
                "invalid KVM I/O width {width}"
            )));
        }
        match result {
            Ok(VcpuExit::Debug(exit)) => {
                crate::debug::stopped(shared, id, exit);
            }
            Ok(VcpuExit::X86Rdmsr(exit)) => {
                let value = crate::macos::user_msr(shared, exit.index, exit.reason, false);
                *exit.error = u8::from(value.is_none());
                *exit.data = value.unwrap_or(0);
            }
            Ok(VcpuExit::X86Wrmsr(exit)) => {
                let done = crate::macos::user_msr(shared, exit.index, exit.reason, true);
                *exit.error = u8::from(done.is_none());
            }
            Ok(VcpuExit::IoIn(port, data)) => {
                for value in data.chunks_mut(io_dispatch_width(port, width, data.len())) {
                    shared.io_read(port, value);
                }
            }
            Ok(VcpuExit::IoOut(port, data)) => {
                for value in data.chunks(io_dispatch_width(port, width, data.len())) {
                    shared.io_write(port, value);
                }
                if port == 0xb2 && shared.take_smi() {
                    // Complete the deferred OUT before SMM saves RIP. Without
                    // this completion, RSM can return to the same APMC write.
                    guard.set_kvm_immediate_exit(1);
                    let mode = crate::diagnostics::report::exit_recorder_mode();
                    if mode != crate::diagnostics::report::RECORD_OFF && self.recorder.is_none() {
                        self.recorder = Some(
                            shared.exit_rings[id as usize]
                                .get_or_init(|| {
                                    Arc::new(crate::diagnostics::flight::ExitRing::default())
                                })
                                .writer()
                                .expect("one exit-history writer per vCPU"),
                        );
                    }
                    sync_exit_rip(
                        guard.get_kvm_run(),
                        &mut self.sync_regs,
                        self.sync_supported && mode == crate::diagnostics::report::RECORD_RIP,
                    );
                    let permit = shared.enter_kvm();
                    let completion = guard.run();
                    drop(permit);
                    if mode != crate::diagnostics::report::RECORD_OFF {
                        // SAFETY: same stable mapping and exit-discriminated metadata.
                        self.recorder.as_mut().unwrap().record(unsafe {
                            exit_record(
                                run_ptr,
                                completion.as_ref().err().map(|e| e.errno()),
                                self.sync_regs,
                            )
                        });
                    }
                    if !matches!(completion, Err(ref e) if e.errno() == libc::EINTR) {
                        return Exit::Fail(crate::vm::ExitReason::Error(format!(
                            "completing APMC output before SMI: {completion:?}"
                        )));
                    }
                    guard.set_kvm_immediate_exit(0);
                    self.pending_completion = false;
                    if shared.dispatch_smi(id) {
                        if let Err(e) = guard.smi() {
                            return Exit::Fail(crate::vm::ExitReason::runtime(
                                format!("vcpu {id}: KVM_SMI"),
                                &e,
                            ));
                        }
                        shared.take_pending_smi(id);
                        shared.wait_smi_delivery();
                    }
                }
            }
            Ok(VcpuExit::MmioRead(addr, data)) => {
                return Exit::MmioRead {
                    address: addr,
                    len: data.len(),
                };
            }
            Ok(VcpuExit::MmioWrite(addr, data)) => {
                if shared.flash_address(addr) {
                    let bytes = data.to_vec();
                    match crate::smm::in_smm(&guard) {
                        Ok(in_smm) => shared.write_flash(addr, &bytes, in_smm),
                        Err(error) => {
                            return Exit::Fail(crate::vm::ExitReason::Error(error.to_string()));
                        }
                    }
                } else {
                    let mut bytes = [0; 8];
                    bytes[..data.len()].copy_from_slice(data);
                    return Exit::MmioWrite {
                        address: addr,
                        data: bytes,
                        len: data.len(),
                    };
                }
            }
            Ok(VcpuExit::Hlt) => {
                // With the in-kernel irqchip the vCPU wakes on the next
                // interrupt; re-enter and let the state check handle pauses.
                return Exit::Halt;
            }
            Ok(VcpuExit::IrqWindowOpen) => {}
            Ok(VcpuExit::Hyperv) => {
                // SAFETY: KVM_EXIT_HYPERV selects this union member.
                self.pending_completion = unsafe {
                    guard.get_kvm_run().__bindgen_anon_1.hyperv.type_
                        == kvm_bindings::KVM_EXIT_HYPERV_HCALL
                };
                let Some(ram) = shared.ram() else {
                    return Exit::Fail(crate::vm::ExitReason::Error(
                        "Hyper-V exit without guest RAM".into(),
                    ));
                };
                if let Err(e) = crate::hyperv::handle_exit(guard.get_kvm_run(), &ram.device_mem) {
                    return Exit::Fail(crate::vm::ExitReason::Error(e.to_string()));
                }
            }
            Ok(VcpuExit::SystemEvent(kvm_bindings::KVM_SYSTEM_EVENT_CRASH, _)) => {
                // Only exposed with VMLAB_HV_GUEST_CRASH_MSRS: report the
                // guest's crash parameters and let it continue its halt path.
                let mut msrs = kvm_bindings::Msrs::from_entries(
                    &(0x4000_0100..=0x4000_0104)
                        .map(|index| kvm_bindings::kvm_msr_entry {
                            index,
                            ..Default::default()
                        })
                        .collect::<Vec<_>>(),
                )
                .expect("five MSRs");
                let _ = guard.get_msrs(&mut msrs);
                let values: Vec<_> = msrs.as_slice().iter().map(|e| e.data).collect();
                log::error!("vcpu {id}: guest crash {values:#x?}");
            }
            Ok(VcpuExit::SystemEvent(kvm_bindings::KVM_SYSTEM_EVENT_RESET, _)) => {
                return Exit::Reset;
            }
            Ok(VcpuExit::SystemEvent(kvm_bindings::KVM_SYSTEM_EVENT_SHUTDOWN, _)) => {
                return Exit::Shutdown;
            }
            Ok(VcpuExit::Shutdown) => {
                // Triple fault: the guest reset. `reboot=t` and `panic=-1`
                // both land here.
                return Exit::Reset;
            }
            Ok(VcpuExit::InternalError) => {
                let run = guard.get_kvm_run();
                // SAFETY: KVM_EXIT_INTERNAL_ERROR selects this union member.
                let error = unsafe { run.__bindgen_anon_1.internal };
                return Exit::Fail(crate::vm::ExitReason::Error(format!(
                    "vcpu {id}: KVM internal error {}: {:x?}",
                    error.suberror,
                    &error.data[..(error.ndata as usize).min(error.data.len())]
                )));
            }
            Ok(exit) => {
                return Exit::Fail(crate::vm::ExitReason::Error(format!(
                    "vcpu {id}: unhandled KVM exit {exit:?}"
                )));
            }
            Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                // EINTR: kicked, re-check the machine state. EAGAIN: the
                // vCPU is not runnable yet — an application processor parked
                // before the kernel's INIT-SIPI-SIPI wakes it — retry.
            }
            Err(e) => {
                return Exit::Fail(crate::vm::ExitReason::runtime(
                    format!("vcpu {id}: KVM_RUN"),
                    &e,
                ));
            }
        }
        Exit::Again
    }

    fn pending(&self) -> bool {
        self.pending_completion
    }

    fn finish_mmio_read(&mut self, data: &[u8]) {
        let mut guard = self.vcpu.lock();
        let run = guard.get_kvm_run();
        // SAFETY: the last exit was KVM_EXIT_MMIO, which selects this union
        // member. `data` is as long as the access that the exit named, at
        // most the 8 bytes of the member.
        unsafe { run.__bindgen_anon_1.mmio.data[..data.len()].copy_from_slice(data) };
    }

    fn kick(&self) -> Kick {
        // SAFETY: pthread_self(3) reads the calling thread's own id.
        Kick::Signal(unsafe { libc::pthread_self() })
    }

    fn state(&mut self) -> Result<VcpuState, String> {
        let guard = self.vcpu.lock();
        Ok(VcpuState::X86 {
            regs: guard.get_regs().map_err(|e| format!("KVM_GET_REGS: {e}"))?,
            sregs: Box::new(
                guard
                    .get_sregs()
                    .map_err(|e| format!("KVM_GET_SREGS: {e}"))?,
            ),
        })
    }

    fn set_state(&mut self, state: &VcpuState) -> Result<(), String> {
        let VcpuState::X86 { regs, sregs } = state else {
            return Err("a KVM vCPU takes the state of an x86 CPU".into());
        };
        let guard = self.vcpu.lock();
        guard
            .set_sregs(sregs)
            .map_err(|e| format!("KVM_SET_SREGS: {e}"))?;
        guard
            .set_regs(regs)
            .map_err(|e| format!("KVM_SET_REGS: {e}"))
    }
}

impl VcpuHost for VmShared {
    fn should_run(&self, id: u8) -> bool {
        self.should_run_vcpu(id)
    }

    fn exited(&self) -> bool {
        VmShared::exited(self)
    }

    fn park(&self, id: u8) {
        VmShared::park(self, id);
    }

    fn register(&self, id: u8, kick: Kick) {
        self.register_kick(id, kick);
    }

    fn unregister(&self, id: u8, kick: &Kick) {
        self.unregister_kick(id, kick);
    }

    /// An address that no device decodes reads as the bus leaves it and
    /// ignores a write: these machines have no bus error.
    fn mmio_read(&self, address: u64, data: &mut [u8]) -> bool {
        VmShared::mmio_read(self, address, data);
        true
    }

    fn mmio_write(&self, address: u64, data: &[u8]) -> bool {
        VmShared::mmio_write(self, address, data);
        true
    }

    fn request_exit(&self, reason: ExitReason) {
        VmShared::request_exit(self, reason);
    }

    fn request_reset(&self) {
        VmShared::request_reset(self);
    }

    fn check(&self) -> Option<ExitReason> {
        self.vga
            .lock()
            .as_ref()
            .and_then(|vga| vga.unsupported_fifo())
            .map(ExitReason::Error)
    }
}

#[inline]
fn sync_exit_rip(run: &mut kvm_bindings::kvm_run, active: &mut bool, requested: bool) {
    if *active != requested {
        let mask = u64::from(kvm_bindings::KVM_SYNC_X86_REGS);
        if requested {
            run.kvm_valid_regs |= mask;
        } else {
            run.kvm_valid_regs &= !mask;
        }
        *active = requested;
    }
}

unsafe fn exit_record(
    run: *const kvm_bindings::kvm_run,
    errno: Option<i32>,
    sync_regs: bool,
) -> crate::diagnostics::flight::ExitMetadata {
    use crate::diagnostics::flight::ExitMetadata;
    let mut record = ExitMetadata {
        // SAFETY: only metadata is read; exit payload slices may be mutably borrowed.
        reason: unsafe { (*run).exit_reason },
        errno,
        ..Default::default()
    };
    // Interrupted ioctls do not publish a new KVM exit or register sample.
    if errno.is_some() {
        record.reason = u32::MAX;
        return record;
    }
    if sync_regs {
        // SAFETY: KVM_SYNC_X86_REGS was requested on a supporting kernel.
        record.rip = Some(unsafe { (*run).s.regs.regs.rip });
    }
    match record.reason {
        kvm_bindings::KVM_EXIT_IO => {
            // SAFETY: KVM_EXIT_IO selects the io union member.
            let io = unsafe { (*run).__bindgen_anon_1.io };
            record.address = Some(io.port as u64);
            record.size = io.size as u32;
            record.write = Some(io.direction == kvm_bindings::KVM_EXIT_IO_OUT as u8);
        }
        kvm_bindings::KVM_EXIT_MMIO => {
            // SAFETY: KVM_EXIT_MMIO selects the mmio union member.
            // SAFETY: read only metadata, since VcpuExit may mutably borrow mmio.data.
            record.address = Some(unsafe { (*run).__bindgen_anon_1.mmio.phys_addr });
            // SAFETY: KVM_EXIT_MMIO selects metadata outside the payload.
            record.size = unsafe { (*run).__bindgen_anon_1.mmio.len };
            // SAFETY: KVM_EXIT_MMIO selects metadata outside the payload.
            record.write = Some(unsafe { (*run).__bindgen_anon_1.mmio.is_write } != 0);
        }
        _ => {}
    }
    record
}

#[cfg(test)]
mod tests {
    use super::io_dispatch_width;

    #[test]
    fn exact_rip_copy_is_disabled_by_default_and_can_be_switched_off_again() {
        let other = u64::from(kvm_bindings::KVM_SYNC_X86_SREGS);
        let mut run = kvm_bindings::kvm_run {
            kvm_valid_regs: other,
            ..Default::default()
        };
        let mut active = false;
        super::sync_exit_rip(&mut run, &mut active, false);
        assert_eq!(run.kvm_valid_regs, other);
        for requested in [true, true, false, false, true, false] {
            super::sync_exit_rip(&mut run, &mut active, requested);
            assert_eq!(active, requested);
            assert_eq!(
                run.kvm_valid_regs,
                other
                    | if requested {
                        u64::from(kvm_bindings::KVM_SYNC_X86_REGS)
                    } else {
                        0
                    }
            );
            assert_eq!(run.kvm_dirty_regs, 0);
        }
    }

    #[test]
    #[ignore = "manual release-mode vCPU recorder microbenchmark; no VM boot"]
    fn recorder_exit_hook_overhead() {
        use crate::diagnostics::flight::ExitRing;
        use std::hint::black_box;
        const N: u32 = 2_000_000;
        let ring = ExitRing::default();
        let mut writer = ring.writer().unwrap();
        let mut run = kvm_bindings::kvm_run {
            exit_reason: kvm_bindings::KVM_EXIT_IO,
            ..Default::default()
        };
        run.__bindgen_anon_1.io = kvm_bindings::kvm_run__bindgen_ty_1__bindgen_ty_4 {
            direction: kvm_bindings::KVM_EXIT_IO_OUT as u8,
            size: 2,
            port: 0x3f8,
            count: 1,
            data_offset: 4096,
        };
        run.s.regs = kvm_bindings::kvm_sync_regs {
            regs: kvm_bindings::kvm_regs {
                rip: 0x1234,
                ..Default::default()
            },
            ..Default::default()
        };
        let pointer = &run as *const kvm_bindings::kvm_run;
        let begin = std::time::Instant::now();
        for _ in 0..N {
            black_box(pointer);
        }
        let baseline = begin.elapsed();
        let begin = std::time::Instant::now();
        for _ in 0..N {
            // SAFETY: the synthetic mapping has initialized the selected metadata.
            writer.record(unsafe { super::exit_record(black_box(pointer), None, false) });
        }
        let recorded = begin.elapsed();
        eprintln!(
            "vCPU exit hook: baseline {:.2} ns/exit; recorded {:.2} ns/exit; delta {:.2} ns/exit (N={N}); excludes kernel sync-reg copy",
            baseline.as_nanos() as f64 / N as f64,
            recorded.as_nanos() as f64 / N as f64,
            (recorded.as_nanos() as f64 - baseline.as_nanos() as f64) / N as f64
        );
        black_box(ring.snapshot());
    }

    #[test]
    #[ignore = "manual KVM exit-loop benchmark; requires /dev/kvm and a memory scope"]
    fn recorder_kvm_loop_overhead() {
        use crate::diagnostics::flight::ExitRing;
        use crate::diagnostics::report;
        use vm_memory::{Bytes, GuestAddress};

        // Keep the ioctl at one code address in every mode. Loop unswitching
        // must not turn a code-layout difference into a recorder result.
        #[inline(never)]
        fn run_once(vcpu: &mut kvm_ioctls::VcpuFd) -> bool {
            match vcpu.run() {
                Ok(kvm_ioctls::VcpuExit::IoOut(0x80, data)) if data.len() == 1 => true,
                Err(error) if error.errno() == libc::EINTR => false,
                other => panic!("unexpected benchmark exit: {other:?}"),
            }
        }

        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("SKIP recorder KVM loop benchmark: /dev/kvm is absent");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        if kvm.kvm.check_extension_int(kvm_ioctls::Cap::SyncRegs)
            & kvm_bindings::KVM_SYNC_X86_REGS as i32
            == 0
        {
            eprintln!("SKIP recorder KVM loop benchmark: sync registers are unavailable");
            return;
        }
        let ram =
            crate::memory::GuestRam::anonymous(crate::layout::MemLayout::new(8).unwrap()).unwrap();
        crate::memory::register_memslots(&kvm.vm, &ram).unwrap();
        // Flat real mode, interrupts disabled: repeatedly OUT one byte to 80h.
        ram.mem
            .write_slice(&[0xba, 0x80, 0, 0xee, 0xeb, 0xfd], GuestAddress(0x8000))
            .unwrap();
        let mut vcpu = kvm.create_vcpu(0).unwrap();
        crate::loader::init_real_mode(&mut vcpu, 0x8000).unwrap();
        let run = vcpu.get_kvm_run() as *const kvm_bindings::kvm_run;
        let ring = ExitRing::default();
        eprintln!(
            "recorder clock: {}",
            serde_json::to_string(&ring.clock_info()).unwrap()
        );
        let mut writer = None;
        const N: usize = 200_000;
        report::init_logging().unwrap();
        let mut samples: [Vec<(f64, f64)>; 6] = std::array::from_fn(|_| Vec::new());
        for round in 0..10 {
            // Sandwich each sample between local baselines to expose host drift.
            // Reverse order and discard the first group as warm-up.
            for measured in if round % 2 == 0 {
                [1, 2, 3, 4, 5]
            } else {
                [5, 4, 3, 2, 1]
            } {
                let mut group = [0.0; 3];
                for (index, mode) in [0, measured, 0].into_iter().enumerate() {
                    report::set_log_filter(match mode {
                        2 => "off,vmlab_hv_vmm::diagnostics::flight::metadata=debug",
                        3 => "off,vmlab_hv_vmm::diagnostics::flight=debug",
                        _ => "off",
                    })
                    .unwrap();
                    vcpu.clear_sync_valid_reg(kvm_ioctls::SyncReg::Register);
                    let mut sync_regs = false;
                    let begin = std::time::Instant::now();
                    let mut completed = 0;
                    while completed < N {
                        let capture = if (1..=3).contains(&mode) {
                            let capture = report::exit_recorder_mode();
                            if capture != report::RECORD_OFF && writer.is_none() {
                                writer = Some(ring.writer().unwrap());
                            }
                            super::sync_exit_rip(
                                vcpu.get_kvm_run(),
                                &mut sync_regs,
                                capture == report::RECORD_RIP,
                            );
                            capture
                        } else {
                            report::RECORD_OFF
                        };
                        if !run_once(&mut vcpu) {
                            continue;
                        }
                        if capture != report::RECORD_OFF {
                            // SAFETY: the mmap published IO metadata; only the RIP mode requested registers.
                            writer
                                .as_mut()
                                .unwrap()
                                .record(unsafe { super::exit_record(run, None, sync_regs) });
                        }
                        match mode {
                            4 => {
                                std::hint::black_box(crate::diagnostics::flight::unix_ns());
                            }
                            5 => {
                                std::hint::black_box(ring.timestamp_for_benchmark());
                            }
                            _ => {}
                        }
                        completed += 1;
                    }
                    let ns = begin.elapsed().as_nanos() as f64 / N as f64;
                    group[index] = ns;
                    if mode == 2 || mode == 3 {
                        let exits = ring.snapshot().unwrap();
                        assert_eq!(exits.len(), 256);
                        assert!(
                            exits
                                .iter()
                                .all(|record| record.exit.rip.is_some() == (mode == 3)
                                    && record.exit.address == Some(0x80))
                        );
                        assert_eq!(
                            vcpu.get_kvm_run().kvm_valid_regs
                                & u64::from(kvm_bindings::KVM_SYNC_X86_REGS),
                            if mode == 3 {
                                u64::from(kvm_bindings::KVM_SYNC_X86_REGS)
                            } else {
                                0
                            }
                        );
                    }
                    if mode == 1 {
                        assert_eq!(report::exit_recorder_mode(), report::RECORD_OFF);
                        assert_eq!(
                            vcpu.get_kvm_run().kvm_valid_regs
                                & u64::from(kvm_bindings::KVM_SYNC_X86_REGS),
                            0
                        );
                    }
                }
                if round != 0 {
                    samples[measured].push(((group[0] + group[2]) / 2.0, group[1]));
                }
            }
        }
        for (mode, label) in [
            (1, "recorder off"),
            (2, "metadata"),
            (3, "exact RIP"),
            (4, "wall-clock-only control"),
            (5, "counter-only control"),
        ] {
            let mut baselines: Vec<_> = samples[mode].iter().map(|(b, _)| *b).collect();
            let mut recordings: Vec<_> = samples[mode].iter().map(|(_, r)| *r).collect();
            let mut deltas: Vec<_> = samples[mode].iter().map(|(b, r)| r - b).collect();
            let mut ratios: Vec<_> = samples[mode]
                .iter()
                .map(|(b, r)| (r / b - 1.0) * 100.0)
                .collect();
            for values in [&mut baselines, &mut recordings, &mut deltas, &mut ratios] {
                values.sort_by(f64::total_cmp);
            }
            let baseline = baselines[4];
            let recorded = recordings[4];
            eprintln!(
                "KVM recorder loop ({label}): median baseline {baseline:.2} ns/exit; recorded {recorded:.2} ns/exit; paired delta {:.2} ns/exit ({:.2}%); paired range {:.2}..{:.2}%; 9 baseline/sample/baseline groups, {N} exits/sample",
                deltas[4], ratios[4], ratios[0], ratios[8]
            );
        }
        report::set_log_filter("off").unwrap();
    }

    #[test]
    fn exit_metadata_does_not_include_payload_and_rejects_stale_ioctl_state() {
        let mut run = kvm_bindings::kvm_run {
            exit_reason: kvm_bindings::KVM_EXIT_IO,
            ..Default::default()
        };
        run.__bindgen_anon_1.io = kvm_bindings::kvm_run__bindgen_ty_1__bindgen_ty_4 {
            direction: kvm_bindings::KVM_EXIT_IO_OUT as u8,
            size: 2,
            port: 0x3f8,
            count: 3,
            data_offset: 4096,
        };
        run.s.regs = kvm_bindings::kvm_sync_regs {
            regs: kvm_bindings::kvm_regs {
                rip: 0x1234,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: the synthetic mapping has initialized the selected union members.
        let record = unsafe { super::exit_record(&run, None, true) };
        assert_eq!(record.address, Some(0x3f8));
        assert_eq!(record.size, 2);
        assert_eq!(record.write, Some(true));
        assert_eq!(record.rip, Some(0x1234));
        // SAFETY: errno paths ignore the old exit's union metadata.
        let record = unsafe { super::exit_record(&run, Some(libc::EINTR), true) };
        assert_eq!(record.reason, u32::MAX);
        assert_eq!(record.rip, None);
        assert_eq!(record.address, None);
        run.exit_reason = kvm_bindings::KVM_EXIT_MMIO;
        run.__bindgen_anon_1.mmio = kvm_bindings::kvm_run__bindgen_ty_1__bindgen_ty_6 {
            phys_addr: 0xe000_0000,
            data: [0xa5; 8],
            len: 4,
            is_write: 1,
        };
        // SAFETY: the selected MMIO union member is initialized.
        let record = unsafe { super::exit_record(&run, None, false) };
        assert_eq!(record.address, Some(0xe000_0000));
        assert_eq!(record.size, 4);
        assert_eq!(record.write, Some(true));
        assert_eq!(record.rip, None);
    }

    #[test]
    fn repeated_ide_fifo_io_keeps_batches_and_other_registers_keep_width() {
        for width in [1, 2, 4] {
            assert_eq!(io_dispatch_width(0x1f0, width, 0), width);
            for length in [width, 512, 4096] {
                for port in [0x1f0, 0x170] {
                    assert_eq!(io_dispatch_width(port, width, length), length);
                }
                for port in [0x1f1, 0x177, 0x3f6, 0x3f8, 0x2f8, 0xc000] {
                    assert_eq!(io_dispatch_width(port, width, length), width);
                }
            }
        }
    }
}
