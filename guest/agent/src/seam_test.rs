//! Session tests against the in-memory [`Spawner`] adapter: no guest, no
//! child process, no filesystem.
//!
//! They cover what the funnel exists to make testable — that terminals,
//! exec and file pushes all reach the one seam, and reach it with the
//! identity the channel resolved to (PRD §19.2).

#![cfg(test)]

use vmlab_agent_proto::{AgentMsg, Frame, FrameKind, HostMsg};

use crate::fake_spawner::{Call, TestPlatform};
use crate::mux::{Input, Mux};
use crate::spawn::Identity;
use crate::testutil::capture_mux;

fn open(mux: &Mux, platform: &TestPlatform, msg: HostMsg) {
    let frame = Frame {
        kind: FrameKind::Ctrl,
        channel: 0,
        payload: serde_json::to_vec(&msg).unwrap(),
    };
    mux.handle_frame(frame, platform);
}

#[test]
fn terminal_exec_and_push_all_reach_the_seam_as_the_agent() {
    let (mux, mut cap) = capture_mux();
    let p = TestPlatform::new();

    open(
        &mux,
        &p,
        HostMsg::OpenTerminal {
            id: 1,
            cols: 80,
            rows: 24,
            command: Some(vec!["/bin/sh".into()]),
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 1 });

    open(
        &mux,
        &p,
        HostMsg::OpenExec {
            id: 2,
            argv: vec!["/bin/echo".into(), "hi".into()],
            env: vec![("K".into(), "V".into())],
            cwd: Some("/tmp".into()),
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 2 });

    open(
        &mux,
        &p,
        HostMsg::OpenFilePush {
            id: 3,
            path: "/srv/app.conf".into(),
            mode: None,
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 3 });

    assert_eq!(
        p.spawner.calls(),
        vec![
            Call::Terminal {
                identity: Identity::Agent,
                command: Some(vec!["/bin/sh".into()]),
                cols: 80,
                rows: 24,
            },
            Call::Exec {
                identity: Identity::Agent,
                argv: vec!["/bin/echo".into(), "hi".into()],
                env: vec![("K".into(), "V".into())],
                cwd: Some("/tmp".into()),
            },
            Call::CreateFile {
                identity: Identity::Agent,
                path: "/srv/app.conf".into(),
            },
        ]
    );
}

#[test]
fn terminal_bridges_keystrokes_output_resize_and_exit() {
    let (mux, mut cap) = capture_mux();
    let p = TestPlatform::new();
    open(
        &mux,
        &p,
        HostMsg::OpenTerminal {
            id: 1,
            cols: 80,
            rows: 24,
            command: None,
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 1 });
    let shell = p.spawner.process(0);

    shell.say(b"prompt$ ");
    cap.data_until(1, b"prompt$ ");

    mux.route_input(1, Input::Bytes(b"whoami\n".to_vec()));
    mux.resize(1, 132, 43);
    shell.say(b"root\n");
    cap.data_until(1, b"root");
    shell.expect_input(b"whoami\n");
    assert_eq!(shell.resizes(), vec![(132, 43)]);

    shell.finish(7);
    let (_out, _err, code) = cap.until_exited(1);
    assert_eq!(code, 7);
}

#[test]
fn terminal_close_kills_the_shell_through_the_seam() {
    let (mux, mut cap) = capture_mux();
    let p = TestPlatform::new();
    open(
        &mux,
        &p,
        HostMsg::OpenTerminal {
            id: 1,
            cols: 80,
            rows: 24,
            command: None,
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 1 });
    let shell = p.spawner.process(0);

    mux.remove(1);
    let (_out, _err, code) = cap.until_exited(1);
    assert!(shell.killed());
    assert_eq!(code, 128 + 9, "expected the SIGKILL-shaped code");
}

#[test]
fn terminal_spawn_failure_reports_the_seam_error() {
    let (mux, mut cap) = capture_mux();
    let p = TestPlatform::new();
    p.spawner.fail_next("no shell found in this guest");
    open(
        &mux,
        &p,
        HostMsg::OpenTerminal {
            id: 1,
            cols: 80,
            rows: 24,
            command: None,
        },
    );
    match cap.ctrl() {
        AgentMsg::Error {
            id: Some(1),
            msg,
            cause: None,
        } => {
            assert_eq!(msg, "terminal: no shell found in this guest");
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[test]
fn exec_splits_stdout_and_stderr_and_closes_stdin_on_eof() {
    let (mux, mut cap) = capture_mux();
    let p = TestPlatform::new();
    open(
        &mux,
        &p,
        HostMsg::OpenExec {
            id: 4,
            argv: vec!["/bin/cat".into()],
            env: vec![],
            cwd: None,
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 4 });
    let child = p.spawner.process(0);

    mux.route_input(4, Input::Bytes(b"piped-through".to_vec()));
    mux.route_input(4, Input::Eof);
    child.say(b"out-here");
    child.say_err(b"err-here");
    child.finish(3);

    let (out, err, code) = cap.until_exited(4);
    assert_eq!(out, b"out-here");
    assert_eq!(err, b"err-here");
    assert_eq!(code, 3);
    child.expect_input(b"piped-through");
}

#[test]
fn exec_spawn_failure_names_the_binary() {
    let (mux, mut cap) = capture_mux();
    let p = TestPlatform::new();
    p.spawner
        .fail_next("No such file or directory (os error 2)");
    open(
        &mux,
        &p,
        HostMsg::OpenExec {
            id: 5,
            argv: vec!["/no/such/binary".into()],
            env: vec![],
            cwd: None,
        },
    );
    match cap.ctrl() {
        AgentMsg::Error {
            id: Some(5),
            msg,
            cause: None,
        } => assert!(msg.contains("/no/such/binary"), "{msg}"),
        other => panic!("expected error, got {other:?}"),
    }
}

#[test]
fn exec_empty_argv_never_reaches_the_seam() {
    let (mux, mut cap) = capture_mux();
    let p = TestPlatform::new();
    open(
        &mux,
        &p,
        HostMsg::OpenExec {
            id: 5,
            argv: vec![],
            env: vec![],
            cwd: None,
        },
    );
    match cap.ctrl() {
        AgentMsg::Error {
            id: Some(5),
            msg,
            cause: None,
        } => assert_eq!(msg, "exec: empty argv"),
        other => panic!("expected error, got {other:?}"),
    }
    assert!(p.spawner.calls().is_empty());
}

#[test]
fn push_writes_through_the_seam_and_applies_the_mode() {
    use sha2::{Digest, Sha256};
    let (mux, mut cap) = capture_mux();
    let p = TestPlatform::new();
    open(
        &mux,
        &p,
        HostMsg::OpenFilePush {
            id: 6,
            path: "/srv/app.conf".into(),
            mode: Some(0o750),
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 6 });

    mux.route_input(6, Input::Bytes(b"hello ".to_vec()));
    mux.route_input(6, Input::Bytes(b"world".to_vec()));
    mux.route_input(6, Input::Eof);

    let (_data, sha, len) = cap.until_file_done(6);
    assert_eq!(len, 11);
    assert_eq!(sha, crate::files::hex(&Sha256::digest(b"hello world")));

    let file = p.spawner.file(0);
    assert_eq!(file.bytes(), b"hello world");
    assert_eq!(file.mode(), Some(0o750));
}

#[test]
fn push_create_failure_reports_the_seam_error() {
    let (mux, mut cap) = capture_mux();
    let p = TestPlatform::new();
    p.spawner
        .fail_next("mkdir: Permission denied (os error 13)");
    open(
        &mux,
        &p,
        HostMsg::OpenFilePush {
            id: 7,
            path: "/srv/app.conf".into(),
            mode: None,
        },
    );
    match cap.ctrl() {
        AgentMsg::Error {
            id: Some(7),
            msg,
            cause: None,
        } => {
            assert_eq!(
                msg,
                "push /srv/app.conf: mkdir: Permission denied (os error 13)"
            );
        }
        other => panic!("expected error, got {other:?}"),
    }
}
