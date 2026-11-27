//! vCPU threads: one loop over a backend that runs the guest, and the
//! machinery to interrupt it.
//!
//! The loop knows no accelerator. A [`VcpuBackend`] runs one vCPU until it
//! needs the machine: an MMIO access, a hypercall, a halt, a shutdown or a
//! reset. A [`VcpuHost`] is the machine as the loop needs it: the run state,
//! the MMIO bus, and where a shutdown and a reset go. KVM is one backend
//! (`kvm`), the aarch64 emulator the other (`emulated`).

pub mod emulated;
pub mod guest_bus;
mod kvm;

#[cfg(test)]
mod emulated_tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use kvm_ioctls::VcpuFd;
use parking_lot::{Condvar, Mutex};

pub use kvm::KvmVcpu;

use crate::vm::{ExitReason, VmShared};

/// The signal used to kick a vCPU out of KVM_RUN. A no-op handler without
/// `SA_RESTART` makes the ioctl return `EINTR`; the run loop then re-checks
/// the machine state. The registry in [`VmShared`] holds each thread's id;
/// kicking goes through it, so thread handles need no tid of their own.
const KICK_SIGNAL: libc::c_int = libc::SIGUSR1;

extern "C" fn kick_handler(_sig: libc::c_int) {}

/// Install the kick signal handler once, before the first vCPU thread runs.
pub fn install_kick_handler() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: sigaction(2) on SIGUSR1 with a handler that does nothing
        // and an empty mask, so nothing else is blocked. Flags exclude
        // SA_RESTART on purpose: blocked ioctls must fail with EINTR.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = kick_handler as *const () as usize;
            action.sa_flags = 0;
            if libc::sigaction(KICK_SIGNAL, &action, std::ptr::null_mut()) != 0 {
                panic!(
                    "sigaction(SIGUSR1) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    });
}

/// How a guest calls what is above its kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conduit {
    Hvc,
    Smc,
}

/// A call of the guest to the machine: PSCI on the aarch64 `virt` machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hypercall {
    pub conduit: Conduit,
    /// The immediate of the instruction.
    pub immediate: u16,
    /// The argument registers: X0 to X7 on aarch64.
    pub arguments: [u64; 8],
}

/// What the machine answers to a [`Hypercall`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HypercallOutcome {
    /// The call is served. The values go into the result registers: X0 to
    /// X3 on aarch64.
    Return([u64; 4]),
    /// The machine has no such conduit: the guest takes the exception of an
    /// instruction that its CPU does not have.
    Refused,
}

/// Why a vCPU stopped running.
#[derive(Debug)]
pub enum Exit {
    /// Nothing for the loop to do but to look at the machine state again:
    /// the backend was kicked, used its time, or served the exit itself.
    Again,
    /// A read of `len` bytes that no RAM answers. The loop reads the bus
    /// and gives the bytes back with [`VcpuBackend::finish_mmio_read`].
    MmioRead { address: u64, len: usize },
    /// A write of the first `len` bytes of `data`.
    MmioWrite {
        address: u64,
        data: [u8; 8],
        len: usize,
    },
    /// The loop asks the machine and gives the answer back with
    /// [`VcpuBackend::finish_hypercall`].
    Hypercall(Hypercall),
    /// The vCPU waits for an interrupt. The loop calls
    /// [`VcpuBackend::wait`].
    Halt,
    /// The guest powered the machine off.
    Shutdown,
    /// The guest reset the machine.
    Reset,
    /// The vCPU cannot run on: the machine ends with this reason.
    Fail(ExitReason),
}

/// The registers of one vCPU that a debug port reads and writes. A snapshot
/// of a KVM vCPU holds more (`snapshot.rs` reads it from the fd).
#[derive(Clone, Debug)]
pub enum VcpuState {
    X86 {
        regs: kvm_bindings::kvm_regs,
        sregs: Box<kvm_bindings::kvm_sregs>,
    },
    Aarch64(Box<vmlab_hv_tcg_aarch64::State>),
}

/// One vCPU of an accelerator or an emulator.
pub trait VcpuBackend {
    /// Run the guest until an exit. `running` is false when the machine is
    /// not to run and the last exit left an access open ([`pending`]): the
    /// backend then finishes that access and runs no instruction.
    ///
    /// [`pending`]: VcpuBackend::pending
    fn run(&mut self, running: bool) -> Exit;

    /// Whether the last exit left an access that the next `run` finishes.
    /// The state of the vCPU is not whole before that, so the loop does not
    /// park the thread while this holds.
    fn pending(&self) -> bool {
        false
    }

    /// The machine stops this vCPU: its thread parks after this.
    fn parking(&mut self) {}

    /// The bytes that the bus gave for the last [`Exit::MmioRead`].
    fn finish_mmio_read(&mut self, _data: &[u8]) {}

    /// The answer of the machine to the last [`Exit::Hypercall`].
    fn finish_hypercall(&mut self, _outcome: HypercallOutcome) {}

    /// After [`Exit::Halt`]: wait until an interrupt or a kick. A backend
    /// whose accelerator waits inside `run` does nothing here.
    fn wait(&mut self) {}

    /// How another thread makes `run` and `wait` return. The loop asks once,
    /// on the thread of the vCPU, before the first `run`.
    fn kick(&self) -> Kick;

    /// The registers of the vCPU. The thread of the vCPU is parked.
    fn state(&mut self) -> Result<VcpuState, String>;

    fn set_state(&mut self, state: &VcpuState) -> Result<(), String>;
}

/// The machine as the loop of a vCPU needs it.
pub trait VcpuHost: Send + Sync {
    /// Whether the vCPU `id` may run the guest now.
    fn should_run(&self, id: u8) -> bool;
    /// Whether the machine stopped for good.
    fn exited(&self) -> bool;
    /// Block until the vCPU may run again or the machine exits.
    fn park(&self, id: u8);
    /// Keep the kick of the vCPU `id` from now until `unregister`.
    fn register(&self, id: u8, kick: Kick);
    fn unregister(&self, id: u8, kick: &Kick);
    /// Read the MMIO bus. `false`: no device decodes the address, and
    /// `data` is what an open bus reads.
    fn mmio_read(&self, address: u64, data: &mut [u8]) -> bool;
    /// Write the MMIO bus. `false`: no device decodes the address.
    fn mmio_write(&self, address: u64, data: &[u8]) -> bool;
    /// Serve a call of the guest. A machine without firmware calls of its
    /// own refuses each one.
    fn hypercall(&self, _id: u8, _call: &Hypercall) -> HypercallOutcome {
        HypercallOutcome::Refused
    }
    /// End the machine.
    fn request_exit(&self, reason: ExitReason);
    /// Reset the machine.
    fn request_reset(&self);
    /// Whether a device found, during the last exit, that the machine
    /// cannot go on.
    fn check(&self) -> Option<ExitReason> {
        None
    }
}

/// Wakes a vCPU that runs or waits in the process, where no signal helps.
#[derive(Default)]
pub struct Waker {
    kicked: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
    /// What else a kick does: an emulated CPU leaves its run of compiled
    /// code at the next block (3e DESIGN.md H10c).
    also: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Waker {
    /// A waker whose kick also calls `also`.
    pub fn with_kick(also: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            also: Some(Box::new(also)),
            ..Self::default()
        }
    }

    /// Ask the vCPU to leave its backend and look at the machine state.
    pub fn kick(&self) {
        self.kicked.store(true, Ordering::SeqCst);
        if let Some(also) = &self.also {
            also();
        }
        self.notify();
    }

    /// Wake a waiting vCPU, for it to look at its interrupt lines.
    pub fn notify(&self) {
        let _guard = self.lock.lock();
        self.wake.notify_all();
    }

    /// Whether a kick is waiting to be seen.
    pub fn kicked(&self) -> bool {
        self.kicked.load(Ordering::SeqCst)
    }

    /// Forget the kick: the vCPU is about to look at the machine state.
    pub fn clear(&self) {
        self.kicked.store(false, Ordering::SeqCst);
    }

    /// Block until `ready` holds, a `notify`, or `timeout`. `ready` is
    /// asked under the lock that `notify` takes, so a wake between the
    /// question and the sleep is not lost.
    pub fn wait(&self, timeout: Duration, ready: impl Fn() -> bool) {
        let mut guard = self.lock.lock();
        if !ready() {
            self.wake.wait_for(&mut guard, timeout);
        }
    }
}

/// How to make the backend of one vCPU return to the loop.
#[derive(Clone)]
pub enum Kick {
    /// A signal to the thread: `KVM_RUN` fails with `EINTR`.
    Signal(libc::pthread_t),
    Wake(Arc<Waker>),
}

impl Kick {
    pub fn kick(&self) {
        match self {
            // SAFETY: pthread_kill(3) with a registered signal number on a
            // thread id that the registry of the machine holds. A stale id
            // cannot exist: unregister runs before the thread exits.
            Self::Signal(thread) => unsafe {
                libc::pthread_kill(*thread, KICK_SIGNAL);
            },
            Self::Wake(waker) => waker.kick(),
        }
    }
}

/// A backend that other threads reach while its thread is parked: the
/// snapshot path and the debug port of an emulated vCPU.
impl<B: VcpuBackend> VcpuBackend for Arc<Mutex<B>> {
    fn run(&mut self, running: bool) -> Exit {
        self.lock().run(running)
    }

    fn pending(&self) -> bool {
        self.lock().pending()
    }

    fn parking(&mut self) {
        self.lock().parking();
    }

    fn finish_mmio_read(&mut self, data: &[u8]) {
        self.lock().finish_mmio_read(data);
    }

    fn finish_hypercall(&mut self, outcome: HypercallOutcome) {
        self.lock().finish_hypercall(outcome);
    }

    fn wait(&mut self) {
        self.lock().wait();
    }

    fn kick(&self) -> Kick {
        self.lock().kick()
    }

    fn state(&mut self) -> Result<VcpuState, String> {
        self.lock().state()
    }

    fn set_state(&mut self, state: &VcpuState) -> Result<(), String> {
        self.lock().set_state(state)
    }
}

/// One running vCPU: its thread. The kick handle is held in the registry of
/// the machine from thread start to thread end. A KVM fd itself is shared
/// with the VMM so snapshot orchestration can read and write vCPU state
/// while the owning thread is parked.
pub struct VcpuThread {
    handle: Option<JoinHandle<()>>,
}

impl VcpuThread {
    /// Spawn the thread that owns the KVM `vcpu` and runs it against
    /// `shared`. The fd sits in a mutex shared with the snapshot path: this
    /// thread holds it across KVM_RUN only, so a snapshot can take it once
    /// the thread parks.
    pub fn spawn(id: u8, vcpu: Arc<Mutex<VcpuFd>>, shared: Arc<VmShared>) -> std::io::Result<Self> {
        install_kick_handler();
        Self::start(id, shared.clone(), move || {
            run_vcpu(id, &mut KvmVcpu::new(id, vcpu, &shared), &*shared);
        })
    }

    /// Spawn the thread that runs `backend` against `host`.
    pub fn spawn_backend<B>(
        id: u8,
        mut backend: B,
        host: Arc<dyn VcpuHost>,
    ) -> std::io::Result<Self>
    where
        B: VcpuBackend + Send + 'static,
    {
        Self::start(id, host.clone(), move || run_vcpu(id, &mut backend, &*host))
    }

    fn start(
        id: u8,
        host: Arc<dyn VcpuHost>,
        run: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Self> {
        let handle = std::thread::Builder::new()
            .name(format!("vcpu-{id}"))
            .spawn(move || {
                if let Err(error) = crate::diagnostics::signal::ensure_alt_stack() {
                    host.request_exit(ExitReason::Error(error));
                    return;
                }
                // The banked registers of an interrupt controller in the
                // process follow the thread.
                vmlab_hv_devices::gic::set_current_cpu(usize::from(id));
                run();
            })?;
        Ok(VcpuThread {
            handle: Some(handle),
        })
    }

    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

/// The loop of one vCPU, on its thread, until the machine exits.
pub fn run_vcpu(id: u8, vcpu: &mut dyn VcpuBackend, host: &dyn VcpuHost) {
    let kick = vcpu.kick();
    host.register(id, kick.clone());
    run_loop(id, vcpu, host);
    host.unregister(id, &kick);
}

fn run_loop(id: u8, vcpu: &mut dyn VcpuBackend, host: &dyn VcpuHost) {
    loop {
        let running = host.should_run(id);
        // An exit can leave an access open that the next run finishes. The
        // state of the vCPU is not whole before that: finish before parking,
        // without executing another instruction.
        if !running && !vcpu.pending() {
            if host.exited() {
                return;
            }
            vcpu.parking();
            host.park(id);
            continue;
        }
        match vcpu.run(running) {
            Exit::Again => {}
            Exit::MmioRead { address, len } => {
                let mut data = [0; 8];
                host.mmio_read(address, &mut data[..len]);
                vcpu.finish_mmio_read(&data[..len]);
            }
            Exit::MmioWrite { address, data, len } => {
                host.mmio_write(address, &data[..len]);
            }
            Exit::Hypercall(call) => {
                let outcome = host.hypercall(id, &call);
                vcpu.finish_hypercall(outcome);
            }
            Exit::Halt => vcpu.wait(),
            Exit::Shutdown => host.request_exit(ExitReason::Shutdown(
                vmlab_hv_proto::control::ShutdownReason::GuestShutdown,
            )),
            Exit::Reset => host.request_reset(),
            Exit::Fail(reason) => {
                host.request_exit(reason);
                return;
            }
        }
        if let Some(reason) = host.check() {
            host.request_exit(reason);
            return;
        }
    }
}
