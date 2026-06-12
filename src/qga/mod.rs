//! QEMU Guest Agent (QGA) client.
//!
//! Talks to `qemu-ga` inside a VM over the virtio-serial chardev socket
//! (PRD §7.4: readiness detection, command execution, file copy, graceful
//! shutdown, IP reporting). The wire format is newline-delimited JSON like
//! QMP, but with no greeting, no events, and strictly one in-flight
//! command at a time (calls are serialised via an async mutex).
//!
//! Two QGA realities shape this client:
//!
//! - **The channel can be desynchronised** (stale bytes from a previous
//!   client, an agent restart mid-message). Recovery is
//!   `guest-sync-delimited`: send a `0xFF` byte plus a sync command with a
//!   random id, then discard input until a `0xFF` sentinel followed by the
//!   matching `{"return": id}`. The client resyncs on the first command of
//!   a fresh connection and again after any timeout.
//! - **The agent may simply never answer** (not installed, guest still
//!   booting), so every call takes an explicit timeout, and a timed-out
//!   client remains usable for subsequent calls.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::time::Instant;

#[cfg(test)]
mod tests;

/// Sentinel byte framing `guest-sync-delimited` responses.
const SYNC_SENTINEL: u8 = 0xFF;

/// Chunk size for `guest-file-write` payloads (pre-base64). QGA accepts up
/// to 48MiB per request, but modest chunks keep each request comfortably
/// inside per-call timeouts.
const FILE_WRITE_CHUNK: usize = 48 * 1024;

/// Read size for `guest-file-read` requests.
const FILE_READ_COUNT: u64 = 1 << 20;

/// Poll interval while waiting on `guest-exec-status`.
const EXEC_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Errors produced by the guest agent client.
#[derive(Debug, thiserror::Error)]
pub enum GaError {
    /// Underlying socket I/O failed.
    #[error("guest agent i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A message could not be serialised or deserialised.
    #[error("guest agent json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The agent did not answer within the caller's timeout.
    #[error("guest agent timed out")]
    Timeout,

    /// The connection is closed (EOF or fatal I/O error).
    #[error("guest agent connection closed")]
    Closed,

    /// The agent rejected a command with `{"error": {"class", "desc"}}`.
    #[error("guest agent command failed: {class}: {desc}")]
    Command { class: String, desc: String },

    /// The agent answered with something that violates the protocol.
    #[error("guest agent protocol error: {0}")]
    Protocol(String),
}

/// Result of [`GaClient::exec`].
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Process exit code. If the process died from a signal, this is
    /// `128 + signal` (unix convention).
    pub exit_code: i32,
    /// Captured stdout (empty unless `capture` was set).
    pub stdout: Vec<u8>,
    /// Captured stderr (empty unless `capture` was set).
    pub stderr: Vec<u8>,
}

/// A guest network interface from `guest-network-get-interfaces`.
#[derive(Debug, Clone)]
pub struct GaInterface {
    /// Interface name inside the guest (e.g. `eth0`, `Ethernet`).
    pub name: String,
    /// MAC address, if the agent reports one.
    pub hardware_address: Option<String>,
    /// Assigned addresses as `(address, type)` where type is `"ipv4"` or
    /// `"ipv6"`.
    pub ips: Vec<(String, String)>,
}

/// Connection state. `needs_sync` is set on fresh connections and after
/// any timeout: a late reply may still be in flight, so the next command
/// must resynchronise the channel first. `pending_sync` holds the id of a
/// sync request already written but not yet matched against a response.
struct GaInner {
    stream: BufReader<UnixStream>,
    needs_sync: bool,
    pending_sync: Option<u32>,
    closed: bool,
}

/// Async QEMU Guest Agent client over a unix socket.
///
/// Cheap to clone (`Arc` inner). QGA is strictly request/response with one
/// command in flight, so all calls serialise on an internal async mutex.
#[derive(Clone)]
pub struct GaClient {
    inner: Arc<Mutex<GaInner>>,
}

impl GaClient {
    /// Connect to a guest agent unix socket.
    ///
    /// The `guest-sync-delimited` request (0xFF-prefixed, random id) is
    /// written immediately, but its response is awaited as part of the
    /// first command — within that command's timeout — so `connect`
    /// cannot hang on a VM whose agent is absent or not yet running.
    pub async fn connect(path: &Path) -> Result<GaClient, GaError> {
        let stream = UnixStream::connect(path).await?;
        let mut inner = GaInner {
            stream: BufReader::new(stream),
            needs_sync: true,
            pending_sync: None,
            closed: false,
        };
        inner.send_sync().await?;
        Ok(GaClient {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Execute a raw guest agent command and return its `return` value.
    ///
    /// Resynchronises the channel first if needed. `timeout` bounds the
    /// whole call including any resync; on expiry the call returns
    /// [`GaError::Timeout`] and the client stays usable (the next call
    /// resyncs).
    pub async fn execute(
        &self,
        command: &str,
        args: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, GaError> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().await;
        if inner.closed {
            return Err(GaError::Closed);
        }
        if inner.needs_sync {
            inner.sync_delimited(deadline).await?;
        }
        inner.call(command, args, deadline).await
    }

    /// Probe agent liveness with `guest-ping`. `false` on any failure —
    /// most commonly a timeout because the agent is not (yet) running.
    pub async fn ping(&self, timeout: Duration) -> bool {
        self.execute("guest-ping", None, timeout).await.is_ok()
    }

    /// Run a command in the guest via `guest-exec`, polling
    /// `guest-exec-status` until it exits. `timeout` bounds the entire
    /// flow (spawn + run + output collection).
    pub async fn exec(
        &self,
        cmd: &str,
        args: &[&str],
        capture: bool,
        timeout: Duration,
    ) -> Result<ExecResult, GaError> {
        let deadline = Instant::now() + timeout;
        let spawn = self
            .execute(
                "guest-exec",
                Some(json!({"path": cmd, "arg": args, "capture-output": capture})),
                remaining(deadline)?,
            )
            .await?;
        let pid = spawn.get("pid").and_then(|p| p.as_i64()).ok_or_else(|| {
            GaError::Protocol(format!("guest-exec response missing pid: {spawn}"))
        })?;

        loop {
            let status = self
                .execute(
                    "guest-exec-status",
                    Some(json!({"pid": pid})),
                    remaining(deadline)?,
                )
                .await?;
            if status.get("exited").and_then(|e| e.as_bool()) == Some(true) {
                let exit_code = match (
                    status.get("exitcode").and_then(|c| c.as_i64()),
                    status.get("signal").and_then(|s| s.as_i64()),
                ) {
                    (Some(code), _) => code as i32,
                    (None, Some(signal)) => 128 + signal as i32,
                    (None, None) => -1,
                };
                return Ok(ExecResult {
                    exit_code,
                    stdout: decode_b64_field(&status, "out-data")?,
                    stderr: decode_b64_field(&status, "err-data")?,
                });
            }
            let nap = EXEC_POLL_INTERVAL.min(remaining(deadline)?);
            tokio::time::sleep(nap).await;
        }
    }

    /// Write `data` to `guest_path` inside the guest (`guest-file-open`
    /// mode `"w"`, chunked `guest-file-write`, `guest-file-close`).
    /// Parent directories must already exist in the guest. `timeout`
    /// applies per underlying agent command, not to the whole transfer.
    pub async fn file_write(
        &self,
        guest_path: &str,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), GaError> {
        self.file_write_mode(guest_path, data, "w", timeout).await
    }

    /// Append `data` to `guest_path` inside the guest (mode `"a"`),
    /// creating the file if absent. Lets callers move a large file in
    /// several transfers without holding it all in one message.
    pub async fn file_append(
        &self,
        guest_path: &str,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), GaError> {
        self.file_write_mode(guest_path, data, "a", timeout).await
    }

    async fn file_write_mode(
        &self,
        guest_path: &str,
        data: &[u8],
        mode: &str,
        timeout: Duration,
    ) -> Result<(), GaError> {
        let handle = self.file_open(guest_path, mode, timeout).await?;
        let result = self.write_chunks(handle, data, timeout).await;
        let close_result = self.file_close(handle, timeout).await;
        result.and(close_result)
    }

    /// Guest OS identification via `guest-get-osinfo`: the raw response
    /// object (`id`, `name`, `kernel-release`, …). On Windows guests
    /// `id` is `"mswindows"`.
    pub async fn get_osinfo(&self, timeout: Duration) -> Result<Value, GaError> {
        self.execute("guest-get-osinfo", None, timeout).await
    }

    /// Read the contents of `guest_path` from the guest
    /// (`guest-file-open` mode `"r"`, `guest-file-read` loop until EOF,
    /// `guest-file-close`). `timeout` applies per underlying agent
    /// command, not to the whole transfer.
    pub async fn file_read(&self, guest_path: &str, timeout: Duration) -> Result<Vec<u8>, GaError> {
        let handle = self.file_open(guest_path, "r", timeout).await?;
        let result = self.read_to_end(handle, timeout).await;
        let close_result = self.file_close(handle, timeout).await;
        match result {
            Ok(data) => close_result.map(|()| data),
            Err(e) => Err(e),
        }
    }

    /// Ask the guest to shut down via `guest-shutdown`. `mode` is
    /// `"powerdown"`, `"reboot"`, or `"halt"`.
    ///
    /// QGA defines this command as returning no success response — the
    /// agent (and the whole socket) may vanish before any bytes come
    /// back, so a timeout, EOF, or I/O error here counts as success. Only
    /// an explicit error response from the agent is reported.
    pub async fn shutdown(&self, mode: &str, timeout: Duration) -> Result<(), GaError> {
        match self
            .execute("guest-shutdown", Some(json!({"mode": mode})), timeout)
            .await
        {
            Ok(_) => Ok(()),
            Err(GaError::Timeout | GaError::Closed | GaError::Io(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// List guest network interfaces with their MAC and IP addresses
    /// (`guest-network-get-interfaces`).
    pub async fn network_interfaces(&self, timeout: Duration) -> Result<Vec<GaInterface>, GaError> {
        #[derive(Deserialize)]
        struct WireIp {
            #[serde(rename = "ip-address")]
            ip_address: String,
            #[serde(rename = "ip-address-type")]
            ip_address_type: String,
        }
        #[derive(Deserialize)]
        struct WireInterface {
            name: String,
            #[serde(rename = "hardware-address")]
            hardware_address: Option<String>,
            #[serde(rename = "ip-addresses", default)]
            ip_addresses: Vec<WireIp>,
        }

        let value = self
            .execute("guest-network-get-interfaces", None, timeout)
            .await?;
        let wire: Vec<WireInterface> = serde_json::from_value(value)?;
        Ok(wire
            .into_iter()
            .map(|iface| GaInterface {
                name: iface.name,
                hardware_address: iface.hardware_address,
                ips: iface
                    .ip_addresses
                    .into_iter()
                    .map(|ip| (ip.ip_address, ip.ip_address_type))
                    .collect(),
            })
            .collect())
    }

    // --- file plumbing -------------------------------------------------------

    async fn file_open(&self, path: &str, mode: &str, timeout: Duration) -> Result<i64, GaError> {
        let value = self
            .execute(
                "guest-file-open",
                Some(json!({"path": path, "mode": mode})),
                timeout,
            )
            .await?;
        value.as_i64().ok_or_else(|| {
            GaError::Protocol(format!(
                "guest-file-open returned non-integer handle: {value}"
            ))
        })
    }

    async fn file_close(&self, handle: i64, timeout: Duration) -> Result<(), GaError> {
        self.execute("guest-file-close", Some(json!({"handle": handle})), timeout)
            .await
            .map(|_| ())
    }

    async fn write_chunks(
        &self,
        handle: i64,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), GaError> {
        for chunk in data.chunks(FILE_WRITE_CHUNK) {
            let value = self
                .execute(
                    "guest-file-write",
                    Some(json!({"handle": handle, "buf-b64": BASE64.encode(chunk)})),
                    timeout,
                )
                .await?;
            let written = value.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
            if written != chunk.len() as u64 {
                return Err(GaError::Protocol(format!(
                    "guest-file-write short write: {written} of {} bytes",
                    chunk.len()
                )));
            }
        }
        Ok(())
    }

    async fn read_to_end(&self, handle: i64, timeout: Duration) -> Result<Vec<u8>, GaError> {
        let mut data = Vec::new();
        loop {
            let value = self
                .execute(
                    "guest-file-read",
                    Some(json!({"handle": handle, "count": FILE_READ_COUNT})),
                    timeout,
                )
                .await?;
            data.extend_from_slice(&decode_b64_field(&value, "buf-b64")?);
            if value.get("eof").and_then(|e| e.as_bool()) == Some(true) {
                return Ok(data);
            }
            if value.get("count").and_then(|c| c.as_u64()) == Some(0) {
                // Defensive: an agent that reports neither progress nor EOF
                // would loop forever.
                return Err(GaError::Protocol(
                    "guest-file-read returned no data and no eof".to_string(),
                ));
            }
        }
    }
}

impl GaInner {
    /// Send one command and wait for its response line.
    async fn call(
        &mut self,
        command: &str,
        args: Option<Value>,
        deadline: Instant,
    ) -> Result<Value, GaError> {
        let mut msg = json!({"execute": command});
        if let Some(arguments) = args {
            msg["arguments"] = arguments;
        }
        self.write_message(&msg, None).await?;

        loop {
            let line = self.read_line(deadline).await?;
            // Tolerate stray sentinel bytes glued to the response.
            let cleaned: Vec<u8> = line
                .iter()
                .copied()
                .filter(|&b| b != SYNC_SENTINEL)
                .collect();
            let Some(value) = parse_json_bytes(&cleaned) else {
                continue; // residual garbage between commands
            };
            if let Some(error) = value.get("error") {
                return Err(command_error(error));
            }
            if let Some(ret) = value.get("return") {
                return Ok(ret.clone());
            }
            // A JSON line that is neither return nor error: stale noise.
        }
    }

    /// Resynchronise the channel with `guest-sync-delimited`: a random id
    /// prefixed by 0xFF; all input is discarded until a 0xFF sentinel
    /// followed by the matching `{"return": id}`. Reuses a sync request
    /// already written by [`GaInner::send_sync`] if one is outstanding.
    async fn sync_delimited(&mut self, deadline: Instant) -> Result<(), GaError> {
        let id = match self.pending_sync {
            Some(id) => id,
            None => self.send_sync().await?,
        };
        match self.await_sync(id, deadline).await {
            Ok(()) => {
                self.needs_sync = false;
                self.pending_sync = None;
                Ok(())
            }
            Err(e) => {
                // On timeout the agent may answer this id later, but a
                // resend with a fresh id is more robust (the old response
                // simply fails the id match and is discarded).
                self.pending_sync = None;
                Err(e)
            }
        }
    }

    /// Write a `guest-sync-delimited` request (0xFF prefix, random id)
    /// without waiting for the response. Returns the id to match.
    async fn send_sync(&mut self) -> Result<u32, GaError> {
        let id: u32 = rand::random();
        let msg = json!({"execute": "guest-sync-delimited", "arguments": {"id": id}});
        self.write_message(&msg, Some(SYNC_SENTINEL)).await?;
        self.pending_sync = Some(id);
        Ok(id)
    }

    /// Discard input until a 0xFF sentinel followed by `{"return": id}`.
    async fn await_sync(&mut self, id: u32, deadline: Instant) -> Result<(), GaError> {
        loop {
            let line = self.read_line(deadline).await?;
            // The response JSON follows the *last* sentinel on the line;
            // anything before it (including earlier sentinels) is garbage.
            let Some(pos) = line.iter().rposition(|&b| b == SYNC_SENTINEL) else {
                continue; // pre-sentinel garbage line
            };
            let Some(value) = parse_json_bytes(&line[pos + 1..]) else {
                continue;
            };
            if value.get("return").and_then(|r| r.as_u64()) == Some(u64::from(id)) {
                return Ok(());
            }
            // Stale sync response with a different id: keep discarding.
        }
    }

    /// Write a JSON message as one line, optionally preceded by a raw
    /// prefix byte. I/O failure poisons the connection.
    async fn write_message(&mut self, msg: &Value, prefix: Option<u8>) -> Result<(), GaError> {
        let mut bytes = Vec::with_capacity(128);
        if let Some(p) = prefix {
            bytes.push(p);
        }
        serde_json::to_writer(&mut bytes, msg)?;
        bytes.push(b'\n');
        if let Err(e) = async {
            self.stream.write_all(&bytes).await?;
            self.stream.flush().await
        }
        .await
        {
            self.closed = true;
            return Err(e.into());
        }
        Ok(())
    }

    /// Read one line, honouring `deadline`. Timeout marks the channel
    /// dirty (a late reply may arrive any time); EOF or I/O error poisons
    /// the connection.
    async fn read_line(&mut self, deadline: Instant) -> Result<Vec<u8>, GaError> {
        let mut buf = Vec::new();
        match tokio::time::timeout_at(deadline, self.stream.read_until(b'\n', &mut buf)).await {
            Err(_) => {
                self.needs_sync = true;
                Err(GaError::Timeout)
            }
            Ok(Err(e)) => {
                self.closed = true;
                Err(e.into())
            }
            Ok(Ok(0)) => {
                self.closed = true;
                Err(GaError::Closed)
            }
            Ok(Ok(_)) => Ok(buf),
        }
    }
}

/// Time left until `deadline`, or [`GaError::Timeout`] if it has passed.
fn remaining(deadline: Instant) -> Result<Duration, GaError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(GaError::Timeout);
    }
    Ok(deadline - now)
}

/// Parse a byte slice as JSON after trimming whitespace; `None` if empty
/// or unparseable.
fn parse_json_bytes(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// Decode an optional base64-encoded string field from a QGA response.
fn decode_b64_field(value: &Value, field: &str) -> Result<Vec<u8>, GaError> {
    match value.get(field).and_then(|v| v.as_str()) {
        Some(encoded) => BASE64
            .decode(encoded)
            .map_err(|e| GaError::Protocol(format!("invalid base64 in {field}: {e}"))),
        None => Ok(Vec::new()),
    }
}

/// Build a [`GaError::Command`] from a QGA `error` object.
fn command_error(error: &Value) -> GaError {
    GaError::Command {
        class: error
            .get("class")
            .and_then(|c| c.as_str())
            .unwrap_or("GenericError")
            .to_string(),
        desc: error
            .get("desc")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string(),
    }
}
