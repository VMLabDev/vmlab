//! `vmlab-fastpath`: the one program that holds CAP_BPF and CAP_NET_ADMIN
//! (F21). Each daemon that selects a kernel tier starts its own helper with
//! a `SOCK_SEQPACKET` socket as stdin and asks it, over the closed request
//! vocabulary in [`proto`], to create taps, load and attach vmlab's two
//! embedded BPF objects and write their maps. The helper serves exactly
//! that connection and exits when the daemon closes it, so the BPF objects
//! live as long as the daemon does. One thing outlives both: the XDP
//! program on a tap stays attached until the tap is destroyed, because a
//! tap lives in every descriptor on it (a hypervisor holds one), and a tap
//! without its program is an open door into the host's network stack.
//! Nothing else in vmlab needs, or is granted, a capability.

pub(crate) mod client;
pub(crate) mod proto;
pub(crate) mod server;

pub use client::HELPER_NAME;

use std::os::fd::AsFd;

/// The helper binary's whole `main`.
pub fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    const USAGE: &str = "vmlab-fastpath is started by the vmlab daemons, over a socket on its \
                         standard input; it takes no arguments";
    if std::env::args_os().len() > 1 {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let sock = match std::io::stdin().as_fd().try_clone_to_owned() {
        Ok(sock) => sock,
        Err(error) => {
            eprintln!("vmlab-fastpath: standard input: {error}");
            return ExitCode::from(2);
        }
    };
    let seqpacket = matches!(
        rustix::net::sockopt::socket_domain(&sock),
        Ok(rustix::net::AddressFamily::UNIX)
    ) && matches!(
        rustix::net::sockopt::socket_type(&sock),
        Ok(rustix::net::SocketType::SEQPACKET)
    );
    if !seqpacket {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    match server::serve(sock, server::Aya) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vmlab-fastpath: {error}");
            ExitCode::FAILURE
        }
    }
}
