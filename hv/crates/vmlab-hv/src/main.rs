//! `vmlab-hv --machine <machine.json> --control <control.sock> [--paused]`
//!
//! One process per machine. Exit status: 0 on guest-initiated or requested
//! clean shutdown (S5, guest reset, `quit`). Other outcomes and reports are
//! documented in docs/diagnostics.md. The control socket is bound before vCPUs start.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vmlab_hv_proto::MachineDescription;
use vmlab_hv_proto::control::ProcessExitReason;
use vmlab_hv_vmm::{VmHandle, builder, control, diagnostics, vm::ExitReason};

use crate::devices::DeviceSet;

const USAGE: &str = "usage: vmlab-hv --machine <machine.json> --control <control.sock> [--paused]\n       vmlab-hv --probe";

enum Invocation {
    Run(Args),
    Probe,
}

struct Args {
    machine: PathBuf,
    control: PathBuf,
    paused: bool,
}

fn parse_args(argv: impl Iterator<Item = String>) -> Result<Invocation, String> {
    let mut machine = None;
    let mut control = None;
    let mut paused = false;
    let mut probe = false;
    let mut argv = argv;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--machine" => machine = Some(next_value(&mut argv, &arg)?),
            "--control" => control = Some(next_value(&mut argv, &arg)?),
            "--paused" => paused = true,
            "--probe" => probe = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                log::info!("process exit: help; exit_code=0");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if probe {
        if machine.is_some() || control.is_some() || paused {
            return Err("--probe cannot be combined with machine arguments".into());
        }
        return Ok(Invocation::Probe);
    }
    Ok(Invocation::Run(Args {
        machine: machine.ok_or("missing --machine")?,
        control: control.ok_or("missing --control")?,
        paused,
    }))
}

fn next_value(argv: &mut impl Iterator<Item = String>, flag: &str) -> Result<PathBuf, String> {
    argv.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod args_tests {
    use super::*;

    #[test]
    fn probe_needs_no_machine_and_refuses_launch_arguments() {
        let parse = |args: &[&str]| parse_args(args.iter().map(|s| s.to_string()));
        assert!(matches!(parse(&["--probe"]), Ok(Invocation::Probe)));
        for args in [
            vec!["--probe", "--paused"],
            vec!["--probe", "--machine", "unused"],
            vec!["--probe", "--control", "unused"],
            vec!["--probe", "--unknown"],
            vec![],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert!(matches!(
            parse(&[
                "--machine",
                "machine.json",
                "--control",
                "control.sock",
                "--paused"
            ]),
            Ok(Invocation::Run(Args { paused: true, .. }))
        ));
    }
}

#[cfg(test)]
mod a20_acceptance;
#[cfg(test)]
mod acceptance;
mod devices;
#[cfg(test)]
mod display_acceptance;
#[cfg(test)]
mod macos_acceptance;
#[cfg(test)]
mod macos_hwref;
#[cfg(test)]
mod pc_acceptance;
#[cfg(test)]
mod pc_hwref;

/// Raise the soft descriptor limit to the hard one. A guest GPU resource
/// holds host descriptors for as long as the guest keeps it, and a guest may
/// keep thousands: the usual soft limit of 1024 ends the VM under a Windows
/// desktop that has run a few 3D programs. Returns the limit in force.
fn raise_descriptor_limit() -> std::io::Result<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only the supplied structure.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if limit.rlim_cur < limit.rlim_max {
        let raised = libc::rlimit {
            rlim_cur: limit.rlim_max,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: setrlimit reads the supplied structure and changes only
        // this process's soft limit, which never exceeds the hard one.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        limit = raised;
    }
    Ok(limit.rlim_cur)
}

#[cfg(test)]
mod descriptor_limit_tests {
    #[test]
    fn the_soft_descriptor_limit_becomes_the_hard_one() {
        let read = || {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: getrlimit writes only the supplied structure.
            let status = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
            assert_eq!(status, 0);
            limit
        };
        let hard = read().rlim_max;
        assert_eq!(super::raise_descriptor_limit().unwrap(), hard);
        assert_eq!(read().rlim_cur, hard);
        assert_eq!(read().rlim_max, hard);
        // A second call has nothing to raise.
        assert_eq!(super::raise_descriptor_limit().unwrap(), hard);
    }
}

fn main() -> ExitCode {
    if let Err(message) = diagnostics::report::init_logging() {
        // Logging cannot report its own initialization failure through the facade.
        eprintln!(
            "vmlab-hv: logging initialization: {message}; exit_reason=startup_error exit_code=2"
        );
        return ExitCode::from(ProcessExitReason::StartupError.exit_code());
    }
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Invocation::Run(args)) => args,
        Ok(Invocation::Probe) => {
            return match vmlab_hv_vmm::host::probe() {
                Ok(facts) => {
                    println!(
                        "{}",
                        serde_json::to_string(&facts).expect("serialize host facts")
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    log::error!("host probe: {error}; exit_reason=startup_error exit_code=2");
                    ExitCode::from(ProcessExitReason::StartupError.exit_code())
                }
            };
        }
        Err(message) => {
            log::error!("{message}\n{USAGE}; exit_reason=startup_error exit_code=2");
            return ExitCode::from(ProcessExitReason::StartupError.exit_code());
        }
    };

    if let Err(error) = raise_descriptor_limit() {
        // The VM still runs: the GPU bounds its resources by the limit in force.
        log::warn!("raising the descriptor limit: {error}");
    }

    let desc = match read_machine(&args.machine, &args.control) {
        Ok(desc) => desc,
        Err(message) => {
            log::error!("{message}; exit_reason=startup_error exit_code=2");
            diagnostics::signal::cleanup();
            return ExitCode::from(ProcessExitReason::StartupError.exit_code());
        }
    };

    let (reason, errors) = match run(desc, args) {
        Ok(reason) => (reason.process_reason(), reason.error_chain()),
        Err(message) => (
            ProcessExitReason::StartupError,
            vec![diagnostics::report::scrub(&message)],
        ),
    };
    let code = reason.exit_code();
    if code == 0 {
        log::info!("process exit: {reason:?}; exit_code={code}");
    } else {
        log::error!("process exit: {reason:?}; exit_code={code}; error_chain={errors:?}");
    }
    let report = if matches!(
        reason,
        ProcessExitReason::RuntimeError | ProcessExitReason::GuestPanic
    ) || std::env::var_os("VMLAB_HV_DIAGNOSTICS").is_some()
    {
        Some(diagnostics::report::write_report(reason, errors, None))
    } else {
        None
    };
    // No worker or teardown log may follow the report path on stderr.
    diagnostics::signal::cleanup();
    if let Some(report) = report {
        diagnostics::report::finish_report(report);
    }
    ExitCode::from(code)
}

fn read_machine(path: &PathBuf, control: &std::path::Path) -> Result<MachineDescription, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    // Syntax errors report only location, never the input line or a secret value.
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        format!(
            "parsing {}: invalid JSON at line {} column {}",
            path.display(),
            e.line(),
            e.column()
        )
    })?;
    diagnostics::report::configure(control, &value)?;
    // Deserializer errors can include input values, including secrets, so only
    // the location is reported.
    serde_json::from_str(&text).map_err(|error| {
        format!(
            "parsing {}: invalid machine description at line {}, column {}",
            path.display(),
            error.line(),
            error.column()
        )
    })
}

fn check_socket_path(path: &Path) -> Result<(), String> {
    const MAX_BYTES: usize = 107; // Linux sun_path also needs a terminating NUL.
    let bytes = path.as_os_str().len();
    if bytes > MAX_BYTES {
        return Err(format!(
            "Unix socket path {} is {bytes} bytes; the limit is {MAX_BYTES} bytes",
            path.display()
        ));
    }
    Ok(())
}

fn run(desc: MachineDescription, args: Args) -> Result<ExitReason, String> {
    // sigwait requires termination signals blocked before workers inherit the mask.
    block_termination_signals()?;
    log::info!(
        "vmlab-hv {}: building machine {:?}, {} vCPUs, {} MiB",
        env!("CARGO_PKG_VERSION"),
        desc.name,
        desc.vcpus,
        desc.memory_mib
    );

    check_socket_path(&args.control)?;
    let display_paths = desc.display.as_ref().map(|display| {
        let directory = args.control.parent().unwrap_or(Path::new("."));
        let native = display
            .socket
            .clone()
            .unwrap_or_else(|| directory.join("display.sock"));
        let rfb = display
            .vnc_socket
            .clone()
            .unwrap_or_else(|| directory.join("vnc.sock"));
        (native, rfb)
    });
    if let Some((native, rfb)) = &display_paths {
        check_socket_path(native)?;
        check_socket_path(rfb)?;
    }

    // Refuse an occupied control path before doing any work, so a stale
    // socket or a second launch fails fast and leaves the path alone.
    if args.control.exists() {
        return Err(format!(
            "control socket: {} already exists",
            args.control.display()
        ));
    }

    if !args.paused
        && desc.nics.iter().any(|nic| {
            matches!(
                nic.attachment,
                vmlab_hv_proto::machine::NicAttachment::Tap { .. }
                    | vmlab_hv_proto::machine::NicAttachment::TapRaw { .. }
            )
        })
    {
        return Err("TAP attachments require --paused and net_attach before resume".into());
    }
    let devices = DeviceSet::default();
    let mut vm = builder::build(&desc, &devices, true)
        .map_err(|e| format!("building machine `{}`: {e}", desc.name))?;
    vm.attach_diagnostics();
    let handle = vm.handle();

    // Bind every served socket before the vCPUs run.
    if let Some((native, rfb)) = display_paths {
        handle
            .serve_display(&native, &rfb)
            .map_err(|e| e.to_string())?;
    }
    let control = control::serve_with_network(&args.control, &handle, devices.network.clone())
        .map_err(|e| format!("control socket: {e}"))?;

    diagnostics::signal::checkpoint()?;

    let signal_waiter = spawn_signal_waiter(handle.clone())?;

    // A low-frequency checkpoint is prepared outside all signal handlers.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let checkpoint = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("diagnostic-checkpoint".into())
            .spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    if let Err(error) = diagnostics::signal::checkpoint() {
                        log::warn!("fatal checkpoint: {error}");
                    }
                    std::thread::park_timeout(std::time::Duration::from_secs(1));
                }
            })
            .map_err(|e| format!("spawning diagnostic checkpoint: {e}"))?
    };

    if !args.paused {
        vm.start();
    }

    handle.wait_exit();
    // Stop checkpoint readers before taking the final joined register sample.
    stop.store(true, std::sync::atomic::Ordering::Release);
    checkpoint.thread().unpark();
    checkpoint
        .join()
        .map_err(|_| "diagnostic checkpoint panicked".to_string())?;
    let reason = vm.wait();
    // Wake an idle sigwait without inventing another exit reason. The state
    // is already ShuttingDown, so the waiter returns without logging a signal.
    if !signal_waiter.is_finished() {
        use std::os::unix::thread::JoinHandleExt;
        // SAFETY: this unjoined thread's pthread id is valid and SIGTERM is blocked.
        unsafe {
            libc::pthread_kill(signal_waiter.as_pthread_t(), libc::SIGTERM);
        }
    }
    signal_waiter
        .join()
        .map_err(|_| "signal waiter panicked".to_string())?;
    control
        .join()
        .map_err(|_| "control server panicked".to_string())?;
    Ok(reason)
}

fn block_termination_signals() -> Result<(), String> {
    // SAFETY: pthread_sigmask changes only the calling thread's mask.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGQUIT);
        libc::sigaddset(&mut set, libc::SIGINT);
        let status = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if status != 0 {
            return Err(format!(
                "cannot block termination signals: {}",
                std::io::Error::from_raw_os_error(status)
            ));
        }
    }
    Ok(())
}

fn spawn_signal_waiter(handle: VmHandle) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("signal-wait".into())
        .spawn(move || {
            // SAFETY: sigwait on the set blocked process-wide at startup.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTERM);
                libc::sigaddset(&mut set, libc::SIGQUIT);
                libc::sigaddset(&mut set, libc::SIGINT);
                let mut sig: libc::c_int = 0;
                let status = libc::sigwait(&set, &mut sig);
                if handle.status() == vmlab_hv_proto::control::RunState::ShuttingDown {
                    return;
                }
                if status == 0 {
                    log::info!("caught signal {sig}, stopping the machine");
                    handle.host_signal(sig);
                } else {
                    handle.runtime_error(format!(
                        "sigwait: {}",
                        std::io::Error::from_raw_os_error(status)
                    ));
                }
            }
        })
        .map_err(|e| format!("spawning signal waiter: {e}"))
}
