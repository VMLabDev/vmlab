//! A verb whose stdout reader has gone ends like any Unix filter: quietly,
//! with the status 141 a shell reports for a process `SIGPIPE` killed.

use std::process::{Command, Stdio};

/// Run `vmlab` with `args`, its stdout a pipe whose reader is already
/// closed, against an empty runtime directory so no daemon is reached.
fn run_into_closed_pipe(args: &[&str]) -> std::process::Output {
    let scratch = tempfile::tempdir().unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    Command::new(env!("CARGO_BIN_EXE_vmlab"))
        .args(args)
        .env("XDG_RUNTIME_DIR", scratch.path())
        .env("XDG_STATE_HOME", scratch.path())
        .env("XDG_CONFIG_HOME", scratch.path())
        .env("XDG_DATA_HOME", scratch.path())
        .stdin(Stdio::null())
        .stdout(writer)
        .output()
        .unwrap()
}

#[test]
fn a_verb_printing_into_a_closed_pipe_exits_141() {
    let out = run_into_closed_pipe(&["daemon", "status"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    assert!(stderr.is_empty(), "stderr: {stderr}");
    assert_eq!(out.status.code(), Some(141), "status: {:?}", out.status);
}
