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
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut inbuf = [0u8; 4096];
    let mut outbuf = [0u8; 4096];
    loop {
        tokio::select! {
            n = tokio::io::AsyncReadExt::read(&mut stdin, &mut inbuf) => {
                let n = n?;
                if n == 0 { break; }
                // Ctrl-] detaches; bytes before it still go through.
                if let Some(esc) = inbuf[..n].iter().position(|&b| b == 0x1d) {
                    if esc > 0 {
                        tokio::io::AsyncWriteExt::write_all(&mut tx, &inbuf[..esc]).await?;
                    }
                    break;
                }
                tokio::io::AsyncWriteExt::write_all(&mut tx, &inbuf[..n]).await?;
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
