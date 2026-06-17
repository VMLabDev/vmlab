//! Host-side VNC viewer launching for `gui = true` and `vmlab console`
//! (PRD §11).
//!
//! Every VM runs headless with VNC on a unix socket; a `gui = true` window
//! is a *separate* viewer process the CLI launches, never QEMU's own GTK
//! window. Decoupling the window from the VM means closing it only
//! disconnects — the VM keeps running and `vmlab console` can reattach.
//!
//! The viewer is chosen by [`detect`]: an explicit `viewer` in host config
//! wins; otherwise we look for a known client on `PATH`. `remote-viewer`
//! (virt-viewer) speaks the VNC unix socket directly and is preferred —
//! it needs no bridge, so it also works for the `gui = true` auto-open on
//! `up` (where the CLI exits right after). `gvncviewer`/`vncviewer` are
//! TCP-only, so the caller bridges the socket to a localhost display port.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

/// How a viewer reaches the display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Connects to the VNC unix socket directly (no bridge needed).
    Unix,
    /// Needs a `host:display` TCP endpoint; the caller bridges first.
    Tcp,
}

/// A resolved viewer: a command template and how it connects. `{}` in the
/// template is replaced by the target (socket path or `host:display`).
#[derive(Debug, Clone)]
pub struct Viewer {
    pub cmd: String,
    pub transport: Transport,
}

/// The viewer command from host config, if one is configured.
fn configured_viewer() -> Option<String> {
    crate::config::host::HostConfig::load_default()
        .unwrap_or_default()
        .viewer
}

/// Whether `bin` is an executable on `PATH`.
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Resolve a viewer: host config first, then known clients on `PATH`.
/// `remote-viewer` is preferred because it dials the unix socket directly.
pub fn detect() -> Option<Viewer> {
    if let Some(cmd) = configured_viewer() {
        return Some(Viewer {
            cmd,
            transport: Transport::Unix,
        });
    }
    let known = [
        // remote-viewer (virt-viewer) is preferred, but its `vnc+unix://`
        // URI is unsupported in virt-viewer 11 (gtk-vnc aborts with a
        // `host != NULL` assertion), so drive it over the TCP bridge with a
        // plain `vnc://` URI — that works on every version. The detached
        // bridge helper holds the socket open, so the `gui = true` auto-open
        // still works after the CLI exits.
        ("remote-viewer", "remote-viewer vnc://{}", Transport::Tcp),
        ("gvncviewer", "gvncviewer {}", Transport::Tcp),
        ("vncviewer", "vncviewer {}", Transport::Tcp),
    ];
    for (bin, cmd, transport) in known {
        if on_path(bin) {
            return Some(Viewer {
                cmd: cmd.to_string(),
                transport,
            });
        }
    }
    None
}

/// Format a bridged TCP endpoint for a viewer. URI-style viewers
/// (`vnc://…`, e.g. remote-viewer) want the real port; gvncviewer/vncviewer
/// take a VNC *display* number (`port − 5900`).
pub fn tcp_target(viewer: &Viewer, host: &str, port: u16, display: u16) -> String {
    if viewer.cmd.contains("://") {
        format!("{host}:{port}")
    } else {
        format!("{host}:{display}")
    }
}

/// Spawn a viewer against a resolved target, returning the child so the
/// caller can wait on it. The target is a unix socket path for
/// [`Transport::Unix`] or a host endpoint from [`tcp_target`] for
/// [`Transport::Tcp`]. `{}` in the command template is the target; a
/// template without `{}` (a bare host-config command) gets `unix://<target>`
/// appended.
pub fn spawn_child(viewer: &Viewer, target: &str) -> Result<std::process::Child> {
    let line = if viewer.cmd.contains("{}") {
        viewer.cmd.replace("{}", target)
    } else {
        format!("{} unix://{target}", viewer.cmd)
    };
    let mut parts = line.split_whitespace();
    let prog = parts
        .next()
        .ok_or_else(|| anyhow!("empty viewer command"))?;
    std::process::Command::new(prog)
        .args(parts)
        .spawn()
        .with_context(|| format!("launching viewer `{}`", viewer.cmd))
}

/// Spawn a viewer and detach (we don't wait on it). For unix-socket
/// viewers that talk straight to QEMU's socket.
pub fn launch(viewer: &Viewer, target: &str) -> Result<()> {
    spawn_child(viewer, target)?;
    println!(
        "launched {}",
        viewer.cmd.split_whitespace().next().unwrap_or("")
    );
    Ok(())
}

/// Launch a detached background helper that holds a unix→TCP VNC bridge and
/// the viewer for a TCP-only client, freeing the caller's terminal. The
/// helper (`vmlab __vncbridge`) exits when the viewer window closes,
/// tearing the bridge down with it. Used by `vmlab console` and the
/// `gui = true` auto-open for gvncviewer/vncviewer.
pub fn spawn_detached_bridge(lab: &str, vm: &str) -> Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().context("locating vmlab binary")?;
    let exe = exe.to_string_lossy().into_owned();
    let args = ["__vncbridge", "--lab", lab, "--vm", vm];
    // `setsid` puts the helper in its own session so closing the terminal
    // doesn't SIGHUP it; fall back to a plain spawn if it isn't installed.
    let mut cmd = if on_path("setsid") {
        let mut c = Command::new("setsid");
        c.arg(&exe).args(args);
        c
    } else {
        let mut c = Command::new(&exe);
        c.args(args);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning background vnc bridge")?;
    Ok(())
}

/// Open a viewer for a running lab VM's display (its socket already exists),
/// used by the `gui = true` auto-open on `vmlab up`. A unix-socket viewer
/// is launched directly; a TCP-only viewer runs in a detached bridge so it
/// survives `up` exiting. Prints how to attach when no viewer is found.
pub fn open_for(lab: &str, vm: &str) -> Result<()> {
    let sock = crate::paths::lab_runtime_dir(lab)
        .join("vms")
        .join(vm)
        .join("vnc.sock");
    if !sock.exists() {
        return Ok(());
    }
    match detect() {
        Some(v) if v.transport == Transport::Unix => launch(&v, &sock.display().to_string()),
        Some(_) => {
            spawn_detached_bridge(lab, vm)?;
            println!("gui: opened a viewer for {vm}");
            Ok(())
        }
        None => {
            println!(
                "gui: no VNC viewer found — install virt-viewer (remote-viewer), or run \
                 `vmlab console {vm}`"
            );
            Ok(())
        }
    }
}

/// Spawn a background task that waits for a VM's VNC socket to appear (the
/// VM is still booting) and then opens a viewer. For `vmlab template
/// build`, whose `up()` blocks until provisioning finishes. Like
/// [`open_for`], only a unix-socket viewer is launched. Must be called from
/// within a tokio runtime.
pub fn open_when_ready(sock: PathBuf) {
    match detect() {
        Some(v) if v.transport == Transport::Unix => {
            tokio::spawn(async move {
                // ~60s for QEMU to create the socket; the build runs longer.
                for _ in 0..600 {
                    if sock.exists() {
                        let _ = launch(&v, &sock.display().to_string());
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            });
        }
        _ => println!(
            "gui: no unix-socket viewer for the build — install virt-viewer (remote-viewer) \
             to watch builds"
        ),
    }
}
