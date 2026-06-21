//! CLI surface (PRD §12). The same binary also hosts the supervisor and lab
//! daemons via hidden subcommands, re-exec'd from the CLI as needed.

pub mod console;
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
    /// Per-VM power control and interaction: start/stop, screenshot, input, OCR
    Vm {
        #[command(subcommand)]
        cmd: VmCmd,
    },
    /// Manage running labs host-wide: list / info / stop / destroy
    Lab {
        #[command(subcommand)]
        cmd: lab::LabCmd,
    },
    /// Take, restore, list, and delete VM/lab snapshots
    Snapshot {
        #[command(subcommand)]
        cmd: SnapshotCmd,
    },
    /// Manage the template store and OCI distribution
    Template {
        #[command(subcommand)]
        cmd: crate::template::cli::TemplateCmd,
    },
    /// Attach a console viewer to a VM
    Console {
        vm: String,
        /// Forward the VNC display over TCP instead of launching a viewer
        #[arg(long)]
        tcp: bool,
    },
    /// Run an ad-hoc wisp script against the current lab
    Script {
        /// Script path, relative to the lab root
        script: String,
    },
    /// Internal: write the wisp interface file (LSP support for lab scripts)
    #[command(hide = true)]
    Wispi {
        /// Output path
        #[arg(default_value = "vmlab.wispi")]
        out: std::path::PathBuf,
    },
    /// Run a command in the guest via the agent
    Exec {
        vm: String,
        /// Seconds to wait for the command to finish
        #[arg(long, value_name = "SECS", default_value_t = 120)]
        timeout: u64,
        /// Command and arguments (after --)
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Copy a host file or directory tree into a guest via the agent
    Cp {
        /// Source path on the host
        src: String,
        /// Destination as <vm>:<path> (parent directories are created)
        dest: String,
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
    #[command(hide = true)]
    Daemon {
        #[command(subcommand)]
        cmd: daemon::DaemonCmd,
    },
    /// Internal: hold a backgrounded console's VNC bridge + viewer
    #[command(name = "__vncbridge", hide = true)]
    Vncbridge {
        #[arg(long)]
        lab: String,
        #[arg(long)]
        vm: String,
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

/// Per-VM power control and interaction (PRD §12, §10.3).
#[derive(Subcommand)]
pub enum VmCmd {
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
    /// Destroy one VM: stop it and delete its clone (config retained)
    Destroy { vm: String },
    /// Capture a running VM's screen to a PNG file
    Screenshot {
        vm: String,
        /// Output PNG path
        path: String,
    },
    /// Send a key chord (e.g. ctrl-alt-delete)
    Sendkeys { vm: String, chord: String },
    /// Move the mouse pointer to absolute screen coordinates
    MouseMove { vm: String, x: i64, y: i64 },
    /// Click a mouse button, optionally first moving to x,y
    Click {
        vm: String,
        /// Move here before clicking (omit to click at the current position)
        x: Option<i64>,
        y: Option<i64>,
        /// Button to click
        #[arg(long, default_value = "left", value_parser = ["left", "right", "middle"])]
        button: String,
    },
    /// Press, drag from x1,y1 to x2,y2, and release the left button
    Drag {
        vm: String,
        x1: i64,
        y1: i64,
        x2: i64,
        y2: i64,
    },
    /// OCR the screen (optionally a region)
    Ocr {
        vm: String,
        /// Restrict to a region: x y w h
        #[arg(long, num_args = 4, value_names = ["X", "Y", "W", "H"])]
        region: Option<Vec<i64>>,
    },
    /// Search the screen for a template image
    FindImage {
        vm: String,
        /// Template image path (PNG/PPM)
        image: String,
        /// Match threshold 0.0–1.0
        #[arg(long, default_value_t = 0.9)]
        threshold: f64,
        /// Restrict the search to a region: x y w h
        #[arg(long, num_args = 4, value_names = ["X", "Y", "W", "H"])]
        region: Option<Vec<i64>>,
    },
}

/// Snapshot management (PRD §7.3).
#[derive(Subcommand)]
pub enum SnapshotCmd {
    /// Take a snapshot of one VM, or lab-wide with no --vm
    Create {
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
    List { vm: String },
    /// Delete a VM snapshot
    Delete { vm: String, name: String },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Up { vms } => lab::cmd_up(vms),
        Command::Down { vms, force } => lab::cmd_down(vms, force),
        Command::Destroy => lab::cmd_destroy(),
        Command::Status => lab::cmd_status(),
        Command::Validate => validate::cmd_validate().map(|_| ()),
        Command::Vm { cmd } => match cmd {
            VmCmd::Start { vm } => lab::cmd_vm_power(&vm, "start", false),
            VmCmd::Stop { vm, force } => lab::cmd_vm_power(&vm, "stop", force),
            VmCmd::Restart { vm } => lab::cmd_vm_power(&vm, "restart", false),
            VmCmd::Destroy { vm } => lab::cmd_vm_destroy(&vm),
            VmCmd::Screenshot { vm, path } => lab::cmd_vm_screenshot(&vm, &path),
            VmCmd::Sendkeys { vm, chord } => lab::cmd_vm_sendkeys(&vm, &chord),
            VmCmd::MouseMove { vm, x, y } => lab::cmd_vm_mouse_move(&vm, x, y),
            VmCmd::Click { vm, x, y, button } => lab::cmd_vm_click(&vm, x, y, &button),
            VmCmd::Drag { vm, x1, y1, x2, y2 } => lab::cmd_vm_drag(&vm, x1, y1, x2, y2),
            VmCmd::Ocr { vm, region } => lab::cmd_vm_ocr(&vm, region),
            VmCmd::FindImage {
                vm,
                image,
                threshold,
                region,
            } => lab::cmd_vm_find_image(&vm, &image, threshold, region),
        },
        Command::Lab { cmd } => lab::cmd_lab(cmd),
        Command::Snapshot { cmd } => match cmd {
            SnapshotCmd::Create { name, vm } => lab::cmd_snapshot(vm, name),
            SnapshotCmd::Restore { name, vm } => lab::cmd_restore(vm, name),
            SnapshotCmd::List { vm } => lab::cmd_snapshots(&vm),
            SnapshotCmd::Delete { vm, name } => lab::cmd_snapshot_delete(&vm, name),
        },
        Command::Template { cmd } => crate::template::cli::cmd_template(cmd),
        Command::Console { vm, tcp } => console::cmd_console(&vm, tcp),
        Command::Vncbridge { lab, vm } => console::run_bridge(lab, vm),
        Command::Script { script } => lab::cmd_run(&script),
        Command::Wispi { out } => crate::scripting::write_interface(&out)
            .map_err(anyhow::Error::from)
            .map(|()| println!("wrote {}", out.display())),
        Command::Exec { vm, timeout, cmd } => lab::cmd_exec(&vm, timeout, cmd),
        Command::Cp { src, dest } => lab::cmd_cp(&src, &dest),
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
