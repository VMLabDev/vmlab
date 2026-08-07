//! The per-lab daemon (PRD §3): owns the lab's QEMU processes, QMP/agent
//! channels, lab-local segments and network services, snapshots, state, and
//! events. One process per running lab, spawned and reaped by the
//! supervisor; the CLI talks to it directly for lab-scoped operations.

pub mod agent_repair;
pub mod container;
pub mod container_ctl;
pub mod display;
pub mod events;
pub mod forward_plan;
pub mod guest_os;
pub mod hypervisor;
pub mod identity;
pub mod lab;
#[cfg(test)]
mod lifecycle_tests;
pub mod machine;
pub mod netservices;
pub mod network;
pub mod one_shot;
pub mod plan;
pub mod playbook;
pub mod pull_ledger;
pub mod share_plan;
pub mod ssh;
pub mod state;
pub mod vm;
pub mod vm_agent;
pub mod workspace;

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

fn matching_handler_runs(
    runtime: &LabRuntime,
    event: &crate::proto::Event,
) -> Vec<(PathBuf, crate::scripting::EventData)> {
    // Container events carry the name under "container"; handlers read it
    // from `event.vm` either way.
    let machine = event.data["vm"]
        .as_str()
        .or_else(|| event.data["container"].as_str())
        .unwrap_or_default();
    runtime
        .config
        .lab
        .handlers
        .iter()
        .filter(|handler| handler_matches(handler, &event.event, machine))
        .map(|handler| {
            (
                runtime.root.join(&handler.run),
                crate::scripting::EventData {
                    name: event.event.clone(),
                    vm: machine.to_string(),
                    data: event.data.to_string(),
                },
            )
        })
        .collect()
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
        if !runtime.config.lab.handlers.is_empty() {
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
                    for (script, event) in matching_handler_runs(&runtime, &ev) {
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

/// Who a person-invoked command on this machine runs as (PRD §19.2).
///
/// Resolved from the machine itself — its declared logins and its guest
/// family — so a caller holding a machine has everything the answer needs,
/// and a refusal names the account and the machine rather than falling back
/// to the agent identity.
fn logon_for(
    m: &dyn crate::labd::machine::Machine,
    user: Option<&str>,
    password: Option<&str>,
) -> Result<Option<vm_agent::Logon>, CommandError> {
    identity::resolve(m.name(), m.logins(), m.guest_os(), user, password)
        .map_err(|e| CommandError::failed(format!("{e:#}")))
}

/// Name the machine on a failure the guest could only name the account for.
///
/// The other half of §19.2's "fails naming the account **and** the machine":
/// the account that could not be logged on is the agent's to report — only
/// it knows the account does not exist or the secret is wrong — and the
/// machine is the host's, because the agent has never been told which one it
/// serves.
fn on_machine<T>(machine: &str, r: Result<T, CommandError>) -> Result<T, CommandError> {
    r.map_err(|e| e.prefixed(machine))
}

/// The agent channel an attach is served over, or §19.4's hard failure.
///
/// **The top rung of the ladder.** A machine with no agent answering cannot
/// serve an attach *at all* — not a shell, not a file, not a forward — so the
/// connection is refused whole rather than degraded per channel, and it is
/// refused in the same words a stale agent's `sftp` is: what is missing, and
/// both remedies. A template built with `agent = false` arrives here, which is
/// how it "fails attach through the same path" as one whose agent is merely
/// old.
///
/// A machine that is simply **not running** gets its own reason back
/// unchanged: nothing about rebuilding a template helps a machine that is off,
/// and telling someone to do it would be the ladder's words in the one place
/// they are wrong.
async fn attach_agent_of(
    m: &Arc<dyn crate::labd::machine::Machine>,
    name: &str,
) -> Result<vm_agent::AgentHandle, CommandError> {
    let origin = m.agent_origin();
    m.agent()
        .await
        .map_err(|e| CommandError::failed(attach_failure(name, origin, &e)))
}

/// What an attach that could not reach an agent says, given why — and given
/// where this machine's agent came from.
///
/// Split out from [`attach_agent_of`] because *which* reason earns §19.4's
/// remedies is the decision worth pinning, and both exceptions are about
/// remedies that would not be true:
///
/// - a machine that is simply **not running** gets its own reason back
///   unchanged, because nothing about rebuilding a template helps a machine
///   that is off;
/// - a machine whose agent **ships with the host** has neither remedy — there
///   is no artefact to rebuild and nothing to push — so it is told what its
///   silence actually means rather than being sent after a rebuild it does not
///   have to perform.
fn attach_failure(
    machine: &str,
    origin: crate::labd::machine::AgentOrigin,
    e: &anyhow::Error,
) -> String {
    use crate::labd::machine::{AgentOrigin, AgentUnavailable};
    let reason = format!("{e:#}");
    match (AgentUnavailable::of(e), origin) {
        (Some(AgentUnavailable::NotRunning(_)) | None, _) => reason,
        (Some(_), AgentOrigin::HostAsset) => format!(
            "{reason}\n\"{machine}\": its agent comes with this host's vmlab rather than with \
             anything it boots, so there is nothing to rebuild or repair — an agent that is \
             not answering here is a machine to restart, or a guest asset to reinstall (§19.4)"
        ),
        (Some(_), AgentOrigin::Image) => format!(
            "{reason}\n{}",
            crate::attach::refusal(Some(machine), "an attach", &crate::attach::ATTACH_FEATURES)
        ),
    }
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
                user,
                password,
            } => {
                let m = machine_of(lab, &machine)?;
                let logon = logon_for(m.as_ref(), user.as_deref(), password.as_deref())?;
                let agent = m.agent().await?;
                let mut argv = vec![cmd];
                argv.extend(args);
                let result = on_machine(
                    &machine,
                    agent
                        .exec(
                            argv,
                            vec![],
                            None,
                            None,
                            std::time::Duration::from_secs(timeout),
                            logon,
                        )
                        .await
                        .map_err(CommandError::from),
                )?;
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
                user,
                password,
            } => {
                let m = machine_of(lab, &machine)?;
                let logon = logon_for(m.as_ref(), user.as_deref(), password.as_deref())?;
                let agent = m.agent().await?;
                let session = on_machine(
                    &machine,
                    agent
                        .open_terminal(cols, rows, None, vec![], logon)
                        .await
                        .map_err(CommandError::from),
                )?;
                let id = session.id;
                let path = m.term_session_sock(id);
                vm_agent::expose_terminal_socket(session, path.clone()).await?;
                Ok(json!({"session": id, "path": path}))
            }
            // The SSH facade (§19.3): the lab daemon terminates SSH itself
            // and maps its channels onto the agent's, so the guest runs no
            // sshd. What goes back is a unix socket path and nothing else —
            // `vmlab ssh-proxy` pipes an `ssh` process's stdin/stdout onto
            // it, so nothing listens on the host and no port is leased.
            LabRequest::MachineSshOpen { machine } => {
                let m = machine_of(lab, &machine)?;
                let agent = attach_agent_of(&m, &machine).await?;
                let labels: Vec<String> = m.logins().iter().map(|l| l.label.clone()).collect();
                let key = ssh::host_key::load_or_mint(&lab.name, m.name(), &labels)
                    .map_err(|e| CommandError::failed(format!("{e:#}")))?;
                let events = lab.events.clone();
                let spec = Arc::new(ssh::FacadeSpec {
                    machine: m.name().to_string(),
                    logins: m.logins().to_vec(),
                    guest_os: m.guest_os(),
                    key,
                    host_user: ssh::host_user(),
                    events: Arc::new(move |event, data| events.emit(event, data)),
                });
                let path = m.ssh_session_sock(rand::random());
                ssh::expose_ssh_socket(spec, agent, path.clone()).await?;
                Ok(json!({"path": path}))
            }
            // Rebuild is policy, repair is a tool (§19.4). It is a command
            // and never a reflex: nothing else in the daemon calls this, and
            // a machine it succeeds on is diverged from what it was built
            // from until its disks are destroyed.
            LabRequest::MachineRepairAgent { machine } => {
                let m = machine_of(lab, &machine)?;
                let report = agent_repair::repair(&m)
                    .await
                    .map_err(|e| CommandError::failed(format!("{e:#}")))?;
                lab.record_agent_repair(&machine, &report.pushed)
                    .await
                    .map_err(|e| CommandError::failed(format!("{e:#}")))?;
                Ok(serde_json::to_value(report).unwrap_or_default())
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
            // Fast binary file transfer over the guest's `fileops` channel
            // (§19.5). `from` is a host path the daemon can see (the CLI
            // resolves it absolute first); `data` is inline base64 for
            // callers that have bytes rather than a file.
            //
            // A copy is something a *person* invokes, so it runs as the
            // machine's declared login (§19.2) — the developer's files get
            // the developer's owner, rather than landing as SYSTEM.
            LabRequest::MachinePushFile {
                machine,
                to,
                from,
                data,
                mode,
            } => {
                let m = machine_of(lab, &machine)?;
                let logon = logon_for(m.as_ref(), None, None)?;
                let agent = m.agent().await?;
                let (sha256, len) = match data {
                    Some(data) => {
                        use base64::Engine as _;
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .map_err(|e| {
                                CommandError::invalid(format!("invalid base64 data: {e}"))
                            })?;
                        if bytes.len() as u64 > crate::proto::INLINE_FILE_LIMIT {
                            return Err(crate::proto::over_inline_limit(
                                format!("push {to}"),
                                crate::proto::INLINE_FILE_LIMIT,
                            ));
                        }
                        let tmp = std::env::temp_dir()
                            .join(format!("vmlab-cp-{}-{machine}", std::process::id()));
                        std::fs::write(&tmp, &bytes)
                            .map_err(|e| CommandError::failed(e.to_string()))?;
                        let res = agent.push_file_as(&tmp, &to, mode, logon).await;
                        let _ = std::fs::remove_file(&tmp);
                        on_machine(&machine, Ok(res?))?
                    }
                    None => {
                        let from =
                            from.ok_or_else(|| CommandError::invalid("missing from or data"))?;
                        on_machine(
                            &machine,
                            Ok(agent
                                .push_file_as(std::path::Path::new(&from), &to, mode, logon)
                                .await?),
                        )?
                    }
                };
                Ok(json!({"sha256": sha256, "len": len}))
            }
            // The mirror of push: `to` is a host path the daemon writes, and
            // omitting it hands the bytes back inline for a caller — a
            // browser — that has nowhere on this host to put them.
            LabRequest::MachinePullFile { machine, from, to } => {
                let m = machine_of(lab, &machine)?;
                let logon = logon_for(m.as_ref(), None, None)?;
                let agent = m.agent().await?;
                match to {
                    Some(to) => {
                        let (sha256, len) = on_machine(
                            &machine,
                            Ok(agent
                                .pull_file_as(&from, std::path::Path::new(&to), logon)
                                .await?),
                        )?;
                        Ok(json!({"sha256": sha256, "len": len}))
                    }
                    None => {
                        use base64::Engine as _;
                        let (sha256, bytes) = on_machine(
                            &machine,
                            Ok(agent
                                .pull_bytes_as(&from, crate::proto::INLINE_FILE_LIMIT, logon)
                                .await?),
                        )?;
                        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        Ok(json!({"sha256": sha256, "len": bytes.len(), "data": data}))
                    }
                }
            }
            // Follow a guest file (tail -F semantics), streamed as chunks
            // until the client hangs up or the machine stops.
            // No logon: §19.2 puts `tail` among the things vmlab does on its
            // own behalf, beside readiness and metrics — it reads a log and
            // produces none of the developer's files. §19.5 still puts the
            // field on the open, for the reads a person makes through the
            // SSH facade (#87); this verb is not one of them.
            LabRequest::MachineTail { machine, path } => {
                let m = machine_of(lab, &machine)?;
                let session = m.agent().await?.open_tail(path, None).await?;
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

            // The three `dev sync` verbs that need the daemon (§19.6).
            // `status` is not among them: a syncer's report is part of the
            // machine's status projection, so asking for it twice would be two
            // answers to keep in step (ADR-0004).
            LabRequest::WorkspaceFlush { machine } => Ok(serde_json::to_value(
                lab.workspace_flush(&machine)
                    .await
                    .map_err(|e| CommandError::failed(format!("{e:#}")))?,
            )
            .unwrap_or_default()),
            LabRequest::WorkspaceResolve {
                machine,
                paths,
                all,
                winner,
            } => Ok(serde_json::to_value(
                lab.workspace_resolve(&machine, paths, all, &winner)
                    .await
                    .map_err(|e| CommandError::failed(format!("{e:#}")))?,
            )
            .unwrap_or_default()),
            LabRequest::WorkspaceDiff { machine, paths } => Ok(lab
                .workspace_diff(&machine, paths)
                .await
                .map_err(|e| CommandError::failed(format!("{e:#}")))?),
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
    use super::{attach_failure, handler_matches};
    use crate::config::model::Handler;
    use crate::labd::machine::{AgentOrigin, AgentUnavailable};
    use std::path::PathBuf;

    fn no_agent_answered(machine: &str) -> anyhow::Error {
        anyhow::Error::from(AgentUnavailable::Handshake {
            machine: machine.into(),
            message: "no vmlab-agent answered on the agent channel".into(),
        })
    }

    /// §19.4's top rung. A machine whose agent never answered — a template
    /// built with `agent = false`, or one whose agent is gone — cannot serve
    /// an attach at all, and fails hard naming **both** remedies: the same
    /// words, through the same path, as a stale agent's refused `sftp`.
    #[test]
    fn an_attach_with_no_agent_answering_fails_naming_both_remedies() {
        let said = attach_failure("dev01", AgentOrigin::Image, &no_agent_answered("dev01"));
        assert!(said.contains("no vmlab-agent answered"), "{said}");
        assert!(said.contains("rebuild the template"), "{said}");
        assert!(said.contains("repair-agent dev01"), "{said}");

        // A vintage guest with no agent channel at all is the same answer:
        // not attachable, and told what would change that.
        let vintage = anyhow::Error::from(AgentUnavailable::NoChannel("dos".into()));
        assert!(attach_failure("dos", AgentOrigin::Image, &vintage).contains("repair-agent dos"));
    }

    /// …but a machine that is simply off is told it is off. Nothing about
    /// rebuilding a template helps a stopped machine, and saying it would be
    /// the ladder's words in the one place they are wrong.
    #[test]
    fn an_attach_on_a_stopped_machine_is_not_told_to_rebuild_anything() {
        let stopped = anyhow::Error::from(AgentUnavailable::NotRunning("dev01".into()));
        let said = attach_failure("dev01", AgentOrigin::Image, &stopped);
        assert_eq!(said, "dev01: not running");
    }

    /// Neither remedy exists for a machine whose agent ships with the host: a
    /// container micro-VM has no artefact to rebuild and nothing to push into
    /// it, so sending its author after either would be the ladder's words in
    /// the second place they are wrong (§19.4).
    #[test]
    fn an_attach_to_a_silent_container_is_told_what_its_silence_means() {
        let said = attach_failure("web", AgentOrigin::HostAsset, &no_agent_answered("web"));
        assert!(said.contains("no vmlab-agent answered"), "{said}");
        assert!(said.contains("nothing to rebuild or repair"), "{said}");
        assert!(!said.contains("rebuild the template"), "{said}");
        assert!(!said.contains("repair-agent"), "{said}");
        assert!(said.contains("guest asset"), "{said}");
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
