//! The per-lab daemon (PRD §3): owns the lab's QEMU processes, QMP/agent
//! channels, lab-local segments and network services, snapshots, state, and
//! events. One process per running lab, spawned and reaped by the
//! supervisor; the CLI talks to it directly for lab-scoped operations.

pub mod events;
pub mod lab;
pub mod network;
pub mod state;
pub mod vm;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::proto::server::{Handler, Server, Streamer};
use events::EventLog;
use lab::LabRuntime;

/// Entry point for `vmlab __labd --lab <name> --root <dir>`.
pub fn run(lab: String, root: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(lab, root))
}

async fn run_async(lab: String, root: PathBuf) -> Result<()> {
    let config = crate::config::load_lab_root(&root)
        .map_err(|e| anyhow::anyhow!("cannot load lab config: {e}"))?;
    anyhow::ensure!(
        config.lab.name == lab,
        "lab file at {} defines \"{}\", not \"{lab}\"",
        root.display(),
        config.lab.name
    );

    // The broadcast channel is shared between the protocol server (which
    // fans events out to subscribers) and the event log.
    let (events_tx, _) = tokio::sync::broadcast::channel(1024);
    let event_log = Arc::new(EventLog::new(&lab, events_tx.clone())?);

    let profiles = crate::profiles::ProfileSet::load_default()?;
    let runtime = LabRuntime::build(config, event_log, &profiles).await?;

    let sock = crate::paths::lab_socket(&lab);
    let handler: Arc<dyn Handler> = Arc::new(LabdHandler(runtime.clone()));
    let server = Server::bind_with_events(&sock, handler, events_tx.clone())
        .await
        .with_context(|| format!("binding {}", sock.display()))?;

    // Disk-space watchdog on the lab-local filesystem — linked clones grow
    // (PRD §8.1); matters even more on WSL2's growing VHDX (§13).
    let host_cfg = crate::config::host::HostConfig::load_default().unwrap_or_default();
    let wd_events = runtime.events.clone();
    let wd_path = runtime.lab_local.clone();
    crate::config::host::spawn_disk_watchdog(
        wd_path.clone(),
        host_cfg.disk_low_percent,
        std::time::Duration::from_secs(60),
        move |free| {
            wd_events.emit(
                "host.disk_low",
                json!({"path": wd_path, "free_percent": free}),
            );
        },
    );

    // Event → wisp handler bindings (PRD §8.2). Failures are logged, never
    // fatal.
    {
        let handlers = runtime.config.lab.handlers.clone();
        if !handlers.is_empty() {
            let mut rx = events_tx.subscribe();
            let runtime = runtime.clone();
            tokio::spawn(async move {
                loop {
                    let ev = match rx.recv().await {
                        Ok(ev) => ev,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    for h in handlers.iter().filter(|h| h.event == ev.event) {
                        let script = runtime.root.join(&h.run);
                        let event = crate::scripting::EventData {
                            name: ev.event.clone(),
                            vm: ev.data["vm"].as_str().unwrap_or_default().to_string(),
                            data: ev.data.to_string(),
                        };
                        let runtime = runtime.clone();
                        let output: crate::scripting::OutputSink = Arc::new(
                            |line| tracing::info!(target: "handler", "{}", line.trim_end()),
                        );
                        tokio::spawn(async move {
                            crate::scripting::run_event_handler(runtime, &script, event, output)
                                .await;
                        });
                    }
                }
            });
        }
    }

    tracing::info!("lab daemon for {lab} listening on {}", sock.display());
    futures::future::pending::<()>().await;
    drop(server);
    Ok(())
}

struct LabdHandler(Arc<LabRuntime>);

/// Output sink for provision/script runs: streamed live to the invoking CLI
/// and appended to the lab log (PRD §8.3).
fn stream_sink(lab: &Arc<LabRuntime>, stream: &Streamer) -> crate::scripting::OutputSink {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let streamer = stream.clone();
    let log_path = crate::paths::state_dir()
        .join("labs")
        .join(&lab.name)
        .join("lab.log");
    tokio::spawn(async move {
        use std::io::Write;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        while let Some(line) = rx.recv().await {
            if let Some(f) = log.as_mut() {
                let _ = write!(f, "{line}");
            }
            streamer.chunk(line).await;
        }
    });
    Arc::new(move |line: String| {
        let _ = tx.send(line);
    })
}

fn vm_arg(args: &Value) -> Result<String, String> {
    args["vm"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "missing vm".to_string())
}

fn vms_arg(args: &Value) -> Vec<String> {
    args["vms"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl Handler for LabdHandler {
    async fn handle(&self, cmd: &str, args: Value, _stream: &Streamer) -> Result<Value, String> {
        let lab = &self.0;
        let err = |e: anyhow::Error| format!("{e:#}");
        match cmd {
            "ping" => Ok(json!("pong")),
            "status" => Ok(lab.status().await),
            "up" => {
                let output = stream_sink(&self.0, _stream);
                lab.up(&vms_arg(&args), output).await.map_err(err)?;
                Ok(json!(true))
            }
            // Ad-hoc script against the lab (PRD §12: vmlab run).
            "run" => {
                let script = args["script"].as_str().ok_or("missing script")?;
                let path = lab.root.join(script);
                let output = stream_sink(&self.0, _stream);
                crate::scripting::run_script_file(lab.clone(), &path, output)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "down" => {
                let force = args["force"].as_bool().unwrap_or(false);
                lab.down(&vms_arg(&args), force).await.map_err(err)?;
                Ok(json!(true))
            }
            "destroy" => {
                lab.destroy().await.map_err(err)?;
                Ok(json!(true))
            }
            "vm.start" => {
                lab.start_vm(&vm_arg(&args)?).await.map_err(err)?;
                Ok(json!(true))
            }
            "vm.stop" => {
                let force = args["force"].as_bool().unwrap_or(false);
                lab.vm(&vm_arg(&args)?)
                    .map_err(err)?
                    .stop(force)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "vm.restart" => {
                let name = vm_arg(&args)?;
                let vm = lab.vm(&name).map_err(err)?.clone();
                vm.stop(false).await.map_err(err)?;
                // Wait for the exit monitor to settle, then boot again.
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                while vm.state().await != vm::PowerState::Stopped {
                    if tokio::time::Instant::now() > deadline {
                        return Err(format!("{name} did not stop for restart"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                lab.start_vm(&name).await.map_err(err)?;
                Ok(json!(true))
            }
            // Guest-agent exec (PRD §12: vmlab exec <vm> -- cmd).
            "vm.exec" => {
                let name = vm_arg(&args)?;
                let cmd = args["cmd"].as_str().ok_or("missing cmd")?;
                let cmd_args: Vec<String> = args["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let timeout =
                    std::time::Duration::from_secs(args["timeout"].as_u64().unwrap_or(120));
                let qga = lab.vm(&name).map_err(err)?.qga().await.map_err(err)?;
                let arg_refs: Vec<&str> = cmd_args.iter().map(String::as_str).collect();
                let result = qga
                    .exec(cmd, &arg_refs, true, timeout)
                    .await
                    .map_err(|e| format!("{e}"))?;
                Ok(json!({
                    "exit_code": result.exit_code,
                    "stdout": String::from_utf8_lossy(&result.stdout),
                    "stderr": String::from_utf8_lossy(&result.stderr),
                }))
            }
            "vm.ip" => {
                let name = vm_arg(&args)?;
                let nic = args["nic"].as_u64().map(|n| n as usize);
                let ip = lab
                    .vm(&name)
                    .map_err(err)?
                    .guest_ip(nic)
                    .await
                    .map_err(err)?;
                Ok(json!(ip))
            }
            "snapshot.take" => {
                let snap = args["name"].as_str().ok_or("missing name")?;
                match args["vm"].as_str() {
                    Some(vm) => {
                        let online = lab.snapshot(vm, snap).await.map_err(err)?;
                        Ok(json!({"online": online}))
                    }
                    None => lab.snapshot_all(snap).await.map_err(err),
                }
            }
            "snapshot.restore" => {
                let snap = args["name"].as_str().ok_or("missing name")?;
                match args["vm"].as_str() {
                    Some(vm) => {
                        lab.restore(vm, snap).await.map_err(err)?;
                    }
                    None => {
                        let names: Vec<String> = lab.vms.keys().cloned().collect();
                        for vm in names {
                            lab.restore(&vm, snap).await.map_err(err)?;
                        }
                    }
                }
                Ok(json!(true))
            }
            "snapshot.delete" => {
                let snap = args["name"].as_str().ok_or("missing name")?;
                lab.delete_snapshot(&vm_arg(&args)?, snap)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "snapshot.list" => lab.snapshots(&vm_arg(&args)?).await.map_err(err),
            "shutdown" => {
                tracing::info!("lab daemon shutdown requested");
                let lab = lab.clone();
                tokio::spawn(async move {
                    // Leave VMs as they are on plain shutdown? No — a lab
                    // daemon going away must not orphan QEMU processes it
                    // can no longer manage (PRD §3: the daemon owns them).
                    let _ = lab.down(&[], false).await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    std::process::exit(0);
                });
                Ok(json!(true))
            }
            _ => Err(format!("unknown command `{cmd}`")),
        }
    }
}
