//! The per-lab daemon (PRD §3): owns the lab's QEMU processes, QMP/agent
//! channels, lab-local segments and network services, snapshots, state, and
//! events. One process per running lab, spawned and reaped by the
//! supervisor; the CLI talks to it directly for lab-scoped operations.

pub mod container;
pub mod container_ctl;
pub mod events;
pub mod lab;
pub mod netservices;
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

    // Bridge any global segments to the supervisor (PRD §9.2). Best-effort:
    // a failure here is logged but doesn't abort the daemon (lab-local
    // segments still work).
    if let Err(e) = runtime.network.lock().await.attach_globals().await {
        tracing::warn!("attaching global segments: {e:#}");
    }

    // Long-lived background tasks register here so the `shutdown` command
    // can cancel and join them deterministically.
    let tasks = Arc::new(crate::lifecycle::TaskGroup::new());

    let sock = crate::paths::lab_socket(&lab);
    let handler: Arc<dyn Handler> = Arc::new(LabdHandler {
        lab: runtime.clone(),
        tasks: tasks.clone(),
    });
    let server = Server::bind_with_events(&sock, handler, events_tx.clone())
        .await
        .with_context(|| format!("binding {}", sock.display()))?;

    // Disk-space watchdog on the lab-local filesystem — linked clones grow
    // (PRD §8.1); matters even more on WSL2's growing VHDX (§13).
    let host_cfg = crate::config::host::HostConfig::load_default().unwrap_or_default();
    let wd_events = runtime.events.clone();
    let wd_path = runtime.lab_local.clone();
    let watchdog = crate::config::host::spawn_disk_watchdog(
        wd_path.clone(),
        host_cfg.disk_low_percent,
        std::time::Duration::from_secs(60),
        tasks.cancel_token(),
        move |free| {
            wd_events.emit(
                "host.disk_low",
                json!({"path": wd_path, "free_percent": free}),
            );
        },
    );
    tasks.adopt("disk-watchdog", watchdog);

    // Event → wscript handler bindings (PRD §8.2). Failures are logged, never
    // fatal.
    {
        let handlers = runtime.config.lab.handlers.clone();
        if !handlers.is_empty() {
            let mut rx = events_tx.subscribe();
            let runtime = runtime.clone();
            let group = tasks.clone();
            let cancel = tasks.cancel_token();
            tasks.spawn("handler-dispatch", async move {
                loop {
                    let ev = tokio::select! {
                        _ = cancel.cancelled() => break,
                        ev = rx.recv() => match ev {
                            Ok(ev) => ev,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        },
                    };
                    for h in handlers.iter().filter(|h| h.event == ev.event) {
                        let script = runtime.root.join(&h.run);
                        // Container events carry the name under "container";
                        // handlers read it from `event.vm` either way (the
                        // full payload is in `event.data`).
                        let machine = ev.data["vm"]
                            .as_str()
                            .or_else(|| ev.data["container"].as_str())
                            .unwrap_or_default();
                        let event = crate::scripting::EventData {
                            name: ev.event.clone(),
                            vm: machine.to_string(),
                            data: ev.data.to_string(),
                        };
                        let runtime = runtime.clone();
                        let output: crate::scripting::OutputSink = Arc::new(
                            |line| tracing::info!(target: "handler", "{}", line.trim_end()),
                        );
                        // Registered so shutdown waits (bounded) for
                        // in-flight handler scripts instead of killing them
                        // mid-run at process exit.
                        group.spawn("handler-run", async move {
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

struct LabdHandler {
    lab: Arc<LabRuntime>,
    /// The daemon's background tasks, cancelled + joined on `shutdown`.
    tasks: Arc<crate::lifecycle::TaskGroup>,
}

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

fn container_arg(args: &Value) -> Result<String, String> {
    args["container"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "missing container".to_string())
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

/// Optional `region` arg as `[x, y, w, h]` (absent/null → whole screen).
fn region_arg(args: &Value) -> Result<Option<(u32, u32, u32, u32)>, String> {
    match args["region"].as_array() {
        None if args["region"].is_null() => Ok(None),
        None => Err("region must be [x, y, w, h]".to_string()),
        Some(r) if r.len() == 4 => {
            let v = |i: usize| r[i].as_i64().unwrap_or(0).max(0) as u32;
            Ok(Some((v(0), v(1), v(2), v(3))))
        }
        Some(r) => Err(format!(
            "region needs [x, y, w, h], got {} elements",
            r.len()
        )),
    }
}

#[async_trait::async_trait]
impl Handler for LabdHandler {
    async fn handle(&self, cmd: &str, args: Value, _stream: &Streamer) -> Result<Value, String> {
        let lab = &self.lab;
        let err = |e: anyhow::Error| format!("{e:#}");
        match cmd {
            "ping" => Ok(json!("pong")),
            "status" => Ok(lab.status().await),
            "up" => {
                let output = stream_sink(&self.lab, _stream);
                lab.up(&vms_arg(&args), output).await.map_err(err)?;
                Ok(json!(true))
            }
            // Ad-hoc script against the lab (PRD §12: vmlab script).
            "run" => {
                let script = args["script"].as_str().ok_or("missing script")?;
                let path = lab.root.join(script);
                let output = stream_sink(&self.lab, _stream);
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
                let vm = vm_arg(&args)?;
                lab.preflight_binaries(std::slice::from_ref(&vm))
                    .map_err(err)?;
                lab.start_vm(&vm).await.map_err(err)?;
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
            "vm.destroy" => {
                lab.destroy_vm(&vm_arg(&args)?).await.map_err(err)?;
                Ok(json!(true))
            }
            // VM interaction (PRD §10.3: vmlab vm screenshot/sendkeys/mouse/…).
            "vm.screenshot" => {
                let name = vm_arg(&args)?;
                let path = args["path"].as_str().ok_or("missing path")?;
                let vm = lab.vm(&name).map_err(err)?;
                crate::scripting::interact::screenshot(vm, std::path::Path::new(path))
                    .await
                    .map_err(err)?;
                Ok(json!({"path": path}))
            }
            "vm.sendkeys" => {
                let name = vm_arg(&args)?;
                let keys = args["keys"].as_str().ok_or("missing keys")?;
                let vm = lab.vm(&name).map_err(err)?;
                crate::scripting::interact::send_keys(vm, keys)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "vm.mouse_move" => {
                let name = vm_arg(&args)?;
                let x = args["x"].as_i64().ok_or("missing x")?;
                let y = args["y"].as_i64().ok_or("missing y")?;
                let vm = lab.vm(&name).map_err(err)?;
                crate::scripting::interact::mouse_move(vm, x, y)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "vm.mouse_click" => {
                let name = vm_arg(&args)?;
                let button = args["button"].as_str().unwrap_or("left");
                let at = match (args["x"].as_i64(), args["y"].as_i64()) {
                    (Some(x), Some(y)) => Some((x, y)),
                    _ => None,
                };
                let vm = lab.vm(&name).map_err(err)?;
                crate::scripting::interact::mouse_click(vm, button, at)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "vm.mouse_drag" => {
                let name = vm_arg(&args)?;
                let x1 = args["x1"].as_i64().ok_or("missing x1")?;
                let y1 = args["y1"].as_i64().ok_or("missing y1")?;
                let x2 = args["x2"].as_i64().ok_or("missing x2")?;
                let y2 = args["y2"].as_i64().ok_or("missing y2")?;
                let vm = lab.vm(&name).map_err(err)?;
                crate::scripting::interact::mouse_drag(vm, x1, y1, x2, y2)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "vm.ocr" => {
                let name = vm_arg(&args)?;
                let region = region_arg(&args)?;
                let vm = lab.vm(&name).map_err(err)?;
                let text = crate::scripting::interact::ocr(vm, region)
                    .await
                    .map_err(err)?;
                Ok(json!(text))
            }
            "vm.find_image" => {
                let name = vm_arg(&args)?;
                let image = args["image"].as_str().ok_or("missing image")?;
                let threshold = args["threshold"].as_f64().unwrap_or(0.9);
                let region = region_arg(&args)?;
                let opts = crate::vision::MatchOptions { threshold, region };
                let vm = lab.vm(&name).map_err(err)?;
                let found =
                    crate::scripting::interact::find_image(vm, &[PathBuf::from(image)], &opts)
                        .await
                        .map_err(err)?;
                Ok(match found {
                    Some(m) => {
                        let (cx, cy) = m.center();
                        json!({"x": m.x, "y": m.y, "w": m.w, "h": m.h,
                               "score": m.score, "cx": cx, "cy": cy})
                    }
                    None => Value::Null,
                })
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
            // Guest OS identification (PRD §12: vmlab osinfo, vmlab cp).
            "vm.osinfo" => {
                let name = vm_arg(&args)?;
                let timeout =
                    std::time::Duration::from_secs(args["timeout"].as_u64().unwrap_or(30));
                let qga = lab.vm(&name).map_err(err)?.qga().await.map_err(err)?;
                qga.get_osinfo(timeout).await.map_err(|e| format!("{e}"))
            }
            // Guest-agent file write (PRD §12: vmlab cp). `append` lets the
            // CLI move large files in several modest JSON-line messages
            // instead of one giant one.
            "vm.copy_in" => {
                let name = vm_arg(&args)?;
                let dest = args["dest"].as_str().ok_or("missing dest")?;
                let data = args["data"].as_str().ok_or("missing data")?;
                let append = args["append"].as_bool().unwrap_or(false);
                let bytes = {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|e| format!("invalid base64 data: {e}"))?
                };
                let timeout =
                    std::time::Duration::from_secs(args["timeout"].as_u64().unwrap_or(120));
                let qga = lab.vm(&name).map_err(err)?.qga().await.map_err(err)?;
                let result = if append {
                    qga.file_append(dest, &bytes, timeout).await
                } else {
                    qga.file_write(dest, &bytes, timeout).await
                };
                result.map_err(|e| format!("{e}"))?;
                Ok(json!(true))
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
            // Container lifecycle (mirrors the vm.* verbs; PRD §18).
            "container.start" => {
                let name = container_arg(&args)?;
                lab.preflight_binaries(std::slice::from_ref(&name))
                    .map_err(err)?;
                lab.start_container(&name).await.map_err(err)?;
                Ok(json!(true))
            }
            "container.stop" => {
                let force = args["force"].as_bool().unwrap_or(false);
                lab.container(&container_arg(&args)?)
                    .map_err(err)?
                    .stop(force)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "container.restart" => {
                let name = container_arg(&args)?;
                let c = lab.container(&name).map_err(err)?.clone();
                c.stop(false).await.map_err(err)?;
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                while c.state().await != vm::PowerState::Stopped {
                    if tokio::time::Instant::now() > deadline {
                        return Err(format!("{name} did not stop for restart"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                lab.start_container(&name).await.map_err(err)?;
                Ok(json!(true))
            }
            "container.destroy" => {
                lab.destroy_container(&container_arg(&args)?)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "container.exec" => {
                let name = container_arg(&args)?;
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
                let c = lab.container(&name).map_err(err)?;
                let arg_refs: Vec<&str> = cmd_args.iter().map(String::as_str).collect();
                let result = c.exec(cmd, &arg_refs, timeout).await.map_err(err)?;
                Ok(json!({
                    "exit_code": result.exit_code,
                    "stdout": String::from_utf8_lossy(&result.stdout),
                    "stderr": String::from_utf8_lossy(&result.stderr),
                }))
            }
            "container.logs" => {
                let name = container_arg(&args)?;
                let lines = args["lines"].as_u64().unwrap_or(100) as usize;
                let c = lab.container(&name).map_err(err)?;
                if !args["follow"].as_bool().unwrap_or(false) {
                    return Ok(json!(c.logs(lines).map_err(err)?));
                }
                // Follow: stream the tail, then poll the console log for
                // growth until the client hangs up or the container stops.
                let path = c.dirs.console_log();
                _stream.chunk(c.logs(lines).map_err(err)?).await;
                let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let c = c.clone();
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    if len > offset {
                        use std::io::{Read, Seek};
                        let chunk = std::fs::File::open(&path)
                            .and_then(|mut f| {
                                f.seek(std::io::SeekFrom::Start(offset))?;
                                let mut buf = String::new();
                                f.read_to_string(&mut buf)?;
                                Ok(buf)
                            })
                            .unwrap_or_default();
                        offset = len;
                        if !chunk.is_empty() {
                            _stream.chunk(chunk).await;
                        }
                    }
                    if c.state().await == vm::PowerState::Stopped {
                        break;
                    }
                }
                Ok(json!(true))
            }
            "container.ip" => {
                let ip = lab
                    .container(&container_arg(&args)?)
                    .map_err(err)?
                    .guest_ip()
                    .await
                    .map_err(err)?;
                Ok(json!(ip))
            }
            "container.copy_in" => {
                let name = container_arg(&args)?;
                let dest = args["dest"].as_str().ok_or("missing dest")?;
                let data = args["data"].as_str().ok_or("missing data")?;
                let bytes = {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|e| format!("invalid base64 data: {e}"))?
                };
                let timeout =
                    std::time::Duration::from_secs(args["timeout"].as_u64().unwrap_or(120));
                let c = lab.container(&name).map_err(err)?;
                let tmp = std::env::temp_dir().join(format!("vmlab-cp-{}", std::process::id()));
                std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
                let res = c.copy_to(&tmp, dest, timeout).await;
                let _ = std::fs::remove_file(&tmp);
                res.map_err(err)?;
                Ok(json!(true))
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
                let tasks = self.tasks.clone();
                tokio::spawn(async move {
                    // A lab daemon going away must not orphan QEMU processes
                    // it can no longer manage (PRD §3: the daemon owns them),
                    // and must release its global-segment references so the
                    // supervisor can reap shared switches (§9.2).
                    let _ = lab.down(&[], false).await;
                    // Stop the lab's SMB server cleanly.
                    if let Some(mut smb) = lab.smb.lock().await.take() {
                        smb.stop();
                    }
                    lab.network.lock().await.detach_globals().await;
                    // Cancel + join the daemon's background tasks (watchdog,
                    // handler dispatch, in-flight handler scripts — the
                    // `down` above may have spawned some for its final
                    // events), so exit doesn't kill work mid-flight.
                    tasks.shutdown(std::time::Duration::from_secs(5)).await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    std::process::exit(0);
                });
                Ok(json!(true))
            }
            _ => Err(format!("unknown command `{cmd}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::region_arg;
    use serde_json::json;

    #[test]
    fn region_arg_parses_and_validates() {
        assert_eq!(region_arg(&json!({})).unwrap(), None);
        assert_eq!(region_arg(&json!({"region": null})).unwrap(), None);
        assert_eq!(
            region_arg(&json!({"region": [1, 2, 3, 4]})).unwrap(),
            Some((1, 2, 3, 4))
        );
        // Negative values clamp to 0.
        assert_eq!(
            region_arg(&json!({"region": [-5, 2, 3, 4]})).unwrap(),
            Some((0, 2, 3, 4))
        );
        assert!(region_arg(&json!({"region": [1, 2, 3]})).is_err());
        assert!(region_arg(&json!({"region": "nope"})).is_err());
    }
}
