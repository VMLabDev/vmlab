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
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
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
    #[error("refusing to write smb.conf: {0}")]
    Share(#[from] super::config::ShareRefused),
    #[error("failed to write smb.conf {path}: {source}")]
    WriteConf {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("creating passdb user `{user}` failed: {detail}")]
    CreateUser { user: String, detail: String },
    #[error("no free port is left for the SMB server of \"{machine}\"")]
    NoPort { machine: String },
    #[error("spawning smbd failed: {0}")]
    Spawn(std::io::Error),
    #[error("smbd failed to start (code {code:?}): {reason}; check log {log}")]
    DiedOnStart {
        code: Option<i32>,
        /// smbd's startup diagnostic.
        reason: String,
        log: PathBuf,
    },
}

type Result<T> = std::result::Result<T, SmbError>;

/// A running (or recently spawned) `smbd` instance, serving one machine.
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
        std::fs::write(&conf_path, config.render_conf()?).map_err(|source| {
            SmbError::WriteConf {
                path: conf_path.clone(),
                source,
            }
        })?;

        // The settings NT1 sessions alone get, where a share asks for smb1.
        // Without one the file is not included; a file left by an earlier
        // start is removed all the same.
        let nt1_path = config.nt1_conf_path();
        match config.render_nt1_conf() {
            Some(nt1) => std::fs::write(&nt1_path, nt1).map_err(|source| SmbError::WriteConf {
                path: nt1_path.clone(),
                source,
            })?,
            None => {
                let _ = std::fs::remove_file(&nt1_path);
            }
        }

        // 3. Create passdb users (unprivileged: tdbsam under lab_dir).
        for (user, pass) in &creds_by_user {
            create_user(&conf_path, user, pass)?;
        }
        forbid_password_change(&conf_path);

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
        let startup_logs = startup_logs(&config);

        let mut child = Command::new("smbd")
            .arg("-F")
            .arg("--no-process-group")
            .arg("-s")
            .arg(&conf_path)
            .arg("-l")
            .arg(&config.lab_dir)
            .env("LC_ALL", "C")
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

        if let Err(error) = wait_for_start(&mut child, &config, &startup_logs) {
            clear_stale_pidfile(&config);
            remove_run_dir(&config);
            return Err(error);
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

/// Give smbd time to report a bind failure even if another server already
/// accepts connections on the port.
const START_MIN: std::time::Duration = std::time::Duration::from_millis(300);
/// The longest wait for `smbd` to open its port or exit. A live `smbd` past
/// it counts as started, as one past [`START_MIN`] always did.
const START_MAX: std::time::Duration = std::time::Duration::from_secs(10);

/// Save the append offsets so a failed attempt cannot poison a port retry.
/// Samba can write to the default file from `-l` or the configured log.
fn startup_logs(config: &SmbConfig) -> [(PathBuf, u64); 2] {
    [config.lab_dir.join("log.smbd"), config.log_path()].map(|path| {
        let offset = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        (path, offset)
    })
}

fn bind_failure(log: &Path, offset: u64) -> Option<String> {
    let mut file = std::fs::File::open(log).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim)
        .find(|line| {
            (line.contains("open_socket_in_protocol failed")
                && line.contains("Address already in use"))
                || line.contains("PANIC: Could not bind to any sockets")
        })
        .map(str::to_owned)
}

/// A failed bind can leave the child alive while it dumps core. Check the
/// startup logs before the readiness probe or the slow-start fallback
/// (VML-61).
fn wait_for_start(child: &mut Child, config: &SmbConfig, logs: &[(PathBuf, u64)]) -> Result<()> {
    let started = std::time::Instant::now();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.listen_port));
    loop {
        let status = child.try_wait().ok().flatten();
        for (log, offset) in logs {
            if let Some(reason) = bind_failure(log, *offset) {
                let _ = child.kill();
                return Err(SmbError::DiedOnStart {
                    code: child.wait().ok().and_then(|status| status.code()),
                    reason,
                    log: log.clone(),
                });
            }
        }
        if let Some(status) = status {
            let log = config.log_path();
            let reason = diagnose_start_failure(config)
                .or_else(|| last_log_line(&log))
                .unwrap_or_else(|| "smbd gave no reason".to_string());
            return Err(SmbError::DiedOnStart {
                code: status.code(),
                reason,
                log,
            });
        }
        let waited = started.elapsed();
        if waited >= START_MAX {
            return Ok(());
        }
        if waited >= START_MIN
            && std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(100))
                .is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
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

/// Keep the account from changing its own password over SMB1's
/// `SamOEMChangePassword`, which is no RPC pipe and so is not covered by
/// `rpc start on demand helpers = no`: a minimum password age of 136 years.
/// `pdbedit -a` sets the password at each start whatever the age. Best
/// effort: a server whose policy could not be written still serves.
fn forbid_password_change(conf_path: &PathBuf) {
    let done = Command::new("pdbedit")
        .arg("-s")
        .arg(conf_path)
        .arg("-P")
        .arg("minimum password age")
        .arg("-C")
        .arg(u32::MAX.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match done {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(
            "smb: could not set the minimum password age: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::warn!("smb: could not set the minimum password age: {e}"),
    }
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

    fn hold_port() -> (socket2::Socket, u16) {
        let socket =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        socket
            .bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        let port = socket.local_addr().unwrap().as_socket().unwrap().port();
        (socket, port)
    }

    /// Start smbd on the first port of `ports` it can bind. A port from
    /// [`free_high_port`] is released before smbd binds it, so a parallel
    /// test can take it in between: smbd then panics with "Address already
    /// in use", and a core dump can keep it alive past the start wait. A
    /// start that lost its port is retried on the next one. The server is
    /// returned once its port accepts, or after 20 s for the caller to judge.
    fn spawn_on_a_free_port(
        mut ports: impl Iterator<Item = u16>,
        config_for: impl Fn(u16) -> SmbConfig,
        creds: &HashMap<String, String>,
    ) -> std::result::Result<SmbServer, SmbError> {
        for _ in 0..5 {
            let port = ports.next().expect("a port");
            let config = config_for(port);
            let logs = startup_logs(&config);
            let lost_its_port = || {
                logs.iter()
                    .any(|(path, offset)| bind_failure(path, *offset).is_some())
            };
            let mut server = match SmbServer::spawn(config, creds.clone()) {
                Ok(server) => server,
                Err(SmbError::DiedOnStart { .. }) if lost_its_port() => continue,
                Err(e) => return Err(e),
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Ok(server);
                }
                if lost_its_port() {
                    server.stop();
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(server);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        panic!("smbd lost its port five times");
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
    fn bind_failure_is_reported_while_the_child_is_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let (_held, port) = hold_port();
        let mut config = test_config(tmp.path().to_path_buf());
        config.listen_port = port;
        let log = config.lab_dir.join("log.smbd");
        std::fs::write(&log, "PANIC: Could not bind to any sockets\n").unwrap();
        let startup_logs = startup_logs(&config);
        let output = std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap();
        let reason = "smbd_open_one_socket: open_socket_in_protocol failed: Address already in use";
        let mut child = Command::new("sh")
            .args([
                "-eu",
                "-c",
                "printf '%s\\n' \"$1\"\nexec sleep 60",
                "sh",
                reason,
            ])
            .stdout(output)
            .spawn()
            .unwrap();

        let result = wait_for_start(&mut child, &config, &startup_logs);
        let reaped = child.try_wait().unwrap().is_some();
        if !reaped {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(
            matches!(result, Err(SmbError::DiedOnStart { reason: ref detail, log: ref path, .. })
                if detail == reason && path == &log),
            "{result:?}"
        );
        assert!(reaped, "the failed child must be killed and reaped");
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
        //
        // Round 0 is first offered a port held bound by another socket, as
        // a parallel test held 33477 when this failed on helios: the start
        // must move on to a free port rather than fail.
        let (_held, held_port) = hold_port();
        for round in 0..2 {
            let config_for = |port| SmbConfig {
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

            let first = (round == 0).then_some(held_port);
            let ports = first
                .into_iter()
                .chain(std::iter::repeat_with(free_high_port));
            let server = match spawn_on_a_free_port(ports, config_for, &creds) {
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
            let port = server.listen_port();
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

    /// docs/reviews/smb.md F1: a writable share whose directory holds
    /// `smb.conf` let the guest add a section of its own, which smbd read
    /// by itself, and then list any directory of the host user. The start
    /// refuses such a share. Should it ever start again, the body below is
    /// the guest's side of it and the listing must fail.
    #[test]
    fn a_share_that_holds_the_smb_state_cannot_reach_outside_it() {
        if which("smbd").is_none() || which("smbclient").is_none() {
            eprintln!("SKIP: smbd or smbclient not found");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let lab_root = tmp.path().join("lab");
        let smb_dir = lab_root.join(".vmlab/smb");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&smb_dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"s").unwrap();

        let user = super::super::config::current_unix_user();
        let pass = "TestPass123abc";
        let config_for = |port| SmbConfig {
            listen_port: port,
            run_dir: crate::paths::smb_runtime_dir(&smb_dir),
            lab_dir: smb_dir.clone(),
            any_smb1: false,
            shares: vec![ShareDef {
                name: "lab".to_string(),
                host_path: lab_root.clone(),
                readonly: false,
                smb1: false,
                allowed_user: user.to_string(),
            }],
        };
        let mut creds = HashMap::new();
        creds.insert(user.to_string(), pass.to_string());
        let ports = std::iter::repeat_with(free_high_port);
        let server = match spawn_on_a_free_port(ports, config_for, &creds) {
            Ok(server) => server,
            Err(SmbError::Share(refused)) => {
                assert!(refused.reason.contains("state"), "{refused}");
                return;
            }
            Err(e) => {
                eprintln!("SKIP: could not spawn smbd: {e}");
                return;
            }
        };

        // The guest's side: one more section, on a directory outside the
        // share, put over the server's own configuration.
        let port = server.listen_port();
        let mut conf = std::fs::read_to_string(server.config.conf_path()).unwrap();
        conf.push_str(&format!(
            "[esc]\n    path = {}\n    read only = no\n    valid users = {user}\n    \
             force user = {user}\n",
            outside.display()
        ));
        let evil = tmp.path().join("new.conf");
        std::fs::write(&evil, conf).unwrap();
        let smbclient = |share: &str, command: &str| {
            let out = Command::new("smbclient")
                .arg(format!("//127.0.0.1/{share}"))
                .arg("-p")
                .arg(port.to_string())
                .arg("-U")
                .arg(format!("{user}%{pass}"))
                .arg("-c")
                .arg(command)
                .output()
                .expect("run smbclient");
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr)
        };
        let put = smbclient(
            "lab",
            &format!("put {} .vmlab/smb/smb.conf", evil.display()),
        );
        // smbd looks at its configuration again by itself; give it a
        // bounded number of tries to do so.
        let mut reached = None;
        for _ in 0..40 {
            let listing = smbclient("esc", "ls");
            if listing.contains("secret.txt") {
                reached = Some(listing);
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        drop(server);
        assert!(
            reached.is_none(),
            "the guest rewrote smb.conf and listed a directory outside its share.\n\
             put:\n{put}\nls:\n{}",
            reached.unwrap_or_default()
        );
    }

    /// A lab with one `smb1` share (`old`) and one modern share (`new`), both
    /// writable, on a real smbd; `None` (with the reason printed) where the
    /// host cannot run it. The closure runs `smbclient` against a share with
    /// extra arguments and returns everything it printed.
    #[allow(clippy::type_complexity)]
    fn smb1_lab(
        tmp: &std::path::Path,
    ) -> Option<(SmbServer, impl Fn(&str, &[&str], &str) -> String + use<>)> {
        if which("smbd").is_none() || which("smbclient").is_none() {
            eprintln!("SKIP: smbd or smbclient not found");
            return None;
        }
        if !check_nt1_supported() {
            eprintln!("SKIP: this smbd has no SMB1 server");
            return None;
        }
        let smb_dir = tmp.join("lab/.vmlab/smb");
        let old_dir = tmp.join("old");
        let new_dir = tmp.join("new");
        for dir in [&smb_dir, &old_dir, &new_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(new_dir.join("hello.txt"), b"hi").unwrap();
        std::fs::write(old_dir.join("readme.txt"), b"hi").unwrap();

        let user = super::super::config::current_unix_user();
        let pass = "TestPass123abc";
        let share = |name: &str, dir: &PathBuf, smb1| ShareDef {
            name: name.to_string(),
            host_path: dir.clone(),
            readonly: false,
            smb1,
            allowed_user: user.clone(),
        };
        let config_for = |port| SmbConfig {
            listen_port: port,
            run_dir: crate::paths::smb_runtime_dir(&smb_dir),
            lab_dir: smb_dir.clone(),
            any_smb1: true,
            shares: vec![share("old", &old_dir, true), share("new", &new_dir, false)],
        };
        let mut creds = HashMap::new();
        creds.insert(user.clone(), pass.to_string());
        let ports = std::iter::repeat_with(free_high_port);
        let server = match spawn_on_a_free_port(ports, config_for, &creds) {
            Ok(server) => server,
            Err(e) => {
                eprintln!("SKIP: could not spawn smbd: {e}");
                return None;
            }
        };
        let port = server.listen_port();
        // `-s /dev/null`: the host's own smb.conf must not decide what the
        // client offers. The client is allowed down to NT1; `-m` picks the
        // newest dialect it offers.
        let smbclient = move |share: &str, args: &[&str], command: &str| {
            let out = Command::new("smbclient")
                .arg(format!("//127.0.0.1/{share}"))
                .args(["-s", "/dev/null", "--option=client min protocol=NT1"])
                .args(args)
                .arg("-p")
                .arg(port.to_string())
                .arg("-U")
                .arg(format!("{user}%{pass}"))
                .arg("-c")
                .arg(command)
                .output()
                .expect("run smbclient");
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr)
        };
        Some((server, smbclient))
    }

    /// docs/reviews/smb.md F4, point 2: Samba's SMB1 server offers the CIFS
    /// Unix extensions unless told not to, and a client that negotiates them
    /// creates a symlink with any target in a writable share. Every host
    /// program that reads the shared directory then follows it.
    #[test]
    fn an_smb1_client_cannot_create_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((server, smbclient)) = smb1_lab(tmp.path()) else {
            return;
        };
        // The share works over SMB1 at all, or the test proves nothing.
        let listing = smbclient("old", &["-m", "NT1"], "ls");
        let said = smbclient("old", &["-m", "NT1"], "posix; symlink /etc/passwd planted");
        drop(server);
        assert!(listing.contains("readme.txt"), "SMB1 ls failed:\n{listing}");
        let planted = tmp.path().join("old/planted");
        assert!(
            planted.symlink_metadata().is_err(),
            "an SMB1 client planted {} -> {:?}.\nsmbclient:\n{said}",
            planted.display(),
            std::fs::read_link(&planted).ok()
        );
    }

    /// docs/reviews/smb.md F4, point 1: `smb1 = true` is a property of the
    /// share. The share beside it is not reachable over SMB1, and no session
    /// newer than SMB1 logs on with NTLMv1.
    #[test]
    fn an_smb1_share_does_not_relax_the_share_beside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((server, smbclient)) = smb1_lab(tmp.path()) else {
            return;
        };
        let ntlmv1 = "--option=client ntlmv2 auth=no";
        let old_nt1 = smbclient("old", &["-m", "NT1", ntlmv1], "ls");
        let new_nt1 = smbclient("new", &["-m", "NT1"], "ls");
        let new_smb3 = smbclient("new", &["-m", "SMB3"], "ls");
        let new_smb2 = smbclient(
            "new",
            &["-m", "SMB2", "--option=client max protocol=SMB2_02"],
            "ls",
        );
        let new_smb3_ntlmv1 = smbclient("new", &["-m", "SMB3", ntlmv1], "ls");
        let old_smb3_ntlmv1 = smbclient("old", &["-m", "SMB3", ntlmv1], "ls");
        drop(server);
        // What `smb1 = true` asks for still works: SMB1 with NTLMv1.
        assert!(
            old_nt1.contains("readme.txt"),
            "the smb1 share refused SMB1 with NTLMv1:\n{old_nt1}"
        );
        // The modern share serves every modern dialect.
        assert!(new_smb3.contains("hello.txt"), "SMB3:\n{new_smb3}");
        assert!(new_smb2.contains("hello.txt"), "SMB2_02:\n{new_smb2}");
        // And nothing else.
        assert!(
            !new_nt1.contains("hello.txt"),
            "the modern share was mapped over SMB1:\n{new_nt1}"
        );
        for (share, said) in [("new", &new_smb3_ntlmv1), ("old", &old_smb3_ntlmv1)] {
            assert!(
                !said.contains(".txt"),
                "an SMB3 session logged on to \"{share}\" with NTLMv1:\n{said}"
            );
        }
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
        // Held bound but not listening for the whole test: nothing else can
        // listen there, so the port never answers and only smbd's own exit
        // ends the start wait (VML-61).
        let held =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        held.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        config.listen_port = held.local_addr().unwrap().as_socket().unwrap().port();
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

    /// A guest logs in with an ordinary Samba account, and Samba lets an
    /// account set its password over the SAMR pipe: by the change call, and
    /// by the administrator's set call too, because an unprivileged smbd
    /// takes every session for root. After either, the password vmlab hands
    /// out must still open the share (docs/reviews/smb.md F5).
    #[test]
    fn a_guest_cannot_change_the_lab_password() {
        if which("smbd").is_none() || which("smbclient").is_none() || which("rpcclient").is_none() {
            eprintln!("SKIP: smbd, smbclient or rpcclient not found");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let smb_dir = tmp.path().join("lab/.vmlab/smb");
        let shared = tmp.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("hello.txt"), b"hi").unwrap();

        let user = super::super::config::current_unix_user();
        let pass = "TestPass123abc";
        let config_for = |port| SmbConfig {
            listen_port: port,
            run_dir: crate::paths::smb_runtime_dir(&smb_dir),
            lab_dir: smb_dir.clone(),
            any_smb1: false,
            shares: vec![ShareDef {
                name: "ro".to_string(),
                host_path: shared.clone(),
                readonly: true,
                smb1: false,
                allowed_user: user.to_string(),
            }],
        };
        let mut creds = HashMap::new();
        creds.insert(user.to_string(), pass.to_string());
        let ports = std::iter::repeat_with(free_high_port);
        let server = match spawn_on_a_free_port(ports, config_for, &creds) {
            Ok(server) => server,
            Err(e) => {
                eprintln!("SKIP: could not spawn smbd: {e}");
                return;
            }
        };
        let port = server.listen_port();
        let run = |program: &str, args: &[&str]| {
            let out = Command::new(program)
                .args(args)
                .output()
                .unwrap_or_else(|e| panic!("run {program}: {e}"));
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr)
        };
        let port_arg = port.to_string();
        let login = format!("{user}%{pass}");
        let ls = || {
            run(
                "smbclient",
                &["//127.0.0.1/ro", "-p", &port_arg, "-U", &login, "-c", "ls"],
            )
        };
        let before = ls();
        let rpc = |command: String| {
            run(
                "rpcclient",
                &["-p", &port_arg, "-U", &login, "127.0.0.1", "-c", &command],
            )
        };
        let changed = rpc(format!("chgpasswd3 {user} {pass} NewPass456def"));
        let after_change = ls();
        let set = rpc(format!("setuserinfo2 {user} 23 OtherPass789ghi"));
        let after_set = ls();
        drop(server);
        assert!(
            before.contains("hello.txt"),
            "the share lists at all:\n{before}"
        );
        assert!(
            after_change.contains("hello.txt"),
            "a guest changed the password of the account.\nchgpasswd3:\n{changed}\nls:\n{after_change}"
        );
        assert!(
            after_set.contains("hello.txt"),
            "a guest set the password of the account.\nsetuserinfo2:\n{set}\nls:\n{after_set}"
        );
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
