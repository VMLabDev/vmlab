//! A unix socket that serves exactly one connection and then goes away.
//!
//! Two things in the lab daemon hand a caller a socket path instead of a
//! stream: an interactive terminal (`machine.tty_open`) and the SSH facade
//! (`machine.ssh_open`, PRD §19.3). Both have the same lifetime — bind,
//! accept once, serve, unlink — because both exist for exactly one process
//! that was just told where to connect. Nothing may outlive that process,
//! and an open nobody ever connects to must not leave a socket behind
//! either.
//!
//! It lives here rather than beside either caller so the grace period has
//! one definition: the two would otherwise have to agree by comment.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::{UnixListener, UnixStream};

/// How long the socket waits for its one client. Long enough for the process
/// that was handed the path to get there, short enough that an abandoned
/// open does not leave a socket behind.
pub const ACCEPT_GRACE: Duration = Duration::from_secs(60);

/// Bind `sock_path`, hand the first connection to `serve`, and unlink the
/// socket when that returns.
///
/// Returns as soon as the socket exists — the caller's reply carries the
/// path, so it must not wait for a client that has not been told where to
/// connect yet. `on_nobody` runs when the grace elapses with nobody there,
/// for a caller holding a resource that then has nothing to serve.
pub async fn serve_one<F, Fut, N, NFut>(sock_path: PathBuf, serve: F, on_nobody: N) -> Result<()>
where
    F: FnOnce(UnixStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
    N: FnOnce() -> NFut + Send + 'static,
    NFut: Future<Output = ()> + Send,
{
    let _ = std::fs::remove_file(&sock_path);
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    tokio::spawn(async move {
        match tokio::time::timeout(ACCEPT_GRACE, listener.accept()).await {
            Ok(Ok((stream, _))) => serve(stream).await,
            _ => on_nobody().await,
        }
        let _ = std::fs::remove_file(&sock_path);
    });
    Ok(())
}
