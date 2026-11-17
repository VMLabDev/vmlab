//! SCM service plumbing: the template build installs the agent as the
//! `vmlab-agent` service (`sc.exe create … start= auto`). When launched by
//! the SCM this module runs the dispatcher; launched from a console (the
//! dispatcher fails with ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) it just
//! runs the agent in the foreground, so `vmlab-agent` in a terminal behaves
//! like `--console`. What it reports, and when, is decided in `crate::svc`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_sys::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, RegisterEventSourceW, ReportEventW,
};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerW, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
    StartServiceCtrlDispatcherW,
};

use super::port::wide;
use crate::svc::{self, AfterDispatch};

// The portable values are the SCM's own.
const _: () = assert!(svc::STOPPED == SERVICE_STOPPED);
const _: () = assert!(svc::STOP_PENDING == SERVICE_STOP_PENDING);
const _: () = assert!(svc::RUNNING == SERVICE_RUNNING);

const SERVICE_NAME: &str = "vmlab-agent";

/// How long the stop waits for the dispatcher to return on the main thread
/// after `STOPPED` before it ends the process itself.
const DISPATCHER_GRACE: Duration = Duration::from_secs(5);

/// The agent body to run (set before dispatching; service_main needs it
/// from a C entry point).
static RUN: OnceLock<fn() -> !> = OnceLock::new();
static STATUS: OnceLock<StatusHandle> = OnceLock::new();
/// Set by the first stop or shutdown control; later ones are ignored.
static STOPPING: AtomicBool = AtomicBool::new(false);

struct StatusHandle(SERVICE_STATUS_HANDLE);
// SAFETY: SetServiceStatus is callable from any thread.
unsafe impl Send for StatusHandle {}
unsafe impl Sync for StatusHandle {}

/// Run as a service if the SCM launched us; otherwise run `body` directly.
pub fn dispatch(body: fn() -> !) -> ! {
    let _ = RUN.set(body);
    let name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_ptr() as *mut u16,
            lpServiceProc: Some(service_main),
        },
        // Terminator.
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: valid, terminated service table; blocks for the service's life.
    let ok = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } != 0;
    let error = (!ok)
        .then(|| std::io::Error::last_os_error().raw_os_error())
        .flatten();
    match svc::after_dispatch(ok, error) {
        AfterDispatch::RunInConsole => body(),
        AfterDispatch::Exit(code) => std::process::exit(code),
        AfterDispatch::Failed(code) => {
            let msg = format!(
                "vmlab-agent: the service dispatcher failed: {}",
                std::io::Error::from_raw_os_error(code)
            );
            eprintln!("{msg}");
            log_error(&msg);
            std::process::exit(code);
        }
    }
}

/// Write `msg` to the Application event log, where a service's failure is
/// looked for: a service has no console to print to.
fn log_error(msg: &str) {
    let source = wide(SERVICE_NAME);
    let text = wide(msg);
    // SAFETY: registering a source by a NUL-terminated name on this machine.
    let log = unsafe { RegisterEventSourceW(std::ptr::null(), source.as_ptr()) };
    if log.is_null() {
        return;
    }
    let strings = [text.as_ptr()];
    // SAFETY: one NUL-terminated string, no SID and no raw data, on the
    // handle just registered and released right after.
    unsafe {
        ReportEventW(
            log,
            EVENTLOG_ERROR_TYPE,
            0,
            0,
            std::ptr::null_mut(),
            1,
            0,
            strings.as_ptr(),
            std::ptr::null(),
        );
        DeregisterEventSource(log);
    }
}

fn set_status(state: u32) {
    if let Some(h) = STATUS.get() {
        let report = svc::report(state, 0);
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: report.state,
            dwControlsAccepted: report.controls_accepted,
            dwWin32ExitCode: report.exit_code,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: report.checkpoint,
            dwWaitHint: report.wait_hint_ms,
        };
        // SAFETY: registered handle + filled struct.
        unsafe { SetServiceStatus(h.0, &status) };
    }
}

unsafe extern "system" fn handler(control: u32) {
    if (control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN)
        && !STOPPING.swap(true, Ordering::SeqCst)
    {
        // The handler must return promptly: report the stop under way, and
        // finish it on a thread of its own.
        set_status(SERVICE_STOP_PENDING);
        std::thread::spawn(stop);
    }
}

/// The rest of an ordered stop: `STOPPED` is the last thing the SCM hears,
/// which makes the dispatcher return on the main thread, and `dispatch`
/// exits from there. Should it not, the process ends anyway.
fn stop() {
    set_status(SERVICE_STOPPED);
    std::thread::sleep(DISPATCHER_GRACE);
    std::process::exit(0);
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    // SAFETY: registering our handler for our service name.
    let handle = unsafe { RegisterServiceCtrlHandlerW(wide(SERVICE_NAME).as_ptr(), Some(handler)) };
    if handle.is_null() {
        return;
    }
    let _ = STATUS.set(StatusHandle(handle));
    set_status(SERVICE_RUNNING);
    if let Some(body) = RUN.get() {
        body(); // never returns; a stop ends the process
    }
}
