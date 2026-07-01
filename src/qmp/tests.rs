//! QMP client tests against a mock QMP server on a unix socket.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use super::QmpClient;
use super::error::QmpError;
use super::types::RunState;

/// A mock QMP server: sends the greeting, then answers each incoming
/// command with whatever lines `responder` produces. Every received
/// message is recorded for post-hoc assertions.
struct MockQmp {
    _dir: tempfile::TempDir,
    path: PathBuf,
    received: Arc<Mutex<Vec<Value>>>,
}

fn spawn_mock<F>(mut responder: F) -> MockQmp
where
    F: FnMut(&Value) -> Vec<String> + Send + 'static,
{
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("qmp.sock");
    let listener = UnixListener::bind(&path).expect("bind mock qmp socket");
    let received: Arc<Mutex<Vec<Value>>> = Arc::default();
    let received_in_task = Arc::clone(&received);

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let greeting = json!({
            "QMP": {
                "version": {"qemu": {"major": 9, "minor": 2, "micro": 0}},
                "capabilities": [],
            }
        });
        write_half
            .write_all(format!("{greeting}\n").as_bytes())
            .await
            .expect("write greeting");

        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let msg: Value = serde_json::from_str(line.trim()).expect("client sent valid json");
            received_in_task.lock().unwrap().push(msg.clone());
            for out in responder(&msg) {
                write_half
                    .write_all(format!("{out}\n").as_bytes())
                    .await
                    .expect("write response");
            }
        }
    });

    MockQmp {
        _dir: dir,
        path,
        received,
    }
}

/// Response `{"return": ret}` echoing the command's id.
fn ok(msg: &Value, ret: Value) -> String {
    json!({"return": ret, "id": msg["id"]}).to_string()
}

fn execute_name(msg: &Value) -> &str {
    msg.get("execute").and_then(|e| e.as_str()).unwrap_or("")
}

#[tokio::test]
async fn connect_negotiates_and_executes() {
    let mock = spawn_mock(|msg| match execute_name(msg) {
        "qmp_capabilities" => vec![ok(msg, json!({}))],
        "query-name" => vec![ok(msg, json!({"name": "vm0"}))],
        other => panic!("unexpected command: {other}"),
    });

    let client = QmpClient::connect(&mock.path).await.expect("connect");
    let result = client.execute("query-name", None).await.expect("execute");
    assert_eq!(result, json!({"name": "vm0"}));

    // Capability negotiation happened before the first command.
    let received = mock.received.lock().unwrap();
    assert_eq!(execute_name(&received[0]), "qmp_capabilities");
    assert_eq!(execute_name(&received[1]), "query-name");
}

#[tokio::test]
async fn qmp_error_becomes_command_error() {
    let mock = spawn_mock(|msg| match execute_name(msg) {
        "qmp_capabilities" => vec![ok(msg, json!({}))],
        _ => vec![
            json!({
                "error": {
                    "class": "CommandNotFound",
                    "desc": "The command bogus has not been found",
                },
                "id": msg["id"],
            })
            .to_string(),
        ],
    });

    let client = QmpClient::connect(&mock.path).await.expect("connect");
    let err = client.execute("bogus", None).await.expect_err("must fail");
    match err {
        QmpError::Command { class, desc } => {
            assert_eq!(class, "CommandNotFound");
            assert!(desc.contains("bogus"));
        }
        other => panic!("expected Command error, got: {other:?}"),
    }
}

#[tokio::test]
async fn events_interleave_with_responses() {
    let mock = spawn_mock(|msg| match execute_name(msg) {
        "qmp_capabilities" => vec![ok(msg, json!({}))],
        "query-status" => vec![
            // Event arrives *before* the response on the same stream.
            json!({
                "event": "STOP",
                "data": {},
                "timestamp": {"seconds": 1718000000_i64, "microseconds": 42},
            })
            .to_string(),
            ok(
                msg,
                json!({"status": "paused", "running": false, "singlestep": false}),
            ),
        ],
        other => panic!("unexpected command: {other}"),
    });

    let client = QmpClient::connect(&mock.path).await.expect("connect");
    let mut events = client.subscribe_events();

    let state = client.query_status().await.expect("query-status");
    assert_eq!(state, RunState::Paused);

    let event = events.recv().await.expect("event");
    assert_eq!(event.event, "STOP");
    assert_eq!(event.timestamp.seconds, 1718000000);
    assert_eq!(event.timestamp.microseconds, 42);
}

#[tokio::test]
async fn screendump_arg_shape() {
    let mock = spawn_mock(|msg| vec![ok(msg, json!({}))]);

    let client = QmpClient::connect(&mock.path).await.expect("connect");
    client
        .screendump(std::path::Path::new("/tmp/shot.ppm"))
        .await
        .expect("screendump");

    let received = mock.received.lock().unwrap();
    let msg = received
        .iter()
        .find(|m| execute_name(m) == "screendump")
        .expect("screendump command sent");
    assert_eq!(msg["arguments"], json!({"filename": "/tmp/shot.ppm"}));
}

#[tokio::test]
async fn send_key_arg_shape() {
    let mock = spawn_mock(|msg| vec![ok(msg, json!({}))]);

    let client = QmpClient::connect(&mock.path).await.expect("connect");
    client
        .send_key(&["ctrl", "alt", "delete"], Some(50))
        .await
        .expect("send-key");

    let received = mock.received.lock().unwrap();
    let msg = received
        .iter()
        .find(|m| execute_name(m) == "send-key")
        .expect("send-key command sent");
    assert_eq!(
        msg["arguments"],
        json!({
            "keys": [
                {"type": "qcode", "data": "ctrl"},
                {"type": "qcode", "data": "alt"},
                {"type": "qcode", "data": "delete"},
            ],
            "hold-time": 50,
        })
    );
}

#[tokio::test]
async fn mouse_events_arg_shape() {
    let mock = spawn_mock(|msg| vec![ok(msg, json!({}))]);

    let client = QmpClient::connect(&mock.path).await.expect("connect");
    client
        .mouse_move_abs(640, 240, 1280, 480)
        .await
        .expect("mouse move");
    client
        .mouse_button("left", true)
        .await
        .expect("mouse button");

    let received = mock.received.lock().unwrap();
    let mut sends = received
        .iter()
        .filter(|m| execute_name(m) == "input-send-event");
    let move_msg = sends.next().expect("move event sent");
    // 640/1280 and 240/480 both scale to half of 32767 = 16383.
    assert_eq!(
        move_msg["arguments"]["events"],
        json!([
            {"type": "abs", "data": {"axis": "x", "value": 16383}},
            {"type": "abs", "data": {"axis": "y", "value": 16383}},
        ])
    );
    let btn_msg = sends.next().expect("button event sent");
    assert_eq!(
        btn_msg["arguments"]["events"],
        json!([{"type": "btn", "data": {"button": "left", "down": true}}])
    );
}

#[tokio::test]
async fn snapshot_save_waits_for_job() {
    // Stateful responder: snapshot-save starts a job that is "running" on
    // the first query-jobs poll and "concluded" afterwards.
    let job_id: Arc<Mutex<Option<String>>> = Arc::default();
    let job_id_in_responder = Arc::clone(&job_id);
    let mut polls = 0u32;

    let mock = spawn_mock(move |msg| match execute_name(msg) {
        "qmp_capabilities" => vec![ok(msg, json!({}))],
        "snapshot-save" => {
            let id = msg["arguments"]["job-id"]
                .as_str()
                .expect("job-id present")
                .to_string();
            *job_id_in_responder.lock().unwrap() = Some(id);
            vec![ok(msg, json!({}))]
        }
        "query-jobs" => {
            polls += 1;
            let id = job_id_in_responder
                .lock()
                .unwrap()
                .clone()
                .expect("job started");
            let status = if polls == 1 { "running" } else { "concluded" };
            vec![ok(
                msg,
                json!([{"id": id, "type": "snapshot-save", "status": status}]),
            )]
        }
        "job-dismiss" => vec![ok(msg, json!({}))],
        other => panic!("unexpected command: {other}"),
    });

    let client = QmpClient::connect(&mock.path).await.expect("connect");
    client
        .snapshot_save("checkpoint", "disk0", &["disk0"])
        .await
        .expect("snapshot save");

    let received = mock.received.lock().unwrap();
    let save = received
        .iter()
        .find(|m| execute_name(m) == "snapshot-save")
        .expect("snapshot-save sent");
    assert_eq!(save["arguments"]["tag"], "checkpoint");
    assert_eq!(save["arguments"]["vmstate"], "disk0");
    assert_eq!(save["arguments"]["devices"], json!(["disk0"]));
    assert!(save["arguments"]["job-id"].is_string());
    // The concluded job was dismissed.
    let dismiss = received
        .iter()
        .find(|m| execute_name(m) == "job-dismiss")
        .expect("job-dismiss sent");
    assert_eq!(
        dismiss["arguments"]["id"].as_str(),
        job_id.lock().unwrap().as_deref()
    );
}

#[tokio::test]
async fn failed_job_reports_job_error() {
    let job_id: Arc<Mutex<Option<String>>> = Arc::default();
    let job_id_in_responder = Arc::clone(&job_id);

    let mock = spawn_mock(move |msg| match execute_name(msg) {
        "qmp_capabilities" | "job-dismiss" => vec![ok(msg, json!({}))],
        "snapshot-delete" => {
            let id = msg["arguments"]["job-id"]
                .as_str()
                .expect("job-id present")
                .to_string();
            *job_id_in_responder.lock().unwrap() = Some(id);
            vec![ok(msg, json!({}))]
        }
        "query-jobs" => {
            let id = job_id_in_responder
                .lock()
                .unwrap()
                .clone()
                .expect("job started");
            vec![ok(
                msg,
                json!([{
                    "id": id,
                    "type": "snapshot-delete",
                    "status": "concluded",
                    "error": "Snapshot 'gone' not found",
                }]),
            )]
        }
        other => panic!("unexpected command: {other}"),
    });

    let client = QmpClient::connect(&mock.path).await.expect("connect");
    let err = client
        .snapshot_delete("gone", &["disk0"])
        .await
        .expect_err("job must fail");
    match err {
        QmpError::JobFailed { error, .. } => assert!(error.contains("not found")),
        other => panic!("expected JobFailed, got: {other:?}"),
    }
}

#[tokio::test]
async fn close_fails_pending_and_future_calls() {
    // Responder that answers negotiation but never answers query-name.
    let mock = spawn_mock(|msg| match execute_name(msg) {
        "qmp_capabilities" => vec![ok(msg, json!({}))],
        _ => vec![],
    });

    let client = QmpClient::connect(&mock.path).await.expect("connect");
    let pending = {
        let client = client.clone();
        tokio::spawn(async move { client.execute("query-name", None).await })
    };
    // Let the pending request hit the wire before closing.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    client.close().await;

    let result = pending.await.expect("task");
    assert!(matches!(result, Err(QmpError::Closed)));
    assert!(matches!(
        client.execute("query-name", None).await,
        Err(QmpError::Closed)
    ));
}
