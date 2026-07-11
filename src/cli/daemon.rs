//! Supervisor control (`vmlab daemon ...`) and the auto-start path every
//! other verb uses to reach the daemons (PRD §3, §12).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::proto::client::Client;

/// Connect to the supervisor, auto-starting it if needed (PRD §3: one per
/// user, auto-started by the CLI).
pub async fn ensure_supervisor() -> Result<Client> {
    let sock = crate::paths::supervisor_socket();
    if let Ok(client) = Client::connect(&sock).await
        && client.call("ping", Value::Null).await.is_ok()
    {
        return Ok(client);
    }

    spawn_supervisor()?;

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(client) = Client::connect(&sock).await
            && client.call("ping", Value::Null).await.is_ok()
        {
            return Ok(client);
        }
    }
    bail!(
        "supervisor did not come up — check {}",
        crate::paths::state_dir().join("vmlabd.log").display()
    )
}

fn spawn_supervisor() -> Result<()> {
    use std::os::unix::process::CommandExt;
    // The supervisor + lab daemons live in the `vmlab` binary; a sibling like
    // `vmlab-web` must spawn that, not itself.
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
pub async fn ensure_lab_daemon(name: &str, root: &std::path::Path) -> Result<Client> {
    let supervisor = ensure_supervisor().await?;
    let resp = supervisor
        .call("lab.ensure", json!({"name": name, "root": root}))
        .await
        .map_err(|e| anyhow::anyhow!("starting lab daemon: {e}"))?;
    let sock = PathBuf::from(
        resp["socket"]
            .as_str()
            .context("malformed lab.ensure response")?,
    );
    Ok(Client::connect(&sock).await?)
}

/// Connect to a lab daemon only if it is already running.
pub async fn try_lab_daemon(name: &str) -> Option<Client> {
    let sock = crate::paths::lab_socket(name);
    let client = Client::connect(&sock).await.ok()?;
    client.call("ping", Value::Null).await.ok()?;
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
            .call("fastpath", Value::Null)
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
                match Client::connect(&sock).await {
                    Ok(client) => {
                        let _ = client.call("shutdown", Value::Null).await;
                        println!("vmlabd stopped");
                    }
                    Err(_) => println!("vmlabd is not running"),
                }
                Ok(())
            }
            DaemonCmd::Status => {
                let sock = crate::paths::supervisor_socket();
                match Client::connect(&sock).await {
                    Ok(client) => {
                        let version = client
                            .call("version", Value::Null)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        let labs = client
                            .call("status", Value::Null)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        println!(
                            "vmlabd {} at {}",
                            version.as_str().unwrap_or("?"),
                            sock.display()
                        );
                        if let Ok(fp) = client.call("fastpath", Value::Null).await {
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
