//! Client for the container micro-VM's ctl channel — the `vmlab.ctl.0`
//! virtio-serial port carrying newline-delimited JSON (see
//! `guest/cinit-proto`). QEMU owns the socket (`server=on,wait=off`); the
//! host connects as a client, like the vmlab-agent client does.
//!
//! A background reader task parses incoming lines into
//! [`vmlab_cinit_proto::CtlEvent`]s, fans them out on a broadcast channel,
//! and keeps a small state cache (last DHCP IP, started, exited) so callers
//! can await milestones without replaying the event stream. EOF — QEMU gone —
//! closes the channel gracefully: the reader exits and pending waiters get a
//! "channel closed" error.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, RwLock, broadcast, watch};

use vmlab_cinit_proto::{CtlCommand, CtlEvent, PROTO_VERSION};

/// Broadcast capacity: cinit emits a handful of events over a container's
/// whole life, so 64 gives slow subscribers plenty of slack.
const EVENT_CAPACITY: usize = 64;
const MAX_CTL_LINE: usize = 64 * 1024;

/// Handle to one container's ctl channel. Cheap to clone (`Arc` inner);
/// dropping every handle does not tear the reader down — that ends on EOF
/// when QEMU exits.
#[derive(Clone)]
// Consumed by labd::container, which is not wired into the daemon yet.
pub struct CtlHandle {
    inner: Arc<CtlInner>,
}

struct CtlInner {
    writer: Mutex<OwnedWriteHalf>,
    events: broadcast::Sender<CtlEvent>,
    /// Last `net_up` IP reported by the guest.
    last_ip: RwLock<Option<String>>,
    /// Flips true on the `started` event.
    started: watch::Receiver<bool>,
    /// Set to the exit code on the `exited` event. The senders live in the
    /// reader task, so EOF closes these channels and wakes waiters.
    exited: watch::Receiver<Option<i32>>,
}

impl CtlHandle {
    /// Connect to the ctl socket and start the reader task. Single attempt —
    /// retry-on-startup policy belongs to the caller (QEMU creates the
    /// socket when it starts, so [`crate::labd::container`] retries like it
    /// does for QMP).
    pub async fn connect(path: &Path) -> Result<CtlHandle> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connecting ctl socket {}", path.display()))?;
        let (read_half, write_half) = stream.into_split();

        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (started_tx, started_rx) = watch::channel(false);
        let (exited_tx, exited_rx) = watch::channel(None);
        let inner = Arc::new(CtlInner {
            writer: Mutex::new(write_half),
            events: events.clone(),
            last_ip: RwLock::new(None),
            started: started_rx,
            exited: exited_rx,
        });

        let reader_inner = inner.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            // Ok(None) is EOF; Err is a dead socket. Both end the channel.
            // A bad line does not: it is skipped like a line that is not JSON.
            while let Ok(Some(line)) = read_ctl_line(&mut reader).await {
                let line = match line {
                    CtlLine::Text(line) => line,
                    CtlLine::Skipped(why) => {
                        tracing::warn!("ctl: skipped an event line: {why}");
                        continue;
                    }
                };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let ev: CtlEvent = match serde_json::from_str(line) {
                    Ok(ev) => ev,
                    Err(e) => {
                        tracing::warn!("ctl: unparseable event line {line:?}: {e}");
                        continue;
                    }
                };
                match &ev {
                    CtlEvent::Boot { proto_version } => {
                        if *proto_version != PROTO_VERSION {
                            tracing::error!(
                                "ctl: guest init speaks proto v{proto_version}, \
                                 host expects v{PROTO_VERSION} — rebuild the guest asset"
                            );
                        }
                    }
                    CtlEvent::NetUp { ip } => {
                        let Ok(ip) = ip.parse::<std::net::Ipv4Addr>() else {
                            continue;
                        };
                        *reader_inner.last_ip.write().await = Some(ip.to_string());
                    }
                    CtlEvent::Started { .. } | CtlEvent::Idle => {
                        let _ = started_tx.send(true);
                    }
                    CtlEvent::Exited { code } => {
                        let _ = exited_tx.send(Some(*code));
                    }
                    CtlEvent::Health { .. } => {}
                }
                let _ = reader_inner.events.send(ev);
            }
            // EOF: dropping started_tx/exited_tx here closes the watch
            // channels, which is how waiters learn the guest is gone.
        });

        Ok(CtlHandle { inner })
    }

    /// Send one command as a JSON line.
    pub async fn send(&self, cmd: &CtlCommand) -> Result<()> {
        let mut line = serde_json::to_string(cmd)?;
        line.push('\n');
        let mut writer = self.inner.writer.lock().await;
        writer
            .write_all(line.as_bytes())
            .await
            .context("writing ctl command")?;
        writer.flush().await.context("flushing ctl command")?;
        Ok(())
    }

    /// Subscribe to the raw event stream (from now on — no replay).
    pub fn subscribe(&self) -> broadcast::Receiver<CtlEvent> {
        self.inner.events.subscribe()
    }

    /// Last IP the guest reported via `net_up`, if any yet.
    pub async fn ip(&self) -> Option<String> {
        self.inner.last_ip.read().await.clone()
    }

    /// Wait until the container process has started (the `started` event).
    #[allow(dead_code)] // unused today; kept for ctl-channel API completeness
    pub async fn wait_started(&self, timeout: Duration) -> Result<()> {
        let mut rx = self.inner.started.clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if *rx.borrow() {
                return Ok(());
            }
            tokio::time::timeout_at(deadline, rx.changed())
                .await
                .map_err(|_| anyhow!("container did not start within {timeout:?}"))?
                .map_err(|_| anyhow!("ctl channel closed before the container started"))?;
        }
    }

    /// Wait for the container's exit code (the `exited` event).
    #[allow(dead_code)] // unused today; kept for ctl-channel API completeness
    pub async fn wait_exited(&self, timeout: Duration) -> Result<i32> {
        let mut rx = self.inner.exited.clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(code) = *rx.borrow() {
                return Ok(code);
            }
            tokio::time::timeout_at(deadline, rx.changed())
                .await
                .map_err(|_| anyhow!("container did not exit within {timeout:?}"))?
                .map_err(|_| anyhow!("ctl channel closed before the container exited"))?;
        }
    }
}

/// One read of the ctl stream.
#[derive(Debug, PartialEq)]
enum CtlLine {
    /// A whole line of text, its newline included.
    Text(String),
    /// A line the reader dropped, and why. The stream is at the start of the
    /// next line.
    Skipped(&'static str),
}

/// Read one line. `Ok(None)` is EOF and `Err` a dead socket. A line that is
/// longer than [`MAX_CTL_LINE`] or not UTF-8 is not an error: it is dropped
/// through its newline, in bounded memory, so one bad line costs that line
/// and not the channel (review F11).
async fn read_ctl_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> std::io::Result<Option<CtlLine>> {
    let mut bytes = Vec::new();
    let n = (&mut *reader)
        .take((MAX_CTL_LINE + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if n > MAX_CTL_LINE {
        if bytes.last() != Some(&b'\n') {
            discard_through_newline(reader).await?;
        }
        return Ok(Some(CtlLine::Skipped("longer than the line cap")));
    }
    Ok(Some(match String::from_utf8(bytes) {
        Ok(line) => CtlLine::Text(line),
        Err(_) => CtlLine::Skipped("not UTF-8"),
    }))
}

/// Consume the rest of the current line, holding no more of it than the
/// reader's own buffer. EOF ends it too: the next read reports that.
async fn discard_through_newline(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> std::io::Result<()> {
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(());
        }
        match buf.iter().position(|b| *b == b'\n') {
            Some(at) => {
                reader.consume(at + 1);
                return Ok(());
            }
            None => {
                let len = buf.len();
                reader.consume(len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::net::UnixListener;

    const LONG: Duration = Duration::from_secs(5);
    const SHORT: Duration = Duration::from_millis(200);

    /// Bind the socket (as QEMU would) and hand the accepted connection to
    /// `serve`, mirroring the vmlab-agent client's mock-server tests.
    async fn spawn_mock<F, Fut>(serve: F) -> (tempfile::TempDir, PathBuf)
    where
        F: FnOnce(UnixStream) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&path).expect("bind mock ctl socket");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            serve(stream).await;
        });
        (dir, path)
    }

    async fn write_line(stream: &mut UnixStream, ev: &CtlEvent) {
        let mut line = serde_json::to_string(ev).unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn unterminated_ctl_line_is_dropped_at_cap() {
        let input = vec![b'x'; MAX_CTL_LINE + 1];
        let mut reader = BufReader::new(input.as_slice());
        assert!(matches!(
            read_ctl_line(&mut reader).await.unwrap(),
            Some(CtlLine::Skipped(_))
        ));
        assert_eq!(read_ctl_line(&mut reader).await.unwrap(), None);
    }

    /// The cap is on the line with its newline, and a dropped line takes
    /// nothing of the next one with it.
    #[tokio::test]
    async fn a_dropped_line_leaves_the_next_line_whole() {
        for len in [MAX_CTL_LINE - 1, MAX_CTL_LINE, MAX_CTL_LINE + 1, 200_000] {
            let mut input = vec![b'x'; len];
            input.extend_from_slice(b"\nnext\n");
            let mut reader = BufReader::new(input.as_slice());
            let first = read_ctl_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(
                matches!(first, CtlLine::Text(_)),
                len < MAX_CTL_LINE,
                "a line of {len} bytes and a newline"
            );
            assert_eq!(
                read_ctl_line(&mut reader).await.unwrap(),
                Some(CtlLine::Text("next\n".into())),
                "after a line of {len} bytes"
            );
        }
    }

    #[tokio::test]
    async fn invalid_net_up_ip_is_not_cached() {
        let (_dir, path) = spawn_mock(|mut stream| async move {
            write_line(
                &mut stream,
                &CtlEvent::NetUp {
                    ip: "127.0.0.1; hostile".into(),
                },
            )
            .await;
            write_line(&mut stream, &CtlEvent::Started { pid: 1 }).await;
        })
        .await;
        let ctl = CtlHandle::connect(&path).await.unwrap();
        ctl.wait_started(LONG).await.unwrap();
        assert!(ctl.inner.last_ip.read().await.is_none());
    }

    #[tokio::test]
    async fn events_update_state_and_broadcast() {
        let (_dir, path) = spawn_mock(|mut stream| async move {
            write_line(&mut stream, &CtlEvent::Boot { proto_version: 1 }).await;
            write_line(
                &mut stream,
                &CtlEvent::NetUp {
                    ip: "10.0.0.9".into(),
                },
            )
            .await;
            write_line(&mut stream, &CtlEvent::Started { pid: 7 }).await;

            // Expect a stop command, then report the exit.
            {
                let (read_half, _write_half) = stream.split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let cmd: CtlCommand = serde_json::from_str(line.trim()).unwrap();
                assert_eq!(cmd, CtlCommand::Stop { grace_secs: 3 });
            }
            write_line(&mut stream, &CtlEvent::Exited { code: 0 }).await;
        })
        .await;

        let ctl = CtlHandle::connect(&path).await.unwrap();
        let mut rx = ctl.subscribe();

        ctl.wait_started(LONG).await.unwrap();
        assert_eq!(ctl.ip().await.as_deref(), Some("10.0.0.9"));

        ctl.send(&CtlCommand::Stop { grace_secs: 3 }).await.unwrap();
        assert_eq!(ctl.wait_exited(LONG).await.unwrap(), 0);

        // The broadcast stream carries every event in order.
        assert_eq!(
            rx.recv().await.unwrap(),
            CtlEvent::Boot { proto_version: 1 }
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            CtlEvent::NetUp {
                ip: "10.0.0.9".into()
            }
        );
        assert_eq!(rx.recv().await.unwrap(), CtlEvent::Started { pid: 7 });
        assert_eq!(rx.recv().await.unwrap(), CtlEvent::Exited { code: 0 });
    }

    #[tokio::test]
    async fn garbage_lines_are_skipped() {
        let (_dir, path) = spawn_mock(|mut stream| async move {
            stream.write_all(b"not json at all\n\n").await.unwrap();
            write_line(&mut stream, &CtlEvent::Started { pid: 1 }).await;
            // Keep the connection open until the client is done.
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
        .await;

        let ctl = CtlHandle::connect(&path).await.unwrap();
        ctl.wait_started(LONG).await.unwrap();
    }

    /// Review F11: one byte that is not UTF-8 costs that line, not the
    /// channel.
    #[tokio::test]
    async fn a_line_that_is_not_utf8_is_skipped() {
        let (_dir, path) = spawn_mock(|mut stream| async move {
            stream.write_all(b"\xff\n").await.unwrap();
            write_line(&mut stream, &CtlEvent::Started { pid: 1 }).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
        .await;

        let ctl = CtlHandle::connect(&path).await.unwrap();
        ctl.wait_started(LONG).await.unwrap();
    }

    /// Review F11: a line past the cap is dropped up to its newline, and the
    /// event after it is still seen.
    #[tokio::test]
    async fn a_line_that_is_too_long_is_skipped() {
        let (_dir, path) = spawn_mock(|mut stream| async move {
            let mut long = vec![b'x'; 70_000];
            long.push(b'\n');
            stream.write_all(&long).await.unwrap();
            write_line(&mut stream, &CtlEvent::Started { pid: 1 }).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
        .await;

        let ctl = CtlHandle::connect(&path).await.unwrap();
        ctl.wait_started(LONG).await.unwrap();
    }

    /// Review F2: a peer that floods `boot` and never reads fills the socket
    /// buffer under the watcher's answers. The blocked answer must not hold
    /// the writer for ever, or a stop behind it never reaches its next rung.
    /// The clock is paused, so it moves only when every task is blocked: the
    /// test has no wall-clock limit of its own.
    #[tokio::test(start_paused = true)]
    async fn a_ctl_peer_that_never_reads_does_not_block_a_stop() {
        let (_dir, path) = spawn_mock(|mut stream| async move {
            write_line(&mut stream, &CtlEvent::Boot { proto_version: 1 }).await;
            // Keep the socket open and never read it.
            std::future::pending::<()>().await;
        })
        .await;
        let ctl = CtlHandle::connect(&path).await.unwrap();

        // The watcher's answer to every `boot`: the whole spec.
        let spec: vmlab_cinit_proto::ContainerSpec = serde_json::from_value(serde_json::json!({
            "hostname": "silent",
            "env": [["PAD", "x".repeat(16 * 1024)]],
        }))
        .unwrap();
        let answer = CtlCommand::Spec {
            spec: Box::new(spec),
        };
        let watcher = {
            let ctl = ctl.clone();
            tokio::spawn(async move {
                let mut sent = 0u32;
                while ctl.send(&answer).await.is_ok() {
                    sent += 1;
                }
                sent
            })
        };

        // Paused time passes only once the watcher is stuck in a write.
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(!watcher.is_finished(), "the watcher is blocked in its write");

        let stop = {
            let ctl = ctl.clone();
            tokio::spawn(async move { ctl.send(&CtlCommand::Stop { grace_secs: 3 }).await })
        };
        let err = stop.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("ctl"), "{err:#}");

        // The watcher's own write gave up too, after some answers went out.
        assert!(watcher.await.unwrap() > 0);
        // A write that was cut short leaves half a line on the wire, so the
        // channel takes no further command.
        let err = ctl.send(&CtlCommand::Resync).await.unwrap_err();
        assert!(format!("{err:#}").contains("no longer usable"), "{err:#}");
    }

    #[tokio::test]
    async fn eof_closes_waiters_gracefully() {
        let (_dir, path) = spawn_mock(|mut stream| async move {
            write_line(&mut stream, &CtlEvent::Boot { proto_version: 1 }).await;
            // QEMU dies: the socket closes without `exited`.
        })
        .await;

        let ctl = CtlHandle::connect(&path).await.unwrap();
        let err = ctl.wait_exited(LONG).await.unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
        let err = ctl.wait_started(LONG).await.unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
    }

    #[tokio::test]
    async fn wait_times_out_when_nothing_happens() {
        let (_dir, path) = spawn_mock(|mut stream| async move {
            write_line(&mut stream, &CtlEvent::Boot { proto_version: 1 }).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .await;

        let ctl = CtlHandle::connect(&path).await.unwrap();
        let err = ctl.wait_started(SHORT).await.unwrap_err();
        assert!(err.to_string().contains("did not start"), "{err}");
        assert!(ctl.ip().await.is_none());
    }
}
