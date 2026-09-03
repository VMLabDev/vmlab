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
/// (`agent-legacy=<rev>`, `agent-templeos=<rev>`). The Rust agent reports its
/// crate version — "0.1.0" — and never carried an `agent=` prefix, so a test
/// for one called every Rust agent legacy. Nothing noticed until a guest old
/// enough to need this tier finally answered on Windows: a Windows 7 build
/// sealed a full agent as `windows-nt`, exec-only (2026-09-03).
fn is_legacy_agent(agent_version: &str) -> bool {
    agent_version.starts_with("agent-legacy=") || agent_version.starts_with("agent-templeos=")
}

/// Which staged flavour answered, from the handshake's OS and that judgement.
fn agent_flavour(os: &str, legacy: bool) -> AgentOs {
    match (os, legacy) {
        ("windows", false) => AgentOs::Windows,
        // The NT and 9x builds both say "windows"; their stamps are one
        // build's, so either staged flavour names it.
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
/// Windows.
pub async fn verify(
    machine: &Arc<dyn Machine>,
    wants_agent: bool,
    staged: Option<&StagedGuestIso>,
    wait: Duration,
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
    let deadline = tokio::time::Instant::now() + wait;
    let handle = loop {
        machine.clear_agent_failure().await;
        match machine.agent().await {
            Ok(handle) => break handle,
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(e).context(
                        "the guest-installed vmlab-agent never answered its handshake \
                         (did the template's unattended install run the VMLAB ISO's \
                         install script?)",
                    );
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    let info = handle.info();
    let legacy = is_legacy_agent(&info.agent_version);
    let os = agent_flavour(&info.os, legacy);
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
            agent_flavour("windows", is_legacy_agent("0.1.0")),
            AgentOs::Windows
        );
        assert_eq!(
            agent_flavour("linux", is_legacy_agent("0.1.0")),
            AgentOs::Linux
        );
    }

    /// Each legacy agent names itself in its stamp, which is the whole signal.
    #[test]
    fn the_legacy_agents_name_themselves() {
        assert!(is_legacy_agent("agent-legacy=1a2b3c4"));
        assert!(is_legacy_agent("agent-templeos=1a2b3c4"));
        assert_eq!(
            agent_flavour("windows", is_legacy_agent("agent-legacy=1a2b3c4")),
            AgentOs::WindowsNt
        );
        assert_eq!(
            agent_flavour("dos", is_legacy_agent("agent-legacy=1a2b3c4")),
            AgentOs::Dos
        );
        assert_eq!(
            agent_flavour("templeos", is_legacy_agent("agent-templeos=1a2b3c4")),
            AgentOs::TempleOs
        );
    }
}
