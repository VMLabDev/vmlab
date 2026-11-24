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
//! - [`steps`]: the ordered guest command plan, per guest OS, for both
//!   transports (virtiofs and SMB).
//!
//! The top-level [`LabSmb`] ties these together for the daemon: given the
//! lab's VMs, gateways, and shares it mints a credential per machine, builds
//! share definitions, spawns **one `smbd` per sharing machine**, and produces
//! a per-VM mount plan plus its credential.
//!
//! ## One server per machine
//!
//! A share is mappable only with its owning machine's credential (PRD §7.5).
//! An unprivileged passdb holds one account, the host user's, so one server
//! for the lab meant one password for the lab: a guest with one read-only
//! share could write every other machine's (docs/reviews/smb.md F2). Each
//! machine therefore gets a server of its own — its own state directory,
//! passdb, password and port — that exports its shares and nothing else.
//!
//! ## Network boundary (NOT this module's job)
//!
//! This module exposes [`LabSmb::listen_port`]. The **daemon must proxy** the
//! segment gateway's TCP port 445 to `127.0.0.1:<that machine's port>` for
//! what that machine's NIC sends — that proxy is the network layer's
//! responsibility, not the SMB module's.

pub mod config;
pub mod mount;
pub mod server;
pub mod steps;

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

pub use config::{ShareDef, SmbConfig, SmbCredentials};
pub use mount::{linux_mount_cmd, macos_mount_cmd, windows_mount_cmds};
pub use server::{SmbError, SmbServer};
pub use steps::{VirtiofsMount, guest_os_hint, mount_plan};

use crate::config::model::Share;

/// OS hint for how a mount step should be executed in the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsHint {
    /// `mount -t cifs ...` run via the guest agent.
    Linux,
    /// `net use` / a PowerShell link step run via the guest agent.
    Windows,
    /// `net use` run via the guest agent.
    WindowsXp,
    /// DOS and Windows 9x profiles need guest-specific mount setup.
    Windows9x,
    /// `mount_smbfs` run via the guest agent.
    Macos,
}

/// A single command to run in the guest to realise one share's mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountStep {
    pub os_hint: OsHint,
    pub command: String,
    pub args: Vec<String>,
    /// The share this step mounts — what a step that never succeeds is
    /// reported against. `None` for a step serving every share at once.
    pub share: Option<String>,
    /// An exit code with which the step refuses rather than fails — the
    /// guest is in a state another attempt will not change, so the executor
    /// reports it at once instead of retrying.
    pub refused_exit: Option<i32>,
    /// The password this step's command line carries, if any. Whoever runs
    /// the step passes every text it reports about it through
    /// [`MountStep::scrub`], because an error that quotes the command line
    /// quotes the password.
    pub secret: Option<String>,
}

/// What [`MountStep::scrub`] puts where the password was.
pub const REDACTED: &str = "[redacted]";

impl MountStep {
    /// `text` without this step's password, in the spelling of the command
    /// line and in the percent-encoded one of a `mount_smbfs` URL.
    pub fn scrub(&self, text: &str) -> String {
        let Some(secret) = self.secret.as_deref().filter(|s| !s.is_empty()) else {
            return text.to_string();
        };
        let encoded = mount::url_component(secret);
        let text = text.replace(secret, REDACTED);
        if encoded == secret {
            text
        } else {
            text.replace(&encoded, REDACTED)
        }
    }
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

/// How many ports one machine's `smbd` is tried on before the start fails: a
/// port free when the plan was made can be taken by the time `smbd` binds it.
const PORT_TRIES: usize = 10;

/// High-level SMB orchestration for one lab.
#[derive(Debug)]
pub struct LabSmb {
    smb_dir: PathBuf,
    vms: BTreeMap<String, VmPlan>,
    /// The running servers, by machine.
    servers: BTreeMap<String, SmbServer>,
}

impl LabSmb {
    /// Build a lab's SMB plan. `vmlab_dir` is the lab's `.vmlab` directory;
    /// SMB state lands under `<vmlab_dir>/smb`, one directory per machine.
    /// `vms` is each sharing VM with its gateway IP and declared shares.
    ///
    /// Each machine's credential is read from its state directory, or minted
    /// and kept there, so it is stable from one start to the next.
    pub fn plan(
        vmlab_dir: &Path,
        vms: &[(String, Ipv4Addr, Vec<Share>)],
    ) -> Result<LabSmb, SmbError> {
        let smb_dir = vmlab_dir.join("smb");
        let mut plans = BTreeMap::new();
        for (vm_name, gateway, shares) in vms {
            let dir = machine_dir(&smb_dir, vm_name);
            let creds =
                SmbCredentials::load_or_create(&dir).map_err(|source| SmbError::Credentials {
                    path: dir.join("creds"),
                    source,
                })?;
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
        Ok(LabSmb {
            smb_dir,
            vms: plans,
            servers: BTreeMap::new(),
        })
    }

    /// The credential minted for a machine. `mount_plan` embeds it in VM
    /// guest mount commands; the lab daemon hands it to volume-declaring
    /// containers (their cinit mounts CIFS itself, PRD §18).
    pub fn credentials(&self, vm: &str) -> Option<&SmbCredentials> {
        self.vms.get(vm).map(|p| &p.creds)
    }

    /// The high port a machine's `smbd` listens on, once started. The daemon
    /// must proxy the machine's `gateway:445` to `127.0.0.1:<this>`.
    pub fn listen_port(&self, vm: &str) -> Option<u16> {
        self.servers.get(vm).map(SmbServer::listen_port)
    }

    #[cfg(test)]
    pub(crate) fn kill_for_test(&mut self, vm: &str) {
        self.servers.get_mut(vm).unwrap().kill_for_test();
    }

    /// Every running server's machine and port, by machine name.
    pub fn listen_ports(&self) -> Vec<(String, u16)> {
        self.servers
            .iter()
            .map(|(vm, server)| (vm.clone(), server.listen_port()))
            .collect()
    }

    pub(crate) fn exited_servers(&mut self) -> Vec<(String, std::process::ExitStatus)> {
        self.servers
            .iter_mut()
            .filter_map(|(machine, server)| match server.try_wait() {
                Ok(status) => status.map(|status| (machine.clone(), status)),
                Err(error) => {
                    tracing::warn!(%machine, %error, "checking smbd child");
                    None
                }
            })
            .collect()
    }

    pub(crate) fn remove_server(&mut self, machine: &str) {
        self.servers.remove(machine);
    }

    pub(crate) fn has_missing_servers(&self) -> bool {
        self.servers.len() < self.vms.len()
    }

    pub(crate) fn restart_missing(
        &mut self,
        ports: &[u16],
    ) -> Vec<(String, Result<u16, SmbError>)> {
        let missing: Vec<_> = self
            .vms
            .keys()
            .filter(|machine| !self.servers.contains_key(*machine))
            .cloned()
            .collect();
        let mut ports = ports.iter().copied();
        missing
            .into_iter()
            .map(|machine| {
                let result = self.start_one(&machine, &mut ports).map(|server| {
                    let port = server.listen_port();
                    self.servers.insert(machine.clone(), server);
                    port
                });
                (machine, result)
            })
            .collect()
    }

    /// Where a machine's `creds` file is, for the line `up` prints.
    pub fn creds_hint() -> &'static str {
        ".vmlab/smb/machines/<machine>/creds"
    }

    /// Build the [`SmbConfig`] of one machine's server, on `listen_port`,
    /// from that machine's shares. `None` for a machine with no plan.
    pub fn build_config(&self, vm: &str, listen_port: u16) -> Option<SmbConfig> {
        let plan = self.vms.get(vm)?;
        let shares: Vec<ShareDef> = plan
            .shares
            .iter()
            .map(|(name, host, _guest, readonly, smb1)| ShareDef {
                name: name.clone(),
                host_path: host.clone(),
                readonly: *readonly,
                smb1: *smb1,
                allowed_user: plan.creds.username.clone(),
            })
            .collect();
        let lab_dir = machine_dir(&self.smb_dir, vm);
        Some(SmbConfig {
            listen_port,
            run_dir: crate::paths::smb_runtime_dir(&lab_dir),
            lab_dir,
            any_smb1: shares.iter().any(|s| s.smb1),
            shares,
        })
    }

    /// Spawn one `smbd` per machine, each on a port of `ports` — the free
    /// ports of the share plan, first choice first. A server that dies at
    /// start is tried on the next port, [`PORT_TRIES`] times at most; any
    /// other failure, and a machine left without a port, stops the servers
    /// already started and fails the whole start, so a lab never runs with
    /// some machines' shares served and others' silently not.
    pub fn start(&mut self, ports: &[u16]) -> Result<(), SmbError> {
        // No share may reach another machine's server state either: the
        // render refuses a share over its own server's directory only.
        for plan in self.vms.values() {
            for (name, host, ..) in &plan.shares {
                if let Some(reason) =
                    config::share_state_problem(host, &self.smb_dir, "vmlab's SMB state directory")
                {
                    return Err(SmbError::Share(config::ShareRefused {
                        name: name.clone(),
                        reason,
                    }));
                }
            }
        }
        remove_lab_wide_state(&self.smb_dir);
        let mut ports = ports.iter().copied();
        let machines: Vec<String> = self.vms.keys().cloned().collect();
        for vm in machines {
            match self.start_one(&vm, &mut ports) {
                Ok(server) => {
                    self.servers.insert(vm, server);
                }
                Err(e) => {
                    self.stop();
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn start_one(
        &self,
        vm: &str,
        ports: &mut dyn Iterator<Item = u16>,
    ) -> Result<SmbServer, SmbError> {
        let creds = &self.vms[vm].creds;
        let mut last = None;
        for port in ports.take(PORT_TRIES) {
            let config = self.build_config(vm, port).expect("a planned machine");
            let account = HashMap::from([(creds.username.clone(), creds.password.clone())]);
            match SmbServer::spawn(config, account) {
                Ok(server) => return Ok(server),
                Err(e @ SmbError::DiedOnStart { .. }) => {
                    tracing::warn!("smbd for \"{vm}\" on port {port} failed: {e}");
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or(SmbError::NoPort {
            machine: vm.to_string(),
        }))
    }

    /// Stop every `smbd` of the lab.
    pub fn stop(&mut self) {
        for server in self.servers.values_mut() {
            server.stop();
        }
        self.servers.clear();
    }

    /// Build the per-VM mount plan the daemon runs through the guest agent.
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
                    // `mount` needs the target to exist, and a guest path is
                    // usually one nobody created.
                    steps.push(MountStep {
                        os_hint,
                        command: "mkdir".into(),
                        args: vec!["-p".into(), guest.clone()],
                        share: Some(share.clone()),
                        refused_exit: None,
                        secret: None,
                    });
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
                        share: Some(share.clone()),
                        refused_exit: None,
                        secret: Some(creds.password.clone()),
                    });
                }
                OsHint::Windows => {
                    for cmd in
                        windows_mount_cmds(gw, share, guest, &creds.username, &creds.password)
                    {
                        steps.push(MountStep {
                            os_hint,
                            command: cmd.program,
                            args: cmd.args,
                            share: Some(share.clone()),
                            refused_exit: cmd.refused_exit,
                            secret: Some(creds.password.clone()),
                        });
                    }
                }
                OsHint::Macos => {
                    steps.push(MountStep {
                        os_hint,
                        command: "mkdir".into(),
                        args: vec!["-p".into(), guest.clone()],
                        share: Some(share.clone()),
                        refused_exit: None,
                        secret: None,
                    });
                    let (cmd, args) = macos_mount_cmd(
                        gw,
                        share,
                        guest,
                        &creds.username,
                        &creds.password,
                        *readonly,
                    );
                    steps.push(MountStep {
                        os_hint,
                        command: cmd,
                        args,
                        share: Some(share.clone()),
                        refused_exit: None,
                        secret: Some(creds.password.clone()),
                    });
                }
                OsHint::WindowsXp => {
                    // XP always maps to a drive letter; `guest` is expected to
                    // be `X:` for these guests.
                    let (command, args) =
                        mount::xp_net_use_cmd(gw, share, guest, &creds.username, &creds.password);
                    steps.push(MountStep {
                        os_hint,
                        command,
                        args,
                        share: Some(share.clone()),
                        refused_exit: None,
                        secret: Some(creds.password.clone()),
                    });
                }
                OsHint::Windows9x => {}
            }
        }
        // The agent's mounts live in SYSTEM's logon session: every session
        // sees the drive letters and folder symlinks, but each logon
        // authenticates to the gateway separately.
        if os_hint == OsHint::Windows && !plan.shares.is_empty() {
            // Drop a double-clickable connect script on the shared desktop
            // naming the letters, so an interactive user can authenticate by
            // hand.
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
                    share: None,
                    refused_exit: None,
                    secret: Some(creds.password.clone()),
                });
            }
            // And authenticate future logons automatically — a folder-path
            // share needs this as much as a letter does, and the agent reads
            // it back into every logon it mints (§19.2).
            let (cmd, args) = mount::windows_logon_cred_cmd(gw, &creds.username, &creds.password);
            steps.push(MountStep {
                os_hint,
                command: cmd,
                args,
                share: None,
                refused_exit: None,
                secret: Some(creds.password.clone()),
            });
        }
        steps
    }
}

/// The state directory of one machine's server under the lab's
/// `.vmlab/smb`: its `smb.conf`, passdb, log and `creds`.
fn machine_dir(smb_dir: &Path, vm: &str) -> PathBuf {
    smb_dir.join("machines").join(vm)
}

/// Remove what the one server of the whole lab kept directly under
/// `.vmlab/smb` before each machine had its own: above all the `creds` file,
/// whose password opens nothing any more.
fn remove_lab_wide_state(smb_dir: &Path) {
    for name in ["creds", "smb.conf", "passdb.tdb"] {
        let _ = std::fs::remove_file(smb_dir.join(name));
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
        LabSmb::plan(tempfile::tempdir().unwrap().path(), &vms).unwrap()
    }

    #[test]
    fn credentials_present_per_vm_and_stable() {
        let lab = sample();
        let web = lab.credentials("web").unwrap().clone();
        let xp = lab.credentials("xp").unwrap().clone();
        // Unprivileged passdb: the username is the Unix user for every
        // machine. The password is what tells them apart.
        assert_eq!(web.username, config::current_unix_user());
        assert_eq!(web.username, xp.username);
        assert!(!web.password.is_empty());
        // stable within the instance
        assert_eq!(lab.credentials("web").unwrap().password, web.password);
        assert_eq!(lab.listen_port("web"), None, "nothing started yet");
    }

    /// A machine's credential is kept in its own state directory and read
    /// back at the next start, and the lab-wide file of the one server
    /// before it is not what any machine is given.
    #[test]
    fn a_machines_credential_is_kept_in_its_own_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let vmlab_dir = tmp.path().join(".vmlab");
        std::fs::create_dir_all(vmlab_dir.join("smb")).unwrap();
        let user = config::current_unix_user();
        std::fs::write(
            vmlab_dir.join("smb/creds"),
            format!("{user}:LabWidePassword"),
        )
        .unwrap();
        let vms = vec![
            (
                "web".to_string(),
                Ipv4Addr::new(10, 0, 0, 1),
                vec![share("src", "/mnt/src", false, false)],
            ),
            (
                "db".to_string(),
                Ipv4Addr::new(10, 0, 0, 1),
                vec![share("data", "/mnt/data", false, false)],
            ),
        ];
        let first = LabSmb::plan(&vmlab_dir, &vms).unwrap();
        let web = first.credentials("web").unwrap().clone();
        let db = first.credentials("db").unwrap().clone();
        assert_ne!(web.password, db.password);
        assert_ne!(web.password, "LabWidePassword");
        assert_eq!(
            std::fs::read_to_string(vmlab_dir.join("smb/machines/web/creds")).unwrap(),
            format!("{user}:{}", web.password)
        );
        let again = LabSmb::plan(&vmlab_dir, &vms).unwrap();
        assert_eq!(again.credentials("web"), Some(&web));
        assert_eq!(again.credentials("db"), Some(&db));
    }

    /// Each machine's server exports that machine's shares and nothing else,
    /// from a state directory of its own, and only the server of a machine
    /// with an `smb1` share is relaxed to NT1.
    #[test]
    fn build_config_holds_one_machines_shares() {
        let lab = sample();
        let xp = lab.build_config("xp", 14451).unwrap();
        assert!(xp.any_smb1); // xp share is smb1
        assert_eq!(xp.listen_port, 14451);
        assert_eq!(xp.shares.len(), 1);
        let state = lab.smb_dir.join("machines/xp");
        assert_eq!(xp.lab_dir, state);
        assert_eq!(xp.run_dir, crate::paths::smb_runtime_dir(&state));
        // the share scoped to the server's one authenticated account
        let data = &xp.shares[0];
        assert_eq!(data.name, "data");
        assert_eq!(data.allowed_user, config::current_unix_user());
        assert!(data.readonly);

        let web = lab.build_config("web", 14450).unwrap();
        assert!(
            !web.any_smb1,
            "another machine's smb1 share relaxes nothing here"
        );
        assert_eq!(web.shares.len(), 1);
        assert_eq!(web.shares[0].name, "src");
        assert_ne!(web.lab_dir, xp.lab_dir);
        assert_ne!(web.run_dir, xp.run_dir);
        assert!(lab.build_config("nope", 14452).is_none());
    }

    /// A share over the lab's SMB state reaches every machine's server, so
    /// no server starts with one, whichever machine declares it.
    #[test]
    fn a_share_over_another_machines_server_state_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let vmlab_dir = tmp.path().join(".vmlab");
        let mut over = share("over", "/mnt/over", false, false);
        over.host = vmlab_dir.join("smb/machines/db");
        let vms = vec![
            ("web".to_string(), Ipv4Addr::new(10, 0, 0, 1), vec![over]),
            (
                "db".to_string(),
                Ipv4Addr::new(10, 0, 0, 1),
                vec![share("data", "/mnt/data", false, false)],
            ),
        ];
        let mut lab = LabSmb::plan(&vmlab_dir, &vms).unwrap();
        match lab.start(&[14450, 14451]) {
            Err(SmbError::Share(refused)) => assert_eq!(refused.name, "over"),
            other => panic!("expected the share to be refused, got {other:?}"),
        }
        assert!(lab.listen_ports().is_empty());
    }

    /// A macOS guest makes the mount point, then `mount_smbfs`s the share
    /// from the same gateway a Linux guest mounts it from.
    #[test]
    fn macos_mount_plan() {
        let lab = sample();
        let steps = lab.mount_plan("web", OsHint::Macos);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].command, "mkdir");
        assert_eq!(steps[0].args, ["-p", "/mnt/src"]);
        assert_eq!(steps[1].command, "/sbin/mount_smbfs");
        assert!(steps.iter().all(|s| s.share.as_deref() == Some("src")));
        let url = &steps[1].args[steps[1].args.len() - 2];
        assert!(url.ends_with("@10.0.0.1/src"), "{url}");
    }

    /// Every step of a plan whose command line holds the password names it,
    /// on each guest family, so the executor can scrub what it reports.
    #[test]
    fn every_step_that_carries_the_password_names_it() {
        let lab = sample();
        let password = lab.credentials("xp").unwrap().password.clone();
        for os in [
            OsHint::Linux,
            OsHint::Windows,
            OsHint::WindowsXp,
            OsHint::Macos,
        ] {
            let steps = lab.mount_plan("xp", os);
            assert!(!steps.is_empty(), "{os:?}");
            let mut carriers = 0;
            for step in &steps {
                let line = format!("{} {}", step.command, step.args.join(" "));
                if line.contains(&password) {
                    carriers += 1;
                    assert_eq!(step.secret.as_deref(), Some(password.as_str()), "{os:?}");
                    assert!(!step.scrub(&line).contains(&password), "{os:?}");
                }
            }
            assert!(carriers > 0, "{os:?}");
        }
    }

    /// The scrub takes the password out in both spellings and leaves a step
    /// without one alone.
    #[test]
    fn scrub_removes_the_password_in_both_spellings() {
        let mut step = MountStep {
            os_hint: OsHint::Macos,
            command: "/sbin/mount_smbfs".into(),
            args: Vec::new(),
            share: None,
            refused_exit: None,
            secret: Some("p@ss w".into()),
        };
        assert_eq!(
            step.scrub("//u:p%40ss%20w@10.0.0.1/data and p@ss w"),
            "//u:[redacted]@10.0.0.1/data and [redacted]"
        );
        step.secret = None;
        assert_eq!(step.scrub("p@ss w"), "p@ss w");
        step.secret = Some(String::new());
        assert_eq!(step.scrub("p@ss w"), "p@ss w");
    }

    #[test]
    fn linux_mount_plan() {
        let lab = sample();
        let steps = lab.mount_plan("web", OsHint::Linux);
        assert_eq!(steps.len(), 2);
        // The mount point is created first: `mount` cannot, and a guest path
        // is usually one nobody made.
        assert_eq!(steps[0].command, "mkdir");
        assert_eq!(steps[0].args, ["-p", "/mnt/src"]);
        assert_eq!(steps[1].command, "mount");
        assert!(steps.iter().all(|s| s.share.as_deref() == Some("src")));
        let joined = steps[1].args.join(" ");
        assert!(joined.contains("//10.0.0.1/src"));
        assert!(joined.contains("vers=3.0"));
        assert!(joined.contains(&format!("username={}", config::current_unix_user())));
    }

    #[test]
    fn a_windows_9x_share_gets_no_modern_windows_step() {
        let lab = sample();
        let steps = lab.mount_plan("xp", guest_os_hint(Some("windows-9x")));
        assert!(
            steps
                .iter()
                .all(|step| !matches!(step.command.as_str(), "cmdkey" | "powershell" | "reg")),
            "Windows 9x plan contains a modern Windows command"
        );
    }

    #[test]
    fn every_mount_step_is_an_argv() {
        let lab = sample();
        for os_hint in [
            OsHint::Linux,
            OsHint::Windows,
            OsHint::WindowsXp,
            OsHint::Windows9x,
            OsHint::Macos,
        ] {
            assert!(
                lab.mount_plan("xp", os_hint)
                    .iter()
                    .all(|step| !step.command.contains(char::is_whitespace)),
                "{os_hint:?} plan must place only the executable in argv[0]"
            );
        }
    }

    #[test]
    fn xp_mount_plan_passes_net_use_arguments_to_the_agent() {
        let lab = sample();
        let steps = lab.mount_plan("xp", OsHint::WindowsXp);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].os_hint, OsHint::WindowsXp);
        assert_eq!(steps[0].command, "net");
        let creds = lab.credentials("xp").unwrap();
        let args = &steps[0].args;
        assert_eq!(args.len(), 6);
        assert_eq!(args[..3], ["use", "Z:", r"\\10.0.1.1\data"]);
        assert!(args[3] == creds.password, "the password is its own argument");
        assert_eq!(args[4], format!("/user:{}", creds.username));
        assert_eq!(args[5], "/persistent:yes");
    }

    /// A folder-path share is a symlink to the gateway's UNC path, which any
    /// logon other than SYSTEM's cannot open without the lab credential — so
    /// it registers the logon hook too, and the agent reads that hook back
    /// into every logon it mints (§19.2). No desktop script: it names letters.
    #[test]
    fn windows_folder_mount_plan_links_and_registers_the_logon_hook() {
        let vms = vec![(
            "win".to_string(),
            Ipv4Addr::new(10, 0, 0, 1),
            vec![share("data", "C:\\mnt\\data", false, false)],
        )];
        let lab = LabSmb::plan(tempfile::tempdir().unwrap().path(), &vms).unwrap();
        let steps = lab.mount_plan("win", OsHint::Windows);
        assert_eq!(steps.len(), 4); // credential + net use auth + link + hook
        assert_eq!(steps[2].command, "powershell");
        assert_eq!(steps[2].refused_exit, Some(mount::LINK_REFUSED_EXIT));
        assert!(steps.iter().filter(|s| s.refused_exit.is_some()).count() == 1);
        assert_eq!(steps[3].command, "reg");
        let hook = steps[3].args.join(" ");
        assert!(
            hook.contains("CurrentVersion\\Run /v vmlab-shares"),
            "{hook}"
        );
        assert!(hook.contains("cmdkey /add:10.0.0.1 "), "{hook}");
        assert!(
            !steps
                .iter()
                .any(|s| s.args.join(" ").contains(mount::DESKTOP_SCRIPT))
        );
    }

    /// A drive letter gets the connect script naming it, then the same hook.
    #[test]
    fn windows_drive_letter_mount_plan_adds_the_desktop_script() {
        let vms = vec![(
            "win".to_string(),
            Ipv4Addr::new(10, 0, 0, 1),
            vec![share("data", "Z:", false, false)],
        )];
        let lab = LabSmb::plan(tempfile::tempdir().unwrap().path(), &vms).unwrap();
        let steps = lab.mount_plan("win", OsHint::Windows);
        assert_eq!(steps.len(), 5); // credential + cleanup + map + script + hook
        assert!(steps[3].args.join(" ").contains(mount::DESKTOP_SCRIPT));
        assert!(steps[4].args.join(" ").contains("/v vmlab-shares"));
    }

    /// PRD §7.5: "a share is mappable only with its owning VM's credential".
    /// The user name is the host user's for every machine, because an
    /// unprivileged passdb takes no other, so the password is what differs
    /// (docs/reviews/smb.md F2).
    #[test]
    fn each_machine_has_its_own_credential() {
        let lab = sample();
        let web = lab.credentials("web").unwrap();
        let xp = lab.credentials("xp").unwrap();
        assert_ne!(
            web.password, xp.password,
            "one password opens the shares of both machines"
        );
    }

    fn which(bin: &str) -> bool {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
    }

    /// `smbclient ... -c ls` on `share` at `port`, stdout and stderr.
    fn smbclient_ls(port: u16, share: &str, creds: &SmbCredentials) -> String {
        let out = std::process::Command::new("smbclient")
            .arg(format!("//127.0.0.1/{share}"))
            .arg("-p")
            .arg(port.to_string())
            .arg("-U")
            .arg(format!("{}%{}", creds.username, creds.password))
            .arg("-c")
            .arg("ls")
            .output()
            .expect("run smbclient");
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    }

    fn free_ports(n: usize) -> Vec<u16> {
        let held: Vec<_> = (0..n)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        held.iter()
            .map(|l| l.local_addr().unwrap().port())
            .collect()
    }

    /// Two machines, a real `smbd`: what `web` was given does not list the
    /// share of `db`, wherever `web` connects (docs/reviews/smb.md F2).
    #[test]
    fn one_machines_credential_does_not_open_anothers_share() {
        if !which("smbd") || !which("smbclient") {
            eprintln!("SKIP: smbd or smbclient not found");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let host_dir = |name: &str| {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{name}.txt")), b"x").unwrap();
            dir
        };
        let gateway = Ipv4Addr::new(10, 0, 0, 1);
        let mut of_web = share("websrc", "/mnt/src", true, false);
        of_web.host = host_dir("websrc");
        let mut of_db = share("dbdata", "/mnt/data", false, false);
        of_db.host = host_dir("dbdata");
        let vms = vec![
            ("web".to_string(), gateway, vec![of_web]),
            ("db".to_string(), gateway, vec![of_db]),
        ];
        let vmlab_dir = tmp.path().join("lab/.vmlab");
        // A port from `free_ports` is released before smbd binds it, so a
        // parallel test can take it in between: the start walks on to the
        // next one, and a start that still fails is tried again.
        let mut started = None;
        for _ in 0..5 {
            let mut lab = LabSmb::plan(&vmlab_dir, &vms).unwrap();
            match lab.start(&free_ports(8)) {
                Ok(()) => {
                    started = Some(lab);
                    break;
                }
                Err(e) => eprintln!("smbd: {e}"),
            }
        }
        let Some(mut lab) = started else {
            eprintln!("SKIP: could not spawn smbd");
            return;
        };
        let web = lab.credentials("web").unwrap().clone();
        let db = lab.credentials("db").unwrap().clone();
        let web_port = lab.listen_port("web").unwrap();
        let db_port = lab.listen_port("db").unwrap();
        assert_ne!(web_port, db_port, "a server each");
        let own = smbclient_ls(web_port, "websrc", &web);
        let owner = smbclient_ls(db_port, "dbdata", &db);
        // The two ways for web to ask: its own server, which does not have
        // the share, and db's server, which does not know its password.
        let at_own_server = smbclient_ls(web_port, "dbdata", &web);
        let at_their_server = smbclient_ls(db_port, "dbdata", &web);
        lab.stop();
        assert!(
            own.contains("websrc.txt"),
            "web lists its own share:\n{own}"
        );
        assert!(
            owner.contains("dbdata.txt"),
            "db lists its own share:\n{owner}"
        );
        for theirs in [at_own_server, at_their_server] {
            assert!(
                !theirs.contains("dbdata.txt"),
                "web's credential listed db's share:\n{theirs}"
            );
        }
    }

    #[test]
    fn unknown_vm_yields_empty_plan() {
        let lab = sample();
        assert!(lab.mount_plan("nope", OsHint::Linux).is_empty());
        assert!(lab.credentials("nope").is_none());
    }
}
