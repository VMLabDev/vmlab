//! `vmlab machine repair-agent` — push the host's shipped agent into a
//! running machine, and mark that machine **diverged** (PRD §19.4) — and the
//! refresh `vmlab up` performs with the same push when a machine's agent is
//! out of date.
//!
//! **Rebuild is the clean answer; repair is a tool; `up` refreshes.** The
//! agent enters an image exactly once, at build (§6.1, §7.4), so the only way
//! to get a machine whose agent matches its template's sealed
//! `agent_version` again is a rebuild. Short of that, `up` (and `vm start`)
//! compare the agent's version stamp with the stamp of the asset this host
//! would push and, where they differ, push it — once the agent first answers
//! and before any provision runs, so provisions see the current agent. A
//! machine that has been refreshed is diverged exactly as a repaired one is:
//! the template's sealed `agent_version` no longer describes it, and every
//! surface reporting the machine says so. `agent_update = false` on the `vm`
//! or `lab` block keeps `up` from touching the agent at all, which restores
//! *same template → same machine*. The verb stays for the moment `up` does
//! not cover: a machine already running, or an agent the developer wants
//! pushed now.
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
use super::machine::{AgentOrigin, AgentReplacement, Machine};
use crate::agent_asset::{AgentAsset, AgentOs, ensure_agent_asset};

/// How long the new agent has to answer its handshake after the swap. A
/// service restart is seconds on both guest families; this is the budget for
/// a guest that is busy, not for one that is broken.
const RECONNECT_WAIT: Duration = Duration::from_secs(120);

/// Long enough for the swap to run, short enough that a wedged guest reports
/// a timeout rather than hanging the caller.
const SWAP_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the push waits for sessions already open on the channel — an
/// exec handed the connection in the moment before the gate closed — to end
/// before it restarts the agent out from under them. Generous, because the
/// wait costs nothing anyone is waiting on (a background refresh, or an
/// inline one before any provision) and a user's command cut off is the
/// bug; a terminal or a tail left open is cut off when the bound is
/// reached, as it would have been at once before.
const DRAIN_WAIT: Duration = Duration::from_secs(60);

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

/// Where the macOS bootstrap installs the agent, and the LaunchDaemon label
/// that runs it as root at boot (§7.4).
pub const MACOS_INSTALL: &str = "/Library/vmlab/vmlab-agent";
pub const MACOS_LAUNCHD_LABEL: &str = "dev.vmlab.agent";

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
        GuestOs::Macos => {
            let install = MACOS_INSTALL.to_string();
            let staging = format!("{install}.new");
            RepairPlan {
                // As on Linux: the rename replaces the directory entry, and
                // the running agent keeps the file it was started from.
                swap: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("chmod 0755 '{staging}' && mv -f '{staging}' '{install}'"),
                ],
                // launchd owns the job, so a kickstart asked for from inside
                // it completes even though the requester is what it kills.
                restart: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!(
                        "{{ sleep 1; launchctl kickstart -k system/{MACOS_LAUNCHD_LABEL}; }} \
                         >/dev/null 2>&1 &"
                    ),
                ],
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
                // Launched through WMI, so the restart runs outside the
                // agent's process tree and holds none of the exec's handles. A
                // `start /b` child inherits the exec's stdout: the exec then
                // waits on it, the service stop waits on the exec, and the
                // timeout kills the restart before `sc start` — the old agent
                // keeps running from its renamed image. `[wmiclass]` works from
                // PowerShell 2.0 on, where `Invoke-CimMethod` does not exist
                // and `wmic` is gone from Server 2025. `ping` is the sleep that
                // works without a console, and the service is asked to start
                // twice because a stop that has not finished refuses the first.
                restart: vec![
                    "powershell.exe".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    "$r = ([wmiclass]'Win32_Process').Create('cmd.exe /c \
                     ping -n 3 127.0.0.1 >nul & sc stop vmlab-agent \
                     & ping -n 4 127.0.0.1 >nul & sc start vmlab-agent \
                     & ping -n 3 127.0.0.1 >nul & sc start vmlab-agent'); \
                     exit $r.ReturnValue"
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
    /// Everything it advertised.
    pub features: Vec<String>,
}

/// The agent asset this host would push into `m` — the one both the repair
/// verb and `up`'s refresh push, so the two can never disagree about what
/// "current" means.
pub fn shipped_asset(m: &dyn Machine) -> Result<AgentAsset> {
    let os = match m.guest_os() {
        GuestOs::Windows => AgentOs::Windows,
        GuestOs::Linux => AgentOs::Linux,
        GuestOs::Macos => AgentOs::Macos,
    };
    ensure_agent_asset(os, &m.arch())
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
    let asset = shipped_asset(m.as_ref())?;
    push(m, &asset).await
}

/// The push both callers share — the repair verb and `up`'s refresh: hand
/// `asset` to `m`'s agent over its own channel, stage it beside the installed
/// binary, swap, restart the service and wait for the new agent's handshake.
/// A failure before the swap leaves the old agent installed and running.
pub async fn push(m: &Arc<dyn Machine>, asset: &AgentAsset) -> Result<RepairReport> {
    let replacing = m.begin_agent_replacement().await;
    push_under(m, asset, replacing).await
}

/// [`push`] under a gate the caller already holds — `up`'s refresh takes it
/// the moment the agent is up, before its own handshake and comparison, so
/// an `exec` arriving the instant `up` returns cannot be handed the channel
/// ahead of the push.
///
/// From the gate until the new agent has answered, nobody else gets the
/// channel: an exec handed the connection now would ride it into the restart
/// and die with it, and one that handshook with the agent on its way out
/// would be talking to a process about to be killed. Both wait on the
/// machine, and are released with the guard however the push ends.
pub async fn push_under(
    m: &Arc<dyn Machine>,
    asset: &AgentAsset,
    replacing: AgentReplacement,
) -> Result<RepairReport> {
    let _replacing = replacing;
    let name = m.name().to_string();
    let plan = plan(m.guest_os());

    let agent = m.agent_during_replacement().await.with_context(|| {
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

    // What was opened on this connection before the gate closed finishes
    // first: the restart kills every process the old agent is running, and
    // an exec that reached the channel a moment before the push did was
    // dying with it, as "agent channel closed during exec" or 137.
    drain(&agent, DRAIN_WAIT).await;

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
    let replaced_instance = agent.info().instance;
    agent.shutdown().await;
    m.clear_agent_failure().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let agent = came_back(
        m,
        &asset.version,
        replaced_instance.as_deref(),
        &plan,
        RECONNECT_WAIT,
    )
    .await?;
    if sets_clock_after_push(m.guest_os(), m.clock())
        && agent.has_feature(vmlab_agent_proto::features::SET_TIME)
    {
        // Never fails the push, which has happened: the clock is wrong only
        // until the guest's own time sync or its next cold boot.
        if let Err(e) = agent
            .set_time(std::time::SystemTime::now(), super::lab::CLOCK_SET_WAIT)
            .await
        {
            tracing::warn!(machine = %name, error = %format!("{e:#}"), "guest clock not set after the agent push");
        }
    }
    let info = agent.info();
    Ok(RepairReport {
        machine: name,
        pushed: asset.version.clone(),
        installed_at: plan.install,
        agent_version: info.agent_version,
        features: info.features,
    })
}

/// Whether a push into a machine sets its clock to the host's once the new
/// agent answers (§7.3, #166). A Windows agent makes the guest read the RTC
/// as UTC when it starts, but the kernel took the RTC as local time at this
/// boot, so a guest whose template predates the flag runs off by its UTC
/// offset until it next boots. Setting the time once corrects the running
/// clock — and is right whenever the clock is `host`'s, so it needs no
/// knowledge of whether the agent wrote the flag. A `snapshot` clock may be
/// running from a restore that kept its captured time, and a fixed one is
/// never set by vmlab; a Linux guest never had the problem.
pub fn sets_clock_after_push(guest_os: GuestOs, clock: crate::config::model::Clock) -> bool {
    guest_os == GuestOs::Windows && clock == crate::config::model::Clock::Host
}

/// The agent that replaced the one told to restart, waited for up to `wait`.
///
/// A reconnect answered by the old agent — the old stamp, or the old instance
/// — is a restart that has not happened *yet*, not one that failed. On
/// Windows the service bounce lands seconds after the command that asked for
/// it returned (the restart script sleeps before `sc stop`, and `sc` takes
/// its time), so the first reconnect is routinely the old agent's. Reported
/// as a failed refresh, that let the machine go ready, and the bounce then
/// landed under the first `exec` after `up`, which hung on an agent that was
/// being stopped. So the old agent answering is waited on: pinged until it
/// stops answering or somebody else answers, then handshaken again, until
/// the deadline — only then is a restart that never took effect reported.
///
/// The messages are the ones a stranded developer reads, so they say exactly
/// what is where: the machine is running an agent that is not the pushed
/// one, and what it takes to put the old one back differs by guest family.
pub(super) async fn came_back(
    m: &Arc<dyn Machine>,
    pushed: &str,
    replaced_instance: Option<&str>,
    plan: &RepairPlan,
    wait: Duration,
) -> Result<super::vm_agent::AgentHandle> {
    let name = m.name().to_string();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let agent = m
            .wait_agent_during_replacement(remaining.max(Duration::from_secs(1)))
            .await
            .with_context(|| {
                let recovery = match &plan.replaced {
                    Some(previous) => format!("the binary it replaced is still at {previous}"),
                    None => "the binary it replaced is gone, so a rebuild is what restores it"
                        .to_string(),
                };
                format!(
                    "\"{name}\"'s agent never came back after the repair; the pushed one is \
                     installed at {} and {recovery}",
                    plan.install
                )
            })?;
        m.clear_agent_failure().await;

        let info = agent.info();
        // An agent that answers is not proof the new one does: a restart that
        // has not happened leaves the old agent running from its renamed
        // image, and it answers the reconnect as readily. The pushed binary
        // carries its stamp in its handshake, and names its process, so
        // either of the old ones answering is the old agent.
        let stale = still_running_the_old_agent(pushed, &info.agent_version)
            .map(|v| format!("still {v}"))
            .or_else(|| {
                same_process_answered(replaced_instance, info.instance.as_deref())
                    .then(|| "the same instance as before".to_string())
            });
        let Some(stale) = stale else {
            return Ok(agent);
        };
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "\"{name}\": the pushed agent is installed at {} but the agent answering is \
                 {stale} — the service did not restart onto it within {}s; `vmlab vm restart \
                 {name}` loads it",
                plan.install,
                wait.as_secs()
            );
        }
        // The old agent, still answering: wait for it to go. A ping it
        // does not answer, or that another instance answers, is the restart
        // taking effect, and the next reconnect handshakes with whoever is
        // there then.
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !agent.ping(Duration::from_secs(5)).await {
                break;
            }
        }
    }
}

/// Wait, up to `bound`, for every session open on `agent` to end. Returns how
/// many were still open when it stopped waiting — zero is the usual answer,
/// and anything else is about to be cut off by the restart.
async fn drain(agent: &super::vm_agent::AgentHandle, bound: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + bound;
    loop {
        let open = agent.open_sessions().await;
        if open == 0 || tokio::time::Instant::now() >= deadline {
            if open > 0 {
                tracing::warn!(
                    open,
                    "agent replacement: sessions still open after {}s; restarting the agent \
                     under them",
                    bound.as_secs()
                );
            }
            return open;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether the agent answering the reconnect is the very process that was
/// told to restart: both named an instance and it is the same one. An agent
/// that names none (one from before instances were on the wire) cannot be
/// told apart this way and is judged by its stamp alone.
fn same_process_answered(replaced: Option<&str>, answering: Option<&str>) -> bool {
    matches!((replaced, answering), (Some(a), Some(b)) if a == b)
}

/// The version the agent answering a reconnect reports, when it is not the
/// agent that was pushed. Only a stamped asset can be checked: an asset with
/// no `agent=` stamp carries nothing its handshake can be compared with.
fn still_running_the_old_agent<'a>(pushed: &str, answering: &'a str) -> Option<&'a str> {
    (pushed.starts_with(STAMP_PREFIX) && answering != pushed).then_some(answering)
}

// ---- `up`'s refresh (§19.4) --------------------------------------------------

/// The prefix the Rust agent's build stamp carries (`agent=<rev>`), in the
/// asset's `VERSION` file and — for an agent built by `guest/build-agent.sh`
/// since the stamp was compiled in — in its handshake. An agent from before
/// that answers with its crate version instead, which is not a stamp.
pub const STAMP_PREFIX: &str = "agent=";

/// Why `up` leaves a machine's agent alone without asking it anything. Each is
/// silent: none of them is something the developer has to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotEligible {
    /// `agent_update = false` on the `vm` block, or on the `lab` block with
    /// the `vm` saying nothing.
    OptedOut,
    /// The agent ships with the host (a container's initramfs guest asset), so
    /// it is current by construction.
    HostAsset,
    /// The legacy tier — the C agent or the HolyC one, over ISA serial
    /// (§7.4). Nothing can replace it over its own channel.
    LegacyTier,
    /// The machine's artefact sealed no agent: a scratch VM, a template built
    /// with `agent = false`, or one predating agent support. There is no agent
    /// to refresh, and waiting for one would hold `up` for nothing.
    NoSealedAgent,
}

/// Whether `up` should look at `m`'s agent at all — decided from what the lab
/// file and the machine already say, before anything waits on the guest.
pub fn eligible(m: &dyn Machine, opted_in: bool) -> Result<(), NotEligible> {
    eligible_from(
        opted_in,
        m.agent_origin(),
        m.agent_on_legacy_tier(),
        m.sealed_agent_version().as_deref(),
    )
}

/// [`eligible`] over plain facts, so every rung is a test.
pub fn eligible_from(
    opted_in: bool,
    origin: AgentOrigin,
    legacy_transport: bool,
    sealed: Option<&str>,
) -> Result<(), NotEligible> {
    if !opted_in {
        return Err(NotEligible::OptedOut);
    }
    if origin == AgentOrigin::HostAsset {
        return Err(NotEligible::HostAsset);
    }
    let Some(sealed) = sealed else {
        return Err(NotEligible::NoSealedAgent);
    };
    if legacy_transport || crate::template::agent_install::is_legacy_agent(sealed) {
        return Err(NotEligible::LegacyTier);
    }
    Ok(())
}

/// What `up` does about one eligible machine's agent once it has answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refresh {
    /// The agent's stamp is the shipped asset's.
    Current,
    /// Cannot be judged; said at debug level only.
    Skip(String),
    /// Push the shipped asset. `from` is what the agent is, `to` what it will
    /// be.
    Update { from: String, to: String },
}

/// The stamp the running agent carries.
///
/// Its own handshake where that is a stamp. An agent that predates the stamp
/// being compiled in answers with its crate version, so what it is falls back
/// to the host's record: the asset last pushed into this machine, else the
/// stamp its template sealed.
pub fn installed_stamp<'a>(
    handshake: &'a str,
    recorded: Option<&'a str>,
    sealed: Option<&'a str>,
) -> Option<&'a str> {
    if handshake.starts_with(STAMP_PREFIX) {
        Some(handshake)
    } else {
        recorded.or(sealed)
    }
}

/// The decision, as a value (ADR-0003). **The host is the source of truth**: a
/// stamp that differs in either direction is replaced, because "newer" is not
/// something two git revisions say about themselves.
pub fn decide(
    handshake: &str,
    recorded: Option<&str>,
    sealed: Option<&str>,
    shipped: &str,
) -> Refresh {
    // The fallback `VERSION`-less assets read as — nothing to compare with.
    if shipped == "unknown" || shipped.is_empty() {
        return Refresh::Skip("the shipped agent asset carries no VERSION stamp".into());
    }
    // This exact asset was already pushed into this machine. An agent that
    // still answers with another stamp is one whose asset's `VERSION` does not
    // describe the binary beside it; pushing it again would change nothing
    // and would repeat on every `up`.
    if recorded == Some(shipped) {
        return Refresh::Current;
    }
    match installed_stamp(handshake, recorded, sealed) {
        Some(installed) if installed == shipped => Refresh::Current,
        installed => Refresh::Update {
            from: installed.unwrap_or(handshake).to_string(),
            to: shipped.to_string(),
        },
    }
}

/// How a refresh ends that failed before its push began — while it waited
/// for the agent to answer (§19.4), decided from the machine's power state
/// once the wait gave up (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreached {
    /// The machine is stopping or stopped: somebody stopped it, so nothing
    /// was pushed and nothing went wrong. A quiet event, no `warning:`.
    Stopped,
    /// The machine is still up and its agent did not answer: a real failure.
    Failed,
}

/// Classify a refresh that never reached its push. Starting counts as up:
/// a wait does not give up on a machine still on its way.
pub fn unreached(state: crate::status::PowerState) -> Unreached {
    use crate::status::PowerState;
    match state {
        PowerState::Stopping | PowerState::Stopped => Unreached::Stopped,
        PowerState::Starting | PowerState::Running | PowerState::Suspended => Unreached::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refresh whose machine stopped under it was stopped, not failed;
    /// one whose machine is still up and never answered failed.
    #[test]
    fn a_refresh_cut_short_by_a_stop_is_not_a_failure() {
        use crate::status::PowerState;
        assert_eq!(unreached(PowerState::Stopping), Unreached::Stopped);
        assert_eq!(unreached(PowerState::Stopped), Unreached::Stopped);
        assert_eq!(unreached(PowerState::Running), Unreached::Failed);
        assert_eq!(unreached(PowerState::Suspended), Unreached::Failed);
        assert_eq!(unreached(PowerState::Starting), Unreached::Failed);
    }

    /// A push sets a Windows guest's clock once the new agent answers — the
    /// boot it is running read the RTC as local time (#166) — when the clock
    /// is the host's; a `snapshot` or fixed clock and a Linux guest are left
    /// alone.
    #[test]
    fn a_push_sets_the_clock_of_a_windows_guest_following_the_host() {
        use crate::config::model::{Clock, parse_clock};
        assert!(sets_clock_after_push(GuestOs::Windows, Clock::Host));
        assert!(!sets_clock_after_push(GuestOs::Windows, Clock::Snapshot));
        let fixed = parse_clock("2020-01-01T00:00:00Z").unwrap();
        assert!(!sets_clock_after_push(GuestOs::Windows, fixed));
        assert!(!sets_clock_after_push(GuestOs::Linux, Clock::Host));
    }

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
    /// A reconnect answered by anything but the pushed stamp is the old agent
    /// still running; an unstamped asset has nothing to compare.
    #[test]
    fn a_reconnect_by_the_old_agent_is_not_a_repair() {
        assert_eq!(still_running_the_old_agent("agent=new", "agent=new"), None);
        assert_eq!(
            still_running_the_old_agent("agent=new", "agent=old"),
            Some("agent=old")
        );
        assert_eq!(
            still_running_the_old_agent("agent=new", "0.1.0"),
            Some("0.1.0")
        );
        assert_eq!(still_running_the_old_agent("unknown", "agent=old"), None);
    }

    /// The stamp says which binary answered; the instance says which process.
    /// A reconnect answered under the instance the restarted agent had is the
    /// same process, whatever its stamp; an agent naming no instance is
    /// judged by its stamp alone, which is all it offers.
    #[test]
    fn a_reconnect_by_the_same_instance_is_not_a_restart() {
        assert!(same_process_answered(Some("a81e"), Some("a81e")));
        assert!(!same_process_answered(Some("a81e"), Some("77fc")));
        assert!(!same_process_answered(None, Some("77fc")));
        assert!(!same_process_answered(Some("a81e"), None));
        assert!(!same_process_answered(None, None));
    }

    #[test]
    fn the_restart_is_detached_and_separate_from_the_swap() {
        let linux = plan(GuestOs::Linux);
        assert!(linux.restart.last().unwrap().ends_with('&'));
        assert!(linux.restart.last().unwrap().contains("systemctl restart"));
        assert!(linux.restart.last().unwrap().contains("rc-service"));
        assert!(!linux.swap.last().unwrap().contains("systemctl"));

        let windows = plan(GuestOs::Windows);
        assert_eq!(windows.restart[0], "powershell.exe");
        assert!(
            windows
                .restart
                .last()
                .unwrap()
                .contains("[wmiclass]'Win32_Process'")
        );
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

    // ---- `up`'s refresh --------------------------------------------------------

    /// A stamp equal to the shipped asset's is left alone; a different one —
    /// in either direction — is replaced, the host being the source of truth.
    #[test]
    fn a_matching_stamp_is_current_and_a_different_one_is_updated() {
        assert_eq!(
            decide(
                "agent=2284722",
                None,
                Some("agent=57fa802"),
                "agent=2284722"
            ),
            Refresh::Current
        );
        assert_eq!(
            decide(
                "agent=57fa802",
                None,
                Some("agent=57fa802"),
                "agent=2284722"
            ),
            Refresh::Update {
                from: "agent=57fa802".into(),
                to: "agent=2284722".into()
            }
        );
        // "Older" on the host is still the host's answer.
        assert_eq!(
            decide("agent=2284722", None, None, "agent=57fa802"),
            Refresh::Update {
                from: "agent=2284722".into(),
                to: "agent=57fa802".into()
            }
        );
    }

    /// An agent from before the stamp was compiled in answers with its crate
    /// version, so what it is comes from the host's record: the last push,
    /// else the template's seal.
    #[test]
    fn an_agent_that_reports_no_stamp_is_judged_by_the_hosts_record() {
        assert_eq!(
            decide("0.1.0", None, Some("agent=abc"), "agent=abc"),
            Refresh::Current
        );
        assert_eq!(
            decide("0.1.0", None, Some("agent=abc"), "agent=def"),
            Refresh::Update {
                from: "agent=abc".into(),
                to: "agent=def".into()
            }
        );
        assert_eq!(
            decide("0.1.0", Some("agent=def"), Some("agent=abc"), "agent=def"),
            Refresh::Current
        );
    }

    /// The asset already pushed into this machine is not pushed again, even
    /// where the agent still answers with some other stamp — that would repeat
    /// on every `up` and change nothing.
    #[test]
    fn the_asset_already_pushed_is_not_pushed_again() {
        assert_eq!(
            decide(
                "agent=old",
                Some("agent=new"),
                Some("agent=old"),
                "agent=new"
            ),
            Refresh::Current
        );
    }

    /// An asset with no `VERSION` file reads as "unknown", which no stamp can
    /// be compared with.
    #[test]
    fn an_unstamped_asset_is_skipped() {
        assert!(matches!(
            decide("agent=abc", None, None, "unknown"),
            Refresh::Skip(_)
        ));
    }

    /// Opt-out, a host-shipped agent, the legacy tier and a machine sealing no
    /// agent each keep `up` away from the guest entirely.
    #[test]
    fn who_up_never_asks() {
        let image = AgentOrigin::Image;
        assert_eq!(eligible_from(true, image, false, Some("agent=a")), Ok(()));
        assert_eq!(
            eligible_from(false, image, false, Some("agent=a")),
            Err(NotEligible::OptedOut)
        );
        assert_eq!(
            eligible_from(true, AgentOrigin::HostAsset, false, Some("agent=a")),
            Err(NotEligible::HostAsset)
        );
        assert_eq!(
            eligible_from(true, image, true, Some("agent=a")),
            Err(NotEligible::LegacyTier)
        );
        assert_eq!(
            eligible_from(true, image, false, Some("agent-legacy=a")),
            Err(NotEligible::LegacyTier)
        );
        assert_eq!(
            eligible_from(true, image, false, Some("agent-templeos=a")),
            Err(NotEligible::LegacyTier)
        );
        assert_eq!(
            eligible_from(true, image, false, None),
            Err(NotEligible::NoSealedAgent)
        );
    }
}
