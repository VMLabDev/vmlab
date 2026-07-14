//! Bake the vmlab-agent service into a template image during the build —
//! after the build VM boots, before any provision script runs (so the
//! install survives a final sysprep/generalize provision that shuts the
//! guest down). Runs over QGA: upload the per-OS/arch binary
//! ([`crate::agent_asset`]), register it as a service (systemd unit /
//! Windows SCM), start it, and verify the agent actually answers on the
//! `vmlab.agent.0` channel before letting provisions proceed.
//!
//! Skips are non-fatal (logged loudly): templates opting out (`agent =
//! false`), vintage guests without an agent channel, non-systemd Linux, or
//! a missing agent binary for the target (riscv64/windows are best-effort
//! builds). The sealed metadata records `agent_version` only on a verified
//! install, so degradation messaging downstream stays truthful.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::agent_asset::{AgentAsset, AgentOs, ensure_agent_asset};
use crate::labd::vm::VmInstance;

/// Where the agent lands inside the guest.
const LINUX_BIN: &str = "/usr/local/lib/vmlab/vmlab-agent";
const LINUX_UNIT: &str = "/etc/systemd/system/vmlab-agent.service";
// Deliberately space-free: QGA's guest-exec re-quotes argv tokens on Windows,
// which mangles an embedded-quoted `binPath= "..."` (sc then registers a bad
// path and StartService fails with error 87). A path without spaces lets
// `binPath=` ride as a plain unquoted token, the form verified live.
const WINDOWS_DIR: &str = r"C:\ProgramData\vmlab";
const WINDOWS_BIN: &str = r"C:\ProgramData\vmlab\vmlab-agent.exe";

const SYSTEMD_UNIT: &str = "\
[Unit]
Description=vmlab guest agent (terminals/exec/files over virtio-serial)

[Service]
ExecStart=/usr/local/lib/vmlab/vmlab-agent
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
";

/// Install + verify; returns the asset's version stamp for the template
/// metadata, or `None` when skipped (reason already logged).
pub async fn install(
    vm: &Arc<VmInstance>,
    wants_agent: bool,
    arch: &str,
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

    // The hook runs right after boot: wait for QGA before driving it.
    vm.wait_agent_up(Duration::from_secs(600))
        .await
        .context("waiting for the guest agent before the vmlab-agent install")?;
    let qga = vm.qga().await?;
    let osinfo = qga.get_osinfo(Duration::from_secs(30)).await?;
    let os = if osinfo["id"].as_str() == Some("mswindows") {
        AgentOs::Windows
    } else {
        AgentOs::Linux
    };

    let asset = match ensure_agent_asset(os, arch) {
        Ok(a) => a,
        Err(e) => {
            log(format!(
                "agent: skipped — no binary for this guest: {e:#}\n"
            ));
            return Ok(None);
        }
    };

    log(format!(
        "agent: installing vmlab-agent ({}-{arch}, {})\n",
        os.key(),
        asset.version
    ));
    let installed = match os {
        AgentOs::Linux => install_linux(&qga, &asset, log).await?,
        AgentOs::Windows => install_windows(&qga, &asset).await?,
    };
    if !installed {
        return Ok(None);
    }

    // The service is running now — prove the channel end-to-end before the
    // build proceeds (drop the handle afterwards; the template seals soon).
    // Agent-first execs during a first-boot provision have usually just
    // failed a handshake against the not-yet-installed agent, which
    // `vm.agent()` remembers for 30 s — so clear that memory before each
    // attempt, and give the freshly started service a moment to open the
    // port.
    let mut verified: Result<()> = Err(anyhow::anyhow!("agent verification never ran"));
    for attempt in 0..12 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        vm.drop_agent().await; // also clears the failed-handshake memory
        verified = vm.agent().await.map(|_| ());
        if verified.is_ok() {
            break;
        }
    }
    verified.context("the installed vmlab-agent did not answer its handshake")?;
    vm.drop_agent().await;
    log("agent: installed and answering\n".into());
    Ok(Some(asset.version.clone()))
}

/// One QGA exec that must succeed (exit 0).
async fn run(
    qga: &crate::qga::GaClient,
    what: &str,
    cmd: &str,
    args: &[&str],
) -> Result<crate::qga::ExecResult> {
    let out = qga
        .exec(cmd, args, true, Duration::from_secs(120))
        .await
        .with_context(|| format!("agent install: {what}"))?;
    if out.exit_code != 0 {
        bail!(
            "agent install: {what} failed (exit {}): {}{}",
            out.exit_code,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok(out)
}

/// Returns whether the service was actually installed (`false` = skipped).
async fn install_linux(
    qga: &crate::qga::GaClient,
    asset: &AgentAsset,
    log: &(dyn Fn(String) + Sync),
) -> Result<bool> {
    // Only systemd guests get a service; anything else would need per-init
    // integration that no shipped template requires.
    let systemd = qga
        .exec(
            "test",
            &["-d", "/run/systemd/system"],
            true,
            Duration::from_secs(30),
        )
        .await
        .map(|r| r.exit_code == 0)
        .unwrap_or(false);
    if !systemd {
        log("agent: skipped (guest has no systemd; no service to register)\n".into());
        return Ok(false);
    }

    run(qga, "mkdir", "mkdir", &["-p", "/usr/local/lib/vmlab"]).await?;
    let upload = Duration::from_secs(300);
    qga.file_write(LINUX_BIN, &asset.read()?, upload)
        .await
        .context("agent install: uploading the binary")?;
    run(qga, "chmod", "chmod", &["0755", LINUX_BIN]).await?;
    qga.file_write(LINUX_UNIT, SYSTEMD_UNIT.as_bytes(), upload)
        .await
        .context("agent install: writing the systemd unit")?;
    run(
        qga,
        "systemctl enable --now",
        "systemctl",
        &["enable", "--now", "vmlab-agent.service"],
    )
    .await?;
    Ok(true)
}

async fn install_windows(qga: &crate::qga::GaClient, asset: &AgentAsset) -> Result<bool> {
    run(
        qga,
        "mkdir",
        "cmd.exe",
        &[
            "/c",
            &format!("if not exist {WINDOWS_DIR} mkdir {WINDOWS_DIR}"),
        ],
    )
    .await?;
    qga.file_write(WINDOWS_BIN, &asset.read()?, Duration::from_secs(300))
        .await
        .context("agent install: uploading the binary")?;
    // `sc create` fails when the service exists (layered template rebuilds);
    // reconfigure instead.
    let create = qga
        .exec(
            "sc.exe",
            &[
                "create",
                "vmlab-agent",
                "binPath=",
                WINDOWS_BIN,
                "start=",
                "auto",
            ],
            true,
            Duration::from_secs(120),
        )
        .await
        .context("agent install: sc create")?;
    if create.exit_code != 0 {
        run(
            qga,
            "sc config",
            "sc.exe",
            &[
                "config",
                "vmlab-agent",
                "binPath=",
                WINDOWS_BIN,
                "start=",
                "auto",
            ],
        )
        .await?;
    }
    // Auto-restart on failure, forever.
    run(
        qga,
        "sc failure",
        "sc.exe",
        &[
            "failure",
            "vmlab-agent",
            "reset=",
            "86400",
            "actions=",
            "restart/5000/restart/5000/restart/5000",
        ],
    )
    .await?;
    run(qga, "sc start", "sc.exe", &["start", "vmlab-agent"]).await?;
    Ok(true)
}
