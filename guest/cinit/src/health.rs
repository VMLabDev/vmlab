//! Container healthcheck loop, mirroring OCI/compose semantics: after
//! `start_period_secs`, run `command` inside the container root every
//! `interval_secs` with a `timeout_secs` kill-timeout; `retries` consecutive
//! failures flip the state to unhealthy, any pass flips it (back) to healthy.
//! Only state *changes* are emitted (the first-ever pass included).

use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::{ForkResult, Pid, chroot, execve, fork, setsid};

use vmlab_cinit_proto::{CtlEvent, HealthSpec};

use crate::container::{ExecIdentity, Namespaces};
use crate::ctl::Ctl;
use crate::mounts::ROOTFS;
use crate::reap::Reaper;
use crate::util::Result;

struct HealthState {
    retries: u32,
    consecutive_fails: u32,
    verdict: Option<bool>,
}

impl HealthState {
    fn new(retries: u32) -> Self {
        Self {
            retries: retries.max(1),
            consecutive_fails: 0,
            verdict: None,
        }
    }

    fn on_result(&mut self, pass: bool, _in_start_period: bool) -> Option<bool> {
        let verdict = if pass {
            self.consecutive_fails = 0;
            true
        } else {
            self.consecutive_fails = self.consecutive_fails.saturating_add(1);
            if self.consecutive_fails < self.retries {
                return None;
            }
            false
        };
        (self.verdict.replace(verdict) != Some(verdict)).then_some(verdict)
    }
}

fn first_check_delay(spec: &HealthSpec) -> Duration {
    Duration::from_secs(spec.start_period_secs)
}

/// Run one check: fork, chroot, exec, wait with timeout (via the reaper —
/// only the main thread reaps). Returns pass/fail.
fn run_check(
    reaper: &Reaper,
    c_exe: &CString,
    c_argv: &[CString],
    c_env: &[CString],
    timeout: Duration,
    namespaces: &Namespaces,
    identity: &ExecIdentity,
) -> bool {
    // SAFETY: same contract as the container fork — the child only makes
    // async-signal-safe calls before execve, with everything pre-allocated.
    let child = match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => child,
        Ok(ForkResult::Child) => {
            // A process group lets the timeout path kill both this supervisor
            // and the check created after PID-namespace entry.
            if setsid().is_err() || namespaces.enter_for_child().is_err() {
                unsafe { libc::_exit(126) }
            }
            match unsafe { fork() } {
                Ok(ForkResult::Parent { child }) => mirror_child_exit(child),
                Ok(ForkResult::Child) => {}
                Err(_) => unsafe { libc::_exit(126) },
            }
            // As the workload runs (F58): its workdir, then its user.
            if chroot(ROOTFS).is_err() || identity.apply().is_err() {
                unsafe { libc::_exit(126) }
            }
            let _ = execve(c_exe, c_argv, c_env);
            unsafe { libc::_exit(127) }
        }
        Err(e) => {
            eprintln!("vmlab-cinit: warning: healthcheck fork failed: {e}");
            return false;
        }
    };

    let rx = reaper.subscribe(child.as_raw());
    match rx.recv_timeout(timeout) {
        Ok(code) => code == 0,
        Err(_) => {
            // Timed out: kill and wait for the reap loop to deliver the exit.
            let _ = kill(Pid::from_raw(-child.as_raw()), Signal::SIGKILL);
            let _ = rx.recv_timeout(Duration::from_secs(5));
            false
        }
    }
}

/// Spawn the healthcheck thread. `exited` stops the loop once the container
/// is gone. The check command resolves inside the rootfs like the container
/// itself and runs with the container's environment, user and workdir.
pub fn spawn(
    spec: &HealthSpec,
    env: &[(String, String)],
    identity: ExecIdentity,
    ctl: Arc<Ctl>,
    reaper: Arc<Reaper>,
    exited: Arc<AtomicBool>,
    namespaces: Namespaces,
) -> Result<()> {
    if spec.command.is_empty() {
        return Err("healthcheck with empty command".into());
    }
    let exe = crate::container::resolve_exe(ROOTFS, &spec.command[0], env)?;
    let c_exe = CString::new(exe).map_err(|e| e.to_string())?;
    let c_argv: Vec<CString> = spec
        .command
        .iter()
        .map(|a| CString::new(a.as_str()).map_err(|e| e.to_string()))
        .collect::<Result<_>>()?;
    let c_env: Vec<CString> = env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")).map_err(|e| e.to_string()))
        .collect::<Result<_>>()?;

    let spec = spec.clone();
    thread::spawn(move || {
        thread::sleep(first_check_delay(&spec));
        let timeout = Duration::from_secs(spec.timeout_secs.max(1));
        let interval = Duration::from_secs(spec.interval_secs.max(1));
        let mut state = HealthState::new(spec.retries);
        while !exited.load(Ordering::SeqCst) {
            let pass = run_check(
                &reaper,
                &c_exe,
                &c_argv,
                &c_env,
                timeout,
                &namespaces,
                &identity,
            );
            if exited.load(Ordering::SeqCst) {
                break;
            }
            if let Some(healthy) = state.on_result(pass, false) {
                ctl.emit(&CtlEvent::Health { healthy });
            }
            thread::sleep(interval);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_inside_the_start_period_reports_healthy() {
        let mut state = HealthState::new(2);
        assert_eq!(state.on_result(true, true), Some(true));
        assert_eq!(state.on_result(true, true), None);
        assert_eq!(state.on_result(false, true), None);
        assert_eq!(state.on_result(false, true), Some(false));
    }

    #[test]
    fn a_failure_inside_the_start_period_is_not_counted() {
        let mut state = HealthState::new(2);
        assert_eq!(state.on_result(false, true), None);
        assert_eq!(state.on_result(false, true), None);
        assert_eq!(state.on_result(false, false), None);
        assert_eq!(state.on_result(false, false), Some(false));
    }

    #[test]
    fn the_first_check_does_not_wait_for_the_start_period() {
        let mut spec = HealthSpec {
            command: vec!["/check".into()],
            interval_secs: 1,
            timeout_secs: 1,
            retries: 2,
            start_period_secs: 0,
        };
        let first = first_check_delay(&spec);
        spec.start_period_secs = 120;
        assert_eq!(first_check_delay(&spec), first);
    }
}

fn mirror_child_exit(child: Pid) -> ! {
    let mut status = 0;
    loop {
        let rc = unsafe { libc::waitpid(child.as_raw(), &mut status, 0) };
        if rc >= 0 {
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                128 + libc::WTERMSIG(status)
            } else {
                continue;
            };
            unsafe { libc::_exit(code) }
        }
        if nix::errno::Errno::last() != nix::errno::Errno::EINTR {
            unsafe { libc::_exit(127) }
        }
    }
}
