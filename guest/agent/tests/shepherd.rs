//! The process-tree shepherd (`vmlab-agent --shepherd -- argv…`), run as
//! the real binary: the agent re-executes itself to get one, so the unit
//! tests cannot reach it.

#![cfg(unix)]

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Start a shepherd over `sh -c script`, returning it and the read end of
/// its status pipe.
fn shepherd(script: &str) -> (Child, BufReader<File>) {
    shepherd_over(&["/bin/sh", "-c", script])
}

fn shepherd_over(argv: &[&str]) -> (Child, BufReader<File>) {
    let mut fds = [0; 2];
    // SAFETY: pipe2 into a two-element array.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let [read, write] = fds;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vmlab-agent"));
    cmd.args(["--shepherd", "--"])
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: dup2 between fork and exec, as the agent does.
    unsafe {
        cmd.pre_exec(move || match libc::dup2(write, 3) {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        })
    };
    let child = cmd.spawn().expect("spawn the shepherd");
    // SAFETY: closing our copy of the write end; `read` becomes owned.
    unsafe { libc::close(write) };
    (child, BufReader::new(unsafe { File::from_raw_fd(read) }))
}

fn line(status: &mut BufReader<File>) -> String {
    let mut line = String::new();
    status.read_line(&mut line).expect("read the status pipe");
    line.trim().to_string()
}

/// Every live process whose parent is `pid`.
fn children(pid: u32) -> Vec<u32> {
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|child| {
            fs::read_to_string(format!("/proc/{child}/stat"))
                .ok()
                .and_then(|s| {
                    let rest = &s[s.rfind(')')? + 1..];
                    rest.split_whitespace().nth(1)?.parse::<u32>().ok()
                })
                == Some(pid)
        })
        .collect()
}

fn wait_with_deadline(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(start.elapsed() < limit, "the shepherd outlived {limit:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The command's exit code arrives as soon as the command ends; the
/// shepherd adopts what it orphaned and outlives it.
#[test]
fn the_command_exit_is_reported_before_the_tree_drains() {
    let (mut shepherd, mut status) = shepherd("sleep 2 & exit 7");
    assert_eq!(line(&mut status), shepherd.id().to_string());

    let start = Instant::now();
    assert_eq!(line(&mut status), "7");
    assert!(start.elapsed() < Duration::from_millis(1500));

    // The background sleep lost its shell, and the subreaper caught it.
    assert!(
        shepherd.try_wait().unwrap().is_none(),
        "the shepherd left early"
    );
    assert_eq!(children(shepherd.id()).len(), 1);

    assert!(wait_with_deadline(&mut shepherd, Duration::from_secs(5)).success());
}

/// SIGTERM is what closing the exec sends: the command dies, what it
/// started does not, and the tree stays countable until that has finished.
#[test]
fn sigterm_kills_the_command_and_not_what_it_started() {
    let (mut shepherd, mut status) = shepherd("sleep 2 & exec sleep 60");
    assert_eq!(line(&mut status), shepherd.id().to_string());
    // The shell must have started its background sleep before it is killed.
    let start = Instant::now();
    while children(shepherd.id())
        .iter()
        .all(|&command| children(command).is_empty())
    {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the command never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(shepherd.id() as i32, libc::SIGTERM) };
    assert_eq!(line(&mut status), (128 + libc::SIGKILL).to_string());

    assert!(
        shepherd.try_wait().unwrap().is_none(),
        "the shepherd left early"
    );
    assert_eq!(children(shepherd.id()).len(), 1);
    assert!(wait_with_deadline(&mut shepherd, Duration::from_secs(5)).success());
}

/// A command that cannot be started is a 127, like any exec.
#[test]
fn a_command_that_cannot_start_reports_127() {
    let (mut child, mut status) = shepherd_over(&["/nonexistent/vmlab-test"]);
    assert_eq!(line(&mut status), child.id().to_string());
    assert_eq!(line(&mut status), "127");
    assert_eq!(child.wait().unwrap().code(), Some(127));
}
