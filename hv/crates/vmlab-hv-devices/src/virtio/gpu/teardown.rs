//! The end of a device reset: the processes that served the guest's Venus
//! contexts have exited (`guest/win3d/docs/tdr-recovery.md`, "Backend
//! prerequisite").
//!
//! virglrenderer's render server runs each Venus context in a worker
//! process of its own, a child of the server. Destroying a context is a
//! message with no answer: the server kills the worker and reaps it later.
//! Until the worker has exited it can still run on the host GPU and reach
//! the guest memory it imported. A guest driver frees that memory when it
//! reads device status zero, so the device acknowledges a reset only after
//! [`Workers::wait`] has seen every worker end. An exited worker has closed
//! its render node, and the kernel driver ends the GPU work of a closed
//! node before that close completes.
//!
//! The workers are noted when a context is made, because a worker outlives
//! a render server that ends (a lost host GPU): it is then no longer the
//! server's child, and only its number and start time still name it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::lost::{Stat, children};

/// How long a reset waits for the workers of its contexts to exit. A
/// worker that is killed exits within milliseconds.
const WORKER_EXIT: Duration = Duration::from_secs(5);

/// How often [`Workers::wait`] looks.
const POLL: Duration = Duration::from_millis(1);

/// The render workers the guest's contexts made, until each has exited.
pub(super) struct Workers {
    proc: PathBuf,
    /// Number and start time of each.
    known: BTreeSet<(u32, u64)>,
    /// How long a reset waits for them.
    pub(super) patience: Duration,
}

impl Default for Workers {
    fn default() -> Self {
        Self::at(Path::new("/proc"), WORKER_EXIT)
    }
}

impl Workers {
    pub(super) fn at(proc: &Path, patience: Duration) -> Self {
        Self {
            proc: proc.to_path_buf(),
            known: BTreeSet::new(),
            patience,
        }
    }

    /// Note the running children of `servers`.
    pub(super) fn note(&mut self, servers: &[u32]) {
        let found = running(&self.proc, servers);
        self.add(found);
    }

    /// Note workers that [`running`] found. A context create looks for
    /// them without the device lock and notes them under it.
    pub(super) fn add(&mut self, found: Vec<(u32, u64)>) {
        self.known.extend(found);
    }

    /// Where [`running`] must look for this set.
    pub(super) fn proc(&self) -> &Path {
        &self.proc
    }

    /// Wait at most `timeout` until every noted worker has exited, and
    /// forget those that have. `Err` holds the numbers of those still
    /// running.
    pub(super) fn wait(&mut self, timeout: Duration) -> Result<(), Vec<u32>> {
        let start = Instant::now();
        loop {
            let proc = &self.proc;
            self.known.retain(|&(pid, started)| {
                Stat::read(proc, pid).is_some_and(|stat| stat.start == started && stat.alive())
            });
            if self.known.is_empty() {
                return Ok(());
            }
            if start.elapsed() >= timeout {
                return Err(self.known.iter().map(|&(pid, _)| pid).collect());
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Number and start time of each running child of `servers`, in `proc`.
///
/// This reads the `stat` file of every process of the host, which takes 5
/// to 17 ms on a host with a few thousand. It needs no device lock, and a
/// caller on the path of a vCPU must not hold one meanwhile.
pub(super) fn running(proc: &Path, servers: &[u32]) -> Vec<(u32, u64)> {
    let mut found = Vec::new();
    for &server in servers {
        for pid in children(proc, server) {
            if let Some(stat) = Stat::read(proc, pid) {
                found.push((pid, stat.start));
            }
        }
    }
    found
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in super::super) fn proc_entry(
        proc: &Path,
        pid: u32,
        state: char,
        parent: u32,
        start: u64,
    ) {
        let dir = proc.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("stat"),
            format!("{pid} (virgl_render_se) {state} {parent} 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0"),
        )
        .unwrap();
    }

    pub(in super::super) fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vmlab-gpu-teardown-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_worker_is_waited_for_until_it_exits_or_is_replaced() {
        let proc = scratch("wait");
        proc_entry(&proc, 10, 'S', 1, 100);
        proc_entry(&proc, 20, 'S', 10, 200);
        proc_entry(&proc, 21, 'S', 10, 210);
        proc_entry(&proc, 30, 'S', 1, 300);
        let mut workers = Workers::at(&proc, Duration::ZERO);
        workers.note(&[10]);
        assert_eq!(workers.wait(Duration::ZERO), Err(vec![20, 21]));
        // What a context create finds without the device lock is the same.
        assert_eq!(running(workers.proc(), &[10]), [(20, 200), (21, 210)]);
        workers.add(vec![(20, 200)]);
        assert_eq!(workers.wait(Duration::ZERO), Err(vec![20, 21]));

        // A zombie has exited; a number taken by a later process is not
        // the worker.
        proc_entry(&proc, 20, 'Z', 10, 200);
        proc_entry(&proc, 21, 'S', 1, 999);
        assert_eq!(workers.wait(Duration::ZERO), Ok(()));
        assert!(workers.known.is_empty());

        // A worker outlives its server: it is still waited for.
        proc_entry(&proc, 22, 'S', 10, 220);
        workers.note(&[10]);
        std::fs::remove_dir_all(proc.join("10")).unwrap();
        proc_entry(&proc, 22, 'S', 1, 220);
        assert_eq!(workers.wait(Duration::from_millis(20)), Err(vec![22]));
        let late = proc.clone();
        let exit = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::remove_dir_all(late.join("22")).unwrap();
        });
        let start = Instant::now();
        assert_eq!(workers.wait(Duration::from_secs(5)), Ok(()));
        assert!(start.elapsed() >= Duration::from_millis(50));
        exit.join().unwrap();
        std::fs::remove_dir_all(&proc).unwrap();
    }
}
