//! Shared interactive-terminal attach loop: bridge the local terminal to a
//! unix socket carrying raw PTY bytes (a VM agent terminal session or a
//! container's cinit shell). The local terminal goes raw for the session;
//! `Ctrl-]` detaches, like telnet. Resize is out-of-band — the caller
//! supplies a closure that tells the daemon about new dimensions.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};

/// An async "the terminal is now cols×rows" notification.
pub type ResizeFn = Arc<dyn Fn(u16, u16) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Attach the current terminal to the raw PTY socket at `path`. Returns when
/// the user detaches (`Ctrl-]`), the remote side closes, or stdin ends.
pub async fn attach_tty(path: &Path, banner: &str, resize: ResizeFn) -> Result<()> {
    let mut sock = tokio::net::UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting {}", path.display()))?;

    // Size the guest PTY to this terminal, now and on every SIGWINCH.
    let send_size = |resize: ResizeFn| async move {
        if let Ok(ws) = rustix::termios::tcgetwinsize(std::io::stdout()) {
            resize(ws.ws_col, ws.ws_row).await;
        }
    };
    send_size(resize.clone()).await;
    {
        let resize = resize.clone();
        tokio::spawn(async move {
            let Ok(mut winch) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            else {
                return;
            };
            while winch.recv().await.is_some() {
                send_size(resize.clone()).await;
            }
        });
    }

    println!("{banner}");

    // Raw mode with restore-on-drop (covers errors and ^] alike).
    struct RawGuard(rustix::termios::Termios);
    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = rustix::termios::tcsetattr(
                std::io::stdin(),
                rustix::termios::OptionalActions::Now,
                &self.0,
            );
        }
    }
    let saved = rustix::termios::tcgetattr(std::io::stdin()).context("not a terminal")?;
    let mut raw = saved.clone();
    raw.make_raw();
    rustix::termios::tcsetattr(
        std::io::stdin(),
        rustix::termios::OptionalActions::Now,
        &raw,
    )?;
    let _guard = RawGuard(saved);

    let (mut rx, mut tx) = sock.split();
    let mut stdin = read_input(std::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut outbuf = [0u8; 4096];
    loop {
        tokio::select! {
            input = stdin.recv() => {
                let Some(input) = input else { break };
                let input = input?;
                if input.is_empty() { break; }
                // Ctrl-] detaches; bytes before it still go through.
                if let Some(esc) = input.iter().position(|&b| b == 0x1d) {
                    if esc > 0 {
                        tokio::io::AsyncWriteExt::write_all(&mut tx, &input[..esc]).await?;
                    }
                    break;
                }
                tokio::io::AsyncWriteExt::write_all(&mut tx, &input).await?;
            }
            n = tokio::io::AsyncReadExt::read(&mut rx, &mut outbuf) => {
                let n = n?;
                if n == 0 { break; } // guest/QEMU gone
                tokio::io::AsyncWriteExt::write_all(&mut stdout, &outbuf[..n]).await?;
                tokio::io::AsyncWriteExt::flush(&mut stdout).await?;
            }
        }
    }
    drop(_guard);
    println!();
    Ok(())
}

/// Read `input` on a plain thread, handing each chunk over a channel; an
/// empty chunk is end of input.
///
/// Not `tokio::io::stdin()`: that reads on the runtime's blocking pool, and a
/// blocking read cannot be cancelled, so dropping the runtime waits for it.
/// When the guest shell exits the attach loop returns, but the process would
/// then sit until the user pressed one more key. A detached thread holds
/// nothing open; process exit takes it with it.
fn read_input<R: std::io::Read + Send + 'static>(
    mut input: R,
) -> tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let chunk = input.read(&mut buf).map(|n| buf[..n].to_vec());
            let last = !matches!(&chunk, Ok(b) if !b.is_empty());
            if tx.blocking_send(chunk).is_err() || last {
                return;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::read_input;
    use std::io::Write;
    use std::time::{Duration, Instant};

    /// The runtime an attach ran on must drop promptly while the local input
    /// still has a read outstanding: that read is what kept `vmlab shell`
    /// alive after the guest shell exited.
    #[test]
    fn a_pending_input_read_does_not_hold_the_runtime_open() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut input = read_input(reader);
            writer.write_all(b"ls\r").unwrap();
            assert_eq!(input.recv().await.unwrap().unwrap(), b"ls\r");
            // Nothing more is typed: the thread is now parked in `read`.
            let quiet = tokio::time::timeout(Duration::from_millis(50), input.recv()).await;
            assert!(quiet.is_err());
        });
        let started = Instant::now();
        drop(rt);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(writer);
    }

    #[test]
    fn end_of_input_arrives_as_an_empty_chunk() {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut input = read_input(reader);
            assert!(input.recv().await.unwrap().unwrap().is_empty());
            assert!(input.recv().await.is_none());
        });
    }
}
