//! Stage the build-only "VMLAB" guest ISO: the vmlab-agent binaries plus a
//! per-OS install script, attached as an extra `media {}` ISO to build VMs
//! only. The template's own unattended-install hook (cloud-init runcmd,
//! subiquity late-commands, autounattend FirstLogonCommands) mounts the ISO
//! and runs the script — the guest installs the agent itself, so no host
//! channel is needed before the agent exists. Sealed templates and clones
//! never see this ISO.
//!
//! Layout:
//!
//! ```text
//! install.sh                    POSIX sh, all Linux distros
//! install.cmd                   Windows batch (FirstLogonCommands)
//! linux/<arch>/vmlab-agent      static musl binary (when built)
//! windows/x86_64/vmlab-agent.exe  (when built)
//! VERSION                       staged asset stamps, one `<os> <stamp>` line each
//! ```
//!
//! The install scripts are embedded in the vmlab binary (version-locked —
//! no asset-sync problem) from `bootstrap/install.{sh,cmd}`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::agent_asset::{AgentOs, candidate_dirs, ensure_agent_asset_in};

const INSTALL_SH: &str = include_str!("bootstrap/install.sh");
const INSTALL_CMD: &str = include_str!("bootstrap/install.cmd");

/// A staged guest-ISO folder, ready for the media cache to pack.
#[derive(Debug)]
pub struct StagedGuestIso {
    pub dir: PathBuf,
    /// Asset version stamp per staged OS flavour, for the sealed metadata
    /// (pick by the verified handshake's `os`).
    pub versions: Vec<(AgentOs, String)>,
}

impl StagedGuestIso {
    pub fn version_for(&self, os: AgentOs) -> Option<&str> {
        self.versions
            .iter()
            .find(|(o, _)| *o == os)
            .map(|(_, v)| v.as_str())
    }
}

/// Stage `<work>/guest-iso` with every agent binary available for `arch`.
/// The guest OS isn't known at stage time (profiles carry no OS field), so
/// both flavours ride along when built; at least one must exist — a build
/// that wants an agent but has no binary at all is an error, not a silently
/// agent-less template (set `agent = false` to opt out).
pub fn stage_guest_iso_dir(work: &Path, arch: &str) -> Result<StagedGuestIso> {
    stage_with_dirs(work, arch, &candidate_dirs())
}

fn stage_with_dirs(work: &Path, arch: &str, asset_dirs: &[PathBuf]) -> Result<StagedGuestIso> {
    let dir = work.join("guest-iso");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    std::fs::write(dir.join("install.sh"), INSTALL_SH)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.join("install.sh"),
            std::fs::Permissions::from_mode(0o755),
        )?;
    }
    std::fs::write(dir.join("install.cmd"), INSTALL_CMD)?;

    let mut versions = Vec::new();
    let mut missing = Vec::new();
    for os in [AgentOs::Linux, AgentOs::Windows] {
        match ensure_agent_asset_in(asset_dirs, os, arch) {
            Ok(asset) => {
                let sub = match os {
                    AgentOs::Linux => dir.join("linux").join(arch).join("vmlab-agent"),
                    AgentOs::Windows => dir.join("windows").join(arch).join("vmlab-agent.exe"),
                };
                std::fs::create_dir_all(sub.parent().unwrap())?;
                std::fs::copy(&asset.path, &sub)
                    .with_context(|| format!("staging {}", asset.path.display()))?;
                versions.push((os, asset.version));
            }
            Err(e) => missing.push(format!("{e:#}")),
        }
    }
    if versions.is_empty() {
        bail!(
            "template wants a vmlab-agent but no binary exists for {arch}: {}",
            missing.join("; ")
        );
    }

    let stamp: String = versions
        .iter()
        .map(|(os, v)| format!("{} {v}\n", os.key()))
        .collect();
    std::fs::write(dir.join("VERSION"), stamp)?;
    Ok(StagedGuestIso { dir, versions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_scripts_and_available_binaries() {
        let assets = tempfile::tempdir().unwrap();
        let agent_dir = assets.path().join("agent/linux-x86_64");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("vmlab-agent"), b"elf").unwrap();
        std::fs::write(agent_dir.join("VERSION"), "agent=abc\n").unwrap();

        let work = tempfile::tempdir().unwrap();
        let staged =
            stage_with_dirs(work.path(), "x86_64", &[assets.path().to_path_buf()]).unwrap();

        assert!(staged.dir.join("install.sh").exists());
        assert!(staged.dir.join("install.cmd").exists());
        assert!(staged.dir.join("linux/x86_64/vmlab-agent").exists());
        assert_eq!(staged.version_for(AgentOs::Linux), Some("agent=abc"));
        assert_eq!(staged.version_for(AgentOs::Windows), None);
        let stamp = std::fs::read_to_string(staged.dir.join("VERSION")).unwrap();
        assert_eq!(stamp, "linux agent=abc\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(staged.dir.join("install.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn no_binaries_at_all_is_an_error() {
        let assets = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let err =
            stage_with_dirs(work.path(), "riscv64", &[assets.path().to_path_buf()]).unwrap_err();
        assert!(format!("{err}").contains("no binary exists for riscv64"));
    }
}
