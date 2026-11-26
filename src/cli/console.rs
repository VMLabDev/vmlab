//! `vmlab console` attaches a native or VNC viewer to a VM (PRD §11).
//!
//! Every VM serves VNC on a unix socket. We pick a viewer with
//! [`crate::viewer::detect`] and launch it: a unix-socket viewer
//! (remote-viewer) dials the socket directly; a TCP-only viewer
//! (gvncviewer/vncviewer) gets a localhost display the bridge proxies to.
//! With `--tcp` or no viewer at all we just bridge and print the address —
//! the WSL2 path, where the viewer lives on the Windows side.
//!
//! A localhost port is open to every user of the host, and `vnc.sock`
//! asks its clients for nothing. So each bridge has a password of its own
//! and asks every TCP peer for it ([`crate::vnc_auth`]) before it connects
//! that peer to the socket.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use super::lab::{current_lab, lab_status, split_vm_ref};
use crate::cli::daemon;
use crate::status::{MachineKind, PowerState};
use crate::viewer::{self, ConsoleViewer, Transport};

pub fn cmd_console(vm_ref: &str, tcp: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let (lab_opt, vm) = split_vm_ref(vm_ref)?;
        let lab = match lab_opt {
            Some(l) => l,
            None => {
                // The cwd's lab: a name registered from another checkout is
                // that checkout's display, not this one's (ADR-0011).
                let (name, root) = current_lab()?;
                daemon::check_registration(&name, &root).await?;
                name
            }
        };
        // Ensure the lab daemon is up so the socket exists.
        let client = daemon::try_lab_daemon(&lab)
            .await?
            .ok_or_else(|| anyhow!("lab \"{lab}\" is not running — `vmlab up` first"))?;

        let status = lab_status(&client).await?;
        let machine = status
            .machines
            .iter()
            .find(|m| m.name == vm)
            .ok_or_else(|| anyhow!("machine {lab}/{vm} not found"))?;
        if machine.kind() != MachineKind::Vm {
            bail!("machine {lab}/{vm} has no console display");
        }
        if machine.state == PowerState::Stopped {
            bail!("machine {lab}/{vm} is stopped — start it first");
        }
        let viewer = match viewer::detect_for(machine.hypervisor, tcp) {
            ConsoleViewer::Native(viewer) => return viewer.launch(&lab, &vm),
            ConsoleViewer::Vnc(viewer) => viewer,
        };

        let sock = crate::paths::machine_runtime_dir(&lab, crate::paths::MachineKind::Vm, &vm)
            .join("vnc.sock");
        if !sock.exists() {
            bail!("VNC socket for {lab}/{vm} not found ({})", sock.display());
        }

        // No viewer (or --tcp forced): bridge and print the address for the
        // user to point any VNC client at (the WSL2 path).
        if tcp || viewer.is_none() {
            let password = crate::vnc_auth::new_password();
            let (port, _) = bridge(&sock, password.clone()).await?;
            // Take SIGINT before the address is printed: a reader that acts
            // on the address line may interrupt at once, and until the
            // handler is installed an interrupt kills the process.
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            println!("VNC for {lab}/{vm} on 127.0.0.1:{port}");
            println!("password: {password}");
            println!("connect a VNC viewer there (Ctrl-C to stop the bridge)");
            interrupt.recv().await;
            return Ok(());
        }

        let viewer = viewer.unwrap();
        match viewer.transport {
            // A host-configured unix-socket viewer dials the socket directly;
            // nothing to hold open.
            Transport::Unix => viewer::launch(&viewer, &sock.display().to_string()),
            // remote-viewer/gvncviewer/vncviewer need a TCP bridge held open
            // for the life
            // of the viewer. Run it in a detached helper so the terminal is
            // freed; the helper exits when the viewer window closes.
            Transport::Tcp => {
                let password = viewer::spawn_detached_bridge(&lab, &vm)?;
                println!("opened {lab}/{vm} in a viewer (closes with the window)");
                println!("if the viewer asks for a password: {password}");
                Ok(())
            }
        }
    })
}

/// Hold a unix→TCP VNC bridge and the viewer for a backgrounded console
/// (the hidden `__vncbridge` verb spawned by [`viewer::spawn_detached_bridge`]).
/// Blocks until the viewer window closes, then exits — tearing the bridge
/// down. Runs detached, so it never ties up the user's terminal.
pub fn run_bridge(lab: String, vm: String) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let sock = crate::paths::machine_runtime_dir(&lab, crate::paths::MachineKind::Vm, &vm)
            .join("vnc.sock");
        if !sock.exists() {
            bail!("VNC socket for {lab}/{vm} not found ({})", sock.display());
        }
        let viewer = viewer::detect().ok_or_else(|| anyhow!("no VNC viewer found"))?;
        // `_handoff` is the file the viewer reads the password from. It is
        // removed when the helper ends.
        let (mut child, _handoff) = match viewer.transport {
            Transport::Unix => (
                viewer::spawn_child(&viewer, &sock.display().to_string())?,
                None,
            ),
            Transport::Tcp => {
                // The command that started this helper made the password,
                // so that it could print it. It is in the environment and
                // not in the arguments, which every user of the host reads.
                let password = std::env::var(viewer::BRIDGE_PASSWORD_ENV)
                    .ok()
                    .filter(|password| crate::vnc_auth::is_password(password))
                    .unwrap_or_else(crate::vnc_auth::new_password);
                let (port, display) = bridge(&sock, password.clone()).await?;
                viewer::spawn_bridged(&viewer, port, display, &password)?
            }
        };
        // Hold the bridge alive until the viewer window is closed.
        tokio::task::spawn_blocking(move || child.wait()).await.ok();
        Ok(())
    })
}

/// Bridge the VM's VNC unix socket to a localhost TCP port. Binds a VNC
/// display port (5900 + N) when one is free so viewers taking `host:N`
/// work, falling back to any free port. Returns `(port, display)` where
/// `display = port - 5900` (only meaningful in the 5900..5999 range).
///
/// Every TCP peer must answer a VNC authentication challenge with
/// `password` before the bridge connects it to the socket.
async fn bridge(sock: &Path, password: String) -> Result<(u16, u16)> {
    use tokio::net::TcpListener;
    // Prefer a display port so `host:display` viewers connect cleanly.
    let mut listener = None;
    for display in 0u16..100 {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", 5900 + display)).await {
            listener = Some(l);
            break;
        }
    }
    let listener = match listener {
        Some(l) => l,
        None => TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("binding console TCP bridge")?,
    };
    let port = listener.local_addr()?.port();
    let display = port.wrapping_sub(5900);
    let sock = sock.to_path_buf();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                break;
            };
            let sock = sock.clone();
            let password = password.clone();
            tokio::spawn(async move {
                let _ = serve_viewer(tcp, &sock, &password).await;
            });
        }
    });
    Ok((port, display))
}

/// One bridged connection: admit the viewer, connect it to `vnc.sock`,
/// and copy both ways until either side closes.
async fn serve_viewer(
    mut tcp: tokio::net::TcpStream,
    sock: &Path,
    password: &str,
) -> std::io::Result<()> {
    use crate::vnc_auth;
    use tokio::io::AsyncWriteExt;

    // Nothing reaches the socket for a peer without the password.
    let admitted = vnc_auth::challenge_viewer(&mut tcp, password).await?;
    let mut unix = match open_console(sock).await {
        Ok(unix) => unix,
        Err(e) => {
            vnc_auth::refuse(&mut tcp, &admitted, "the console is not available").await?;
            return Err(e);
        }
    };
    vnc_auth::admit(&mut tcp, admitted).await?;

    // RFB has no half-open state: when either side closes, the session is
    // over, and the other side must see a close too. `copy` shuts no
    // writer down, so waiting for both copies would keep the viewer on a
    // dead server, and a closed viewer's session in the display server.
    let (mut tr, mut tw) = tcp.split();
    let (mut ur, mut uw) = unix.split();
    tokio::select! {
        _ = tokio::io::copy(&mut tr, &mut uw) => {}
        _ = tokio::io::copy(&mut ur, &mut tw) => {}
    }
    let _ = tw.shutdown().await;
    let _ = uw.shutdown().await;
    Ok(())
}

/// Connect to `vnc.sock` and take the handshake to where an admitted
/// viewer's stands.
async fn open_console(sock: &Path) -> std::io::Result<tokio::net::UnixStream> {
    let mut unix = tokio::net::UnixStream::connect(sock).await?;
    crate::vnc_auth::open_server(&mut unix).await?;
    Ok(unix)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UnixListener, UnixStream};
    use tokio::sync::mpsc;

    use super::*;

    /// A fake `vnc.sock`: an RFB server that offers security type None, as
    /// vmlab-hv and QEMU do. It counts what it accepts and hands each
    /// connection to the test once the security result is sent.
    struct FakeVnc {
        _dir: tempfile::TempDir,
        sock: PathBuf,
        accepted: Arc<AtomicUsize>,
        sessions: mpsc::UnboundedReceiver<UnixStream>,
    }

    fn fake_vnc() -> FakeVnc {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vnc.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let (tx, sessions) = mpsc::unbounded_channel();
        let count = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut unix, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut version = [0u8; 12];
                    unix.write_all(b"RFB 003.008\n").await.ok()?;
                    unix.read_exact(&mut version).await.ok()?;
                    unix.write_all(&[1, 1]).await.ok()?;
                    unix.read_u8().await.ok()?;
                    unix.write_u32(0).await.ok()?;
                    tx.send(unix).ok()
                });
            }
        });
        FakeVnc {
            _dir: dir,
            sock,
            accepted,
            sessions,
        }
    }

    /// The viewer's half of the RFB handshake, up to the security result,
    /// which it returns (0 is success). It takes security type None when
    /// the server offers it, and else answers the VNC authentication
    /// challenge with `password`.
    async fn viewer_handshake(tcp: &mut TcpStream, password: &str) -> u32 {
        let mut version = [0u8; 12];
        tcp.read_exact(&mut version).await.unwrap();
        tcp.write_all(b"RFB 003.008\n").await.unwrap();
        let mut types = vec![0u8; tcp.read_u8().await.unwrap() as usize];
        tcp.read_exact(&mut types).await.unwrap();
        if types.contains(&1) {
            tcp.write_all(&[1]).await.unwrap();
        } else {
            assert_eq!(types, [2], "the bridge offers VNC authentication only");
            tcp.write_all(&[2]).await.unwrap();
            let mut challenge = [0u8; 16];
            tcp.read_exact(&mut challenge).await.unwrap();
            tcp.write_all(&crate::vnc_auth::response(password, &challenge))
                .await
                .unwrap();
        }
        tcp.read_u32().await.unwrap()
    }

    const PASSWORD: &str = "Ab3dEf7h";

    // display-2 F9: the bridge copied every TCP connection to `vnc.sock`
    // and asked the peer for nothing.
    #[tokio::test]
    async fn the_console_bridge_does_not_serve_an_unknown_tcp_peer() {
        let vnc = fake_vnc();
        let (port, _) = bridge(&vnc.sock, PASSWORD.into()).await.unwrap();
        let mut tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        // A peer vmlab did not start has no password to give.
        let result = viewer_handshake(&mut tcp, "").await;
        assert_ne!(result, 0, "the bridge let a peer in that gave no password");
        assert_eq!(
            vnc.accepted.load(Ordering::SeqCst),
            0,
            "the bridge connected an unknown peer to vnc.sock"
        );
        // The refusal ends the connection.
        let mut rest = Vec::new();
        let _ = tcp.read_to_end(&mut rest).await;
        assert_eq!(vnc.accepted.load(Ordering::SeqCst), 0);
    }

    // display-2 F12, first effect: the VM stops, and the viewer never
    // learns that the server is gone.
    #[tokio::test]
    async fn the_bridge_passes_a_server_close_to_the_viewer() {
        let mut vnc = fake_vnc();
        let (port, _) = bridge(&vnc.sock, PASSWORD.into()).await.unwrap();
        let mut tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        assert_eq!(viewer_handshake(&mut tcp, PASSWORD).await, 0);
        drop(vnc.sessions.recv().await.unwrap());
        let mut buf = [0u8; 16];
        assert_eq!(tcp.read(&mut buf).await.unwrap(), 0);
    }

    // display-2 F12, second effect: the viewer closes, and its session
    // stays in the display server.
    #[tokio::test]
    async fn the_bridge_passes_a_viewer_close_to_the_server() {
        let mut vnc = fake_vnc();
        let (port, _) = bridge(&vnc.sock, PASSWORD.into()).await.unwrap();
        let mut tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        assert_eq!(viewer_handshake(&mut tcp, PASSWORD).await, 0);
        let mut unix = vnc.sessions.recv().await.unwrap();
        drop(tcp);
        let mut buf = [0u8; 16];
        assert_eq!(unix.read(&mut buf).await.unwrap(), 0);
    }
}
