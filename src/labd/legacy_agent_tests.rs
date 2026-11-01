//! Compile the C POSIX adapter and run the same contract as any other binary.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, ensure};

use super::vm_agent::conformance::Fixture;

#[tokio::test]
async fn legacy_agent_conformance() -> Result<()> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("guest/agent-legacy/src");
    let dir = tempfile::tempdir()?;
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
        .status()
        .context("C compiler required for legacy agent conformance")?;
    ensure!(status.success(), "legacy agent did not compile cleanly");
    let fixture = Fixture::process(&bin, "listen", vec![]).await?;
    fixture.run("legacy", None).await
}
