//! End-to-end session tests against real processes/files on the build host:
//! the same code paths a guest runs, minus the virtio port (frames land in a
//! capture channel instead).

#![cfg(test)]
#![cfg(unix)]

use vmlab_agent_proto::fileops::{ErrorCode, Op, OpenFlags, Reply};
use vmlab_agent_proto::watch::EntryKind;
use vmlab_agent_proto::{AgentMsg, HostMsg};

use crate::fileops::hex;
use crate::mux::{Input, Mux, Platform};
use crate::testutil::{ask, ask_with, capture_mux};

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
            env: vec![],
            logon: None,
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
            env: vec![],
            logon: None,
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
            env: vec![],
            logon: None,
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
            logon: None,
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
            logon: None,
        },
    );
    match cap.ctrl() {
        AgentMsg::Error {
            id: Some(5), msg, ..
        } => assert!(msg.contains("/no/such/binary")),
        other => panic!("expected error, got {other:?}"),
    }
}

/// The whole push shape over the new vocabulary: open, write at offsets,
/// close, then ask the guest for its own digest of what landed — which is
/// the verification the retired whole-file transfer offered, kept (§19.5).
#[test]
fn fileops_writes_at_offsets_applies_the_mode_and_digests_what_landed() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub/dir")).unwrap();
    let path = dir.path().join("sub/dir/pushed.bin");
    let (mux, mut cap) = capture_mux();
    let p = platform();
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();

    open(&mux, &p, HostMsg::OpenFileOps { id: 6, logon: None });
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 6 });

    ask(
        &mux,
        6,
        1,
        Op::Open {
            path: path.to_str().unwrap().into(),
            flags: OpenFlags::create_truncate(),
            mode: Some(0o750),
        },
    );
    let (reply, _) = cap.fileops(6);
    let Reply::Handle { handle } = reply.reply else {
        panic!("expected a handle, got {reply:?}");
    };
    assert_eq!(reply.id, 1);

    // Offset-addressed: the chunks go out back to back without waiting, and
    // the file is assembled from where each one says it belongs.
    let chunks: Vec<&[u8]> = payload.chunks(64 * 1024).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        ask_with(
            &mux,
            6,
            100 + i as u64,
            Op::Write {
                handle,
                offset: (i * 64 * 1024) as u64,
            },
            chunk,
        );
    }
    let mut acked: Vec<u64> = Vec::new();
    while acked.len() < chunks.len() {
        let (reply, _) = cap.fileops(6);
        assert_eq!(reply.reply, Reply::Ok, "write {} failed", reply.id);
        acked.push(reply.id);
    }
    acked.sort_unstable();
    assert_eq!(acked, (100..100 + chunks.len() as u64).collect::<Vec<_>>());

    ask(&mux, 6, 2, Op::Close { handle });
    assert_eq!(cap.fileops(6).0.reply, Reply::Ok);

    ask(
        &mux,
        6,
        3,
        Op::Digest {
            path: path.to_str().unwrap().into(),
        },
    );
    let (reply, _) = cap.fileops(6);
    assert_eq!(
        reply.reply,
        Reply::Digest {
            sha256: hex(&Sha256::digest(&payload)),
            len: payload.len() as u64,
        }
    );

    assert_eq!(std::fs::read(&path).unwrap(), payload);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o750);
}

/// The pull shape: read at offsets until a short read says end-of-file, with
/// the bytes riding raw beside the metadata rather than inflated inside it.
#[test]
fn fileops_reads_at_offsets_until_a_short_read_ends_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pulled.bin");
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &payload).unwrap();
    let (mux, mut cap) = capture_mux();
    let p = platform();

    open(&mux, &p, HostMsg::OpenFileOps { id: 7, logon: None });
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 7 });
    // The payload exceeds the initial window: grant more while the reads run,
    // exercising the credit path a record is chunked into.
    let mux2 = mux.clone();
    let granter = std::thread::spawn(move || {
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            mux2.grant(7, 64 * 1024);
        }
    });

    ask(
        &mux,
        7,
        1,
        Op::Open {
            path: path.to_str().unwrap().into(),
            flags: OpenFlags::read(),
            mode: None,
        },
    );
    let Reply::Handle { handle } = cap.fileops(7).0.reply else {
        panic!("expected a handle");
    };

    let mut got = Vec::new();
    let mut id = 2;
    loop {
        ask(
            &mux,
            7,
            id,
            Op::Read {
                handle,
                offset: got.len() as u64,
                len: 64 * 1024,
            },
        );
        let (reply, bytes) = cap.fileops(7);
        assert_eq!(reply.id, id);
        assert_eq!(reply.reply, Reply::Data);
        if bytes.is_empty() {
            break;
        }
        got.extend(bytes);
        id += 1;
    }
    granter.join().unwrap();
    assert_eq!(got, payload);
}

/// A failed operation is a reply, not a dead channel: the session that asked
/// for a file that is not there keeps going and its next request works.
#[test]
fn a_failed_operation_answers_a_coded_error_and_the_session_lives() {
    let dir = tempfile::tempdir().unwrap();
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(&mux, &p, HostMsg::OpenFileOps { id: 8, logon: None });
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 8 });

    ask(
        &mux,
        8,
        1,
        Op::Open {
            path: "/no/such/file".into(),
            flags: OpenFlags::read(),
            mode: None,
        },
    );
    let (reply, _) = cap.fileops(8);
    assert_eq!(reply.id, 1);
    match reply.reply {
        Reply::Error { code, msg } => {
            assert_eq!(code, ErrorCode::NoSuchFile);
            assert!(msg.contains("/no/such/file"), "{msg}");
        }
        other => panic!("expected a coded error, got {other:?}"),
    }

    // A read past the record cap is refused rather than quietly cut short,
    // because a short answer is how end-of-file is spelled.
    std::fs::write(dir.path().join("f"), b"xyz").unwrap();
    ask(
        &mux,
        8,
        10,
        Op::Open {
            path: dir.path().join("f").to_str().unwrap().into(),
            flags: OpenFlags::read(),
            mode: None,
        },
    );
    let Reply::Handle { handle } = cap.fileops(8).0.reply else {
        panic!("expected a handle");
    };
    ask(
        &mux,
        8,
        11,
        Op::Read {
            handle,
            offset: 0,
            len: vmlab_agent_proto::fileops::MAX_DATA as u32 + 1,
        },
    );
    match cap.fileops(8).0.reply {
        Reply::Error { code, msg } => {
            assert_eq!(code, ErrorCode::Failure);
            assert!(msg.contains("record cap"), "{msg}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // A handle this channel never handed out is refused the same way.
    ask(&mux, 8, 2, Op::Close { handle: 99 });
    let (reply, _) = cap.fileops(8);
    assert!(matches!(
        reply.reply,
        Reply::Error {
            code: ErrorCode::BadHandle,
            ..
        }
    ));

    // And the session is still good.
    ask(
        &mux,
        8,
        3,
        Op::Stat {
            path: dir.path().to_str().unwrap().into(),
        },
    );
    let (reply, _) = cap.fileops(8);
    match reply.reply {
        Reply::Attrs { attrs } => assert_eq!(attrs.kind, EntryKind::Dir),
        other => panic!("expected attrs, got {other:?}"),
    }
}

/// The directory half a tree push and the syncer both need: `mkdir` with its
/// case-sensitivity flag, `opendir`/`readdir`, `rename`, `remove`, `rmdir`
/// and `realpath`.
#[test]
fn fileops_serves_the_directory_vocabulary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap().to_string();
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(&mux, &p, HostMsg::OpenFileOps { id: 9, logon: None });
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 9 });

    // A Linux directory is case-sensitive by construction, so the flag is
    // satisfied rather than refused.
    ask(
        &mux,
        9,
        1,
        Op::Mkdir {
            path: format!("{root}/src"),
            mode: Some(0o755),
            case_sensitive: true,
        },
    );
    assert_eq!(cap.fileops(9).0.reply, Reply::Ok);
    std::fs::write(dir.path().join("src/a.rs"), "fn main() {}").unwrap();

    ask(
        &mux,
        9,
        2,
        Op::OpenDir {
            path: format!("{root}/src"),
        },
    );
    let Reply::Handle { handle } = cap.fileops(9).0.reply else {
        panic!("expected a handle");
    };
    ask(&mux, 9, 3, Op::ReadDir { handle });
    match cap.fileops(9).0.reply {
        Reply::Entries { entries, eof } => {
            assert!(eof);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "a.rs");
            assert_eq!(entries[0].attrs.size, 12);
        }
        other => panic!("expected entries, got {other:?}"),
    }
    ask(&mux, 9, 4, Op::Close { handle });
    assert_eq!(cap.fileops(9).0.reply, Reply::Ok);

    ask(
        &mux,
        9,
        5,
        Op::Rename {
            from: format!("{root}/src/a.rs"),
            to: format!("{root}/src/b.rs"),
        },
    );
    assert_eq!(cap.fileops(9).0.reply, Reply::Ok);

    ask(
        &mux,
        9,
        6,
        Op::Realpath {
            path: format!("{root}/src/../src/b.rs"),
        },
    );
    match cap.fileops(9).0.reply {
        // The tempdir may itself sit behind a symlink, so what matters is
        // that the `..` is gone rather than the exact prefix.
        Reply::Name { path } => assert!(path.ends_with("/src/b.rs"), "{path}"),
        other => panic!("expected a name, got {other:?}"),
    }

    ask(
        &mux,
        9,
        7,
        Op::Remove {
            path: format!("{root}/src/b.rs"),
        },
    );
    assert_eq!(cap.fileops(9).0.reply, Reply::Ok);
    ask(
        &mux,
        9,
        8,
        Op::Rmdir {
            path: format!("{root}/src"),
        },
    );
    assert_eq!(cap.fileops(9).0.reply, Reply::Ok);
    assert!(!dir.path().join("src").exists());
}

/// Handles are scoped to the channel and die with it: after a `close` the
/// session's files are released, so a guest is never left holding handles a
/// host has forgotten.
#[test]
fn closing_the_channel_releases_every_handle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("held.bin");
    std::fs::write(&path, b"held").unwrap();
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenFileOps {
            id: 10,
            logon: None,
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 10 });
    ask(
        &mux,
        10,
        1,
        Op::Open {
            path: path.to_str().unwrap().into(),
            flags: OpenFlags::read(),
            mode: None,
        },
    );
    let Reply::Handle { handle } = cap.fileops(10).0.reply else {
        panic!("expected a handle");
    };

    mux.remove(10);

    // A second session starts its handle table from nothing, so the first
    // session's handle means nothing in it.
    open(
        &mux,
        &p,
        HostMsg::OpenFileOps {
            id: 11,
            logon: None,
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 11 });
    ask(
        &mux,
        11,
        1,
        Op::Read {
            handle,
            offset: 0,
            len: 4,
        },
    );
    assert!(matches!(
        cap.fileops(11).0.reply,
        Reply::Error {
            code: ErrorCode::BadHandle,
            ..
        }
    ));
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
            logon: None,
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

/// The dial happens inside the guest and the channel is the byte pipe: the
/// host writes, the peer answers, and the answer comes back as DATA frames.
#[test]
fn tunnel_dials_and_carries_bytes_both_ways() {
    use std::io::{Read, Write};
    // An echo server standing in for whatever the guest can reach.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        let mut buf = [0u8; 64];
        let n = peer.read(&mut buf).unwrap();
        peer.write_all(b"pong:").unwrap();
        peer.write_all(&buf[..n]).unwrap();
        // Half-close: the peer is done sending, the host is not.
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let n = peer.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"after-eof");
    });

    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenTunnel {
            id: 20,
            // A name, not an address: the guest resolves it.
            host: "localhost".into(),
            port,
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 20 });
    mux.route_input(20, Input::Bytes(b"ping".to_vec()));
    let (data, _) = cap.data_until(20, b"pong:ping");
    assert_eq!(data, b"pong:ping");
    // The peer's shutdown is an agent `eof`, not a dead channel.
    assert_eq!(cap.ctrl(), AgentMsg::Eof { id: 20 });
    // ...and the other direction still carries bytes.
    mux.route_input(20, Input::Bytes(b"after-eof".to_vec()));
    mux.route_input(20, Input::Eof);
    server.join().unwrap();
}

/// Nothing listening is a *connect* failure, which the SSH facade has to
/// tell apart from vmlab refusing the open.
#[test]
fn tunnel_connect_failure_carries_its_cause() {
    // Bind then drop: a port nothing is listening on, chosen by the OS.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);

    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenTunnel {
            id: 21,
            host: "127.0.0.1".into(),
            port,
        },
    );
    match cap.ctrl() {
        AgentMsg::Error {
            id: Some(21),
            cause,
            msg,
        } => {
            assert_eq!(cause, Some(vmlab_agent_proto::ErrorCause::ConnectFailed));
            assert!(msg.contains(&port.to_string()), "{msg}");
        }
        other => panic!("expected a connect failure, got {other:?}"),
    }
}

/// An unresolvable name fails guest-side too, and reports the same way.
#[test]
fn tunnel_unresolvable_host_is_a_connect_failure() {
    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenTunnel {
            id: 22,
            host: "no-such-host.invalid".into(),
            port: 80,
        },
    );
    match cap.ctrl() {
        AgentMsg::Error {
            id: Some(22),
            cause: Some(vmlab_agent_proto::ErrorCause::ConnectFailed),
            ..
        } => {}
        other => panic!("expected a connect failure, got {other:?}"),
    }
}

/// A host `close` drops the connection, so the peer sees the socket go away
/// instead of hanging on a tunnel nobody owns any more.
#[test]
fn tunnel_close_stops_the_connection() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        // The tunnel's close shuts the socket, so this read ends rather than
        // blocking for the lifetime of the test.
        let mut buf = Vec::new();
        peer.read_to_end(&mut buf).unwrap();
    });

    let (mux, mut cap) = capture_mux();
    let p = platform();
    open(
        &mux,
        &p,
        HostMsg::OpenTunnel {
            id: 23,
            host: "127.0.0.1".into(),
            port,
        },
    );
    assert_eq!(cap.ctrl(), AgentMsg::Opened { id: 23 });
    mux.remove(23);
    server.join().unwrap();
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
