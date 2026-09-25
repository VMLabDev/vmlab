//! Supervisor control (`vmlab daemon ...`) and the auto-start path every
//! other verb uses to reach the daemons (PRD §3, §12).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

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
            return Ok(client);
        }
        // Something holds the socket and will not serve: a supervisor on its
        // way out. A new one cannot bind until it has gone.
        if !wait_supervisor_gone(&sock).await {
            bail!("the running supervisor is still shutting down — try again shortly");
        }
    }

    spawn_supervisor()?;

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(client) = SupClient::connect(&sock).await
            && client.send(SupRequest::Ping {}).await.is_ok()
        {
            return Ok(client);
        }
    }
    bail!(
        "supervisor did not come up — check {}",
        crate::paths::state_dir().join("vmlabd.log").display()
    )
}

/// How long a supervisor gets to finish its teardown: every lab it holds is
/// released, then its background tasks get five seconds to stop.
const SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Wait until nothing is listening on the supervisor socket, or give up after
/// [`SHUTDOWN_WAIT`]. True when it has gone.
async fn wait_supervisor_gone(sock: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + SHUTDOWN_WAIT;
    while std::time::Instant::now() < deadline {
        if SupClient::connect(sock).await.is_err() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

fn spawn_supervisor() -> Result<()> {
    use std::os::unix::process::CommandExt;
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
    // New process group so the daemon survives the CLI's terminal.
    std::process::Command::new(exe)
        .arg("__supervisord")
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .process_group(0)
        .spawn()
        .context("spawning vmlabd")?;
    Ok(())
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
    Ok(LabClient::connect(&sock).await?)
}

/// Connect to a lab daemon only if it is already running.
pub async fn try_lab_daemon(name: &str) -> Option<LabClient> {
    let sock = crate::paths::lab_socket(name);
    let client = LabClient::connect(&sock).await.ok()?;
    client.send(LabRequest::Ping {}).await.ok()?;
    Some(client)
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
                        if wait_supervisor_gone(&sock).await {
                            println!("vmlabd stopped");
                        } else {
                            bail!(
                                "vmlabd is still shutting down after {}s",
                                SHUTDOWN_WAIT.as_secs()
                            );
                        }
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
