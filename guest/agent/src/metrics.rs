//! Periodic metrics: one sampler thread, started on the first subscribe,
//! parked while unsubscribed. CPU utilisation needs two samples, so the
//! thread keeps the previous counters between ticks.

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
}

static STATE: OnceLock<(Mutex<Sub>, Condvar)> = OnceLock::new();

fn state() -> &'static (Mutex<Sub>, Condvar) {
    STATE.get_or_init(|| {
        thread::spawn(sampler);
        (
            Mutex::new(Sub {
                interval: None,
                mux: None,
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
    cv.notify_all();
}

pub fn unsubscribe() {
    let (lock, cv) = state();
    let mut sub = lock.lock().unwrap();
    sub.interval = None;
    sub.mux = None;
    cv.notify_all();
}

fn sampler() {
    let mut prev_cpu = platform::cpu_sample();
    loop {
        let (interval, mux) = {
            let (lock, cv) = state();
            let mut sub = lock.lock().unwrap();
            while sub.interval.is_none() {
                sub = cv.wait(sub).unwrap();
                prev_cpu = platform::cpu_sample(); // fresh baseline on resume
            }
            (sub.interval.unwrap(), sub.mux.clone().unwrap())
        };

        let cur = platform::cpu_sample();
        let cpu_pct = platform::cpu_pct(&prev_cpu, &cur);
        prev_cpu = cur;
        let (mem_used, mem_total) = platform::mem_sample();
        mux.send_ctrl(&AgentMsg::Metrics {
            cpu_pct,
            mem_used,
            mem_total,
            disks: platform::disk_sample(),
        });

        // Sleep the interval, but wake early on re/unsubscribe.
        let (lock, cv) = state();
        let sub = lock.lock().unwrap();
        let _ = cv.wait_timeout(sub, interval).unwrap();
    }
}
