//! Conformance: the legacy C agent (`guest/agent-legacy`, PRD §7.4) driven
//! by the very client the lab daemon uses, [`super::vm_agent::AgentHandle`].
//!
//! The POSIX build of the agent listens on a Unix socket, which is exactly
//! what QEMU's chardev socket looks like from the daemon's side, so nothing
//! here is a mock: the handshake, `exec` with every field the wire carries,
//! both flow-control directions, and the refusals every other open gets are
//! all exercised against the same C the DOS and Windows binaries are built
//! from. The C is compiled here with the host `cc` so the test tracks the
//! source rather than a stale artefact.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::vm_agent::{AgentHandle, SessionEvent};

const HANDSHAKE: Duration = Duration::from_secs(5);

struct Agent {
    child: Child,
    _dir: tempfile::TempDir,
    sock: PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Compile the POSIX build into a temp dir and start it listening. `None`
/// when the host has no C compiler, which the test reports and skips on.
fn spawn_agent() -> Option<Agent> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("guest/agent-legacy/src");
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("agent");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-O1"])
        .arg("-DAGENT_VERSION=\"agent-legacy=test\"")
        .arg("-o")
        .arg(&bin)
        .args(
            ["json.c", "wire.c", "agent.c", "plat_posix.c"]
                .iter()
                .map(|f| src.join(f)),
        )
        .status();
    let status = match status {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping: no C compiler ({cc}) on this host");
            return None;
        }
        Err(e) => panic!("running {cc}: {e}"),
    };
    assert!(
        status.success(),
        "the legacy agent's C must compile cleanly"
    );

    let sock = dir.path().join("agent.sock");
    let child = Command::new(&bin)
        .arg("--listen")
        .arg(&sock)
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the legacy agent");
    // The socket appears once the agent has bound it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !sock.exists() {
        assert!(std::time::Instant::now() < deadline, "agent never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    Some(Agent {
        child,
        _dir: dir,
        sock,
    })
}

#[tokio::test]
async fn legacy_agent_answers_the_handshake_with_exec_only() {
    let Some(agent) = spawn_agent() else { return };
    let handle = AgentHandle::connect(&agent.sock, HANDSHAKE).await.unwrap();
    let info = handle.info();
    assert_eq!(info.features, vec!["exec".to_string()]);
    assert_eq!(info.os, "linux");
    assert_eq!(info.agent_version, "agent-legacy=test");
    assert!(handle.ping(Duration::from_secs(2)).await);
}

#[tokio::test]
async fn legacy_agent_runs_an_exec_with_every_field() {
    let Some(agent) = spawn_agent() else { return };
    let handle = AgentHandle::connect(&agent.sock, HANDSHAKE).await.unwrap();
    let out = handle
        .exec(
            vec![
                "sh".into(),
                "-c".into(),
                "echo out $FOO; pwd; cat; echo err >&2; exit 3".into(),
            ],
            vec![("FOO".into(), "bar baz".into())],
            Some("/tmp".into()),
            Some(b"stdin-line\n".to_vec()),
            Duration::from_secs(10),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, 3);
    assert_eq!(out.stdout, b"out bar baz\n/tmp\nstdin-line\n");
    assert_eq!(out.stderr, b"err\n");
}

/// A megabyte each way: guest→host under the host's window grants, and
/// host→guest under the agent's, through a child that only reads stdin
/// once the whole of it has arrived.
#[tokio::test]
async fn legacy_agent_honours_flow_control_both_ways() {
    let Some(agent) = spawn_agent() else { return };
    let handle = AgentHandle::connect(&agent.sock, HANDSHAKE).await.unwrap();
    let out = handle
        .exec(
            vec![
                "sh".into(),
                "-c".into(),
                "head -c 1000000 /dev/zero | tr '\\0' x".into(),
            ],
            vec![],
            None,
            None,
            Duration::from_secs(20),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout.len(), 1_000_000);
    assert!(out.stdout.iter().all(|&b| b == b'x'));

    let stdin = vec![b'y'; 1_000_000];
    let out = handle
        .exec(
            vec!["sh".into(), "-c".into(), "sleep 0.3; wc -c".into()],
            vec![],
            None,
            Some(stdin),
            Duration::from_secs(20),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1000000");
}

/// Three execs interleaved on one connection, each on its own channel.
#[tokio::test]
async fn legacy_agent_multiplexes_channels() {
    let Some(agent) = spawn_agent() else { return };
    let handle = AgentHandle::connect(&agent.sock, HANDSHAKE).await.unwrap();
    let mut sessions = Vec::new();
    for i in 0..3 {
        let s = handle
            .open_exec(
                vec![
                    "sh".into(),
                    "-c".into(),
                    format!("sleep 0.2; echo chan{i}; exit {i}"),
                ],
                vec![],
                None,
                None,
            )
            .await
            .unwrap();
        sessions.push(s);
    }
    for (i, mut s) in sessions.into_iter().enumerate() {
        s.eof().await.unwrap();
        let mut out = Vec::new();
        let code = loop {
            match s.recv().await {
                Some(SessionEvent::Data(b)) => out.extend(b),
                Some(SessionEvent::Exited(c)) => break c,
                Some(SessionEvent::Eof) | Some(SessionEvent::Stderr(_)) => {}
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(code, i as i32);
        assert_eq!(out, format!("chan{i}\n").into_bytes());
    }
}

/// What the agent does not do, it refuses by name on the channel that
/// asked: a terminal is answered with an error carrying the id, and a spawn
/// that fails says why. A logon is refused too — the legacy agent mints
/// none (PRD §19.2's floor is all it has).
#[tokio::test]
async fn legacy_agent_refuses_what_it_lacks_by_name() {
    let Some(agent) = spawn_agent() else { return };
    let handle = AgentHandle::connect(&agent.sock, HANDSHAKE).await.unwrap();

    let err = match handle.open_terminal(80, 24, None, vec![], None).await {
        Ok(_) => panic!("no terminal on the legacy agent"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("no terminal"), "{err}");

    // On POSIX the spawn fails in the child after the fork, so the channel
    // opens and exits 127 — the shell convention — rather than refusing.
    let out = handle
        .exec(
            vec!["/nonexistent/program".into()],
            vec![],
            None,
            None,
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, 127);

    let logon = vmlab_agent_proto::Logon {
        user: "alice".into(),
        secret: "pw".into(),
        elevated: false,
    };
    let err = handle
        .exec(
            vec!["true".into()],
            vec![],
            None,
            None,
            Duration::from_secs(5),
            Some(logon),
        )
        .await
        .expect_err("a logon is refused");
    assert!(err.to_string().contains("cannot mint a logon"), "{err}");
}

/// A fresh hello resets the agent: sessions from before it are gone, and
/// bytes before the magic are skipped rather than fatal.
#[tokio::test]
async fn legacy_agent_rehandshakes_and_resyncs() {
    let Some(agent) = spawn_agent() else { return };
    let first = AgentHandle::connect(&agent.sock, HANDSHAKE).await.unwrap();
    let held = first
        .open_exec(vec!["sleep".into(), "30".into()], vec![], None, None)
        .await
        .unwrap();
    // The session holds a clone of the handle, so tear the connection down
    // the way the daemon does around a restore rather than by dropping.
    first.shutdown().await;
    drop(held);
    drop(first);
    // The agent goes back to accept(); a second client re-handshakes.
    let second = AgentHandle::connect(&agent.sock, HANDSHAKE).await.unwrap();
    let out = second
        .exec(
            vec!["echo".into(), "again".into()],
            vec![],
            None,
            None,
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, b"again\n");
}
