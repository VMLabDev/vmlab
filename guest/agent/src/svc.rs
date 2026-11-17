//! What the Windows service tells the service control manager, as values:
//! portable like `rdp`, so the build host's tests reach it, and compiled
//! only where it is used.
//!
//! A stop is ordered: the agent reports `STOP_PENDING` and stops accepting
//! controls, then reports `STOPPED` as its last word, and the process exits
//! from the main thread once the dispatcher returns. A dispatcher that fails
//! for any reason but a console launch is a failure the exit code says.

/// The SCM's service states (`winsvc.h`).
pub const STOPPED: u32 = 1;
pub const STOP_PENDING: u32 = 3;
pub const RUNNING: u32 = 4;

/// The controls a running agent accepts: stop and shutdown.
pub const ACCEPT_STOP: u32 = 1;
pub const ACCEPT_SHUTDOWN: u32 = 4;

/// `StartServiceCtrlDispatcherW`'s error when the process was not started
/// by the SCM: a console launch.
pub const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;

/// How long a stop may take before the SCM considers the service hung, as
/// the wait hint `STOP_PENDING` carries.
pub const STOP_WAIT_HINT_MS: u32 = 5_000;

/// One `SERVICE_STATUS`, less the service type, which never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub state: u32,
    pub controls_accepted: u32,
    pub exit_code: u32,
    pub checkpoint: u32,
    pub wait_hint_ms: u32,
}

/// What to report in `state`. Only a running service accepts controls: a
/// second stop while one is under way has nothing left to do.
pub fn report(state: u32, exit_code: u32) -> Report {
    let pending = state == STOP_PENDING;
    Report {
        state,
        controls_accepted: match state {
            RUNNING => ACCEPT_STOP | ACCEPT_SHUTDOWN,
            _ => 0,
        },
        exit_code,
        checkpoint: u32::from(pending),
        wait_hint_ms: if pending { STOP_WAIT_HINT_MS } else { 0 },
    }
}

/// What the process does once `StartServiceCtrlDispatcherW` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterDispatch {
    /// The service ran and stopped: exit with this code.
    Exit(i32),
    /// Not started by the SCM: run the agent in this console.
    RunInConsole,
    /// The dispatcher failed: say so, and exit non-zero with this code.
    Failed(i32),
}

/// Read the dispatcher's result: `ok` is its return value, `error` the
/// thread's last error when it failed.
pub fn after_dispatch(ok: bool, error: Option<i32>) -> AfterDispatch {
    match (ok, error) {
        (true, _) => AfterDispatch::Exit(0),
        (false, Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT)) => AfterDispatch::RunInConsole,
        (false, Some(code)) if code != 0 => AfterDispatch::Failed(code),
        (false, _) => AfterDispatch::Failed(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stop is reported in order: pending, with a wait hint and no
    /// controls, then stopped, accepting nothing. Running accepts both.
    #[test]
    fn a_stop_reports_pending_before_stopped_and_accepts_nothing_meanwhile() {
        assert_eq!(
            report(RUNNING, 0).controls_accepted,
            ACCEPT_STOP | ACCEPT_SHUTDOWN
        );
        let pending = report(STOP_PENDING, 0);
        assert_eq!(pending.controls_accepted, 0);
        assert!(pending.wait_hint_ms > 0 && pending.checkpoint > 0);
        let stopped = report(STOPPED, 0);
        assert_eq!(stopped.controls_accepted, 0);
        assert_eq!((stopped.wait_hint_ms, stopped.checkpoint), (0, 0));
    }

    /// Only a console launch falls back to running in the foreground; any
    /// other dispatcher failure exits non-zero instead of claiming success.
    #[test]
    fn a_dispatcher_failure_is_a_non_zero_exit() {
        assert_eq!(after_dispatch(true, None), AfterDispatch::Exit(0));
        assert_eq!(
            after_dispatch(false, Some(1063)),
            AfterDispatch::RunInConsole
        );
        assert_eq!(after_dispatch(false, Some(5)), AfterDispatch::Failed(5));
        assert_eq!(after_dispatch(false, Some(0)), AfterDispatch::Failed(1));
        assert_eq!(after_dispatch(false, None), AfterDispatch::Failed(1));
    }
}
