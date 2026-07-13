//! Windows platform half: vioserial port I/O (OVERLAPPED, shared exclusive
//! handle), ConPTY-hosted PowerShell terminals, EvtSubscribe event-log
//! tailing, GetSystemTimes/GlobalMemoryStatusEx metrics, the user-session
//! clipboard helper, and the SCM service entry.

pub mod clipboard;
pub mod conpty;
pub mod eventlog;
pub mod metrics;
pub mod port;
pub mod service;

pub use conpty::kill_process;
pub use metrics::{cpu_pct, cpu_sample, disk_sample, mem_sample};
pub use port::open_port;

use vmlab_agent_proto::features;

use crate::mux::Mux;

pub struct WindowsPlatform;

pub fn new_platform() -> WindowsPlatform {
    WindowsPlatform
}

impl WindowsPlatform {
    /// Start the background clipboard manager (pipe server + helper
    /// spawner). Called once the mux exists.
    pub fn start_clipboard(&self, mux: &Mux) {
        clipboard::start(mux);
    }
}

impl crate::mux::Platform for WindowsPlatform {
    fn os(&self) -> &'static str {
        "windows"
    }

    fn features(&self) -> Vec<String> {
        // Clipboard is advertised unconditionally: whether it works depends
        // on a user being logged on *right now*, which can change during the
        // agent's life — calls answer with a clear error when nobody is.
        vec![
            features::TERMINAL.to_string(),
            features::EXEC.to_string(),
            features::FILE.to_string(),
            features::TAIL.to_string(),
            features::METRICS.to_string(),
            features::EVENTLOG.to_string(),
            features::CLIPBOARD.to_string(),
        ]
    }

    fn open_terminal(
        &self,
        mux: &Mux,
        id: u32,
        cols: u16,
        rows: u16,
        command: Option<Vec<String>>,
    ) {
        conpty::open_terminal(mux, id, cols, rows, command);
    }

    fn open_eventlog(&self, mux: &Mux, id: u32, filter: Option<String>) {
        eventlog::open(mux, id, filter);
    }

    fn set_clipboard(&self, mux: &Mux, text: String) {
        clipboard::set(mux, text);
    }

    fn get_clipboard(&self, mux: &Mux) {
        clipboard::get(mux);
    }
}
