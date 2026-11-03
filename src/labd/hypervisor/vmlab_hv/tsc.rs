//! The KVM TSC frequency from `vmlab-hv --probe`, without booting a guest.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use vmlab_hv_proto::host::HostFacts;

pub(crate) async fn probe_host_hz(binary: &Path) -> Result<u64> {
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(binary)
            .arg("--probe")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("vmlab-hv host probe timed out after 20s")?
    .with_context(|| format!("starting {} --probe", binary.display()))?;
    if !output.status.success() {
        bail!(
            "{} --probe failed ({}): {}",
            binary.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_host_hz(&output.stdout)
}

fn parse_host_hz(bytes: &[u8]) -> Result<u64> {
    let facts: HostFacts = serde_json::from_slice(bytes).context("invalid vmlab-hv host facts")?;
    if !facts.kvm {
        bail!("vmlab-hv host probe: KVM is unavailable");
    }
    if !facts.invariant_tsc || !facts.stable_tsc {
        bail!("vmlab-hv host probe: tsc = \"host\" requires an invariant, stable host TSC");
    }
    let khz = facts
        .tsc_khz
        .filter(|khz| *khz > 0)
        .context("vmlab-hv host probe: KVM reports no TSC frequency")?;
    Ok(u64::from(khz) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_probe_parsing_converts_khz_to_hz() {
        assert_eq!(
            parse_host_hz(
                br#"{"kvm":true,"tsc_khz":4291956,"invariant_tsc":true,"stable_tsc":true}"#
            )
            .unwrap(),
            4_291_956_000
        );
    }

    #[test]
    fn host_probe_parsing_refuses_unusable_facts() {
        let good = serde_json::json!({"kvm":true,"tsc_khz":4291956,"invariant_tsc":true,"stable_tsc":true});
        for (key, bad) in [
            ("kvm", serde_json::json!(false)),
            ("invariant_tsc", serde_json::json!(false)),
            ("stable_tsc", serde_json::json!(false)),
            ("tsc_khz", serde_json::json!(null)),
            ("tsc_khz", serde_json::json!(0)),
            ("tsc_khz", serde_json::json!(-1)),
            ("tsc_khz", serde_json::json!(u64::MAX)),
        ] {
            let mut value = good.clone();
            value[key] = bad;
            assert!(
                parse_host_hz(&serde_json::to_vec(&value).unwrap()).is_err(),
                "{value}"
            );
        }
        for bad in [b"not JSON".as_slice(), b"{}", b"{\"kvm\":true}"] {
            assert!(parse_host_hz(bad).is_err());
        }
    }

    #[tokio::test]
    async fn host_probe_uses_the_selected_binary_and_probe_argument() {
        let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tsc-probe");
        assert_eq!(probe_host_hz(&binary).await.unwrap(), 4_291_956_000);
    }

    #[tokio::test]
    async fn host_probe_reports_process_failures() {
        let error = probe_host_hz(Path::new("/bin/false")).await.unwrap_err();
        assert!(error.to_string().contains("--probe failed"), "{error:#}");
        assert!(probe_host_hz(Path::new("/no/vmlab-hv")).await.is_err());
    }
}
