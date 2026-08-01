//! Lab-scoped CLI verbs (PRD §12): up/down/destroy/status, per-VM power
//! ops, snapshots, exec, logs. The CLI resolves the lab from cwd (or an
//! explicit `lab/vm` reference), starts daemons as needed, and talks to the
//! lab daemon directly.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::daemon;
use crate::proto::client::Client;
use crate::status::{LabStatus, MachineDetail, MachineStatus};

/// Resolve the current lab (name + root) from cwd, like git.
pub fn current_lab() -> Result<(String, std::path::PathBuf)> {
    let cwd = std::env::current_dir()?;
    let root = crate::paths::find_lab_root(&cwd)?;
    let file =
        crate::config::load_lab_root(&root).map_err(|e| anyhow!("{:?}", miette::Report::new(e)))?;
    Ok((file.lab.name, root))
}

/// Resolve a `[lab/]vm` reference (PRD §9.3): with a slash the lab is
/// explicit (daemon must be running); otherwise the cwd's lab.
pub fn split_vm_ref(vm_ref: &str) -> Result<(Option<String>, String)> {
    match vm_ref.split_once('/') {
        Some((lab, vm)) if !lab.is_empty() && !vm.is_empty() => {
            Ok((Some(lab.to_string()), vm.to_string()))
        }
        Some(_) => bail!("malformed reference `{vm_ref}` (expected [lab/]vm)"),
        None => Ok((None, vm_ref.to_string())),
    }
}

async fn lab_client_for(lab: Option<String>) -> Result<(String, Client)> {
    match lab {
        Some(name) => {
            let client = daemon::try_lab_daemon(&name)
                .await
                .ok_or_else(|| anyhow!("lab \"{name}\" is not running"))?;
            Ok((name, client))
        }
        None => {
            let (name, root) = current_lab()?;
            let client = daemon::ensure_lab_daemon(&name, &root).await?;
            Ok((name, client))
        }
    }
}

fn rt() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Runtime::new()?)
}

fn remote(e: crate::proto::ProtoError) -> anyhow::Error {
    anyhow!("{e}")
}

pub fn cmd_up(vms: Vec<String>) -> Result<()> {
    rt()?.block_on(async {
        // Validate before any side effect (PRD §5.1: implicitly every verb).
        super::validate::validate_current()?;
        let (name, root) = current_lab()?;
        let client = daemon::ensure_lab_daemon(&name, &root).await?;
        client
            .call_streaming("up", json!({"vms": vms}), |chunk| print!("{chunk}"))
            .await
            .map_err(remote)?;
        println!("lab \"{name}\" is up");

        // `gui = true` VMs get a detached VNC viewer opened from this
        // interactive session (the daemon is headless and can't reach the
        // user's display). Closing the viewer only disconnects; the VM
        // keeps running (§11). Done CLI-side so VMs always boot headless.
        let file = crate::config::load_lab_root(&root)
            .map_err(|e| anyhow!("{:?}", miette::Report::new(e)))?;
        let lab_gui = file.lab.gui.unwrap_or(false);
        for vm in &file.lab.vms {
            if !vm.gui.unwrap_or(lab_gui) {
                continue;
            }
            if vms.is_empty() || vms.iter().any(|v| v == &vm.name) {
                crate::viewer::open_for(&name, &vm.name)?;
            }
        }
        Ok(())
    })
}

/// `vmlab pull [vms…]`: download missing registry templates/images with
/// streamed progress, without starting anything — the CLI twin of the web
/// UI's "Download templates" button.
pub fn cmd_pull(vms: Vec<String>) -> Result<()> {
    rt()?.block_on(async {
        super::validate::validate_current()?;
        let (name, root) = current_lab()?;
        let client = daemon::ensure_lab_daemon(&name, &root).await?;
        client
            .call_streaming("pull", json!({"vms": vms}), |chunk| print!("{chunk}"))
            .await
            .map_err(remote)?;
        println!("lab \"{name}\": templates ready");
        Ok(())
    })
}

pub fn cmd_down(vms: Vec<String>, force: bool) -> Result<()> {
    rt()?.block_on(async {
        let (name, _root) = current_lab()?;
        let Some(client) = daemon::try_lab_daemon(&name).await else {
            // No daemon — but a daemon that died without stopping its machines
            // leaves QEMU (and swtpm/virtiofsd/smbd) running with the guest
            // disks open, and `down` is exactly where a user asks for that to
            // stop. Releasing the lab makes the supervisor reap them.
            if let Ok(sup) = daemon::ensure_supervisor().await {
                let _ = sup.call("lab.release", json!({"name": name})).await;
            }
            println!("lab \"{name}\" is not running (any orphaned processes were reaped)");
            return Ok(());
        };
        client
            .call("down", json!({"vms": vms, "force": force}))
            .await
            .map_err(remote)?;
        println!("lab \"{name}\" is down (clones retained)");
        Ok(())
    })
}

pub fn cmd_destroy() -> Result<()> {
    rt()?.block_on(async {
        let (name, root) = current_lab()?;
        // Destroy needs a daemon (to stop VMs and delete state) even if one
        // isn't currently running — .vmlab may still hold clones.
        let lab_local = crate::paths::lab_local_dir(&root);
        match daemon::try_lab_daemon(&name).await {
            Some(client) => {
                client.call("destroy", Value::Null).await.map_err(remote)?;
            }
            None if lab_local.exists() => {
                std::fs::remove_dir_all(&lab_local)
                    .with_context(|| format!("removing {}", lab_local.display()))?;
            }
            None => {}
        }
        // Reap the lab daemon.
        if let Ok(sup) = daemon::ensure_supervisor().await {
            let _ = sup.call("lab.release", json!({"name": name})).await;
        }
        println!("lab \"{name}\" destroyed");
        Ok(())
    })
}

pub fn cmd_status(verbose: bool) -> Result<()> {
    rt()?.block_on(async {
        let (name, _root) = current_lab()?;
        let Some(client) = daemon::try_lab_daemon(&name).await else {
            println!("lab \"{name}\": not running");
            return Ok(());
        };
        print!("{}", render_status(&lab_status(&client).await?, verbose));
        Ok(())
    })
}

/// Ask a lab daemon for its status projection (ADR-0004).
///
/// A payload that will not parse is an error, not an empty lab: the daemon and
/// this binary disagreeing about the shape is exactly the failure that once
/// printed a status table with no rows in it.
async fn lab_status(client: &Client) -> Result<LabStatus> {
    let payload = client.call("status", Value::Null).await.map_err(remote)?;
    serde_json::from_value(payload).context("the lab daemon reported a status vmlab cannot read")
}

/// The width of a column: its header, or the widest value under it.
fn column_width(header: &str, values: impl Iterator<Item = usize>) -> usize {
    values.max().unwrap_or(0).max(header.len())
}

/// Render the lab status projection as the `vmlab status` report.
///
/// Returns the text instead of printing it so the rendering can be asserted
/// against a projection built in a test — this is the function a producer-side
/// rename once blanked, with nothing to catch it (ADR-0004).
///
/// One table for both kinds, with the derived label under `STATUS`. The raw
/// power state and the kind-specific fields are `--verbose`: to a user "is it
/// up?" is the question, and `booting` answers it in a way `running` plus a
/// separate readiness column did not.
fn render_status(status: &LabStatus, verbose: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "lab \"{}\"", status.lab);

    if !status.machines.is_empty() {
        let name_w = column_width("NAME", status.machines.iter().map(|m| m.name.len()));
        let status_w = column_width("STATUS", status.machines.iter().map(|m| m.label.text.len()));
        let ip_w = column_width(
            "IP",
            status
                .machines
                .iter()
                .map(|m| or_dash(m.ip.as_deref()).len()),
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  {:<name_w$} {:<9} {:<status_w$} {:<ip_w$} TEMPLATE/IMAGE",
            "NAME", "KIND", "STATUS", "IP"
        );
        for m in &status.machines {
            let _ = writeln!(
                out,
                "  {:<name_w$} {:<9} {:<status_w$} {:<ip_w$} {}",
                m.name,
                m.kind().to_string(),
                m.label.text,
                or_dash(m.ip.as_deref()),
                m.detail.artefact(),
            );
            if verbose {
                let _ = writeln!(out, "      {}", machine_detail(m));
            }
        }
    }

    if !status.segments.is_empty() {
        let name_w = column_width("SEGMENT", status.segments.iter().map(|s| s.name.len()));
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  {:<name_w$} {:<18} {:<15} {:<8} {:<10} PEER",
            "SEGMENT", "SUBNET", "GATEWAY", "NAT/DHCP", "DROPPED"
        );
        for s in &status.segments {
            // PEER: the cross-host trunk state (PRD §9.2) — the configured
            // `connect` target plus whether the trunk is currently up.
            let peer = match (s.connect.as_deref(), s.peer_connected) {
                (Some(host), Some(true)) => format!("{host} (up)"),
                (Some(host), Some(false)) => format!("{host} (down)"),
                (Some(host), None) => host.to_string(),
                (None, Some(true)) => "connected".to_string(),
                _ => "-".to_string(),
            };
            // DROPPED: switch frames shed on this segment. Anything other than
            // 0 means the fabric is losing frames under load — the thing that
            // makes guest transfers mysteriously slow.
            let _ = writeln!(
                out,
                "  {:<name_w$} {:<18} {:<15} {:<8} {:<10} {peer}",
                s.name,
                s.subnet,
                s.gateway,
                format!("{}/{}", on_off(s.nat), on_off(s.dhcp)),
                s.frames.dropped,
            );
        }
    }

    // Downloads in flight, so a lab that looks stuck during `up` can be told
    // apart from one that is merely slow.
    if !status.pulls.is_empty() {
        let name_w = column_width("MACHINE", status.pulls.iter().map(|p| p.machine.len()));
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  {:<name_w$} {:<9} {:<8} REFERENCE",
            "MACHINE", "PULLING", "PERCENT"
        );
        for p in &status.pulls {
            let _ = writeln!(
                out,
                "  {:<name_w$} {:<9} {:<8} {}",
                p.machine,
                p.kind.as_str(),
                format!("{}%", p.percent),
                p.reference,
            );
        }
    }
    out
}

/// The `--verbose` line under a machine: the raw state the derived label was
/// built from, plus whatever its kind alone can report.
fn machine_detail(m: &MachineStatus) -> String {
    let mut detail = format!(
        "state={} ready={} cached={}",
        m.state,
        yes_no(m.ready),
        yes_no(m.cached)
    );
    let mut field = |k: &str, v: String| detail.push_str(&format!(" {k}={v}"));
    match &m.detail {
        MachineDetail::Vm(vm) => {
            field("arch", or_dash(vm.arch.as_deref()).to_string());
            field("cpus", vm.cpus.map_or("-".into(), |c| c.to_string()));
            field(
                "memory",
                vm.memory
                    .map_or("-".into(), crate::template::meta::format_size),
            );
            field("agent", or_dash(vm.agent_version.as_deref()).to_string());
        }
        MachineDetail::Container(c) => {
            field(
                "health",
                match c.health {
                    None => "-".into(),
                    Some(true) => "ok".into(),
                    Some(false) => "failing".into(),
                },
            );
            field("restarts", c.restarts.to_string());
            field(
                "exit",
                c.exit_code.map_or("-".into(), |code| code.to_string()),
            );
            field("digest", or_dash(c.digest.as_deref()).to_string());
        }
    }
    detail
}

fn or_dash(value: Option<&str>) -> &str {
    value.unwrap_or("-")
}

fn yes_no(flag: bool) -> &'static str {
    if flag { "yes" } else { "no" }
}

fn on_off(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

/// Manage running labs host-wide, by name (not the cwd's lab).
#[derive(clap::Subcommand)]
pub enum LabCmd {
    /// List every tracked lab: name, state, and directory
    List {
        /// Emit a JSON array instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Show detailed status (machines and segments) of a running lab
    Info {
        lab: String,
        /// Add the raw power state, readiness, and each machine's
        /// kind-specific detail
        #[arg(short, long)]
        verbose: bool,
    },
    /// Gracefully stop a running lab; clones retained
    Stop {
        lab: String,
        /// Hard kill instead of the graceful ladder
        #[arg(long)]
        force: bool,
    },
    /// Stop a lab and delete its clones and local state
    Destroy { lab: String },
}

pub fn cmd_lab(cmd: LabCmd) -> Result<()> {
    match cmd {
        LabCmd::List { json } => cmd_lab_list(json),
        LabCmd::Info { lab, verbose } => cmd_lab_info(&lab, verbose),
        LabCmd::Stop { lab, force } => cmd_lab_stop(&lab, force),
        LabCmd::Destroy { lab } => cmd_lab_destroy(&lab),
    }
}

/// Ask the supervisor for its lab registry. Returns an empty list when the
/// supervisor isn't running — read-only queries don't auto-start it.
async fn registry_labs() -> Result<Vec<Value>> {
    let sock = crate::paths::supervisor_socket();
    let Ok(client) = Client::connect(&sock).await else {
        return Ok(Vec::new());
    };
    let labs = client.call("status", Value::Null).await.map_err(remote)?;
    Ok(labs.as_array().cloned().unwrap_or_default())
}

/// Find a registry entry's root directory by lab name.
fn root_for(labs: &[Value], name: &str) -> Option<std::path::PathBuf> {
    labs.iter()
        .find(|l| l["name"].as_str() == Some(name))
        .and_then(|l| l["root"].as_str())
        .map(std::path::PathBuf::from)
}

fn cmd_lab_list(json: bool) -> Result<()> {
    rt()?.block_on(async {
        let labs = registry_labs().await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&labs)?);
            return Ok(());
        }
        if labs.is_empty() {
            println!("no running labs");
            return Ok(());
        }
        let name_w = labs
            .iter()
            .map(|l| l["name"].as_str().unwrap_or("?").len())
            .max()
            .unwrap_or(0)
            .max(4);
        println!("{:<name_w$} {:<10} DIRECTORY", "NAME", "STATE");
        for l in &labs {
            println!(
                "{:<name_w$} {:<10} {}",
                l["name"].as_str().unwrap_or("?"),
                l["state"].as_str().unwrap_or("?"),
                l["root"].as_str().unwrap_or("?"),
            );
        }
        Ok(())
    })
}

fn cmd_lab_info(name: &str, verbose: bool) -> Result<()> {
    rt()?.block_on(async {
        let labs = registry_labs().await?;
        let entry = labs.iter().find(|l| l["name"].as_str() == Some(name));
        match daemon::try_lab_daemon(name).await {
            Some(client) => {
                if let Some(root) = entry.and_then(|l| l["root"].as_str()) {
                    println!("directory: {root}");
                }
                print!("{}", render_status(&lab_status(&client).await?, verbose));
                Ok(())
            }
            // Registered but unreachable (e.g. crashed/Failed): show what the
            // registry knows.
            None => match entry {
                Some(l) => {
                    println!(
                        "lab \"{name}\" [{}] (not reachable) directory {}",
                        l["state"].as_str().unwrap_or("?"),
                        l["root"].as_str().unwrap_or("?"),
                    );
                    Ok(())
                }
                None => bail!("lab \"{name}\" is not running"),
            },
        }
    })
}

fn cmd_lab_stop(name: &str, force: bool) -> Result<()> {
    rt()?.block_on(async {
        let Some(client) = daemon::try_lab_daemon(name).await else {
            println!("lab \"{name}\" is not running");
            return Ok(());
        };
        client
            .call("down", json!({"vms": Vec::<String>::new(), "force": force}))
            .await
            .map_err(remote)?;
        println!("lab \"{name}\" is down (clones retained)");
        Ok(())
    })
}

fn cmd_lab_destroy(name: &str) -> Result<()> {
    rt()?.block_on(async {
        let labs = registry_labs().await?;
        let root = root_for(&labs, name);
        match daemon::try_lab_daemon(name).await {
            Some(client) => {
                client.call("destroy", Value::Null).await.map_err(remote)?;
            }
            None => match &root {
                // No daemon, but .vmlab may still hold clones to clean up.
                Some(root) => {
                    let lab_local = crate::paths::lab_local_dir(root);
                    if lab_local.exists() {
                        std::fs::remove_dir_all(&lab_local)
                            .with_context(|| format!("removing {}", lab_local.display()))?;
                    }
                }
                None => bail!("lab \"{name}\" is not running"),
            },
        }
        // Reap the lab daemon.
        if let Ok(sup) = daemon::ensure_supervisor().await {
            let _ = sup.call("lab.release", json!({"name": name})).await;
        }
        println!("lab \"{name}\" destroyed");
        Ok(())
    })
}

pub fn cmd_vm_power(vm_ref: &str, op: &str, force: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        match op {
            "start" => client
                .call("machine.start", json!({"machine": vm}))
                .await
                .map_err(remote)?,
            "stop" => client
                .call("machine.stop", json!({"machine": vm, "force": force}))
                .await
                .map_err(remote)?,
            "restart" => client
                .call("machine.restart", json!({"machine": vm}))
                .await
                .map_err(remote)?,
            _ => unreachable!(),
        };
        Ok(())
    })
}

pub fn cmd_vm_destroy(vm_ref: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        client
            .call("machine.destroy", json!({"machine": vm}))
            .await
            .map_err(remote)?;
        println!("vm \"{vm}\" destroyed");
        Ok(())
    })
}

pub fn cmd_container_power(container_ref: &str, op: &str, force: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, container) = split_vm_ref(container_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        match op {
            "start" => client
                .call("machine.start", json!({"machine": container}))
                .await
                .map_err(remote)?,
            "stop" => client
                .call(
                    "machine.stop",
                    json!({"machine": container, "force": force}),
                )
                .await
                .map_err(remote)?,
            "restart" => client
                .call("machine.restart", json!({"machine": container}))
                .await
                .map_err(remote)?,
            _ => unreachable!(),
        };
        Ok(())
    })
}

pub fn cmd_container_destroy(container_ref: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, container) = split_vm_ref(container_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        client
            .call("machine.destroy", json!({"machine": container}))
            .await
            .map_err(remote)?;
        println!("container \"{container}\" destroyed");
        Ok(())
    })
}

pub fn cmd_container_exec(container_ref: &str, timeout: u64, cmd: Vec<String>) -> Result<()> {
    if cmd.is_empty() {
        bail!("nothing to execute — usage: vmlab container exec <container> -- <cmd> [args...]");
    }
    rt()?.block_on(async {
        let (lab, container) = split_vm_ref(container_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let result = client
            .call(
                "machine.exec",
                json!({"machine": container, "cmd": cmd[0],
                       "args": cmd[1..].to_vec(), "timeout": timeout}),
            )
            .await
            .map_err(remote)?;
        print!("{}", result["stdout"].as_str().unwrap_or(""));
        eprint!("{}", result["stderr"].as_str().unwrap_or(""));
        let code = result["exit_code"].as_i64().unwrap_or(0);
        if code != 0 {
            std::process::exit(code as i32);
        }
        Ok(())
    })
}

/// `vmlab container logs <container>` — dump the console log tail, or with
/// `--follow` stream it (the daemon polls the log for growth) until ^C or
/// the container stops.
pub fn cmd_container_logs(container_ref: &str, follow: bool, lines: usize) -> Result<()> {
    rt()?.block_on(async {
        let (lab, container) = split_vm_ref(container_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        if !follow {
            let logs = client
                .call(
                    "machine.logs",
                    json!({"machine": container, "lines": lines}),
                )
                .await
                .map_err(remote)?;
            let text = logs.as_str().unwrap_or_default();
            if !text.is_empty() {
                println!("{text}");
            }
            return Ok(());
        }
        // Follow: the first chunk is the tail (no trailing newline — the
        // daemon joins lines); later chunks are raw file bytes.
        let mut first = true;
        client
            .call_streaming(
                "machine.logs",
                json!({"machine": container, "lines": lines, "follow": true}),
                |chunk| {
                    if std::mem::take(&mut first) {
                        if !chunk.is_empty() {
                            println!("{chunk}");
                        }
                    } else {
                        print!("{chunk}");
                    }
                },
            )
            .await
            .map_err(remote)?;
        Ok(())
    })
}

/// Print a machine's IP address. One implementation for both kinds — the
/// address comes from the guest agent either way.
pub fn cmd_machine_ip(machine_ref: &str, nic: Option<usize>) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let mut args = json!({"machine": machine});
        if let Some(n) = nic {
            args["nic"] = json!(n);
        }
        let ip = client.call("machine.ip", args).await.map_err(remote)?;
        println!("{}", ip.as_str().unwrap_or_default());
        Ok(())
    })
}

/// `vmlab container shell` — attach an interactive shell running inside the
/// container (vmlab-agent over the `vmlab.agent.0` port, PRD §18). Every
/// attach opens a fresh session; the local terminal goes raw; `Ctrl-]`
/// detaches, like telnet.
pub fn cmd_container_shell(container_ref: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, container) = split_vm_ref(container_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let (cols, rows) = rustix::termios::tcgetwinsize(std::io::stdout())
            .map(|ws| (ws.ws_col, ws.ws_row))
            .unwrap_or((80, 24));
        let opened = client
            .call(
                "machine.tty_open",
                json!({"machine": container, "cols": cols, "rows": rows}),
            )
            .await
            .map_err(remote)?;
        let session = opened["session"].as_u64().unwrap_or(0);
        let path = std::path::PathBuf::from(opened["path"].as_str().unwrap_or_default());
        let resize: super::tty_attach::ResizeFn = {
            let (client, container) = (client.clone(), container.clone());
            std::sync::Arc::new(move |cols, rows| {
                let (client, container) = (client.clone(), container.clone());
                Box::pin(async move {
                    let _ = client
                        .call(
                            "machine.tty_resize",
                            json!({"machine": container, "session": session,
                                   "cols": cols, "rows": rows}),
                        )
                        .await;
                })
            })
        };
        super::tty_attach::attach_tty(
            &path,
            &format!("connected to \"{container}\" — escape character is ^]"),
            resize,
        )
        .await
    })
}

/// `vmlab shell <vm>` — attach an interactive shell inside the VM over the
/// vmlab-agent channel (root on Linux, SYSTEM PowerShell on Windows). Every
/// attach opens a fresh session; concurrent shells are independent.
pub fn cmd_shell(vm_ref: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        // Open at the real terminal size so the first prompt lays out right.
        let (cols, rows) = rustix::termios::tcgetwinsize(std::io::stdout())
            .map(|ws| (ws.ws_col, ws.ws_row))
            .unwrap_or((80, 24));
        let opened = client
            .call(
                "machine.tty_open",
                json!({"machine": vm, "cols": cols, "rows": rows}),
            )
            .await
            .map_err(remote)?;
        let session = opened["session"].as_u64().unwrap_or(0);
        let path = std::path::PathBuf::from(opened["path"].as_str().unwrap_or_default());
        let resize: super::tty_attach::ResizeFn = {
            let (client, vm) = (client.clone(), vm.clone());
            std::sync::Arc::new(move |cols, rows| {
                let (client, vm) = (client.clone(), vm.clone());
                Box::pin(async move {
                    let _ = client
                        .call(
                            "machine.tty_resize",
                            json!({"machine": vm, "session": session, "cols": cols, "rows": rows}),
                        )
                        .await;
                })
            })
        };
        super::tty_attach::attach_tty(
            &path,
            &format!("connected to \"{vm}\" — escape character is ^]"),
            resize,
        )
        .await
    })
}

/// `vmlab tail <vm> <path>` — follow a file inside the guest (tail -F
/// semantics over the agent channel; no network, no shell required).
pub fn cmd_tail(vm_ref: &str, path: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        client
            .call_streaming(
                "machine.tail",
                json!({"machine": vm, "path": path}),
                |chunk| {
                    print!("{chunk}");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                },
            )
            .await
            .map_err(remote)?;
        Ok(())
    })
}

/// `vmlab eventlog <vm>` — follow the Windows event log over the agent
/// channel.
pub fn cmd_eventlog(vm_ref: &str, filter: Option<&str>) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let mut args = json!({"machine": vm});
        if let Some(f) = filter {
            args["filter"] = json!(f);
        }
        client
            .call_streaming("machine.eventlog", args, |chunk| {
                print!("{chunk}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            })
            .await
            .map_err(remote)?;
        Ok(())
    })
}

/// Make a CLI-supplied path absolute against the cwd, so the daemon (whose
/// working directory differs) resolves it to the same file.
fn abs_path(path: &str) -> Result<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

/// Validate an optional `--region x y w h` flag into a JSON value for the RPC.
fn region_value(region: Option<Vec<i64>>) -> Result<Value> {
    match region {
        None => Ok(Value::Null),
        Some(r) if r.len() == 4 => Ok(json!(r)),
        Some(r) => bail!("--region needs 4 values (x y w h), got {}", r.len()),
    }
}

pub fn cmd_vm_screenshot(vm_ref: &str, path: &str) -> Result<()> {
    let out = abs_path(path)?;
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let result = client
            .call(
                "machine.screenshot",
                json!({"machine": vm, "path": out.to_string_lossy()}),
            )
            .await
            .map_err(remote)?;
        println!("{}", result["path"].as_str().unwrap_or(path));
        Ok(())
    })
}

pub fn cmd_vm_sendkeys(vm_ref: &str, chord: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        client
            .call("machine.sendkeys", json!({"machine": vm, "keys": chord}))
            .await
            .map_err(remote)?;
        Ok(())
    })
}

pub fn cmd_vm_mouse_move(vm_ref: &str, x: i64, y: i64) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        client
            .call("machine.mouse_move", json!({"machine": vm, "x": x, "y": y}))
            .await
            .map_err(remote)?;
        Ok(())
    })
}

pub fn cmd_vm_click(vm_ref: &str, x: Option<i64>, y: Option<i64>, button: &str) -> Result<()> {
    if x.is_some() != y.is_some() {
        bail!("click coordinates need both x and y");
    }
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let mut args = json!({"machine": vm, "button": button});
        if let (Some(x), Some(y)) = (x, y) {
            args["x"] = json!(x);
            args["y"] = json!(y);
        }
        client
            .call("machine.mouse_click", args)
            .await
            .map_err(remote)?;
        Ok(())
    })
}

pub fn cmd_vm_drag(vm_ref: &str, x1: i64, y1: i64, x2: i64, y2: i64) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        client
            .call(
                "machine.mouse_drag",
                json!({"machine": vm, "x1": x1, "y1": y1, "x2": x2, "y2": y2}),
            )
            .await
            .map_err(remote)?;
        Ok(())
    })
}

pub fn cmd_vm_ocr(vm_ref: &str, region: Option<Vec<i64>>) -> Result<()> {
    let region = region_value(region)?;
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let text = client
            .call("machine.ocr", json!({"machine": vm, "region": region}))
            .await
            .map_err(remote)?;
        println!("{}", text.as_str().unwrap_or_default());
        Ok(())
    })
}

pub fn cmd_vm_find_image(
    vm_ref: &str,
    image: &str,
    threshold: f64,
    region: Option<Vec<i64>>,
) -> Result<()> {
    let img = std::fs::canonicalize(image).with_context(|| format!("reference image {image}"))?;
    let region = region_value(region)?;
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let m = client
            .call(
                "machine.find_image",
                json!({"machine": vm, "image": img.to_string_lossy(),
                       "threshold": threshold, "region": region}),
            )
            .await
            .map_err(remote)?;
        if m.is_null() {
            eprintln!("no match");
            std::process::exit(1);
        }
        println!(
            "x={} y={} w={} h={} score={:.3} center={},{}",
            m["x"],
            m["y"],
            m["w"],
            m["h"],
            m["score"].as_f64().unwrap_or(0.0),
            m["cx"],
            m["cy"],
        );
        Ok(())
    })
}

pub fn cmd_exec(vm_ref: &str, timeout: u64, cmd: Vec<String>) -> Result<()> {
    if cmd.is_empty() {
        bail!("nothing to execute — usage: vmlab exec <vm> -- <cmd> [args...]");
    }
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let result = client
            .call(
                "machine.exec",
                json!({"machine": vm, "cmd": cmd[0], "args": cmd[1..].to_vec(), "timeout": timeout}),
            )
            .await
            .map_err(remote)?;
        print!("{}", result["stdout"].as_str().unwrap_or(""));
        eprint!("{}", result["stderr"].as_str().unwrap_or(""));
        let code = result["exit_code"].as_i64().unwrap_or(0);
        if code != 0 {
            std::process::exit(code as i32);
        }
        Ok(())
    })
}

/// `vmlab osinfo <vm>` — guest OS identification as one JSON object, fit
/// for machine consumption. config-weave's testlab vmlab backend polls this
/// to detect agent readiness and pick the guest binary, so it stays even
/// though it is just a thin wrapper over the vm.osinfo RPC.
pub fn cmd_osinfo(vm_ref: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let info = client
            .call("machine.osinfo", json!({"machine": vm}))
            .await
            .map_err(remote)?;
        println!("{info}");
        Ok(())
    })
}

/// `vmlab playbook list` — every machine's playbook blocks, with their
/// variable overrides and any in-flight run.
pub fn cmd_playbook_list() -> Result<()> {
    rt()?.block_on(async {
        let (_name, client) = lab_client_for(None).await?;
        let list = client
            .call("playbook.list", Value::Null)
            .await
            .map_err(remote)?;
        let rows = list.as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            println!("no playbook blocks declared in this lab");
            return Ok(());
        }
        for row in rows {
            println!(
                "{} → {} play {}",
                row["machine"].as_str().unwrap_or("?"),
                row["path"].as_str().unwrap_or("?"),
                row["play"].as_str().unwrap_or("?"),
            );
            for var in row["vars"].as_array().unwrap_or(&Vec::new()) {
                println!(
                    "  var {}={}",
                    var["name"].as_str().unwrap_or("?"),
                    var["value"].as_str().unwrap_or(""),
                );
            }
            if let Some(run) = row["running"].as_object() {
                println!(
                    "  {} running since {}",
                    run["kind"].as_str().unwrap_or("?"),
                    run["started"].as_str().unwrap_or("?"),
                );
            }
        }
        Ok(())
    })
}

/// `vmlab playbook check|apply <machine>` — stream the run, then print the
/// per-step report and propagate config-weave's exit code (3 = reboot still
/// required, matching config-weave's own convention).
pub fn cmd_playbook_run(
    machine_ref: &str,
    playbook: Option<String>,
    play: Option<String>,
    apply: bool,
) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let cmd = if apply {
            "playbook.apply"
        } else {
            "playbook.check"
        };
        let result = client
            .call_streaming(
                cmd,
                json!({"machine": machine, "playbook": playbook, "play": play}),
                |chunk| print!("{chunk}"),
            )
            .await
            .map_err(remote)?;

        // Step table from the final --json report (when one came back).
        if let Some(steps) = result["report"]["steps"].as_array() {
            let mut counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for s in steps {
                *counts
                    .entry(s["status"].as_str().unwrap_or("?"))
                    .or_default() += 1;
            }
            let summary: Vec<String> = counts
                .iter()
                .map(|(status, n)| format!("{n} {status}"))
                .collect();
            println!(
                "{}: {}",
                result["mode"].as_str().unwrap_or("?"),
                summary.join(" · ")
            );
        }
        let reboots = result["reboots"].as_u64().unwrap_or(0);
        if reboots > 0 {
            println!("rebooted {reboots} time(s)");
        }
        let code = result["exit_code"].as_i64().unwrap_or(0);
        if code != 0 {
            std::process::exit(code as i32);
        }
        Ok(())
    })
}

/// `vmlab cp <src> <vm>:<dest>` — copy a host file or directory tree into
/// a guest through the agent, creating parent directories.
pub fn cmd_cp(src: &str, dest: &str) -> Result<()> {
    // Split on the *first* colon: VM names contain none, guest paths may
    // (e.g. box:C:/weave). A guest ref on the destination is a push, on the
    // source a pull.
    if let Some((vm_part, guest_dest)) = dest.split_once(':')
        && !vm_part.is_empty()
        && !guest_dest.is_empty()
    {
        return cp_push(src, vm_part, guest_dest);
    }
    if let Some((vm_part, guest_src)) = src.split_once(':')
        && !vm_part.is_empty()
        && !guest_src.is_empty()
    {
        return cp_pull(vm_part, guest_src, dest);
    }
    bail!(
        "one side must be a guest ref — usage: vmlab cp <src> <vm>:<path> | vmlab cp <vm>:<path> <dest>"
    );
}

/// Host → guest. The agent channel moves raw verified bytes.
fn cp_push(src: &str, vm_part: &str, guest_dest: &str) -> Result<()> {
    let src_path = std::path::Path::new(src);
    if !src_path.exists() {
        bail!("source {src} does not exist");
    }
    let (lab, vm) = split_vm_ref(vm_part)?;
    rt()?.block_on(async {
        let (_name, client) = lab_client_for(lab).await?;
        push_via_agent(&client, &vm, src_path, guest_dest).await
    })
}

/// Push one file or a tree over the agent channel (parent directories are
/// created by the agent; digests verified end-to-end by the daemon).
async fn push_via_agent(
    client: &Client,
    vm: &str,
    src: &std::path::Path,
    guest_dest: &str,
) -> Result<()> {
    let entries = crate::labd::vm_agent::walk_tree_for_push(src, guest_dest)?;
    let single = !src.is_dir();
    let mut total = 0u64;
    let mut files = 0usize;
    for (local, to, mode) in entries {
        // The daemon opens the file itself, so hand it an absolute path.
        let from = abs_path(local.to_str().unwrap_or_default())?;
        let args = json!({"machine": vm, "from": from, "to": to, "mode": mode});
        let r = client
            .call("machine.push_file", args)
            .await
            .map_err(remote)?;
        total += r["len"].as_u64().unwrap_or(0);
        files += 1;
    }
    if single {
        println!("pushed {total} bytes to {vm}:{guest_dest}");
    } else {
        println!("pushed {files} file(s), {total} bytes to {vm}:{guest_dest}");
    }
    Ok(())
}

/// Guest → host over the agent channel.
fn cp_pull(vm_part: &str, guest_src: &str, dest: &str) -> Result<()> {
    let (lab, vm) = split_vm_ref(vm_part)?;
    // Into an existing directory: keep the guest file's name.
    let mut dest_path = std::path::PathBuf::from(dest);
    if dest_path.is_dir() {
        let name = guest_src
            .rsplit(['/', '\\'])
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or("pulled");
        dest_path = dest_path.join(name);
    }
    let dest_abs = abs_path(dest_path.to_str().unwrap_or_default())?;
    rt()?.block_on(async {
        let (_name, client) = lab_client_for(lab).await?;
        let r = client
            .call(
                "machine.pull_file",
                json!({"machine": vm, "from": guest_src, "to": dest_abs}),
            )
            .await
            .map_err(remote)?;
        println!(
            "pulled {} bytes to {}",
            r["len"].as_u64().unwrap_or(0),
            dest_path.display()
        );
        Ok(())
    })
}

pub fn cmd_run(script: &str) -> Result<()> {
    rt()?.block_on(async {
        let (name, root) = current_lab()?;
        if !root.join(script).is_file() {
            bail!("script {script} not found under {}", root.display());
        }
        let client = daemon::ensure_lab_daemon(&name, &root).await?;
        client
            .call_streaming("run", json!({"script": script}), |chunk| print!("{chunk}"))
            .await
            .map_err(remote)?;
        Ok(())
    })
}

pub fn cmd_snapshot(vm_ref: Option<String>, name: String) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = match &vm_ref {
            Some(r) => {
                let (l, v) = split_vm_ref(r)?;
                (l, Some(v))
            }
            None => (None, None),
        };
        let (_lab_name, client) = lab_client_for(lab).await?;
        let mut args = json!({"name": name});
        if let Some(v) = vm {
            args["vm"] = json!(v);
        }
        client.call("snapshot.take", args).await.map_err(remote)?;
        println!("snapshot \"{name}\" created");
        Ok(())
    })
}

pub fn cmd_restore(vm_ref: Option<String>, name: String) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = match &vm_ref {
            Some(r) => {
                let (l, v) = split_vm_ref(r)?;
                (l, Some(v))
            }
            None => (None, None),
        };
        let (_lab_name, client) = lab_client_for(lab).await?;
        let mut args = json!({"name": name});
        if let Some(v) = vm {
            args["vm"] = json!(v);
        }
        client
            .call("snapshot.restore", args)
            .await
            .map_err(remote)?;
        println!("snapshot \"{name}\" restored");
        Ok(())
    })
}

pub fn cmd_snapshots(vm_ref: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let snaps = client
            .call("snapshot.list", json!({"machine": vm}))
            .await
            .map_err(remote)?;
        let list = snaps.as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            println!("no snapshots for {vm}");
            return Ok(());
        }
        println!("{:<24} {:<8} TAKEN", "NAME", "KIND");
        for s in list {
            println!(
                "{:<24} {:<8} {}",
                s["name"].as_str().unwrap_or("?"),
                if s["online"].as_bool().unwrap_or(false) {
                    "online"
                } else {
                    "offline"
                },
                s["taken_at"].as_str().unwrap_or("?"),
            );
        }
        Ok(())
    })
}

pub fn cmd_snapshot_delete(vm_ref: &str, name: String) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_lab_name, client) = lab_client_for(lab).await?;
        client
            .call("snapshot.delete", json!({"machine": vm, "name": name}))
            .await
            .map_err(remote)?;
        println!("snapshot \"{name}\" deleted");
        Ok(())
    })
}

/// Render one raw log line according to `format`. `is_events` marks lines
/// from a structured `events.jsonl` (worth pretty-printing); plain-text VM
/// logs pass through unchanged. `color` enables ANSI styling (TTY only).
fn format_log_line(
    line: &str,
    is_events: bool,
    format: crate::cli::LogFormat,
    color: bool,
) -> String {
    use crate::cli::LogFormat;
    if format == LogFormat::Jsonl || !is_events {
        return line.to_string();
    }
    let Ok(ev) = serde_json::from_str::<crate::proto::Event>(line) else {
        return line.to_string();
    };
    let ts = ev.ts.with_timezone(&chrono::Local).format("%H:%M:%S");
    // Flatten the event the same way the web log stream does.
    let summary = crate::logs::format_event(&ev);
    let (event, data) = summary.split_once(' ').unwrap_or((summary.as_str(), ""));
    if color {
        format!("\x1b[2m{ts}\x1b[0m  \x1b[36m{event:<16}\x1b[0m {data}")
    } else {
        format!("{ts}  {event:<16} {data}")
    }
    .trim_end()
    .to_string()
}

/// `vmlab logs [lab/][vm]` — tail or dump JSON-line logs (PRD §8.3). Reads
/// the state-dir files directly so it works with no daemon running.
pub fn cmd_logs(
    target: Option<String>,
    follow: bool,
    lines: usize,
    format: crate::cli::LogFormat,
) -> Result<()> {
    let (lab, vm) = match &target {
        None => (current_lab()?.0, None),
        Some(t) => match split_vm_ref(t)? {
            (Some(lab), vm) => (lab, Some(vm)),
            (None, maybe_vm) => {
                // Bare name: it's a VM in the cwd lab if that lab defines
                // it, otherwise a lab name.
                match current_lab() {
                    Ok((lab_name, root)) => {
                        let file = crate::config::load_lab_root(&root)
                            .map_err(|e| anyhow!("{:?}", miette::Report::new(e)))?;
                        if file.lab.vms.iter().any(|v| v.name == maybe_vm) {
                            (lab_name, Some(maybe_vm))
                        } else {
                            (maybe_vm, None)
                        }
                    }
                    Err(_) => (maybe_vm, None),
                }
            }
        },
    };

    let base = crate::paths::state_dir().join("labs").join(&lab);
    let paths: Vec<std::path::PathBuf> = match &vm {
        Some(vm) => {
            let d = base.join("vms").join(vm);
            vec![d.join("qemu.log"), d.join("serial.log")]
        }
        None => vec![base.join("events.jsonl")],
    };
    let existing: Vec<_> = paths.into_iter().filter(|p| p.exists()).collect();
    if existing.is_empty() {
        bail!(
            "no logs found for {}{}",
            lab,
            vm.map(|v| format!("/{v}")).unwrap_or_default()
        );
    }

    let is_events = |p: &std::path::Path| p.file_name().is_some_and(|n| n == "events.jsonl");
    use std::io::IsTerminal;
    let color = format == crate::cli::LogFormat::Pretty && std::io::stdout().is_terminal();

    for path in &existing {
        // Tail from the end — a serial log can be tens of MB and only the last
        // `lines` are wanted.
        let tail = crate::logs::tail(path, lines);
        if existing.len() > 1 {
            println!("==> {} <==", path.display());
        }
        let ev = is_events(path);
        for line in &tail {
            println!("{}", format_log_line(line, ev, format, color));
        }
    }

    if follow {
        // Poll-based tail on the first file (simple, portable).
        let path = existing[0].clone();
        let ev = is_events(&path);
        let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if len > offset {
                use std::io::{Read, Seek};
                let mut f = std::fs::File::open(&path)?;
                f.seek(std::io::SeekFrom::Start(offset))?;
                let mut buf = String::new();
                f.read_to_string(&mut buf)?;
                for line in buf.lines() {
                    println!("{}", format_log_line(line, ev, format, color));
                }
                offset = len;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{format_log_line, region_value, render_status, root_for};
    use crate::cli::LogFormat;
    use crate::status::fixtures::{container, lab, vm};
    use crate::status::{MachineDetail, MachineStatus, PowerState, PullKind, PullStatus};
    use crate::status::{SegmentFrames, SegmentStatus};
    use serde_json::json;

    /// A machine at an address, since every column but `IP` is exercised by the
    /// shared fixture as it stands.
    fn addressed(
        name: &str,
        state: PowerState,
        ready: bool,
        detail: MachineDetail,
    ) -> MachineStatus {
        MachineStatus {
            ip: Some("10.0.0.5".into()),
            ..crate::status::fixtures::machine(name, state, ready, detail)
        }
    }

    /// The table renders straight off a projection value — no lab, no daemon.
    /// Both kinds share one table and one `STATUS` column carrying the label
    /// the daemon derived, so the CLI and the console cannot word a machine
    /// differently.
    #[test]
    fn the_status_table_reports_the_derived_label_for_both_kinds() {
        let out = render_status(
            &lab(vec![
                addressed("dc01", PowerState::Running, false, vm()),
                addressed(
                    "web",
                    PowerState::Running,
                    true,
                    container(Some(false), None),
                ),
            ]),
            false,
        );
        assert!(out.contains("lab \"demo\""), "got:\n{out}");
        assert!(out.contains("NAME"), "got:\n{out}");
        assert!(out.contains("KIND"), "got:\n{out}");
        assert!(out.contains("STATUS"), "got:\n{out}");
        // A VM that is up but not ready is booting; a container failing its
        // check is unhealthy. Neither wording is built here.
        assert!(out.contains("dc01 vm        booting"), "got:\n{out}");
        assert!(out.contains("web  container unhealthy"), "got:\n{out}");
        // The artefact each kind runs, and its address, stay on the row.
        assert!(out.contains("x86_64/win11"), "got:\n{out}");
        assert!(
            out.contains("docker.io/library/nginx:latest"),
            "got:\n{out}"
        );
        assert!(out.contains("10.0.0.5"), "got:\n{out}");
        // The raw power state is not in the default table.
        assert!(!out.contains("state="), "got:\n{out}");
    }

    /// `--verbose` is where the raw power state went when `STATE`/`READY`
    /// became one derived column, along with the fields only one kind has.
    #[test]
    fn verbose_adds_the_raw_state_and_the_kind_specific_fields() {
        let out = render_status(
            &lab(vec![
                addressed("dc01", PowerState::Running, false, vm()),
                addressed("web", PowerState::Stopping, false, container(None, None)),
            ]),
            true,
        );
        assert!(
            out.contains(
                "state=running ready=no cached=yes arch=x86_64 cpus=4 memory=8GiB agent=0.1.0"
            ),
            "got:\n{out}"
        );
        assert!(
            out.contains("state=stopping ready=no cached=yes health=- restarts=2"),
            "got:\n{out}"
        );
    }

    /// A machine with no address reads as `-` rather than shifting the columns.
    #[test]
    fn a_machine_without_an_address_still_lines_up() {
        let mut stopped = addressed("dc01", PowerState::Stopped, false, vm());
        stopped.ip = None;
        let out = render_status(&lab(vec![stopped]), false);
        assert!(
            out.contains("dc01 vm        stopped -  x86_64/win11"),
            "got:\n{out}"
        );
    }

    /// The segment table surfaces the drop counter — a non-zero value is the
    /// only warning a user gets that the fabric is shedding frames under load.
    #[test]
    fn the_segment_table_reports_dropped_frames() {
        let mut status = lab(Vec::new());
        status.segments.push(SegmentStatus {
            name: "lan".into(),
            subnet: "10.0.0.0/24".into(),
            gateway: "10.0.0.1".into(),
            nat: true,
            dhcp: false,
            global: false,
            connect: None,
            peer_connected: None,
            frames: SegmentFrames {
                dropped: 17,
                ..SegmentFrames::default()
            },
        });
        let out = render_status(&status, false);
        assert!(out.contains("DROPPED"), "got:\n{out}");
        assert!(
            out.contains("lan     10.0.0.0/24        10.0.0.1        on/off   17"),
            "got:\n{out}"
        );
    }

    /// Downloads in flight are reported too, so `up` looking stuck can be told
    /// apart from `up` being slow.
    #[test]
    fn downloads_in_flight_are_listed_with_progress() {
        let mut status = lab(Vec::new());
        status.pulls.push(PullStatus {
            machine: "dc01".into(),
            kind: PullKind::Template,
            reference: "ghcr.io/vmlab/win11:1".into(),
            bytes_done: 512,
            bytes_total: 1024,
            percent: 50,
        });
        let out = render_status(&status, false);
        assert!(
            out.contains("dc01    template  50%      ghcr.io/vmlab/win11:1"),
            "got:\n{out}"
        );
    }

    /// Nothing running is a table with no rows, not a panic or a stray header.
    #[test]
    fn an_empty_lab_renders_just_its_name() {
        assert_eq!(render_status(&lab(Vec::new()), true), "lab \"demo\"\n");
    }

    #[test]
    fn region_value_validates_arity() {
        assert_eq!(region_value(None).unwrap(), serde_json::Value::Null);
        assert_eq!(
            region_value(Some(vec![1, 2, 3, 4])).unwrap(),
            json!([1, 2, 3, 4])
        );
        assert!(region_value(Some(vec![1, 2, 3])).is_err());
        assert!(region_value(Some(vec![1, 2, 3, 4, 5])).is_err());
    }

    #[test]
    fn pretty_formats_events_without_color() {
        let line = json!({
            "event": "vm.started",
            "ts": "2026-06-21T14:32:01Z",
            "data": {"vm": "web01", "pid": 12345}
        })
        .to_string();
        let out = format_log_line(&line, true, LogFormat::Pretty, false);
        assert!(out.contains("vm.started"), "got: {out}");
        assert!(out.contains("vm=web01"), "got: {out}");
        assert!(out.contains("pid=12345"), "got: {out}");
        // No ANSI escapes when color is disabled.
        assert!(!out.contains('\x1b'), "got: {out}");
    }

    #[test]
    fn jsonl_returns_input_verbatim() {
        let line = r#"{"event":"vm.ready","ts":"2026-06-21T14:32:09Z"}"#;
        assert_eq!(format_log_line(line, true, LogFormat::Jsonl, false), line);
    }

    #[test]
    fn plain_text_passes_through_in_pretty() {
        let line = "qemu: some raw serial output";
        assert_eq!(
            format_log_line(line, false, LogFormat::Pretty, true),
            line,
            "non-events lines must pass through untouched"
        );
    }

    #[test]
    fn unparseable_events_line_falls_back_to_raw() {
        let line = "not json at all";
        assert_eq!(format_log_line(line, true, LogFormat::Pretty, false), line);
    }

    #[test]
    fn root_for_matches_by_name() {
        let labs = vec![
            json!({"name": "alpha", "root": "/labs/alpha", "state": "running"}),
            json!({"name": "beta", "root": "/labs/beta", "state": "failed"}),
        ];
        assert_eq!(
            root_for(&labs, "beta").unwrap(),
            std::path::PathBuf::from("/labs/beta")
        );
        assert!(root_for(&labs, "gamma").is_none());
        assert!(root_for(&[], "alpha").is_none());
    }
}
