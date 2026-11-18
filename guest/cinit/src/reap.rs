//! Child reaping for a threaded PID 1.
//!
//! Only the main thread calls `waitpid(-1)` (the reap loop); anything else
//! that forks (the healthcheck runner) subscribes here by pid and receives
//! its child's exit code through a channel. Without this, concurrent waiters
//! would race the reap loop for each other's statuses. Exits nobody has
//! subscribed to yet are parked in `unclaimed` so subscribe-after-exit still
//! works (the subscriber forks first and subscribes right after, so the
//! window is real). That window is a few instructions long, so only the
//! most recent [`MAX_UNCLAIMED`] parked exits are kept (F57): every orphan
//! the container re-parents to PID 1 lands here too, and none of those is
//! ever claimed.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use nix::sys::wait::WaitStatus;

/// The most parked exits kept; the oldest is dropped past it.
pub const MAX_UNCLAIMED: usize = 64;

#[derive(Default)]
struct Inner {
    subscribers: HashMap<i32, Sender<i32>>,
    /// Exits nobody subscribed to, oldest first. A pid reused by a later
    /// child replaces its older entry.
    unclaimed: VecDeque<(i32, i32)>,
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
        if let Some(at) = inner.unclaimed.iter().position(|&(p, _)| p == pid) {
            let (_, code) = inner.unclaimed.remove(at).unwrap();
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
                // A child nobody waits on yet (a subscriber between fork
                // and subscribe) or ever (a stale udhcpc, an orphan).
                inner.unclaimed.retain(|&(p, _)| p != pid);
                if inner.unclaimed.len() >= MAX_UNCLAIMED {
                    inner.unclaimed.pop_front();
                }
                inner.unclaimed.push_back((pid, code));
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

    /// F57: exits nobody claims do not pile up; the newest stay claimable.
    #[test]
    fn unclaimed_exits_are_bounded() {
        let r = Reaper::default();
        for pid in 0..10_000 {
            r.route(pid, 1);
        }
        assert_eq!(r.inner.lock().unwrap().unclaimed.len(), MAX_UNCLAIMED);
        let rx = r.subscribe(9_999);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
        let gone = r.subscribe(0);
        assert!(gone.recv_timeout(Duration::from_millis(10)).is_err());
    }

    /// A reused pid's newer exit wins over the parked one.
    #[test]
    fn a_reused_pid_keeps_its_newest_exit() {
        let r = Reaper::default();
        r.route(5, 1);
        r.route(5, 2);
        assert_eq!(r.inner.lock().unwrap().unclaimed.len(), 1);
        let rx = r.subscribe(5);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 2);
    }
}
