//! The emulator backend of a vCPU (DESIGN.md of phase 3d, F1): the aarch64
//! core of `vmlab-hv-tcg-aarch64` behind the same loop as KVM. A machine
//! that is described with `accel: tcg` runs its vCPUs on it.
//!
//! The core runs on a [`GuestBus`], so a load or a store of a device
//! register is a bus access inside the instruction and no exit. The exits
//! are the hypercall (PSCI is the machine's), the halt of `WFI`, and what
//! stops the core for good.
//!
//! The core has two interrupt inputs, IRQ and FIQ, as levels. The interrupt
//! controller of the machine drives them through [`EmulatedVcpu::lines`],
//! from any thread. The core looks at them between two slices of
//! instructions, so an interrupt waits for the end of a slice at most. The
//! outputs of its generic timer go to the lines of [`TimerLines`], which the
//! machine wires to the controller.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use vmlab_hv_devices::IrqLine;
use vmlab_hv_tcg_aarch64::exec::Unknown;
use vmlab_hv_tcg_aarch64::timer::Clock;
use vmlab_hv_tcg_aarch64::{Cpu, State, Stop};

use super::guest_bus::GuestBus;
use super::{Conduit, Exit, Hypercall, HypercallOutcome, Kick, VcpuBackend, VcpuState, Waker};
use crate::vm::ExitReason;

/// The instructions of one slice. A kick, and an interrupt that another
/// thread raised, wait for the end of a slice.
const SLICE: u64 = 1 << 16;

/// The longest sleep of a halted vCPU whose timer has no deadline. A wake
/// ends the sleep at once, so this bounds only a wake that was lost.
const IDLE: Duration = Duration::from_millis(100);

/// One interrupt input of the core: a level that any thread sets.
pub struct CpuLine {
    level: AtomicBool,
    waker: Arc<Waker>,
}

impl CpuLine {
    /// The level of the line now.
    pub fn level(&self) -> bool {
        self.level.load(Ordering::SeqCst)
    }
}

impl IrqLine for CpuLine {
    fn set_level(&self, high: bool) {
        if self.level.swap(high, Ordering::SeqCst) != high && high {
            self.waker.notify();
        }
    }
}

/// The two interrupt inputs of one emulated vCPU.
#[derive(Clone)]
pub struct CpuLines {
    pub irq: Arc<CpuLine>,
    pub fiq: Arc<CpuLine>,
}

/// Where the outputs of the generic timer of one vCPU go: on the `virt`
/// machine the physical timer is PPI 30 and the virtual timer PPI 27.
#[derive(Clone)]
pub struct TimerLines {
    pub physical: Arc<dyn IrqLine>,
    pub virtual_: Arc<dyn IrqLine>,
}

pub struct EmulatedVcpu {
    id: u8,
    cpu: Cpu<GuestBus>,
    waker: Arc<Waker>,
    lines: CpuLines,
    timer: Option<TimerLines>,
    /// The levels that the timer lines were given last.
    timer_levels: [Option<bool>; 2],
    /// The address of the hypercall that waits for its answer.
    hypercall: Option<u64>,
    /// What the guest asked for and the CPU does not have, as logged.
    logged: usize,
    /// The system counter of the machine and its frequency. The counter of
    /// the CPU is set from it before each slice, so that it stands still
    /// while the machine is paused.
    counter: Option<(Counter, u64)>,
}

/// The count of the system counter of a machine now.
pub type Counter = Arc<dyn Fn() -> u64 + Send + Sync>;

impl EmulatedVcpu {
    /// The vCPU `id` in the reset state `state`, on `memory`, with the
    /// system counter of `clock`.
    pub fn new(id: u8, memory: GuestBus, state: State, clock: Clock) -> Self {
        let waker = Arc::new(Waker::default());
        let line = || {
            Arc::new(CpuLine {
                level: AtomicBool::new(false),
                waker: waker.clone(),
            })
        };
        let lines = CpuLines {
            irq: line(),
            fiq: line(),
        };
        let mut cpu = Cpu::new(memory, state);
        cpu.clock = clock;
        Self {
            id,
            cpu,
            waker,
            lines,
            timer: None,
            timer_levels: [None; 2],
            hypercall: None,
            logged: 0,
            counter: None,
        }
    }

    /// Take the system counter from `counter`, at `frequency` counts per
    /// second, in place of the clock that the vCPU was made with.
    pub fn set_counter(&mut self, counter: Counter, frequency: u64) {
        self.counter = Some((counter, frequency));
        self.sync_counter();
    }

    fn sync_counter(&mut self) {
        if let Some((counter, frequency)) = &self.counter {
            self.cpu.clock = Clock::host(counter(), *frequency);
        }
    }

    /// The core, for a harness that steps it and for the machine that
    /// resets it. The thread of the vCPU is parked.
    pub fn cpu(&mut self) -> &mut Cpu<GuestBus> {
        &mut self.cpu
    }

    /// Put the CPU into `state`, as a reset and PSCI `CPU_ON` do.
    pub fn reset(&mut self, state: State) {
        self.cpu.state = state;
        self.cpu.clear_exclusive();
        self.cpu.irq = false;
        self.cpu.fiq = false;
        self.hypercall = None;
        self.timer_levels = [None; 2];
        self.sync_counter();
    }

    /// The interrupt inputs, for the interrupt controller of the machine.
    pub fn lines(&self) -> CpuLines {
        self.lines.clone()
    }

    /// Wire the outputs of the generic timer.
    pub fn set_timer_lines(&mut self, lines: TimerLines) {
        self.timer = Some(lines);
        self.timer_levels = [None; 2];
    }

    /// Give the timer lines the levels of the timers now.
    pub fn drive_timer(&mut self) {
        let Some(timer) = &self.timer else {
            return;
        };
        let levels = self.cpu.timer_lines();
        for (slot, (line, level)) in [
            (&timer.physical, levels.physical),
            (&timer.virtual_, levels.virtual_),
        ]
        .into_iter()
        .enumerate()
        {
            if self.timer_levels[slot] != Some(level) {
                self.timer_levels[slot] = Some(level);
                line.set_level(level);
            }
        }
    }

    /// How long until a timer line rises, when one can.
    fn timer_wait(&self) -> Option<Duration> {
        let deadline = self.cpu.timer_deadline()?;
        match self.cpu.clock {
            Clock::Host { frequency, .. } if frequency > 0 => {
                let ticks = deadline.saturating_sub(self.cpu.clock.count());
                let nanos = u128::from(ticks) * 1_000_000_000 / u128::from(frequency);
                Some(Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64))
            }
            // The owner of a manual clock moves it and kicks.
            _ => None,
        }
    }

    fn interrupted(&self) -> bool {
        self.lines.irq.level.load(Ordering::SeqCst) || self.lines.fiq.level.load(Ordering::SeqCst)
    }

    /// Write what the guest asked for and this CPU does not have to the
    /// log, each once (DESIGN.md G3f).
    fn log_unknown(&mut self) {
        if self.cpu.unknown.len() == self.logged {
            return;
        }
        // The set is ordered by value, not by time: say the whole of it
        // when it grew. It is bounded, and it grows a few times in a boot.
        self.logged = self.cpu.unknown.len();
        let unknown: Vec<String> = self.cpu.unknown.iter().map(Unknown::to_string).collect();
        log::warn!(
            "vcpu {}: the guest took the Undefined Instruction exception for: {}",
            self.id,
            unknown.join("; ")
        );
    }

    fn fail(&self, what: String) -> Exit {
        Exit::Fail(ExitReason::Error(format!("vcpu {}: {what}", self.id)))
    }
}

impl VcpuBackend for EmulatedVcpu {
    fn run(&mut self, running: bool) -> Exit {
        // No exit of this backend leaves an access open.
        if !running {
            return Exit::Again;
        }
        self.waker.clear();
        self.sync_counter();
        self.drive_timer();
        self.cpu.irq = self.lines.irq.level.load(Ordering::SeqCst);
        self.cpu.fiq = self.lines.fiq.level.load(Ordering::SeqCst);
        let stop = self.cpu.run(SLICE);
        self.drive_timer();
        self.log_unknown();
        match stop {
            Stop::Budget => Exit::Again,
            Stop::Halt => Exit::Halt,
            Stop::Hypercall {
                conduit,
                immediate,
                pc,
            } => {
                self.hypercall = Some(pc);
                Exit::Hypercall(Hypercall {
                    conduit: match conduit {
                        vmlab_hv_tcg_aarch64::Conduit::Hvc => Conduit::Hvc,
                        vmlab_hv_tcg_aarch64::Conduit::Smc => Conduit::Smc,
                    },
                    immediate,
                    arguments: std::array::from_fn(|n| self.cpu.state.x(n)),
                })
            }
            Stop::Unimplemented { pc, record } => self.fail(format!(
                "the emulator has no instruction {record:?} yet, at {pc:#x}"
            )),
            Stop::Host(error) => self.fail(error),
        }
    }

    fn finish_hypercall(&mut self, outcome: HypercallOutcome) {
        let Some(pc) = self.hypercall.take() else {
            return;
        };
        match outcome {
            HypercallOutcome::Return(values) => {
                for (n, value) in values.into_iter().enumerate() {
                    self.cpu.state.set_x(n, value);
                }
            }
            HypercallOutcome::Refused => self.cpu.refuse_hypercall(pc),
        }
    }

    /// `WFI`: sleep until an interrupt line is high, a timer line rises, or
    /// a kick. The lines are levels, so a wake that comes first is seen.
    fn wait(&mut self) {
        loop {
            self.sync_counter();
            self.drive_timer();
            if self.interrupted() || self.waker.kicked() {
                return;
            }
            let timer = self.timer_wait();
            let lines = &self.lines;
            let waker = &self.waker;
            waker.wait(timer.unwrap_or(IDLE).min(IDLE), || {
                waker.kicked()
                    || lines.irq.level.load(Ordering::SeqCst)
                    || lines.fiq.level.load(Ordering::SeqCst)
            });
        }
    }

    fn kick(&self) -> Kick {
        Kick::Wake(self.waker.clone())
    }

    fn state(&mut self) -> Result<VcpuState, String> {
        Ok(VcpuState::Aarch64(Box::new(self.cpu.state.clone())))
    }

    fn set_state(&mut self, state: &VcpuState) -> Result<(), String> {
        let VcpuState::Aarch64(state) = state else {
            return Err("an emulated aarch64 vCPU takes the state of an aarch64 CPU".into());
        };
        self.cpu.state = (**state).clone();
        self.cpu.clear_exclusive();
        self.hypercall = None;
        Ok(())
    }
}
