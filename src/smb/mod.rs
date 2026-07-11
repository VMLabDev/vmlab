//! SMB shared-folder backend (PRD §7.5).
//!
//! Each VM may declare `share {}` blocks mapping a host directory to a guest
//! path, served over SMB by the lab daemon at the segment gateway and exposed
//! as `\\<gateway>\<share>`. This module implements the **bundled `smbd`**
//! strategy (PRD §7.5 strategy 2): the daemon generates a Samba config, runs
//! `smbd` **unprivileged** on a localhost high port, and the switch proxies the
//! segment gateway's port 445 to it.
//!
//! - [`config`]: `smb.conf` generation + credential generation + NT1 check.
//! - [`server`]: spawn/stop the unprivileged `smbd`.
//! - [`mount`]: guest mount-command string generation (Linux/Windows/XP).
//!
//! The top-level [`LabSmb`] ties these together for the daemon: given the
//! lab's VMs, gateways, and shares it assigns a port, mints per-VM credentials,
//! builds share definitions, spawns one `smbd`, and produces a per-VM mount
//! plan plus its credential.
//!
//! ## Network boundary (NOT this module's job)
//!
//! This module exposes [`LabSmb::listen_port`]. The **daemon must proxy** the
//! segment gateway's TCP port 445 to `127.0.0.1:<listen_port>` on every segment
//! a sharing VM sits on — that proxy is the network layer's responsibility, not
//! the SMB module's.

pub mod config;
pub mod mount;
pub mod server;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

pub use config::{ShareDef, SmbConfig, SmbCredentials};
pub use mount::{linux_mount_cmd, windows_mount_cmds, xp_net_use_string};
pub use server::{SmbError, SmbServer};

use crate::config::model::Share;

/// OS hint for how a mount step should be executed in the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsHint {
    /// `mount -t cifs ...` run via the guest agent.
    Linux,
    /// `net use` / `mklink` run via the guest agent.
    Windows,
    /// `net use` typed via the screen-automation surface (no agent on XP).
    WindowsXp,
}

/// A single command to run in the guest to realise one share's mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountStep {
    pub os_hint: OsHint,
    pub command: String,
    pub args: Vec<String>,
}

/// Per-VM SMB plan for a lab: the credential, the share definitions, and the
/// gateway the VM reaches its shares through.
#[derive(Debug, Clone)]
struct VmPlan {
    creds: SmbCredentials,
    gateway: Ipv4Addr,
    /// (share name, host path, guest target, readonly, smb1)
    shares: Vec<(String, PathBuf, String, bool, bool)>,
}

/// High-level SMB orchestration for one lab.
#[derive(Debug)]
pub struct LabSmb {
    listen_port: u16,
    smb_dir: PathBuf,
    vms: HashMap<String, VmPlan>,
    server: Option<SmbServer>,
}

impl LabSmb {
    /// Build a lab's SMB plan. `vmlab_dir` is the lab's `.vmlab` directory;
    /// SMB state lands under `<vmlab_dir>/smb`. `vms` is each sharing VM with
    /// its gateway IP and declared shares.
    ///
    /// Credentials are minted deterministically per call but the passwords are
    /// random, so they are stable within one `LabSmb` instance (the daemon
    /// keeps one per lab while it is up).
    pub fn plan(
        lab: &str,
        vmlab_dir: &Path,
        listen_port: u16,
        vms: &[(String, Ipv4Addr, Vec<Share>)],
    ) -> LabSmb {
        let smb_dir = vmlab_dir.join("smb");
        // One stable credential per lab (see load_or_create) — also keeps
        // the passdb consistent when several VMs share (same unix user).
        let lab_creds = SmbCredentials::load_or_create(lab, &smb_dir);
        let mut plans = HashMap::new();
        for (vm_name, gateway, shares) in vms {
            let creds = lab_creds.clone();
            let share_tuples = shares
                .iter()
                .map(|s| {
                    (
                        s.name.clone(),
                        s.host.clone(),
                        s.guest.clone(),
                        s.readonly,
                        s.smb1,
                    )
                })
                .collect();
            plans.insert(
                vm_name.clone(),
                VmPlan {
                    creds,
                    gateway: *gateway,
                    shares: share_tuples,
                },
            );
        }
        LabSmb {
            listen_port,
            smb_dir,
            vms: plans,
            server: None,
        }
    }

    /// The credential minted for a machine. `mount_plan` embeds it in VM
    /// guest mount commands; the lab daemon hands it to volume-declaring
    /// containers (their cinit mounts CIFS itself, PRD §18).
    pub fn credentials(&self, vm: &str) -> Option<&SmbCredentials> {
        self.vms.get(vm).map(|p| &p.creds)
    }

    /// The high port `smbd` listens on. The daemon must proxy each relevant
    /// segment gateway's port 445 to `127.0.0.1:<this>`.
    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }

    /// Build the [`SmbConfig`] for this lab from all VMs' shares.
    pub fn build_config(&self) -> SmbConfig {
        let mut shares = Vec::new();
        let mut any_smb1 = false;
        // Deterministic ordering for stable conf output.
        let mut vm_names: Vec<&String> = self.vms.keys().collect();
        vm_names.sort();
        for vm in vm_names {
            let plan = &self.vms[vm];
            for (name, host, _guest, readonly, smb1) in &plan.shares {
                if *smb1 {
                    any_smb1 = true;
                }
                shares.push(ShareDef {
                    name: name.clone(),
                    host_path: host.clone(),
                    readonly: *readonly,
                    smb1: *smb1,
                    allowed_user: plan.creds.username.clone(),
                });
            }
        }
        SmbConfig {
            listen_port: self.listen_port,
            lab_dir: self.smb_dir.clone(),
            shares,
            any_smb1,
        }
    }

    /// Spawn the lab's `smbd` from a fully-formed config (host paths resolved
    /// by the caller) and the per-VM credential map. Holds the server so it is
    /// stopped on drop.
    pub fn spawn(&mut self, config: SmbConfig) -> Result<u16, SmbError> {
        let creds: HashMap<String, String> = self
            .vms
            .values()
            .map(|p| (p.creds.username.clone(), p.creds.password.clone()))
            .collect();
        let server = SmbServer::spawn(config, creds)?;
        let port = server.listen_port();
        self.server = Some(server);
        Ok(port)
    }

    /// Stop the lab's `smbd`.
    pub fn stop(&mut self) {
        if let Some(s) = self.server.as_mut() {
            s.stop();
        }
        self.server = None;
    }

    /// Build the per-VM mount plan: one [`MountStep`] sequence the daemon runs
    /// (via the guest agent, or screen automation on XP) to mount every share.
    /// `os_hint` selects Linux/Windows/XP command generation.
    pub fn mount_plan(&self, vm: &str, os_hint: OsHint) -> Vec<MountStep> {
        let Some(plan) = self.vms.get(vm) else {
            return Vec::new();
        };
        let creds = &plan.creds;
        let gw = plan.gateway;
        let mut steps = Vec::new();
        for (share, _host, guest, readonly, smb1) in &plan.shares {
            match os_hint {
                OsHint::Linux => {
                    let (cmd, args) = linux_mount_cmd(
                        gw,
                        share,
                        guest,
                        &creds.username,
                        &creds.password,
                        *readonly,
                        *smb1,
                    );
                    steps.push(MountStep {
                        os_hint,
                        command: cmd,
                        args,
                    });
                }
                OsHint::Windows => {
                    for (cmd, args) in
                        windows_mount_cmds(gw, share, guest, &creds.username, &creds.password)
                    {
                        steps.push(MountStep {
                            os_hint,
                            command: cmd,
                            args,
                        });
                    }
                }
                OsHint::WindowsXp => {
                    // XP always maps to a drive letter; `guest` is expected to
                    // be `X:` for these guests.
                    let s = xp_net_use_string(gw, share, guest, &creds.username, &creds.password);
                    steps.push(MountStep {
                        os_hint,
                        command: s,
                        args: Vec::new(),
                    });
                }
            }
        }
        // Drive letters are per-logon-session: the agent's mounts live in
        // SYSTEM's session, invisible to the console desktop. Drop a
        // double-clickable connect script on the shared desktop so
        // interactive users can map the same shares in their own session.
        if os_hint == OsHint::Windows {
            let letters: Vec<(&str, &str)> = plan
                .shares
                .iter()
                .filter(|(_, _, guest, _, _)| mount::is_drive_letter(guest))
                .map(|(share, _, guest, _, _)| (share.as_str(), &guest[..2]))
                .collect();
            if !letters.is_empty() {
                let (cmd, args) = mount::windows_desktop_script_cmd(
                    gw,
                    &letters,
                    &creds.username,
                    &creds.password,
                );
                steps.push(MountStep {
                    os_hint,
                    command: cmd,
                    args,
                });
                // And authenticate future logons automatically.
                let (cmd, args) =
                    mount::windows_logon_cred_cmd(gw, &creds.username, &creds.password);
                steps.push(MountStep {
                    os_hint,
                    command: cmd,
                    args,
                });
            }
        }
        steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(name: &str, guest: &str, readonly: bool, smb1: bool) -> Share {
        Share {
            span: (0, 0),
            host: PathBuf::from(format!("/host/{name}")),
            guest: guest.to_string(),
            readonly,
            smb1,
            name: name.to_string(),
            transport: crate::config::model::ShareTransport::Smb,
        }
    }

    fn sample() -> LabSmb {
        let vms = vec![
            (
                "web".to_string(),
                Ipv4Addr::new(10, 0, 0, 1),
                vec![share("src", "/mnt/src", false, false)],
            ),
            (
                "xp".to_string(),
                Ipv4Addr::new(10, 0, 1, 1),
                vec![share("data", "Z:", true, true)],
            ),
        ];
        LabSmb::plan("mylab", Path::new("/lab/.vmlab"), 14450, &vms)
    }

    #[test]
    fn credentials_present_per_vm_and_stable() {
        let lab = sample();
        let web = lab.credentials("web").unwrap().clone();
        let xp = lab.credentials("xp").unwrap().clone();
        // Unprivileged passdb: username is the Unix user, shared per lab.
        assert_eq!(web.username, config::current_unix_user());
        assert_eq!(web.username, xp.username);
        assert!(!web.password.is_empty());
        // stable within the instance
        assert_eq!(lab.credentials("web").unwrap().password, web.password);
        assert_eq!(lab.listen_port(), 14450);
    }

    #[test]
    fn build_config_collects_shares_and_smb1_flag() {
        let lab = sample();
        let cfg = lab.build_config();
        assert!(cfg.any_smb1); // xp share is smb1
        assert_eq!(cfg.shares.len(), 2);
        assert_eq!(cfg.lab_dir, PathBuf::from("/lab/.vmlab/smb"));
        // each share scoped to the lab's authenticated account
        let data = cfg.shares.iter().find(|s| s.name == "data").unwrap();
        assert_eq!(data.allowed_user, config::current_unix_user());
        assert!(data.readonly);
    }

    #[test]
    fn linux_mount_plan() {
        let lab = sample();
        let steps = lab.mount_plan("web", OsHint::Linux);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].command, "mount");
        let joined = steps[0].args.join(" ");
        assert!(joined.contains("//10.0.0.1/src"));
        assert!(joined.contains("vers=3.0"));
        assert!(joined.contains(&format!("username={}", config::current_unix_user())));
    }

    #[test]
    fn xp_mount_plan_is_net_use_string() {
        let lab = sample();
        let steps = lab.mount_plan("xp", OsHint::WindowsXp);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].os_hint, OsHint::WindowsXp);
        assert!(
            steps[0]
                .command
                .starts_with("net use Z: \\\\10.0.1.1\\data")
        );
        assert!(
            steps[0]
                .command
                .contains(&format!("/user:{}", config::current_unix_user()))
        );
    }

    #[test]
    fn windows_folder_mount_plan_has_two_steps() {
        let vms = vec![(
            "win".to_string(),
            Ipv4Addr::new(10, 0, 0, 1),
            vec![share("data", "C:\\mnt\\data", false, false)],
        )];
        let lab = LabSmb::plan("l", Path::new("/lab/.vmlab"), 14451, &vms);
        let steps = lab.mount_plan("win", OsHint::Windows);
        assert_eq!(steps.len(), 2); // net use auth + mklink
        assert_eq!(steps[1].command, "cmd");
        assert!(steps[1].args.join(" ").contains("mklink /D C:\\mnt\\data"));
    }

    #[test]
    fn unknown_vm_yields_empty_plan() {
        let lab = sample();
        assert!(lab.mount_plan("nope", OsHint::Linux).is_empty());
        assert!(lab.credentials("nope").is_none());
    }
}
