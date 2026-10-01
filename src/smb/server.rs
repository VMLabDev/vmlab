//! Running the bundled `smbd` unprivileged (PRD §7.5 strategy 2).
//!
//! ## Why no root is required
//!
//! Samba normally wants root to bind port 445, write under `/var/lib/samba`,
//! and switch uid per connection. This backend sidesteps all three:
//!
//! 1. **High port.** `smbd` listens on a port > 1024 (`smb ports =`), which any
//!    user may bind. The switch proxies the segment gateway's 445 onto it.
//! 2. **Relocated state.** Every Samba private/state/cache/lock/pid directory
//!    is moved somewhere the invoking user owns (see [`super::config`]): the
//!    persistent ones under the lab's `.vmlab/smb`, the ones smbd binds unix
//!    sockets in under a short per-lab directory in vmlab's runtime dir, so a
//!    lab in a deep directory cannot push a socket path past `sun_path`.
//! 3. **`force user`.** Each share accesses the host tree as the invoking unix
//!    user, so `smbd` never needs to switch to another uid.
//!
//! As a result the entire lifecycle — `smbpasswd` to create accounts, then
//! `smbd -F` to serve — runs as an ordinary user.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use thiserror::Error;

use super::config::{SmbConfig, check_nt1_supported};

#[derive(Debug, Error)]
pub enum SmbError {
    #[error("smbd binary not found on PATH (install Samba)")]
    SmbdMissing,
    #[error("pdbedit binary not found on PATH (install Samba)")]
    PdbeditMissing,
    #[error(
        "passdb account `{user}` is not a real Unix user — the unprivileged \
         tdbsam backend requires the SMB username to map to /etc/passwd"
    )]
    NotUnixUser { user: String },
    #[error(
        "a share requested smb1 (NT1) but this smbd build lacks SMB1 server support \
         (WITH_SMB1SERVER); the distro has trimmed it"
    )]
    Nt1Unsupported,
    #[error("failed to create smb state dir {path}: {source}")]
    StateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write smb.conf {path}: {source}")]
    WriteConf {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("creating passdb user `{user}` failed: {detail}")]
    CreateUser { user: String, detail: String },
    #[error("spawning smbd failed: {0}")]
    Spawn(std::io::Error),
    #[error("smbd exited immediately (code {code:?}): {reason}; check log {log}")]
    DiedOnStart {
        code: Option<i32>,
        /// smbd's own last words — its log normally holds nothing, because
        /// the startup failures it hits are logged below `log level = 1`.
        reason: String,
        log: PathBuf,
    },
}

type Result<T> = std::result::Result<T, SmbError>;

/// A running (or recently spawned) `smbd` instance for one lab.
#[derive(Debug)]
pub struct SmbServer {
    child: Option<Child>,
    config: SmbConfig,
}

impl SmbServer {
    /// Write the config, create the passdb accounts, and spawn `smbd`
    /// foregrounded on the configured high port.
    ///
    /// `creds_by_user` maps each `valid users` account name to its plaintext
    /// password (the same passwords plumbed into the guest mounts).
    pub fn spawn(config: SmbConfig, creds_by_user: HashMap<String, String>) -> Result<SmbServer> {
        // Fail fast if a share needs NT1 but the build dropped it.
        if config.any_smb1 && !check_nt1_supported() {
            return Err(SmbError::Nt1Unsupported);
        }

        // 1. Create the unprivileged state tree.
        std::fs::create_dir_all(&config.lab_dir).map_err(|source| SmbError::StateDir {
            path: config.lab_dir.clone(),
            source,
        })?;
        // The socket directories: private (they sit in the runtime dir, beside
        // vmlab's own control sockets) and bounded in length.
        let ncalrpc = config.ncalrpc_dir();
        crate::paths::ensure_private_dir(&config.run_dir)
            .and_then(|()| crate::paths::ensure_private_dir(&ncalrpc))
            .map_err(|e| SmbError::StateDir {
                path: ncalrpc.clone(),
                source: std::io::Error::other(format!("{e:#}")),
            })?;
        clear_stale_pidfile(&config);

        // 2. Write smb.conf.
        let conf_path = config.conf_path();
        std::fs::write(&conf_path, config.render_conf()).map_err(|source| SmbError::WriteConf {
            path: conf_path.clone(),
            source,
        })?;

        // 3. Create passdb users (unprivileged: tdbsam under lab_dir).
        for (user, pass) in &creds_by_user {
            create_user(&conf_path, user, pass)?;
        }

        // 4. Spawn smbd foregrounded.
        //    -F                 : run in the foreground (we own the child)
        //    --no-process-group : don't make a new pgrp, so our kill reaches it
        //    -s <conf>          : our lab-local config (this smbd build only
        //                         accepts the `-s` form, not `--configfile X`)
        //    -l <lab_dir>       : log basename under the lab dir, so smbd's
        //                         default `log.smbd` does not try to write the
        //                         root-owned /var/log/samba (would just warn).
        let log = config.log_path();
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|source| SmbError::StateDir {
                path: log.clone(),
                source,
            })?;
        let log_file2 = log_file.try_clone().map_err(|source| SmbError::StateDir {
            path: log.clone(),
            source,
        })?;

        let mut child = Command::new("smbd")
            .arg("-F")
            .arg("--no-process-group")
            .arg("-s")
            .arg(&conf_path)
            .arg("-l")
            .arg(&config.lab_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file2))
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SmbError::SmbdMissing
                } else {
                    SmbError::Spawn(e)
                }
            })?;

        // Give smbd a beat; if it immediately died (e.g. port in use), report.
        std::thread::sleep(std::time::Duration::from_millis(300));
        if let Ok(Some(status)) = child.try_wait() {
            let reason = diagnose_start_failure(&config)
                .or_else(|| last_log_line(&log))
                .unwrap_or_else(|| "smbd gave no reason".to_string());
            remove_run_dir(&config);
            return Err(SmbError::DiedOnStart {
                code: status.code(),
                reason,
                log,
            });
        }

        Ok(SmbServer {
            child: Some(child),
            config,
        })
    }

    pub fn listen_port(&self) -> u16 {
        self.config.listen_port
    }

    /// Kill the `smbd` child and reap it, then remove its socket directory
    /// from the runtime dir — nothing in it outlives the process.
    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        clear_stale_pidfile(&self.config);
        remove_run_dir(&self.config);
    }
}

impl Drop for SmbServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Remove Samba's lab-local pidfile only when it names a process that no
/// longer exists. A live PID (including an unrelated process after PID
/// reuse) is left untouched rather than risking interference.
fn clear_stale_pidfile(config: &SmbConfig) {
    let path = config.pid_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return;
    };
    if pid <= 0 {
        return;
    }
    if let Err(nix::errno::Errno::ESRCH) =
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
        && let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(path = %path.display(), %error, "failed to remove stale smbd pidfile");
    }
}

/// Remove the lab's smbd socket directory ([`SmbConfig::run_dir`]) unless its
/// pidfile still names a live process — another smbd for the same lab, which
/// is left alone exactly as [`clear_stale_pidfile`] leaves its pidfile.
fn remove_run_dir(config: &SmbConfig) {
    if config.pid_path().exists() {
        return;
    }
    if let Err(error) = std::fs::remove_dir_all(&config.run_dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(path = %config.run_dir.display(), %error, "failed to remove smbd run dir");
    }
}

/// Why smbd died at start. Its startup failures (`messaging_dgm_ref failed:
/// File name too long`, a port already bound) are logged at debug level 2,
/// below the `log level = 1` the server runs at, so they never reach its log
/// and stderr is silent. Re-run it once with that level on stdout and keep
/// its last word. Bounded: a re-run that does come up is killed.
fn diagnose_start_failure(config: &SmbConfig) -> Option<String> {
    let mut child = Command::new("smbd")
        .arg("-F")
        .arg("--no-process-group")
        .arg("-s")
        .arg(config.conf_path())
        .arg("-l")
        .arg(&config.lab_dir)
        .arg("--debug-stdout")
        .arg("-d")
        .arg("2")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = child.kill();
    let out = child.wait_with_output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    last_reason(&text)
}

/// The last line of smbd output that says something: not blank, not a debug
/// header (`[2026/10/01 11:05:00,  2] file.c:12(fn)`) and not the start-up
/// banner every run prints.
fn last_reason(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|l| {
            !l.is_empty()
                && !l.starts_with('[')
                && !l.starts_with("smbd version")
                && !l.starts_with("Copyright")
                && !l.starts_with("uid=")
        })
        .map(str::to_string)
}

fn last_log_line(log: &std::path::Path) -> Option<String> {
    last_reason(&std::fs::read_to_string(log).ok()?)
}

/// Create a passdb account with `pdbedit`, piping the password twice on stdin.
///
/// We use `pdbedit -s <conf> -a -u <user> -t` rather than `smbpasswd -a`
/// because, on a stock build, `smbpasswd -a`/`-L` is root-only; `pdbedit`
/// against our lab-local `-s <conf>` lands the account in the relocated tdbsam
/// with no root and without touching the system passdb. `-t` reads the new
/// password and its confirmation from stdin.
///
/// The account name **must** be a real Unix user (the tdbsam backend maps SMB
/// accounts to `/etc/passwd`); we pre-check that and surface a clear error.
fn create_user(conf_path: &PathBuf, user: &str, pass: &str) -> Result<()> {
    if !unix_user_exists(user) {
        return Err(SmbError::NotUnixUser {
            user: user.to_string(),
        });
    }

    let mut child = Command::new("pdbedit")
        .arg("-s")
        .arg(conf_path)
        .arg("-a") // add account
        .arg("-u")
        .arg(user)
        .arg("-t") // read password (and confirmation) from stdin
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SmbError::PdbeditMissing
            } else {
                SmbError::CreateUser {
                    user: user.to_string(),
                    detail: e.to_string(),
                }
            }
        })?;

    {
        let mut stdin = child.stdin.take().ok_or_else(|| SmbError::CreateUser {
            user: user.to_string(),
            detail: "no stdin handle".to_string(),
        })?;
        // pdbedit -t reads the new password and its confirmation.
        let payload = format!("{pass}\n{pass}\n");
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| SmbError::CreateUser {
                user: user.to_string(),
                detail: format!("writing password: {e}"),
            })?;
    }

    let out = child.wait_with_output().map_err(|e| SmbError::CreateUser {
        user: user.to_string(),
        detail: e.to_string(),
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(SmbError::CreateUser {
            user: user.to_string(),
            detail: stderr.trim().to_string(),
        });
    }
    Ok(())
}

/// Whether `user` exists in the system passwd database (`getent passwd`,
/// falling back to `id`).
fn unix_user_exists(user: &str) -> bool {
    if let Ok(out) = Command::new("getent").arg("passwd").arg(user).output() {
        return out.status.success();
    }
    Command::new("id")
        .arg(user)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::super::config::{ShareDef, SmbConfig};
    use super::*;
    use std::net::TcpStream;
    use std::time::Duration;

    fn free_high_port() -> u16 {
        // Bind :0 to grab a free port, then release it for smbd to reuse.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn test_config(dir: PathBuf) -> SmbConfig {
        SmbConfig {
            listen_port: 14450,
            run_dir: dir.join("run"),
            lab_dir: dir,
            any_smb1: false,
            shares: vec![],
        }
    }

    #[test]
    fn stale_smbd_pidfile_is_removed_but_live_pid_is_preserved() {
        let tmp = std::env::temp_dir().join(format!(
            "vmlab-smb-pid-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let config = test_config(tmp.clone());
        std::fs::create_dir_all(&config.run_dir).unwrap();

        std::fs::write(config.pid_path(), "2147483647\n").unwrap();
        clear_stale_pidfile(&config);
        assert!(!config.pid_path().exists());

        std::fs::write(config.pid_path(), format!("{}\n", std::process::id())).unwrap();
        clear_stale_pidfile(&config);
        assert!(config.pid_path().exists());

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn stop_kills_and_reaps_owned_child() {
        let tmp = std::env::temp_dir().join(format!(
            "vmlab-smb-stop-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id() as i32;
        let config = test_config(tmp.clone());
        std::fs::create_dir_all(config.ncalrpc_dir()).unwrap();
        let run_dir = config.run_dir.clone();
        let mut server = SmbServer {
            child: Some(child),
            config,
        };

        server.stop();
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        );
        // The socket directory goes with the process.
        assert!(!run_dir.exists());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn real_smbd_serves_a_share() {
        // smbd is present on this host; smbclient is the optional client.
        if which("smbd").is_none() {
            eprintln!("SKIP: smbd not found");
            return;
        }

        // A lab deep enough that the old layout (sockets under `.vmlab/smb`)
        // put `msg.sock/<pid>` past sun_path's 108 bytes and smbd died at start.
        let tmp = std::env::temp_dir().join(format!("vmlab-smb-test-{}", std::process::id()));
        let smb_dir = tmp.join("d".repeat(120)).join(".vmlab/smb");
        assert!(smb_dir.as_os_str().len() > 120);
        let share_dir = tmp.join("share");
        std::fs::create_dir_all(&share_dir).unwrap();
        std::fs::write(share_dir.join("hello.txt"), b"hi").unwrap();

        // The unprivileged passdb requires a real Unix account; use ours.
        let user = super::super::config::current_unix_user();
        let pass = "TestPass123abc";

        // Twice: the second start finds the passdb (kept with the lab) from
        // the first but a fresh runtime directory, as after any `down`/`up`.
        for round in 0..2 {
            let port = free_high_port();
            let config = SmbConfig {
                listen_port: port,
                run_dir: crate::paths::smb_runtime_dir(&smb_dir),
                lab_dir: smb_dir.clone(),
                any_smb1: false,
                shares: vec![ShareDef {
                    name: "testshare".to_string(),
                    host_path: share_dir.clone(),
                    readonly: true,
                    smb1: false,
                    allowed_user: user.to_string(),
                }],
            };

            let mut creds = HashMap::new();
            creds.insert(user.to_string(), pass.to_string());

            let server = match SmbServer::spawn(config, creds) {
                Ok(s) => s,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&tmp);
                    // The lab-path defect this layout fixes is never a skip.
                    assert!(!e.to_string().contains("too long"), "{e}");
                    eprintln!("SKIP: could not spawn smbd: {e}");
                    return;
                }
            };

            // Wait until the port accepts connections (smbd init can be slow).
            let mut connected = false;
            for _ in 0..50 {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    connected = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(connected, "round {round}: smbd never opened port {port}");

            if which("smbclient").is_none() {
                eprintln!("SKIP smbclient ls assertion: smbclient not found (smbd spawn OK)");
                drop(server);
                let _ = std::fs::remove_dir_all(&tmp);
                return;
            }

            let out = Command::new("smbclient")
                .arg("//127.0.0.1/testshare")
                .arg("-p")
                .arg(port.to_string())
                .arg("-U")
                .arg(format!("{user}%{pass}"))
                .arg("-c")
                .arg("ls")
                .output()
                .expect("run smbclient");

            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let run_dir = server.config.run_dir.clone();
            drop(server);
            assert!(!run_dir.exists(), "{} outlived smbd", run_dir.display());
            if !stdout.contains("hello.txt") {
                let _ = std::fs::remove_dir_all(&tmp);
                panic!(
                    "round {round}: smbclient ls did not list hello.txt.\n\
                     stdout:\n{stdout}\nstderr:\n{stderr}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn start_failure_carries_smbds_own_reason() {
        if which("smbd").is_none() {
            eprintln!("SKIP: smbd not found");
            return;
        }
        // Force the original defect: a socket directory too deep for
        // sun_path. smbd says why only at debug level 2, which the spawn's
        // diagnostic re-run recovers.
        let tmp = std::env::temp_dir().join(format!(
            "vmlab-smb-fail-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut config = test_config(tmp.join("smb"));
        config.listen_port = free_high_port();
        config.run_dir = tmp.join("r".repeat(120));
        let err = SmbServer::spawn(config.clone(), HashMap::new()).unwrap_err();
        let _ = std::fs::remove_dir_all(&tmp);
        match err {
            SmbError::DiedOnStart { reason, .. } => {
                assert!(reason.contains("File name too long"), "{reason}");
            }
            other => panic!("expected DiedOnStart, got {other}"),
        }
    }

    #[test]
    fn last_reason_skips_banner_and_debug_headers() {
        let out = "smbd version 4.24.6 started.\n\
                   Copyright Andrew Tridgell and the Samba Team 1992-2026\n\
                   [2026/10/01 11:05:00,  2] ../../source3/smbd/server.c:1(main)\n\
                   uid=1000 gid=1000 euid=1000 egid=1000\n\
                   messaging_dgm_ref failed: File name too long\n\n";
        assert_eq!(
            last_reason(out).as_deref(),
            Some("messaging_dgm_ref failed: File name too long")
        );
        assert_eq!(last_reason("smbd version 4.24.6 started.\n"), None);
    }

    fn which(bin: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(bin);
            if cand.is_file() {
                return Some(cand);
            }
        }
        None
    }
}
