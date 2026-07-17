//! CLI surface (PRD §12). The same binary also hosts the supervisor and lab
//! daemons via hidden subcommands, re-exec'd from the CLI as needed.

pub mod console;
pub mod daemon;
pub mod lab;
pub mod tty_attach;
pub mod validate;

use clap::{Parser, Subcommand, ValueEnum};
use std::process::ExitCode;

/// How `vmlab logs` renders its output.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable, terminal-rendered (colorized on a TTY)
    #[default]
    Pretty,
    /// Raw JSON-lines, one event per line
    Jsonl,
}

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
    /// Download missing registry templates/images without starting anything
    Pull {
        /// Machines to pull for (default: all)
        vms: Vec<String>,
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
    /// Per-container lifecycle and interaction: start/stop, exec, logs, IP
    Container {
        #[command(subcommand)]
        cmd: ContainerCmd,
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
    /// Run config-weave playbooks against lab machines
    Playbook {
        #[command(subcommand)]
        cmd: PlaybookCmd,
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
    /// Run an ad-hoc wscript script against the current lab
    Script {
        /// Script path, relative to the lab root
        script: String,
    },
    /// Internal: write the wscript interface file (LSP support for lab scripts)
    #[command(hide = true)]
    Wscripti {
        /// Output path
        #[arg(default_value = "vmlab.wscripti")]
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
    /// Attach an interactive shell inside a VM (root / SYSTEM PowerShell
    /// over virtio-serial — works with no guest network; Ctrl-] detaches)
    Shell { vm: String },
    /// Copy files between host and guest (either side may be <vm>:<path>;
    /// parent directories are created)
    Cp {
        /// Source: a host path, or <vm>:<path> to pull from the guest
        src: String,
        /// Destination: <vm>:<path> to push, or a host path when pulling
        dest: String,
    },
    /// Follow a file inside a guest (tail -F over the agent channel)
    Tail {
        vm: String,
        /// Guest file path
        path: String,
    },
    /// Follow the Windows event log of a guest
    Eventlog {
        vm: String,
        /// XPath filter (default: everything on the System channel)
        #[arg(long)]
        filter: Option<String>,
    },
    /// Print guest OS information (guest-get-osinfo) as JSON
    Osinfo { vm: String },
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
        /// Output format
        #[arg(short = 'o', long = "output", value_enum, default_value_t = LogFormat::Pretty)]
        output: LogFormat,
    },
    /// Show which network fast-path tier is active (and why others are not)
    Fastpath,
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

/// Per-container lifecycle and interaction (PRD §16).
#[derive(Subcommand)]
pub enum ContainerCmd {
    /// Start one container
    Start { container: String },
    /// Stop one container (graceful ladder; --force to kill)
    Stop {
        container: String,
        #[arg(long)]
        force: bool,
    },
    /// Restart one container
    Restart { container: String },
    /// Destroy one container: stop it and delete its scratch state (config retained)
    Destroy { container: String },
    /// Run a command inside the container via the agent
    Exec {
        container: String,
        /// Seconds to wait for the command to finish
        #[arg(long, value_name = "SECS", default_value_t = 120)]
        timeout: u64,
        /// Command and arguments (after --)
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Tail or dump a container's console log (kernel + stdout/stderr)
    Logs {
        container: String,
        /// Keep following
        #[arg(short, long)]
        follow: bool,
        /// Lines of history to show
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Print a container's IP address
    Ip { container: String },
    /// Attach an interactive shell inside the container (Ctrl-] to detach)
    Shell { container: String },
}

/// Snapshot management (PRD §7.3; containers snapshot identically, §18).
#[derive(Subcommand)]
pub enum SnapshotCmd {
    /// Take a snapshot of one VM/container, or lab-wide with no --vm
    Create {
        /// Snapshot name
        name: String,
        /// Machine ([lab/]name); omitted = every VM and container in the lab
        #[arg(long)]
        vm: Option<String>,
    },
    /// Restore a snapshot (resumes running iff it was taken online)
    Restore {
        /// Snapshot name
        name: String,
        /// Machine ([lab/]name); omitted = every VM and container in the lab
        #[arg(long)]
        vm: Option<String>,
    },
    /// List a VM's/container's snapshots
    List { vm: String },
    /// Delete a VM/container snapshot
    Delete { vm: String, name: String },
}

/// config-weave playbook runs (declared with `playbook {}` lab blocks).
/// Exit codes mirror config-weave: 0 ok, 1 step error, 2 validation,
/// 3 reboot still required after bounded retries.
#[derive(Subcommand)]
pub enum PlaybookCmd {
    /// List the lab's playbook blocks and any in-flight runs
    List,
    /// Report drift without changing the guest (re-pushes the playbook first)
    Check {
        /// Machine ([lab/]name)
        machine: String,
        /// Playbook folder path, when several target this machine
        #[arg(long)]
        playbook: Option<String>,
        /// Play name, when several target this machine
        #[arg(long)]
        play: Option<String>,
    },
    /// Push the playbook and converge the guest (auto-reboots on demand)
    Apply {
        /// Machine ([lab/]name)
        machine: String,
        /// Playbook folder path, when several target this machine
        #[arg(long)]
        playbook: Option<String>,
        /// Play name, when several target this machine
        #[arg(long)]
        play: Option<String>,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Up { vms } => lab::cmd_up(vms),
        Command::Pull { vms } => lab::cmd_pull(vms),
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
        Command::Container { cmd } => match cmd {
            ContainerCmd::Start { container } => {
                lab::cmd_container_power(&container, "start", false)
            }
            ContainerCmd::Stop { container, force } => {
                lab::cmd_container_power(&container, "stop", force)
            }
            ContainerCmd::Restart { container } => {
                lab::cmd_container_power(&container, "restart", false)
            }
            ContainerCmd::Destroy { container } => lab::cmd_container_destroy(&container),
            ContainerCmd::Exec {
                container,
                timeout,
                cmd,
            } => lab::cmd_container_exec(&container, timeout, cmd),
            ContainerCmd::Logs {
                container,
                follow,
                lines,
            } => lab::cmd_container_logs(&container, follow, lines),
            ContainerCmd::Ip { container } => lab::cmd_container_ip(&container),
            ContainerCmd::Shell { container } => lab::cmd_container_shell(&container),
        },
        Command::Lab { cmd } => lab::cmd_lab(cmd),
        Command::Snapshot { cmd } => match cmd {
            SnapshotCmd::Create { name, vm } => lab::cmd_snapshot(vm, name),
            SnapshotCmd::Restore { name, vm } => lab::cmd_restore(vm, name),
            SnapshotCmd::List { vm } => lab::cmd_snapshots(&vm),
            SnapshotCmd::Delete { vm, name } => lab::cmd_snapshot_delete(&vm, name),
        },
        Command::Playbook { cmd } => match cmd {
            PlaybookCmd::List => lab::cmd_playbook_list(),
            PlaybookCmd::Check {
                machine,
                playbook,
                play,
            } => lab::cmd_playbook_run(&machine, playbook, play, false),
            PlaybookCmd::Apply {
                machine,
                playbook,
                play,
            } => lab::cmd_playbook_run(&machine, playbook, play, true),
        },
        Command::Template { cmd } => crate::template::cli::cmd_template(cmd),
        Command::Console { vm, tcp } => console::cmd_console(&vm, tcp),
        Command::Vncbridge { lab, vm } => console::run_bridge(lab, vm),
        Command::Script { script } => lab::cmd_run(&script),
        Command::Wscripti { out } => crate::scripting::write_interface(&out)
            .map_err(anyhow::Error::from)
            .map(|()| println!("wrote {}", out.display())),
        Command::Exec { vm, timeout, cmd } => lab::cmd_exec(&vm, timeout, cmd),
        Command::Shell { vm } => lab::cmd_shell(&vm),
        Command::Cp { src, dest } => lab::cmd_cp(&src, &dest),
        Command::Tail { vm, path } => lab::cmd_tail(&vm, &path),
        Command::Eventlog { vm, filter } => lab::cmd_eventlog(&vm, filter.as_deref()),
        Command::Osinfo { vm } => lab::cmd_osinfo(&vm),
        Command::Logs {
            target,
            follow,
            lines,
            output,
        } => lab::cmd_logs(target, follow, lines, output),
        Command::Fastpath => daemon::cmd_fastpath(),
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
