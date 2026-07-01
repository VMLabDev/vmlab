//! QMP client implementation: connection handshake, request/response
//! correlation by id, event fan-out, and typed command wrappers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::task::JoinHandle;

use super::error::QmpError;
use super::types::{QmpEvent, RunState};

/// Maximum value on the abs axes of QEMU's `input-send-event` (per QAPI,
/// absolute pointer coordinates are scaled to a 0..32767 range).
const INPUT_ABS_MAX: u64 = 32767;

/// Poll interval for [`QmpClient::wait_for_job`].
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Capacity of the event broadcast channel; slow subscribers observe
/// `RecvError::Lagged` rather than blocking the reader task.
const EVENT_CHANNEL_CAPACITY: usize = 256;

type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, QmpError>>>;

/// Requests in flight, keyed by command id.
///
/// `None` means the connection is closed: no further requests are accepted
/// and the reader task has drained (or will never populate) the map.
struct Shared {
    pending: StdMutex<Option<PendingMap>>,
}

impl Shared {
    /// Fail every pending request with [`QmpError::Closed`] and refuse
    /// future ones.
    fn close(&self) {
        let drained = self.pending.lock().expect("qmp pending lock").take();
        if let Some(map) = drained {
            for (_, tx) in map {
                let _ = tx.send(Err(QmpError::Closed));
            }
        }
    }
}

struct Inner {
    writer: Mutex<OwnedWriteHalf>,
    shared: Arc<Shared>,
    event_tx: broadcast::Sender<QmpEvent>,
    next_id: AtomicU64,
    reader_task: StdMutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    /// Last clone gone: stop the background reader so it does not outlive
    /// the client (it would otherwise run until the socket EOFs).
    fn drop(&mut self) {
        if let Ok(mut task) = self.reader_task.lock()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}

/// Async QMP client over a unix socket.
///
/// Cheap to clone (`Arc` inner); all clones share the connection, so the
/// lab daemon can hand the same client to power management, input
/// injection, and the event pump concurrently. Writes are serialised via
/// a mutex on the write half; a background reader task routes responses to
/// their callers by id and events to the broadcast channel.
#[derive(Clone)]
pub struct QmpClient {
    inner: Arc<Inner>,
}

impl QmpClient {
    /// Connect to a QMP unix socket, read the greeting, and negotiate
    /// capabilities. On return the client is ready to execute commands.
    pub async fn connect(path: &Path) -> Result<QmpClient, QmpError> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;

        // Greeting: {"QMP": {"version": ..., "capabilities": [...]}}
        let greeting = read_json_line(&mut reader).await?;
        if greeting.get("QMP").is_none() {
            return Err(QmpError::Protocol(format!(
                "expected QMP greeting, got: {greeting}"
            )));
        }

        // Capability negotiation. Performed inline before the reader task
        // exists: QEMU emits no events until negotiation completes, so the
        // next line is necessarily our response.
        write_json_line(&mut writer, &json!({"execute": "qmp_capabilities"})).await?;
        let resp = read_json_line(&mut reader).await?;
        if let Some(error) = resp.get("error") {
            return Err(QmpError::from_error_object(error));
        }

        let shared = Arc::new(Shared {
            pending: StdMutex::new(Some(HashMap::new())),
        });
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let reader_task = tokio::spawn(reader_loop(reader, Arc::clone(&shared), event_tx.clone()));

        Ok(QmpClient {
            inner: Arc::new(Inner {
                writer: Mutex::new(writer),
                shared,
                event_tx,
                next_id: AtomicU64::new(1),
                reader_task: StdMutex::new(Some(reader_task)),
            }),
        })
    }

    /// Subscribe to asynchronous QMP events.
    ///
    /// Only events arriving after the call are delivered; a lagging
    /// receiver gets `RecvError::Lagged` and may resubscribe.
    pub fn subscribe_events(&self) -> broadcast::Receiver<QmpEvent> {
        self.inner.event_tx.subscribe()
    }

    /// Execute a raw QMP command and return its `return` value.
    ///
    /// Sends `{"execute": command, "arguments": args, "id": n}` and waits
    /// for the response carrying the same id. A QMP-level
    /// `{"error": {...}}` becomes [`QmpError::Command`].
    pub async fn execute(&self, command: &str, args: Option<Value>) -> Result<Value, QmpError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({"execute": command, "id": id});
        if let Some(arguments) = args {
            msg["arguments"] = arguments;
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.inner.shared.pending.lock().expect("qmp pending lock");
            match guard.as_mut() {
                Some(map) => {
                    map.insert(id, tx);
                }
                None => return Err(QmpError::Closed),
            }
        }

        let write_result = {
            let mut writer = self.inner.writer.lock().await;
            write_json_line(&mut *writer, &msg).await
        };
        if let Err(e) = write_result {
            // Nobody will ever answer this id; un-register it.
            if let Some(map) = self
                .inner
                .shared
                .pending
                .lock()
                .expect("qmp pending lock")
                .as_mut()
            {
                map.remove(&id);
            }
            return Err(e);
        }

        match rx.await {
            Ok(result) => result,
            Err(_) => Err(QmpError::Closed),
        }
    }

    /// Gracefully shut the connection down: stops the reader task, closes
    /// the write half, and fails all in-flight requests with
    /// [`QmpError::Closed`]. Affects every clone of this client.
    /// (Production drops the last clone instead — see `Drop` on `Inner` —
    /// but the tests close explicitly to assert in-flight behaviour.)
    #[allow(dead_code)]
    pub async fn close(&self) {
        let task = self
            .inner
            .reader_task
            .lock()
            .expect("qmp reader task lock")
            .take();
        if let Some(task) = task {
            task.abort();
        }
        {
            let mut writer = self.inner.writer.lock().await;
            let _ = writer.shutdown().await;
        }
        self.inner.shared.close();
    }

    // --- power management -------------------------------------------------

    /// Request an ACPI powerdown (`system_powerdown`).
    pub async fn system_powerdown(&self) -> Result<(), QmpError> {
        self.execute("system_powerdown", None).await.map(|_| ())
    }

    /// Terminate the QEMU process (`quit`).
    pub async fn quit(&self) -> Result<(), QmpError> {
        self.execute("quit", None).await.map(|_| ())
    }

    /// Pause guest execution (`stop`).
    pub async fn stop(&self) -> Result<(), QmpError> {
        self.execute("stop", None).await.map(|_| ())
    }

    /// Resume guest execution (`cont`).
    pub async fn cont(&self) -> Result<(), QmpError> {
        self.execute("cont", None).await.map(|_| ())
    }

    /// Query the VM run state (`query-status`). (The daemon watches run
    /// state via events; the QMP tests poll through this.)
    #[allow(dead_code)]
    pub async fn query_status(&self) -> Result<RunState, QmpError> {
        let value = self.execute("query-status", None).await?;
        let status = value
            .get("status")
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                QmpError::Protocol(format!("query-status response missing status: {value}"))
            })?;
        Ok(RunState::from_status(status))
    }

    // --- screen and input --------------------------------------------------

    /// Dump the guest display to `filename` on the host (`screendump`,
    /// PPM format).
    pub async fn screendump(&self, filename: &Path) -> Result<(), QmpError> {
        let args = json!({"filename": filename.to_string_lossy()});
        self.execute("screendump", Some(args)).await.map(|_| ())
    }

    /// Press (and release) a key chord via `send-key`.
    ///
    /// `keys` are QMP qcode names (e.g. `["ctrl", "alt", "delete"]`); they
    /// are pressed in order, held for `hold_time_ms` (QEMU default 100ms),
    /// and released in reverse order.
    pub async fn send_key(&self, keys: &[&str], hold_time_ms: Option<u32>) -> Result<(), QmpError> {
        let key_values: Vec<Value> = keys
            .iter()
            .map(|k| json!({"type": "qcode", "data": k}))
            .collect();
        let mut args = json!({"keys": key_values});
        if let Some(hold) = hold_time_ms {
            args["hold-time"] = hold.into();
        }
        self.execute("send-key", Some(args)).await.map(|_| ())
    }

    /// Inject raw input events (`input-send-event`).
    pub async fn input_send_event(&self, events: Vec<Value>) -> Result<(), QmpError> {
        self.execute("input-send-event", Some(json!({"events": events})))
            .await
            .map(|_| ())
    }

    /// Move the absolute pointer to `(x, y)` on a display of
    /// `max_x` x `max_y` pixels; coordinates are scaled to QEMU's
    /// 0..32767 abs axis range.
    pub async fn mouse_move_abs(
        &self,
        x: u32,
        y: u32,
        max_x: u32,
        max_y: u32,
    ) -> Result<(), QmpError> {
        let scale = |value: u32, max: u32| -> u64 {
            if max == 0 {
                0
            } else {
                (u64::from(value.min(max)) * INPUT_ABS_MAX) / u64::from(max)
            }
        };
        let events = vec![
            json!({"type": "abs", "data": {"axis": "x", "value": scale(x, max_x)}}),
            json!({"type": "abs", "data": {"axis": "y", "value": scale(y, max_y)}}),
        ];
        self.input_send_event(events).await
    }

    /// Press or release a mouse button (`"left"`, `"right"`, `"middle"`,
    /// `"wheel-up"`, ...).
    pub async fn mouse_button(&self, button: &str, down: bool) -> Result<(), QmpError> {
        let events = vec![json!({"type": "btn", "data": {"button": button, "down": down}})];
        self.input_send_event(events).await
    }

    // --- snapshots ----------------------------------------------------------

    /// Take an online snapshot (`snapshot-save`): disk + RAM + device
    /// state into the qcow2-internal snapshot `tag`. Blocks until the
    /// background job concludes.
    pub async fn snapshot_save(
        &self,
        tag: &str,
        vmstate: &str,
        devices: &[&str],
    ) -> Result<(), QmpError> {
        let job_id = self.new_job_id("snapshot-save");
        self.execute(
            "snapshot-save",
            Some(json!({
                "job-id": job_id,
                "tag": tag,
                "vmstate": vmstate,
                "devices": devices,
            })),
        )
        .await?;
        self.wait_for_job(&job_id).await
    }

    /// Restore an online snapshot (`snapshot-load`). The VM must be
    /// stopped (paused) first. Blocks until the background job concludes.
    pub async fn snapshot_load(
        &self,
        tag: &str,
        vmstate: &str,
        devices: &[&str],
    ) -> Result<(), QmpError> {
        let job_id = self.new_job_id("snapshot-load");
        self.execute(
            "snapshot-load",
            Some(json!({
                "job-id": job_id,
                "tag": tag,
                "vmstate": vmstate,
                "devices": devices,
            })),
        )
        .await?;
        self.wait_for_job(&job_id).await
    }

    /// Delete a snapshot (`snapshot-delete`). Blocks until the background
    /// job concludes.
    pub async fn snapshot_delete(&self, tag: &str, devices: &[&str]) -> Result<(), QmpError> {
        let job_id = self.new_job_id("snapshot-delete");
        self.execute(
            "snapshot-delete",
            Some(json!({
                "job-id": job_id,
                "tag": tag,
                "devices": devices,
            })),
        )
        .await?;
        self.wait_for_job(&job_id).await
    }

    /// Poll `query-jobs` until the job with `job_id` concludes, then
    /// dismiss it. Returns [`QmpError::JobFailed`] if the job carried an
    /// error; a job that has vanished from `query-jobs` is treated as
    /// already concluded and dismissed.
    pub async fn wait_for_job(&self, job_id: &str) -> Result<(), QmpError> {
        loop {
            let jobs = self.execute("query-jobs", None).await?;
            let job = jobs.as_array().and_then(|list| {
                list.iter()
                    .find(|j| j.get("id").and_then(|i| i.as_str()) == Some(job_id))
            });
            let Some(job) = job else {
                return Ok(());
            };
            let status = job.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status == "concluded" {
                let error = job
                    .get("error")
                    .and_then(|e| e.as_str())
                    .map(str::to_string);
                // Dismiss regardless of outcome so the job list stays clean.
                let _ = self
                    .execute("job-dismiss", Some(json!({"id": job_id})))
                    .await;
                return match error {
                    Some(error) => Err(QmpError::JobFailed {
                        job_id: job_id.to_string(),
                        error,
                    }),
                    None => Ok(()),
                };
            }
            tokio::time::sleep(JOB_POLL_INTERVAL).await;
        }
    }

    /// Generate a unique job id for a job-based command.
    fn new_job_id(&self, kind: &str) -> String {
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        format!("vmlab-{kind}-{n}")
    }
}

/// Read one newline-delimited JSON value (used only during the handshake;
/// afterwards the reader task owns the read half).
async fn read_json_line(reader: &mut BufReader<OwnedReadHalf>) -> Result<Value, QmpError> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(QmpError::Closed);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Ok(serde_json::from_str(trimmed)?);
    }
}

/// Serialise `msg` and write it as one line.
async fn write_json_line(
    writer: &mut (impl AsyncWriteExt + Unpin),
    msg: &Value,
) -> Result<(), QmpError> {
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Background task: routes responses to pending requests by id and events
/// to the broadcast channel. Exits on EOF or read error, failing all
/// in-flight requests.
async fn reader_loop(
    mut reader: BufReader<OwnedReadHalf>,
    shared: Arc<Shared>,
    event_tx: broadcast::Sender<QmpEvent>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!(error = %e, "qmp: discarding unparseable line");
                continue;
            }
        };

        // Asynchronous event.
        if let Some(event) = msg.get("event").and_then(|e| e.as_str()) {
            let timestamp = msg
                .get("timestamp")
                .and_then(|t| serde_json::from_value(t.clone()).ok())
                .unwrap_or_default();
            let data = msg.get("data").cloned().unwrap_or(Value::Null);
            let _ = event_tx.send(QmpEvent {
                event: event.to_string(),
                data,
                timestamp,
            });
            continue;
        }

        // Response: match by id.
        let Some(id) = msg.get("id").and_then(|v| v.as_u64()) else {
            tracing::warn!("qmp: discarding response without numeric id: {msg}");
            continue;
        };
        let tx = shared
            .pending
            .lock()
            .expect("qmp pending lock")
            .as_mut()
            .and_then(|map| map.remove(&id));
        if let Some(tx) = tx {
            let result = match msg.get("error") {
                Some(error) => Err(QmpError::from_error_object(error)),
                None => Ok(msg.get("return").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
        }
    }
    shared.close();
}
