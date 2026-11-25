//! Supervisor control (`vmlab daemon ...`) and the auto-start path every
//! other verb uses to reach the daemons (PRD §3, §12).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};

use crate::proto::client::{LabClient, SupClient};
use crate::proto::{LabRequest, ProtoError, SupRequest};

/// A daemon failure as an `anyhow` error that still carries its code, so
/// [`crate::cli::run`] can pick an exit code a script can branch on.
pub fn remote(e: ProtoError) -> anyhow::Error {
    anyhow::Error::new(crate::proto::CommandError::from(e))
}

/// Make a CLI-supplied path absolute against the cwd.
///
/// A relative path means "beside me" to whoever typed it, and no daemon is
/// standing where they are — each would resolve the same string against its
/// own working directory.
pub fn abs_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

/// Connect to the supervisor, auto-starting it if needed (PRD §3: one per
/// user, auto-started by the CLI).
pub async fn ensure_supervisor() -> Result<SupClient> {
    let sock = crate::paths::supervisor_socket();
    if let Ok(client) = SupClient::connect(&sock).await {
        if client.send(SupRequest::Ping {}).await.is_ok() {
            same_build(
                "the running vmlabd",
                client.send(SupRequest::Version {}).await.ok(),
            )?;
            return Ok(client);
        }
        // Something holds the socket and will not serve: a supervisor on its
        // way out. A new one cannot bind until it has gone.
        wait_supervisor_gone(&sock).await;
    }

    // The supervisor does not start on a host config that does not load.
    // Say why here, at once, and not after the wait for a process that has
    // exited.
    crate::config::host::HostConfig::load_default()
        .context("the host config is unreadable, and vmlabd does not start on it")?;

    let mut scoped = match spawn_supervisor(true) {
        Ok(child) => Some(child),
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
        {
            spawn_supervisor(false).context("spawning vmlabd")?;
            None
        }
        Err(err) => return Err(err).context("launching the vmlabd scope"),
    };

    let startup_wait = std::time::Duration::from_secs(10);
    let mut deadline = tokio::time::Instant::now() + startup_wait;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(client) = SupClient::connect(&sock).await
            && client.send(SupRequest::Ping {}).await.is_ok()
        {
            return Ok(client);
        }
        if let Some(child) = &mut scoped
            && let Some(status) = child.try_wait().context("checking the vmlabd scope")?
        {
            tracing::debug!(%status, "scoped vmlabd exited before readiness, trying direct spawn");
            spawn_supervisor(false).context("spawning vmlabd without a systemd scope")?;
            scoped = None;
            deadline = tokio::time::Instant::now() + startup_wait;
        }
    }
    bail!(
        "supervisor did not come up — check {}",
        crate::paths::state_dir().join("vmlabd.log").display()
    )
}

/// When `vmlab daemon stop` says it is still waiting: the supervisor releases
/// its labs one after the other, each stopping its guests.
const SHUTDOWN_NOTICE: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait until nothing is listening on the supervisor socket.
///
/// No deadline of its own, as `vmlab down` has none: the supervisor bounds
/// each lab's release by the stop wait that lab's daemon reports and kills
/// what outlives it, so a fixed wait here could only call a teardown still
/// in order a failure.
async fn wait_supervisor_gone(sock: &std::path::Path) {
    let notice = std::time::Instant::now() + SHUTDOWN_NOTICE;
    let mut noticed = false;
    while SupClient::connect(sock).await.is_ok() {
        if !noticed && std::time::Instant::now() >= notice {
            eprintln!("vmlabd is stopping its labs' machines; waiting for it to exit");
            noticed = true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn spawn_supervisor(scoped: bool) -> Result<Child> {
    // The supervisor + lab daemons live in the `vmlab` binary; resolve that
    // rather than assuming the current executable is it.
    let exe = crate::paths::vmlab_exe()?;
    crate::paths::ensure_dir(&crate::paths::state_dir())?;
    let log_path = crate::paths::state_dir().join("vmlabd.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let log_err = log.try_clone()?;
    Ok(supervisor_command(&exe, log, log_err, scoped).spawn()?)
}

/// The supervisor's spawn, minus the binary's own location.
///
/// Deliberately *not* `process_group(0)`: the supervisor's first act is
/// [`crate::supervisor::start_session`], and `setsid` refuses a process-group
/// leader. Its own session is what lets it — and the lab daemons it spawns,
/// which inherit that session — outlive the terminal the CLI ran in.
/// A transient scope also lets the process tree outlive the starter's
/// cgroup. Scope mode preserves the caller's environment and working directory.
fn supervisor_command(
    exe: &Path,
    log: std::fs::File,
    log_err: std::fs::File,
    scoped: bool,
) -> Command {
    let mut cmd = if scoped {
        let mut cmd = Command::new("systemd-run");
        cmd.args([
            "--user",
            "--scope",
            "--quiet",
            "--description=vmlab supervisor",
        ]);
        // Argument expansion in scope mode arrived with this option in v254.
        // Ordinary paths also work with older systemd releases.
        if exe.as_os_str().as_encoded_bytes().contains(&b'$') {
            cmd.arg("--expand-environment=no");
        }
        cmd.arg("--").arg(exe);
        cmd
    } else {
        Command::new(exe)
    };
    cmd.arg("__supervisord")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    cmd
}

/// Connect to a lab's daemon, starting the supervisor and lab daemon as
/// needed. Lab-scoped CLI verbs go through here, then talk to the lab
/// daemon directly (PRD §3: no proxying in the hot path).
pub async fn ensure_lab_daemon(name: &str, root: &std::path::Path) -> Result<LabClient> {
    let supervisor = ensure_supervisor().await?;
    let resp = supervisor
        .send(SupRequest::LabEnsure {
            name: name.to_string(),
            root: root.to_path_buf(),
        })
        .await
        .map_err(remote)
        .context("starting lab daemon")?;
    let sock = PathBuf::from(
        resp["socket"]
            .as_str()
            .context("malformed lab.ensure response")?,
    );
    let client = LabClient::connect(&sock).await?;
    same_build(
        &format!("lab \"{name}\"'s daemon"),
        client.send(LabRequest::Version {}).await.ok(),
    )?;
    Ok(client)
}

/// Refuse a daemon of another build than this binary (`crate::version`):
/// `answer` is what it said to `version`, or `None` where it could not say.
/// The daemons outlive the `vmlab` that started them, so a vmlab installed
/// over a running one meets daemons of the build before — which answered
/// with a `vmlab.wcl` digest that "changed" rather than with the real
/// difference, until this named it.
fn same_build(what: &str, answer: Option<serde_json::Value>) -> Result<()> {
    let theirs = answer.as_ref().and_then(|v| v.as_str());
    match crate::version::mismatch(what, crate::version::BUILD, theirs) {
        Some(why) => bail!(why),
        None => Ok(()),
    }
}

/// Refuse when `name` is registered from a root other than `root` — the same
/// conflict `lab.ensure` answers with (ADR-0011).
///
/// For the verbs that reach a cwd lab's daemon without going through
/// `lab.ensure` (`status`, `down`, `destroy`): a lab daemon's socket is keyed
/// by name, so without this a second checkout declaring the same name would
/// read, stop or destroy the first one's lab as if it were its own. No
/// supervisor running does not mean nothing is registered: a lab daemon
/// outlives a supervisor that crashed or was killed, so the registry the
/// supervisor persisted (`labs.json`) is read instead, as the next supervisor
/// would adopt it.
pub async fn check_registration(name: &str, root: &Path) -> Result<()> {
    use crate::supervisor::registry::{LabEntry, Registry};
    let labs: Vec<LabEntry> = match SupClient::connect(&crate::paths::supervisor_socket()).await {
        Ok(supervisor) => {
            let labs = supervisor
                .send(SupRequest::Status {})
                .await
                .map_err(remote)?;
            serde_json::from_value(labs)
                .context("the supervisor reported a registry vmlab cannot read")?
        }
        // Unreadable is not empty: the name may be registered from
        // another root, and this verb would then act on that lab.
        Err(_) => Registry::load()
            .context("cannot tell whether this lab's name is registered elsewhere")?
            .labs()
            .to_vec(),
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match labs.iter().find(|entry| entry.name == name) {
        Some(entry) => entry.claims(name, &root).map_err(anyhow::Error::new),
        None => Ok(()),
    }
}

/// [`try_lab_daemon`] for the lab in the current directory: `None` when its
/// daemon is not running, and the ADR-0011 conflict when the name is
/// registered from another root (see [`check_registration`]).
pub async fn try_own_lab_daemon(name: &str, root: &Path) -> Result<Option<LabClient>> {
    check_registration(name, root).await?;
    try_lab_daemon(name).await
}

/// Connect to a lab daemon only if it is already running: `None` when it is
/// not, and a refusal when one is running but is another build than this
/// `vmlab` (see [`same_build`]).
pub async fn try_lab_daemon(name: &str) -> Result<Option<LabClient>> {
    let sock = crate::paths::lab_socket(name);
    let Ok(client) = LabClient::connect(&sock).await else {
        return Ok(None);
    };
    if client.send(LabRequest::Ping {}).await.is_err() {
        return Ok(None);
    }
    same_build(
        &format!("lab \"{name}\"'s daemon"),
        client.send(LabRequest::Version {}).await.ok(),
    )?;
    Ok(Some(client))
}

/// `vmlab fastpath` — which network fast-path tier the supervisor selected
/// (PRD §9.1's substitutable backend) and why faster tiers were skipped.
/// Auto-starts the supervisor like every other verb: the answer is the
/// probe result of the daemon that will carry the traffic.
pub fn cmd_fastpath() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = ensure_supervisor().await?;
        let v = client
            .send(SupRequest::FastPath {})
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        println!(
            "network fast path: {} (mode {})",
            v["tier"].as_str().unwrap_or("?"),
            v["mode"].as_str().unwrap_or("?"),
        );
        if let Some(reasons) = v["reasons"].as_object() {
            for (tier, reason) in reasons {
                println!("  {tier} unavailable: {}", reason.as_str().unwrap_or("?"));
            }
        }
        Ok(())
    })
}

#[derive(clap::Subcommand)]
pub enum DaemonCmd {
    /// Start the supervisor (normally automatic)
    Start,
    /// Stop the supervisor and all lab daemons
    Stop,
    /// Show supervisor status and lab daemons
    Status,
}

pub fn cmd_daemon(cmd: DaemonCmd) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        match cmd {
            DaemonCmd::Start => {
                ensure_supervisor().await?;
                println!(
                    "vmlabd running at {}",
                    crate::paths::supervisor_socket().display()
                );
                Ok(())
            }
            DaemonCmd::Stop => {
                let sock = crate::paths::supervisor_socket();
                match SupClient::connect(&sock).await {
                    Ok(client) => {
                        let _ = client.send(SupRequest::Shutdown {}).await;
                        // The reply comes before the teardown, so "stopped"
                        // waits for the process to be gone.
                        wait_supervisor_gone(&sock).await;
                        println!("vmlabd stopped");
                    }
                    Err(_) => println!("vmlabd is not running"),
                }
                Ok(())
            }
            DaemonCmd::Status => {
                let sock = crate::paths::supervisor_socket();
                match SupClient::connect(&sock).await {
                    Ok(client) => {
                        let version = client
                            .send(SupRequest::Version {})
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        let labs = client
                            .send(SupRequest::Status {})
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        println!(
                            "vmlabd {} at {}",
                            version.as_str().unwrap_or("?"),
                            sock.display()
                        );
                        if let Ok(fp) = client.send(SupRequest::FastPath {}).await {
                            println!("network fast path: {}", fp["tier"].as_str().unwrap_or("?"));
                        }
                        let entries = labs.as_array().cloned().unwrap_or_default();
                        if entries.is_empty() {
                            println!("no lab daemons");
                        } else {
                            for l in entries {
                                println!(
                                    "  {} [{}] pid {} root {}",
                                    l["name"].as_str().unwrap_or("?"),
                                    l["state"].as_str().unwrap_or("?"),
                                    l["pid"],
                                    l["root"].as_str().unwrap_or("?"),
                                );
                            }
                        }
                        Ok(())
                    }
                    Err(_) => {
                        println!("vmlabd is not running");
                        Ok(())
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELPER_ENV: &str = "VMLAB_TEST_SESSION_HELPER";

    /// Stand-in for `vmlab __supervisord`: what the supervisor does first,
    /// then the session it ended up in. Only meaningful when spawned by
    /// [`supervisor_leaves_the_spawning_terminals_session`].
    #[test]
    #[ignore = "helper process for supervisor_leaves_the_spawning_terminals_session"]
    fn session_helper() {
        if std::env::var_os(HELPER_ENV).is_none() {
            return;
        }
        crate::supervisor::start_session();
        let sid = nix::unistd::getsid(None).unwrap();
        println!("SID={sid} PID={}", std::process::id());
    }

    /// Field 6 of `/proc/<pid>/stat`: the process's session id.
    fn proc_sid(pid: u32) -> i32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        // `comm` (field 2) may hold spaces; everything after its `)` splits.
        let rest = &stat[stat.rfind(')').unwrap() + 2..];
        rest.split(' ').nth(3).unwrap().parse().unwrap()
    }

    /// F23: the supervisor shared the launching terminal's session, so
    /// closing that terminal took every lab down. Spawned exactly as the CLI
    /// spawns it, it must end up leading a session of its own.
    #[test]
    fn supervisor_leaves_the_spawning_terminals_session() {
        let ours = proc_sid(std::process::id());
        let dir = tempfile::tempdir().unwrap();
        let log = std::fs::File::create(dir.path().join("log")).unwrap();
        let log_err = log.try_clone().unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut cmd = supervisor_command(&exe, log, log_err, false);
        cmd.args([
            "--exact",
            "cli::daemon::tests::session_helper",
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, "1");
        let status = cmd.as_std_mut().status().unwrap();
        assert!(status.success());
        let out = std::fs::read_to_string(dir.path().join("log")).unwrap();
        let line = out
            .lines()
            .find_map(|l| l.find("SID=").map(|i| &l[i..]))
            .unwrap_or_else(|| panic!("helper printed no session:\n{out}"));
        let (sid, pid) = line.split_once(' ').unwrap();
        let sid: i32 = sid.trim_start_matches("SID=").parse().unwrap();
        let pid: i32 = pid.trim_start_matches("PID=").parse().unwrap();
        assert_ne!(sid, ours, "the supervisor stayed in the spawner's session");
        assert_eq!(sid, pid, "the supervisor leads its own session");
    }

    const REGISTRATION_HELPER: &str = "VMLAB_TEST_REGISTRATION_HELPER";

    /// Lab-lifecycle F3: with no supervisor running, `check_registration`
    /// took that for "nothing is registered", so `down` and `destroy` in a
    /// second checkout reached the first checkout's daemon by name. The
    /// registration on disk must still hold. Run in a child with scratch XDG
    /// directories so the paths of this process are untouched.
    #[test]
    fn a_registration_holds_while_no_supervisor_answers() {
        let scratch = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::daemon::tests::registration_helper",
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(REGISTRATION_HELPER, "1")
            .env("XDG_RUNTIME_DIR", scratch.path().join("run"))
            .env("XDG_STATE_HOME", scratch.path().join("state"))
            .env("XDG_DATA_HOME", scratch.path().join("data"))
            .env("XDG_CONFIG_HOME", scratch.path().join("config"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "helper failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `mylab` registered from `/labs/first` in `labs.json`, no supervisor
    /// socket bound: a check from `/labs/second` is the ADR-0011 conflict,
    /// and a check from `/labs/first` or for another name passes.
    #[test]
    #[ignore = "subprocess helper for a_registration_holds_while_no_supervisor_answers"]
    fn registration_helper() {
        if std::env::var_os(REGISTRATION_HELPER).is_none() {
            return;
        }
        let state = crate::paths::state_dir();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("labs.json"),
            r#"{"labs":[{"name":"mylab","root":"/labs/first","pid":1,"state":"running"}]}"#,
        )
        .unwrap();
        assert!(!crate::paths::supervisor_socket().exists());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(check_registration("mylab", Path::new("/labs/second")))
            .expect_err("a second checkout passed while the first holds the name");
        let err = err
            .downcast_ref::<crate::proto::CommandError>()
            .expect("the ADR-0011 conflict");
        assert_eq!(err.code, crate::proto::error::ErrorCode::Conflict);
        assert!(err.message.contains("/labs/first"), "{}", err.message);
        rt.block_on(check_registration("mylab", Path::new("/labs/first")))
            .expect("the registered root holds its own name");
        rt.block_on(check_registration("otherlab", Path::new("/labs/second")))
            .expect("an unregistered name is free");
    }
}
