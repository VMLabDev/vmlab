//! The emulator's own CPU use on the host, beside the guest's figure in
//! `machine.stats`.
//!
//! The guest agent reports how busy the guest thinks its CPUs are; this is
//! what the machine costs the host — every vCPU thread plus QEMU's device
//! emulation — read from `/proc/<pid>/stat` twice over a fixed window. The
//! figure is a percentage of **one host core**, as `top` gives it, so a busy
//! machine with several vCPUs reads above 100.

use std::time::{Duration, Instant};

/// How long the host figure is measured over.
pub const WINDOW: Duration = Duration::from_secs(1);

/// The process's cumulative user + system time in clock ticks, from the text
/// of `/proc/<pid>/stat`. `None` when the text is not one.
///
/// The command name (field 2) is parenthesised and may hold spaces or
/// parentheses of its own, so the fields are counted from the *last* `)`:
/// after it comes the state (field 3), and `utime`/`stime` are fields 14 and
/// 15.
pub fn parse_stat_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Percent of one core used between two tick readings `elapsed` apart.
/// Zero over an empty window, and never negative: a counter that went
/// backwards is a different process, not negative work.
pub fn cpu_pct(prev_ticks: u64, cur_ticks: u64, elapsed: Duration, clk_tck: u64) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 || clk_tck == 0 {
        return 0.0;
    }
    let busy = cur_ticks.saturating_sub(prev_ticks) as f64 / clk_tck as f64;
    100.0 * busy / secs
}

/// The kernel's clock tick, which `/proc/<pid>/stat` counts in.
fn clk_tck() -> u64 {
    use nix::unistd::{SysconfVar, sysconf};
    match sysconf(SysconfVar::CLK_TCK) {
        Ok(Some(t)) if t > 0 => t as u64,
        // USER_HZ, on every architecture vmlab runs on.
        _ => 100,
    }
}

fn read_ticks(pid: u32) -> Option<u64> {
    parse_stat_ticks(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// The process's CPU use over [`WINDOW`], or `None` when it is not there to
/// read at either end of the window.
pub async fn sample(pid: u32) -> Option<f64> {
    let (t0, start) = (read_ticks(pid)?, Instant::now());
    tokio::time::sleep(WINDOW).await;
    let t1 = read_ticks(pid)?;
    Some(cpu_pct(t0, t1, start.elapsed(), clk_tck()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real line's shape, with a command name built to trip a naive split.
    const STAT: &str = "4242 (qemu-system-x86) (c1)) S 1 4242 4242 0 -1 4194560 \
        2939 0 0 0 1234 567 0 0 20 0 9 0 81723 2147483648 50000 \
        18446744073709551615 1 1 0 0 0 0 0 4096 0 0 0 17 3 0 0 0 0 0";

    #[test]
    fn ticks_are_utime_plus_stime_counted_past_the_command_name() {
        assert_eq!(parse_stat_ticks(STAT), Some(1234 + 567));
    }

    #[test]
    fn a_truncated_or_foreign_line_has_no_ticks() {
        assert_eq!(parse_stat_ticks(""), None);
        assert_eq!(parse_stat_ticks("4242 (qemu) S 1 2 3"), None);
        assert_eq!(parse_stat_ticks("not a stat line"), None);
    }

    #[test]
    fn percent_is_of_one_core_and_may_exceed_a_hundred() {
        let second = Duration::from_secs(1);
        assert_eq!(cpu_pct(1000, 1025, second, 100), 25.0);
        // Two vCPUs flat out for a second.
        assert_eq!(cpu_pct(1000, 1200, second, 100), 200.0);
        assert_eq!(cpu_pct(0, 50, Duration::from_millis(500), 100), 100.0);
        assert_eq!(cpu_pct(0, 250, second, 250), 100.0);
    }

    #[test]
    fn an_idle_empty_or_rewound_window_reads_zero() {
        assert_eq!(cpu_pct(500, 500, Duration::from_secs(1), 100), 0.0);
        assert_eq!(cpu_pct(0, 100, Duration::ZERO, 100), 0.0);
        assert_eq!(cpu_pct(900, 100, Duration::from_secs(1), 100), 0.0);
        assert_eq!(cpu_pct(0, 100, Duration::from_secs(1), 0), 0.0);
    }

    #[tokio::test]
    async fn this_process_samples_and_a_missing_one_does_not() {
        let pct = sample(std::process::id()).await.unwrap();
        assert!(pct >= 0.0);
        assert_eq!(sample(u32::MAX).await, None);
    }
}
