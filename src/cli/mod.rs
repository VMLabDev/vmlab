//! CLI surface (PRD §12). The same binary also hosts the supervisor and lab
//! daemons via hidden subcommands, re-exec'd from the CLI as needed.

pub mod daemon;
mod lab;
mod validate;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "vmlab", version, about = "Single-host VM lab orchestrator")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create/start the lab (or a subset of VMs), run provision scripts
    Up {
        /// VMs to bring up (default: all)
        vms: Vec<String>,
    },
    /// Graceful stop; clones retained
    Down {
        /// VMs to stop (default: all)
        vms: Vec<String>,
        /// Hard kill instead of the graceful ladder
        #[arg(long)]
        force: bool,
    },
    /// Stop the lab and delete clones, lab-local state, dynamic net config
    Destroy,
    /// Lab/VM/segment state, IPs, ready flags
    Status,
    /// Validate the lab file with no side effects
    Validate,
    /// Start one VM
    Start { vm: String },
    /// Stop one VM (graceful ladder; --force to kill)
    Stop {
        vm: String,
        #[arg(long)]
        force: bool,
    },
    /// Restart one VM
    Restart { vm: String },
    /// Take a snapshot of one VM, or lab-wide with no VM
    Snapshot {
        /// Snapshot name
        name: String,
        /// VM ([lab/]vm); omitted = every VM in the lab
        #[arg(long)]
        vm: Option<String>,
    },
    /// Restore a snapshot (resumes running iff it was taken online)
    Restore {
        /// Snapshot name
        name: String,
        /// VM ([lab/]vm); omitted = every VM in the lab
        #[arg(long)]
        vm: Option<String>,
    },
    /// List a VM's snapshots
    Snapshots { vm: String },
    /// Delete a VM snapshot
    SnapshotDelete { vm: String, name: String },
    /// Run an ad-hoc wisp script against the current lab
    Run {
        /// Script path, relative to the lab root
        script: String,
    },
    /// Write the wisp interface file (LSP support for lab scripts)
    Wispi {
        /// Output path
        #[arg(default_value = "vmlab.wispi")]
        out: std::path::PathBuf,
    },
    /// Run a command in the guest via the agent
    Exec {
        vm: String,
        /// Command and arguments (after --)
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Tail or dump JSON-line logs for the lab or one VM
    Logs {
        /// [lab/][vm] (default: the cwd's lab)
        target: Option<String>,
        /// Keep following
        #[arg(short, long)]
        follow: bool,
        /// Lines of history to show
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Supervisor control (normally automatic)
    Daemon {
        #[command(subcommand)]
        cmd: daemon::DaemonCmd,
    },
    /// Internal: run the supervisor daemon in the foreground
    #[command(name = "__supervisord", hide = true)]
    Supervisord,
    /// Internal: run a lab daemon in the foreground
    #[command(name = "__labd", hide = true)]
    Labd {
        /// Lab name
        #[arg(long)]
        lab: String,
        /// Directory containing vmlab.wcl
        #[arg(long)]
        root: std::path::PathBuf,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Up { vms } => lab::cmd_up(vms),
        Command::Down { vms, force } => lab::cmd_down(vms, force),
        Command::Destroy => lab::cmd_destroy(),
        Command::Status => lab::cmd_status(),
        Command::Validate => validate::cmd_validate().map(|_| ()),
        Command::Start { vm } => lab::cmd_vm_power(&vm, "start", false),
        Command::Stop { vm, force } => lab::cmd_vm_power(&vm, "stop", force),
        Command::Restart { vm } => lab::cmd_vm_power(&vm, "restart", false),
        Command::Snapshot { name, vm } => lab::cmd_snapshot(vm, name),
        Command::Restore { name, vm } => lab::cmd_restore(vm, name),
        Command::Snapshots { vm } => lab::cmd_snapshots(&vm),
        Command::SnapshotDelete { vm, name } => lab::cmd_snapshot_delete(&vm, name),
        Command::Run { script } => lab::cmd_run(&script),
        Command::Wispi { out } => crate::scripting::write_interface(&out)
            .map_err(anyhow::Error::from)
            .map(|()| println!("wrote {}", out.display())),
        Command::Exec { vm, cmd } => lab::cmd_exec(&vm, cmd),
        Command::Logs {
            target,
            follow,
            lines,
        } => lab::cmd_logs(target, follow, lines),
        Command::Daemon { cmd } => daemon::cmd_daemon(cmd),
        Command::Supervisord => {
            init_daemon_tracing();
            crate::supervisor::run()
        }
        Command::Labd { lab, root } => {
            init_daemon_tracing();
            crate::labd::run(lab, root)
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // ConfigErrors render as rich miette reports; everything else as
            // a plain error chain.
            eprintln!("{err:?}");
            ExitCode::FAILURE
        }
    }
}

fn init_daemon_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();
}
