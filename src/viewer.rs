//! Host-side viewer launching for `gui = true` and `vmlab console`
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
//!
//! vmlab-hv machines prefer the native viewer via [`detect_for`]. It connects
//! to `display.sock` and uses the lab control socket for clipboard requests.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::config::host::HostConfig;
use crate::config::hypervisor::HypervisorKind;

/// A vmlab-hv viewer launched directly against the native display protocol.
#[derive(Debug, Clone)]
pub struct NativeViewer {
    pub binary: PathBuf,
    pub release_chord: Option<String>,
}

impl NativeViewer {
    fn command(&self, lab: &str, vm: &str) -> Command {
        let display = crate::paths::machine_runtime_dir(lab, crate::paths::MachineKind::Vm, vm)
            .join("display.sock");
        let mut command = Command::new(&self.binary);
        command
            .arg("--display")
            .arg(display)
            .arg("--lab-socket")
            .arg(crate::paths::lab_socket(lab))
            .arg("--machine")
            .arg(vm);
        if let Some(chord) = &self.release_chord {
            command.arg("--release-chord").arg(chord);
        }
        command
    }

    /// The viewer waits for its socket and reconnects itself after a reboot.
    pub fn launch(&self, lab: &str, vm: &str) -> Result<()> {
        self.command(lab, vm)
            .spawn()
            .with_context(|| format!("launching native viewer `{}`", self.binary.display()))?;
        println!("launched {} for {lab}/{vm}", self.binary.display());
        Ok(())
    }
}

/// The display client selected for a machine's actual hypervisor.
#[derive(Debug, Clone)]
pub enum ConsoleViewer {
    Native(NativeViewer),
    Vnc(Option<Viewer>),
}

const NATIVE_VIEWER_MISSING: &str = "vmlab-hv-viewer not found; install it from \
    VMLabDev/vmlab-hypervisor beside vmlab-hv or on PATH, or set host.vmlab_hv_viewer; using VNC";

/// Select a machine's viewer. `tcp` explicitly requests the VNC bridge.
pub fn detect_for(hypervisor: HypervisorKind, tcp: bool) -> ConsoleViewer {
    let host = HostConfig::load_default().unwrap_or_default();
    let viewer = select_for(hypervisor, tcp, &host, &find_binary, || {
        detect_vnc(&host, &on_path)
    });
    if hypervisor == HypervisorKind::Vmlab && !tcp && matches!(viewer, ConsoleViewer::Vnc(_)) {
        eprintln!("{NATIVE_VIEWER_MISSING}");
    }
    viewer
}

fn select_for(
    hypervisor: HypervisorKind,
    tcp: bool,
    host: &HostConfig,
    find: &impl Fn(&Path) -> Option<PathBuf>,
    vnc: impl FnOnce() -> Option<Viewer>,
) -> ConsoleViewer {
    if hypervisor == HypervisorKind::Vmlab && !tcp {
        let binary = match &host.vmlab_hv_viewer {
            Some(binary) => find(binary),
            None => host
                .vmlab_hv
                .as_deref()
                .and_then(find)
                .and_then(|hv| find(&hv.with_file_name("vmlab-hv-viewer")))
                .or_else(|| find(Path::new("vmlab-hv-viewer"))),
        };
        if let Some(binary) = binary {
            return ConsoleViewer::Native(NativeViewer {
                binary,
                release_chord: host.viewer_release_chord.clone(),
            });
        }
    }
    ConsoleViewer::Vnc(vnc())
}

/// Resolve a bare binary on PATH, or check a configured path directly.
fn find_binary(binary: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &Path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if binary.components().count() > 1 || binary.is_absolute() {
        return executable(binary).then(|| binary.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(binary))
            .find(|path| executable(path))
    })
}

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

/// Whether `bin` is an executable on `PATH`.
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Resolve a viewer: host config first, then known clients on `PATH`.
/// `remote-viewer` is preferred because it dials the unix socket directly.
pub fn detect() -> Option<Viewer> {
    let host = HostConfig::load_default().unwrap_or_default();
    detect_vnc(&host, &on_path)
}

fn detect_vnc(host: &HostConfig, on_path: &impl Fn(&str) -> bool) -> Option<Viewer> {
    if let Some(cmd) = &host.viewer {
        return Some(Viewer {
            cmd: cmd.clone(),
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

/// Open a viewer for a lab VM, using its reported hypervisor for `gui = true`.
/// The native viewer waits for its display socket. A VNC unix-socket viewer
/// is launched directly; a TCP-only viewer runs in a detached bridge so it
/// survives `up` exiting. Prints how to attach when no viewer is found.
pub fn open_for(lab: &str, vm: &str, hypervisor: HypervisorKind) -> Result<()> {
    let viewer = match detect_for(hypervisor, false) {
        ConsoleViewer::Native(viewer) => return viewer.launch(lab, vm),
        ConsoleViewer::Vnc(viewer) => viewer,
    };
    let sock =
        crate::paths::machine_runtime_dir(lab, crate::paths::MachineKind::Vm, vm).join("vnc.sock");
    if !sock.exists() {
        return Ok(());
    }
    match viewer {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn available<'a>(binaries: &'a [&'a str]) -> impl Fn(&Path) -> Option<PathBuf> + 'a {
        |path| {
            binaries
                .iter()
                .any(|bin| path == Path::new(bin))
                .then(|| path.to_path_buf())
        }
    }

    fn select_available(
        hypervisor: HypervisorKind,
        tcp: bool,
        host: &HostConfig,
        find: &impl Fn(&Path) -> Option<PathBuf>,
    ) -> ConsoleViewer {
        select_for(hypervisor, tcp, host, find, || {
            detect_vnc(host, &|bin| find(Path::new(bin)).is_some())
        })
    }

    #[test]
    fn viewer_selection_follows_the_reported_hypervisor_and_tcp_override() {
        let host = HostConfig::default();
        let find = available(&["vmlab-hv-viewer", "remote-viewer"]);
        assert!(matches!(
            select_available(HypervisorKind::Vmlab, false, &host, &find),
            ConsoleViewer::Native(_)
        ));
        for (hypervisor, tcp) in [(HypervisorKind::Qemu, false), (HypervisorKind::Vmlab, true)] {
            let ConsoleViewer::Vnc(Some(viewer)) = select_available(hypervisor, tcp, &host, &find)
            else {
                panic!("expected VNC");
            };
            assert_eq!(viewer.cmd, "remote-viewer vnc://{}");
            assert_eq!(viewer.transport, Transport::Tcp);
        }
    }

    #[test]
    fn configured_native_viewer_wins_then_sibling_then_path() {
        let mut host = HostConfig {
            vmlab_hv: Some("/opt/hv/vmlab-hv".into()),
            vmlab_hv_viewer: Some("/custom/viewer".into()),
            ..Default::default()
        };
        let find = available(&[
            "/custom/viewer",
            "/opt/hv/vmlab-hv",
            "/opt/hv/vmlab-hv-viewer",
            "vmlab-hv-viewer",
        ]);
        let selected = |host: &HostConfig| {
            let ConsoleViewer::Native(viewer) =
                select_available(HypervisorKind::Vmlab, false, host, &find)
            else {
                panic!("expected native viewer");
            };
            viewer.binary
        };
        assert_eq!(selected(&host), Path::new("/custom/viewer"));
        host.vmlab_hv_viewer = None;
        assert_eq!(selected(&host), Path::new("/opt/hv/vmlab-hv-viewer"));
        host.vmlab_hv = None;
        assert_eq!(selected(&host), Path::new("vmlab-hv-viewer"));
    }

    #[test]
    fn missing_native_viewer_falls_back_to_configured_or_detected_vnc_or_bridge() {
        let mut host = HostConfig {
            vmlab_hv_viewer: Some("/missing/viewer".into()),
            viewer: Some("custom-vnc {}".into()),
            ..Default::default()
        };
        // An explicit override is authoritative even if a native viewer is on PATH.
        let find = available(&["vmlab-hv-viewer", "gvncviewer"]);
        let ConsoleViewer::Vnc(Some(viewer)) =
            select_available(HypervisorKind::Vmlab, false, &host, &find)
        else {
            panic!("expected configured VNC fallback");
        };
        assert_eq!(viewer.cmd, "custom-vnc {}");
        assert_eq!(viewer.transport, Transport::Unix);
        host.viewer = None;
        let ConsoleViewer::Vnc(Some(viewer)) =
            select_available(HypervisorKind::Vmlab, false, &host, &find)
        else {
            panic!("expected detected VNC fallback");
        };
        assert_eq!(viewer.cmd, "gvncviewer {}");
        host.vmlab_hv_viewer = None;
        assert!(matches!(
            select_available(HypervisorKind::Vmlab, false, &host, &available(&[])),
            ConsoleViewer::Vnc(None)
        ));
    }

    #[test]
    fn native_arguments_preserve_paths_and_leave_shm_detection_to_the_viewer() {
        let mut viewer = NativeViewer {
            binary: "/path with spaces/viewer".into(),
            release_chord: None,
        };
        let expected = [
            "--display".into(),
            crate::paths::machine_runtime_dir("lab", crate::paths::MachineKind::Vm, "vm")
                .join("display.sock")
                .into_os_string(),
            "--lab-socket".into(),
            crate::paths::lab_socket("lab").into_os_string(),
            "--machine".into(),
            "vm".into(),
        ];
        let command = viewer.command("lab", "vm");
        assert_eq!(
            command.get_program(),
            OsStr::new("/path with spaces/viewer")
        );
        assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
        viewer.release_chord = Some("ctrl_r+alt_r".into());
        let command = viewer.command("lab", "vm");
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(&args[..6], expected);
        assert_eq!(&args[6..], ["--release-chord", "ctrl_r+alt_r"]);
    }

    #[test]
    fn native_command_runs_a_fake_executable_without_a_display_socket() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("fake viewer");
        std::fs::write(&binary, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(find_binary(&binary).is_none());
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(find_binary(&binary), Some(binary.clone()));
        let viewer = NativeViewer {
            binary: binary.clone(),
            release_chord: Some("ctrl_r+alt_r".into()),
        };
        assert!(
            viewer
                .command("fake-lab", "fake-vm")
                .status()
                .unwrap()
                .success()
        );
        let args = std::fs::read_to_string(binary.with_file_name("fake viewer.args")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            viewer
                .command("fake-lab", "fake-vm")
                .get_args()
                .map(|a| a.to_str().unwrap())
                .collect::<Vec<_>>()
        );
    }
}
