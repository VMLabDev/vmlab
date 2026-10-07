//! A closed stdout ends a CLI verb the way it ends any Unix filter: quietly,
//! with the status 141 a shell reports for a process `SIGPIPE` killed.
//!
//! Rust ignores `SIGPIPE` before `main`, so a write to a pipe whose reader
//! has gone returns `EPIPE`, and `println!` turns that into a panic. Putting
//! the default disposition back would end the panic, but it would also kill
//! the CLI silently whenever a daemon hangs up on one of its sockets, which
//! must stay an error it reports. So the disposition stays ignored and this
//! recognises the one failure that means "the reader of stdout is gone":
//! std's own print panic, and a verb error caused by `EPIPE` while stdout's
//! reader is gone. Only the person-facing verbs install it. The daemons the
//! same binary hosts never do, since a daemon must outlive whoever started it.

use std::os::fd::AsFd;

/// Make a print to a closed stdout end the process with status 141 rather
/// than a panic. Every other panic reaches the hook that was installed before.
pub(super) fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.payload_as_str().is_some_and(is_stdout_print_failure) {
            exit_as_sigpipe();
        }
        previous(info);
    }));
}

/// Whether a verb's error is a write into stdout after its reader left. The
/// error alone cannot say which descriptor failed, since a socket to a daemon
/// fails with the same `EPIPE`, so stdout itself is asked.
pub(super) fn is_closed_stdout(err: &anyhow::Error) -> bool {
    let epipe = err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
    });
    epipe && reader_gone(std::io::stdout())
}

/// End the process with the status a shell reports for one `SIGPIPE`
/// killed, 128 + 13, so a pipeline under `set -o pipefail` reads the same
/// 141 it reads from `yes | head -1`. The signal itself is not raised:
/// putting its default disposition back is `unsafe`, which the crate denies.
pub(super) fn exit_as_sigpipe() -> ! {
    std::process::exit(128 + nix::sys::signal::Signal::SIGPIPE as i32)
}

/// std's message for a `print!` whose write to stdout failed with `EPIPE`.
fn is_stdout_print_failure(message: &str) -> bool {
    let epipe = std::io::Error::from_raw_os_error(nix::errno::Errno::EPIPE as i32);
    message == format!("failed printing to stdout: {epipe}")
}

/// Whether the reading end of `fd` has closed. A pipe's writer polls
/// `POLLERR` once it has no reader, and a socket's polls `POLLHUP` once the
/// peer has shut down.
fn reader_gone(fd: impl AsFd) -> bool {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLOUT)];
    if poll(&mut fds, PollTimeout::ZERO).is_err() {
        return false;
    }
    fds[0]
        .revents()
        .is_some_and(|r| r.intersects(PollFlags::POLLERR | PollFlags::POLLHUP))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_print_into_a_closed_pipe_is_recognised() {
        assert!(is_stdout_print_failure(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
    }

    #[test]
    fn other_panics_are_not_mistaken_for_it() {
        assert!(!is_stdout_print_failure(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
        assert!(!is_stdout_print_failure(
            "failed printing to stdout: No space left on device (os error 28)"
        ));
        assert!(!is_stdout_print_failure(
            "called `Option::unwrap()` on a `None` value"
        ));
    }

    #[test]
    fn a_pipe_without_a_reader_is_gone() {
        let (reader, writer) = std::io::pipe().unwrap();
        assert!(!reader_gone(&writer));
        drop(reader);
        assert!(reader_gone(&writer));
    }

    #[test]
    fn a_hung_up_socket_is_gone() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!reader_gone(&ours));
        drop(theirs);
        assert!(reader_gone(&ours));
    }
}
