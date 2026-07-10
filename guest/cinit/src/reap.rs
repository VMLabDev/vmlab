//! Child reaping for a threaded PID 1.
//!
//! Only the main thread calls `waitpid(-1)` (the reap loop); anything else
//! that forks (the healthcheck runner) subscribes here by pid and receives
//! its child's exit code through a channel. Without this, concurrent waiters
//! would race the reap loop for each other's statuses. Exits nobody has
//! subscribed to yet are parked in `unclaimed` so subscribe-after-exit still
//! works (the subscriber forks first and subscribes right after, so the
//! window is real).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use nix::sys::wait::WaitStatus;

#[derive(Default)]
struct Inner {
    subscribers: HashMap<i32, Sender<i32>>,
    unclaimed: HashMap<i32, i32>,
}

#[derive(Default)]
pub struct Reaper {
    inner: Mutex<Inner>,
}

impl Reaper {
    /// Register interest in `pid`'s exit code. Call after fork; an exit that
    /// already happened is delivered immediately.
    pub fn subscribe(&self, pid: i32) -> Receiver<i32> {
        let (tx, rx) = channel();
        let mut inner = self.inner.lock().unwrap();
        if let Some(code) = inner.unclaimed.remove(&pid) {
            let _ = tx.send(code);
        } else {
            inner.subscribers.insert(pid, tx);
        }
        rx
    }

    /// Route one reaped exit (called by the main reap loop for every child
    /// that is not the container itself).
    pub fn route(&self, pid: i32, code: i32) {
        let mut inner = self.inner.lock().unwrap();
        match inner.subscribers.remove(&pid) {
            Some(tx) => {
                let _ = tx.send(code);
            }
            None => {
                // A child nobody waits on (qemu-ga, a stale udhcpc). Parked
                // entries are bounded by how few processes a micro-VM runs.
                inner.unclaimed.insert(pid, code);
            }
        }
    }
}

/// Exit code from a wait status, unix-style: `128 + signal` for signal
/// deaths. `None` for stop/continue notifications.
pub fn exit_code(status: &WaitStatus) -> Option<(i32, i32)> {
    match *status {
        WaitStatus::Exited(pid, code) => Some((pid.as_raw(), code)),
        WaitStatus::Signaled(pid, sig, _) => Some((pid.as_raw(), 128 + sig as i32)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn routes_to_subscriber() {
        let r = Reaper::default();
        let rx = r.subscribe(7);
        r.route(7, 3);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 3);
    }

    #[test]
    fn exit_before_subscribe_is_parked() {
        let r = Reaper::default();
        r.route(9, 0);
        let rx = r.subscribe(9);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
    }
}
