//! `vmlab machine repair-agent` — push the host's shipped agent into a
//! running machine, and mark that machine **diverged** (PRD §19.4).
//!
//! **Rebuild is policy; repair is a tool.** The agent enters an image exactly
//! once, at build (§6.1, §7.4), so a stale agent is a rebuild — and this verb
//! never fires by itself, because an automatic refresh at `up` would make the
//! template's sealed `agent_version` a lie and stop *same template → same
//! machine* holding. It exists because a 15–45 minute Windows rebuild to pick
//! up an agent change is otherwise the inner loop of building §19 itself.
//!
//! **What it can and cannot recover.** The binary rides the agent's own
//! channel, so the agent already there has to be able to receive it: an agent
//! with no `fileops` cannot be handed a file at all, and rebuilding is then
//! the only remedy. That boundary is named at the call, loudly, rather than
//! discovered as a confusing transfer failure — the one execution path that
//! would not need the agent is screen keystrokes, which vmlab does not use to
//! install software.
//!
//! **The swap is separated from the restart** (ADR-0003: the decision is a
//! value, computed before anything acts on it). Putting the staged binary in
//! place is observable — it runs in the foreground and its exit code says
//! whether it worked — while restarting the service kills the very channel the
//! command was issued over, so nothing can observe *that*. Keeping them apart
//! is what makes a failed repair report a failure instead of a silence.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::guest_os::GuestOs;
use super::machine::{AgentOrigin, Machine};
use crate::agent_asset::{AgentOs, ensure_agent_asset};

/// How long the new agent has to answer its handshake after the swap. A
/// service restart is seconds on both guest families; this is the budget for
/// a guest that is busy, not for one that is broken.
const RECONNECT_WAIT: Duration = Duration::from_secs(120);

/// Long enough for the swap to run, short enough that a wedged guest reports
/// a timeout rather than hanging the caller.
const SWAP_TIMEOUT: Duration = Duration::from_secs(60);

/// How a repair replaces the binary inside one guest family — computed before
/// anything runs, so what a repair will do is a value a test can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairPlan {
    /// Where the pushed binary lands. Never the installed path directly: a
    /// half-written file at the path the service runs from would break the
    /// agent that a failed repair otherwise leaves working.
    pub staging: String,
    /// The path the guest's service starts the agent from.
    pub install: String,
    /// **Foreground.** Put the staged binary in place, and report whether it
    /// worked. Nothing here kills the channel, which is why the caller can
    /// still hear the answer.
    pub swap: Vec<String>,
    /// **Detached.** Restart the service, which ends the agent this was sent
    /// over. Backgrounded inside the guest so the command returns before its
    /// own channel dies.
    pub restart: Vec<String>,
    /// Where the binary that was replaced ends up, on a guest family that
    /// keeps it. `None` where the swap consumed it — and the distinction is
    /// load-bearing, because it is what a repair that never came back tells a
    /// developer to go and look at.
    pub replaced: Option<String>,
}

/// The plan for `guest_os`.
///
/// Both paths are the ones the bootstrap installer wrote at build time
/// (`src/template/bootstrap/install.sh` and `install.cmd`) — the repair verb
/// replaces what that installed, so it must agree with it rather than invent
/// a second location.
pub fn plan(guest_os: GuestOs) -> RepairPlan {
    match guest_os {
        GuestOs::Linux => {
            let install = "/usr/local/lib/vmlab/vmlab-agent".to_string();
            let staging = format!("{install}.new");
            RepairPlan {
                // `mv` over a *running* binary is fine on Linux: the rename
                // replaces the directory entry and the running process keeps
                // the inode it already mapped.
                swap: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("chmod 0755 '{staging}' && mv -f '{staging}' '{install}'"),
                ],
                // Whichever init the image actually runs — the same two the
                // bootstrap installer registers the service with, in the same
                // order. `systemctl restart` issued from inside the service is
                // safe: systemd owns the job, so it completes even though the
                // requester is what it stops.
                restart: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "{ sleep 1; if [ -d /run/systemd/system ]; then \
                       systemctl restart vmlab-agent; \
                     elif command -v rc-service >/dev/null 2>&1; then \
                       rc-service vmlab-agent restart; \
                     else \
                       kill \"$(cat /run/vmlab-agent.pid 2>/dev/null)\" 2>/dev/null; \
                     fi; } >/dev/null 2>&1 &"
                        .into(),
                ],
                // `mv -f` consumes it: the replaced binary survives only as
                // the inode the running process still has open, which is
                // nothing a developer can go and look at.
                replaced: None,
                staging,
                install,
            }
        }
        GuestOs::Windows => {
            let install = r"C:\ProgramData\vmlab\vmlab-agent.exe".to_string();
            let staging = r"C:\ProgramData\vmlab\vmlab-agent.new.exe".to_string();
            let previous = r"C:\ProgramData\vmlab\vmlab-agent.old.exe".to_string();
            RepairPlan {
                // Windows refuses to *overwrite* a running image but allows it
                // to be *renamed*, so the live agent is moved aside and the
                // staged binary takes its name — with the service still up,
                // which is what keeps this half observable.
                swap: vec![
                    "cmd.exe".into(),
                    "/c".into(),
                    format!("move /y {install} {previous} && move /y {staging} {install}"),
                ],
                // Detached through `start /b`, so the agent's own exec is not
                // the process being stopped. `ping` is the sleep that works
                // without a console, and the service is asked to start twice
                // because a stop that has not finished refuses the first.
                restart: vec![
                    "cmd.exe".into(),
                    "/c".into(),
                    "start \"\" /b cmd.exe /c \"ping -n 3 127.0.0.1 >nul \
                     & sc stop vmlab-agent & ping -n 4 127.0.0.1 >nul \
                     & sc start vmlab-agent & ping -n 3 127.0.0.1 >nul \
                     & sc start vmlab-agent\""
                        .into(),
                ],
                // The rename leaves it on disk under its own name, which is
                // what a developer restores by hand if the new one never
                // starts.
                replaced: Some(previous),
                staging,
                install,
            }
        }
    }
}

/// Why pushing the shipped agent into a machine of this origin would be
/// meaningless — `None` where the verb is the remedy it exists to be.
///
/// **Reported rather than implied.** Telling a container author their agent is
/// stale, or silently doing nothing, would both be lies: the answer is that
/// this machine's agent is the host's and there is nothing here to repair.
pub fn meaningless_for(origin: AgentOrigin) -> Option<String> {
    match origin {
        AgentOrigin::Image => None,
        AgentOrigin::HostAsset => Some(
            "this machine's agent lives in the initramfs guest asset this host installed, \
             not in anything it boots — it already tracks the vmlab you are running and \
             cannot go stale, so there is nothing to push into it. Refreshing it means \
             reinstalling the guest asset (§19.4)"
                .to_string(),
        ),
    }
}

/// What a repair did, for the surface that asked for it.
#[derive(Debug, Clone, Serialize)]
pub struct RepairReport {
    pub machine: String,
    /// The version stamp of the agent asset this host shipped and pushed.
    pub pushed: String,
    /// Where it landed in the guest.
    pub installed_at: String,
    /// What the agent said about itself once it came back — the honest
    /// after-state, read from a fresh handshake rather than assumed from what
    /// was pushed.
    pub agent_version: String,
    /// Everything it advertised, which is the evidence `attachable` is read
    /// from rather than a second opinion about it.
    pub features: Vec<String>,
    pub attachable: bool,
}

/// Push the host's shipped agent into `m` and wait for it to come back.
///
/// Marking the machine diverged is the *caller's* half, because the record
/// lives in the lab's persisted state and this function holds a machine, not a
/// lab.
pub async fn repair(m: &Arc<dyn Machine>) -> Result<RepairReport> {
    let name = m.name().to_string();
    if let Some(why) = meaningless_for(m.agent_origin()) {
        bail!("\"{name}\": {why}");
    }

    let os = match m.guest_os() {
        GuestOs::Windows => AgentOs::Windows,
        GuestOs::Linux => AgentOs::Linux,
    };
    let asset = ensure_agent_asset(os, &m.arch())?;
    let plan = plan(m.guest_os());

    let agent = m.agent().await.with_context(|| {
        format!(
            "\"{name}\" must be running with its agent answering before a new one can be \
             pushed into it over that channel"
        )
    })?;
    // The boundary between the tool and the policy: a binary rides the agent's
    // own file vocabulary, so an agent that does not serve one cannot be
    // replaced this way at all.
    if !agent.has_feature(vmlab_agent_proto::features::FILEOPS) {
        bail!(
            "\"{name}\"'s agent serves no `fileops`, so it cannot be handed a binary over its \
             own channel — this one can only be replaced by rebuilding the template (§19.4)"
        );
    }

    agent
        .push_file(&asset.path, &plan.staging, Some(0o755))
        .await
        .with_context(|| format!("pushing the shipped agent to {}", plan.staging))?;

    let swapped = agent
        .exec(plan.swap.clone(), vec![], None, None, SWAP_TIMEOUT, None)
        .await
        .context("putting the pushed agent in place")?;
    if swapped.exit_code != 0 {
        bail!(
            "\"{name}\": putting the pushed agent in place failed ({}): {}",
            swapped.exit_code,
            String::from_utf8_lossy(&swapped.stderr).trim(),
        );
    }

    // From here the channel is expected to die: the restart takes the agent
    // that is carrying this command with it. A command that returns is what we
    // asked for, and one that does not is the restart arriving early — neither
    // is a failure, so what happened next is read from the handshake rather
    // than from this exit code.
    let _ = agent
        .exec(
            plan.restart.clone(),
            vec![],
            None,
            None,
            Duration::from_secs(15),
            None,
        )
        .await;

    // Let go of the connection so the guest's one chardev slot is free for the
    // agent that is coming back, and forget the handshake failures the gap
    // produces on the way.
    agent.shutdown().await;
    m.clear_agent_failure().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    // The one message a stranded developer reads, so it says exactly what is
    // where: the machine is running an agent that is not answering, and what
    // it takes to put the old one back differs by guest family.
    let agent = m.wait_agent(RECONNECT_WAIT).await.with_context(|| {
        let recovery = match &plan.replaced {
            Some(previous) => format!("the binary it replaced is still at {previous}"),
            None => "the binary it replaced is gone, so a rebuild is what restores it".to_string(),
        };
        format!(
            "\"{name}\"'s agent never came back after the repair; the pushed one is installed \
             at {} and {recovery}",
            plan.install
        )
    })?;
    m.clear_agent_failure().await;

    let info = agent.info();
    Ok(RepairReport {
        machine: name,
        pushed: asset.version,
        installed_at: plan.install,
        agent_version: info.agent_version,
        attachable: crate::attach::attachable(&info.features),
        features: info.features,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The staged binary never lands on the path the service runs from: a
    /// half-written file there would break the agent a failed repair would
    /// otherwise have left working.
    #[test]
    fn the_pushed_binary_lands_beside_the_installed_one_never_on_it() {
        for os in [GuestOs::Linux, GuestOs::Windows] {
            let plan = plan(os);
            assert_ne!(plan.staging, plan.install, "{os:?}");
            assert!(plan.swap.last().unwrap().contains(&plan.staging), "{os:?}");
            assert!(plan.swap.last().unwrap().contains(&plan.install), "{os:?}");
        }
    }

    /// The paths are the ones the bootstrap installer wrote at build time —
    /// the repair verb replaces what that installed, and a second location
    /// would leave the service running the old binary for ever.
    #[test]
    fn the_installed_path_is_the_one_the_bootstrap_installer_used() {
        assert!(
            include_str!("../template/bootstrap/install.sh")
                .contains(&plan(GuestOs::Linux).install)
        );
        assert!(
            include_str!("../template/bootstrap/install.cmd")
                .contains(&plan(GuestOs::Windows).install)
        );
    }

    /// Windows refuses to overwrite a running image but allows it to be
    /// renamed, so the live agent is moved aside first — overwriting in place
    /// is the swap that cannot work.
    #[test]
    fn windows_renames_the_running_image_aside() {
        let plan = plan(GuestOs::Windows);
        let swap = plan.swap.last().unwrap().clone();
        let aside = swap.find(".old.exe").expect("the running image is renamed");
        let staged = swap
            .find(".new.exe")
            .expect("the staged binary is moved in");
        assert!(aside < staged, "the rename must come first: {swap}");
        // …and where it went is carried, because that is what a repair which
        // never came back tells a developer to restore from.
        let replaced = plan.replaced.expect("windows keeps the replaced binary");
        assert!(swap.contains(&replaced), "{swap}");
    }

    /// Linux keeps nothing to restore from: `mv -f` consumes the old binary,
    /// and carrying a path anyway would send a stranded developer to a file
    /// that is not there.
    #[test]
    fn linux_does_not_claim_to_have_kept_the_replaced_binary() {
        assert_eq!(plan(GuestOs::Linux).replaced, None);
    }

    /// The half that kills the channel is separated from the half that can be
    /// observed, and it is detached inside the guest — so a repair reports
    /// what happened instead of dying with the service it restarted.
    #[test]
    fn the_restart_is_detached_and_separate_from_the_swap() {
        let linux = plan(GuestOs::Linux);
        assert!(linux.restart.last().unwrap().ends_with('&'));
        assert!(linux.restart.last().unwrap().contains("systemctl restart"));
        assert!(linux.restart.last().unwrap().contains("rc-service"));
        assert!(!linux.swap.last().unwrap().contains("systemctl"));

        let windows = plan(GuestOs::Windows);
        assert!(windows.restart.last().unwrap().starts_with("start \"\" /b"));
        assert!(windows.restart.last().unwrap().contains("sc start"));
        assert!(!windows.swap.last().unwrap().contains("sc stop"));
    }

    /// A container is told the truth — its agent is the host's and cannot go
    /// stale — rather than being told to rebuild something, or being quietly
    /// handed a no-op that implies it worked.
    #[test]
    fn a_machine_whose_agent_ships_with_the_host_is_told_why_there_is_nothing_to_do() {
        let why = meaningless_for(AgentOrigin::HostAsset).expect("a refusal");
        assert!(why.contains("guest asset"), "{why}");
        assert!(why.contains("cannot go stale"), "{why}");
        assert!(!why.contains("rebuild the template"), "{why}");
        assert_eq!(meaningless_for(AgentOrigin::Image), None);
    }
}
