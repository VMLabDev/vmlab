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
pub mod sysinfo;

pub use conpty::kill_process;
pub use metrics::{cpu_pct, cpu_sample, disk_sample, mem_sample};
pub use port::open_port;

use vmlab_agent_proto::{NetInterface, OsInfo, ShutdownMode, features};

use crate::mux::Mux;
use crate::spawn::{
    Identity, ProcessSpec, Spawned, Spawner, TerminalSpec, WriteFile, create_file_as_agent,
    piped_command,
};

pub struct WindowsPlatform {
    spawner: WindowsSpawner,
}

/// The Windows half of the process/handle seam: ConPTY terminals, piped
/// exec, and the file writes behind `push`.
pub struct WindowsSpawner;

pub fn new_platform() -> WindowsPlatform {
    WindowsPlatform {
        spawner: WindowsSpawner,
    }
}

impl Spawner for WindowsSpawner {
    fn terminal(&self, _identity: Identity, spec: TerminalSpec) -> std::io::Result<Spawned> {
        conpty::spawn(spec)
    }

    fn exec(&self, _identity: Identity, spec: ProcessSpec) -> std::io::Result<Spawned> {
        piped_command(spec)
    }

    fn create_file(&self, _identity: Identity, path: &str) -> std::io::Result<Box<dyn WriteFile>> {
        create_file_as_agent(path)
    }
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
            features::TUNNEL.to_string(),
        ]
    }

    fn spawner(&self) -> &dyn Spawner {
        &self.spawner
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

    fn net_info(&self) -> Result<Vec<NetInterface>, String> {
        sysinfo::net_info()
    }

    fn os_info(&self) -> Result<OsInfo, String> {
        sysinfo::os_info()
    }

    fn shutdown(&self, mux: &Mux, mode: ShutdownMode) {
        let mux = mux.clone();
        std::thread::spawn(move || {
            // Let the ShuttingDown ack drain to the host first.
            std::thread::sleep(std::time::Duration::from_millis(200));
            if let Err(e) = sysinfo::shutdown(mode) {
                mux.send_error(None, format!("shutdown: {e}"));
            }
        });
    }
}
