//! The supervisor's GPU keepers (PRD §5.2, ADR-0018): one
//! `vmlab-gpu-keepawake` per host GPU that a lab daemon asked to hold,
//! started the first time a `virgl`/`vulkan` VM starts on that GPU and kept
//! until the supervisor stops. A lab's `down` then `up` never leaves the GPU
//! without a client, so the driver never runtime-suspends it between them.
//!
//! The supervisor owns them rather than a lab daemon because the gap to
//! close is the one between labs: a lab daemon exits with its lab. A keeper
//! that exits by itself is logged and forgotten, so the next VM start on its
//! GPU starts another. The helper exits on its own once this process is no
//! longer its parent, so a supervisor that dies without a teardown does not
//! leave the GPU held.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::Mutex;

use crate::proto::CommandError;
use crate::qemu::keepawake;

/// How long a new helper has to open the GPU and say so.
const READY_WAIT: Duration = Duration::from_secs(15);

/// One running helper.
#[derive(Debug, Clone)]
struct Keeper {
    gpu: String,
    pid: u32,
}

/// Every keeper this supervisor runs, by PCI address.
#[derive(Default)]
pub struct Keepers {
    held: Arc<Mutex<HashMap<String, Keeper>>>,
}

impl Keepers {
    /// Hold the GPU at `address` (declared as `gpu`) awake, starting its
    /// helper unless one already runs. Answers with the helper's pid; a
    /// helper that cannot open the GPU is `failed`, with what it said.
    pub async fn ensure(&self, gpu: &str, address: &str) -> Result<u32, CommandError> {
        let mut held = self.held.lock().await;
        if let Some(k) = held.get(address)
            && super::process_running(k.pid)
        {
            return Ok(k.pid);
        }
        let helper = keepawake::path(&crate::paths::data_dir());
        keepawake::install(&helper).map_err(|e| {
            CommandError::failed(format!(
                "host GPU `{gpu}`: installing the keep-awake helper: {e:#}"
            ))
        })?;
        let mut child = tokio::process::Command::new(&helper)
            .args(keepawake::args(address, std::process::id()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                CommandError::failed(format!(
                    "host GPU `{gpu}`: starting {}: {e}",
                    helper.display()
                ))
            })?;
        let pid = child.id().unwrap_or_default();
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let first = tokio::time::timeout(READY_WAIT, stdout.next_line()).await;
        let ready = match &first {
            Ok(Ok(Some(line))) => line.strip_prefix(keepawake::READY).map(str::to_string),
            _ => None,
        };
        let Some(ready) = ready else {
            let _ = child.start_kill();
            let mut said = String::new();
            let _ = tokio::time::timeout(Duration::from_secs(2), stderr.read_to_string(&mut said))
                .await;
            let said = said.trim();
            let why = if first.is_err() {
                format!("it did not open the GPU within {}s", READY_WAIT.as_secs())
            } else if said.is_empty() {
                "it exited without opening the GPU".to_string()
            } else {
                said.to_string()
            };
            return Err(CommandError::failed(format!(
                "host GPU `{gpu}` ({address}): the keep-awake helper failed: {why}"
            )));
        };
        tracing::info!("holding host GPU {gpu} awake: pid {pid}, {ready}");
        held.insert(
            address.to_string(),
            Keeper {
                gpu: gpu.to_string(),
                pid,
            },
        );
        let map = self.held.clone();
        let (gpu, address) = (gpu.to_string(), address.to_string());
        tokio::spawn(async move {
            let mut said = String::new();
            let _ = stderr.read_to_string(&mut said).await;
            while let Ok(Some(_)) = stdout.next_line().await {}
            let status = child.wait().await;
            let mut held = map.lock().await;
            if held.get(&address).is_some_and(|k| k.pid == pid) {
                held.remove(&address);
                let said = said.trim();
                let status = match status {
                    Ok(s) => s.to_string(),
                    Err(e) => e.to_string(),
                };
                tracing::warn!(
                    "the keep-awake helper for host GPU {gpu} ({address}) exited ({status}){}{said}; \
                     the next VM start on it starts another",
                    if said.is_empty() { "" } else { ": " }
                );
            }
        });
        Ok(pid)
    }

    /// Stop every helper, waiting briefly for each to release its GPU.
    pub async fn stop_all(&self) {
        let keepers: Vec<Keeper> = self.held.lock().await.drain().map(|(_, k)| k).collect();
        for k in &keepers {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(k.pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        let until = std::time::Instant::now() + Duration::from_secs(3);
        for k in keepers {
            while super::process_running(k.pid) && std::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tracing::info!("released host GPU {}", k.gpu);
        }
    }
}
