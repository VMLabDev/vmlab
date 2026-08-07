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

use vmlab_agent_proto::Logon;

/// Who a guest-side process runs as, and who owns the files it writes.
///
/// PRD §19.2 declares identity on the machine and the host resolves it per
/// channel; the agent only ever receives the answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// Whatever the agent itself already runs as: `LocalSystem` on Windows,
    /// `root` on a Linux VM, and in a container micro-VM the user cinit
    /// resolved. PRD §19.2's floor — spawn directly, with no logon.
    Agent,
    /// A declared account the agent logs on as. The triple arrives on every
    /// open that carries one (§19.5), so the agent's cache is an internal
    /// optimisation and a host re-handshake costs nothing.
    Declared(Logon),
}

impl From<Option<Logon>> for Identity {
    /// The wire's optional `logon`: absent is the agent identity.
    fn from(logon: Option<Logon>) -> Identity {
        match logon {
            Some(logon) => Identity::Declared(logon),
            None => Identity::Agent,
        }
    }
}

/// What to run, and with what environment.
#[derive(Debug, PartialEq, Eq)]
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

/// An identity lent to the calling thread, released when dropped.
///
/// Reads are the one shape the seam cannot hand a handle back for: `tail`
/// reopens its file across rotation, so a single opened handle would not
/// carry the identity far enough. The seam lends the identity to the thread
/// instead — Windows impersonates the logon, and every open the thread makes
/// while the guard lives is that user's.
pub trait Adopted {}

/// Resolves an identity once, then lends it to whichever thread asks.
///
/// Two steps rather than one because the two failures are different: the
/// logon is minted when the adopter is built, so a missing account or a
/// wrong secret fails the *open* loudly (§19.2), while adopting is a
/// per-thread call a session makes wherever its reads happen.
pub type Adopter = Box<dyn Fn() -> std::io::Result<Box<dyn Adopted>> + Send + Sync>;

/// Guest-side process and handle creation — the whole of it.
///
/// One implementation per guest target, plus the in-memory one the tests
/// substitute. Every method takes the identity the work runs as.
pub trait Spawner: Send + Sync {
    /// Start a shell on a terminal bridged to a channel.
    fn terminal(&self, identity: &Identity, spec: TerminalSpec) -> std::io::Result<Spawned>;
    /// Start a process with piped stdio.
    fn exec(&self, identity: &Identity, spec: ProcessSpec) -> std::io::Result<Spawned>;
    /// Create (truncating) a file for writing, making its parent
    /// directories first.
    fn create_file(&self, identity: &Identity, path: &str) -> std::io::Result<Box<dyn WriteFile>>;
    /// Resolve `identity` into something a session thread can adopt for the
    /// reads it makes. See [`Adopted`] for why reads take this shape.
    fn adopter(&self, identity: &Identity) -> std::io::Result<Adopter>;
}

/// The guard for [`Identity::Agent`], on every target: the agent is already
/// itself, so there is nothing to adopt and nothing to release.
pub struct AlreadyMe;
impl Adopted for AlreadyMe {}

/// The adopter for [`Identity::Agent`] — hands out [`AlreadyMe`] forever.
pub fn adopt_as_agent() -> Adopter {
    Box::new(|| Ok(Box::new(AlreadyMe) as Box<dyn Adopted>))
}

/// Keep `held` alive until the spawned process ends.
///
/// The logon cache drops a logon no channel holds, and *no channel holds it*
/// has to mean the session has ended — not that the spawn call returned.
/// Without this the sweeper would unload the profile and close the token out
/// from under a running shell after the idle grace, which reads as the shell
/// losing its home directory and its mapped drives for no visible reason
/// (PRD §19.2).
///
/// It hangs off `wait` rather than off a new [`Spawned`] field because
/// `wait` is exactly "the process has ended, and its resources are
/// released": one more resource joins the ones already released there.
pub fn hold_until_it_exits<T: Send + Sync + 'static>(
    mut spawned: Spawned,
    held: Option<std::sync::Arc<T>>,
) -> Spawned {
    let Some(held) = held else {
        return spawned;
    };
    let wait = spawned.wait;
    spawned.wait = Box::new(move || {
        let code = wait();
        drop(held);
        code
    });
    spawned
}

/// An argv rendered as one Windows command line.
///
/// Windows hands a process a single string and lets it parse its own argv,
/// so the caller does the quoting: a run of backslashes is doubled only
/// where it precedes a quote (or the closing quote), which is the rule
/// `CommandLineToArgvW` inverts. Both Windows process shapes need this —
/// the ConPTY shell and the exec that `CreateProcessAsUserW` starts — so it
/// lives here rather than being written twice.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn command_line(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            out.push_str(arg);
            continue;
        }
        out.push('"');
        let mut backslashes = 0;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    backslashes = 0;
                    out.push('"');
                }
                _ => {
                    out.extend(std::iter::repeat_n('\\', backslashes));
                    backslashes = 0;
                    out.push(ch);
                }
            }
        }
        out.extend(std::iter::repeat_n('\\', backslashes * 2));
        out.push('"');
    }
    out
}

// ---- creation with whatever identity the caller already has ---------------
//
// Both production adapters share these. For the agent identity they are the
// whole of PRD §19.2's floor — no platform-specific logon at all. Windows
// also reaches for `create_file_directly` under impersonation, where "the
// identity the calling thread has" is a declared logon.

/// Spawn `spec` with piped stdio, with one hook into the `Command` before it
/// is spawned.
///
/// The hook exists because the Linux adapter drops to a declared login's
/// credentials in a `pre_exec`, which is the one thing about the spawn that is
/// not `ProcessSpec`-shaped — while the rest of the plumbing (pipes, the kill
/// hook, the reaper) is identical, so it stays in one place rather than being
/// written twice. `|_| {}` is the agent's own identity, on both targets.
///
/// The hook runs **before** the spec's environment is applied, so an adapter
/// can clear what the agent itself is holding without losing what the caller
/// asked for — which is what a login has to do.
pub fn piped_command(
    spec: ProcessSpec,
    prepare: impl FnOnce(&mut Command),
) -> std::io::Result<Spawned> {
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
    prepare(&mut cmd);
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

/// Create a file for writing with whatever identity the calling thread
/// already has, creating its parent directories first.
pub fn create_file_directly(path: &str) -> std::io::Result<Box<dyn WriteFile>> {
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| std::io::Error::new(e.kind(), format!("mkdir: {e}")))?;
    }
    Ok(Box::new(create_plain_file(path)?))
}

/// The file itself, without the parent directories or the boxing — for an
/// adapter that has something to do to the handle before it hands it over.
/// The Linux one chowns it to the session that will own the file (§19.2).
pub fn create_plain_file(path: &str) -> std::io::Result<PlainFile> {
    Ok(PlainFile(std::fs::File::create(path)?))
}

pub struct PlainFile(std::fs::File);

impl PlainFile {
    /// The handle itself, so the Linux adapter can chown it to the session
    /// that will own the file. Windows takes the identity from the thread
    /// that created it and has nothing to do here.
    #[cfg(unix)]
    pub fn as_file(&self) -> &std::fs::File {
        &self.0
    }
}

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

#[cfg(test)]
mod tests {
    use super::{Spawned, command_line, hold_until_it_exits};
    use std::sync::Arc;

    /// A `Spawned` with nothing behind it: `wait` reports what the test
    /// asks for and nothing else is touched.
    fn nothing_running(code: i32) -> Spawned {
        Spawned {
            input: Box::new(std::io::sink()),
            output: Box::new(std::io::empty()),
            errors: None,
            resize: None,
            kill: Box::new(|| {}),
            wait: Box::new(move || code),
        }
    }

    /// A logon must outlive the process it started, or the cache's sweeper
    /// unloads the profile under a running shell after the idle grace —
    /// which is what §19.2's "lives while any channel uses it" forbids.
    #[test]
    fn a_logon_is_held_until_the_process_it_started_exits() {
        let logon = Arc::new("a token");
        let spawned = hold_until_it_exits(nothing_running(3), Some(logon.clone()));
        assert_eq!(
            Arc::strong_count(&logon),
            2,
            "the session must still hold the logon while its process runs"
        );
        assert_eq!((spawned.wait)(), 3, "the exit code must survive the wrap");
        assert_eq!(
            Arc::strong_count(&logon),
            1,
            "and let go once the process has ended"
        );
    }

    /// The agent identity holds no logon, and wrapping must not change what
    /// the seam handed back.
    #[test]
    fn the_agent_identity_holds_nothing() {
        let spawned = hold_until_it_exits(nothing_running(7), None::<Arc<()>>);
        assert_eq!((spawned.wait)(), 7);
    }

    /// Windows parses its own argv out of one string, so what the seam
    /// hands `CreateProcessAsUserW` has to survive `CommandLineToArgvW`
    /// putting it back together — including the paths a dev machine is full
    /// of, which end in backslashes.
    #[test]
    fn a_command_line_quotes_what_windows_would_otherwise_resplit() {
        assert_eq!(command_line(&["whoami".into()]), "whoami");
        assert_eq!(
            command_line(&["C:\\Program Files\\git\\git.exe".into(), "status".into()]),
            r#""C:\Program Files\git\git.exe" status"#
        );
        // A trailing backslash inside quotes would escape the closing quote,
        // so the run is doubled.
        assert_eq!(
            command_line(&["cd".into(), "C:\\src\\my project\\".into()]),
            r#"cd "C:\src\my project\\""#
        );
        // An embedded quote is escaped, and the backslashes before it with it.
        assert_eq!(command_line(&[r#"say"hi"#.into()]), r#""say\"hi""#);
        assert_eq!(command_line(&["".into()]), r#""""#);
    }
}
