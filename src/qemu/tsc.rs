//! The host TSC frequency a `tsc = "host"` VM is pinned at (§5.2).
//!
//! KVM knows it (`KVM_GET_TSC_KHZ`), but reaching it directly takes an ioctl
//! on a throwaway VM, and this crate denies `unsafe`. QEMU asks KVM the same
//! question when it creates a vCPU and reports the answer as the CPU's
//! `tsc-frequency` property, so the probe is a paused emulator with no
//! devices, asked once over QMP on its stdio and told to quit. Being the
//! emulator the VM itself runs under, it reads exactly the value that VM
//! would see.
//!
//! The lab daemon pins the answer per VM, at its first KVM start, rather than
//! reading it afresh each time: the host kernel recalibrates its TSC at every
//! boot, and QEMU refuses to load an online snapshot whose recorded frequency
//! differs from the one the VM was started with. KVM runs a pinned frequency
//! within its tolerance of the host's without scaling, so an old pin on a
//! recalibrated host costs nothing.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// How long the probe may take, spawn to quit. A paused, device-less
/// emulator answers in well under a second.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The probe emulator's arguments: the smallest machine that creates a KVM
/// vCPU, paused, with QMP on stdio and nothing else.
pub fn probe_args() -> Vec<String> {
    [
        "-nodefaults",
        "-display",
        "none",
        "-machine",
        "q35",
        "-accel",
        "kvm",
        "-cpu",
        "host",
        "-m",
        "16M",
        "-S",
        "-qmp",
        "stdio",
    ]
    .map(str::to_string)
    .to_vec()
}

/// Ask `binary` (a `qemu-system-x86_64`) for the host TSC frequency, in Hz.
pub async fn probe_host_hz(binary: &str) -> Result<u64> {
    tokio::time::timeout(PROBE_TIMEOUT, probe(binary))
        .await
        .map_err(|_| {
            anyhow!(
                "{binary} did not report the TSC frequency within {}s",
                PROBE_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("reading the host TSC frequency through {binary}"))
}

async fn probe(binary: &str) -> Result<u64> {
    let mut child = Command::new(binary)
        .args(probe_args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {binary}"))?;
    let mut stdin = child.stdin.take().context("probe stdin")?;
    let mut lines = BufReader::new(child.stdout.take().context("probe stdout")?).lines();

    let mut ask = async |command: Value, id: &str| -> Result<Value> {
        let mut line = serde_json::to_vec(&command)?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        while let Some(line) = lines.next_line().await? {
            if let Some(answer) = reply(&line, id) {
                return answer;
            }
        }
        bail!("the emulator exited before answering `{id}`")
    };
    let answer = async {
        ask(
            serde_json::json!({"execute": "qmp_capabilities", "id": "caps"}),
            "caps",
        )
        .await?;
        let cpus = ask(
            serde_json::json!({"execute": "query-cpus-fast", "id": "cpus"}),
            "cpus",
        )
        .await?;
        let path = first_cpu_path(&cpus)?;
        let hz = ask(
            serde_json::json!({
                "execute": "qom-get",
                "arguments": {"path": path, "property": "tsc-frequency"},
                "id": "tsc",
            }),
            "tsc",
        )
        .await?;
        frequency(&hz)
    }
    .await;
    let _ = ask(serde_json::json!({"execute": "quit", "id": "quit"}), "quit").await;
    drop(ask);
    match answer {
        Ok(hz) => {
            let _ = child.wait().await;
            Ok(hz)
        }
        Err(e) => {
            let _ = child.start_kill();
            let out = child.wait_with_output().await?;
            let stderr = String::from_utf8_lossy(&out.stderr);
            match stderr.trim() {
                "" => Err(e),
                said => Err(e.context(said.to_string())),
            }
        }
    }
}

/// The answer to the command sent with `id`, from one QMP line: its
/// `return`, or its `error` as an error. `None` for anything else — the
/// greeting, events, another command's reply.
fn reply(line: &str, id: &str) -> Option<Result<Value>> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("id").and_then(Value::as_str) != Some(id) {
        return None;
    }
    Some(match (v.get("return"), v.get("error")) {
        (Some(ret), _) => Ok(ret.clone()),
        (None, Some(err)) => Err(anyhow!(
            "{}",
            err.get("desc")
                .and_then(Value::as_str)
                .unwrap_or("QMP error")
        )),
        (None, None) => Err(anyhow!("a QMP reply with neither `return` nor `error`")),
    })
}

/// The QOM path of the first vCPU in a `query-cpus-fast` answer.
fn first_cpu_path(cpus: &Value) -> Result<String> {
    cpus.as_array()
        .and_then(|cpus| cpus.first())
        .and_then(|cpu| cpu.get("qom-path"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("query-cpus-fast named no vCPU"))
}

/// The frequency a `qom-get … tsc-frequency` answered, refusing zero: QEMU
/// reports zero when KVM gave it no TSC frequency to report.
fn frequency(v: &Value) -> Result<u64> {
    match v.as_u64() {
        Some(0) => bail!("KVM reports no TSC frequency"),
        Some(hz) => Ok(hz),
        None => bail!("tsc-frequency is not a number: {v}"),
    }
}

/// The frequency pinned in `file` — a decimal Hz count — or `None` when
/// nothing is pinned there yet.
pub fn read_pin(file: &Path) -> Result<Option<u64>> {
    match std::fs::read_to_string(file) {
        Ok(text) => text
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|hz| *hz > 0)
            .map(Some)
            .ok_or_else(|| anyhow!("{} holds no TSC frequency", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

/// Pin `hz` in `file`.
pub fn write_pin(file: &Path, hz: u64) -> Result<()> {
    std::fs::write(file, format!("{hz}\n")).with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_matched_by_its_id() {
        assert!(reply(r#"{"QMP": {"version": {}}}"#, "cpus").is_none());
        assert!(reply(r#"{"event": "STOP"}"#, "cpus").is_none());
        assert!(reply(r#"{"return": {}, "id": "caps"}"#, "cpus").is_none());
        let ok = reply(r#"{"return": 4291956000, "id": "tsc"}"#, "tsc").unwrap();
        assert_eq!(frequency(&ok.unwrap()).unwrap(), 4_291_956_000);
        let err = reply(
            r#"{"error": {"class": "GenericError", "desc": "no such property"}, "id": "tsc"}"#,
            "tsc",
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(err.to_string(), "no such property");
    }

    #[test]
    fn the_first_vcpu_is_asked() {
        let cpus = serde_json::json!([
            {"cpu-index": 0, "qom-path": "/machine/unattached/device[0]"},
            {"cpu-index": 1, "qom-path": "/machine/unattached/device[1]"},
        ]);
        assert_eq!(
            first_cpu_path(&cpus).unwrap(),
            "/machine/unattached/device[0]"
        );
        assert!(first_cpu_path(&serde_json::json!([])).is_err());
    }

    #[test]
    fn a_zero_frequency_is_refused() {
        assert!(frequency(&serde_json::json!(0)).is_err());
        assert!(frequency(&serde_json::json!("fast")).is_err());
    }

    #[test]
    fn the_probe_creates_a_paused_kvm_vcpu_and_nothing_else() {
        let args = probe_args().join(" ");
        assert_eq!(
            args,
            "-nodefaults -display none -machine q35 -accel kvm -cpu host -m 16M -S -qmp stdio"
        );
    }

    #[test]
    fn a_pin_round_trips_and_a_missing_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tsc-hz");
        assert_eq!(read_pin(&file).unwrap(), None);
        write_pin(&file, 4_291_956_000).unwrap();
        assert_eq!(read_pin(&file).unwrap(), Some(4_291_956_000));
        std::fs::write(&file, "garbage").unwrap();
        assert!(read_pin(&file).is_err());
    }

    /// The real probe, where this host has KVM and the emulator: it answers
    /// a plausible frequency (between 100 MHz and 10 GHz).
    #[tokio::test]
    async fn the_probe_reads_this_hosts_tsc_where_kvm_runs() {
        if !crate::qemu::kvm_usable("x86_64") || which("qemu-system-x86_64").is_none() {
            return;
        }
        let hz = probe_host_hz("qemu-system-x86_64").await.unwrap();
        assert!((100_000_000..10_000_000_000).contains(&hz), "{hz}");
    }

    fn which(bin: &str) -> Option<std::path::PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    }
}
