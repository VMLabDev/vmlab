//! QEMU (and swtpm) process management: spawn with logs, watch for exit,
//! kill. Graceful-stop policy lives in the lab daemon's lifecycle (§7.2);
//! this layer is mechanics only.
//!
//! The waiter task owns the `Child` exclusively (holding a lock across
//! `child.wait()` would deadlock `kill()`); killing goes through a signal to
//! the recorded pid instead.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::sync::watch;

/// A spawned VM (or helper) process.
pub struct Proc {
    pub name: String,
    /// 0 once the process has been reaped.
    pid: AtomicU32,
    /// Becomes Some(status_string) when the process exits.
    exited: watch::Receiver<Option<String>>,
}

/// A parent-held fd installed at a fixed descriptor number in the child —
/// how pre-opened tap netdevs reach QEMU (`-netdev tap,fd=N`).
pub struct ChildFd {
    pub parent: std::os::fd::OwnedFd,
    pub child: i32,
}

impl Proc {
    /// Spawn `binary` with `args`, stdout+stderr appended to `log_path`.
    pub async fn spawn(
        name: &str,
        binary: &str,
        args: &[String],
        log_path: &Path,
    ) -> Result<Arc<Proc>> {
        Self::spawn_with_fds(name, binary, args, log_path, Vec::new()).await
    }

    /// [`Proc::spawn`], additionally installing `fds` at their fixed
    /// descriptor numbers in the child.
    pub async fn spawn_with_fds(
        name: &str,
        binary: &str,
        args: &[String],
        log_path: &Path,
        fds: Vec<ChildFd>,
    ) -> Result<Arc<Proc>> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;
        let log_err = log.try_clone()?;

        let mut cmd = Command::new(binary);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            .kill_on_drop(false);
        #[cfg(feature = "ebpf")]
        if !fds.is_empty() {
            use command_fds::CommandFdExt as _;
            let mappings = fds
                .into_iter()
                .map(|f| command_fds::FdMapping {
                    parent_fd: f.parent,
                    child_fd: f.child,
                })
                .collect();
            cmd.as_std_mut()
                .fd_mappings(mappings)
                .map_err(|e| anyhow::anyhow!("child fd mapping: {e:?}"))?;
        }
        #[cfg(not(feature = "ebpf"))]
        anyhow::ensure!(
            fds.is_empty(),
            "pre-opened netdev fds need the `ebpf` feature"
        );

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {binary} for {name}"))?;

        let (tx, rx) = watch::channel(None);
        let proc = Arc::new(Proc {
            name: name.to_string(),
            pid: AtomicU32::new(child.id().unwrap_or(0)),
            exited: rx,
        });

        // Waiter task: sole owner of the Child; reaps and publishes the
        // exit status.
        let watcher = proc.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            let s = match status {
                Ok(st) => st.to_string(),
                Err(e) => format!("wait failed: {e}"),
            };
            watcher.pid.store(0, Ordering::SeqCst);
            let _ = tx.send(Some(s));
        });

        Ok(proc)
    }

    pub fn pid(&self) -> Option<u32> {
        match self.pid.load(Ordering::SeqCst) {
            0 => None,
            p => Some(p),
        }
    }

    pub fn is_running(&self) -> bool {
        self.exited.borrow().is_none()
    }

    /// Exit status string, if the process has exited.
    pub fn exit_status(&self) -> Option<String> {
        self.exited.borrow().clone()
    }

    /// Wait for exit with a timeout. Ok(status) on exit, Err on timeout.
    pub async fn wait_exit(&self, timeout: std::time::Duration) -> Result<String> {
        let mut rx = self.exited.clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(s) = rx.borrow().clone() {
                return Ok(s);
            }
            tokio::time::timeout_at(deadline, rx.changed())
                .await
                .map_err(|_| anyhow::anyhow!("{} did not exit within {timeout:?}", self.name))?
                .map_err(|_| anyhow::anyhow!("process watcher gone"))?;
        }
    }

    /// SIGKILL the process (the hard end of the §7.2 stop ladder).
    pub async fn kill(&self) {
        if let Some(pid) = self.pid() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

/// Is `bin` on PATH? Used by the lab daemon's pre-`up` binary check so a
/// missing package is one clear error instead of a spawn failure mid-boot.
pub fn binary_on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(bin).is_file())
}

/// Spawn swtpm for a VM (PRD §5.3): TPM 2.0 emulator on a unix control
/// socket, state under `state_dir`.
pub async fn spawn_swtpm(
    vm_name: &str,
    state_dir: &Path,
    ctrl_sock: &Path,
    log_path: &Path,
) -> Result<Arc<Proc>> {
    std::fs::create_dir_all(state_dir)?;
    let args = vec![
        "socket".to_string(),
        "--tpm2".to_string(),
        "--tpmstate".to_string(),
        format!("dir={}", state_dir.display()),
        "--ctrl".to_string(),
        format!("type=unixio,path={}", ctrl_sock.display()),
        "--terminate".to_string(),
    ];
    Proc::spawn(&format!("swtpm:{vm_name}"), "swtpm", &args, log_path).await
}

/// The argv fragments that identify a process as belonging to `lab`:
///
/// - our QEMU/micro-VM `-name vmlab:<lab>/<machine>` marker (see cmdline.rs);
///   the trailing `/` keeps `foo` from matching `foobar`'s VMs,
/// - the lab's runtime directory, which appears in every helper's argv as a
///   socket or state path (`swtpm --ctrl …/swtpm.sock`, `virtiofsd
///   --socket-path …/virtiofs0.sock`),
/// - the lab's SMB state directory, which is how `smbd -s <…>/smb/smb.conf`
///   is recognised — it carries no runtime-dir path (needs `root`).
///
/// All three are unique to one lab, so a match can't hit an unrelated process.
fn lab_process_markers(lab: &str, root: Option<&Path>) -> Vec<String> {
    let mut markers = vec![
        format!("vmlab:{lab}/"),
        format!("{}/", crate::paths::lab_runtime_dir(lab).display()),
    ];
    if let Some(root) = root {
        let smb = crate::paths::lab_local_dir(root).join("smb");
        markers.push(smb.display().to_string());
    }
    markers
}

/// Does this raw `/proc/<pid>/cmdline` (NUL-separated argv) belong to `lab`?
fn cmdline_matches(cmdline: &[u8], markers: &[String]) -> bool {
    cmdline.split(|b| *b == 0).any(|arg| {
        markers.iter().any(|m| {
            arg.starts_with(m.as_bytes())
                // Helpers carry the path mid-argument (`dir=…`, `type=unixio,path=…`).
                || arg
                    .windows(m.len())
                    .any(|w| w == m.as_bytes())
        })
    })
}

/// SIGKILL every process belonging to `lab` — QEMU **and** its helpers (swtpm,
/// virtiofsd, smbd). Returns how many were signalled.
///
/// Used to reap what a lab daemon orphaned by dying without stopping anything:
/// there is no `Proc` handle left, so we scan `/proc` for the lab's markers.
/// Pass the lab's `root` when known so the lab's `smbd` is covered too — an
/// orphaned smbd holds its port against the next `up`.
pub fn kill_lab_orphans(lab: &str, root: Option<&Path>) -> usize {
    let markers = lab_process_markers(lab, root);
    let us = std::process::id();
    let mut killed = 0;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid as u32 == us {
            continue; // our own argv can name the lab (`vmlab __labd --lab …`)
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if cmdline_matches(&cmdline, &markers) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            killed += 1;
        }
    }
    killed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_watch_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("p.log");
        let p = Proc::spawn("t", "sh", &["-c".into(), "echo hi; exit 3".into()], &log)
            .await
            .unwrap();
        let status = p
            .wait_exit(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(status.contains('3'), "{status}");
        assert!(!p.is_running());
        assert!(p.pid().is_none());
        let logged = std::fs::read_to_string(&log).unwrap();
        assert_eq!(logged.trim(), "hi");
    }

    #[tokio::test]
    async fn kill_terminates_even_after_waiter_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Proc::spawn("t", "sleep", &["30".into()], &tmp.path().join("p.log"))
            .await
            .unwrap();
        assert!(p.is_running());
        // Let the waiter task start waiting first — this order used to
        // deadlock when the waiter held a lock across child.wait().
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        p.kill().await;
        let status = p
            .wait_exit(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(status.contains("signal"), "{status}");
    }

    #[test]
    fn binary_on_path_probes_path() {
        assert!(binary_on_path("sh"));
        assert!(!binary_on_path("definitely-not-a-real-binary-1a2b3c"));
    }

    #[test]
    fn lab_qemu_cmdline_matching() {
        let markers = lab_process_markers("mylab", None);
        // Real-ish argv: NUL-separated, with the -name marker.
        let cmd = b"qemu-system-x86_64\0-name\0vmlab:mylab/web\0-machine\0q35\0";
        assert!(cmdline_matches(cmd, &markers));
        // A different lab must not match.
        assert!(!cmdline_matches(cmd, &lab_process_markers("other", None)));
        // Prefix collision: `my` must not match `mylab`'s VMs.
        assert!(!cmdline_matches(cmd, &lab_process_markers("my", None)));
        // No marker at all.
        assert!(!cmdline_matches(b"sleep\x0030\x00", &markers));
    }

    /// The helpers a crashed daemon orphans carry the lab's paths mid-argument,
    /// not as an argv prefix — that is how they are recognised.
    #[test]
    fn helper_cmdline_matching() {
        let root = std::path::Path::new("/labs/mylab");
        let markers = lab_process_markers("mylab", Some(root));
        let run = crate::paths::lab_runtime_dir("mylab");
        let smb = crate::paths::lab_local_dir(root).join("smb");

        let swtpm = format!(
            "swtpm\0socket\0--tpm2\0--ctrl\0type=unixio,path={}/vms/web/swtpm.sock\0",
            run.display()
        );
        assert!(cmdline_matches(swtpm.as_bytes(), &markers), "swtpm");

        let vfsd = format!(
            "virtiofsd\0--socket-path\0{}/vms/web/virtiofs0.sock\0--shared-dir\0/srv\0",
            run.display()
        );
        assert!(cmdline_matches(vfsd.as_bytes(), &markers), "virtiofsd");

        let smbd = format!("smbd\0-F\0-s\0{}/smb.conf\0", smb.display());
        assert!(cmdline_matches(smbd.as_bytes(), &markers), "smbd");

        // Another lab's helpers are untouched.
        let other = lab_process_markers("otherlab", Some(std::path::Path::new("/labs/otherlab")));
        for argv in [&swtpm, &vfsd, &smbd] {
            assert!(!cmdline_matches(argv.as_bytes(), &other), "{argv}");
        }
    }

    #[test]
    fn kill_lab_orphans_reaps_qemu_and_helpers() {
        use std::os::unix::process::CommandExt;
        // Two real processes: one carrying the QEMU `-name` marker as argv[0],
        // one carrying the lab's runtime path the way a helper does (the binary
        // is `sleep` either way, so both just block).
        let lab = "orphan-reap-test";
        let run = crate::paths::lab_runtime_dir(lab);
        // Both block on an unread stdin pipe the test holds — no nested child,
        // so a reaped process leaves nothing behind. `; :` keeps the shell from
        // exec'ing the builtin away and losing our argv markers.
        let blocker = |arg0: String, marker: String| {
            std::process::Command::new("sh")
                .arg("-c")
                .arg("read line; :")
                .arg(marker)
                .arg0(arg0)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap()
        };
        let mut qemu = blocker(format!("vmlab:{lab}/vm0"), "-name".into());
        let mut helper = blocker(
            "swtpm".into(),
            format!("type=unixio,path={}/vms/vm0/swtpm.sock", run.display()),
        );
        // Let /proc settle, then reap both by lab name.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let killed = kill_lab_orphans(lab, None);
        let (qemu_status, helper_status) = (qemu.wait().unwrap(), helper.wait().unwrap());
        assert_eq!(killed, 2, "expected the VM and its helper reaped");
        assert!(!qemu_status.success(), "qemu should have been signalled");
        assert!(
            !helper_status.success(),
            "helper should have been signalled"
        );
        // A different lab name reaps nothing.
        assert_eq!(kill_lab_orphans("some-other-lab", None), 0);
    }
}
