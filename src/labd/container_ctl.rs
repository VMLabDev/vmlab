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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, RwLock, broadcast, watch};

use vmlab_cinit_proto::{CtlCommand, CtlEvent, PROTO_VERSION};

/// Broadcast capacity: cinit emits a handful of events over a container's
/// whole life, so 64 gives slow subscribers plenty of slack.
const EVENT_CAPACITY: usize = 64;

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
            let mut lines = BufReader::new(read_half).lines();
            // Ok(None) is EOF; Err is a dead socket. Both end the channel.
            while let Ok(Some(line)) = lines.next_line().await {
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
                        *reader_inner.last_ip.write().await = Some(ip.clone());
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
