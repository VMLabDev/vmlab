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
    use super::baseline;

    #[test]
    fn a_fresh_subscription_reports_nothing_until_it_has_a_baseline() {
        assert_eq!(baseline::<u64>(None, false), None);
        assert_eq!(baseline(Some(7u64), true), None);
        assert_eq!(baseline(Some(7u64), false), Some(7));
    }
}
