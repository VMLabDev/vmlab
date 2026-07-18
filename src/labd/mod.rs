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
pub mod playbook;
pub mod state;
pub mod vm;
pub mod vm_agent;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::proto::server::{Handler, Server, Streamer};
use events::EventLog;
use lab::LabRuntime;

fn handler_matches(handler: &crate::config::model::Handler, event: &str, machine: &str) -> bool {
    handler.event == event
        && (handler.targets.is_empty() || handler.targets.iter().any(|name| name == machine))
}

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

    // Select the network fast-path tier (PRD §9.1) before the runtime builds
    // any switches; the host config is reused for the disk watchdog below.
    let host_cfg = crate::config::host::HostConfig::load_default().unwrap_or_default();
    crate::net::fastpath::init(host_cfg.fastpath);

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
                    // Container events carry the name under "container";
                    // handlers read it from `event.vm` either way.
                    let machine = ev.data["vm"]
                        .as_str()
                        .or_else(|| ev.data["container"].as_str())
                        .unwrap_or_default();
                    for h in handlers
                        .iter()
                        .filter(|handler| handler_matches(handler, &ev.event, machine))
                    {
                        let script = runtime.root.join(&h.run);
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
            // Download any pending templates/images without starting anything
            // (the web UI's "Download templates" button, `vmlab pull`). The
            // exact code path `up` runs first — same progress events.
            "pull" => {
                let output = stream_sink(&self.lab, _stream);
                lab.ensure_pulled(&vms_arg(&args), Some(&output))
                    .await
                    .map_err(err)?;
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
                // Pull with CLI-visible progress before the preflight (the
                // pulled meta can change the resolved firmware/TPM needs);
                // start_vm's internal pull is then a no-op.
                let output = stream_sink(&self.lab, _stream);
                lab.ensure_pulled(std::slice::from_ref(&vm), Some(&output))
                    .await
                    .map_err(err)?;
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
                let force = args["force"].as_bool().unwrap_or(false);
                let vm = lab.vm(&name).map_err(err)?.clone();
                vm.stop(force).await.map_err(err)?;
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
                let vm = lab.vm(&name).map_err(err)?;
                // Prefer the vmlab-agent transport (streamed, no 100ms QGA
                // polling, no base64); fall back to QGA for guests whose
                // template predates the agent.
                if let Ok(agent) = vm.agent().await {
                    let mut argv = vec![cmd.to_string()];
                    argv.extend(cmd_args.iter().cloned());
                    let result = agent
                        .exec(argv, vec![], None, None, timeout)
                        .await
                        .map_err(err)?;
                    return Ok(json!({
                        "exit_code": result.exit_code,
                        "stdout": String::from_utf8_lossy(&result.stdout),
                        "stderr": String::from_utf8_lossy(&result.stderr),
                    }));
                }
                let qga = vm.qga().await.map_err(err)?;
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
            // Interactive terminal over the vmlab-agent channel: opens a
            // fresh session (multi-session — every attach gets its own
            // shell), re-exposed as a raw-byte unix socket clients connect
            // to directly; resize rides the agent's control channel.
            "vm.tty_open" => {
                let name = vm_arg(&args)?;
                let cols = args["cols"].as_u64().unwrap_or(80) as u16;
                let rows = args["rows"].as_u64().unwrap_or(24) as u16;
                let vm = lab.vm(&name).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                let session = agent.open_terminal(cols, rows, None).await.map_err(err)?;
                let id = session.id;
                let path = vm.dirs.term_session_sock(id);
                vm_agent::expose_terminal_socket(session, path.clone())
                    .await
                    .map_err(err)?;
                Ok(json!({"session": id, "path": path}))
            }
            "vm.tty_resize" => {
                let name = vm_arg(&args)?;
                let session = args["session"].as_u64().ok_or("missing session")? as u32;
                let cols = args["cols"].as_u64().unwrap_or(80) as u16;
                let rows = args["rows"].as_u64().unwrap_or(24) as u16;
                let vm = lab.vm(&name).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                agent.resize(session, cols, rows).await.map_err(err)?;
                Ok(json!(true))
            }
            "vm.agent_info" => {
                let vm = lab.vm(&vm_arg(&args)?).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                let info = agent.info();
                Ok(json!({
                    "version": info.agent_version,
                    "os": info.os,
                    "features": info.features,
                }))
            }
            // Fast binary file transfer over the agent channel (host paths
            // are the daemon's — the CLI resolves them absolute first).
            "vm.push_file" => {
                let name = vm_arg(&args)?;
                let from = args["from"].as_str().ok_or("missing from")?;
                let to = args["to"].as_str().ok_or("missing to")?;
                let mode = args["mode"].as_u64().map(|m| m as u32);
                let vm = lab.vm(&name).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                let (sha256, len) = agent
                    .push_file(std::path::Path::new(from), to, mode)
                    .await
                    .map_err(err)?;
                Ok(json!({"sha256": sha256, "len": len}))
            }
            "vm.pull_file" => {
                let name = vm_arg(&args)?;
                let from = args["from"].as_str().ok_or("missing from")?;
                let to = args["to"].as_str().ok_or("missing to")?;
                let vm = lab.vm(&name).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                let (sha256, len) = agent
                    .pull_file(from, std::path::Path::new(to))
                    .await
                    .map_err(err)?;
                Ok(json!({"sha256": sha256, "len": len}))
            }
            // Follow a guest file (tail -F semantics), streamed as chunks
            // until the client hangs up or the VM stops.
            "vm.tail" => {
                let name = vm_arg(&args)?;
                let path = args["path"].as_str().ok_or("missing path")?;
                let vm = lab.vm(&name).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                let mut session = agent.open_tail(path.to_string()).await.map_err(err)?;
                loop {
                    tokio::select! {
                        ev = session.recv() => match ev {
                            Some(vm_agent::SessionEvent::Data(b)) => {
                                _stream.chunk(String::from_utf8_lossy(&b).into_owned()).await;
                            }
                            Some(vm_agent::SessionEvent::Error(msg)) => return Err(msg),
                            None => break,
                            Some(_) => {}
                        },
                        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                            if vm.state().await != vm::PowerState::Running {
                                break;
                            }
                        }
                    }
                }
                session.close().await;
                Ok(json!(true))
            }
            // Windows event log follow (agent `eventlog` feature).
            "vm.eventlog" => {
                let name = vm_arg(&args)?;
                let filter = args["filter"].as_str().map(String::from);
                let vm = lab.vm(&name).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                if !agent.has_feature(vmlab_agent_proto::features::EVENTLOG) {
                    return Err(format!(
                        "{name}: the guest agent has no event log (Windows-only feature)"
                    ));
                }
                let mut session = agent.open_eventlog(filter).await.map_err(err)?;
                loop {
                    tokio::select! {
                        ev = session.recv() => match ev {
                            Some(vm_agent::SessionEvent::Data(b)) => {
                                _stream.chunk(String::from_utf8_lossy(&b).into_owned()).await;
                            }
                            Some(vm_agent::SessionEvent::Error(msg)) => return Err(msg),
                            None => break,
                            Some(_) => {}
                        },
                        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                            if vm.state().await != vm::PowerState::Running {
                                break;
                            }
                        }
                    }
                }
                session.close().await;
                Ok(json!(true))
            }
            // Latest guest metrics (subscribes the 2s sampler on first use).
            "vm.stats" => {
                let vm = lab.vm(&vm_arg(&args)?).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                let m = agent
                    .stats(std::time::Duration::from_secs(10))
                    .await
                    .map_err(err)?;
                Ok(json!({
                    "cpu_pct": m.cpu_pct,
                    "mem_used": m.mem_used,
                    "mem_total": m.mem_total,
                    "disks": m.disks.iter().map(|d| json!({
                        "mount": d.mount, "used": d.used, "total": d.total,
                    })).collect::<Vec<_>>(),
                }))
            }
            "vm.clipboard_get" => {
                let vm = lab.vm(&vm_arg(&args)?).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                let text = agent
                    .get_clipboard(std::time::Duration::from_secs(10))
                    .await
                    .map_err(err)?;
                Ok(json!(text))
            }
            "vm.clipboard_set" => {
                let text = args["text"].as_str().ok_or("missing text")?;
                let vm = lab.vm(&vm_arg(&args)?).map_err(err)?;
                let agent = vm.agent().await.map_err(err)?;
                agent.set_clipboard(text.to_string()).await.map_err(err)?;
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
            // Ensure a loopback forward for a declared web page; the web
            // server's proxy dials the returned addr. Reply carries the
            // page's auth spec (host socket only — never to the browser).
            "web.forward" => {
                let machine = args["machine"].as_str().ok_or("missing machine")?;
                let page = args["page"].as_str().ok_or("missing page")?;
                lab.ensure_web_forward(machine, page).await.map_err(err)
            }
            // Container lifecycle (mirrors the vm.* verbs; PRD §18).
            "container.start" => {
                let name = container_arg(&args)?;
                // Same ordering as vm.start: pull (with progress), preflight,
                // start.
                let output = stream_sink(&self.lab, _stream);
                lab.ensure_pulled(std::slice::from_ref(&name), Some(&output))
                    .await
                    .map_err(err)?;
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
                let force = args["force"].as_bool().unwrap_or(false);
                let c = lab.container(&name).map_err(err)?.clone();
                c.stop(force).await.map_err(err)?;
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
            // Interactive shell over the vmlab-agent channel — the same
            // session model as vm.tty_open (multi-session; every attach gets
            // its own shell inside the container namespaces).
            "container.tty_open" => {
                let name = container_arg(&args)?;
                let cols = args["cols"].as_u64().unwrap_or(80) as u16;
                let rows = args["rows"].as_u64().unwrap_or(24) as u16;
                let c = lab.container(&name).map_err(err)?;
                let agent = c.agent().await.map_err(err)?;
                let session = agent.open_terminal(cols, rows, None).await.map_err(err)?;
                let id = session.id;
                let path = c.dirs.term_session_sock(id);
                vm_agent::expose_terminal_socket(session, path.clone())
                    .await
                    .map_err(err)?;
                Ok(json!({"session": id, "path": path}))
            }
            "container.tty_resize" => {
                let session = args["session"].as_u64().ok_or("missing session")? as u32;
                let cols = args["cols"].as_u64().unwrap_or(80) as u16;
                let rows = args["rows"].as_u64().unwrap_or(24) as u16;
                let c = lab.container(&container_arg(&args)?).map_err(err)?;
                let agent = c.agent().await.map_err(err)?;
                agent.resize(session, cols, rows).await.map_err(err)?;
                Ok(json!(true))
            }
            "container.push_file" => {
                let name = container_arg(&args)?;
                let from = args["from"].as_str().ok_or("missing from")?;
                let to = args["to"].as_str().ok_or("missing to")?;
                let mode = args["mode"].as_u64().map(|m| m as u32);
                let c = lab.container(&name).map_err(err)?;
                let agent = c.agent().await.map_err(err)?;
                let (sha256, len) = agent
                    .push_file(std::path::Path::new(from), to, mode)
                    .await
                    .map_err(err)?;
                Ok(json!({"sha256": sha256, "len": len}))
            }
            "container.pull_file" => {
                let name = container_arg(&args)?;
                let from = args["from"].as_str().ok_or("missing from")?;
                let to = args["to"].as_str().ok_or("missing to")?;
                let c = lab.container(&name).map_err(err)?;
                let agent = c.agent().await.map_err(err)?;
                let (sha256, len) = agent
                    .pull_file(from, std::path::Path::new(to))
                    .await
                    .map_err(err)?;
                Ok(json!({"sha256": sha256, "len": len}))
            }
            // Follow a file inside the container (tail -F semantics).
            "container.tail" => {
                let name = container_arg(&args)?;
                let path = args["path"].as_str().ok_or("missing path")?;
                let c = lab.container(&name).map_err(err)?;
                let agent = c.agent().await.map_err(err)?;
                let mut session = agent.open_tail(path.to_string()).await.map_err(err)?;
                loop {
                    tokio::select! {
                        ev = session.recv() => match ev {
                            Some(vm_agent::SessionEvent::Data(b)) => {
                                _stream.chunk(String::from_utf8_lossy(&b).into_owned()).await;
                            }
                            Some(vm_agent::SessionEvent::Error(msg)) => return Err(msg),
                            None => break,
                            Some(_) => {}
                        },
                        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                            if c.state().await != vm::PowerState::Running {
                                break;
                            }
                        }
                    }
                }
                session.close().await;
                Ok(json!(true))
            }
            // Micro-VM metrics (the whole VM is the container).
            "container.stats" => {
                let c = lab.container(&container_arg(&args)?).map_err(err)?;
                let agent = c.agent().await.map_err(err)?;
                let m = agent
                    .stats(std::time::Duration::from_secs(10))
                    .await
                    .map_err(err)?;
                Ok(json!({
                    "cpu_pct": m.cpu_pct,
                    "mem_used": m.mem_used,
                    "mem_total": m.mem_total,
                    "disks": m.disks.iter().map(|d| json!({
                        "mount": d.mount, "used": d.used, "total": d.total,
                    })).collect::<Vec<_>>(),
                }))
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
            // config-weave playbooks (declared with `playbook {}` blocks):
            // list the lab's assignments, and run check/apply on demand
            // against one machine — the playbook folder is re-pushed each
            // run, so this is the edit→check dev loop. Progress streams as
            // chunks here and as `playbook.op.*` events for the web UI.
            "playbook.list" => {
                let machines: Vec<String> = lab
                    .config
                    .lab
                    .vms
                    .iter()
                    .map(|v| v.name.clone())
                    .chain(lab.config.lab.containers.iter().map(|c| c.name.clone()))
                    .collect();
                Ok(Value::Array(
                    lab.config
                        .lab
                        .playbooks
                        .iter()
                        .map(|p| {
                            let resolved: Vec<&String> = if p.vms.is_empty() {
                                machines.iter().collect()
                            } else {
                                p.vms.iter().collect()
                            };
                            let running = resolved
                                .iter()
                                .map(|m| lab.playbook_ops.op_of(m))
                                .find(|v| !v.is_null())
                                .unwrap_or(Value::Null);
                            json!({
                                "path": p.path.display().to_string(),
                                "play": p.play,
                                "span": p.span,
                                "vms": p.vms,
                                "machines": resolved,
                                "running": running,
                            })
                        })
                        .collect(),
                ))
            }
            "playbook.check" | "playbook.apply" => {
                let machine = args["machine"]
                    .as_str()
                    .map(String::from)
                    .ok_or_else(|| "missing machine".to_string())?;
                let pb = playbook::resolve_playbook(
                    &lab.config.lab,
                    &machine,
                    args["playbook"].as_str(),
                    args["play"].as_str(),
                )?
                .clone();
                let mode = if cmd == "playbook.apply" {
                    playbook::PlaybookMode::Apply
                } else {
                    playbook::PlaybookMode::Check
                };
                let output = stream_sink(&self.lab, _stream);
                let outcome = playbook::run_playbook(lab, &machine, &pb, mode, &output)
                    .await
                    .map_err(err)?;
                Ok(json!({
                    "machine": machine,
                    "playbook": pb.path.display().to_string(),
                    "play": pb.play,
                    "mode": mode.verb(),
                    "exit_code": outcome.exit_code,
                    "reboots": outcome.reboots,
                    "report": outcome.report.unwrap_or(Value::Null),
                }))
            }
            "playbook.op_status" => Ok(lab.playbook_ops.status()),
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
                        let names: Vec<String> = lab
                            .vms
                            .keys()
                            .chain(lab.containers.keys())
                            .cloned()
                            .collect();
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
    use super::{handler_matches, region_arg};
    use crate::config::model::Handler;
    use serde_json::json;
    use std::path::PathBuf;

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

    #[test]
    fn event_handler_target_filter_is_optional_and_exact() {
        let mut handler = Handler {
            event: "vm.ready".into(),
            run: PathBuf::from("handler.ws"),
            targets: Vec::new(),
            span: (0, 0),
        };
        assert!(handler_matches(&handler, "vm.ready", "a"));
        handler.targets = vec!["a".into()];
        assert!(handler_matches(&handler, "vm.ready", "a"));
        assert!(!handler_matches(&handler, "vm.ready", "b"));
        assert!(!handler_matches(&handler, "vm.stopped", "a"));
    }
}
