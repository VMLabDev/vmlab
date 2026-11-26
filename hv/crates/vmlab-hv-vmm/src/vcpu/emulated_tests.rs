//! The seam of the vCPU loop without KVM: a scripted backend for the loop
//! itself, the memory adapter of the emulator, and a test VM of one emulated
//! aarch64 vCPU with a GICv2 and a PL011 on its bus (card C9 of phase 3d).

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::GuestMemory;
use vmlab_hv_devices::gic::{self, CpuInterface, Distributor, Gic};
use vmlab_hv_devices::pl011::{self, Pl011};
use vmlab_hv_devices::serial::SerialOut;
use vmlab_hv_proto::control::ShutdownReason;
use vmlab_hv_tcg::{Error, Memory};
use vmlab_hv_tcg_aarch64::State;
use vmlab_hv_tcg_aarch64::oracle::asm;
use vmlab_hv_tcg_aarch64::timer::Clock;

use super::emulated::{EmulatedVcpu, TimerLines};
use super::guest_bus::GuestBus;
use super::{
    Conduit, Exit, Hypercall, HypercallOutcome, Kick, VcpuBackend, VcpuHost, VcpuState, VcpuThread,
    Waker, run_vcpu,
};
use crate::bus::MmioBus;
use crate::vm::ExitReason;

const RAM: u64 = 0x4000_0000;
const RAM_SIZE: usize = 2 << 20;
const GICD: u64 = 0x0800_0000;
const GICC: u64 = 0x0801_0000;
const UART: u64 = 0x0900_0000;
/// The vector table, and its entries for the level that the guest runs at.
const VECTORS: u64 = RAM + 0x800;
const VECTOR_SYNC: u64 = 0x200;
const VECTOR_IRQ: u64 = 0x280;
const MESSAGE: u64 = RAM + 0x1000;

const PSCI_VERSION: u64 = 0x8400_0000;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_NOT_SUPPORTED: u64 = -1i64 as u64;

const TIMEOUT: Duration = Duration::from_secs(20);

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The machine of the tests: a run state, a bus and PSCI's power-off.
#[derive(Default)]
struct TestHost {
    bus: OnceLock<MmioBus>,
    paused: Mutex<bool>,
    resumed: Condvar,
    exit: Mutex<Option<ExitReason>>,
    parked: AtomicUsize,
    kicks: Mutex<Vec<(u8, Kick)>>,
    resets: AtomicUsize,
    calls: Mutex<Vec<Hypercall>>,
    /// What `check` reports, once.
    broken: Mutex<Option<ExitReason>>,
}

impl TestHost {
    fn kick_all(&self) {
        for (_, kick) in self.kicks.lock().iter() {
            kick.kick();
        }
    }

    fn pause(&self) {
        *self.paused.lock() = true;
        self.kick_all();
    }

    fn resume(&self) {
        *self.paused.lock() = false;
        self.resumed.notify_all();
    }

    fn exit(&self) -> Option<ExitReason> {
        self.exit.lock().clone()
    }

    fn wait_exit(&self) -> ExitReason {
        wait_until("the machine to exit", || self.exit().is_some());
        self.exit().unwrap()
    }
}

impl VcpuHost for TestHost {
    fn should_run(&self, _id: u8) -> bool {
        !*self.paused.lock() && self.exit.lock().is_none()
    }

    fn exited(&self) -> bool {
        self.exit.lock().is_some()
    }

    fn park(&self, _id: u8) {
        self.parked.fetch_add(1, Ordering::SeqCst);
        let mut paused = self.paused.lock();
        while *paused && self.exit.lock().is_none() {
            self.resumed
                .wait_for(&mut paused, Duration::from_millis(10));
        }
        drop(paused);
        self.parked.fetch_sub(1, Ordering::SeqCst);
    }

    fn register(&self, id: u8, kick: Kick) {
        self.kicks.lock().push((id, kick));
    }

    fn unregister(&self, id: u8, _kick: &Kick) {
        self.kicks.lock().retain(|(vcpu, _)| *vcpu != id);
    }

    fn mmio_read(&self, address: u64, data: &mut [u8]) -> bool {
        match self.bus.get().filter(|bus| bus.covers(address)) {
            Some(bus) => {
                bus.read(address, data);
                true
            }
            None => {
                data.fill(0xff);
                false
            }
        }
    }

    fn mmio_write(&self, address: u64, data: &[u8]) -> bool {
        match self.bus.get().filter(|bus| bus.covers(address)) {
            Some(bus) => {
                bus.write(address, data);
                true
            }
            None => false,
        }
    }

    fn hypercall(&self, _id: u8, call: &Hypercall) -> HypercallOutcome {
        self.calls.lock().push(*call);
        if call.conduit == Conduit::Smc {
            return HypercallOutcome::Refused;
        }
        HypercallOutcome::Return(match call.arguments[0] {
            PSCI_VERSION => [0x0001_0001, 0, 0, 0],
            PSCI_SYSTEM_OFF => {
                self.request_exit(ExitReason::Shutdown(ShutdownReason::GuestShutdown));
                [0; 4]
            }
            _ => [PSCI_NOT_SUPPORTED, 0, 0, 0],
        })
    }

    fn request_exit(&self, reason: ExitReason) {
        self.exit.lock().get_or_insert(reason);
        self.resumed.notify_all();
        self.kick_all();
    }

    fn request_reset(&self) {
        self.resets.fetch_add(1, Ordering::SeqCst);
    }

    fn check(&self) -> Option<ExitReason> {
        self.broken.lock().take()
    }
}

// ---- the loop, with a backend that follows a script ------------------------

#[derive(Default)]
struct Scripted {
    exits: VecDeque<Exit>,
    pending: bool,
    /// What the loop called, in order.
    calls: Vec<String>,
    waker: Arc<Waker>,
}

impl VcpuBackend for Scripted {
    fn run(&mut self, running: bool) -> Exit {
        self.calls.push(format!("run({running})"));
        self.pending = false;
        let exit = self
            .exits
            .pop_front()
            .unwrap_or(Exit::Fail(ExitReason::Error("the script is over".into())));
        self.pending = matches!(exit, Exit::MmioRead { .. });
        exit
    }

    fn pending(&self) -> bool {
        self.pending
    }

    fn finish_mmio_read(&mut self, data: &[u8]) {
        self.calls.push(format!("read {data:02x?}"));
    }

    fn finish_hypercall(&mut self, outcome: HypercallOutcome) {
        self.calls.push(format!("{outcome:x?}"));
    }

    fn wait(&mut self) {
        self.calls.push("wait".into());
    }

    fn kick(&self) -> Kick {
        Kick::Wake(self.waker.clone())
    }

    fn state(&mut self) -> Result<VcpuState, String> {
        Err("a script has no registers".into())
    }

    fn set_state(&mut self, _state: &VcpuState) -> Result<(), String> {
        Err("a script has no registers".into())
    }
}

/// A device of four bytes that reads what was written.
struct Latch([u8; 4]);

impl vmlab_hv_devices::BusDevice for Latch {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = self.0[(offset as usize + index) % 4];
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        for (index, byte) in data.iter().enumerate() {
            self.0[(offset as usize + index) % 4] = *byte;
        }
    }
}

fn host_with_latch() -> Arc<TestHost> {
    let host = Arc::new(TestHost::default());
    let mut bus = MmioBus::new();
    bus.register(0x1000, 4, Arc::new(Mutex::new(Latch([0; 4]))))
        .unwrap();
    assert!(host.bus.set(bus).is_ok());
    host
}

#[test]
fn the_loop_serves_each_exit_and_ends_with_the_machine() {
    let host = host_with_latch();
    let mut vcpu = Scripted {
        exits: VecDeque::from([
            Exit::Again,
            Exit::MmioWrite {
                address: 0x1000,
                data: [0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0],
                len: 4,
            },
            Exit::MmioRead {
                address: 0x1002,
                len: 2,
            },
            Exit::Hypercall(Hypercall {
                conduit: Conduit::Hvc,
                immediate: 0,
                arguments: [PSCI_VERSION, 0, 0, 0, 0, 0, 0, 0],
            }),
            Exit::Hypercall(Hypercall {
                conduit: Conduit::Smc,
                immediate: 7,
                arguments: [0; 8],
            }),
            Exit::Halt,
            Exit::Reset,
            Exit::Shutdown,
        ]),
        ..Scripted::default()
    };
    run_vcpu(3, &mut vcpu, &*host);
    assert_eq!(
        vcpu.calls,
        [
            "run(true)",
            "run(true)",
            "run(true)",
            "read [33, 44]",
            "run(true)",
            "Return([10001, 0, 0, 0])",
            "run(true)",
            "Refused",
            "run(true)",
            "wait",
            "run(true)",
            "run(true)",
        ]
    );
    assert_eq!(host.resets.load(Ordering::SeqCst), 1);
    assert_eq!(
        host.exit(),
        Some(ExitReason::Shutdown(ShutdownReason::GuestShutdown))
    );
    assert_eq!(host.calls.lock().len(), 2);
    // The kick was registered for the run and is gone after it.
    assert!(host.kicks.lock().is_empty());
}

#[test]
fn the_loop_finishes_an_open_access_before_it_parks() {
    let host = host_with_latch();
    host.pause();
    let mut vcpu = Scripted {
        exits: VecDeque::from([Exit::Again]),
        // The last run before the pause left a read open.
        pending: true,
        ..Scripted::default()
    };
    let resume = host.clone();
    let resumer = std::thread::spawn(move || {
        wait_until("the vCPU to park", || {
            resume.parked.load(Ordering::SeqCst) == 1
        });
        resume.resume();
    });
    run_vcpu(0, &mut vcpu, &*host);
    resumer.join().unwrap();
    // Not running: one run to finish the access, then the park. Running
    // again: the script is over, which fails the machine.
    assert_eq!(vcpu.calls, ["run(false)", "run(true)"]);
    assert_eq!(
        host.exit(),
        Some(ExitReason::Error("the script is over".into()))
    );
}

#[test]
fn the_loop_ends_when_a_device_reports_that_the_machine_cannot_go_on() {
    let host = host_with_latch();
    *host.broken.lock() = Some(ExitReason::Error("a device gave up".into()));
    let mut vcpu = Scripted {
        exits: VecDeque::from([Exit::Again, Exit::Again]),
        ..Scripted::default()
    };
    run_vcpu(0, &mut vcpu, &*host);
    assert_eq!(vcpu.calls, ["run(true)"]);
    assert_eq!(
        host.exit(),
        Some(ExitReason::Error("a device gave up".into()))
    );
}

// ---- the memory of an emulated CPU ------------------------------------------

fn ram() -> GuestMemory {
    GuestMemory::from_ranges(&[(GuestAddress(RAM), RAM_SIZE)]).unwrap()
}

#[test]
fn ram_is_ram_and_every_other_address_is_the_bus() {
    let host = host_with_latch();
    let memory = ram();
    let mut bus = GuestBus::new(memory.clone(), host.clone(), 1).unwrap();
    assert!(
        GuestBus::new(memory.clone(), host, 1).is_err(),
        "one memory takes one adapter"
    );

    bus.write(RAM + 0x10, &[1, 2, 3, 4]).unwrap();
    let mut bytes = [0; 4];
    bus.read(RAM + 0x10, &mut bytes).unwrap();
    assert_eq!(bytes, [1, 2, 3, 4]);
    // The devices see the same bytes.
    memory
        .read_slice(&mut bytes, GuestAddress(RAM + 0x10))
        .unwrap();
    assert_eq!(bytes, [1, 2, 3, 4]);
    assert_eq!(
        bus.ram_bounds(RAM + 5),
        Some((RAM, RAM + RAM_SIZE as u64 - 1))
    );
    assert_eq!(bus.ram_bounds(RAM - 1), None);

    // The device on the bus, in parts of at most eight bytes.
    bus.write(0x1000, &[0xaa, 0xbb, 0xcc, 0xdd]).unwrap();
    bus.read(0x1001, &mut bytes[..2]).unwrap();
    assert_eq!(bytes[..2], [0xbb, 0xcc]);
    // No device and no RAM: the access fault of the address.
    assert!(matches!(
        bus.read(0x2000, &mut bytes),
        Err(Error::Memory(0x2000))
    ));
    assert!(matches!(
        bus.write(0x2000, &bytes),
        Err(Error::Memory(0x2000))
    ));
    // Over the end of RAM: a fault, and no bus access for the rest.
    let end = RAM + RAM_SIZE as u64;
    assert!(bus.read(end - 2, &mut bytes).is_err());
    assert!(bus.write(end - 2, &bytes).is_err());
    assert!(!bus.code_bytes(end - 2, &mut bytes));
    assert!(bus.code_stamp(end - 2, 4).is_none());
    assert!(bus.code_stamp(0x1000, 4).is_none());
}

#[test]
fn each_write_is_one_generation_also_the_write_of_a_device() {
    let memory = ram();
    let mut bus = GuestBus::new(memory.clone(), host_with_latch(), 1).unwrap();
    let start = bus.generation();
    let code = bus.code_stamp(RAM, 8).unwrap();
    let crossing = bus.code_stamp(RAM + 0xffe, 4).unwrap();
    assert_eq!((crossing.first_page, crossing.last_page), (0, 1));
    assert_eq!(crossing.mapping, RAM);

    // A CPU store elsewhere: one generation, and the code pages are as
    // they were.
    bus.write(RAM + 0x3000, &[7; 16]).unwrap();
    assert_eq!(bus.generation(), start + 1);
    assert_eq!(bus.code_stamp(RAM, 8), Some(code));
    assert_eq!(bus.code_stamp(RAM + 0xffe, 4), Some(crossing));
    // A bus write counts too, and stamps no page.
    bus.write(0x1000, &[1]).unwrap();
    assert_eq!(bus.generation(), start + 2);
    assert_eq!(bus.code_stamp(RAM, 8), Some(code));

    // A device writes into the second page, through vm-memory.
    memory
        .write_slice(&[9; 4], GuestAddress(RAM + 0x1800))
        .unwrap();
    assert_eq!(bus.generation(), start + 3);
    assert_eq!(bus.code_stamp(RAM, 8), Some(code));
    assert_ne!(bus.code_stamp(RAM + 0xffe, 4), Some(crossing));
    let mut bytes = [0; 4];
    assert!(bus.code_bytes(RAM + 0x1800, &mut bytes));
    assert_eq!(bytes, [9; 4]);
    assert_eq!(bus.generation(), start + 3);
}

#[test]
fn an_exclusive_store_fails_after_any_write_to_its_page() {
    let memory = ram();
    let mut bus = GuestBus::new(memory.clone(), host_with_latch(), 2).unwrap();
    assert!(bus.transaction_lock().is_some());
    let mut other = bus.clone();
    let at = RAM + 0x4000;
    let mut bytes = [0; 8];

    let reservation = bus.load_reserved(at, &mut bytes).unwrap();
    assert_eq!(reservation, bus.reservation_generation(at, 8));
    // A store of another CPU to another page keeps it.
    other.write(at + 0x1000, &[1]).unwrap();
    assert!(bus.store_conditional(at, &[5; 8], reservation).unwrap());
    // The store itself moved the generation of the page.
    assert!(!bus.store_conditional(at, &[6; 8], reservation).unwrap());

    let reservation = bus.load_reserved(at, &mut bytes).unwrap();
    assert_eq!(bytes, [5; 8]);
    other.write(at + 0x20, &[1]).unwrap();
    assert!(!bus.store_conditional(at, &[7; 8], reservation).unwrap());

    let reservation = bus.load_reserved(at, &mut bytes).unwrap();
    memory.write_slice(&[2], GuestAddress(at + 0x40)).unwrap();
    assert!(!bus.store_conditional(at, &[8; 8], reservation).unwrap());
    bus.read(at, &mut bytes).unwrap();
    assert_eq!(bytes, [5; 8]);

    // One CPU takes no lock.
    let single = GuestBus::new(ram(), host_with_latch(), 1).unwrap();
    assert!(single.transaction_lock().is_none());
}

// ---- the test VM ------------------------------------------------------------

/// `STRB Wt, [Xn]`.
fn strb(rt: u32, rn: u32) -> u32 {
    0x3900_0000 | rn << 5 | rt
}

/// `LDRB Wt, [Xn], #1`.
fn ldrb_next(rt: u32, rn: u32) -> u32 {
    0x3840_1400 | rn << 5 | rt
}

/// `STR Wt, [Xn, #offset]`.
fn str32(rt: u32, rn: u32, offset: u32) -> u32 {
    0xb900_0000 | (offset / 4) << 10 | rn << 5 | rt
}

/// `LDR Wt, [Xn, #offset]`.
fn ldr32(rt: u32, rn: u32, offset: u32) -> u32 {
    0xb940_0000 | (offset / 4) << 10 | rn << 5 | rt
}

fn smc(immediate: u32) -> u32 {
    0xd400_0003 | immediate << 5
}

const WFI: u32 = 0xd503_207f;
/// `MSR DAIFClr, #2`: take IRQs.
const IRQ_ON: u32 = 0xd503_42ff;
const CNTVCT_EL0: u16 = 0xdf02;
const CNTV_CTL_EL0: u16 = 0xdf19;
const CNTV_CVAL_EL0: u16 = 0xdf1a;

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Output {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }
}

/// One emulated vCPU with RAM, a GICv2 and a PL011, at the addresses of the
/// `virt` machine. The PL011 is on SPI 1 and the timers on PPI 30 and 27.
struct TestVm {
    host: Arc<TestHost>,
    output: Output,
    gic: Arc<Mutex<Gic>>,
    vcpu: Arc<Mutex<EmulatedVcpu>>,
    thread: Option<VcpuThread>,
}

impl TestVm {
    /// A machine that runs `program` from the start of RAM, with `sync` and
    /// `irq` at their vectors and `message` in its data.
    fn start(program: &[u32], sync: &[u32], irq: &[u32], message: &[u8]) -> Self {
        let host = Arc::new(TestHost::default());
        let memory = ram();
        for (address, words) in [
            (RAM, program),
            (VECTORS + VECTOR_SYNC, sync),
            (VECTORS + VECTOR_IRQ, irq),
        ] {
            assert!(words.len() * 4 <= 0x80 || address == RAM && words.len() * 4 <= 0x800);
            memory
                .write_slice(&asm::bytes(words), GuestAddress(address))
                .unwrap();
        }
        memory.write_slice(message, GuestAddress(MESSAGE)).unwrap();
        let bus = GuestBus::new(memory, host.clone(), 1).unwrap();
        let mut vcpu = EmulatedVcpu::new(0, bus, State::reset(RAM), Clock::host(0, 1_000_000));
        let lines = vcpu.lines();
        let gic = Gic::new(
            "gic",
            vec![gic::CpuLines {
                irq: lines.irq,
                fiq: lines.fiq,
            }],
            64,
        );
        vcpu.set_timer_lines(TimerLines {
            physical: Gic::ppi_line(&gic, 0, 30),
            virtual_: Gic::ppi_line(&gic, 0, 27),
        });
        let output = Output::default();
        let uart = Pl011::new(
            "pl011",
            SerialOut::new(Box::new(output.clone())),
            Gic::spi_line(&gic, 1),
        );
        let mut devices = MmioBus::new();
        devices
            .register(
                GICD,
                gic::DISTRIBUTOR_SIZE,
                Arc::new(Mutex::new(Distributor(gic.clone()))),
            )
            .unwrap();
        devices
            .register(
                GICC,
                gic::CPU_INTERFACE_SIZE,
                Arc::new(Mutex::new(CpuInterface(gic.clone()))),
            )
            .unwrap();
        devices
            .register(UART, pl011::SIZE, Arc::new(Mutex::new(uart)))
            .unwrap();
        assert!(host.bus.set(devices).is_ok());
        let vcpu = Arc::new(Mutex::new(vcpu));
        let thread = VcpuThread::spawn_backend(0, vcpu.clone(), host.clone()).unwrap();
        Self {
            host,
            output,
            gic,
            vcpu,
            thread: Some(thread),
        }
    }

    fn wait_output(&self, text: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while self.output.text() != text {
            assert!(
                Instant::now() < deadline && self.host.exit().is_none(),
                "the guest wrote {:?}, not {text:?}, and the machine says {:?}",
                self.output.text(),
                self.host.exit()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The thread of the vCPU ends with the machine.
    fn join(&mut self) {
        let thread = self.thread.take().unwrap();
        wait_until("the vCPU thread to end", || thread.is_finished());
        thread.join();
    }
}

impl Drop for TestVm {
    fn drop(&mut self) {
        self.host
            .request_exit(ExitReason::Shutdown(ShutdownReason::HostRequest));
        if let Some(thread) = self.thread.take() {
            thread.join();
        }
    }
}

/// The start of every program with interrupts: the vector table, the
/// distributor and the CPU interface on, every priority let through. It
/// leaves the distributor in X0, the CPU interface in X2 and the UART in X4.
fn interrupt_setup() -> Vec<u32> {
    let mut words = Vec::new();
    words.extend(asm::load(0, VECTORS));
    words.push(asm::msr(asm::reg::VBAR_EL1, 0));
    words.extend(asm::load(0, GICD));
    words.push(asm::movz(1, 1, 0));
    words.push(str32(1, 0, 0x000));
    words.extend(asm::load(2, GICC));
    words.push(str32(1, 2, 0x00));
    words.push(asm::movz(3, 0xff, 0));
    words.push(str32(3, 2, 0x04));
    words.extend(asm::load(4, UART));
    words
}

/// `HVC` of PSCI's SYSTEM_OFF, and a loop for what comes back.
fn power_off() -> Vec<u32> {
    let mut words = asm::load(0, PSCI_SYSTEM_OFF).to_vec();
    words.extend([asm::hvc(0), asm::b(0)]);
    words
}

/// The interrupt handler of the tests: write the interrupt ID plus `add`
/// as a character, run `between`, end the interrupt, power off.
fn report_interrupt(add: u32, between: &[u32]) -> Vec<u32> {
    let mut words = vec![ldr32(6, 2, 0x0c), asm::add(5, 6, add), strb(5, 4)];
    words.extend_from_slice(between);
    words.push(str32(6, 2, 0x10));
    words.extend(power_off());
    words
}

#[test]
fn one_emulated_vcpu_writes_to_the_pl011_and_powers_off() {
    let message = b"hello from an emulated vCPU\n\0";
    let mut program = Vec::new();
    program.extend(asm::load(0, UART));
    program.extend(asm::load(1, MESSAGE));
    program.extend([
        ldrb_next(2, 1),
        asm::cbnz(2, 2),
        asm::b(3),
        strb(2, 0),
        asm::b(-4),
    ]);
    program.extend(power_off());
    let mut vm = TestVm::start(&program, &[], &[], message);
    assert_eq!(
        vm.host.wait_exit(),
        ExitReason::Shutdown(ShutdownReason::GuestShutdown)
    );
    vm.join();
    assert_eq!(vm.output.text(), "hello from an emulated vCPU\n");
    let calls = vm.host.calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].conduit, Conduit::Hvc);
    assert_eq!(calls[0].arguments[0], PSCI_SYSTEM_OFF);
    // The state of the parked vCPU: behind the HVC, with the answer in X0.
    let VcpuState::Aarch64(state) = vm.vcpu.lock().state().unwrap() else {
        panic!("an emulated vCPU has aarch64 registers");
    };
    assert_eq!(state.pc(), RAM + 4 * (program.len() as u64 - 1));
    assert_eq!(state.x(0), 0);
}

#[test]
fn the_pl011_interrupts_the_vcpu_through_the_gic() {
    let mut program = interrupt_setup();
    program.extend([
        // SPI 1, the PL011: enable it, and its transmit interrupt.
        asm::movz(3, 1 << 1, 0),
        str32(3, 0, 0x104),
        asm::movz(3, 0x20, 0),
        str32(3, 4, 0x38),
        asm::movz(3, u16::from(b'a'), 0),
        strb(3, 4),
        IRQ_ON,
        WFI,
        asm::b(-1),
    ]);
    // INTID 33 is '!'. The handler clears the transmit interrupt, which its
    // own byte raised again, before the end of the interrupt.
    let handler = report_interrupt(0, &[asm::movz(3, 0x20, 0), str32(3, 4, 0x44)]);
    let mut vm = TestVm::start(&program, &[], &handler, &[]);
    assert_eq!(
        vm.host.wait_exit(),
        ExitReason::Shutdown(ShutdownReason::GuestShutdown)
    );
    vm.join();
    assert_eq!(vm.output.text(), "a!");
}

#[test]
fn a_halted_vcpu_parks_for_a_pause_and_wakes_for_an_interrupt() {
    let mut program = interrupt_setup();
    program.extend([
        // SPI 2: a line that the test drives.
        asm::movz(3, 1 << 2, 0),
        str32(3, 0, 0x104),
        IRQ_ON,
        asm::movz(3, u16::from(b'w'), 0),
        strb(3, 4),
        WFI,
        asm::b(-1),
    ]);
    // INTID 34, plus 0x20, is 'B'.
    let handler = report_interrupt(0x20, &[]);
    let mut vm = TestVm::start(&program, &[], &handler, &[]);
    vm.wait_output("w");
    // Let it reach the WFI. Halted, it holds no interrupt and writes no more.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(vm.output.text(), "w");
    assert!(vm.host.exit().is_none());

    // A pause kicks it out of the halt, and its registers can be read.
    vm.host.pause();
    wait_until("the vCPU to park", || {
        vm.host.parked.load(Ordering::SeqCst) == 1
    });
    let VcpuState::Aarch64(mut state) = vm.vcpu.lock().state().unwrap() else {
        panic!("an emulated vCPU has aarch64 registers");
    };
    let wfi = RAM + 4 * (program.len() as u64 - 2);
    assert!(
        (wfi..=wfi + 4).contains(&state.pc()),
        "the vCPU is at {:#x}",
        state.pc()
    );
    assert_eq!(state.x(4), UART);
    // A debug port writes a register of the parked vCPU: the handler adds
    // its character to X6, which it loads first, so the write shows in X7.
    state.set_x(7, 0x1234);
    vm.vcpu
        .lock()
        .set_state(&VcpuState::Aarch64(state))
        .unwrap();
    // The interrupt arrives while the machine is paused: nothing runs.
    Gic::spi_line(&vm.gic, 2).set_level(true);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(vm.output.text(), "w");
    vm.host.resume();
    assert_eq!(
        vm.host.wait_exit(),
        ExitReason::Shutdown(ShutdownReason::GuestShutdown)
    );
    vm.join();
    assert_eq!(vm.output.text(), "wB");
    let VcpuState::Aarch64(state) = vm.vcpu.lock().state().unwrap() else {
        panic!("an emulated vCPU has aarch64 registers");
    };
    assert_eq!(state.x(7), 0x1234);
    assert_eq!(state.x(6), 34);
}

#[test]
fn the_virtual_timer_wakes_a_halted_vcpu_through_ppi_27() {
    let mut program = interrupt_setup();
    program.extend(asm::load(3, 1 << 27));
    program.extend([
        str32(3, 0, 0x100),
        // Two milliseconds from now, on the 1 MHz counter of the test.
        asm::mrs(5, CNTVCT_EL0),
        asm::add(5, 5, 2000),
        asm::msr(CNTV_CVAL_EL0, 5),
        asm::movz(3, 1, 0),
        asm::msr(CNTV_CTL_EL0, 3),
        IRQ_ON,
        WFI,
        asm::b(-1),
    ]);
    // INTID 27, plus 0x40, is '['. The handler masks the timer, whose line
    // is a level, before the end of the interrupt.
    let handler = report_interrupt(0x40, &[asm::movz(3, 3, 0), asm::msr(CNTV_CTL_EL0, 3)]);
    let started = Instant::now();
    let mut vm = TestVm::start(&program, &[], &handler, &[]);
    assert_eq!(
        vm.host.wait_exit(),
        ExitReason::Shutdown(ShutdownReason::GuestShutdown)
    );
    vm.join();
    assert_eq!(vm.output.text(), "[");
    assert!(started.elapsed() >= Duration::from_millis(2));
}

#[test]
fn a_refused_call_is_an_undefined_instruction_and_a_kick_ends_a_spinning_guest() {
    let mut program = interrupt_setup();
    program.extend(asm::load(0, PSCI_VERSION));
    program.extend([
        asm::hvc(0),
        // PSCI_VERSION came back in X0: 1.1 is 0x10001. Write its low byte
        // plus '0'.
        asm::add(5, 0, u32::from(b'0')),
        strb(5, 4),
        smc(0),
        asm::b(0),
    ]);
    // The machine has no SMC: the guest takes the exception, says so, and
    // spins.
    let sync = [asm::movz(3, u16::from(b'?'), 0), strb(3, 4), asm::b(0)];
    let mut vm = TestVm::start(&program, &sync, &[], &[]);
    vm.wait_output("1?");
    let calls = vm.host.calls.lock().clone();
    assert_eq!(
        calls.iter().map(|call| call.conduit).collect::<Vec<_>>(),
        [Conduit::Hvc, Conduit::Smc]
    );
    // It spins in a loop of one instruction. The end of the machine kicks it.
    assert!(!vm.thread.as_ref().unwrap().is_finished());
    vm.host
        .request_exit(ExitReason::Shutdown(ShutdownReason::HostRequest));
    vm.join();
}

/// `AND Xd, Xn, #0x3f`.
fn and_low6(rd: u32, rn: u32) -> u32 {
    0x9240_1400 | rn << 5 | rd
}

/// An address with no RAM and no device.
const NOTHING: u64 = 0x1000_0000;

// The core had two stops for these until it translated addresses (card C8),
// and the backend ended the machine on them. They are exceptions of the
// guest now: the machine runs on, and the guest sees the cause.
#[test]
fn a_data_abort_and_an_unaligned_access_are_exceptions_of_the_guest_and_no_exit() {
    use vmlab_hv_tcg_aarch64::sysreg::reg;

    let mut program = interrupt_setup();
    // The MMU is off, so data is Device memory: a load of four bytes at an
    // odd address is an alignment fault.
    program.extend(asm::load(7, MESSAGE + 1));
    program.push(ldr32(8, 7, 0));
    // Nothing is at this address: an external abort, for a load and a store.
    program.extend(asm::load(7, NOTHING));
    program.extend([ldr32(8, 7, 0), str32(8, 7, 4)]);
    program.extend(power_off());
    // Write the fault status code of ESR_EL1 plus 0x20 as a character, and
    // go on behind the instruction that faulted.
    let sync = [
        asm::mrs(5, asm::reg::ESR_EL1),
        and_low6(5, 5),
        asm::add(5, 5, 0x20),
        strb(5, 4),
        asm::mrs(6, asm::reg::ELR_EL1),
        asm::add(6, 6, 4),
        asm::blr(6),
    ];
    let mut vm = TestVm::start(&program, &sync, &[], &[]);
    assert_eq!(
        vm.host.wait_exit(),
        ExitReason::Shutdown(ShutdownReason::GuestShutdown)
    );
    vm.join();
    // 0x21 is the alignment fault and 0x10 the external abort.
    assert_eq!(vm.output.text(), "A00");
    // What the last one left: a data abort of the same level, of a store.
    let VcpuState::Aarch64(state) = vm.vcpu.lock().state().unwrap() else {
        panic!("an emulated vCPU has aarch64 registers");
    };
    let esr = state.system.regs[reg::ESR_EL1];
    assert_eq!(esr >> 26, 0x25, "ESR_EL1 is {esr:#x}");
    assert_eq!(esr & 0x7f, 0x40 | 0x10, "ESR_EL1 is {esr:#x}");
    assert_eq!(state.system.regs[reg::FAR_EL1], NOTHING + 4);
}
