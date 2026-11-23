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
    if let Some(refusal) = caller_refusal_now() {
        eprintln!("vmlab-fastpath: {refusal}");
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

/// Why this start is refused, or `None` to serve it.
///
/// File capabilities go to whoever executes the file, not to its owner. So
/// when this process got privilege from its file (`gained`), it serves only
/// the account the grant was made for: the owner of the file. Root holds
/// every capability already, and a file that root owns was granted by the
/// administrator for the whole host. Without a gain the caller gets nothing
/// it did not hold before, and every caller is served.
fn caller_refusal(gained: bool, caller: u32, owner: Option<u32>) -> Option<String> {
    if !gained || caller == 0 {
        return None;
    }
    match owner {
        Some(owner) if owner == caller || owner == 0 => None,
        Some(owner) => Some(format!(
            "this helper holds the fast path's capabilities for uid {owner} only, and uid \
             {caller} started it"
        )),
        None => Some(
            "this helper holds file capabilities and cannot read the owner of its own file, so \
             it serves nobody"
                .into(),
        ),
    }
}

/// [`caller_refusal`] for this process. `AT_SECURE` is how the kernel says
/// that the exec gained privilege from the file (capabilities or a set-id
/// bit). The owner is read through `/proc/self/exe`, which is the file that
/// was executed, whatever its path is now.
fn caller_refusal_now() -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let gained = rustix::param::linux_secure();
    let caller = nix::unistd::getuid().as_raw();
    let owner = std::fs::metadata("/proc/self/exe")
        .ok()
        .map(|meta| meta.uid());
    caller_refusal(gained, caller, owner)
}

#[cfg(test)]
mod tests {
    use super::caller_refusal;

    /// Review `install-release.md` F2 (card 608): a helper with file
    /// capabilities served any account that could execute it.
    #[test]
    fn a_capable_helper_refuses_every_user_but_its_owner() {
        let refusal = caller_refusal(true, 1001, Some(1000)).expect("another user is refused");
        assert!(refusal.contains("uid 1000 only"), "{refusal}");
        assert!(refusal.contains("uid 1001"), "{refusal}");
        assert_eq!(caller_refusal(true, 1000, Some(1000)), None);
    }

    #[test]
    fn a_capable_helper_with_an_unreadable_owner_serves_nobody() {
        assert!(caller_refusal(true, 1000, None).is_some());
    }

    #[test]
    fn root_and_a_root_owned_helper_are_served() {
        assert_eq!(caller_refusal(true, 0, Some(1000)), None);
        assert_eq!(caller_refusal(true, 0, None), None);
        assert_eq!(caller_refusal(true, 1001, Some(0)), None);
    }

    /// Without a gain from the file the helper has only what its caller
    /// held, so a build owned by another account still runs.
    #[test]
    fn a_helper_that_gained_nothing_serves_every_caller() {
        assert_eq!(caller_refusal(false, 1001, Some(1000)), None);
        assert_eq!(caller_refusal(false, 1001, None), None);
    }
}
