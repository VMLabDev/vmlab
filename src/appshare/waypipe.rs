//! The waypipe host end (PRD §20.3): `waypipe client`, listening on a private
//! socket, and one connection to it per connection the guest's
//! `waypipe server` makes.
//!
//! The waypipe client listens and the server connects, once per Wayland
//! client connection (`research.md` §A1), so the guest's connections arrive
//! as `pending` records and each is spliced to a fresh connection here. The
//! client reaches the compositor through the `WAYLAND_DISPLAY` it inherits.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::net::UnixStream;

use super::Adopt;

/// How long the client gets to create its socket before the session gives up.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

/// The client's arguments. Compression is pinned to the agent's, because a
/// client and server that disagree on it refuse each other (`research.md`
/// §A2).
pub fn client_args(socket: &Path) -> Vec<String> {
    vec![
        "--compress".into(),
        "lz4".into(),
        "--socket".into(),
        socket.display().to_string(),
        "client".into(),
    ]
}

/// A running `waypipe client` in its own private directory. Dropping it
/// kills the client and removes the directory.
pub struct Client {
    child: tokio::process::Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Client {
    /// Start the client and wait for its socket to appear.
    pub async fn start(bin: &Path, verbose: bool) -> Result<Self> {
        let dir = super::private_dir()?;
        let socket = dir.path().join("wp.sock");
        let mut child = tokio::process::Command::new(bin)
            .args(client_args(&socket))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if verbose {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting `{}`", bin.display()))?;
        let deadline = tokio::time::Instant::now() + SOCKET_TIMEOUT;
        while !socket.exists() {
            if let Some(status) = child.try_wait()? {
                bail!("the host's waypipe client exited before it listened ({status})");
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "the host's waypipe client did not create {} within {}s",
                    socket.display(),
                    SOCKET_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(Self {
            child,
            socket,
            _dir: dir,
        })
    }

    /// Wait for the client to exit, and return its exit code (`-1` for a
    /// signal).
    pub async fn wait(&mut self) -> Result<i32> {
        Ok(self.child.wait().await?.code().unwrap_or(-1))
    }

    /// Splice each adopted guest connection to a new connection to the
    /// client's socket.
    pub fn adopter(&self) -> Adopt {
        let socket = self.socket.clone();
        Arc::new(move |mut guest: UnixStream| {
            let socket = socket.clone();
            Box::pin(async move {
                let mut host = UnixStream::connect(&socket)
                    .await
                    .with_context(|| format!("connecting to {}", socket.display()))?;
                tokio::io::copy_bidirectional(&mut guest, &mut host).await?;
                Ok(())
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn start_fake_client(bin: &Path, verbose: bool) -> Client {
        // A parallel test's fork can inherit fs::write's handle until exec
        // or exit, even though our writer is already closed. Retry only
        // ETXTBSY, and keep both the attempts and their delay bounded.
        for attempt in 0..50 {
            match Client::start(bin, verbose).await {
                Ok(client) => return client,
                Err(error)
                    if attempt < 49
                        && error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                            e.kind() == std::io::ErrorKind::ExecutableFileBusy
                        }) =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("starting fake waypipe: {error:#}"),
            }
        }
        unreachable!("the last attempt returns or panics")
    }

    #[tokio::test]
    async fn stderr_probe() {
        let Some(bin) = std::env::var_os("VMLAB_TEST_WAYPIPE_BIN") else {
            return;
        };
        let verbose = std::env::var_os("VMLAB_TEST_WAYPIPE_VERBOSE").is_some();
        let _client = start_fake_client(Path::new(&bin), verbose).await;
    }

    #[test]
    fn host_diagnostics_are_quiet_unless_verbose() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = tempfile::tempdir().unwrap();
        let fake = scratch.path().join("waypipe");
        std::fs::write(
            &fake,
            "#!/usr/bin/env python3\nimport socket, sys, time\nprint('host-waypipe-diagnostic', file=sys.stderr, flush=True)\ns = socket.socket(socket.AF_UNIX); s.bind(sys.argv[sys.argv.index('--socket') + 1]); s.listen(1)\ntime.sleep(60)\n",
        ).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        for verbose in [false, true] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "appshare::waypipe::tests::stderr_probe",
                    "--exact",
                    "--nocapture",
                ])
                .env("VMLAB_TEST_WAYPIPE_BIN", &fake)
                .env_remove("VMLAB_TEST_WAYPIPE_VERBOSE");
            if verbose {
                command.env("VMLAB_TEST_WAYPIPE_VERBOSE", "1");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stderr).contains("host-waypipe-diagnostic"),
                verbose
            );
        }
    }

    /// The client listens on the session's private socket, with the
    /// compression the agent's server is started with.
    #[test]
    fn the_client_listens_on_the_private_socket_with_lz4() {
        assert_eq!(
            client_args(Path::new("/run/user/1000/vmlab-app-x/wp.sock")),
            [
                "--compress",
                "lz4",
                "--socket",
                "/run/user/1000/vmlab-app-x/wp.sock",
                "client"
            ]
        );
    }

    /// A stand-in client that listens where it is told: `start` waits for the
    /// socket, every adopted connection reaches it, and dropping the client
    /// removes its directory.
    #[tokio::test]
    async fn adopted_connections_reach_the_clients_socket() {
        let scratch = tempfile::tempdir().unwrap();
        let fake = scratch.path().join("waypipe");
        // Listens on the `--socket` argument and echoes one line back.
        std::fs::write(
            &fake,
            "#!/usr/bin/env python3\n\
             import socket, sys\n\
             path = sys.argv[sys.argv.index('--socket') + 1]\n\
             s = socket.socket(socket.AF_UNIX); s.bind(path); s.listen(1)\n\
             c, _ = s.accept(); c.sendall(c.recv(64)); c.close()\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let client = start_fake_client(&fake, false).await;
        let dir = client.socket.parent().unwrap().to_path_buf();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let splice = tokio::spawn(client.adopter()(theirs));
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        ours.write_all(b"hello").await.unwrap();
        let mut back = [0u8; 5];
        ours.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello");
        drop(ours);
        splice.await.unwrap().unwrap();

        drop(client);
        assert!(!dir.exists());
    }
}
