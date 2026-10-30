//! vmlab-agent — the in-guest agent for vmlab VMs and container micro-VMs.
//!
//! Serves interactive terminals, streaming exec, file operations, tailing,
//! metrics, clipboard and the workspace watch to the host over the
//! `vmlab.agent.0` virtio-serial port, without touching the guest network.
//! See `guest/agent-proto` for the wire contract and `src/mux.rs` for the
//! dispatch core.
//!
//! Runs as a service (systemd on Linux, launchd on macOS, SCM on Windows —
//! installed by the template build) or in the foreground for debugging.

mod app;
mod clipboard;
// What a macOS guest's own answers mean: portable like `rdp`, so the build
// host's tests reach it.
#[cfg(any(target_os = "macos", test))]
mod darwin;
mod exec;
mod fileops;
// The logon cache is portable policy, and both adapters resolve through it:
// what a logon *is* stays with the platform that mints it.
mod logon;
mod metrics;
mod mux;
// What preparing a Windows guest for RDP RemoteApp decides: portable so it
// is tested on any host, compiled only where it is used.
#[cfg(any(windows, test))]
mod rdp;
// That a Windows guest reads the RTC as UTC: portable like `rdp`.
#[cfg(any(windows, test))]
mod rtc;
mod spawn;
mod tail;
mod terminal;
// The recursive tree watch has a backend on Linux (inotify) and Windows
// (ReadDirectoryChangesW) only; elsewhere the mux refuses the open.
#[cfg(any(target_os = "linux", windows))]
mod watch;

#[cfg(test)]
mod app_test;
#[cfg(test)]
mod fake_spawner;
#[cfg(test)]
mod seam_test;
#[cfg(test)]
mod sessions_test;
#[cfg(test)]
mod testutil;

// Linux and macOS share one Unix half; what differs between them lives in
// its `linux` and `macos` submodules.
#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform_impl;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform_impl;

/// Platform functions the portable modules call directly.
pub mod platform {
    pub use crate::platform_impl::{cpu_pct, cpu_sample, disk_sample, kill_process, mem_sample};
}

use std::io::Read;

use vmlab_agent_proto::FrameDecoder;

use crate::mux::{Mux, Platform};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Container-mode entries first: they consume the rest of the argv.
    // Container micro-VMs and process trees are Linux's alone.
    #[cfg(target_os = "linux")]
    match args.first().map(String::as_str) {
        // Exec trampoline for container sessions (see unix::container).
        Some("--nsexec") => unix::container::nsexec_main(&args[1..]),
        // The subreaper an exec in a process tree runs under
        // (see unix::shepherd).
        Some("--shepherd") => unix::shepherd::main(&args[1..]),
        // cinit spawns the agent with the container config it wrote.
        Some("--container") => {
            let Some(config) = args.get(1) else {
                eprintln!("vmlab-agent: --container needs a config path");
                std::process::exit(2);
            };
            run_with(unix::container::new_platform(config));
        }
        _ => {}
    }
    let mut console = false;
    let mut args_iter = args.iter();
    // Not a `for`: the Unix `--port` arm takes its value from the same
    // iterator, which a `for` loop would be holding.
    #[allow(clippy::while_let_on_iterator)]
    while let Some(a) = args_iter.next() {
        match a.as_str() {
            // Unix: serve on a serial device instead of the virtio port —
            // a guest too old for virtio-serial on an `isa-serial` profile
            // (PRD §7.4). Without it the port is found by name.
            #[cfg(unix)]
            "--port" => {
                let Some(path) = args_iter.next() else {
                    eprintln!("vmlab-agent: --port needs a device path");
                    std::process::exit(2);
                };
                unix::set_port_override(std::path::PathBuf::from(path));
            }
            "--daemonize" => {
                #[cfg(unix)]
                unix::daemonize();
            }
            // Windows: skip the SCM dispatcher and run in this console.
            "--console" => console = true,
            // Windows-internal: the user-session clipboard helper the
            // service spawns (see windows/clipboard.rs).
            #[cfg(windows)]
            "--clipboard-helper" => {
                windows::clipboard::helper_main();
            }
            // Undocumented live-test aid: run the shared-app RDP preparation
            // once for an account and print what it changed (PRD §20.3).
            #[cfg(windows)]
            "--rdp-prepare" => rdp_prepare_main(args_iter.as_slice()),
            "--version" => {
                println!("vmlab-agent {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            other => {
                eprintln!("vmlab-agent: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    #[cfg(windows)]
    if !console {
        // Runs `run` under the SCM, or directly when launched from a
        // console (the dispatcher tells the two apart).
        windows::service::dispatch(run);
    }
    let _ = console;
    run();
}

#[cfg(windows)]
fn rdp_prepare_main(args: &[String]) -> ! {
    let [user, secret] = args else {
        eprintln!("vmlab-agent: --rdp-prepare needs <user> <password>");
        std::process::exit(2);
    };
    let logon = vmlab_agent_proto::Logon {
        user: user.clone(),
        secret: secret.clone(),
        elevated: false,
    };
    match windows::rdp::prepare_reporting(&logon) {
        Ok(changed) if changed.is_empty() => println!("rdp-prepare: nothing to change"),
        Ok(changed) => changed.iter().for_each(|c| println!("rdp-prepare: {c}")),
        Err(e) => {
            println!("rdp-prepare: {e}");
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}

fn run() -> ! {
    run_with(platform_impl::new_platform())
}

#[cfg(unix)]
type PlatformImpl = unix::UnixPlatform;
#[cfg(windows)]
type PlatformImpl = windows::WindowsPlatform;

fn run_with(platform: PlatformImpl) -> ! {
    let (mut port_r, port_w) = platform_impl::open_port();
    let mux = Mux::new(port_w);
    #[cfg(windows)]
    platform.start_clipboard(&mux);
    eprintln!(
        "vmlab-agent {} serving on {} (features: {})",
        env!("CARGO_PKG_VERSION"),
        vmlab_agent_proto::PORT_NAME,
        platform.features().join(",")
    );

    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 32 * 1024];
    loop {
        match port_r.read(&mut buf) {
            // EOF: host side detached; it may reconnect.
            Ok(0) => std::thread::sleep(std::time::Duration::from_millis(200)),
            Ok(n) => {
                decoder.push(&buf[..n]);
                while let Some(frame) = decoder.next_frame() {
                    mux.handle_frame(frame, &platform);
                }
            }
            // A read that fails is a port connection being reset (the
            // Windows adapter poisons one whose write stalled; the reader
            // reopens it on its next pass). Bytes were lost on it, so every
            // session dies with it, and the host is told on each.
            Err(e) => {
                eprintln!("vmlab-agent: port read failed: {e}");
                mux.reset(&format!("port connection reset ({e})"));
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}
