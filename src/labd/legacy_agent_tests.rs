//! Compile the C POSIX adapter and run the same contract as any other binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use super::vm_agent::conformance::Fixture;
use super::vm_agent::{AgentHandle, SessionEvent};

const BUDGET: Duration = Duration::from_secs(12);

fn build(dir: &Path) -> Result<PathBuf> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("guest/agent-legacy/src");
    let bin = dir.join("agent");
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
        .status()
        .context("C compiler required for legacy agent conformance")?;
    ensure!(status.success(), "legacy agent did not compile cleanly");
    Ok(bin)
}

#[tokio::test]
async fn legacy_agent_conformance() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let bin = build(dir.path())?;
    let fixture = Fixture::process(&bin, "listen", vec![]).await?;
    fixture.run("legacy", None).await
}

#[tokio::test]
async fn legacy_exec_reports_exit_with_inherited_output_pipes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let bin = build(dir.path())?;
    let fixture = Fixture::process(&bin, "listen", vec![]).await?;
    let handle = AgentHandle::connect(fixture.socket(), BUDGET).await?;
    let keep_running = tempfile::tempdir()?;
    let mut argv = sh("(while [ -d \"$1\" ]; do sleep 1; done) & \
                       printf stdout; printf stderr >&2; exit 3");
    argv.push("sh".into());
    argv.push(keep_running.path().to_str().unwrap().into());

    let output = handle.exec(argv, vec![], None, None, BUDGET, None).await?;
    assert_eq!(output.exit_code, 3);
    assert_eq!(output.stdout, b"stdout");
    assert_eq!(output.stderr, b"stderr");
    Ok(())
}

/// F49: a full argv (the agent's MAX_ARGV, 64) still reaches execvp with its
/// NULL terminator. F52: a child inherits none of the agent's descriptors —
/// not the channel, not the listener, not another exec's pipe ends.
#[tokio::test]
async fn legacy_agent_full_argv_and_no_inherited_descriptors() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let bin = build(dir.path())?;
    let fixture = Fixture::process(&bin, "listen", vec![]).await?;
    let handle = AgentHandle::connect(fixture.socket(), BUDGET).await?;

    let mut argv = sh("echo $#");
    argv.push("x".into());
    argv.extend((0..60).map(|i| format!("a{i}")));
    assert_eq!(argv.len(), 64);
    let out = handle.exec(argv, vec![], None, None, BUDGET, None).await?;
    ensure!(
        out.exit_code == 0 && out.stdout == b"60\n",
        "full argv: exit {} stdout {:?}",
        out.exit_code,
        String::from_utf8_lossy(&out.stdout)
    );

    // No child sees the channel or the listener (both sockets). Only
    // sockets are looked for: the test runner may hand the agent other
    // descriptors (a jobserver pipe) that are not the agent's to close.
    let script = "for f in /proc/$$/fd/*; do n=${f##*/}; l=$(readlink \"$f\"); \
                  case \"$l\" in socket:*) [ \"$n\" -gt 2 ] && echo \"$n -> $l\";; esac; done; true";
    let out = handle
        .exec(sh(script), vec![], None, None, BUDGET, None)
        .await?;
    ensure!(
        out.exit_code == 0 && out.stdout.is_empty(),
        "the child inherited the agent channel: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // A later child must not hold an earlier one's stdin open: `cat` sees
    // its EOF while `sleep` (forked after it) still runs.
    let mut cat = handle
        .open_exec(vec!["cat".into()], vec![], None, None)
        .await?;
    let sleeper = handle.open_exec(sh("sleep 20"), vec![], None, None).await?;
    cat.eof().await?;
    let exited = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = cat.recv().await {
            if let SessionEvent::Exited(code) = event {
                return Some(code);
            }
        }
        None
    })
    .await;
    sleeper.close().await;
    ensure!(
        matches!(exited, Ok(Some(0))),
        "cat did not see its EOF while a later child ran: {exited:?}"
    );
    Ok(())
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}
