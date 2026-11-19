//! The frame thread of the 2D scanout.
//!
//! A guest sends a flush command for every rectangle it draws, many of them
//! in the time of one displayed frame. A flush only records its damage and
//! asks this thread for a frame. The thread composes and publishes once per
//! frame interval, so the cost of a compose does not depend on the number
//! of flushes and is not paid by the thread that serves the queue.

use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use super::State;

/// The shortest time between two published frames, 1/60 s rounded down.
pub(super) const FRAME_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Default)]
struct Request {
    due: Option<Instant>,
    closed: bool,
}

#[derive(Default)]
pub(super) struct Pacer {
    request: Mutex<Request>,
    wake: Condvar,
}

impl Pacer {
    /// Start the thread. It ends when the device state is dropped.
    pub(super) fn spawn(
        id: &str,
        state: Weak<parking_lot::Mutex<State>>,
    ) -> std::io::Result<Arc<Self>> {
        let pacer = Arc::new(Self::default());
        let thread = pacer.clone();
        std::thread::Builder::new()
            .name(format!("{id}-frames"))
            .spawn(move || thread.run(&state))?;
        Ok(pacer)
    }

    /// Ask for a frame at `due`. An earlier request that is still open
    /// stands: the thread asks the device for the right time when it wakes.
    pub(super) fn request(&self, due: Instant) {
        let mut request = self.request.lock().unwrap_or_else(|e| e.into_inner());
        if request.due.is_none() {
            request.due = Some(due);
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

    /// Sleep until a requested time has come. `false` when the device is gone.
    fn wait(&self) -> bool {
        let mut request = self.request.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if request.closed {
                return false;
            }
            let now = Instant::now();
            request = match request.due {
                Some(due) if due <= now => {
                    request.due = None;
                    return true;
                }
                Some(due) => {
                    self.wake
                        .wait_timeout(request, due - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.wake.wait(request).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    fn run(&self, state: &Weak<parking_lot::Mutex<State>>) {
        while self.wait() {
            let Some(state) = state.upgrade() else {
                return;
            };
            // The request is taken before the device lock, so a flush that
            // arrives during the compose opens a new one.
            let again = state.lock().publish_pending(Instant::now());
            if let Some(due) = again {
                self.request(due);
            }
        }
    }
}
