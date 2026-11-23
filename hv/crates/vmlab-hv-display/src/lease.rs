use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long `retire` waits for readers before it revokes the ones that can be
/// revoked. Sampling a frame takes milliseconds. A reader that is still there
/// after this time is stopped, hangs or is hostile.
pub const REVOKE_AFTER: Duration = Duration::from_secs(1);

type Revoke = Box<dyn FnOnce() + Send>;

struct State {
    valid: bool,
    readers: usize,
    /// One entry for each live revocable guard, by the guard's key.
    revokers: Vec<(u64, Revoke)>,
    next_key: u64,
}

/// One publication of a reusable allocation. Invalidating it excludes new readers.
pub struct BufferLease {
    state: Mutex<State>,
    released: Condvar,
}

impl BufferLease {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                valid: true,
                readers: 0,
                revokers: Vec::new(),
                next_key: 0,
            }),
            released: Condvar::new(),
        })
    }

    /// A reader that ends by itself in bounded time, such as a readback.
    pub fn acquire(self: &Arc<Self>) -> Option<BufferGuard> {
        let mut state = self.state.lock().unwrap();
        if !state.valid {
            return None;
        }
        state.readers += 1;
        Some(BufferGuard(self.clone(), None))
    }

    /// A reader whose end depends on a peer. `revoke` must drop the returned
    /// guard, or make its owner drop it, without waiting for that peer.
    /// `retire` calls it at most once, with no lock of the lease held, when
    /// the guard has outlived the wait. It is dropped unused with the guard.
    pub fn acquire_revocable(
        self: &Arc<Self>,
        revoke: impl FnOnce() + Send + 'static,
    ) -> Option<BufferGuard> {
        let mut state = self.state.lock().unwrap();
        if !state.valid {
            return None;
        }
        state.readers += 1;
        let key = state.next_key;
        state.next_key += 1;
        state.revokers.push((key, Box::new(revoke)));
        Some(BufferGuard(self.clone(), Some(key)))
    }

    /// Stop lending this publication and wait for all existing readers.
    /// Revocable readers that are still there after [`REVOKE_AFTER`] are
    /// revoked, so no peer of the display can hold this call for ever.
    pub fn retire(&self) {
        self.retire_within(REVOKE_AFTER);
    }

    /// [`Self::retire`] with the wait before the revocation given.
    pub fn retire_within(&self, patience: Duration) {
        let deadline = Instant::now() + patience;
        let mut state = self.state.lock().unwrap();
        state.valid = false;
        loop {
            if state.readers == 0 {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self.released.wait_timeout(state, left).unwrap().0;
        }
        // No new reader can arrive: the publication is invalid.
        let revokers = std::mem::take(&mut state.revokers);
        drop(state);
        for (_, revoke) in revokers {
            revoke();
        }
        let mut state = self.state.lock().unwrap();
        while state.readers != 0 {
            state = self.released.wait(state).unwrap();
        }
    }

    pub fn try_retire(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.readers != 0 {
            return false;
        }
        state.valid = false;
        true
    }
}

pub struct BufferGuard(Arc<BufferLease>, Option<u64>);

impl Drop for BufferGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.readers -= 1;
        // The revoker can own what this drop runs in. Drop it after the lock.
        let revoker = self.1.and_then(|key| {
            let index = state.revokers.iter().position(|(k, _)| *k == key)?;
            Some(state.revokers.swap_remove(index))
        });
        self.0.released.notify_all();
        drop(state);
        drop(revoker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retirement_excludes_new_readers_and_waits_for_all_releases() {
        let lease = BufferLease::new();
        let first = lease.acquire().unwrap();
        let second = lease.acquire().unwrap();
        let writer = lease.clone();
        let (started, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            writer.retire();
        });
        receiver.recv().unwrap();
        while lease.state.lock().unwrap().valid {
            std::thread::yield_now();
        }
        assert!(lease.acquire().is_none());
        drop(first);
        assert!(!worker.is_finished());
        drop(second);
        worker.join().unwrap();
    }

    #[test]
    fn retirement_revokes_only_the_revocable_readers_that_outlive_the_wait() {
        let lease = BufferLease::new();
        let (revoke, revoked) = std::sync::mpsc::channel::<()>();
        let holder = Arc::new(Mutex::new(None));
        let owner = holder.clone();
        let guard = lease
            .acquire_revocable(move || {
                owner.lock().unwrap().take();
                revoke.send(()).unwrap();
            })
            .unwrap();
        *holder.lock().unwrap() = Some(guard);
        // A guard that is released in time takes its revoker with it.
        let (unused, never) = std::sync::mpsc::channel::<()>();
        drop(
            lease
                .acquire_revocable(move || unused.send(()).unwrap())
                .unwrap(),
        );
        lease.retire_within(Duration::ZERO);
        revoked.recv().unwrap();
        assert!(never.recv().is_err(), "a released guard was revoked");
        assert!(lease.try_retire());
        assert!(lease.acquire().is_none());
        assert!(lease.acquire_revocable(|| ()).is_none());
    }
}
