//! Protocol client side, used by the CLI against both daemon tiers and by
//! the supervisor against lab daemons.
//!
//! A client is typed by the vocabulary its socket speaks ([`LabClient`],
//! [`SupClient`]): the request it sends is a variant of that vocabulary, so a
//! command the daemon does not serve, or an argument of the wrong shape, does
//! not compile.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::{
    CommandError, ErrorCode, Event, LabRequest, Message, ProtoError, SupRequest, WireRequest,
};

/// A client for a lab daemon's control socket.
pub type LabClient = Client<LabRequest>;
/// A client for the supervisor's control socket.
pub type SupClient = Client<SupRequest>;

struct Pending {
    resp: oneshot::Sender<Result<Value, CommandError>>,
    chunks: Option<mpsc::Sender<String>>,
}

/// Where the reader task routes what the daemon sends: the answer each
/// request waits for, and the event subscription. This is all the reader
/// holds — no write half and no [`Inner`] — so the connection lives exactly
/// as long as the [`Client`] clones do.
#[derive(Default)]
struct Routes {
    pending: std::sync::Mutex<HashMap<u64, Pending>>,
    events: std::sync::Mutex<Option<mpsc::Sender<Event>>>,
}

impl Routes {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Pending>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn events(&self) -> std::sync::MutexGuard<'_, Option<mpsc::Sender<Event>>> {
        self.events.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Answer every request still waiting with `why`.
    fn fail_pending(&self, why: &str) {
        for (_, p) in self.pending().drain() {
            let _ = p.resp.send(Err(CommandError::failed(why)));
        }
    }
}

/// The connection the [`Client`] clones share. Dropped with the last clone,
/// and that closes it: the write half shuts down, which the daemon reads as
/// EOF, and the reader task, which owns the read half, is aborted.
struct Inner {
    write: Mutex<tokio::net::unix::OwnedWriteHalf>,
    routes: Arc<Routes>,
    next_id: AtomicU64,
    reader: tokio::task::AbortHandle,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.reader.abort();
        // A request is only still pending here if the future awaiting it was
        // dropped; its receiver, if anything kept it, gets an answer. The
        // event subscription ends with the client.
        self.routes.fail_pending("client dropped");
        self.routes.events().take();
    }
}

/// Cloneable async client for one daemon socket. The connection closes
/// when the last clone drops.
pub struct Client<R: WireRequest> {
    inner: Arc<Inner>,
    _vocabulary: PhantomData<fn(R)>,
}

// Derived `Clone` would demand `R: Clone`, which a vocabulary has no reason to
// be: the client holds no request, only the type that names one.
impl<R: WireRequest> Clone for Client<R> {
    fn clone(&self) -> Self {
        Client {
            inner: self.inner.clone(),
            _vocabulary: PhantomData,
        }
    }
}

impl<R: WireRequest> Client<R> {
    pub async fn connect(path: &Path) -> Result<Client<R>, ProtoError> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, write_half) = stream.into_split();
        let routes = Arc::new(Routes::default());
        let reader = tokio::spawn(read_loop(read_half, Arc::clone(&routes)));
        Ok(Client {
            inner: Arc::new(Inner {
                write: Mutex::new(write_half),
                routes,
                next_id: AtomicU64::new(1),
                reader: reader.abort_handle(),
            }),
            _vocabulary: PhantomData,
        })
    }

    async fn send_req(
        &self,
        cmd: &str,
        args: Value,
        chunks: Option<mpsc::Sender<String>>,
    ) -> Result<oneshot::Receiver<Result<Value, CommandError>>, ProtoError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner
            .routes
            .pending()
            .insert(id, Pending { resp: tx, chunks });
        let msg = Message::Req {
            id,
            cmd: cmd.to_string(),
            args,
        };
        let mut line =
            serde_json::to_string(&msg).map_err(|e| ProtoError::Protocol(e.to_string()))?;
        line.push('\n');
        let mut w = self.inner.write.lock().await;
        w.write_all(line.as_bytes()).await?;
        Ok(rx)
    }

    /// Send one request and wait for its answer.
    pub async fn send(&self, req: R) -> Result<Value, ProtoError> {
        let (cmd, args) = req.to_wire();
        let rx = self.send_req(cmd, args, None).await?;
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(ProtoError::Remote(e)),
            Err(_) => Err(ProtoError::Closed),
        }
    }

    /// Send one request whose output streams: `on_chunk` receives incremental
    /// text (build logs, provision output) until the final answer arrives.
    pub async fn send_streaming(
        &self,
        req: R,
        mut on_chunk: impl FnMut(String) + Send,
    ) -> Result<Value, ProtoError> {
        let (cmd, args) = req.to_wire();
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let resp_rx = self.send_req(cmd, args, Some(tx)).await?;
        tokio::pin!(resp_rx);
        loop {
            tokio::select! {
                chunk = rx.recv() => {
                    if let Some(c) = chunk {
                        on_chunk(c);
                    }
                }
                resp = &mut resp_rx => {
                    // Drain any chunks that raced the response.
                    while let Ok(c) = rx.try_recv() {
                        on_chunk(c);
                    }
                    return match resp {
                        Ok(Ok(v)) => Ok(v),
                        Ok(Err(e)) => Err(ProtoError::Remote(e)),
                        Err(_) => Err(ProtoError::Closed),
                    };
                }
            }
        }
    }

    /// Subscribe to the daemon's event stream.
    ///
    /// `subscribe` belongs to the framing rather than to either vocabulary:
    /// the server intercepts it to flip this connection into event mode, and
    /// it never reaches a command handler.
    pub async fn subscribe(&self) -> Result<mpsc::Receiver<Event>, ProtoError> {
        let (tx, rx) = mpsc::channel(256);
        *self.inner.routes.events() = Some(tx);
        let resp = self.send_req("subscribe", Value::Null, None).await?;
        match resp.await {
            Ok(Ok(_)) => Ok(rx),
            Ok(Err(e)) => Err(ProtoError::Remote(e)),
            Err(_) => Err(ProtoError::Closed),
        }
    }
}

/// Route every message the daemon sends until the connection ends, then
/// fail whatever still waits. Holds only `routes`, never the client.
async fn read_loop(read_half: tokio::net::unix::OwnedReadHalf, routes: Arc<Routes>) {
    let mut lines = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Message>(&line) else {
            continue;
        };
        match msg {
            Message::Resp { id, ok, err, code } => {
                let waiting = routes.pending().remove(&id);
                if let Some(p) = waiting {
                    let result = match (ok, err) {
                        // A daemon older than ADR-0007 sends prose and
                        // no code; `Failed` is what its errors always
                        // meant to the surfaces above.
                        (_, Some(e)) => {
                            Err(CommandError::new(code.unwrap_or(ErrorCode::Failed), e))
                        }
                        (Some(v), None) => Ok(v),
                        (None, None) => Ok(Value::Null),
                    };
                    let _ = p.resp.send(result);
                }
            }
            Message::Stream { id, chunk } => {
                if let Some(Pending {
                    chunks: Some(tx), ..
                }) = routes.pending().get(&id)
                {
                    let _ = tx.try_send(chunk);
                }
            }
            Message::Event { data, .. } => {
                if let Some(tx) = routes.events().as_ref()
                    && let Ok(ev) = serde_json::from_value::<Event>(data)
                {
                    let _ = tx.try_send(ev);
                }
            }
            Message::Req { .. } => {}
        }
    }
    // Connection died: fail everything pending.
    routes.fail_pending("connection closed");
}

#[cfg(test)]
mod tests {
    use super::super::Region;
    use super::super::server::{Handler, Server, Streamer};
    use super::*;

    /// A stand-in lab daemon: enough of the real vocabulary to exercise
    /// request/response, streaming, concurrency and each error code.
    struct FakeLab;

    #[async_trait::async_trait]
    impl Handler<LabRequest> for FakeLab {
        async fn handle(&self, req: LabRequest, stream: &Streamer) -> Result<Value, CommandError> {
            match req {
                LabRequest::Ping {} => Ok(Value::String("pong".into())),
                LabRequest::MachineStart { machine, .. } => Err(CommandError::conflict(format!(
                    "{machine} is already running"
                ))),
                LabRequest::MachineStop { machine, force } => {
                    Ok(serde_json::json!({"machine": machine, "force": force}))
                }
                LabRequest::MachineLogs { machine, .. } => Err(CommandError::unsupported(format!(
                    "{machine}: this machine keeps no console log"
                ))),
                LabRequest::SnapshotList { machine } => {
                    Err(CommandError::not_found(format!("no machine `{machine}`")))
                }
                LabRequest::Up { machines, .. } => {
                    for machine in &machines {
                        stream.chunk(format!("starting {machine}")).await;
                    }
                    Ok(serde_json::json!(true))
                }
                LabRequest::Status {} => {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    Ok(Value::String("slow-done".into()))
                }
                other => Err(CommandError::failed(format!("unhandled {}", other.cmd()))),
            }
        }
    }

    async fn start() -> (tempfile::TempDir, Server<LabRequest>, LabClient) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let server = Server::bind(&sock, Arc::new(FakeLab)).await.unwrap();
        let client = LabClient::connect(&sock).await.unwrap();
        (dir, server, client)
    }

    #[tokio::test]
    async fn request_response() {
        let (_dir, _server, client) = start().await;
        let v = client
            .send(LabRequest::MachineStop {
                machine: "dc01".into(),
                force: true,
            })
            .await
            .unwrap();
        assert_eq!(v["machine"], "dc01");
        assert_eq!(v["force"], true);
    }

    /// The point of the codes: a caller branches on why, not on wording.
    #[tokio::test]
    async fn remote_errors_carry_their_code() {
        let (_dir, _server, client) = start().await;
        let cases: Vec<(LabRequest, ErrorCode)> = vec![
            (
                LabRequest::MachineStart {
                    machine: "dc01".into(),
                    record: Vec::new(),
                },
                ErrorCode::Conflict,
            ),
            (
                LabRequest::SnapshotList {
                    machine: "ghost".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                LabRequest::MachineLogs {
                    machine: "web".into(),
                    lines: 10,
                    follow: false,
                },
                ErrorCode::Unsupported,
            ),
            (
                LabRequest::Destroy {},
                // Nothing more specific to say: the default.
                ErrorCode::Failed,
            ),
        ];
        for (req, want) in cases {
            let cmd = req.cmd();
            let err = client.send(req).await.unwrap_err();
            assert_eq!(err.code(), want, "{cmd}");
            assert!(!err.to_string().is_empty(), "{cmd}");
        }
    }

    /// A command the daemon does not serve is answered, not dropped — and it
    /// is a different code from a command whose arguments are wrong.
    #[tokio::test]
    async fn unknown_commands_and_bad_arguments_are_answered_by_code() {
        let (_dir, _server, client) = start().await;
        // Only a client bypassing the vocabulary can produce these, so the
        // test writes the raw frames a foreign client would.
        for (cmd, args, want) in [
            (
                "machine.teleport",
                serde_json::json!({}),
                ErrorCode::UnknownCommand,
            ),
            (
                "machine.stop",
                serde_json::json!({}),
                ErrorCode::InvalidArgument,
            ),
        ] {
            let rx = client.send_req(cmd, args, None).await.unwrap();
            let err = rx.await.unwrap().unwrap_err();
            assert_eq!(err.code, want, "{cmd}");
        }
    }

    #[tokio::test]
    async fn streamed_output() {
        let (_dir, _server, client) = start().await;
        let mut chunks = Vec::new();
        let v = client
            .send_streaming(
                LabRequest::Up {
                    machines: vec!["a".into(), "b".into()],
                    record: Vec::new(),
                    wait: false,
                },
                |c| chunks.push(c),
            )
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!(true));
        assert_eq!(chunks, vec!["starting a", "starting b"]);
    }

    #[tokio::test]
    async fn concurrent_requests_dont_block() {
        let (_dir, _server, client) = start().await;
        let slow = client.clone();
        let slow_task = tokio::spawn(async move { slow.send(LabRequest::Status {}).await });
        // The fast call completes while the slow one is still in flight.
        let fast = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            client.send(LabRequest::Ping {}),
        )
        .await
        .expect("fast call should not be blocked by slow one")
        .unwrap();
        assert_eq!(fast, Value::String("pong".into()));
        let slow_result = slow_task.await.unwrap().unwrap();
        assert_eq!(slow_result, Value::String("slow-done".into()));
    }

    #[tokio::test]
    async fn events_flow_after_subscribe() {
        let (_dir, server, client) = start().await;
        let mut rx = client.subscribe().await.unwrap();
        server.emit(Event::new(
            "vm.ready",
            "lab1",
            serde_json::json!({"vm": "dc01"}),
        ));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.event, "vm.ready");
        assert_eq!(ev.lab, "lab1");
        assert_eq!(ev.data["vm"], "dc01");
    }

    /// A bare listener standing in for a daemon, so a test sees what the
    /// client does to the socket: the accepted stream, line by line.
    async fn bare_daemon() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        tokio::net::UnixListener,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("bare.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        (dir, sock, listener)
    }

    const SHORT: std::time::Duration = std::time::Duration::from_millis(200);
    const LONG: std::time::Duration = std::time::Duration::from_secs(5);

    /// F10: dropping a clone leaves the connection usable; dropping the
    /// last one closes it, and the daemon reads EOF.
    #[tokio::test]
    async fn the_daemon_reads_eof_once_the_last_clone_drops() {
        let (_dir, sock, listener) = bare_daemon().await;
        let client = LabClient::connect(&sock).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (read, mut write) = server.into_split();
        let mut lines = BufReader::new(read).lines();

        let other = client.clone();
        drop(client);
        assert!(
            tokio::time::timeout(SHORT, lines.next_line())
                .await
                .is_err(),
            "the connection closed while a clone still holds it"
        );

        // The remaining clone still asks and is answered.
        let asking = tokio::spawn({
            let other = other.clone();
            async move { other.send(LabRequest::Ping {}).await }
        });
        let line = tokio::time::timeout(LONG, lines.next_line())
            .await
            .expect("the request arrives")
            .unwrap()
            .expect("a line, not EOF");
        let Message::Req { id, cmd, .. } = serde_json::from_str(&line).unwrap() else {
            panic!("{line}");
        };
        assert_eq!(cmd, "ping");
        let mut resp = serde_json::to_string(&Message::Resp {
            id,
            ok: Some(Value::String("pong".into())),
            err: None,
            code: None,
        })
        .unwrap();
        resp.push('\n');
        write.write_all(resp.as_bytes()).await.unwrap();
        let answer = tokio::time::timeout(LONG, asking).await.unwrap().unwrap();
        assert_eq!(answer.unwrap(), Value::String("pong".into()));

        drop(other);
        let end = tokio::time::timeout(LONG, lines.next_line())
            .await
            .expect("the daemon reads EOF, not silence");
        assert!(matches!(end, Ok(None)), "{end:?}");
    }

    /// A request still pending when the client goes is answered with an
    /// error rather than left to hang, and a subscription ends.
    #[tokio::test]
    async fn what_waits_on_a_dropped_client_ends() {
        let (_dir, sock, listener) = bare_daemon().await;
        let client = LabClient::connect(&sock).await.unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        let (tx, mut events) = mpsc::channel(1);
        *client.inner.routes.events() = Some(tx);
        let answer = client.send_req("ping", Value::Null, None).await.unwrap();
        drop(client);
        let answer = tokio::time::timeout(LONG, answer)
            .await
            .expect("the pending request hangs");
        let err = answer.unwrap().unwrap_err();
        assert!(err.message.contains("client dropped"), "{err:?}");
        let ended = tokio::time::timeout(LONG, events.recv()).await;
        assert!(matches!(ended, Ok(None)), "the subscription hangs");
    }

    /// A daemon that closes still fails what was waiting on it, as before.
    #[tokio::test]
    async fn a_closed_connection_fails_pending_requests() {
        let (_dir, sock, listener) = bare_daemon().await;
        let client = LabClient::connect(&sock).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let asking = tokio::spawn({
            let client = client.clone();
            async move { client.send(LabRequest::Ping {}).await }
        });
        // Read the request so it is in flight, then hang up.
        let mut lines = BufReader::new(server).lines();
        tokio::time::timeout(LONG, lines.next_line())
            .await
            .unwrap()
            .unwrap();
        drop(lines);
        let answer = tokio::time::timeout(LONG, asking).await.unwrap().unwrap();
        assert!(answer.is_err(), "{answer:?}");
    }

    /// The region argument reaches the daemon as a rectangle, not as a raw
    /// array the handler has to re-validate.
    #[tokio::test]
    async fn typed_arguments_survive_the_round_trip() {
        let req = LabRequest::MachineOcr {
            machine: "dc01".into(),
            region: Some(Region {
                x: 1,
                y: 2,
                w: 3,
                h: 4,
            }),
            options: crate::vision::OcrOptions {
                scale: Some(2),
                layout: Some(crate::vision::Layout::Line),
            },
        };
        let (cmd, args) = req.to_wire();
        assert_eq!(args["region"], serde_json::json!([1, 2, 3, 4]));
        assert_eq!(
            args["options"],
            serde_json::json!({"scale": 2, "layout": "line"})
        );
        assert_eq!(LabRequest::from_wire(cmd, args).unwrap(), req);
    }
}
