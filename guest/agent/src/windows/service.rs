//! SCM service plumbing: the template build installs the agent as the
//! `vmlab-agent` service (`sc.exe create … start= auto`). When launched by
//! the SCM this module runs the dispatcher; launched from a console (the
//! dispatcher fails with ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) it just
//! runs the agent in the foreground, so `vmlab-agent` in a terminal behaves
//! like `--console`.

use std::sync::OnceLock;

use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerW, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};

use super::port::wide;

const SERVICE_NAME: &str = "vmlab-agent";
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;
const SERVICE_ACCEPT_STOP: u32 = 1;
const SERVICE_ACCEPT_SHUTDOWN: u32 = 4;

/// The agent body to run (set before dispatching; service_main needs it
/// from a C entry point).
static RUN: OnceLock<fn() -> !> = OnceLock::new();
static STATUS: OnceLock<StatusHandle> = OnceLock::new();

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
    let ok = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    if ok == 0
        && std::io::Error::last_os_error().raw_os_error()
            == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT)
    {
        // Console launch.
        body();
    }
    std::process::exit(0);
}

fn set_status(state: u32) {
    if let Some(h) = STATUS.get() {
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
            dwWin32ExitCode: 0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: 0,
        };
        // SAFETY: registered handle + filled struct.
        unsafe { SetServiceStatus(h.0, &status) };
    }
}

unsafe extern "system" fn handler(control: u32) {
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        set_status(SERVICE_STOPPED);
        std::process::exit(0);
    }
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
        body(); // never returns; the STOP handler exits the process
    }
}
