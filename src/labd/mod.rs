//! The per-lab daemon (PRD §3): owns the lab's QEMU processes, QMP/agent
//! channels, lab-local segments and network services, snapshots, state, and
//! events. One process per running lab, spawned and reaped by the
//! supervisor; the CLI talks to it directly for lab-scoped operations.

pub mod container;
pub mod container_ctl;
pub mod display;
pub mod events;
pub mod forward_plan;
pub mod guest_os;
pub mod hypervisor;
pub mod lab;
#[cfg(test)]
mod lifecycle_tests;
pub mod machine;
pub mod netservices;
pub mod network;
pub mod plan;
pub mod playbook;
pub mod pull_ledger;
pub mod share_plan;
pub mod state;
pub mod vm;
pub mod vm_agent;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::proto::server::{Handler, Server, Streamer};
use crate::proto::{CommandError, LabRequest, Region};
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
    let handler: Arc<dyn Handler<LabRequest>> = Arc::new(LabdHandler {
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

/// The addressed machine.
///
/// A name that is not in the lab is a not-found, and says so by code — the
/// surfaces above no longer have to recognise the wording.
fn machine_of(
    lab: &Arc<LabRuntime>,
    name: &str,
) -> Result<Arc<dyn crate::labd::machine::Machine>, CommandError> {
    lab.machine(name)
        .map_err(|e| CommandError::not_found(format!("{e:#}")))
}

/// The framebuffer of the addressed machine, or an error naming the reason a
/// caller cannot have one. Containers run with no display device at all, so
/// this is a hard "no" — unsupported, not a transient failure.
fn display_of(
    lab: &Arc<LabRuntime>,
    name: &str,
) -> Result<crate::labd::machine::Display, CommandError> {
    machine_of(lab, name)?
        .display()
        .ok_or_else(|| CommandError::unsupported(format!("machine `{name}` has no display")))
}

/// The agent channel of the addressed machine.
async fn agent_of(
    lab: &Arc<LabRuntime>,
    name: &str,
) -> Result<vm_agent::AgentHandle, CommandError> {
    Ok(machine_of(lab, name)?.agent().await?)
}

/// Stream an agent session's data to the client until it ends, the machine
/// stops, or the session errors. Shared by `machine.tail` and
/// `machine.eventlog`.
async fn pump_session(
    mut session: vm_agent::AgentSession,
    m: &dyn crate::labd::machine::Machine,
    stream: &Streamer,
) -> Result<Value, CommandError> {
    loop {
        tokio::select! {
            ev = session.recv() => match ev {
                Some(vm_agent::SessionEvent::Data(b)) => {
                    stream.chunk(String::from_utf8_lossy(&b).into_owned()).await;
                }
                Some(vm_agent::SessionEvent::Error(msg)) => return Err(CommandError::failed(msg)),
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

/// Run one playbook against one machine; `check` and `apply` differ only in
/// the mode they pass down.
async fn run_playbook(
    lab: &Arc<LabRuntime>,
    stream: &Streamer,
    machine: String,
    playbook: Option<String>,
    play: Option<String>,
    mode: playbook::PlaybookMode,
) -> Result<Value, CommandError> {
    let pb = playbook::resolve_playbook(
        &lab.config.lab,
        &machine,
        playbook.as_deref(),
        play.as_deref(),
    )
    .map_err(CommandError::not_found)?
    .clone();
    let output = stream_sink(lab, stream);
    let outcome = playbook::run_playbook(lab, &machine, &pb, mode, &output).await?;
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

#[async_trait::async_trait]
impl Handler<LabRequest> for LabdHandler {
    async fn handle(&self, req: LabRequest, stream: &Streamer) -> Result<Value, CommandError> {
        let lab = &self.lab;
        match req {
            LabRequest::Ping {} => Ok(json!("pong")),
            // The wire form of the status projection (ADR-0004); every surface
            // parses it straight back into `LabStatus`.
            LabRequest::Status {} => Ok(json!(lab.status().await)),
            LabRequest::DnsTable {} => Ok(lab.dns_table().await),
            LabRequest::Up { machines } => {
                let output = stream_sink(&self.lab, stream);
                lab.up(&machines, output).await?;
                Ok(json!(true))
            }
            // Download any pending templates/images without starting anything
            // (`vmlab pull`). The exact code path `up` runs first — same
            // progress events.
            LabRequest::Pull { machines } => {
                let output = stream_sink(&self.lab, stream);
                lab.ensure_pulled(&machines, Some(&output)).await?;
                Ok(json!(true))
            }
            // Abort one machine's running download (Templates page); whatever
            // is waiting on it fails with "download cancelled".
            LabRequest::PullCancel { machine } => Ok(json!(lab.cancel_pull(&machine))),
            // Ad-hoc script against the lab (PRD §12: vmlab script).
            LabRequest::Run { script } => {
                let path = lab.root.join(script);
                let output = stream_sink(&self.lab, stream);
                crate::scripting::run_script_file(lab.clone(), &path, None, output).await?;
                Ok(json!(true))
            }
            LabRequest::Down { machines, force } => {
                lab.down(&machines, force).await?;
                Ok(json!(true))
            }
            LabRequest::Destroy {} => {
                lab.destroy().await?;
                Ok(json!(true))
            }
            // ---- machines (PRD §7, §18) ---------------------------------
            //
            // One command set for VMs and containers alike. Where a machine
            // genuinely cannot serve a command it says so through its
            // capabilities (no display, no console log) rather than through
            // its kind — see `labd::machine`.
            LabRequest::MachineStart { machine } => {
                // Pull with CLI-visible progress before the preflight (the
                // pulled meta can change the resolved firmware/TPM needs);
                // the internal pull in start is then a no-op.
                let output = stream_sink(&self.lab, stream);
                lab.ensure_pulled(std::slice::from_ref(&machine), Some(&output))
                    .await?;
                lab.preflight_binaries(std::slice::from_ref(&machine))?;
                lab.start_machine(&machine).await?;
                Ok(json!(true))
            }
            LabRequest::MachineStop { machine, force } => {
                machine_of(lab, &machine)?.stop(force).await?;
                Ok(json!(true))
            }
            LabRequest::MachineRestart { machine, force } => {
                lab.restart_machine(&machine, force).await?;
                Ok(json!(true))
            }
            LabRequest::MachineDestroy { machine } => {
                lab.destroy_machine(&machine).await?;
                Ok(json!(true))
            }
            // What this machine can do beyond the universal commands, probed
            // live: a display, a console log, in-place reboot, and whichever
            // features its agent negotiated at handshake.
            LabRequest::MachineCapabilities { machine } => {
                Ok(json!(machine_of(lab, &machine)?.capabilities().await))
            }
            LabRequest::MachineIp { machine, nic } => {
                let ip = machine_of(lab, &machine)?.guest_ip(nic).await?;
                Ok(json!(ip))
            }

            // ---- display (machines with a framebuffer) --------------------
            LabRequest::MachineScreenshot { machine, path } => {
                display_of(lab, &machine)?
                    .screenshot(std::path::Path::new(&path))
                    .await?;
                Ok(json!({"path": path}))
            }
            LabRequest::MachineSendKeys { machine, keys } => {
                display_of(lab, &machine)?.send_keys(&keys).await?;
                Ok(json!(true))
            }
            LabRequest::MachineMouseMove { machine, x, y } => {
                display_of(lab, &machine)?.mouse_move(x, y).await?;
                Ok(json!(true))
            }
            LabRequest::MachineMouseClick {
                machine,
                button,
                x,
                y,
            } => {
                let at = match (x, y) {
                    (Some(x), Some(y)) => Some((x, y)),
                    _ => None,
                };
                display_of(lab, &machine)?.mouse_click(&button, at).await?;
                Ok(json!(true))
            }
            LabRequest::MachineMouseDrag {
                machine,
                x1,
                y1,
                x2,
                y2,
            } => {
                display_of(lab, &machine)?
                    .mouse_drag(x1, y1, x2, y2)
                    .await?;
                Ok(json!(true))
            }
            LabRequest::MachineOcr { machine, region } => {
                let text = display_of(lab, &machine)?
                    .ocr(region.map(Region::as_tuple))
                    .await?;
                Ok(json!(text))
            }
            LabRequest::MachineFindImage {
                machine,
                image,
                threshold,
                region,
            } => {
                let opts = crate::vision::MatchOptions {
                    threshold,
                    region: region.map(Region::as_tuple),
                };
                let found = display_of(lab, &machine)?
                    .find_image(&[PathBuf::from(image)], &opts)
                    .await?;
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
            LabRequest::MachineExec {
                machine,
                cmd,
                args,
                timeout,
            } => {
                let agent = agent_of(lab, &machine).await?;
                let mut argv = vec![cmd];
                argv.extend(args);
                let result = agent
                    .exec(
                        argv,
                        vec![],
                        None,
                        None,
                        std::time::Duration::from_secs(timeout),
                    )
                    .await?;
                Ok(json!({
                    "exit_code": result.exit_code,
                    "stdout": String::from_utf8_lossy(&result.stdout),
                    "stderr": String::from_utf8_lossy(&result.stderr),
                }))
            }
            LabRequest::MachineOsInfo { machine, timeout } => {
                let agent = agent_of(lab, &machine).await?;
                let info = agent
                    .osinfo(std::time::Duration::from_secs(timeout))
                    .await?;
                Ok(json!({
                    "id": info.id,
                    "name": info.name,
                    "version": info.version,
                    "kernel": info.kernel,
                    "arch": info.arch,
                    "hostname": info.hostname,
                }))
            }
            // Interactive terminal: opens a fresh session (multi-session —
            // every attach gets its own shell), re-exposed as a raw-byte unix
            // socket clients connect to directly; resize rides the agent's
            // control channel.
            LabRequest::MachineTtyOpen {
                machine,
                cols,
                rows,
            } => {
                let m = machine_of(lab, &machine)?;
                let agent = m.agent().await?;
                let session = agent.open_terminal(cols, rows, None).await?;
                let id = session.id;
                let path = m.term_session_sock(id);
                vm_agent::expose_terminal_socket(session, path.clone()).await?;
                Ok(json!({"session": id, "path": path}))
            }
            LabRequest::MachineTtyResize {
                machine,
                session,
                cols,
                rows,
            } => {
                agent_of(lab, &machine)
                    .await?
                    .resize(session, cols, rows)
                    .await?;
                Ok(json!(true))
            }
            // Fast binary file transfer over the agent channel. `from` is a
            // host path the daemon can see (the CLI resolves it absolute
            // first); `data` is inline base64 for callers that have bytes
            // rather than a file.
            LabRequest::MachinePushFile {
                machine,
                to,
                from,
                data,
                mode,
            } => {
                let agent = agent_of(lab, &machine).await?;
                let (sha256, len) = match data {
                    Some(data) => {
                        use base64::Engine as _;
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .map_err(|e| {
                                CommandError::invalid(format!("invalid base64 data: {e}"))
                            })?;
                        let tmp = std::env::temp_dir()
                            .join(format!("vmlab-cp-{}-{machine}", std::process::id()));
                        std::fs::write(&tmp, &bytes)
                            .map_err(|e| CommandError::failed(e.to_string()))?;
                        let res = agent.push_file(&tmp, &to, mode).await;
                        let _ = std::fs::remove_file(&tmp);
                        res?
                    }
                    None => {
                        let from =
                            from.ok_or_else(|| CommandError::invalid("missing from or data"))?;
                        agent
                            .push_file(std::path::Path::new(&from), &to, mode)
                            .await?
                    }
                };
                Ok(json!({"sha256": sha256, "len": len}))
            }
            LabRequest::MachinePullFile { machine, from, to } => {
                let agent = agent_of(lab, &machine).await?;
                let (sha256, len) = agent.pull_file(&from, std::path::Path::new(&to)).await?;
                Ok(json!({"sha256": sha256, "len": len}))
            }
            // Follow a guest file (tail -F semantics), streamed as chunks
            // until the client hangs up or the machine stops.
            LabRequest::MachineTail { machine, path } => {
                let m = machine_of(lab, &machine)?;
                let session = m.agent().await?.open_tail(path).await?;
                pump_session(session, m.as_ref(), stream).await
            }
            // Windows event log follow (agent `eventlog` feature).
            LabRequest::MachineEventLog { machine, filter } => {
                let m = machine_of(lab, &machine)?;
                let agent = m.agent().await?;
                if !agent.has_feature(vmlab_agent_proto::features::EVENTLOG) {
                    return Err(CommandError::unsupported(format!(
                        "{machine}: the guest agent has no event log (Windows-only feature)"
                    )));
                }
                let session = agent.open_eventlog(filter).await?;
                pump_session(session, m.as_ref(), stream).await
            }
            // Latest guest metrics (subscribes the 2s sampler on first use).
            LabRequest::MachineStats { machine } => {
                let m = agent_of(lab, &machine)
                    .await?
                    .stats(std::time::Duration::from_secs(10))
                    .await?;
                Ok(json!({
                    "cpu_pct": m.cpu_pct,
                    "mem_used": m.mem_used,
                    "mem_total": m.mem_total,
                    "disks": m.disks.iter().map(|d| json!({
                        "mount": d.mount, "used": d.used, "total": d.total,
                    })).collect::<Vec<_>>(),
                }))
            }
            LabRequest::MachineClipboardGet { machine } => {
                let text = agent_of(lab, &machine)
                    .await?
                    .get_clipboard(std::time::Duration::from_secs(10))
                    .await?;
                Ok(json!(text))
            }
            LabRequest::MachineClipboardSet { machine, text } => {
                agent_of(lab, &machine).await?.set_clipboard(text).await?;
                Ok(json!(true))
            }

            // ---- console log (machines that keep one) ---------------------
            LabRequest::MachineLogs {
                machine,
                lines,
                follow,
            } => {
                let m = machine_of(lab, &machine)?;
                let Some(tail) = m.console_log(lines) else {
                    return Err(CommandError::unsupported(format!(
                        "machine `{machine}` has no console log"
                    )));
                };
                let tail = tail?;
                if !follow {
                    return Ok(json!(tail));
                }
                // Follow: stream the tail, then poll for growth until the
                // client hangs up or the machine stops.
                stream.chunk(tail).await;
                let mut seen = 0usize;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Some(Ok(all)) = m.console_log(usize::MAX) {
                        let len = all.len();
                        if len > seen {
                            stream.chunk(all[seen..].to_string()).await;
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
            LabRequest::WebForward { machine, page } => {
                Ok(lab.ensure_web_forward(&machine, &page).await?)
            }
            // config-weave playbooks (declared with `playbook {}` blocks):
            // list the lab's assignments, and run check/apply on demand
            // against one machine — the playbook folder is re-pushed each
            // run, so this is the edit→check dev loop. Progress streams as
            // chunks here and as `playbook.op.*` events for the web UI.
            LabRequest::PlaybookList {} => {
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
            LabRequest::PlaybookCheck {
                machine,
                playbook,
                play,
            } => {
                run_playbook(
                    lab,
                    stream,
                    machine,
                    playbook,
                    play,
                    playbook::PlaybookMode::Check,
                )
                .await
            }
            LabRequest::PlaybookApply {
                machine,
                playbook,
                play,
            } => {
                run_playbook(
                    lab,
                    stream,
                    machine,
                    playbook,
                    play,
                    playbook::PlaybookMode::Apply,
                )
                .await
            }
            LabRequest::PlaybookOpStatus {} => Ok(lab.playbook_ops.status()),
            LabRequest::SnapshotTake { name, machine } => match machine {
                Some(machine) => {
                    let online = lab.snapshot(&machine, &name).await?;
                    Ok(json!({"online": online}))
                }
                None => Ok(lab.snapshot_all(&name).await?),
            },
            LabRequest::SnapshotRestore { name, machine } => {
                match machine {
                    Some(machine) => lab.restore(&machine, &name).await?,
                    None => {
                        let names: Vec<String> = lab
                            .vms
                            .keys()
                            .chain(lab.containers.keys())
                            .cloned()
                            .collect();
                        for vm in names {
                            lab.restore(&vm, &name).await?;
                        }
                    }
                }
                Ok(json!(true))
            }
            LabRequest::SnapshotDelete { machine, name } => {
                lab.delete_snapshot(&machine, &name).await?;
                Ok(json!(true))
            }
            LabRequest::SnapshotList { machine } => Ok(lab.snapshots(&machine).await?),
            LabRequest::Shutdown {} => {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::handler_matches;
    use crate::config::model::Handler;
    use std::path::PathBuf;

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
