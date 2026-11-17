//! Periodic metrics: one sampler thread, started on the first subscribe,
//! parked while unsubscribed. CPU utilisation needs two samples, so the
//! thread keeps the previous counters between ticks — and a subscription's
//! first tick only takes that baseline, reporting one interval later, so no
//! sample ever covers a window too short to have counted anything (#185).

use std::sync::{Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use vmlab_agent_proto::AgentMsg;

use crate::mux::Mux;
use crate::platform;

struct Sub {
    /// `None` = unsubscribed (thread parks).
    interval: Option<Duration>,
    mux: Option<Mux>,
    /// A subscribe the sampler has not seen yet: its next tick starts a
    /// fresh baseline rather than measuring against an old one.
    fresh: bool,
}

static STATE: OnceLock<(Mutex<Sub>, Condvar)> = OnceLock::new();

fn state() -> &'static (Mutex<Sub>, Condvar) {
    STATE.get_or_init(|| {
        thread::spawn(sampler);
        (
            Mutex::new(Sub {
                interval: None,
                mux: None,
                fresh: false,
            }),
            Condvar::new(),
        )
    })
}

pub fn subscribe(mux: &Mux, interval_secs: u64) {
    let (lock, cv) = state();
    let mut sub = lock.lock().unwrap();
    sub.interval = Some(Duration::from_secs(interval_secs.clamp(1, 3600)));
    sub.mux = Some(mux.clone());
    sub.fresh = true;
    cv.notify_all();
}

pub fn unsubscribe() {
    let (lock, cv) = state();
    let mut sub = lock.lock().unwrap();
    sub.interval = None;
    sub.mux = None;
    cv.notify_all();
}

/// Drop the subscription `mux`'s port holds, if it holds the current one:
/// a host re-handshake ends every session of the previous host, and its
/// samples with them.
pub fn release(mux: &Mux) {
    let Some((lock, cv)) = STATE.get() else {
        return; // nothing ever subscribed
    };
    if release_from(&mut lock.lock().unwrap(), mux) {
        cv.notify_all();
    }
}

/// Clear `sub` if it sends to `mux`'s port; whether it did.
fn release_from(sub: &mut Sub, mux: &Mux) -> bool {
    if !sub.mux.as_ref().is_some_and(|m| m.same_port(mux)) {
        return false;
    }
    sub.interval = None;
    sub.mux = None;
    true
}

/// The counters this tick measures against: none on a fresh subscription,
/// whose tick only records the baseline the next one reports from.
fn baseline<T>(prev: Option<T>, fresh: bool) -> Option<T> {
    if fresh { None } else { prev }
}

fn sampler() {
    let mut prev_cpu = None;
    loop {
        let (interval, mux, fresh) = {
            let (lock, cv) = state();
            let mut sub = lock.lock().unwrap();
            while sub.interval.is_none() {
                sub = cv.wait(sub).unwrap();
            }
            let fresh = std::mem::take(&mut sub.fresh);
            (sub.interval.unwrap(), sub.mux.clone().unwrap(), fresh)
        };

        let cur = platform::cpu_sample();
        if let Some(prev) = baseline(prev_cpu.replace(cur), fresh) {
            let (mem_used, mem_total) = platform::mem_sample();
            mux.send_ctrl(&AgentMsg::Metrics {
                cpu_pct: platform::cpu_pct(&prev, &cur),
                mem_used,
                mem_total,
                disks: platform::disk_sample(),
            });
        }

        // Sleep the interval, but wake early on re/unsubscribe.
        let (lock, cv) = state();
        let sub = lock.lock().unwrap();
        let _ = cv.wait_timeout(sub, interval).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::{Sub, baseline, release_from};
    use crate::testutil::capture_mux;
    use std::time::Duration;

    /// A re-handshake on the subscribing port ends the subscription; one on
    /// another port leaves it alone.
    #[test]
    fn a_rehandshake_releases_only_its_own_ports_subscription() {
        let (ours, _cap) = capture_mux();
        let (other, _other_cap) = capture_mux();
        let mut sub = Sub {
            interval: Some(Duration::from_secs(5)),
            mux: Some(ours.clone()),
            fresh: false,
        };
        assert!(!release_from(&mut sub, &other));
        assert!(sub.interval.is_some());
        assert!(release_from(&mut sub, &ours.for_channel(3)));
        assert!(sub.interval.is_none() && sub.mux.is_none());
    }

    #[test]
    fn a_fresh_subscription_reports_nothing_until_it_has_a_baseline() {
        assert_eq!(baseline::<u64>(None, false), None);
        assert_eq!(baseline(Some(7u64), true), None);
        assert_eq!(baseline(Some(7u64), false), Some(7));
    }
}
