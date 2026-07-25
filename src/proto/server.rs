//! Protocol server side: accept unix connections, dispatch requests to a
//! handler, fan out events to subscribed connections.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

use super::{Event, Message};

/// Longest request line the server will buffer. Requests are small JSON
/// objects; anything approaching this is a broken or hostile client, and an
/// unbounded `read_line` would let it grow the daemon's memory at will.
const MAX_REQ_LINE: usize = 1 << 20;

/// Sink for incremental output of a long-running command. Dropping it is
/// fine — chunks are best-effort.
#[derive(Clone)]
pub struct Streamer {
    id: u64,
    tx: mpsc::Sender<Message>,
}

impl Streamer {
    pub async fn chunk(&self, text: impl Into<String>) {
        let _ = self
            .tx
            .send(Message::Stream {
                id: self.id,
                chunk: text.into(),
            })
            .await;
    }
}

/// Command handler implemented by the supervisor and lab daemons.
#[async_trait::async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn handle(&self, cmd: &str, args: Value, stream: &Streamer) -> Result<Value, String>;
}

/// A running protocol server bound to a unix socket.
pub struct Server {
    pub events: broadcast::Sender<Event>,
    handle: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Bind `path` (parent dirs created, stale socket file replaced) and
    /// serve until dropped/aborted.
    pub async fn bind(path: &Path, handler: Arc<dyn Handler>) -> std::io::Result<Server> {
        let (events, _) = broadcast::channel::<Event>(1024);
        Self::bind_with_events(path, handler, events).await
    }

    /// [`bind`](Self::bind) with a caller-supplied event channel, so the
    /// daemon can emit events without holding the server.
    pub async fn bind_with_events(
        path: &Path,
        handler: Arc<dyn Handler>,
        events: broadcast::Sender<Event>,
    ) -> std::io::Result<Server> {
        // The socket is a full-privilege interface (scripts, guest files,
        // daemon-side writes), so both the directory holding it and the socket
        // itself are owner-only — `bind` would otherwise honour the umask and
        // leave it connectable by any local user.
        if let Some(parent) = path.parent() {
            crate::paths::ensure_private_dir(parent).map_err(std::io::Error::other)?;
        }
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let events_accept = events.clone();
        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let handler = handler.clone();
                        let events = events_accept.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_conn(stream, handler, events).await {
                                tracing::debug!("connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });
        Ok(Server { events, handle })
    }

    /// Emit an event to all subscribed connections.
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Read one `\n`-terminated line, refusing to buffer more than
/// [`MAX_REQ_LINE`] bytes. `Ok(None)` is EOF; an over-long line ends the
/// connection (the stream is desynchronised at that point anyway).
async fn read_capped_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        // A BufReader hands back at most its capacity, so `line` grows in
        // bounded steps and the check below runs before it can balloon.
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8(line)?))
            };
        }
        let newline_at = available.iter().position(|b| *b == b'\n');
        let take = newline_at.unwrap_or(available.len());
        line.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(newline_at.is_some()));
        anyhow::ensure!(
            line.len() <= MAX_REQ_LINE,
            "request line exceeds {MAX_REQ_LINE} bytes"
        );
        if newline_at.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(String::from_utf8(line)?));
        }
    }
}

async fn serve_conn(
    stream: UnixStream,
    handler: Arc<dyn Handler>,
    events: broadcast::Sender<Event>,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // All outbound traffic for this connection funnels through one channel so
    // responses, stream chunks, and events interleave without tearing.
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let mut line = match serde_json::to_string(&msg) {
                Ok(l) => l,
                Err(_) => continue,
            };
            line.push('\n');
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut event_pump: Option<tokio::task::JoinHandle<()>> = None;

    while let Some(line) = read_capped_line(&mut reader).await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Message = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                let _ = out_tx
                    .send(Message::Resp {
                        id: 0,
                        ok: None,
                        err: Some(format!("bad message: {e}")),
                    })
                    .await;
                continue;
            }
        };
        let Message::Req { id, cmd, args } = msg else {
            continue; // clients only send requests
        };

        // `subscribe` flips this connection into event mode: events flow
        // until the client disconnects. It still gets a normal Resp.
        if cmd == "subscribe" && event_pump.is_none() {
            let mut rx = events.subscribe();
            let tx = out_tx.clone();
            event_pump = Some(tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let data = serde_json::to_value(&ev).unwrap_or(Value::Null);
                            if tx
                                .send(Message::Event {
                                    event: ev.event.clone(),
                                    data,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }));
            let _ = out_tx
                .send(Message::Resp {
                    id,
                    ok: Some(Value::Bool(true)),
                    err: None,
                })
                .await;
            continue;
        }

        let streamer = Streamer {
            id,
            tx: out_tx.clone(),
        };
        let handler = handler.clone();
        let out = out_tx.clone();
        // Handle each request on its own task so a long build doesn't block
        // a status query on the same connection.
        tokio::spawn(async move {
            let resp = match handler.handle(&cmd, args, &streamer).await {
                Ok(v) => Message::Resp {
                    id,
                    ok: Some(v),
                    err: None,
                },
                Err(e) => Message::Resp {
                    id,
                    ok: None,
                    err: Some(e),
                },
            };
            let _ = out.send(resp).await;
        });
    }

    if let Some(p) = event_pump {
        p.abort();
    }
    writer.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait::async_trait]
    impl Handler for Echo {
        async fn handle(&self, cmd: &str, _args: Value, _s: &Streamer) -> Result<Value, String> {
            Ok(Value::String(cmd.to_string()))
        }
    }

    /// The control socket is a full-privilege interface: owner-only socket in
    /// an owner-only directory, whatever the umask.
    #[tokio::test]
    #[cfg(unix)]
    async fn bind_restricts_socket_and_directory_modes() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run/labs/demo");
        let sock = dir.join("control.sock");
        let server = Server::bind(&sock, Arc::new(Echo)).await.unwrap();

        let sock_mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(sock_mode, 0o600, "socket mode {sock_mode:o}");
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "directory mode {dir_mode:o}");
        drop(server);
    }

    /// A directory owned by somebody else must never be used for sockets.
    #[test]
    #[cfg(unix)]
    fn foreign_owned_socket_directory_is_refused() {
        // /tmp itself is root-owned on any normal host — the exact shape of the
        // `/tmp/vmlab-<uid>` squat this guards against.
        if nix::unistd::Uid::effective().is_root() {
            eprintln!("SKIP: running as root, every directory passes the owner check");
            return;
        }
        let err = crate::paths::ensure_private_dir(std::path::Path::new("/tmp")).unwrap_err();
        assert!(err.to_string().contains("owned by uid"), "{err}");
    }

    #[tokio::test]
    async fn capped_line_reader_handles_split_crlf_and_eof() {
        let mut r = BufReader::new(std::io::Cursor::new(b"one\r\ntwo\nthree".to_vec()));
        assert_eq!(
            read_capped_line(&mut r).await.unwrap().as_deref(),
            Some("one")
        );
        assert_eq!(
            read_capped_line(&mut r).await.unwrap().as_deref(),
            Some("two")
        );
        // A trailing fragment with no newline is still delivered, then EOF.
        assert_eq!(
            read_capped_line(&mut r).await.unwrap().as_deref(),
            Some("three")
        );
        assert_eq!(read_capped_line(&mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn capped_line_reader_refuses_an_oversized_line() {
        let mut wire = vec![b'x'; MAX_REQ_LINE + 1];
        wire.push(b'\n');
        let mut r = BufReader::new(std::io::Cursor::new(wire));
        let err = read_capped_line(&mut r).await.unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }
}
