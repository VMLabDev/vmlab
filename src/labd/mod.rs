//! The per-lab daemon (PRD §3): owns the lab's QEMU processes, QMP/agent
//! channels, lab-local segments and network services, snapshots, state, and
//! events. One process per running lab, spawned and reaped by the
//! supervisor; the CLI talks to it directly for lab-scoped operations.

pub mod container;
pub mod container_ctl;
pub mod events;
pub mod hypervisor;
pub mod lab;
#[cfg(test)]
mod lifecycle_tests;
pub mod machine;
pub mod netservices;
pub mod network;
pub mod plan;
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
    // A termination signal runs the same teardown as the `shutdown` command:
    // the machines this daemon owns have no other manager, so exiting without
    // stopping them would orphan QEMU, swtpm, virtiofsd and smbd.
    crate::lifecycle::termination_signal().await;
    tracing::info!("lab daemon for {lab} caught a termination signal; stopping the lab");
    teardown(&runtime, &tasks).await;
    drop(server);
    Ok(())
}

/// Everything a lab daemon must do before its process goes away: stop the
/// machines it owns (PRD §3 — nothing else manages them), reap the SMB server,
/// release global-segment references so the supervisor can free shared
/// switches (§9.2), and cancel + join background tasks so nothing dies
/// mid-flight.
async fn teardown(lab: &Arc<LabRuntime>, tasks: &crate::lifecycle::TaskGroup) {
    let _ = lab.down(&[], false).await;
    if let Some(mut smb) = lab.smb.lock().await.take() {
        smb.stop();
    }
    lab.network.lock().await.detach_globals().await;
    tasks.shutdown(std::time::Duration::from_secs(5)).await;
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
        // Rotating: provision output is appended for the life of the lab.
        let mut log = crate::logs::AppendLog::open(log_path);
        while let Some(line) = rx.recv().await {
            log.write(&line);
            streamer.chunk(line).await;
        }
    });
    Arc::new(move |line: String| {
        let _ = tx.send(line);
    })
}

/// The machine a `machine.*` command addresses. `machine` is the arg name;
/// `vm` and `container` are still accepted so a client that has not been
/// updated keeps working.
fn machine_arg(args: &Value) -> Result<String, String> {
    for key in ["machine", "vm", "container"] {
        if let Some(name) = args[key].as_str() {
            return Ok(name.to_string());
        }
    }
    Err("missing machine".to_string())
}

/// The framebuffer of the addressed machine, or an error naming the reason a
/// caller cannot have one. Containers run with no display device at all, so
/// this is a hard "no" rather than a transient failure.
fn display_of(
    lab: &Arc<LabRuntime>,
    args: &Value,
) -> Result<crate::labd::machine::Display, String> {
    let name = machine_arg(args)?;
    let m = lab.machine(&name).map_err(|e| format!("{e:#}"))?;
    m.display()
        .ok_or_else(|| format!("{name}: this machine has no display"))
}

/// Stream an agent session's data to the client until it ends, the machine
/// stops, or the session errors. Shared by `machine.tail` and
/// `machine.eventlog`.
async fn pump_session(
    mut session: vm_agent::AgentSession,
    m: &dyn crate::labd::machine::Machine,
    stream: &Streamer,
) -> Result<Value, String> {
    loop {
        tokio::select! {
            ev = session.recv() => match ev {
                Some(vm_agent::SessionEvent::Data(b)) => {
                    stream.chunk(String::from_utf8_lossy(&b).into_owned()).await;
                }
                Some(vm_agent::SessionEvent::Error(msg)) => return Err(msg),
                None => break,
                Some(_) => {}
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if m.state().await != vm::PowerState::Running {
                    break;
                }
            }
        }
    }
    session.close().await;
    Ok(json!(true))
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
            "dns.table" => Ok(lab.dns_table().await),
            "up" => {
                let output = stream_sink(&self.lab, _stream);
                lab.up(&vms_arg(&args), output).await.map_err(err)?;
                Ok(json!(true))
            }
            // Download any pending templates/images without starting anything
            // (`vmlab pull`). The exact code path `up` runs first — same
            // progress events.
            "pull" => {
                let output = stream_sink(&self.lab, _stream);
                lab.ensure_pulled(&vms_arg(&args), Some(&output))
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            // Abort one machine's running download (Templates page); whatever
            // is waiting on it fails with "download cancelled".
            "pull.cancel" => {
                let machine = args["machine"].as_str().ok_or("missing machine")?;
                Ok(json!(lab.cancel_pull(machine)))
            }
            // Ad-hoc script against the lab (PRD §12: vmlab script).
            "run" => {
                let script = args["script"].as_str().ok_or("missing script")?;
                let path = lab.root.join(script);
                let output = stream_sink(&self.lab, _stream);
                crate::scripting::run_script_file(lab.clone(), &path, None, output)
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
            // ---- machines (PRD §7, §18) ---------------------------------
            //
            // One command set for VMs and containers alike. Where a machine
            // genuinely cannot serve a command it says so through its
            // capabilities (no display, no console log) rather than through
            // its kind — see `labd::machine`.
            "machine.start" => {
                let name = machine_arg(&args)?;
                // Pull with CLI-visible progress before the preflight (the
                // pulled meta can change the resolved firmware/TPM needs);
                // the internal pull in start is then a no-op.
                let output = stream_sink(&self.lab, _stream);
                lab.ensure_pulled(std::slice::from_ref(&name), Some(&output))
                    .await
                    .map_err(err)?;
                lab.preflight_binaries(std::slice::from_ref(&name))
                    .map_err(err)?;
                lab.start_machine(&name).await.map_err(err)?;
                Ok(json!(true))
            }
            "machine.stop" => {
                let force = args["force"].as_bool().unwrap_or(false);
                lab.machine(&machine_arg(&args)?)
                    .map_err(err)?
                    .stop(force)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "machine.restart" => {
                let force = args["force"].as_bool().unwrap_or(false);
                lab.restart_machine(&machine_arg(&args)?, force)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "machine.destroy" => {
                lab.destroy_machine(&machine_arg(&args)?)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            // What this machine can do beyond the universal commands, probed
            // live: a display, a console log, in-place reboot, and whichever
            // features its agent negotiated at handshake.
            "machine.capabilities" => {
                let m = lab.machine(&machine_arg(&args)?).map_err(err)?;
                Ok(json!(m.capabilities().await))
            }
            "machine.ip" => {
                let nic = args["nic"].as_u64().map(|n| n as usize);
                let ip = lab
                    .machine(&machine_arg(&args)?)
                    .map_err(err)?
                    .guest_ip(nic)
                    .await
                    .map_err(err)?;
                Ok(json!(ip))
            }

            // ---- display (machines with a framebuffer) --------------------
            "machine.screenshot" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                display_of(lab, &args)?
                    .screenshot(std::path::Path::new(path))
                    .await
                    .map_err(err)?;
                Ok(json!({"path": path}))
            }
            "machine.sendkeys" => {
                let keys = args["keys"].as_str().ok_or("missing keys")?;
                display_of(lab, &args)?.send_keys(keys).await.map_err(err)?;
                Ok(json!(true))
            }
            "machine.mouse_move" => {
                let x = args["x"].as_i64().ok_or("missing x")?;
                let y = args["y"].as_i64().ok_or("missing y")?;
                display_of(lab, &args)?
                    .mouse_move(x, y)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "machine.mouse_click" => {
                let button = args["button"].as_str().unwrap_or("left");
                let at = match (args["x"].as_i64(), args["y"].as_i64()) {
                    (Some(x), Some(y)) => Some((x, y)),
                    _ => None,
                };
                display_of(lab, &args)?
                    .mouse_click(button, at)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "machine.mouse_drag" => {
                let x1 = args["x1"].as_i64().ok_or("missing x1")?;
                let y1 = args["y1"].as_i64().ok_or("missing y1")?;
                let x2 = args["x2"].as_i64().ok_or("missing x2")?;
                let y2 = args["y2"].as_i64().ok_or("missing y2")?;
                display_of(lab, &args)?
                    .mouse_drag(x1, y1, x2, y2)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "machine.ocr" => {
                let region = region_arg(&args)?;
                let text = display_of(lab, &args)?.ocr(region).await.map_err(err)?;
                Ok(json!(text))
            }
            "machine.find_image" => {
                let image = args["image"].as_str().ok_or("missing image")?;
                let threshold = args["threshold"].as_f64().unwrap_or(0.9);
                let region = region_arg(&args)?;
                let opts = crate::vision::MatchOptions { threshold, region };
                let found = display_of(lab, &args)?
                    .find_image(&[PathBuf::from(image)], &opts)
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

            // ---- agent-backed commands ------------------------------------
            //
            // All of these ride the one `vmlab.agent.0` channel, so they work
            // on any machine whose agent advertises the feature.
            "machine.exec" => {
                let name = machine_arg(&args)?;
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
                let agent = lab
                    .machine(&name)
                    .map_err(err)?
                    .agent()
                    .await
                    .map_err(err)?;
                let mut argv = vec![cmd.to_string()];
                argv.extend(cmd_args);
                let result = agent
                    .exec(argv, vec![], None, None, timeout)
                    .await
                    .map_err(err)?;
                Ok(json!({
                    "exit_code": result.exit_code,
                    "stdout": String::from_utf8_lossy(&result.stdout),
                    "stderr": String::from_utf8_lossy(&result.stderr),
                }))
            }
            "machine.osinfo" => {
                let timeout =
                    std::time::Duration::from_secs(args["timeout"].as_u64().unwrap_or(30));
                let agent = lab
                    .machine(&machine_arg(&args)?)
                    .map_err(err)?
                    .agent()
                    .await
                    .map_err(err)?;
                let info = agent.osinfo(timeout).await.map_err(err)?;
                Ok(json!({
                    "id": info.id,
                    "name": info.name,
                    "version": info.version,
                    "kernel": info.kernel,
                    "arch": info.arch,
                    "hostname": info.hostname,
                }))
            }
            "machine.agent_info" => {
                let agent = lab
                    .machine(&machine_arg(&args)?)
                    .map_err(err)?
                    .agent()
                    .await
                    .map_err(err)?;
                let info = agent.info();
                Ok(json!({
                    "version": info.agent_version,
                    "os": info.os,
                    "features": info.features,
                }))
            }
            // Interactive terminal: opens a fresh session (multi-session —
            // every attach gets its own shell), re-exposed as a raw-byte unix
            // socket clients connect to directly; resize rides the agent's
            // control channel.
            "machine.tty_open" => {
                let name = machine_arg(&args)?;
                let cols = args["cols"].as_u64().unwrap_or(80) as u16;
                let rows = args["rows"].as_u64().unwrap_or(24) as u16;
                let m = lab.machine(&name).map_err(err)?;
                let agent = m.agent().await.map_err(err)?;
                let session = agent.open_terminal(cols, rows, None).await.map_err(err)?;
                let id = session.id;
                let path = m.term_session_sock(id);
                vm_agent::expose_terminal_socket(session, path.clone())
                    .await
                    .map_err(err)?;
                Ok(json!({"session": id, "path": path}))
            }
            "machine.tty_resize" => {
                let session = args["session"].as_u64().ok_or("missing session")? as u32;
                let cols = args["cols"].as_u64().unwrap_or(80) as u16;
                let rows = args["rows"].as_u64().unwrap_or(24) as u16;
                let agent = lab
                    .machine(&machine_arg(&args)?)
                    .map_err(err)?
                    .agent()
                    .await
                    .map_err(err)?;
                agent.resize(session, cols, rows).await.map_err(err)?;
                Ok(json!(true))
            }
            // Fast binary file transfer over the agent channel. `from` is a
            // host path the daemon can see (the CLI resolves it absolute
            // first); `data` is inline base64 for callers that have bytes
            // rather than a file.
            "machine.push_file" => {
                let name = machine_arg(&args)?;
                let to = args["to"].as_str().ok_or("missing to")?;
                let mode = args["mode"].as_u64().map(|m| m as u32);
                let agent = lab
                    .machine(&name)
                    .map_err(err)?
                    .agent()
                    .await
                    .map_err(err)?;
                let (sha256, len) = match args["data"].as_str() {
                    Some(data) => {
                        use base64::Engine as _;
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .map_err(|e| format!("invalid base64 data: {e}"))?;
                        let tmp = std::env::temp_dir()
                            .join(format!("vmlab-cp-{}-{name}", std::process::id()));
                        std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
                        let res = agent.push_file(&tmp, to, mode).await;
                        let _ = std::fs::remove_file(&tmp);
                        res.map_err(err)?
                    }
                    None => {
                        let from = args["from"].as_str().ok_or("missing from or data")?;
                        agent
                            .push_file(std::path::Path::new(from), to, mode)
                            .await
                            .map_err(err)?
                    }
                };
                Ok(json!({"sha256": sha256, "len": len}))
            }
            "machine.pull_file" => {
                let from = args["from"].as_str().ok_or("missing from")?;
                let to = args["to"].as_str().ok_or("missing to")?;
                let agent = lab
                    .machine(&machine_arg(&args)?)
                    .map_err(err)?
                    .agent()
                    .await
                    .map_err(err)?;
                let (sha256, len) = agent
                    .pull_file(from, std::path::Path::new(to))
                    .await
                    .map_err(err)?;
                Ok(json!({"sha256": sha256, "len": len}))
            }
            // Follow a guest file (tail -F semantics), streamed as chunks
            // until the client hangs up or the machine stops.
            "machine.tail" => {
                let name = machine_arg(&args)?;
                let path = args["path"].as_str().ok_or("missing path")?;
                let m = lab.machine(&name).map_err(err)?;
                let agent = m.agent().await.map_err(err)?;
                let session = agent.open_tail(path.to_string()).await.map_err(err)?;
                pump_session(session, m.as_ref(), _stream).await
            }
            // Windows event log follow (agent `eventlog` feature).
            "machine.eventlog" => {
                let name = machine_arg(&args)?;
                let filter = args["filter"].as_str().map(String::from);
                let m = lab.machine(&name).map_err(err)?;
                let agent = m.agent().await.map_err(err)?;
                if !agent.has_feature(vmlab_agent_proto::features::EVENTLOG) {
                    return Err(format!(
                        "{name}: the guest agent has no event log (Windows-only feature)"
                    ));
                }
                let session = agent.open_eventlog(filter).await.map_err(err)?;
                pump_session(session, m.as_ref(), _stream).await
            }
            // Latest guest metrics (subscribes the 2s sampler on first use).
            "machine.stats" => {
                let agent = lab
                    .machine(&machine_arg(&args)?)
                    .map_err(err)?
                    .agent()
                    .await
                    .map_err(err)?;
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
            "machine.clipboard_get" => {
                let agent = lab
                    .machine(&machine_arg(&args)?)
                    .map_err(err)?
                    .agent()
                    .await
                    .map_err(err)?;
                let text = agent
                    .get_clipboard(std::time::Duration::from_secs(10))
                    .await
                    .map_err(err)?;
                Ok(json!(text))
            }
            "machine.clipboard_set" => {
                let text = args["text"].as_str().ok_or("missing text")?;
                let agent = lab
                    .machine(&machine_arg(&args)?)
                    .map_err(err)?
                    .agent()
                    .await
                    .map_err(err)?;
                agent.set_clipboard(text.to_string()).await.map_err(err)?;
                Ok(json!(true))
            }

            // ---- console log (machines that keep one) ---------------------
            "machine.logs" => {
                let name = machine_arg(&args)?;
                let lines = args["lines"].as_u64().unwrap_or(100) as usize;
                let m = lab.machine(&name).map_err(err)?;
                let Some(tail) = m.console_log(lines) else {
                    return Err(format!("{name}: this machine keeps no console log"));
                };
                let tail = tail.map_err(err)?;
                if !args["follow"].as_bool().unwrap_or(false) {
                    return Ok(json!(tail));
                }
                // Follow: stream the tail, then poll for growth until the
                // client hangs up or the machine stops.
                _stream.chunk(tail).await;
                let mut seen = 0usize;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Some(Ok(all)) = m.console_log(usize::MAX) {
                        let len = all.len();
                        if len > seen {
                            _stream.chunk(all[seen..].to_string()).await;
                            seen = len;
                        }
                    }
                    if m.state().await == vm::PowerState::Stopped {
                        break;
                    }
                }
                Ok(json!(true))
            }

            // Ensure a loopback forward for a declared web page; the web
            // server's proxy dials the returned addr. Reply carries the
            // page's auth spec (host socket only — never to the browser).
            "web.forward" => {
                let machine = args["machine"].as_str().ok_or("missing machine")?;
                let page = args["page"].as_str().ok_or("missing page")?;
                lab.ensure_web_forward(machine, page).await.map_err(err)
            }
            // config-weave playbooks (declared with `playbook {}` blocks):
            // list the lab's assignments, and run check/apply on demand
            // against one machine — the playbook folder is re-pushed each
            // run, so this is the edit→check dev loop. Progress streams as
            // chunks here and as `playbook.op.*` events for the web UI.
            "playbook.list" => {
                // One row per (machine, playbook block) — the blocks live
                // inside the machine they configure.
                let cfg = &lab.config.lab;
                let machines = cfg
                    .vms
                    .iter()
                    .map(|v| (&v.name, &v.playbooks))
                    .chain(cfg.containers.iter().map(|c| (&c.name, &c.playbooks)));
                Ok(Value::Array(
                    machines
                        .flat_map(|(machine, playbooks)| {
                            playbooks.iter().map(move |p| {
                                json!({
                                    "machine": machine,
                                    "path": p.path.display().to_string(),
                                    "play": p.play,
                                    "span": p.span,
                                    "vars": p.vars.iter()
                                        .map(|v| json!({"name": v.name, "value": v.value}))
                                        .collect::<Vec<_>>(),
                                    "running": lab.playbook_ops.op_of(machine),
                                })
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
                lab.delete_snapshot(&machine_arg(&args)?, snap)
                    .await
                    .map_err(err)?;
                Ok(json!(true))
            }
            "snapshot.list" => lab.snapshots(&machine_arg(&args)?).await.map_err(err),
            "shutdown" => {
                tracing::info!("lab daemon shutdown requested");
                let lab = lab.clone();
                let tasks = self.tasks.clone();
                // Spawned so this command's response reaches the caller before
                // the process goes away.
                tokio::spawn(async move {
                    teardown(&lab, &tasks).await;
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
    use super::{handler_matches, machine_arg, region_arg};
    use crate::config::model::Handler;
    use serde_json::json;
    use std::path::PathBuf;

    /// The wire says `machine`, but a client that predates the collapse says
    /// `vm` or `container`. Both keep working rather than failing with a
    /// confusing "missing machine".
    #[test]
    fn machine_arg_accepts_the_old_names() {
        assert_eq!(machine_arg(&json!({"machine": "dc01"})).unwrap(), "dc01");
        assert_eq!(machine_arg(&json!({"vm": "dc01"})).unwrap(), "dc01");
        assert_eq!(machine_arg(&json!({"container": "web"})).unwrap(), "web");
        // `machine` wins when a client sends both.
        assert_eq!(
            machine_arg(&json!({"machine": "a", "vm": "b"})).unwrap(),
            "a"
        );
        assert!(machine_arg(&json!({})).is_err());
        // A non-string is as good as absent.
        assert!(machine_arg(&json!({"machine": 7})).is_err());
    }

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
