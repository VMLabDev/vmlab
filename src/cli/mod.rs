//! CLI surface (PRD §12). The same binary also hosts the supervisor and lab
//! daemons via hidden subcommands, re-exec'd from the CLI as needed.

pub mod console;
pub mod daemon;
pub mod dev;
pub mod lab;
pub mod machine;
pub mod tty_attach;
pub mod validate;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use std::process::ExitCode;

/// Print a daemon payload the way `--json` asks for: pretty, so a human
/// reading a piped file can still follow it.
pub(crate) fn print_json(payload: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(payload)?);
    Ok(())
}

/// Answer a verb the two ways every verb answers: the daemon's payload
/// verbatim under `--json`, or `render`'s reading of it.
///
/// The convention is `vmlab lab list --json`'s, applied uniformly rather than
/// decided per verb — `vmlab osinfo` prints JSON unconditionally and predates
/// it, which is its own compatibility question.
pub(crate) fn emit(
    json: bool,
    payload: &Value,
    render: impl FnOnce(&Value) -> String,
) -> Result<()> {
    if json {
        return print_json(payload);
    }
    print!("{}", render(payload));
    Ok(())
}

/// A boolean as a person reads it, for the reports that tabulate flags.
pub(crate) fn yes_no(flag: bool) -> &'static str {
    if flag { "yes" } else { "no" }
}

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
    /// Machine and segment status: what each machine is doing, and its IP
    Status {
        /// Add the raw power state, readiness, and each machine's
        /// kind-specific detail (template/hardware, image/health/last exit)
        #[arg(short, long)]
        verbose: bool,
    },
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
    /// Ask a machine — VM or container — what it can do and how it is doing
    Machine {
        #[command(subcommand)]
        cmd: MachineCmd,
    },
    /// Read or write a guest's clipboard
    Clipboard {
        #[command(subcommand)]
        cmd: ClipboardCmd,
    },
    /// Print the DNS zones this lab's segments serve
    Dns {
        /// Emit the raw JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Manage running labs host-wide: list / info / stop / restart / destroy
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
    ///
    /// On a machine that declares a `login {}` this stops being SYSTEM/root:
    /// it runs as that login, so writing into `C:\Windows\System32` starts
    /// failing where it used to work. `--user SYSTEM` (or `root`) is the old
    /// behaviour, spelled out (PRD §19.2).
    Exec {
        vm: String,
        /// Seconds to wait for the command to finish
        #[arg(long, value_name = "SECS", default_value_t = 120)]
        timeout: u64,
        #[command(flatten)]
        run_as: As,
        /// Command and arguments (after --)
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Attach an interactive shell inside a VM (the machine's default login,
    /// else SYSTEM/root — over virtio-serial, so it works with no guest
    /// network; Ctrl-] detaches)
    Shell {
        vm: String,
        #[command(flatten)]
        run_as: As,
    },
    /// Attach over the SSH facade: refresh the managed `~/.ssh/config` block,
    /// then hand over to the system `ssh` (PRD §19.7)
    ///
    /// Not a second SSH client — one implementation of the client side, and
    /// it is the one editors already use. Refuses on a stopped machine and
    /// never starts one, like `console` and `exec`.
    Ssh {
        /// [lab/]machine — a bare name inside a lab directory, or the
        /// qualified form from anywhere
        machine: String,
        /// Command and arguments to run instead of a shell (after --)
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Refresh the managed `~/.ssh/config` block for the lab in this
    /// directory (PRD §19.7)
    SshConfig {
        /// Print one machine's stanza and the editor settings snippet, for a
        /// client that will not read the file
        #[arg(long, value_name = "MACHINE")]
        print: Option<String>,
    },
    /// Dev machines: attach to one, and record which one is yours (PRD §19.7)
    ///
    /// Only what is meaningless for a machine that is not `@dev` lives here —
    /// the SSH facade is a general capability, so its verbs are top level.
    Dev {
        #[command(subcommand)]
        cmd: DevCmd,
    },
    /// The `ProxyCommand` target: pipe stdin/stdout onto a machine's SSH
    /// facade (PRD §19.3). Hidden — spawned by `ssh`, never typed.
    #[command(hide = true)]
    SshProxy {
        /// [lab/]machine — the lab-qualified form is what the generated
        /// `ssh_config` block passes, since an editor spawns this from
        /// wherever it happens to be.
        machine: String,
    },
    /// Copy files between host and guest (either side may be <vm>:<path>;
    /// parent directories are created)
    ///
    /// Still the agent identity (SYSTEM/root) even on a machine that
    /// declares a `login {}`, unlike `exec` and `shell`: transfers move onto
    /// the agent's file vocabulary before they can carry a login (PRD
    /// §19.5). Until then a pushed file is owned by SYSTEM/root, not by the
    /// login you would attach as.
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
    /// Print guest OS information, as reported by the guest agent, as JSON
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

/// The dev-machine verbs (PRD §19.7).
#[derive(Subcommand)]
pub enum DevCmd {
    /// Up the dev machine, wait until it is attachable, and become a shell
    /// on it — cold to editing in one command
    ///
    /// It prints the SSH alias and the editor settings snippet, and launches
    /// no editor: pick the alias out of your own client's host list. The
    /// workspace syncer belongs to the lab daemon, so leaving the shell does
    /// not stop it.
    Attach {
        /// Which dev machine (default: $VMLAB_DEV_MACHINE, then the
        /// `vmlab dev use` selection, then the lab's default `@dev` machine)
        machine: Option<String>,
    },
    /// Record which dev machine is yours, in the lab's gitignored `.vmlab/`
    ///
    /// Per-developer by construction: `vmlab.wcl` is committed, so it cannot
    /// say it. `vmlab destroy` forgets the selection.
    Use { machine: String },
    /// The workspace syncer: what it is doing, and what to do about a halt
    /// (PRD §19.6)
    Sync {
        #[command(subcommand)]
        cmd: DevSyncCmd,
    },
}

/// `vmlab dev sync` (PRD §19.6) — reading a workspace syncer, and resolving a
/// halt.
///
/// **Resolution is host-side, necessarily.** ADR-0013's invariant is that the
/// host opens channels and the guest answers, so there is no guest→host control
/// path at all: a `vmlab` inside the dev machine could not call back even if one
/// were shipped. That is why these are typed in the lab directory rather than
/// in the shell `dev attach` drops you into — and why the console shows a halt
/// but offers no button.
#[derive(Subcommand)]
pub enum DevSyncCmd {
    /// What the syncer last decided: halted paths, warnings, and what it
    /// skipped by name
    Status {
        /// Which dev machine (default: $VMLAB_DEV_MACHINE, then the
        /// `vmlab dev use` selection, then the lab's default `@dev` machine)
        machine: Option<String>,
    },
    /// Run a sync pass now and wait for it, rather than for the next edit
    Flush { machine: Option<String> },
    /// Show the guest's copy of a path beside the host's
    ///
    /// The host copy is a directory on this workstation; only the guest's is
    /// behind the seam, which is what this brings across. With no path it
    /// takes every halted one.
    Diff {
        /// Workspace-relative paths (default: every halted path)
        paths: Vec<String>,
        #[arg(long)]
        machine: Option<String>,
    },
    /// Pick which side wins at a halted path, and carry it out
    ///
    /// The copy that loses is overwritten and is not recoverable from vmlab.
    /// Making both sides identical by hand is a third route needing no verb:
    /// the next pass adopts them as agreed.
    Resolve {
        /// Workspace-relative paths (omit with `--all`)
        paths: Vec<String>,
        /// The canonical host copy wins: carry it into the guest
        #[arg(long, conflicts_with = "guest")]
        host: bool,
        /// The guest's working copy wins: carry it onto the canonical copy
        #[arg(long)]
        guest: bool,
        /// Every halted path, as the halt currently stands
        #[arg(long)]
        all: bool,
        #[arg(long)]
        machine: Option<String>,
    },
}

/// Per-VM power control and interaction (PRD §12, §10.3).
#[derive(Subcommand)]
pub enum VmCmd {
    /// Start one VM
    Start { vm: String },
    /// Print a VM's IP address (defaults to the first NIC with a lease)
    Ip {
        vm: String,
        /// Report this NIC's address instead of the first one.
        #[arg(long)]
        nic: Option<usize>,
    },
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
        #[command(flatten)]
        run_as: As,
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
    Shell {
        container: String,
        #[command(flatten)]
        run_as: As,
    },
}

/// Kind-neutral machine queries: a VM and a container answer both (PRD §18).
#[derive(Subcommand)]
pub enum MachineCmd {
    /// What a machine can do beyond the universal commands, probed live: a
    /// display, a console log, in-place reboot, a healthcheck, and whichever
    /// features its agent negotiated
    Capabilities {
        machine: String,
        /// Emit the raw JSON instead of a report
        #[arg(long)]
        json: bool,
    },
    /// Latest guest metrics: CPU, memory and mounted filesystems.
    ///
    /// Reading is not free of side effects: it subscribes the daemon's
    /// sampler, so a machine nothing had asked about starts being sampled.
    Stats {
        machine: String,
        /// Emit the raw JSON instead of a report
        #[arg(long)]
        json: bool,
    },
    /// Push the agent this vmlab ships into a running machine, and mark that
    /// machine diverged.
    ///
    /// A tool, not a policy: the agent normally enters a machine once, when
    /// its template is built, and this changes the running machine in place so
    /// the sealed `agent_version` no longer describes it. Nothing does this by
    /// itself. Rebuilding the template is the other remedy, and the one that
    /// keeps *same template → same machine* true.
    RepairAgent {
        machine: String,
        /// Emit the raw JSON instead of a report
        #[arg(long)]
        json: bool,
    },
}

/// Guest clipboard access over the agent channel (no guest network involved).
#[derive(Subcommand)]
pub enum ClipboardCmd {
    /// Write the guest clipboard to stdout, with no trailing newline added
    Get {
        machine: String,
        /// Emit the raw JSON string instead of the bare text
        #[arg(long)]
        json: bool,
    },
    /// Set the guest clipboard from TEXT, or from stdin when TEXT is omitted.
    ///
    /// Stdin is passed through verbatim, trailing newline included, so
    /// `vmlab clipboard get a | vmlab clipboard set b` round-trips exactly.
    Set {
        machine: String,
        /// The text to copy (omit to read stdin)
        text: Option<String>,
        /// Emit the raw JSON reply instead of a confirmation
        #[arg(long)]
        json: bool,
    },
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

/// Who a person-invoked verb runs as, as the command line spells it — the
/// top rung of PRD §19.2's precedence ladder.
///
/// One value rather than two loose `Option<String>`s because the pair is
/// meaningless split: a password without a user selects nothing, which is
/// why clap requires the one with the other. Every verb that attaches
/// flattens the same pair in, so the two flags read identically wherever
/// they appear.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct As {
    /// Run as this login: the label a `login {}` block declares, or the
    /// account name as an alias. Defaults to the machine's default login;
    /// `SYSTEM` (Windows) or `root` (Linux) is the agent identity (PRD
    /// §19.2)
    #[arg(long, value_name = "LOGIN")]
    pub user: Option<String>,
    /// Password for an account the lab file does not declare, or one whose
    /// declared password has been rotated
    #[arg(long, value_name = "PASSWORD", requires = "user")]
    pub password: Option<String>,
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Up { vms } => lab::cmd_up(vms),
        Command::Pull { vms } => lab::cmd_pull(vms),
        Command::Down { vms, force } => lab::cmd_down(vms, force),
        Command::Destroy => lab::cmd_destroy(),
        Command::Status { verbose } => lab::cmd_status(verbose),
        Command::Validate => validate::cmd_validate().map(|_| ()),
        Command::Vm { cmd } => match cmd {
            VmCmd::Start { vm } => lab::cmd_machine_power(&vm, lab::PowerOp::Start, false),
            VmCmd::Ip { vm, nic } => lab::cmd_machine_ip(&vm, nic),
            VmCmd::Stop { vm, force } => lab::cmd_machine_power(&vm, lab::PowerOp::Stop, force),
            VmCmd::Restart { vm } => lab::cmd_machine_power(&vm, lab::PowerOp::Restart, false),
            VmCmd::Destroy { vm } => lab::cmd_machine_destroy(&vm, "vm"),
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
                lab::cmd_machine_power(&container, lab::PowerOp::Start, false)
            }
            ContainerCmd::Stop { container, force } => {
                lab::cmd_machine_power(&container, lab::PowerOp::Stop, force)
            }
            ContainerCmd::Restart { container } => {
                lab::cmd_machine_power(&container, lab::PowerOp::Restart, false)
            }
            ContainerCmd::Destroy { container } => {
                lab::cmd_machine_destroy(&container, "container")
            }
            ContainerCmd::Exec {
                container,
                timeout,
                run_as,
                cmd,
            } => lab::cmd_container_exec(&container, timeout, cmd, run_as),
            ContainerCmd::Logs {
                container,
                follow,
                lines,
            } => lab::cmd_container_logs(&container, follow, lines),
            ContainerCmd::Ip { container } => lab::cmd_machine_ip(&container, None),
            ContainerCmd::Shell { container, run_as } => {
                lab::cmd_container_shell(&container, run_as)
            }
        },
        Command::Machine { cmd } => match cmd {
            MachineCmd::Capabilities { machine, json } => machine::cmd_capabilities(&machine, json),
            MachineCmd::Stats { machine, json } => machine::cmd_stats(&machine, json),
            MachineCmd::RepairAgent { machine, json } => machine::cmd_repair_agent(&machine, json),
        },
        Command::Clipboard { cmd } => match cmd {
            ClipboardCmd::Get { machine, json } => machine::cmd_clipboard_get(&machine, json),
            ClipboardCmd::Set {
                machine,
                text,
                json,
            } => machine::cmd_clipboard_set(&machine, text, json),
        },
        Command::Dns { json } => lab::cmd_dns(json),
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
        Command::Exec {
            vm,
            timeout,
            run_as,
            cmd,
        } => lab::cmd_exec(&vm, timeout, cmd, run_as),
        Command::Shell { vm, run_as } => lab::cmd_shell(&vm, run_as),
        Command::Ssh { machine, cmd } => lab::cmd_ssh(&machine, cmd),
        Command::SshConfig { print } => lab::cmd_ssh_config(print.as_deref()),
        Command::SshProxy { machine } => lab::cmd_ssh_proxy(&machine),
        Command::Dev { cmd } => match cmd {
            DevCmd::Attach { machine } => dev::cmd_dev_attach(machine),
            DevCmd::Use { machine } => dev::cmd_dev_use(&machine),
            DevCmd::Sync { cmd } => match cmd {
                DevSyncCmd::Status { machine } => dev::cmd_dev_sync_status(machine),
                DevSyncCmd::Flush { machine } => dev::cmd_dev_sync_flush(machine),
                DevSyncCmd::Diff { paths, machine } => dev::cmd_dev_sync_diff(machine, paths),
                DevSyncCmd::Resolve {
                    paths,
                    host,
                    guest,
                    all,
                    machine,
                } => dev::cmd_dev_sync_resolve(machine, paths, host, guest, all),
            },
        },
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
            // A config file's issues render as a rich miette report — the
            // offending text underlined in its source — whichever of the four
            // files they came from (ADR-0006). Lab files arrive already
            // rendered; the other three carry their spans in an `IssueError`.
            // Everything else renders as a plain error chain.
            match err.downcast_ref::<crate::config::block::IssueError>() {
                Some(issues) => eprintln!("{:?}", miette::Report::new(issues.diagnostic())),
                None => eprintln!("{err:?}"),
            }
            exit_code_for(&err)
        }
    }
}

/// The process exit code for a failed verb.
///
/// A daemon failure carries an [`ErrorCode`], so a script can branch on what
/// went wrong without matching on the message. Anything else — a config
/// error, an unreachable daemon, a local IO failure — is the plain failure
/// code the CLI has always used.
fn exit_code_for(err: &anyhow::Error) -> ExitCode {
    match err.downcast_ref::<crate::proto::CommandError>() {
        Some(e) => ExitCode::from(e.code.exit_code()),
        None => ExitCode::FAILURE,
    }
}

fn init_daemon_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::exit_code_for;
    use crate::proto::{CommandError, ErrorCode};

    /// Story 15 of the wire-protocol spec: a script branches on why the verb
    /// failed. That only works if the code survives the `anyhow` the verbs
    /// return, so this drives the real conversion rather than the mapping
    /// alone.
    #[test]
    fn a_daemon_failure_exits_with_its_code() {
        for code in ErrorCode::ALL {
            let err = anyhow::Error::new(CommandError::new(*code, "nope"));
            assert_eq!(
                format!("{:?}", exit_code_for(&err)),
                format!("{:?}", std::process::ExitCode::from(code.exit_code())),
                "{code}"
            );
        }
    }

    /// A local failure — a config error, an unreadable file — has no code and
    /// exits the way the CLI always has.
    #[test]
    fn a_local_failure_exits_one() {
        let err = anyhow::anyhow!("cannot read vmlab.wcl");
        assert_eq!(
            format!("{:?}", exit_code_for(&err)),
            format!("{:?}", std::process::ExitCode::FAILURE),
        );
    }
}
