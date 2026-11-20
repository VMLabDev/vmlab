//! The frame clock of the scanout, and the vblank that vmlab's driver waits
//! on ([`super::CMD_VBLANK`], `guest/win3d/docs/vblank.md`).
//!
//! The device reports one mode at 60 Hz in its EDID, and this clock ticks at
//! that rate from a fixed start, so a tick neither drifts nor depends on
//! when the guest asks. A vblank wait is held in its queue, without blocking
//! the commands behind it, and answered at the first tick after it arrived
//! with the tick's number and its host time. The thread sleeps while no
//! wait is held.

use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use virtio_queue::QueueT;
use vm_memory::{Bytes, GuestAddress};

use super::{GuestMemory, State};

/// The refresh rate of the mode in the device's EDID.
pub(super) const RATE_HZ: u64 = 60;
/// The response of a vblank wait: header, tick, host time of the tick in
/// `CLOCK_MONOTONIC` nanoseconds, the period in nanoseconds, reserved.
pub(super) const RESPONSE_LEN: u32 = 48;

/// A vblank wait held until the next tick. Its response is written then, so
/// the device keeps where the guest wants it.
pub(super) struct Wait {
    pub(super) queue: u16,
    pub(super) head: u16,
    /// The writable descriptors of the chain: guest address and length.
    pub(super) response: Vec<(GuestAddress, u32)>,
}

/// One tick of the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Tick {
    pub(super) sequence: u64,
    pub(super) timestamp_ns: u64,
}

impl Tick {
    /// The response bytes of a wait answered at this tick.
    pub(super) fn response(self) -> Vec<u8> {
        let mut out = vec![0; 24];
        out[..4].copy_from_slice(&super::RESP_OK_VBLANK.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&self.timestamp_ns.to_le_bytes());
        out.extend_from_slice(&(period_ns(1) as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }
}

/// The length of `ticks` periods, in nanoseconds, without a rounding error
/// that adds up.
fn period_ns(ticks: u64) -> u64 {
    (u128::from(ticks) * 1_000_000_000 / u128::from(RATE_HZ)) as u64
}

/// The first tick after `elapsed` from the start of the clock, and later
/// than `after`.
pub(super) fn next_tick(elapsed: Duration, after: Option<u64>) -> u64 {
    let elapsed = elapsed.as_nanos();
    let due = (elapsed * u128::from(RATE_HZ) / 1_000_000_000 + 1) as u64;
    after.map_or(due, |last| due.max(last + 1))
}

#[derive(Default)]
struct Request {
    armed: bool,
    closed: bool,
}

pub(super) struct Clock {
    start: Instant,
    start_ns: u64,
    request: Mutex<Request>,
    wake: Condvar,
}

impl Clock {
    /// Start the thread. It ends when the device state is dropped.
    pub(super) fn spawn(
        id: &str,
        state: Weak<parking_lot::Mutex<State>>,
    ) -> std::io::Result<Arc<Self>> {
        let clock = Arc::new(Self {
            start: Instant::now(),
            start_ns: super::monotonic_ns(),
            request: Mutex::default(),
            wake: Condvar::new(),
        });
        let thread = clock.clone();
        std::thread::Builder::new()
            .name(format!("{id}-vblank"))
            .spawn(move || thread.run(&state))?;
        Ok(clock)
    }

    /// A clock without a thread, for a test that answers the waits itself.
    #[cfg(test)]
    pub(super) fn idle() -> Self {
        Self {
            start: Instant::now(),
            start_ns: super::monotonic_ns(),
            request: Mutex::default(),
            wake: Condvar::new(),
        }
    }

    /// A wait is held: tick until none is.
    pub(super) fn arm(&self) {
        let mut request = self.request.lock().unwrap_or_else(|e| e.into_inner());
        if !request.armed {
            request.armed = true;
            self.wake.notify_one();
        }
    }

    pub(super) fn close(&self) {
        self.request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed = true;
        self.wake.notify_one();
    }

    fn tick(&self, sequence: u64) -> Tick {
        Tick {
            sequence,
            timestamp_ns: self.start_ns + period_ns(sequence),
        }
    }

    /// Sleep until tick `sequence`. `false` when the device is gone.
    fn sleep_until(&self, sequence: u64) -> bool {
        let due = self.start + Duration::from_nanos(period_ns(sequence));
        let mut request = self.request.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if request.closed {
                return false;
            }
            let now = Instant::now();
            if due <= now {
                return true;
            }
            request = self
                .wake
                .wait_timeout(request, due - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Sleep until a wait is held. `false` when the device is gone.
    fn sleep_until_armed(&self) -> bool {
        let mut request = self.request.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if request.closed {
                return false;
            }
            if request.armed {
                return true;
            }
            request = self.wake.wait(request).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn run(&self, state: &Weak<parking_lot::Mutex<State>>) {
        let mut last = None;
        while self.sleep_until_armed() {
            let sequence = next_tick(self.start.elapsed(), last);
            if !self.sleep_until(sequence) {
                return;
            }
            let Some(state) = state.upgrade() else {
                return;
            };
            // Disarm before the device lock: a wait that the queue holds
            // while this tick is answered arms the clock again.
            self.request
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .armed = false;
            if state.lock().answer_vblanks(self.tick(sequence)) {
                last = Some(sequence);
            }
        }
    }
}

impl State {
    /// The clock's tick: answer every held wait. True when one was answered.
    pub(super) fn answer_vblanks(&mut self, tick: Tick) -> bool {
        if self.vblank_waits.is_empty() {
            return false;
        }
        let Some(mem) = self.mem.clone() else {
            self.vblank_waits.clear();
            return false;
        };
        let response = tick.response();
        let mut notify = [false; 2];
        for wait in std::mem::take(&mut self.vblank_waits) {
            write_response(&mem, &wait.response, &response);
            let Some(queue) = self.queues.get_mut(wait.queue as usize) else {
                continue;
            };
            if queue.add_used(&mem, wait.head, RESPONSE_LEN).is_ok()
                && queue.needs_notification(&mem).unwrap_or(false)
            {
                notify[wait.queue as usize & 1] = true;
            }
        }
        if let Some(irq) = &self.interrupt {
            for (queue, needed) in notify.into_iter().enumerate() {
                if needed {
                    irq.notify_queue(queue as u16);
                }
            }
        }
        if self.host_vblanks == 0 {
            log::info!(
                "virtio-gpu: the guest takes its vblank from the host frame clock (tick {})",
                tick.sequence
            );
        }
        self.host_vblanks += 1;
        true
    }
}

/// Write `bytes` across the writable descriptors of a held chain. The
/// device checked at hold time that they are long enough.
fn write_response(mem: &GuestMemory, segments: &[(GuestAddress, u32)], bytes: &[u8]) {
    let mut rest = bytes;
    for &(addr, len) in segments {
        if rest.is_empty() {
            break;
        }
        let n = rest.len().min(len as usize);
        if mem.write_slice(&rest[..n], addr).is_err() {
            return;
        }
        rest = &rest[n..];
    }
}
