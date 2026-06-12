//! Guest agent client tests against a mock qemu-ga on a unix socket.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use super::{GaClient, GaError};

const SHORT: Duration = Duration::from_millis(200);
const LONG: Duration = Duration::from_secs(5);

/// Bind a socket in a tempdir and hand the accepted connection to `serve`.
async fn spawn_mock<F, Fut>(serve: F) -> (tempfile::TempDir, PathBuf)
where
    F: FnOnce(BufReader<OwnedReadHalf>, OwnedWriteHalf) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("qga.sock");
    let listener = UnixListener::bind(&path).expect("bind mock qga socket");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (read_half, write_half) = stream.into_split();
        serve(BufReader::new(read_half), write_half).await;
    });
    (dir, path)
}

/// Read the next JSON message from the client, skipping blank lines and
/// stripping 0xFF sync prefixes. `None` on EOF.
async fn read_msg(reader: &mut BufReader<OwnedReadHalf>) -> Option<Value> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        let cleaned: Vec<u8> = buf.iter().copied().filter(|&b| b != 0xFF).collect();
        let text = String::from_utf8(cleaned).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed).ok();
    }
}

fn execute_name(msg: &Value) -> &str {
    msg.get("execute").and_then(|e| e.as_str()).unwrap_or("")
}

/// Reply to `guest-sync-delimited` per protocol: 0xFF sentinel, then the
/// echoed id. Returns `true` if the message was a sync request.
async fn answer_if_sync(msg: &Value, writer: &mut OwnedWriteHalf) -> bool {
    if execute_name(msg) != "guest-sync-delimited" {
        return false;
    }
    let id = msg["arguments"]["id"].clone();
    let mut out = vec![0xFF];
    out.extend_from_slice(json!({"return": id}).to_string().as_bytes());
    out.push(b'\n');
    writer.write_all(&out).await.expect("write sync response");
    true
}

async fn send_return(writer: &mut OwnedWriteHalf, ret: Value) {
    let line = format!("{}\n", json!({"return": ret}));
    writer
        .write_all(line.as_bytes())
        .await
        .expect("write response");
}

#[tokio::test]
async fn sync_delimited_recovers_from_desynced_channel() {
    let (_dir, path) = spawn_mock(|mut reader, mut writer| async move {
        // Garbage already sitting in the channel: a truncated JSON
        // fragment without newline, then a complete junk line.
        writer
            .write_all(b"\x01\x02{\"trunc")
            .await
            .expect("garbage");
        writer
            .write_all(b"ated\": \n{\"stale\": \"line\"}\n")
            .await
            .expect("garbage");
        while let Some(msg) = read_msg(&mut reader).await {
            if answer_if_sync(&msg, &mut writer).await {
                continue;
            }
            match execute_name(&msg) {
                "guest-ping" => send_return(&mut writer, json!({})).await,
                other => panic!("unexpected command: {other}"),
            }
        }
    })
    .await;

    let client = GaClient::connect(&path).await.expect("connect");
    assert!(client.ping(LONG).await, "ping must succeed after resync");
}

#[tokio::test]
async fn exec_polls_status_and_decodes_output() {
    let (_dir, path) = spawn_mock(|mut reader, mut writer| async move {
        let mut status_polls = 0;
        while let Some(msg) = read_msg(&mut reader).await {
            if answer_if_sync(&msg, &mut writer).await {
                continue;
            }
            match execute_name(&msg) {
                "guest-exec" => {
                    assert_eq!(msg["arguments"]["path"], "/bin/sh");
                    assert_eq!(msg["arguments"]["arg"], json!(["-c", "echo hi"]));
                    assert_eq!(msg["arguments"]["capture-output"], true);
                    send_return(&mut writer, json!({"pid": 4242})).await;
                }
                "guest-exec-status" => {
                    assert_eq!(msg["arguments"]["pid"], 4242);
                    status_polls += 1;
                    if status_polls == 1 {
                        send_return(&mut writer, json!({"exited": false})).await;
                    } else {
                        send_return(
                            &mut writer,
                            json!({
                                "exited": true,
                                "exitcode": 3,
                                "out-data": BASE64.encode(b"hi\n"),
                                "err-data": BASE64.encode(b"warning: x"),
                            }),
                        )
                        .await;
                    }
                }
                other => panic!("unexpected command: {other}"),
            }
        }
    })
    .await;

    let client = GaClient::connect(&path).await.expect("connect");
    let result = client
        .exec("/bin/sh", &["-c", "echo hi"], true, LONG)
        .await
        .expect("exec");
    assert_eq!(result.exit_code, 3);
    assert_eq!(result.stdout, b"hi\n");
    assert_eq!(result.stderr, b"warning: x");
}

#[tokio::test]
async fn file_write_then_read_round_trips() {
    let (_dir, path) = spawn_mock(|mut reader, mut writer| async move {
        // In-memory guest filesystem.
        let mut files: HashMap<String, Vec<u8>> = HashMap::new();
        // handle -> (path, read position)
        let mut handles: HashMap<i64, (String, usize)> = HashMap::new();
        let mut next_handle = 1000_i64;

        while let Some(msg) = read_msg(&mut reader).await {
            if answer_if_sync(&msg, &mut writer).await {
                continue;
            }
            match execute_name(&msg) {
                "guest-file-open" => {
                    let file_path = msg["arguments"]["path"].as_str().unwrap().to_string();
                    if msg["arguments"]["mode"] == "w" {
                        files.insert(file_path.clone(), Vec::new());
                    }
                    next_handle += 1;
                    handles.insert(next_handle, (file_path, 0));
                    send_return(&mut writer, json!(next_handle)).await;
                }
                "guest-file-write" => {
                    let handle = msg["arguments"]["handle"].as_i64().unwrap();
                    let data = BASE64
                        .decode(msg["arguments"]["buf-b64"].as_str().unwrap())
                        .expect("valid base64 from client");
                    let (file_path, _) = &handles[&handle];
                    let count = data.len();
                    files.get_mut(file_path).unwrap().extend_from_slice(&data);
                    send_return(&mut writer, json!({"count": count, "eof": false})).await;
                }
                "guest-file-read" => {
                    let handle = msg["arguments"]["handle"].as_i64().unwrap();
                    let (file_path, pos) = handles.get_mut(&handle).unwrap();
                    let content = &files[file_path.as_str()];
                    // Return at most 4 KiB per read so the client must loop.
                    let n = 4096.min(content.len() - *pos);
                    let chunk = &content[*pos..*pos + n];
                    *pos += n;
                    let eof = *pos >= content.len();
                    send_return(
                        &mut writer,
                        json!({"count": n, "buf-b64": BASE64.encode(chunk), "eof": eof}),
                    )
                    .await;
                }
                "guest-file-close" => {
                    handles.remove(&msg["arguments"]["handle"].as_i64().unwrap());
                    send_return(&mut writer, json!({})).await;
                }
                other => panic!("unexpected command: {other}"),
            }
        }
    })
    .await;

    // Large enough to exercise both the 48 KiB write chunking and the
    // server-side 4 KiB read chunking.
    let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();

    let client = GaClient::connect(&path).await.expect("connect");
    client
        .file_write("/etc/blob.bin", &data, LONG)
        .await
        .expect("file_write");
    let read_back = client
        .file_read("/etc/blob.bin", LONG)
        .await
        .expect("file_read");
    assert_eq!(read_back, data);
}

#[tokio::test]
async fn file_append_extends_existing_content() {
    let (_dir, path) = spawn_mock(|mut reader, mut writer| async move {
        let mut files: HashMap<String, Vec<u8>> = HashMap::new();
        let mut handles: HashMap<i64, (String, usize)> = HashMap::new();
        let mut next_handle = 1000_i64;

        while let Some(msg) = read_msg(&mut reader).await {
            if answer_if_sync(&msg, &mut writer).await {
                continue;
            }
            match execute_name(&msg) {
                "guest-file-open" => {
                    let file_path = msg["arguments"]["path"].as_str().unwrap().to_string();
                    match msg["arguments"]["mode"].as_str().unwrap() {
                        "w" => {
                            files.insert(file_path.clone(), Vec::new());
                        }
                        "a" | "r" => {
                            files.entry(file_path.clone()).or_default();
                        }
                        other => panic!("unexpected open mode: {other}"),
                    }
                    next_handle += 1;
                    handles.insert(next_handle, (file_path, 0));
                    send_return(&mut writer, json!(next_handle)).await;
                }
                "guest-file-write" => {
                    let handle = msg["arguments"]["handle"].as_i64().unwrap();
                    let data = BASE64
                        .decode(msg["arguments"]["buf-b64"].as_str().unwrap())
                        .expect("valid base64 from client");
                    let (file_path, _) = &handles[&handle];
                    let count = data.len();
                    files.get_mut(file_path).unwrap().extend_from_slice(&data);
                    send_return(&mut writer, json!({"count": count, "eof": false})).await;
                }
                "guest-file-read" => {
                    let handle = msg["arguments"]["handle"].as_i64().unwrap();
                    let (file_path, pos) = handles.get_mut(&handle).unwrap();
                    let content = &files[file_path.as_str()];
                    let n = content.len() - *pos;
                    let chunk = &content[*pos..];
                    *pos += n;
                    send_return(
                        &mut writer,
                        json!({"count": n, "buf-b64": BASE64.encode(chunk), "eof": true}),
                    )
                    .await;
                }
                "guest-file-close" => {
                    handles.remove(&msg["arguments"]["handle"].as_i64().unwrap());
                    send_return(&mut writer, json!({})).await;
                }
                other => panic!("unexpected command: {other}"),
            }
        }
    })
    .await;

    let client = GaClient::connect(&path).await.expect("connect");
    client
        .file_write("/tmp/chunked", b"first ", LONG)
        .await
        .expect("file_write");
    client
        .file_append("/tmp/chunked", b"second", LONG)
        .await
        .expect("file_append");
    let read_back = client
        .file_read("/tmp/chunked", LONG)
        .await
        .expect("file_read");
    assert_eq!(read_back, b"first second");
}

#[tokio::test]
async fn osinfo_returns_raw_object() {
    let (_dir, path) = spawn_mock(|mut reader, mut writer| async move {
        while let Some(msg) = read_msg(&mut reader).await {
            if answer_if_sync(&msg, &mut writer).await {
                continue;
            }
            assert_eq!(execute_name(&msg), "guest-get-osinfo");
            send_return(
                &mut writer,
                json!({"id": "mswindows", "name": "Microsoft Windows", "version": "11"}),
            )
            .await;
        }
    })
    .await;

    let client = GaClient::connect(&path).await.expect("connect");
    let info = client.get_osinfo(LONG).await.expect("osinfo");
    assert_eq!(info["id"], "mswindows");
    assert_eq!(info["name"], "Microsoft Windows");
}

#[tokio::test]
async fn ping_timeout_leaves_client_usable() {
    let (_dir, path) = spawn_mock(|mut reader, mut writer| async move {
        let mut pings = 0;
        while let Some(msg) = read_msg(&mut reader).await {
            if answer_if_sync(&msg, &mut writer).await {
                continue;
            }
            match execute_name(&msg) {
                "guest-ping" => {
                    pings += 1;
                    if pings == 1 {
                        // Agent goes silent: read the command, never answer.
                        continue;
                    }
                    send_return(&mut writer, json!({})).await;
                }
                other => panic!("unexpected command: {other}"),
            }
        }
    })
    .await;

    let client = GaClient::connect(&path).await.expect("connect");
    assert!(!client.ping(SHORT).await, "first ping must time out");
    // The client resyncs and works again.
    assert!(client.ping(LONG).await, "second ping must succeed");
}

#[tokio::test]
async fn never_answering_agent_times_out_cleanly() {
    let (_dir, path) = spawn_mock(|mut reader, writer| async move {
        // Keep the write half alive (dropping it would EOF the client),
        // but read everything and answer nothing (agent not installed).
        let _writer = writer;
        while read_msg(&mut reader).await.is_some() {}
    })
    .await;

    let client = GaClient::connect(&path).await.expect("connect");
    let err = client
        .execute("guest-ping", None, SHORT)
        .await
        .expect_err("must time out");
    assert!(matches!(err, GaError::Timeout), "got: {err:?}");
    // Still usable: times out again rather than hanging or panicking.
    assert!(!client.ping(SHORT).await);
}

#[tokio::test]
async fn shutdown_treats_eof_as_success() {
    let (_dir, path) = spawn_mock(|mut reader, mut writer| async move {
        while let Some(msg) = read_msg(&mut reader).await {
            if answer_if_sync(&msg, &mut writer).await {
                continue;
            }
            match execute_name(&msg) {
                "guest-shutdown" => {
                    assert_eq!(msg["arguments"]["mode"], "powerdown");
                    return; // drop the connection without replying
                }
                other => panic!("unexpected command: {other}"),
            }
        }
    })
    .await;

    let client = GaClient::connect(&path).await.expect("connect");
    client
        .shutdown("powerdown", LONG)
        .await
        .expect("shutdown must treat EOF as success");
}

#[tokio::test]
async fn agent_error_becomes_command_error() {
    let (_dir, path) = spawn_mock(|mut reader, mut writer| async move {
        while let Some(msg) = read_msg(&mut reader).await {
            if answer_if_sync(&msg, &mut writer).await {
                continue;
            }
            let line = format!(
                "{}\n",
                json!({"error": {"class": "CommandNotFound", "desc": "unsupported"}})
            );
            writer.write_all(line.as_bytes()).await.expect("write");
        }
    })
    .await;

    let client = GaClient::connect(&path).await.expect("connect");
    let err = client
        .execute("guest-frobnicate", None, LONG)
        .await
        .expect_err("must fail");
    match err {
        GaError::Command { class, desc } => {
            assert_eq!(class, "CommandNotFound");
            assert_eq!(desc, "unsupported");
        }
        other => panic!("expected Command error, got: {other:?}"),
    }
}

#[tokio::test]
async fn network_interfaces_parses_fixture() {
    let (_dir, path) = spawn_mock(|mut reader, mut writer| async move {
        while let Some(msg) = read_msg(&mut reader).await {
            if answer_if_sync(&msg, &mut writer).await {
                continue;
            }
            assert_eq!(execute_name(&msg), "guest-network-get-interfaces");
            send_return(
                &mut writer,
                json!([
                    {
                        "name": "lo",
                        "ip-addresses": [
                            {"ip-address": "127.0.0.1", "ip-address-type": "ipv4", "prefix": 8},
                        ],
                    },
                    {
                        "name": "eth0",
                        "hardware-address": "52:54:00:12:34:56",
                        "ip-addresses": [
                            {"ip-address": "10.0.0.7", "ip-address-type": "ipv4", "prefix": 24},
                            {"ip-address": "fe80::1", "ip-address-type": "ipv6", "prefix": 64},
                        ],
                        "statistics": {"rx-bytes": 1, "tx-bytes": 2},
                    },
                ]),
            )
            .await;
        }
    })
    .await;

    let client = GaClient::connect(&path).await.expect("connect");
    let interfaces = client.network_interfaces(LONG).await.expect("interfaces");
    assert_eq!(interfaces.len(), 2);
    assert_eq!(interfaces[0].name, "lo");
    assert_eq!(interfaces[0].hardware_address, None);
    assert_eq!(
        interfaces[0].ips,
        vec![("127.0.0.1".to_string(), "ipv4".to_string())]
    );
    assert_eq!(interfaces[1].name, "eth0");
    assert_eq!(
        interfaces[1].hardware_address.as_deref(),
        Some("52:54:00:12:34:56")
    );
    assert_eq!(
        interfaces[1].ips,
        vec![
            ("10.0.0.7".to_string(), "ipv4".to_string()),
            ("fe80::1".to_string(), "ipv6".to_string()),
        ]
    );
}
