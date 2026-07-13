//! vmlab-agent — the in-guest agent for vmlab VMs and container micro-VMs.
//!
//! Serves interactive terminals, streaming exec, file transfer, tailing,
//! metrics and clipboard to the host over the `vmlab.agent.0` virtio-serial
//! port. No guest network involved anywhere. See `guest/agent-proto` for the
//! wire contract and `src/mux.rs` for the dispatch core.
//!
//! Runs as a service (systemd on Linux, SCM on Windows — installed by the
//! template build) or in the foreground for debugging.

mod exec;
mod files;
mod metrics;
mod mux;
mod tail;

#[cfg(test)]
mod sessions_test;
#[cfg(test)]
mod testutil;

#[cfg(unix)]
mod linux;
#[cfg(unix)]
use linux as platform_impl;

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
    for a in &args {
        match a.as_str() {
            "--daemonize" => {
                #[cfg(unix)]
                linux::daemonize();
            }
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
    run();
}

fn run() -> ! {
    let platform = platform_impl::new_platform();
    let (mut port_r, port_w) = platform_impl::open_port();
    let mux = Mux::new(port_w);
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
            Err(e) => {
                eprintln!("vmlab-agent: port read failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}
