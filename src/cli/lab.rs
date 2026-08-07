//! Lab-scoped CLI verbs (PRD §12): up/down/destroy/status, per-VM power
//! ops, snapshots, exec, logs. The CLI resolves the lab from cwd (or an
//! explicit `lab/vm` reference), starts daemons as needed, and talks to the
//! lab daemon directly.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use super::daemon::{self, abs_path, remote};
use super::{As, yes_no};
use crate::proto::client::{LabClient, SupClient};
use crate::proto::{LabRequest, Region, SupRequest};
use crate::status::{LabStatus, MachineDetail, MachineStatus};

/// Resolve the current lab (name + root) from cwd, like git — and register
/// it in the managed SSH block on the way past.
///
/// **Any command that successfully loads a lab refreshes the block** (§19.7),
/// which is what makes working inside a lab directory enough to put its
/// machines in an editor's host picker. Rendering and comparing costs a read;
/// a write happens only on a real difference. A failure here **warns**: the
/// command the developer actually ran is not about SSH, and `vmlab ssh` fails
/// hard for itself where the alias is load-bearing.
pub fn current_lab() -> Result<(String, std::path::PathBuf)> {
    let (file, root) = load_lab_here()?;
    crate::ssh_config::refresh_or_warn(&file.lab, &root);
    Ok((file.lab.name, root))
}

/// The same resolution with no side effect at all — for the long-lived
/// callers (`vmlab-web`) that read a lab without being a command a developer
/// typed in its directory.
pub fn lab_here() -> Result<(String, std::path::PathBuf)> {
    let (file, root) = load_lab_here()?;
    Ok((file.lab.name, root))
}

fn load_lab_here() -> Result<(crate::config::LabFile, std::path::PathBuf)> {
    let cwd = std::env::current_dir()?;
    let root = crate::paths::find_lab_root(&cwd)?;
    let file = load_lab_at(&root)?;
    Ok((file, root))
}

fn load_lab_at(root: &std::path::Path) -> Result<crate::config::LabFile> {
    crate::config::load_lab_root(root).map_err(|e| anyhow!("{:?}", miette::Report::new(e)))
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

pub(super) async fn lab_client_for(lab: Option<String>) -> Result<(String, LabClient)> {
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

pub(super) fn rt() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Runtime::new()?)
}

pub fn cmd_up(vms: Vec<String>) -> Result<()> {
    rt()?.block_on(async {
        // Validate before any side effect (PRD §5.1: implicitly every verb).
        super::validate::validate_current()?;
        let (name, root) = current_lab()?;
        let client = daemon::ensure_lab_daemon(&name, &root).await?;
        client
            .send_streaming(
                LabRequest::Up {
                    machines: vms.clone(),
                },
                |chunk| print!("{chunk}"),
            )
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
///
/// ^C cancels the downloads rather than only abandoning them. The daemon
/// outlives this process, so an interrupt that just walked away would leave
/// the transfers running with nobody watching; there is no separate
/// `pull cancel` verb because interrupting the pull is how a person asks for
/// exactly this (issue #38).
pub fn cmd_pull(vms: Vec<String>) -> Result<()> {
    rt()?.block_on(async {
        super::validate::validate_current()?;
        let (name, root) = current_lab()?;
        let client = daemon::ensure_lab_daemon(&name, &root).await?;
        let pull = client.send_streaming(
            LabRequest::Pull {
                machines: vms.clone(),
            },
            |chunk| print!("{chunk}"),
        );
        tokio::pin!(pull);
        tokio::select! {
            result = &mut pull => {
                result.map_err(remote)?;
                println!("lab \"{name}\": templates ready");
                Ok(())
            }
            _ = tokio::signal::ctrl_c() => {
                // The cancel rides a second request on the same multiplexed
                // connection, so it does not wait on the download it aborts.
                let cancelled = cancel_pulls_in_flight(&client, &vms).await?;
                match cancelled.as_slice() {
                    [] => bail!("interrupted — no download was in flight"),
                    machines => bail!("interrupted — cancelled the download for {}", machines.join(", ")),
                }
            }
        }
    })
}

/// Cancel the downloads this `pull` was waiting on, and answer with the
/// machines whose download was aborted.
///
/// `pull.cancel` names a machine, and the lab's status projection is what
/// knows which machines are downloading — so an interrupted `pull` asks it
/// rather than tracking the fan-out itself.
async fn cancel_pulls_in_flight(client: &LabClient, targets: &[String]) -> Result<Vec<String>> {
    let mut cancelled = Vec::new();
    for machine in pulling_machines(&lab_status(client).await?, targets) {
        let aborted = client
            .send(LabRequest::PullCancel {
                machine: machine.clone(),
            })
            .await
            .map_err(remote)?;
        if aborted.as_bool().unwrap_or(false) {
            cancelled.push(machine);
        }
    }
    Ok(cancelled)
}

/// Which machines have a download in flight, once each: one download can have
/// several machines waiting on it, and `status` reports a row per waiter.
///
/// `targets` is the machine list the interrupted `pull` named, and an empty
/// one means it asked for the whole lab. Cancelling only what this invocation
/// waited on is the difference between `^C` on `vmlab pull web` and taking
/// down a download the console or another terminal started.
fn pulling_machines(status: &LabStatus, targets: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    status
        .pulls
        .iter()
        .filter(|p| targets.is_empty() || targets.contains(&p.machine))
        .filter(|p| seen.insert(p.machine.clone()))
        .map(|p| p.machine.clone())
        .collect()
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
                let _ = sup
                    .send(SupRequest::LabRelease { name: name.clone() })
                    .await;
            }
            println!("lab \"{name}\" is not running (any orphaned processes were reaped)");
            return Ok(());
        };
        client
            .send(LabRequest::Down {
                machines: vms,
                force,
            })
            .await
            .map_err(remote)?;
        println!("lab \"{name}\" is down (clones retained)");
        Ok(())
    })
}

/// Every alias a lab (or one machine in it) publishes, for the withdrawal
/// `destroy` performs.
///
/// Best effort by construction: a lab whose file no longer loads still has
/// muxes to kill, and the bare alias is the one every client actually uses.
fn aliases_to_withdraw(
    lab: &str,
    root: Option<&std::path::Path>,
    machine: Option<&str>,
) -> Vec<String> {
    let declared = root
        .and_then(|root| Some((load_lab_at(root).ok()?, root)))
        .map(|(file, root)| {
            let block = crate::ssh_config::LabBlock::of(&file.lab, root);
            match machine {
                Some(m) => block.aliases_for(m),
                None => block.alias_names(),
            }
        });
    match declared {
        Some(aliases) if !aliases.is_empty() => aliases,
        _ => machine
            .map(|m| {
                vec![
                    crate::ssh_config::Alias {
                        machine: m.to_string(),
                        login: None,
                    }
                    .name(lab),
                ]
            })
            .unwrap_or_default(),
    }
}

pub fn cmd_destroy() -> Result<()> {
    rt()?.block_on(async {
        let (name, root) = current_lab()?;
        // Before anything is torn down, and while the stanzas still resolve:
        // `ssh -O exit` is the tool's own way to kill a multiplexer, and it
        // reads the alias out of the block to find the socket (§19.7).
        crate::ssh_config::withdraw(&aliases_to_withdraw(&name, Some(&root), None));
        // Destroy needs a daemon (to stop VMs and delete state) even if one
        // isn't currently running — .vmlab may still hold clones.
        let lab_local = crate::paths::lab_local_dir(&root);
        match daemon::try_lab_daemon(&name).await {
            Some(client) => {
                client.send(LabRequest::Destroy {}).await.map_err(remote)?;
            }
            None if lab_local.exists() => {
                std::fs::remove_dir_all(&lab_local)
                    .with_context(|| format!("removing {}", lab_local.display()))?;
            }
            None => {}
        }
        // Reap the lab daemon.
        if let Ok(sup) = daemon::ensure_supervisor().await {
            let _ = sup
                .send(SupRequest::LabRelease { name: name.clone() })
                .await;
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
async fn lab_status(client: &LabClient) -> Result<LabStatus> {
    let payload = client.send(LabRequest::Status {}).await.map_err(remote)?;
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

    // Dev machines (PRD §19.1), in their own section rather than as a column
    // on every machine: most labs have none, and the two things worth naming —
    // which one is *the* dev machine, and where its workspace lands — do not
    // fit a shared row.
    let devs: Vec<_> = status.dev_machines().collect();
    if !devs.is_empty() {
        let name_w = column_width("DEV", devs.iter().map(|(m, _)| m.name.len()));
        let ws_w = column_width(
            "WORKSPACE",
            devs.iter()
                .map(|(_, d)| or_dash(d.workspace.as_deref()).len()),
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  {:<name_w$} {:<8} {:<7} {:<ws_w$} GUEST WORKSPACE",
            "DEV", "DEFAULT", "ATTACH", "WORKSPACE"
        );
        for (m, dev) in &devs {
            // ATTACH is `attachable` (§19.4) — whether this machine's agent
            // can serve an attach at all, which is the question asked here
            // and not, ever, whether *your* attach will succeed.
            let _ = writeln!(
                out,
                "  {:<name_w$} {:<8} {:<7} {:<ws_w$} {}",
                m.name,
                yes_no(dev.default),
                yes_no(m.attachable),
                or_dash(dev.workspace.as_deref()),
                dev.workspace_guest,
            );
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
        "state={} ready={} cached={} attachable={}",
        m.state,
        yes_no(m.ready),
        yes_no(m.cached),
        yes_no(m.attachable),
    );
    // Only where it is true: a machine nothing has changed in place is the
    // ordinary case, and saying so on every line would bury the one that
    // matters (§19.4).
    if m.agent_diverged {
        detail.push_str(" diverged=yes");
    }
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

fn on_off(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

/// `vmlab dns` — the zones this lab's segments serve, which is what to read
/// when a guest cannot resolve a peer. Segments with no local zone (global
/// ones, and `dns { enabled = false }`) serve nothing and are not listed.
pub fn cmd_dns(json: bool) -> Result<()> {
    rt()?.block_on(async {
        let (_name, client) = lab_client_for(None).await?;
        let table = client.send(LabRequest::DnsTable {}).await.map_err(remote)?;
        super::emit(json, &table, render_dns)
    })
}

/// One table per segment, with the zone's exact records, its wildcards and
/// its sinkholes in one list: a name either resolves or it does not, and
/// which of the three rules decided is the `KIND` column, not a separate
/// table to cross-reference.
fn render_dns(table: &Value) -> String {
    use std::fmt::Write as _;
    let segments = table["segments"].as_array().cloned().unwrap_or_default();
    if segments.is_empty() {
        return "no segment in this lab serves DNS\n".to_string();
    }
    let mut out = String::new();
    for segment in &segments {
        let zone = &segment["zone"];
        let _ = writeln!(
            out,
            "segment \"{}\" — zone {}",
            segment["segment"].as_str().unwrap_or("?"),
            zone["suffix"].as_str().unwrap_or("?"),
        );
        let rows = dns_rows(zone);
        if rows.is_empty() {
            let _ = writeln!(out, "  (no records)");
            continue;
        }
        let name_w = column_width("NAME", rows.iter().map(|(name, _, _)| name.len()));
        let ip_w = column_width("IP", rows.iter().map(|(_, ip, _)| ip.len()));
        let _ = writeln!(out, "  {:<name_w$} {:<ip_w$} KIND", "NAME", "IP");
        for (name, ip, kind) in rows {
            let _ = writeln!(out, "  {name:<name_w$} {ip:<ip_w$} {kind}");
        }
    }
    out
}

/// One zone's rules as `(name, address, kind)` rows: exact records first, then
/// wildcards, then sinkholes — the order the resolver consults them in.
fn dns_rows(zone: &Value) -> Vec<(String, String, String)> {
    let list = |key: &str| zone[key].as_array().cloned().unwrap_or_default();
    let text = |v: &Value, key: &str| v[key].as_str().unwrap_or("?").to_string();
    let mut rows: Vec<(String, String, String)> = list("records")
        .iter()
        .map(|r| (text(r, "name"), text(r, "ip"), text(r, "kind")))
        .collect();
    rows.extend(
        list("wildcards")
            .iter()
            .map(|w| (text(w, "pattern"), text(w, "ip"), "wildcard".to_string())),
    );
    // A sinkhole answers with nothing, so it has no address to show; the mode
    // is what a reader needs — NXDOMAIN and 0.0.0.0 fail differently in a
    // guest.
    rows.extend(list("sinkholes").iter().map(|s| {
        (
            text(s, "pattern"),
            "-".to_string(),
            format!("sinkhole/{}", text(s, "mode")),
        )
    }));
    rows
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
    /// Restart a lab's daemon so it re-reads vmlab.wcl.
    ///
    /// Not `down` + `up`: that stops every machine and re-runs provisioning,
    /// whereas this replaces only the daemon. The lab must already be stopped
    /// — a fresh daemon cannot re-adopt running machines — so a running lab
    /// is refused rather than quietly taken down.
    Restart {
        lab: String,
        /// Emit the raw JSON reply instead of a confirmation
        #[arg(long)]
        json: bool,
    },
    /// Stop a lab and delete its clones and local state
    Destroy { lab: String },
}

pub fn cmd_lab(cmd: LabCmd) -> Result<()> {
    match cmd {
        LabCmd::List { json } => cmd_lab_list(json),
        LabCmd::Info { lab, verbose } => cmd_lab_info(&lab, verbose),
        LabCmd::Stop { lab, force } => cmd_lab_stop(&lab, force),
        LabCmd::Restart { lab, json } => cmd_lab_restart(&lab, json),
        LabCmd::Destroy { lab } => cmd_lab_destroy(&lab),
    }
}

/// Ask the supervisor for its lab registry. Returns an empty list when the
/// supervisor isn't running — read-only queries don't auto-start it.
async fn registry_labs() -> Result<Vec<Value>> {
    let sock = crate::paths::supervisor_socket();
    let Ok(client) = SupClient::connect(&sock).await else {
        return Ok(Vec::new());
    };
    let labs = client.send(SupRequest::Status {}).await.map_err(remote)?;
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
            return super::print_json(&Value::Array(labs));
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
            .send(LabRequest::Down {
                machines: Vec::new(),
                force,
            })
            .await
            .map_err(remote)?;
        println!("lab \"{name}\" is down (clones retained)");
        Ok(())
    })
}

/// `vmlab lab restart <lab>` — replace a lab's daemon so it re-reads
/// `vmlab.wcl`.
///
/// Not `down` + `up`: that stops the machines and starts them again, running
/// every provision script a second time. This replaces only the daemon. It
/// does still need the lab stopped first — a fresh daemon cannot re-adopt
/// machines the old one was running, and its own shutdown stops them — so a
/// running lab is refused rather than quietly taken down, which is the rule
/// the console's reload button follows too.
fn cmd_lab_restart(name: &str, json: bool) -> Result<()> {
    rt()?.block_on(async {
        let labs = registry_labs().await?;
        // Prefer the cwd when it declares this name. The supervisor must see
        // that requested root so it can reject a collision; substituting the
        // registered root here would make two different labs look identical.
        // Outside that lab, a named restart still uses the registry entry.
        let root = match current_lab() {
            Ok((cwd_lab, root)) if cwd_lab == name => root,
            _ => root_for(&labs, name).ok_or_else(|| anyhow!("lab \"{name}\" is not running"))?,
        };
        // Machines still running is a veto: the restart shuts the old daemon
        // down, and that stops them. A `status` that would not answer or
        // would not parse is *not* — a lab whose `vmlab.wcl` no longer loads
        // is exactly what restart exists to recover from, and blocking there
        // would make the verb useless when it is needed most (the console's
        // reload button reasons the same way).
        if let Some(client) = daemon::try_lab_daemon(name).await
            && let Ok(status) = lab_status(&client).await
            && !status.all_stopped()
        {
            bail!(
                "lab \"{name}\" still has machines running — stop them first; \
                 a restarted daemon cannot re-adopt them"
            );
        }
        let supervisor = daemon::ensure_supervisor().await?;
        let reply = supervisor
            .send(SupRequest::LabRestart {
                name: name.to_string(),
                root,
            })
            .await
            .map_err(remote)?;
        // Follow the socket the supervisor answered with, not the one this
        // process already knew: the daemon that owned that one is gone.
        let socket = std::path::PathBuf::from(
            reply["socket"]
                .as_str()
                .context("malformed lab.restart response")?,
        );
        LabClient::connect(&socket)
            .await?
            .send(LabRequest::Ping {})
            .await
            .map_err(remote)?;
        super::emit(json, &reply, |_| {
            format!("lab \"{name}\" daemon restarted at {}\n", socket.display())
        })
    })
}

fn cmd_lab_destroy(name: &str) -> Result<()> {
    rt()?.block_on(async {
        let labs = registry_labs().await?;
        let root = root_for(&labs, name);
        crate::ssh_config::withdraw(&aliases_to_withdraw(name, root.as_deref(), None));
        match daemon::try_lab_daemon(name).await {
            Some(client) => {
                client.send(LabRequest::Destroy {}).await.map_err(remote)?;
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
            let _ = sup
                .send(SupRequest::LabRelease {
                    name: name.to_string(),
                })
                .await;
        }
        println!("lab \"{name}\" destroyed");
        Ok(())
    })
}

/// Which power operation a `vm`/`container` verb asked for. The two nouns
/// are two ways to say the same wire request, so they share one path — the
/// noun only decides the wording of what is printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerOp {
    Start,
    Stop,
    Restart,
}

impl PowerOp {
    /// The request this operation makes against one machine.
    fn request(self, machine: String, force: bool) -> LabRequest {
        match self {
            PowerOp::Start => LabRequest::MachineStart { machine },
            PowerOp::Stop => LabRequest::MachineStop { machine, force },
            PowerOp::Restart => LabRequest::MachineRestart { machine, force },
        }
    }
}

/// `vmlab vm start|stop|restart` and `vmlab container start|stop|restart`.
pub fn cmd_machine_power(machine_ref: &str, op: PowerOp, force: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        client
            .send(op.request(machine, force))
            .await
            .map_err(remote)?;
        Ok(())
    })
}

/// `vmlab vm destroy` and `vmlab container destroy`; `noun` is what the
/// confirmation calls the machine.
pub fn cmd_machine_destroy(machine_ref: &str, noun: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (name, client) = lab_client_for(lab).await?;
        // The mux goes first, while the alias still resolves (§19.7). The
        // stanza itself stays — a destroyed machine is still a *declared*
        // one, and the host key it will present next time is unchanged.
        let root = root_for(&registry_labs().await?, &name);
        crate::ssh_config::withdraw(&aliases_to_withdraw(&name, root.as_deref(), Some(&machine)));
        client
            .send(LabRequest::MachineDestroy {
                machine: machine.clone(),
            })
            .await
            .map_err(remote)?;
        println!("{noun} \"{machine}\" destroyed");
        Ok(())
    })
}

/// `vmlab container exec` — the container spelling of [`cmd_exec`]; `usage`
/// is the invocation the error message suggests.
pub fn cmd_container_exec(
    container_ref: &str,
    timeout: u64,
    cmd: Vec<String>,
    run_as: As,
) -> Result<()> {
    exec_on_machine(
        container_ref,
        timeout,
        cmd,
        run_as,
        "vmlab container exec <container> -- <cmd> [args...]",
    )
}

/// `vmlab container logs <container>` — dump the console log tail, or with
/// `--follow` stream it (the daemon polls the log for growth) until ^C or
/// the container stops.
pub fn cmd_container_logs(container_ref: &str, follow: bool, lines: usize) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(container_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        if !follow {
            let logs = client
                .send(LabRequest::MachineLogs {
                    machine,
                    lines,
                    follow: false,
                })
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
            .send_streaming(
                LabRequest::MachineLogs {
                    machine,
                    lines,
                    follow: true,
                },
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
        let ip = client
            .send(LabRequest::MachineIp { machine, nic })
            .await
            .map_err(remote)?;
        println!("{}", ip.as_str().unwrap_or_default());
        Ok(())
    })
}

/// `vmlab container shell` — attach an interactive shell running inside the
/// container (vmlab-agent over the `vmlab.agent.0` port, PRD §18). Every
/// attach opens a fresh session; the local terminal goes raw; `Ctrl-]`
/// detaches, like telnet.
pub fn cmd_container_shell(container_ref: &str, run_as: As) -> Result<()> {
    rt()?.block_on(shell_on_machine(container_ref, run_as))
}

/// `vmlab shell <vm>` — attach an interactive shell inside the VM over the
/// vmlab-agent channel. Every attach opens a fresh session; concurrent
/// shells are independent.
///
/// The shell is the machine's default `login {}` where it declares one, and
/// SYSTEM/root where it does not (PRD §19.2). `--user`/`--password` pick
/// another identity, and `--user SYSTEM` (or `root`) is the agent identity
/// by name.
pub fn cmd_shell(vm_ref: &str, run_as: As) -> Result<()> {
    rt()?.block_on(shell_on_machine(vm_ref, run_as))
}

/// Open an agent terminal on one machine and hand the local terminal over to
/// it. Both shell verbs land here — a VM and a container serve the same
/// request.
async fn shell_on_machine(machine_ref: &str, run_as: As) -> Result<()> {
    let (lab, machine) = split_vm_ref(machine_ref)?;
    let (_name, client) = lab_client_for(lab).await?;
    // Open at the real terminal size so the first prompt lays out right.
    let (cols, rows) = rustix::termios::tcgetwinsize(std::io::stdout())
        .map(|ws| (ws.ws_col, ws.ws_row))
        .unwrap_or((80, 24));
    let opened = client
        .send(LabRequest::MachineTtyOpen {
            machine: machine.clone(),
            cols,
            rows,
            user: run_as.user,
            password: run_as.password,
        })
        .await
        .map_err(remote)?;
    let session = opened["session"].as_u64().unwrap_or(0) as u32;
    let path = std::path::PathBuf::from(opened["path"].as_str().unwrap_or_default());
    let resize: super::tty_attach::ResizeFn = {
        let (client, machine) = (client.clone(), machine.clone());
        std::sync::Arc::new(move |cols, rows| {
            let (client, machine) = (client.clone(), machine.clone());
            Box::pin(async move {
                let _ = client
                    .send(LabRequest::MachineTtyResize {
                        machine,
                        session,
                        cols,
                        rows,
                    })
                    .await;
            })
        })
    };
    super::tty_attach::attach_tty(
        &path,
        &format!("connected to \"{machine}\" — escape character is ^]"),
        resize,
    )
    .await
}

/// `vmlab ssh [lab/]<machine> [-- cmd]` — refresh the managed block, then
/// hand the terminal to the **system `ssh`** against the generated alias
/// (§19.7).
///
/// Not a second SSH client: one implementation of the client side, and it is
/// the one editors already use — so a developer's `Host *` settings,
/// `ssh_config` habits and `ssh` version are the ones in play, and a failure
/// here is a failure they can reproduce with `ssh <alias>`.
///
/// **The refresh fails hard.** Everywhere else a failed write warns, because
/// the command was about something else; here the alias *is* the command, and
/// a stale or displaced block would otherwise send `ssh` somewhere unrelated
/// (§19.7's ladder).
///
/// **It refuses on a stopped machine and never starts one**, matching
/// `console` and `exec` — which is also why it asks a daemon that is already
/// running rather than spawning one to be told no.
pub fn cmd_ssh(machine_ref: &str, cmd: Vec<String>) -> Result<()> {
    let (lab_ref, machine) = split_vm_ref(machine_ref)?;
    let alias = rt()?.block_on(async {
        let (name, root) = match &lab_ref {
            None => lab_here()?,
            // `<lab>/<machine>` from anywhere: the registry is what knows
            // where a lab by that name lives (ADR-0011).
            Some(name) => {
                let root = root_for(&registry_labs().await?, name)
                    .ok_or_else(|| anyhow!("lab \"{name}\" is not running"))?;
                (name.clone(), root)
            }
        };

        let file = load_lab_at(&root)?;
        if file.lab.machine(&machine).is_none() {
            bail!("lab \"{name}\" declares no machine \"{machine}\"");
        }
        let (managed, _, outcome) = crate::ssh_config::refresh_lab(&file.lab, &root)
            .context("the managed SSH block must be current before `ssh` can use it")?;
        let alias = crate::ssh_config::Alias {
            machine: machine.clone(),
            login: None,
        };
        // A write verified itself; an unchanged block did not, and this is
        // the one command that must not proceed on an alias OpenSSH resolves
        // to somebody else's `ProxyCommand`.
        if outcome == crate::ssh_config::Outcome::Unchanged {
            managed.verify(&name, Some(&alias))?;
        }

        // Liveness, from the daemon that is already up. No daemon at all
        // means nothing is running, which is the same refusal one step
        // earlier and without starting anything.
        let Some(client) = daemon::try_lab_daemon(&name).await else {
            bail!(
                "lab \"{name}\" is not running — `vmlab ssh` never starts a machine; \
                 run `vmlab up {machine}` first"
            );
        };
        let status = lab_status(&client).await?;
        let found = status
            .machines
            .iter()
            .find(|m| m.name == machine)
            .ok_or_else(|| {
                anyhow!(
                    "the daemon for lab \"{name}\" does not know machine \"{machine}\" — it \
                     predates an edit to {}; run `vmlab lab restart {name}`",
                    crate::paths::LAB_FILE
                )
            })?;
        if found.state != crate::status::PowerState::Running {
            bail!(
                "machine \"{machine}\" is {} — `vmlab ssh` never starts a machine; \
                 run `vmlab up {machine}` first",
                found.label.text
            );
        }
        Ok(alias.name(&name))
    })?;

    exec_ssh(&alias, &cmd)
}

/// Become `ssh`. Nothing after this line runs in this process — which is the
/// point: signals, the terminal, `~.` and the exit code are the client's,
/// exactly as if the developer had typed `ssh <alias>`.
fn exec_ssh(alias: &str, cmd: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt as _;

    let mut ssh = std::process::Command::new("ssh");
    ssh.arg(alias).args(cmd);
    Err(anyhow!("running ssh: {}", ssh.exec()))
}

/// `vmlab ssh-config [--print <machine>]` — refresh the managed block for the
/// lab in this directory (§19.7).
///
/// The verb exists for the two moments the ambient refresh cannot serve: when
/// the developer wants to *know* the block is current, and when their client
/// will not read `~/.ssh/config` at all, which is what `--print` is for
/// (§19.8). Its own failure is loud, because the write is what was asked for.
pub fn cmd_ssh_config(print: Option<&str>) -> Result<()> {
    let (_name, root) = lab_here()?;
    let file = load_lab_at(&root)?;
    let (managed, block, outcome) = crate::ssh_config::refresh_lab(&file.lab, &root)?;

    // A login the block could not give an alias is said out loud here, at the
    // verb whose job the block is — an identity missing from the editor's
    // picker for a reason nobody ever states is the failure this avoids.
    for (machine, label) in &block.unaliasable {
        eprintln!(
            "vmlab: machine \"{machine}\": login \"{label}\" gets no alias — the label has to be \
             one ssh_config word (letters, digits, `-`, `_`, `.`). Attach with \
             `ssh -l \"{label}\" {}`.",
            crate::ssh_config::Alias {
                machine: machine.clone(),
                login: None
            }
            .name(&block.lab)
        );
    }

    let Some(machine) = print else {
        println!(
            "{} — {} ({} alias{} for lab \"{}\")",
            managed.path.display(),
            match outcome {
                crate::ssh_config::Outcome::Wrote => "block updated",
                crate::ssh_config::Outcome::Unchanged => "block already current",
            },
            block.aliases.len(),
            if block.aliases.len() == 1 { "" } else { "es" },
            block.lab
        );
        return Ok(());
    };

    print!("{}\n\n", managed.print(&block, machine)?);
    let family = guest_family(&file.lab, machine)?;
    print!(
        "{}",
        crate::ssh_config::editor_snippet(
            &block.aliases_for(machine),
            family == crate::labd::guest_os::GuestOs::Windows
        )
    );
    Ok(())
}

/// Which guest family a declared machine runs — the one thing the editor
/// snippet needs, since `remote.SSH.remotePlatform` exists for Windows and
/// nothing else (§19.8).
///
/// Resolved through the **effective** profile, not the declared one: a VM
/// usually names no profile at all and inherits its template's (§5.2), and
/// `template = "x86_64/win"` is exactly the shape a Windows dev machine is
/// declared in. Reading the store here is what `vmlab validate` already does
/// for the same reason.
fn guest_family(
    lab: &crate::config::model::Lab,
    machine: &str,
) -> Result<crate::labd::guest_os::GuestOs> {
    use crate::config::model::{MachineCfg, TemplateRef};

    let found = lab
        .machine(machine)
        .ok_or_else(|| anyhow!("lab \"{}\" declares no machine \"{machine}\"", lab.name))?;
    let profile = match found {
        MachineCfg::Container(c) => c.profile.clone(),
        MachineCfg::Vm(vm) => {
            let meta = match &vm.template {
                TemplateRef::Store {
                    arch,
                    name,
                    version,
                } => crate::template::TemplateStore::new(crate::paths::template_store_dir())
                    .resolve(arch, name, version.as_deref())
                    .ok()
                    .map(|t| t.meta),
                // A registry reference not yet pulled, and `scratch`, have no
                // template layer to inherit from; the VM's own word is all
                // there is.
                _ => None,
            };
            crate::qemu::resolve::effective_profile_name(vm, meta.as_ref())
        }
    };
    Ok(crate::labd::guest_os::guest_os_of(profile.as_deref()))
}

/// `vmlab ssh-proxy [lab/]<machine>` — the `ProxyCommand` an `ssh` process
/// is given (PRD §19.3). It asks the lab for a socket onto the machine's SSH
/// facade and copies bytes between that socket and this process's
/// stdin/stdout. That is all it does: the proxy *is* the client's server
/// connection, so nothing listens on the host and no port is leased.
///
/// **It never does lifecycle.** It is spawned by an editor with no TTY, its
/// stderr may never be shown, and a client spawns several concurrently — so
/// "boot and wait" would be a silent multi-minute hang racing itself against
/// the client's own connect timeout. It fails immediately instead, with a
/// diagnostic that survives being printed into an editor's log (§19.7).
pub fn cmd_ssh_proxy(machine_ref: &str) -> Result<()> {
    rt()?.block_on(async {
        use tokio::io::AsyncWriteExt as _;

        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let opened = client
            .send(LabRequest::MachineSshOpen {
                machine: machine.clone(),
            })
            .await
            .map_err(remote)?;
        let path = opened["path"]
            .as_str()
            .ok_or_else(|| anyhow!("lab daemon returned no socket for \"{machine}\""))?;
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .with_context(|| format!("connecting the SSH facade socket for \"{machine}\""))?;

        let (mut from_facade, mut to_facade) = stream.into_split();
        // Both directions run to their own end: `ssh` closing its side must
        // reach the facade as EOF, and the facade closing must end this
        // process rather than leave a proxy holding a dead socket.
        let up = tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let _ = tokio::io::copy(&mut stdin, &mut to_facade).await;
            let _ = to_facade.shutdown().await;
        });
        let mut stdout = tokio::io::stdout();
        let copied = tokio::io::copy(&mut from_facade, &mut stdout).await;
        let _ = stdout.flush().await;
        up.abort();
        copied.context("the SSH facade closed the connection")?;
        Ok(())
    })
}

/// `vmlab tail <vm> <path>` — follow a file inside the guest (tail -F
/// semantics over the agent channel; no network, no shell required).
pub fn cmd_tail(vm_ref: &str, path: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        client
            .send_streaming(
                LabRequest::MachineTail {
                    machine: vm,
                    path: path.to_string(),
                },
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
        client
            .send_streaming(
                LabRequest::MachineEventLog {
                    machine: vm,
                    filter: filter.map(str::to_string),
                },
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

/// Validate an optional `--region x y w h` flag into the request's rectangle.
fn region_value(region: Option<Vec<i64>>) -> Result<Option<Region>> {
    match region.as_deref() {
        None => Ok(None),
        Some([x, y, w, h]) => {
            let at = |v: i64| v.max(0) as u32;
            Ok(Some(Region {
                x: at(*x),
                y: at(*y),
                w: at(*w),
                h: at(*h),
            }))
        }
        Some(r) => bail!("--region needs 4 values (x y w h), got {}", r.len()),
    }
}

pub fn cmd_vm_screenshot(vm_ref: &str, path: &str) -> Result<()> {
    let out = abs_path(std::path::Path::new(path))?;
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let result = client
            .send(LabRequest::MachineScreenshot {
                machine: vm,
                path: out.to_string_lossy().into_owned(),
            })
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
            .send(LabRequest::MachineSendKeys {
                machine: vm,
                keys: chord.to_string(),
            })
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
            .send(LabRequest::MachineMouseMove { machine: vm, x, y })
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
        client
            .send(LabRequest::MachineMouseClick {
                machine: vm,
                button: button.to_string(),
                x,
                y,
            })
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
            .send(LabRequest::MachineMouseDrag {
                machine: vm,
                x1,
                y1,
                x2,
                y2,
            })
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
            .send(LabRequest::MachineOcr {
                machine: vm,
                region,
            })
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
            .send(LabRequest::MachineFindImage {
                machine: vm,
                image: img.to_string_lossy().into_owned(),
                threshold,
                region,
            })
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

/// `vmlab exec <vm> -- <cmd>` — run one command in the guest and mirror its
/// output and exit code.
///
/// **This stops being SYSTEM/root on a machine that declares a `login {}`**:
/// it runs as that login, so pushing into `C:\Windows\System32` starts
/// failing where it used to work. Only machines that opted in are affected,
/// and it is what makes "I am the dev user on this box" true in every verb
/// rather than in one (PRD §19.2). `--user SYSTEM` (or `root`) asks for the
/// agent identity by name.
pub fn cmd_exec(vm_ref: &str, timeout: u64, cmd: Vec<String>, run_as: As) -> Result<()> {
    exec_on_machine(
        vm_ref,
        timeout,
        cmd,
        run_as,
        "vmlab exec <vm> -- <cmd> [args...]",
    )
}

/// Run a command in one machine's guest and mirror its output and exit code.
/// Both exec verbs land here; `usage` is the invocation to suggest when the
/// caller passed nothing to run.
fn exec_on_machine(
    machine_ref: &str,
    timeout: u64,
    mut cmd: Vec<String>,
    run_as: As,
    usage: &str,
) -> Result<()> {
    if cmd.is_empty() {
        bail!("nothing to execute — usage: {usage}");
    }
    let args = cmd.split_off(1);
    let program = cmd.remove(0);
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let result = client
            .send(LabRequest::MachineExec {
                machine,
                cmd: program,
                args,
                timeout,
                user: run_as.user,
                password: run_as.password,
            })
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
            .send(LabRequest::MachineOsInfo {
                machine: vm,
                timeout: 30,
            })
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
            .send(LabRequest::PlaybookList {})
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
        let req = if apply {
            LabRequest::PlaybookApply {
                machine,
                playbook,
                play,
            }
        } else {
            LabRequest::PlaybookCheck {
                machine,
                playbook,
                play,
            }
        };
        let result = client
            .send_streaming(req, |chunk| print!("{chunk}"))
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
    client: &LabClient,
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
        let from = abs_path(&local)?;
        let r = client
            .send(LabRequest::MachinePushFile {
                machine: vm.to_string(),
                to,
                from: Some(from.to_string_lossy().into_owned()),
                data: None,
                mode,
            })
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
    let dest_abs = abs_path(&dest_path)?;
    rt()?.block_on(async {
        let (_name, client) = lab_client_for(lab).await?;
        let r = client
            .send(LabRequest::MachinePullFile {
                machine: vm,
                from: guest_src.to_string(),
                to: Some(dest_abs.to_string_lossy().into_owned()),
            })
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
            .send_streaming(
                LabRequest::Run {
                    script: script.to_string(),
                },
                |chunk| print!("{chunk}"),
            )
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
        client
            .send(LabRequest::SnapshotTake {
                name: name.clone(),
                machine: vm,
            })
            .await
            .map_err(remote)?;
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
        client
            .send(LabRequest::SnapshotRestore {
                name: name.clone(),
                machine: vm,
            })
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
            .send(LabRequest::SnapshotList {
                machine: vm.clone(),
            })
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
            .send(LabRequest::SnapshotDelete {
                machine: vm,
                name: name.clone(),
            })
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
    use super::{
        LabRequest, PowerOp, Region, format_log_line, pulling_machines, region_value, render_dns,
        render_status, root_for,
    };
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

    /// `vmlab status` names which machine is the dev machine, off the same
    /// projection the console reads (§19.1) — and says nothing at all about
    /// dev machines in a lab that has none, which is most labs.
    #[test]
    fn the_status_table_names_the_dev_machine() {
        use crate::status::fixtures::dev;
        let out = render_status(
            &lab(vec![
                addressed("dc01", PowerState::Running, true, vm()),
                dev(addressed("dev01", PowerState::Running, true, vm()), true),
                dev(
                    addressed("buildbox", PowerState::Running, true, container(None, None)),
                    false,
                ),
            ]),
            false,
        );
        assert!(out.contains("DEV"), "got:\n{out}");
        assert!(out.contains("GUEST WORKSPACE"), "got:\n{out}");
        assert!(
            out.contains("dev01    yes      no      ./src     C:\\src"),
            "got:\n{out}"
        );
        assert!(
            out.contains("buildbox no       no      ./src     C:\\src"),
            "got:\n{out}"
        );

        // A lab with no dev machine gets no section at all.
        let plain = render_status(
            &lab(vec![addressed("dc01", PowerState::Running, true, vm())]),
            false,
        );
        assert!(!plain.contains("DEV"), "got:\n{plain}");
    }

    /// The `ATTACH` column is `attachable` off the projection (§19.4), not a
    /// second opinion assembled here: a dev machine whose agent serves the
    /// attach pair reads `yes`, and the one beside it whose agent is stale
    /// reads `no` while still being listed as a perfectly good machine.
    #[test]
    fn the_dev_section_reports_attachable() {
        use crate::status::fixtures::{attachable, dev};
        let out = render_status(
            &lab(vec![
                dev(
                    attachable(addressed("dev01", PowerState::Running, true, vm())),
                    true,
                ),
                dev(addressed("stale", PowerState::Running, true, vm()), false),
            ]),
            false,
        );
        assert!(out.contains("ATTACH"), "got:\n{out}");
        assert!(out.contains("dev01 yes      yes"), "got:\n{out}");
        assert!(out.contains("stale no       no"), "got:\n{out}");
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
                "state=running ready=no cached=yes attachable=no arch=x86_64 cpus=4 \
                 memory=8GiB agent=0.1.0"
            ),
            "got:\n{out}"
        );
        assert!(
            out.contains("state=stopping ready=no cached=yes attachable=no health=- exit=-"),
            "got:\n{out}"
        );
    }

    /// A machine a repair verb changed in place says so wherever its state is
    /// reported (§19.4) — and a machine nothing has touched, which is every
    /// other machine, says nothing rather than carrying `diverged=no` on every
    /// line.
    #[test]
    fn verbose_names_a_diverged_machine_and_only_that_one() {
        let mut diverged = addressed("dev01", PowerState::Running, true, vm());
        diverged.agent_diverged = true;
        let out = render_status(
            &lab(vec![
                diverged,
                addressed("dc01", PowerState::Running, true, vm()),
            ]),
            true,
        );
        let lines: Vec<&str> = out.lines().filter(|l| l.contains("state=")).collect();
        assert!(lines[0].contains("diverged=yes"), "got:\n{out}");
        assert!(!lines[1].contains("diverged"), "got:\n{out}");
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

    /// What `vmlab vm|container start|stop|restart` puts on the wire. Both
    /// nouns route here, so this is the whole mapping — and `--force` is the
    /// difference between a graceful stop and a kill, so it has to reach the
    /// request rather than stopping at the verb.
    #[test]
    fn power_ops_build_their_documented_requests() {
        assert_eq!(
            PowerOp::Start.request("dc01".into(), true),
            // Start has nothing to force.
            LabRequest::MachineStart {
                machine: "dc01".into(),
            }
        );
        assert_eq!(
            PowerOp::Stop.request("dc01".into(), true),
            LabRequest::MachineStop {
                machine: "dc01".into(),
                force: true,
            }
        );
        assert_eq!(
            PowerOp::Restart.request("dc01".into(), false),
            LabRequest::MachineRestart {
                machine: "dc01".into(),
                force: false,
            }
        );
    }

    #[test]
    fn region_value_validates_arity() {
        assert_eq!(region_value(None).unwrap(), None);
        assert_eq!(
            region_value(Some(vec![1, 2, 3, 4]))
                .unwrap()
                .map(Region::as_tuple),
            Some((1, 2, 3, 4))
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

    /// The DNS report renders off the `dns.table` payload. All three kinds of
    /// rule land in one table: a name that will not resolve is answered by
    /// whichever of them matched, so a reader should not have to join tables
    /// to find out which.
    #[test]
    fn the_dns_table_lists_records_wildcards_and_sinkholes_together() {
        let out = render_dns(&json!({
            "segments": [{
                "segment": "lan",
                "zone": {
                    "suffix": "vmlab.internal",
                    "records": [
                        {"name": "dc01.lan.vmlab.internal", "ip": "10.0.0.10", "kind": "dynamic"},
                        {"name": "intranet.corp", "ip": "10.0.0.5", "kind": "static"},
                    ],
                    "wildcards": [{"id": 1, "pattern": "*.corp", "ip": "10.0.0.6"}],
                    "sinkholes": [{"id": 2, "pattern": "*.telemetry.example", "mode": "nxdomain"}],
                },
            }],
        }));
        assert!(
            out.contains("segment \"lan\" — zone vmlab.internal"),
            "got:\n{out}"
        );
        assert!(
            out.contains("dc01.lan.vmlab.internal 10.0.0.10 dynamic"),
            "got:\n{out}"
        );
        assert!(
            out.contains("intranet.corp           10.0.0.5  static"),
            "got:\n{out}"
        );
        assert!(
            out.contains("*.corp                  10.0.0.6  wildcard"),
            "got:\n{out}"
        );
        // A sinkhole answers with nothing, so it has no address — but the mode
        // decides how the guest fails, so it stays on the row.
        assert!(
            out.contains("*.telemetry.example     -         sinkhole/nxdomain"),
            "got:\n{out}"
        );
    }

    /// Segments with no local zone are omitted by the daemon, so a lab whose
    /// segments all delegate reads as a sentence rather than a bare header.
    #[test]
    fn a_lab_serving_no_dns_says_so() {
        assert_eq!(
            render_dns(&json!({"segments": []})),
            "no segment in this lab serves DNS\n"
        );
    }

    /// A segment whose zone is empty is still worth naming: it serves DNS, it
    /// just knows nothing yet.
    #[test]
    fn an_empty_zone_is_named_rather_than_skipped() {
        let out = render_dns(&json!({
            "segments": [{
                "segment": "dmz",
                "zone": {"suffix": "vmlab.internal", "records": [], "wildcards": [], "sinkholes": []},
            }],
        }));
        assert_eq!(
            out,
            "segment \"dmz\" — zone vmlab.internal\n  (no records)\n"
        );
    }

    /// What an interrupted `vmlab pull` cancels. `status` reports one row per
    /// machine waiting on a download, so two machines sharing one template
    /// appear twice — and each machine is cancelled once.
    #[test]
    fn an_interrupted_pull_cancels_each_waiting_machine_once() {
        let mut status = lab(Vec::new());
        status.pulls = downloading(&["dc01", "web", "dc01"]);
        assert_eq!(pulling_machines(&status, &[]), vec!["dc01", "web"]);
        assert!(pulling_machines(&lab(Vec::new()), &[]).is_empty());
    }

    /// `vmlab pull web` cancels `web`'s download and nothing else. The lab may
    /// be downloading for a machine this invocation never named — the console
    /// or a second terminal started it — and walking away from that one is
    /// right where cancelling it is not.
    #[test]
    fn an_interrupted_pull_leaves_downloads_it_did_not_ask_for_alone() {
        let mut status = lab(Vec::new());
        status.pulls = downloading(&["dc01", "web"]);
        assert_eq!(pulling_machines(&status, &["web".to_string()]), vec!["web"]);
        // A machine that is named but is not downloading has nothing to cancel.
        assert!(pulling_machines(&status, &["ghost".to_string()]).is_empty());
    }

    /// One in-flight download per named machine, as `status` reports them.
    fn downloading(machines: &[&str]) -> Vec<PullStatus> {
        machines
            .iter()
            .map(|machine| PullStatus {
                machine: (*machine).into(),
                kind: PullKind::Template,
                reference: format!("ghcr.io/vmlab/{machine}:1"),
                bytes_done: 1,
                bytes_total: 2,
                percent: 50,
            })
            .collect()
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
