//! Interactive shell sessions over the `vmlab.tty.0` virtio-serial port.
//!
//! One shell at a time: a PTY is allocated, a child forks, chroots into the
//! container rootfs (like `docker exec` it runs as root), applies the spec
//! env (+ `TERM`/`PATH` fallbacks) and execs a shell. The PTY master is
//! pumped to/from the virtio port by two threads:
//!
//! - a **persistent input pump** (host → PTY): reads the port forever and
//!   writes into whichever session is current, so reconnecting host clients
//!   always land in the live shell. With no session, input is dropped.
//! - a **per-session output pump** (PTY → host): exits when the shell dies
//!   (master reads fail once the slave side closes). Port writes block while
//!   no host client is attached — that is fine here, the shell only writes
//!   in response to input or its initial prompt, which flushes on attach.
//!
//! When the shell exits (the client typed `exit`), the manager reaps it via
//! the shared [`Reaper`] and respawns a fresh one after a short delay so the
//! next attach gets a prompt.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::pty::{Winsize, openpty};
use nix::unistd::{ForkResult, Pid, chdir, chroot, dup2, execve, fork, setsid};

use vmlab_cinit_proto::ContainerSpec;

use crate::ctl::find_virtio_port;
use crate::mounts::ROOTFS;
use crate::reap::Reaper;
use crate::util::{Ctx, Result};

/// Where the mount phase drops the initramfs' static busybox inside the
/// overlay (see [`crate::mounts::install_shell_fallback`]), so distroless
/// images still get a shell.
pub const BUSYBOX_FALLBACK: &str = "/.vmlab/busybox";

const PORT_NAME: &str = "vmlab.tty.0";

/// Respawn delay after a shell exits, so a client typing `exit` and
/// re-attaching lands on a fresh prompt rather than a dead port.
const RESPAWN_DELAY: Duration = Duration::from_millis(200);

// Guest-only ioctls (the whole VM is ours; PID-1 work allows unsafe here).
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, Winsize);
nix::ioctl_write_int_bad!(tiocsctty, libc::TIOCSCTTY);

/// A [`Winsize`] from the wire's cols/rows (pixel fields unused).
pub fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Pick the shell argv for the session: the rootfs' own `/bin/sh` when it
/// has one, else the busybox copy the mount phase installed. Paths are as
/// the post-chroot child sees them.
pub fn choose_shell(rootfs: &str) -> Option<Vec<String>> {
    let executable = |inside: &str| {
        let full = format!("{rootfs}{inside}");
        Path::new(&full)
            .metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if executable("/bin/sh") {
        return Some(vec!["/bin/sh".to_string()]);
    }
    if executable(BUSYBOX_FALLBACK) {
        return Some(vec![BUSYBOX_FALLBACK.to_string(), "sh".to_string()]);
    }
    None
}

/// The shell's environment: the container env verbatim plus a terminal type
/// and a PATH fallback when the spec carries none.
pub fn shell_env(env: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = env.to_vec();
    if !env.iter().any(|(k, _)| k == "TERM") {
        env.push(("TERM".into(), "xterm-256color".into()));
    }
    if !env.iter().any(|(k, _)| k == "PATH") {
        env.push(("PATH".into(), crate::container::DEFAULT_PATH.into()));
    }
    env
}

struct Shared {
    /// Latest requested size — applied to the live PTY on `tty_resize` and
    /// to every new session at spawn.
    size: Mutex<Winsize>,
    /// The current session's PTY master; `None` between sessions.
    master: Mutex<Option<OwnedFd>>,
}

/// Handle for the ctl command path: [`Tty::resize`]. Cheap to clone.
/// Degrades to a size-only store when the port is absent (a hand-launched
/// debug VM), so boot still proceeds.
#[derive(Clone)]
pub struct Tty {
    shared: Arc<Shared>,
}

impl Tty {
    /// Open the tty port and start the session manager. Never fails: with no
    /// port (or an unopenable one) the returned handle only remembers sizes.
    pub fn start(spec: &ContainerSpec, env: &[(String, String)], reaper: Arc<Reaper>) -> Tty {
        let shared = Arc::new(Shared {
            size: Mutex::new(winsize(80, 24)),
            master: Mutex::new(None),
        });
        let tty = Tty {
            shared: shared.clone(),
        };

        let Some(path) = find_virtio_port(PORT_NAME) else {
            eprintln!(
                "vmlab-cinit: warning: tty port {PORT_NAME} not found; no interactive shell"
            );
            return tty;
        };
        // Exclusive-open like the ctl port: open once read+write, clone fds
        // for the pump threads.
        let port = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "vmlab-cinit: warning: cannot open tty port {}: {e}",
                    path.display()
                );
                return tty;
            }
        };
        let mut port_in = match port.try_clone() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("vmlab-cinit: warning: tty port clone failed: {e}");
                return tty;
            }
        };

        // Persistent input pump: host bytes → the current session's PTY.
        {
            let shared = shared.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match port_in.read(&mut buf) {
                        // EOF: host side detached; it may reconnect.
                        Ok(0) => thread::sleep(Duration::from_millis(200)),
                        Ok(n) => {
                            let master = shared.master.lock().unwrap();
                            if let Some(fd) = master.as_ref() {
                                // Full PTY buffer or a dying shell: drop the
                                // input, the session is ending anyway.
                                let _ = write_all_fd(fd, &buf[..n]);
                            }
                        }
                        Err(e) => {
                            eprintln!("vmlab-cinit: warning: tty port read failed: {e}");
                            thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            });
        }

        // Session manager: spawn, wait (via the reaper — only the main
        // thread calls waitpid), respawn.
        let env = shell_env(env);
        let workdir = spec.workdir.clone().unwrap_or_else(|| "/".to_string());
        thread::spawn(move || {
            loop {
                let Some(shell) = choose_shell(ROOTFS) else {
                    eprintln!(
                        "vmlab-cinit: warning: no shell in the container rootfs \
                         (and no busybox fallback)"
                    );
                    thread::sleep(Duration::from_secs(10));
                    continue;
                };
                let size = *shared.size.lock().unwrap();
                let (master, pid) = match spawn_shell(&shell, &env, &workdir, &size) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("vmlab-cinit: warning: tty shell spawn failed: {e}");
                        thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };
                // Subscribe before publishing the master so the exit can't
                // slip past us (subscribe-after-exit is also handled).
                let exit = reaper.subscribe(pid.as_raw());

                // Per-session output pump: PTY → host port.
                if let (Ok(out_fd), Ok(mut port_out)) = (master.try_clone(), port.try_clone()) {
                    thread::spawn(move || {
                        let mut f = File::from(out_fd);
                        let mut buf = [0u8; 4096];
                        loop {
                            match f.read(&mut buf) {
                                // EOF/EIO: the shell (slave side) is gone.
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    // Blocks while the host is detached —
                                    // intended (the prompt flushes on attach).
                                    if port_out.write_all(&buf[..n]).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }

                *shared.master.lock().unwrap() = Some(master);
                let _ = exit.recv();
                *shared.master.lock().unwrap() = None;
                thread::sleep(RESPAWN_DELAY);
            }
        });
        tty
    }

    /// `tty_resize` from the host: remember the size and apply it to the
    /// live session (SIGWINCH reaches the shell via the PTY).
    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = winsize(cols, rows);
        *self.shared.size.lock().unwrap() = ws;
        if let Some(fd) = self.shared.master.lock().unwrap().as_ref() {
            // SAFETY: TIOCSWINSZ on a live PTY master with a valid Winsize.
            if let Err(e) = unsafe { tiocswinsz(fd.as_raw_fd(), &ws) } {
                eprintln!("vmlab-cinit: warning: tty resize failed: {e}");
            }
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

/// Allocate a PTY sized `size` and fork the shell: the child becomes a
/// session leader on the slave, chroots into [`ROOTFS`], chdirs to the
/// container workdir (falling back to `/`) and execs. Returns the master
/// (close-on-exec, so it never leaks into the shell) and the child pid.
fn spawn_shell(
    shell: &[String],
    env: &[(String, String)],
    workdir: &str,
    size: &Winsize,
) -> Result<(OwnedFd, Pid)> {
    let pty = openpty(size, None).ctx("openpty")?;
    fcntl(pty.master.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).ctx("cloexec master")?;

    // Everything the child needs, allocated before fork (same contract as
    // container::spawn: the child must not allocate).
    let cstring = |s: &str| CString::new(s).ctx(&format!("NUL byte in {s:?}"));
    let c_exe = cstring(&shell[0])?;
    let c_argv: Vec<CString> = shell.iter().map(|a| cstring(a)).collect::<Result<_>>()?;
    let c_env: Vec<CString> = env
        .iter()
        .map(|(k, v)| cstring(&format!("{k}={v}")))
        .collect::<Result<_>>()?;
    let c_workdir = cstring(workdir)?;
    let c_root = cstring("/")?;
    let slave_raw = pty.slave.as_raw_fd();

    // SAFETY: multithreaded fork; the child only performs async-signal-safe
    // operations (raw syscalls via nix + _exit) before execve.
    match unsafe { fork() }.ctx("fork shell")? {
        ForkResult::Parent { child } => {
            drop(pty.slave); // parent keeps only the master
            Ok((pty.master, child))
        }
        ForkResult::Child => {
            let die = |what: &str| -> ! {
                eprintln!("vmlab-cinit: tty shell launch failed: {what}");
                unsafe { libc::_exit(127) }
            };
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
            if chroot(ROOTFS).is_err() {
                die("chroot");
            }
            if chdir(c_workdir.as_c_str()).is_err() && chdir(c_root.as_c_str()).is_err() {
                die("chdir");
            }
            let _ = execve(&c_exe, &c_argv, &c_env);
            die("execve");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch_exe(path: &str) {
        fs::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn shell_choice_prefers_rootfs_sh_then_busybox() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().to_str().unwrap().to_string();

        // Nothing there: no shell.
        assert_eq!(choose_shell(&rootfs), None);

        // Busybox fallback only (a distroless image).
        touch_exe(&format!("{rootfs}{BUSYBOX_FALLBACK}"));
        assert_eq!(
            choose_shell(&rootfs),
            Some(vec![BUSYBOX_FALLBACK.to_string(), "sh".to_string()])
        );

        // The image's own /bin/sh wins.
        touch_exe(&format!("{rootfs}/bin/sh"));
        assert_eq!(choose_shell(&rootfs), Some(vec!["/bin/sh".to_string()]));
    }

    #[test]
    fn shell_choice_ignores_non_executables() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().to_str().unwrap().to_string();
        let sh = format!("{rootfs}/bin/sh");
        fs::create_dir_all(format!("{rootfs}/bin")).unwrap();
        fs::write(&sh, "x").unwrap();
        fs::set_permissions(&sh, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(choose_shell(&rootfs), None);
    }

    #[test]
    fn shell_env_adds_term_and_path_fallbacks() {
        let env = shell_env(&[("FOO".into(), "bar".into())]);
        assert!(env.contains(&("TERM".into(), "xterm-256color".into())));
        assert!(
            env.iter()
                .any(|(k, v)| k == "PATH" && v == crate::container::DEFAULT_PATH)
        );
        assert!(env.contains(&("FOO".into(), "bar".into())));

        // Explicit values win.
        let env = shell_env(&[
            ("TERM".into(), "vt100".into()),
            ("PATH".into(), "/only".into()),
        ]);
        assert_eq!(env.iter().filter(|(k, _)| k == "TERM").count(), 1);
        assert_eq!(env.iter().filter(|(k, _)| k == "PATH").count(), 1);
        assert!(env.contains(&("TERM".into(), "vt100".into())));
        assert!(env.contains(&("PATH".into(), "/only".into())));
    }

    #[test]
    fn resize_is_stored_for_future_sessions() {
        // On a host with no /sys/class/virtio-ports the handle degrades to a
        // size store — exactly what the "apply to new sessions" path needs.
        let spec: ContainerSpec =
            serde_json::from_str(r#"{ "hostname": "t", "cmd": ["/bin/true"] }"#).unwrap();
        let tty = Tty::start(&spec, &[], Arc::new(Reaper::default()));
        tty.resize(132, 43);
        let ws = *tty.shared.size.lock().unwrap();
        assert_eq!((ws.ws_col, ws.ws_row), (132, 43));
        assert!(tty.shared.master.lock().unwrap().is_none());
    }

    #[test]
    fn winsize_maps_cols_and_rows() {
        let ws = winsize(80, 24);
        assert_eq!(ws.ws_col, 80);
        assert_eq!(ws.ws_row, 24);
    }
}
