//! `vmlab console` — attach a viewer to a VM's VNC display (PRD §11).
//!
//! Every VM serves VNC on a unix socket. We pick a viewer with
//! [`crate::viewer::detect`] and launch it: a unix-socket viewer
//! (remote-viewer) dials the socket directly; a TCP-only viewer
//! (gvncviewer/vncviewer) gets a localhost display the bridge proxies to.
//! With `--tcp` or no viewer at all we just bridge and print the address —
//! the WSL2 path, where the viewer lives on the Windows side.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use super::lab::{current_lab, split_vm_ref};
use crate::cli::daemon;
use crate::viewer::{self, Transport};

pub fn cmd_console(vm_ref: &str, tcp: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let (lab_opt, vm) = split_vm_ref(vm_ref)?;
        let lab = match lab_opt {
            Some(l) => l,
            None => current_lab()?.0,
        };
        // Ensure the lab daemon is up so the socket exists.
        let _client = daemon::try_lab_daemon(&lab)
            .await
            .ok_or_else(|| anyhow!("lab \"{lab}\" is not running — `vmlab up` first"))?;

        let sock = crate::paths::lab_runtime_dir(&lab)
            .join("vms")
            .join(&vm)
            .join("vnc.sock");
        if !sock.exists() {
            bail!("VNC socket for {lab}/{vm} not found ({})", sock.display());
        }

        let viewer = viewer::detect();

        // No viewer (or --tcp forced): bridge and print the address for the
        // user to point any VNC client at (the WSL2 path).
        if tcp || viewer.is_none() {
            let (port, _) = bridge(&sock).await?;
            println!("VNC for {lab}/{vm} on 127.0.0.1:{port}");
            println!("connect a VNC viewer there (Ctrl-C to stop the bridge)");
            tokio::signal::ctrl_c().await.ok();
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
                viewer::spawn_detached_bridge(&lab, &vm)?;
                println!("opened {lab}/{vm} in a viewer (closes with the window)");
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
        let sock = crate::paths::lab_runtime_dir(&lab)
            .join("vms")
            .join(&vm)
            .join("vnc.sock");
        if !sock.exists() {
            bail!("VNC socket for {lab}/{vm} not found ({})", sock.display());
        }
        let viewer = viewer::detect().ok_or_else(|| anyhow!("no VNC viewer found"))?;
        let target = match viewer.transport {
            Transport::Unix => sock.display().to_string(),
            Transport::Tcp => {
                let (port, display) = bridge(&sock).await?;
                viewer::tcp_target(&viewer, "127.0.0.1", port, display)
            }
        };
        let mut child = viewer::spawn_child(&viewer, &target)?;
        // Hold the bridge alive until the viewer window is closed.
        tokio::task::spawn_blocking(move || child.wait()).await.ok();
        Ok(())
    })
}

/// Bridge the VM's VNC unix socket to a localhost TCP port. Binds a VNC
/// display port (5900 + N) when one is free so viewers taking `host:N`
/// work, falling back to any free port. Returns `(port, display)` where
/// `display = port - 5900` (only meaningful in the 5900..5999 range).
async fn bridge(sock: &Path) -> Result<(u16, u16)> {
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
            tokio::spawn(async move {
                if let Ok(unix) = tokio::net::UnixStream::connect(&sock).await {
                    let (mut tr, mut tw) = tcp.into_split();
                    let (mut ur, mut uw) = unix.into_split();
                    let a = tokio::io::copy(&mut tr, &mut uw);
                    let b = tokio::io::copy(&mut ur, &mut tw);
                    let _ = tokio::join!(a, b);
                }
            });
        }
    });
    Ok((port, display))
}
