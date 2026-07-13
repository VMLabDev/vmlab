//! End-to-end session tests against real processes/files on the build host:
//! the same code paths a guest runs, minus the virtio port (frames land in a
//! capture channel instead).

#![cfg(test)]
#![cfg(unix)]

use vmlab_agent_proto::{AgentMsg, HostMsg};

use crate::mux::{Input, Mux, Platform};
use crate::testutil::capture_mux;

fn platform() -> impl Platform {
    crate::platform_impl::new_platform()
}

fn open(mux: &Mux, p: &impl Platform, msg: HostMsg) {
    // Route through the public dispatch surface like real frames do.
    let frame = vmlab_agent_proto::Frame {
        kind: vmlab_agent_proto::FrameKind::Ctrl,
        channel: 0,
        payload: serde_json::to_vec(&msg).unwrap(),
    };
    mux.handle_frame(frame, p);
}

#[test]
fn terminal_runs_an_interactive_shell() {
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenTerminal {
            id: 1,
            cols: 80,
            rows: 24,
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo ready; read x; echo got:$x; exit 7".into(),
            ]),
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 1 });
    // MOTD + our marker arrive over the PTY.
    cap.data_until(1, b"ready");
    mux.route_input(1, Input::Bytes(b"abc\n".to_vec()));
    let (_out, _err, code) = {
        cap.data_until(1, b"got:abc");
        cap.until_exited(1)
    };
    assert_eq!(code, 7);
}

#[test]
fn terminal_resize_reaches_the_pty() {
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenTerminal {
            id: 2,
            cols: 80,
            rows: 24,
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                // stty reads the PTY size; print it after the host resizes.
                "read x; stty size; exit 0".into(),
            ]),
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 2 });
    mux.resize(2, 132, 43);
    mux.route_input(2, Input::Bytes(b"\n".to_vec()));
    let (out, _err, code) = cap.until_exited(2);
    assert_eq!(code, 0);
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("43 132"), "stty saw: {text}");
}

#[test]
fn terminal_close_kills_the_shell() {
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenTerminal {
            id: 3,
            cols: 80,
            rows: 24,
            command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 300".into()]),
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 3 });
    mux.remove(3);
    // The reaper still reports the (signal) death it observed.
    loop {
        if let AgentMsg::Exited { id: 3, code } = cap.ctrl() {
            assert_eq!(code, 128 + 9, "expected SIGKILL death");
            break;
        }
    }
}

#[test]
fn exec_streams_stdio_and_exit_code() {
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenExec {
            id: 4,
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo out-here; echo err-here >&2; cat; exit 3".into(),
            ],
            env: vec![("VMLAB_TEST".into(), "1".into())],
            cwd: None,
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 4 });
    mux.route_input(4, Input::Bytes(b"piped-through".to_vec()));
    mux.route_input(4, Input::Eof);
    let (out, err, code) = cap.until_exited(4);
    let out = String::from_utf8_lossy(&out);
    let err = String::from_utf8_lossy(&err);
    assert!(out.contains("out-here"), "{out}");
    assert!(out.contains("piped-through"), "{out}");
    assert!(err.contains("err-here"), "{err}");
    assert!(!out.contains("err-here"), "stderr must ride DataErr: {out}");
    assert_eq!(code, 3);
}

#[test]
fn exec_missing_binary_reports_error() {
    let (mux, mut cap) = capture_mux();
    let p = platform();
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
        AgentMsg::Error { id: Some(5), msg } => assert!(msg.contains("/no/such/binary")),
        other => panic!("expected error, got {other:?}"),
    }
}

#[test]
fn file_push_writes_bytes_mode_and_digest() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/dir/pushed.bin");
    let (mux, mut cap) = capture_mux();
    let p = platform();
    let payload = vec![0xabu8; 200_000];
    open(
        &mux,
        &p,
        HostMsg::OpenFilePush {
            id: 6,
            path: path.to_str().unwrap().into(),
            mode: Some(0o750),
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 6 });
    for chunk in payload.chunks(60_000) {
        mux.route_input(6, Input::Bytes(chunk.to_vec()));
    }
    mux.route_input(6, Input::Eof);
    let (_, sha, len) = cap.until_file_done(6);
    assert_eq!(len, payload.len() as u64);
    assert_eq!(sha, crate::files::hex(&Sha256::digest(&payload)));
    assert_eq!(std::fs::read(&path).unwrap(), payload);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o750);
}

#[test]
fn file_pull_streams_bytes_and_digest() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pulled.bin");
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &payload).unwrap();
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenFilePull {
            id: 7,
            path: path.to_str().unwrap().into(),
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 7 });
    // The payload exceeds the initial window: grant more while the transfer
    // runs, exercising the credit path.
    let mux2 = mux.clone();
    let granter = std::thread::spawn(move || {
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            mux2.grant(7, 64 * 1024);
        }
    });
    let (data, sha, len) = cap.until_file_done(7);
    granter.join().unwrap();
    assert_eq!(len, payload.len() as u64);
    assert_eq!(data, payload);
    assert_eq!(sha, crate::files::hex(&Sha256::digest(&payload)));
}

#[test]
fn file_pull_missing_file_reports_error() {
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenFilePull {
            id: 8,
            path: "/no/such/file".into(),
        },
    );
    match cap.ctrl() {
        AgentMsg::Error { id: Some(8), .. } => {}
        other => panic!("expected error, got {other:?}"),
    }
}

#[test]
fn tail_sends_backlog_then_appends() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.log");
    std::fs::write(&path, "old line\n").unwrap();
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenTail {
            id: 9,
            path: path.to_str().unwrap().into(),
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 9 });
    cap.data_until(9, b"old line");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "fresh line").unwrap();
    f.flush().unwrap();
    cap.data_until(9, b"fresh line");
    // Rotation: replace the file wholesale; the tail follows the new one.
    drop(f);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "rotated content\n").unwrap();
    cap.data_until(9, b"rotated content");
    mux.remove(9);
}

#[test]
fn metrics_subscription_emits_samples() {
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(&mux, &p, HostMsg::SubscribeMetrics { interval_secs: 1 });
    match cap.ctrl() {
        AgentMsg::Metrics {
            mem_used,
            mem_total,
            ..
        } => {
            assert!(mem_total > 0);
            assert!(mem_used <= mem_total);
        }
        other => panic!("expected metrics, got {other:?}"),
    }
    open(&mux, &p, HostMsg::UnsubscribeMetrics);
}
