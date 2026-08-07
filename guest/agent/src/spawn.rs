//! The one seam every guest-side process and file handle is created through.
//!
//! The agent used to create processes in three unrelated places: the
//! interactive terminal, streaming exec, and the file push. Each did its own
//! creation, on each of the two guest targets. PRD §19.2 makes *who a
//! process runs as* a per-channel decision, so all three funnel here — one
//! call, taking an [`Identity`] and returning a running process.
//!
//! The returned handles are the seam's own types, not the platform's, so an
//! adapter can be entirely in-memory (ADR-0015, which follows ADR-0001).
//! Three adapters exist: `LinuxSpawner`, `WindowsSpawner`, and the
//! in-memory `FakeSpawner`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

/// Who a guest-side process runs as, and who owns the files it writes.
///
/// PRD §19.2 declares identity on the machine and resolves it per channel;
/// [`Identity::Agent`] is the only value the agent can produce today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    /// Whatever the agent itself already runs as: `LocalSystem` on Windows,
    /// `root` on a Linux VM, and in a container micro-VM the user cinit
    /// resolved. PRD §19.2's floor — spawn directly, with no logon.
    Agent,
}

/// What to run, and with what environment.
pub struct ProcessSpec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
}

/// A shell hosted on a terminal — a PTY on Linux, a ConPTY on Windows.
pub struct TerminalSpec {
    /// The shell argv, or `None` for whatever the guest offers.
    pub command: Option<Vec<String>>,
    pub cols: u16,
    pub rows: u16,
}

/// A running process, with its stdio wired and its control hooks captured.
///
/// Every field is seam-owned, so an adapter never has to produce a real
/// child (ADR-0015).
pub struct Spawned {
    /// Bytes into the process: the child's stdin, or the terminal's
    /// keystroke side. Dropping it closes that end.
    pub input: Box<dyn Write + Send>,
    /// Bytes out of the process: its stdout, or the terminal's whole VT
    /// stream.
    pub output: Box<dyn Read + Send>,
    /// Standard error, where the caller asked for separate pipes. A
    /// terminal has none — it multiplexes both streams onto `output`.
    pub errors: Option<Box<dyn Read + Send>>,
    /// Resize the hosting terminal. `None` for a process without one.
    pub resize: Option<Box<dyn Fn(u16, u16) + Send + Sync>>,
    /// Force-stop the process. Callers must not run it after the process
    /// has been reaped — pids and handles get recycled.
    pub kill: Box<dyn Fn() + Send + Sync>,
    /// Block until the process exits, release the terminal that hosted it,
    /// and report the exit code. Runs once.
    pub wait: Box<dyn FnOnce() -> i32 + Send>,
}

/// A file the seam opened for writing.
pub trait WriteFile: Write + Send {
    /// Apply the host-requested POSIX mode to the file. A no-op on Windows,
    /// where the wire's `mode` has no meaning.
    fn set_mode(&self, mode: u32);
}

/// Guest-side process and handle creation — the whole of it.
///
/// One implementation per guest target, plus the in-memory one the tests
/// substitute. Every method takes the identity the work runs as.
pub trait Spawner: Send + Sync {
    /// Start a shell on a terminal bridged to a channel.
    fn terminal(&self, identity: Identity, spec: TerminalSpec) -> std::io::Result<Spawned>;
    /// Start a process with piped stdio.
    fn exec(&self, identity: Identity, spec: ProcessSpec) -> std::io::Result<Spawned>;
    /// Create (truncating) a file for writing, making its parent
    /// directories first.
    fn create_file(&self, identity: Identity, path: &str) -> std::io::Result<Box<dyn WriteFile>>;
}

// ---- the agent's own identity ---------------------------------------------
//
// Both production adapters share these: spawning as the agent needs no
// platform-specific logon, which is exactly PRD §19.2's floor.

/// Spawn `spec` with piped stdio as the agent's own identity.
pub fn piped_command(spec: ProcessSpec) -> std::io::Result<Spawned> {
    if spec.argv.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty argv",
        ));
    }
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    for (k, v) in spec.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = spec.cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn()?;
    let input = child.stdin.take().expect("piped stdin");
    let output = child.stdout.take().expect("piped stdout");
    let errors = child.stderr.take().expect("piped stderr");
    // The kill hook works from the pid: the caller stops using it once the
    // reaper below has consumed the child.
    let pid = child.id();
    Ok(Spawned {
        input: Box::new(input),
        output: Box::new(output),
        errors: Some(Box::new(errors)),
        resize: None,
        kill: Box::new(move || crate::platform::kill_process(pid)),
        wait: Box::new(move || match child.wait() {
            Ok(status) => exit_code(status),
            Err(_) => 127,
        }),
    })
}

/// Create a file for writing as the agent's own identity, creating its
/// parent directories first.
pub fn create_file_as_agent(path: &str) -> std::io::Result<Box<dyn WriteFile>> {
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| std::io::Error::new(e.kind(), format!("mkdir: {e}")))?;
    }
    Ok(Box::new(PlainFile(std::fs::File::create(path)?)))
}

struct PlainFile(std::fs::File);

impl Write for PlainFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl WriteFile for PlainFile {
    #[cfg(unix)]
    fn set_mode(&self, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let _ = self
            .0
            .set_permissions(std::fs::Permissions::from_mode(mode));
    }

    #[cfg(windows)]
    fn set_mode(&self, _mode: u32) {}
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(127)
}

#[cfg(windows)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(127)
}
