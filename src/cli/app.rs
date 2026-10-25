//! `vmlab app` (PRD §20): one guest program's windows on the host desktop.
//!
//! The verb is a thin shell over [`crate::appshare`]: it finds the machine's
//! guest family in the lab file, reads the host session from its own
//! environment, picks the protocol, and drives the session in this process —
//! the only one with the user's desktop session — or hands it to a detached
//! helper with `--detach`.

use anyhow::{Context, Result, anyhow, bail};

use super::As;
use super::lab::{
    current_lab, effective_profile_name, lab_client_for, registry_labs, root_for, rt, split_vm_ref,
};
use crate::appshare::{self, GuestFamily, HostEnv, Mode};

/// The hidden verb `--detach` re-executes as.
pub const HELPER_VERB: &str = "__appsession";

/// `vmlab app run`'s arguments, shared with the hidden helper `--detach`
/// re-executes.
#[derive(clap::Args, Clone, Debug, PartialEq, Eq)]
pub struct RunArgs {
    /// Machine ([lab/]name)
    pub machine: String,
    /// The window protocol, instead of the one chosen from the guest family
    /// and this desktop session
    #[arg(long, value_enum)]
    pub via: Option<Mode>,
    /// Hand the session to a background helper and return at once
    #[arg(long)]
    pub detach: bool,
    /// Guest working directory for the program
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<String>,
    #[command(flatten)]
    pub run_as: As,
    /// Command and arguments (after --)
    #[arg(last = true)]
    pub cmd: Vec<String>,
}

impl RunArgs {
    /// The arguments the detached helper is re-executed with: these, less
    /// `--detach`.
    fn helper_args(&self) -> Vec<String> {
        let mut out = vec![HELPER_VERB.to_string(), self.machine.clone()];
        if let Some(via) = self.via {
            out.extend(["--via".into(), via.as_str().into()]);
        }
        if let Some(cwd) = &self.cwd {
            out.extend(["--cwd".into(), cwd.clone()]);
        }
        if let Some(user) = &self.run_as.user {
            out.extend(["--user".into(), user.clone()]);
        }
        if let Some(password) = &self.run_as.password {
            out.extend(["--password".into(), password.clone()]);
        }
        out.push("--".into());
        out.extend(self.cmd.iter().cloned());
        out
    }
}

/// `vmlab app run` — show a guest program's windows on this desktop, and exit
/// with the program's code (FreeRDP's, for a Windows guest).
pub fn cmd_app_run(args: RunArgs) -> Result<()> {
    if args.cmd.is_empty() {
        bail!("nothing to run — usage: vmlab app run [lab/]machine -- <cmd> [args...]");
    }
    let (lab, machine) = split_vm_ref(&args.machine)?;
    let rt = rt()?;
    let family = rt.block_on(guest_family(lab.as_deref(), &machine))?;
    let host = HostEnv::detect();
    let selected =
        appshare::select_mode(&machine, family, args.via, &host).map_err(|msg| anyhow!(msg))?;
    if args.detach {
        spawn_helper(&args)?;
        println!(
            "sharing `{}` from {machine} in the background (it ends when the program does)",
            args.cmd[0]
        );
        return Ok(());
    }
    if let Some(notice) = &selected.notice {
        eprintln!("{notice}");
    }
    let req = appshare::Request {
        machine,
        argv: args.cmd,
        cwd: args.cwd,
        user: args.run_as.user,
        password: args.run_as.password,
    };
    let code = rt.block_on(async {
        let (_name, client) = lab_client_for(lab).await?;
        appshare::run(&client, &req, selected.mode, args.via, &host).await
    })?;
    // Everything the session held — the host client, the private directory —
    // is gone by now, so exiting directly loses nothing.
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// The machine's guest family, from its effective profile in the lab file:
/// the cwd's lab, or the root the supervisor registered for a `lab/machine`
/// reference to another one.
async fn guest_family(lab: Option<&str>, machine: &str) -> Result<GuestFamily> {
    let root = match lab {
        None => current_lab()?.1,
        Some(name) => match current_lab() {
            Ok((cwd_lab, root)) if cwd_lab == name => root,
            _ => root_for(&registry_labs().await?, name)
                .ok_or_else(|| anyhow!("lab \"{name}\" is not running"))?,
        },
    };
    let file = crate::instance::load(&root)?;
    let cfg = file
        .lab
        .machine(machine)
        .ok_or_else(|| anyhow!("lab \"{}\" has no machine `{machine}`", file.lab.name))?;
    Ok(GuestFamily::of_profile(
        effective_profile_name(cfg).as_deref(),
    ))
}

/// Re-execute this binary as the hidden helper, in its own session with
/// null stdio, the way the VNC bridge detaches (`viewer::spawn_detached_bridge`).
fn spawn_helper(args: &RunArgs) -> Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating vmlab binary")?;
    // `setsid` puts the helper in its own session so closing the terminal
    // doesn't SIGHUP it; fall back to a plain spawn if it isn't installed.
    let mut cmd = match which_setsid() {
        Some(setsid) => {
            let mut c = Command::new(setsid);
            c.arg(&exe);
            c
        }
        None => Command::new(&exe),
    };
    cmd.args(args.helper_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning the background app session")?;
    Ok(())
}

fn which_setsid() -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join("setsid"))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Helper {
        #[command(subcommand)]
        verb: Verb,
    }

    #[derive(clap::Subcommand)]
    enum Verb {
        #[command(name = "__appsession")]
        Session(RunArgs),
    }

    /// The detached helper is re-executed with everything the user typed but
    /// `--detach`, and parses back to the same request.
    #[test]
    fn the_detached_helper_gets_the_same_arguments_less_detach() {
        let typed = RunArgs {
            machine: "lab/web".into(),
            via: Some(Mode::X11),
            detach: true,
            cwd: Some("/srv".into()),
            run_as: As {
                user: Some("dev".into()),
                password: Some("pw".into()),
            },
            cmd: vec!["foot".into(), "--hold".into(), "-e".into(), "top".into()],
        };
        let argv = std::iter::once("vmlab".to_string()).chain(typed.helper_args());
        let Verb::Session(parsed) = Helper::parse_from(argv).verb;
        assert_eq!(
            parsed,
            RunArgs {
                detach: false,
                ..typed
            }
        );
    }
}
