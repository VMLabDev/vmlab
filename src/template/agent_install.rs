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
use crate::labd::vm::VmInstance;

/// Wait for the agent handshake; returns the staged asset's version stamp
/// (by the handshake's OS flavour), or `None` when verification was skipped
/// (reason already logged). `wait` is how long to keep probing: a
/// layered/qcow2 source starts the agent within its first boot, but a fresh
/// install from ISO only gets one once the unattended installer has laid
/// down the OS and run its first-logon hooks — routinely 15–45 minutes for
/// Windows.
pub async fn verify(
    vm: &Arc<VmInstance>,
    wants_agent: bool,
    staged: Option<&StagedGuestIso>,
    wait: Duration,
    log: &(dyn Fn(String) + Sync),
) -> Result<Option<String>> {
    if !wants_agent {
        log("agent: skipped (template sets agent = false)\n".into());
        return Ok(None);
    }
    if !vm.template().resolved.agent_channel {
        log("agent: skipped (guest profile has no agent channel)\n".into());
        return Ok(None);
    }
    let staged = staged.context("agent verification without a staged bootstrap ISO")?;

    // Probe until the freshly installed service answers. Earlier agent-first
    // execs may have failed a handshake against the not-yet-installed agent,
    // which `vm.agent()` remembers for 30 s — clear that memory before each
    // attempt. Never drop_agent here: a concurrent provision may be using
    // the cached handle, and teardown at seal time cleans it up anyway.
    let deadline = tokio::time::Instant::now() + wait;
    let handle = loop {
        vm.clear_agent_failure().await;
        match vm.agent().await {
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
    let os = if info.os == "windows" {
        AgentOs::Windows
    } else {
        AgentOs::Linux
    };
    // The staged stamp identifies what the ISO carried; the handshake's own
    // version is the fallback if the flavour somehow wasn't staged.
    let version = staged
        .version_for(os)
        .map(str::to_string)
        .unwrap_or(info.agent_version);
    log(format!(
        "agent: verified ({} {}, agent answering)\n",
        os.key(),
        version
    ));
    Ok(Some(version))
}
