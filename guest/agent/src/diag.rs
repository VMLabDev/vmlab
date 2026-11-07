//! VML-106 diagnostics only (branch vml-106-diag, never merged): a
//! heartbeat on the console and timestamps for port events, to tell a
//! stalled agent from a stalled micro-VM after an online restore.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// `up=<CLOCK_BOOTTIME s> real=<unix ms>`.
pub fn now() -> String {
    let up = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .unwrap_or_default();
    let real = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    format!("up={up} real={real}")
}

/// Print a heartbeat each second, and a GAP line when a beat came late.
pub fn spawn_heartbeat(who: &'static str) {
    std::thread::spawn(move || {
        let mut last = Instant::now();
        let mut n: u64 = 0;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let gap = last.elapsed();
            last = Instant::now();
            n += 1;
            if gap > Duration::from_millis(2000) {
                eprintln!("{who}: diag GAP {} ms {}", gap.as_millis(), now());
            }
            if n % 5 == 0 {
                eprintln!("{who}: diag hb {}", now());
            }
        }
    });
}
