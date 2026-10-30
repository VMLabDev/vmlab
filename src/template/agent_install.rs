//! Verify the vmlab-agent baked into a template build. The install itself
//! is guest-driven: the build attaches the VMLAB bootstrap ISO
//! ([`super::bootstrap`]) and the template's unattended-install hook
//! (cloud-init runcmd / subiquity late-commands / autounattend
//! FirstLogonCommands) runs its install script. This side only proves the
//! channel end-to-end — wait for the agent's handshake on `vmlab.agent.0` —
//! and returns the staged asset's version stamp for the sealed metadata.
//!
//! Skips are non-fatal (logged loudly): templates opting out (`agent =
//! false`) and vintage guests without an agent channel. The sealed metadata
//! records `agent_version` only on a verified handshake, so degradation
//! messaging downstream stays truthful.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use super::bootstrap::StagedGuestIso;
use crate::agent_asset::AgentOs;
use crate::labd::machine::Machine;

/// Whether the answering agent is a legacy-tier one.
///
/// Judged by what those agents assert, not by what the Rust one was assumed
/// to. The C and HolyC agents compile their build stamp in as their version
/// (`agent-legacy=<rev>`, `agent-templeos=<rev>`). The Rust agent reports
/// `agent=<rev>` when `guest/build-agent.sh` built it and its crate version —
/// "0.1.0" — otherwise; an `agent=` test once called every Rust agent legacy. Nothing noticed until a guest old
/// enough to need this tier finally answered on Windows: a Windows 7 build
/// sealed a full agent as `windows-nt`, exec-only (2026-09-03).
pub(crate) fn is_legacy_agent(agent_version: &str) -> bool {
    agent_version.starts_with("agent-legacy=") || agent_version.starts_with("agent-templeos=")
}

/// Which staged flavour answered, from the handshake's OS and that judgement.
fn agent_flavour(os: &str, legacy: bool, profile: Option<&str>) -> AgentOs {
    match (os, legacy) {
        ("windows", false) => AgentOs::Windows,
        ("macos", _) => AgentOs::Macos,
        // The NT and 9x builds both say "windows"; the machine's profile
        // tells them apart.
        ("windows", true) if profile == Some("windows-9x") => AgentOs::Windows9x,
        ("windows", true) => AgentOs::WindowsNt,
        ("dos", _) => AgentOs::Dos,
        ("templeos", _) => AgentOs::TempleOs,
        _ => AgentOs::Linux,
    }
}

/// Wait for the agent handshake; returns the staged asset's version stamp
/// (by the handshake's OS flavour), or `None` when verification was skipped
/// (reason already logged). `wait` is how long to keep probing: a
/// layered/qcow2 source starts the agent within its first boot, but a fresh
/// install from ISO only gets one once the unattended installer has laid
/// down the OS and run its first-logon hooks — routinely 15–45 minutes for
/// Windows. `None` probes until the caller drops the future: the wait an
/// installer-driven build runs beside its provisions, which bound it.
pub async fn verify(
    machine: &Arc<dyn Machine>,
    wants_agent: bool,
    staged: Option<&StagedGuestIso>,
    wait: Option<Duration>,
    log: &(dyn Fn(String) + Sync),
) -> Result<Option<String>> {
    if !wants_agent {
        log("agent: skipped (template sets agent = false)\n".into());
        return Ok(None);
    }
    if !machine.has_agent_channel() {
        log("agent: skipped (guest profile has no agent channel)\n".into());
        return Ok(None);
    }
    let staged = staged.context("agent verification without a staged bootstrap ISO")?;

    // Probe until the freshly installed service answers. Earlier agent-first
    // execs may have failed a handshake against the not-yet-installed agent,
    // which `machine.agent()` remembers for 30 s — clear that memory before each
    // attempt. Never drop_agent here: a concurrent provision may be using
    // the cached handle, and teardown at seal time cleans it up anyway.
    let handle = probe_until_answered(
        || async {
            machine.clear_agent_failure().await;
            machine.agent().await
        },
        wait,
    )
    .await
    .context(
        "the guest-installed vmlab-agent never answered its handshake \
         (did the template's unattended install run the VMLAB ISO's \
         install script?)",
    )?;

    let info = handle.info();
    let legacy = is_legacy_agent(&info.agent_version);
    let os = agent_flavour(&info.os, legacy, machine.profile().as_deref());
    // The staged stamp identifies what the ISO carried; the handshake's own
    // version is the fallback if the flavour somehow wasn't staged — and is
    // the answer for the legacy agent, whose stamp *is* its version string.
    let version = if legacy {
        info.agent_version
    } else {
        staged
            .version_for(os)
            .map(str::to_string)
            .unwrap_or(info.agent_version)
    };
    log(format!(
        "agent: verified ({} {}, agent answering)\n",
        os.key(),
        version
    ));
    Ok(Some(version))
}

/// Try `attempt` every five seconds until it succeeds or `wait` has passed
/// (the last error); `None` never gives up.
async fn probe_until_answered<T, F, Fut>(mut attempt: F, wait: Option<Duration>) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let deadline = wait.map(|wait| tokio::time::Instant::now() + wait);
    loop {
        match attempt().await {
            Ok(answered) => return Ok(answered),
            Err(e) if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_secs(5)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Rust agent answers with its crate version, and must not be mistaken
    /// for the exec-only tier — the mistake that sealed a Windows 7 template
    /// as `windows-nt` with a full agent inside it.
    #[test]
    fn the_rust_agent_is_not_a_legacy_agent() {
        assert!(!is_legacy_agent("0.1.0"));
        assert_eq!(
            agent_flavour("windows", is_legacy_agent("0.1.0"), None),
            AgentOs::Windows
        );
        assert_eq!(
            agent_flavour("linux", is_legacy_agent("0.1.0"), None),
            AgentOs::Linux
        );
        assert_eq!(
            agent_flavour("macos", is_legacy_agent("agent=1a2b3c4"), Some("macos")),
            AgentOs::Macos
        );
    }

    /// Each legacy agent names itself in its stamp, which is the whole signal.
    #[test]
    fn the_legacy_agents_name_themselves() {
        assert!(is_legacy_agent("agent-legacy=1a2b3c4"));
        assert!(is_legacy_agent("agent-templeos=1a2b3c4"));
        assert_eq!(
            agent_flavour("windows", is_legacy_agent("agent-legacy=1a2b3c4"), None),
            AgentOs::WindowsNt
        );
        assert_eq!(
            agent_flavour(
                "windows",
                is_legacy_agent("agent-legacy=1a2b3c4"),
                Some("windows-9x")
            ),
            AgentOs::Windows9x
        );
        assert_eq!(
            agent_flavour("dos", is_legacy_agent("agent-legacy=1a2b3c4"), None),
            AgentOs::Dos
        );
        assert_eq!(
            agent_flavour("templeos", is_legacy_agent("agent-templeos=1a2b3c4"), None),
            AgentOs::TempleOs
        );
    }

    /// An agent that answers after `after` of virtual time, counting attempts.
    fn answering_after(
        after: Duration,
        attempts: &std::sync::atomic::AtomicU32,
    ) -> impl FnMut() -> std::future::Ready<Result<&'static str>> + '_ {
        let start = tokio::time::Instant::now();
        move || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(if start.elapsed() >= after {
                Ok("answered")
            } else {
                Err(anyhow::anyhow!("no handshake yet"))
            })
        }
    }

    /// The wait an installer-driven build runs beside its provisions has no
    /// deadline of its own: an install that outlasts the hour the wait used
    /// to give up after is still heard when its agent answers (V5).
    #[tokio::test(start_paused = true)]
    async fn an_unbounded_probe_outlasts_a_long_install() {
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let answered = probe_until_answered(
            answering_after(Duration::from_secs(2 * 3600), &attempts),
            None,
        )
        .await
        .unwrap();
        assert_eq!(answered, "answered");
        assert!(attempts.into_inner() > 1000);
    }

    /// A bounded probe gives up at its deadline with the last error, and
    /// answers at once when the agent already does.
    #[tokio::test(start_paused = true)]
    async fn a_bounded_probe_gives_up_at_its_deadline() {
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let started = tokio::time::Instant::now();
        let err = probe_until_answered(
            answering_after(Duration::from_secs(3600), &attempts),
            Some(Duration::from_secs(60)),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no handshake yet"), "{err}");
        assert!(started.elapsed() >= Duration::from_secs(60));
        assert!(started.elapsed() < Duration::from_secs(70));

        let attempts = std::sync::atomic::AtomicU32::new(0);
        let answered = probe_until_answered(
            answering_after(Duration::ZERO, &attempts),
            Some(Duration::ZERO),
        )
        .await
        .unwrap();
        assert_eq!(answered, "answered");
        assert_eq!(attempts.into_inner(), 1);
    }
}
