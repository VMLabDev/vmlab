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
use nix::unistd::{ForkResult, chdir, chroot, execve, fork};

use vmlab_cinit_proto::{CtlEvent, HealthSpec};

use crate::ctl::Ctl;
use crate::mounts::ROOTFS;
use crate::reap::Reaper;
use crate::util::Result;

/// Run one check: fork, chroot, exec, wait with timeout (via the reaper —
/// only the main thread reaps). Returns pass/fail.
fn run_check(
    reaper: &Reaper,
    c_exe: &CString,
    c_argv: &[CString],
    c_env: &[CString],
    timeout: Duration,
) -> bool {
    // SAFETY: same contract as the container fork — the child only makes
    // async-signal-safe calls before execve, with everything pre-allocated.
    let child = match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => child,
        Ok(ForkResult::Child) => {
            if chroot(ROOTFS).is_err() || chdir("/").is_err() {
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
            let _ = kill(child, Signal::SIGKILL);
            let _ = rx.recv_timeout(Duration::from_secs(5));
            false
        }
    }
}

/// Spawn the healthcheck thread. `exited` stops the loop once the container
/// is gone. The check command resolves inside the rootfs like the container
/// itself and runs with the container's environment.
pub fn spawn(
    spec: &HealthSpec,
    env: &[(String, String)],
    ctl: Arc<Ctl>,
    reaper: Arc<Reaper>,
    exited: Arc<AtomicBool>,
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
        thread::sleep(Duration::from_secs(spec.start_period_secs));
        let timeout = Duration::from_secs(spec.timeout_secs.max(1));
        let interval = Duration::from_secs(spec.interval_secs.max(1));
        let mut consecutive_fails: u32 = 0;
        let mut state: Option<bool> = None; // None until the first report
        while !exited.load(Ordering::SeqCst) {
            let pass = run_check(&reaper, &c_exe, &c_argv, &c_env, timeout);
            if exited.load(Ordering::SeqCst) {
                break;
            }
            if pass {
                consecutive_fails = 0;
                // First-ever pass and recovery-after-failure both land here.
                if state != Some(true) {
                    state = Some(true);
                    ctl.emit(&CtlEvent::Health { healthy: true });
                }
            } else {
                consecutive_fails += 1;
                if consecutive_fails >= spec.retries.max(1) && state != Some(false) {
                    state = Some(false);
                    ctl.emit(&CtlEvent::Health { healthy: false });
                }
            }
            thread::sleep(interval);
        }
    });
    Ok(())
}
