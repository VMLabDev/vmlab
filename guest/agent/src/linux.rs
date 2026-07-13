//! Linux platform half: virtio-port discovery, the PTY terminal (a
//! namespace-free descendant of `guest/cinit/src/tty.rs`), metrics sampling
//! from /proc, and a best-effort clipboard when a display session is
//! actually reachable (in practice: never on the headless server templates,
//! and the feature is then simply not advertised).

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, chdir, dup2, execve, fork, setsid};

use vmlab_agent_proto::{AgentMsg, DiskUsage, FrameKind, PORT_NAME, RecvWindow, features};

use crate::mux::{Input, Mux, pump_out};

const TERMINAL_MOTD: &str = concat!(
    "\n",
    " __   ____  __ _      _   ___ \n",
    " \\ \\ / /  \\/  | |    /_\\ | _ )\n",
    "  \\ V /| |\\/| | |__ / _ \\| _ \\\n",
    "   \\_/ |_|  |_|____/_/ \\_\\___/\n",
    "\n",
    "vmlab terminal - root shell over virtio-serial (works with no network).\n",
    "  'exit' ends this session; Ctrl-] detaches the CLI client.\n",
    "\n",
);

// Guest-only ioctls (the whole VM is ours; agent work allows unsafe here).
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, Winsize);
nix::ioctl_write_int_bad!(tiocsctty, libc::TIOCSCTTY);

fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Resolve a virtio-serial port by its name property. Without udev there are
/// no /dev/virtio-ports/<name> symlinks, so scan /sys and fall back to the
/// symlink path for distros that do run udev.
fn find_virtio_port(name: &str) -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir("/sys/class/virtio-ports") {
        for entry in entries.flatten() {
            let port_name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
            if port_name.trim() == name {
                return Some(PathBuf::from("/dev").join(entry.file_name()));
            }
        }
    }
    let byname = PathBuf::from("/dev/virtio-ports").join(name);
    byname.exists().then_some(byname)
}

/// Open the agent port read+write (virtio ports are exclusive-open; the two
/// halves are fd clones). Retries until the device exists — the service may
/// start before the virtio-console driver has bound. A busy port means
/// another agent instance is serving: exit quietly so double-starts are
/// harmless.
pub fn open_port() -> (
    impl Read + Send + 'static,
    impl std::io::Write + Send + 'static,
) {
    loop {
        let Some(path) = find_virtio_port(PORT_NAME) else {
            eprintln!("vmlab-agent: waiting for port {PORT_NAME}");
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(port) => match port.try_clone() {
                Ok(w) => return (port, w),
                Err(e) => {
                    eprintln!("vmlab-agent: port clone failed: {e}");
                    thread::sleep(Duration::from_secs(2));
                }
            },
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                eprintln!("vmlab-agent: port {PORT_NAME} busy (another instance is serving)");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("vmlab-agent: cannot open {}: {e}", path.display());
                thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

pub struct LinuxPlatform {
    clipboard: Option<ClipboardTool>,
}

pub fn new_platform() -> LinuxPlatform {
    LinuxPlatform {
        clipboard: ClipboardTool::probe(),
    }
}

impl crate::mux::Platform for LinuxPlatform {
    fn os(&self) -> &'static str {
        "linux"
    }

    fn features(&self) -> Vec<String> {
        let mut f = vec![
            features::TERMINAL.to_string(),
            features::EXEC.to_string(),
            features::FILE.to_string(),
            features::TAIL.to_string(),
            features::METRICS.to_string(),
        ];
        if self.clipboard.is_some() {
            f.push(features::CLIPBOARD.to_string());
        }
        f
    }

    fn open_terminal(
        &self,
        mux: &Mux,
        id: u32,
        cols: u16,
        rows: u16,
        command: Option<Vec<String>>,
    ) {
        let shell = match command.or_else(default_shell) {
            Some(s) if !s.is_empty() => s,
            _ => {
                mux.send_error(Some(id), "terminal: no shell found in this guest");
                return;
            }
        };
        let size = winsize(cols, rows);
        let (master, pid) = match spawn_shell(&shell, &size) {
            Ok(v) => v,
            Err(e) => {
                mux.send_error(Some(id), format!("terminal: {e}"));
                return;
            }
        };
        let master = Arc::new(master);

        let done = Arc::new(AtomicBool::new(false));
        let resize_master = master.clone();
        let kill_done = done.clone();
        let Some((input, credit)) = mux.register(
            id,
            Some(Box::new(move |cols, rows| {
                let ws = winsize(cols, rows);
                // SAFETY: TIOCSWINSZ on a live PTY master with a valid size.
                let _ = unsafe { tiocswinsz(resize_master.as_raw_fd(), &ws) };
            })),
            Some(Box::new(move || {
                if !kill_done.load(Ordering::SeqCst) {
                    let _ = kill(pid, Signal::SIGKILL);
                }
            })),
        ) else {
            let _ = kill(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
            return;
        };
        mux.send_ctrl(&AgentMsg::Opened { id });

        // Input pump: host bytes → PTY master.
        {
            let mux = mux.clone();
            let master = master.clone();
            thread::spawn(move || {
                let mut window = RecvWindow::default();
                for input in input {
                    match input {
                        Input::Bytes(b) => {
                            // A dying shell may stop reading; dropped input
                            // is fine, the session is ending anyway.
                            let _ = write_all_fd(&master, &b);
                            if let Some(grant) = window.recv(b.len()) {
                                mux.send_ctrl(&AgentMsg::WindowAdjust { id, bytes: grant });
                            }
                        }
                        Input::Eof => {}
                    }
                }
            });
        }

        // Output pump: PTY master → host (EIO once the slave side closes).
        let out_pump = {
            let (mux, credit) = (mux.clone(), credit.clone());
            let master = master.clone();
            thread::spawn(move || {
                let Ok(fd) = master.try_clone() else { return };
                pump_out(&mux, id, FrameKind::Data, &credit, File::from(fd));
            })
        };

        // Reaper: shell exited → flush output → report → clean up.
        let mux = mux.clone();
        thread::spawn(move || {
            let code = match waitpid(pid, None) {
                Ok(WaitStatus::Exited(_, code)) => code,
                Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
                _ => 127,
            };
            done.store(true, Ordering::SeqCst);
            drop(master); // closes our master ref → output pump sees EOF/EIO
            let _ = out_pump.join();
            mux.send_ctrl(&AgentMsg::Exited { id, code });
            mux.remove_finished(id);
        });
    }

    fn open_eventlog(&self, mux: &Mux, id: u32, _filter: Option<String>) {
        mux.send_error(Some(id), "event log tailing is Windows-only");
    }

    fn set_clipboard(&self, mux: &Mux, text: String) {
        match &self.clipboard {
            Some(tool) => {
                if let Err(e) = tool.set(&text) {
                    mux.send_error(None, format!("clipboard: {e}"));
                }
            }
            None => mux.send_error(None, "clipboard: no display session reachable"),
        }
    }

    fn get_clipboard(&self, mux: &Mux) {
        match &self.clipboard {
            Some(tool) => match tool.get() {
                Ok(text) => mux.send_ctrl(&AgentMsg::Clipboard { text }),
                Err(e) => mux.send_error(None, format!("clipboard: {e}")),
            },
            None => mux.send_error(None, "clipboard: no display session reachable"),
        }
    }
}

/// The default interactive shell: a bash login shell when the guest has
/// bash, else POSIX sh.
fn default_shell() -> Option<Vec<String>> {
    for sh in ["/bin/bash", "/usr/bin/bash"] {
        if Path::new(sh).exists() {
            return Some(vec![sh.to_string(), "-l".to_string()]);
        }
    }
    Path::new("/bin/sh")
        .exists()
        .then(|| vec!["/bin/sh".to_string(), "-l".to_string()])
}

fn shell_env() -> Vec<CString> {
    [
        "TERM=xterm-256color",
        "HOME=/root",
        "USER=root",
        "LOGNAME=root",
        "SHELL=/bin/sh",
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "LANG=C.UTF-8",
    ]
    .iter()
    .map(|s| CString::new(*s).unwrap())
    .collect()
}

/// Allocate a PTY sized `size` and fork the shell: the child becomes a
/// session leader on the slave and execs. Returns the master (close-on-exec)
/// and the child pid. Post-fork the child only performs async-signal-safe
/// operations before execve (allocation-free, like cinit's spawn).
fn spawn_shell(shell: &[String], size: &Winsize) -> std::io::Result<(OwnedFd, Pid)> {
    let pty = openpty(size, None).map_err(std::io::Error::from)?;
    fcntl(
        pty.master.as_raw_fd(),
        FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC),
    )
    .map_err(std::io::Error::from)?;

    let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, what.to_string());
    let c_exe = CString::new(shell[0].as_str()).map_err(|_| bad("NUL in shell path"))?;
    let c_argv: Vec<CString> = shell
        .iter()
        .map(|a| CString::new(a.as_str()))
        .collect::<Result<_, _>>()
        .map_err(|_| bad("NUL in shell argv"))?;
    let c_env = shell_env();
    let c_home = CString::new("/root").unwrap();
    let c_root = CString::new("/").unwrap();
    let slave_raw = pty.slave.as_raw_fd();

    // SAFETY: multithreaded fork; the child only performs async-signal-safe
    // operations (raw syscalls via nix + _exit) before execve.
    match unsafe { fork() }.map_err(std::io::Error::from)? {
        ForkResult::Parent { child } => {
            drop(pty.slave); // parent keeps only the master
            Ok((pty.master, child))
        }
        ForkResult::Child => {
            let die = |_what: &str| -> ! { unsafe { libc::_exit(127) } };
            if setsid().is_err() {
                die("setsid");
            }
            // SAFETY: TIOCSCTTY on the fresh session's slave fd.
            if unsafe { tiocsctty(slave_raw, 0) }.is_err() {
                die("tiocsctty");
            }
            for fd in 0..=2 {
                if dup2(slave_raw, fd).is_err() {
                    die("dup2");
                }
            }
            let _ = write_all_raw(libc::STDOUT_FILENO, TERMINAL_MOTD.as_bytes());
            if chdir(c_home.as_c_str()).is_err() && chdir(c_root.as_c_str()).is_err() {
                die("chdir");
            }
            let _ = execve(&c_exe, &c_argv, &c_env);
            die("execve");
        }
    }
}

/// Write the whole buffer to a raw fd (PTY master writes can be short).
fn write_all_fd(fd: &OwnedFd, mut buf: &[u8]) -> nix::Result<()> {
    while !buf.is_empty() {
        let n = nix::unistd::write(fd, buf)?;
        if n == 0 {
            return Err(nix::errno::Errno::EIO);
        }
        buf = &buf[n..];
    }
    Ok(())
}

fn write_all_raw(fd: libc::c_int, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        // SAFETY: plain write(2) on a valid fd with an in-bounds buffer.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n > 0 {
            buf = &buf[n as usize..];
        } else if n < 0 && nix::errno::Errno::last() == nix::errno::Errno::EINTR {
            continue;
        } else {
            return false;
        }
    }
    true
}

pub fn kill_process(pid: u32) {
    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
}

// ---- metrics sampling ------------------------------------------------------

/// Cumulative (busy, total) jiffies from /proc/stat's aggregate cpu line.
pub type CpuSample = (u64, u64);

pub fn cpu_sample() -> CpuSample {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let Some(line) = stat.lines().find(|l| l.starts_with("cpu ")) else {
        return (0, 0);
    };
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    let total: u64 = fields.iter().sum();
    let idle = fields.get(3).copied().unwrap_or(0) + fields.get(4).copied().unwrap_or(0);
    (total.saturating_sub(idle), total)
}

pub fn cpu_pct(prev: &CpuSample, cur: &CpuSample) -> f32 {
    let busy = cur.0.saturating_sub(prev.0) as f32;
    let total = cur.1.saturating_sub(prev.1) as f32;
    if total <= 0.0 {
        0.0
    } else {
        (100.0 * busy / total).clamp(0.0, 100.0)
    }
}

/// (used, total) bytes; "used" excludes reclaimable cache (MemAvailable).
pub fn mem_sample() -> (u64, u64) {
    let mut total = 0u64;
    let mut avail = 0u64;
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    for line in meminfo.lines() {
        let kb = |l: &str| {
            l.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                * 1024
        };
        if line.starts_with("MemTotal:") {
            total = kb(line);
        } else if line.starts_with("MemAvailable:") {
            avail = kb(line);
        }
    }
    (total.saturating_sub(avail), total)
}

pub fn disk_sample() -> Vec<DiskUsage> {
    let mut out = Vec::new();
    let mut seen_devices = Vec::new();
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    for line in mounts.lines() {
        let mut it = line.split_whitespace();
        let (Some(device), Some(mount)) = (it.next(), it.next()) else {
            continue;
        };
        // Real block-backed filesystems only; one entry per device (bind
        // mounts and btrfs subvolumes repeat the device).
        if !device.starts_with("/dev/") || seen_devices.contains(&device.to_string()) {
            continue;
        }
        let Ok(vfs) = nix::sys::statvfs::statvfs(mount) else {
            continue;
        };
        // c_ulong == u64 on every target we build (all 64-bit).
        let frsize: u64 = vfs.fragment_size();
        let total = vfs.blocks() * frsize;
        if total == 0 {
            continue;
        }
        seen_devices.push(device.to_string());
        out.push(DiskUsage {
            mount: mount.to_string(),
            used: total - vfs.blocks_available() * frsize,
            total,
        });
    }
    out
}

// ---- clipboard (best-effort; headless guests never advertise it) ----------

struct ClipboardTool {
    get: Vec<String>,
    set: Vec<String>,
}

impl ClipboardTool {
    /// A clipboard exists only when the agent's own environment can reach a
    /// display server (never true for the root service on server templates)
    /// and a helper tool is installed.
    fn probe() -> Option<ClipboardTool> {
        let has = |bin: &str| {
            std::env::var_os("PATH")
                .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(bin).exists()))
        };
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && has("wl-copy") && has("wl-paste") {
            return Some(ClipboardTool {
                get: vec!["wl-paste".into(), "--no-newline".into()],
                set: vec!["wl-copy".into()],
            });
        }
        if std::env::var_os("DISPLAY").is_some() && has("xclip") {
            return Some(ClipboardTool {
                get: vec![
                    "xclip".into(),
                    "-selection".into(),
                    "clipboard".into(),
                    "-o".into(),
                ],
                set: vec!["xclip".into(), "-selection".into(), "clipboard".into()],
            });
        }
        None
    }

    fn get(&self) -> std::io::Result<String> {
        let out = Command::new(&self.get[0])
            .args(&self.get[1..])
            .stderr(Stdio::null())
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn set(&self, text: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut child = Command::new(&self.set[0])
            .args(&self.set[1..])
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }
        drop(child.stdin.take());
        child.wait()?;
        Ok(())
    }
}

/// Detach from the launching terminal/agent: double-fork + setsid + stdio to
/// /dev/null. Used for hand-launches over QGA during development; the
/// systemd service runs in the foreground.
pub fn daemonize() {
    // SAFETY: standard double-fork; parents _exit immediately.
    unsafe {
        match libc::fork() {
            -1 => std::process::exit(1),
            0 => {}
            _ => libc::_exit(0),
        }
        if libc::setsid() < 0 {
            std::process::exit(1);
        }
        match libc::fork() {
            -1 => std::process::exit(1),
            0 => {}
            _ => libc::_exit(0),
        }
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..=2 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_pct_computes_delta_utilisation() {
        assert_eq!(cpu_pct(&(50, 100), &(80, 200)), 30.0);
        assert_eq!(cpu_pct(&(0, 0), &(0, 0)), 0.0);
        // Clamped even if counters go weird.
        assert_eq!(cpu_pct(&(0, 100), &(300, 200)), 100.0);
    }

    #[test]
    fn proc_samples_do_not_panic() {
        // Smoke on the build host (also has /proc).
        let (busy, total) = cpu_sample();
        assert!(total >= busy);
        let (used, total) = mem_sample();
        assert!(total >= used);
        let disks = disk_sample();
        for d in &disks {
            assert!(d.total >= d.used, "{}", d.mount);
        }
    }

    #[test]
    fn default_shell_exists_on_the_build_host() {
        let shell = default_shell().unwrap();
        assert!(Path::new(&shell[0]).exists());
        assert_eq!(shell[1], "-l");
    }

    #[test]
    fn motd_mentions_detach_and_no_network() {
        assert!(TERMINAL_MOTD.contains("Ctrl-]"));
        assert!(TERMINAL_MOTD.contains("no network"));
    }
}
