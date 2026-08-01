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

struct Inner {
    write: Mutex<tokio::net::unix::OwnedWriteHalf>,
    pending: Mutex<HashMap<u64, Pending>>,
    events: Mutex<Option<mpsc::Sender<Event>>>,
    next_id: AtomicU64,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Cloneable async client for one daemon socket.
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
        let inner = Arc::new(Inner {
            write: Mutex::new(write_half),
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(None),
            next_id: AtomicU64::new(1),
            reader: Mutex::new(None),
        });
        let reader_inner = inner.clone();
        let handle = tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Message>(&line) else {
                    continue;
                };
                match msg {
                    Message::Resp { id, ok, err, code } => {
                        if let Some(p) = reader_inner.pending.lock().await.remove(&id) {
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
                        let pending = reader_inner.pending.lock().await;
                        if let Some(Pending {
                            chunks: Some(tx), ..
                        }) = pending.get(&id)
                        {
                            let _ = tx.try_send(chunk);
                        }
                    }
                    Message::Event { data, .. } => {
                        let guard = reader_inner.events.lock().await;
                        if let Some(tx) = guard.as_ref()
                            && let Ok(ev) = serde_json::from_value::<Event>(data)
                        {
                            let _ = tx.try_send(ev);
                        }
                    }
                    Message::Req { .. } => {}
                }
            }
            // Connection died: fail everything pending.
            let mut pending = reader_inner.pending.lock().await;
            for (_, p) in pending.drain() {
                let _ = p.resp.send(Err(CommandError::failed("connection closed")));
            }
        });
        *inner.reader.lock().await = Some(handle);
        Ok(Client {
            inner,
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
            .pending
            .lock()
            .await
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
        *self.inner.events.lock().await = Some(tx);
        let resp = self.send_req("subscribe", Value::Null, None).await?;
        match resp.await {
            Ok(Ok(_)) => Ok(rx),
            Ok(Err(e)) => Err(ProtoError::Remote(e)),
            Err(_) => Err(ProtoError::Closed),
        }
    }
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
                LabRequest::MachineStart { machine } => Err(CommandError::conflict(format!(
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
                LabRequest::Up { machines } => {
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
        };
        let (cmd, args) = req.to_wire();
        assert_eq!(args["region"], serde_json::json!([1, 2, 3, 4]));
        assert_eq!(LabRequest::from_wire(cmd, args).unwrap(), req);
    }
}
