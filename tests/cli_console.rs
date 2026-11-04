//! Console selection through the actual CLI, with a fake daemon and viewers.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use vmlab::proto::server::{Handler, Server, Streamer};
use vmlab::proto::{CommandError, LabRequest};
use vmlab::status::LabStatus;

struct Daemon(LabStatus);

#[async_trait::async_trait]
impl Handler<LabRequest> for Daemon {
    async fn handle(&self, request: LabRequest, _: &Streamer) -> Result<Value, CommandError> {
        match request {
            LabRequest::Ping {} => Ok(json!(true)),
            LabRequest::Version {} => Ok(json!(vmlab::version::BUILD)),
            LabRequest::Status {} => Ok(json!(self.0)),
            LabRequest::Up { .. } => Ok(json!(null)),
            _ => Err(CommandError::unsupported("unexpected console test request")),
        }
    }
}

struct Fixture {
    root: tempfile::TempDir,
    runtime: PathBuf,
    _server: Server<LabRequest>,
}

impl Fixture {
    async fn new(hypervisor: &str, gui: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("vmlab.wcl"),
            format!(
                r#"import <vmlab.wcl>
lab "console-test" {{
  gui = {gui}
  vm "vm" {{ template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 1GiB }}
}}
"#
            ),
        )
        .unwrap();
        let lab = vmlab::config::load_lab_root(root.path()).unwrap();
        let status: LabStatus = serde_json::from_value(json!({
            "lab": "console-test", "segments": [], "provisioned": true, "pulls": [],
            "config_digest": vmlab::config::digest(&lab),
            "machines": [{
                "name": "vm", "kind": "vm", "hypervisor": hypervisor,
                "state": "running", "ready": true, "ip": null, "nics": [],
                "cached": true, "agent_diverged": false, "template": "scratch",
                "label": {"state": "running", "text": "running", "severity": "success"}
            }]
        }))
        .unwrap();
        let runtime = root.path().join("runtime/vmlab");
        let socket =
            vmlab::paths::lab_runtime_dir_in(&runtime, "console-test").join("control.sock");
        let server = Server::bind(&socket, Arc::new(Daemon(status)))
            .await
            .unwrap();
        Self {
            root,
            runtime,
            _server: server,
        }
    }

    fn machine_dir(&self) -> PathBuf {
        vmlab::paths::machine_runtime_dir_in(
            &self.runtime,
            "console-test",
            vmlab::paths::MachineKind::Vm,
            "vm",
        )
    }

    fn viewer(&self, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        std::fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn config(&self, fields: &str) {
        let dir = self.root.path().join("config/vmlab");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.wcl"),
            format!("import <vmlab-host.wcl>\nhost {{\n{fields}\n}}\n"),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_vmlab"));
        command
            .args(args)
            .current_dir(self.root.path())
            .env("XDG_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env_remove("VMLAB_INSTANCE")
            .kill_on_drop(true);
        command
    }

    async fn run(&self, args: &[&str]) -> Output {
        let output = tokio::time::timeout(Duration::from_secs(15), self.command(args).output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn recorded(&self, binary: &Path) -> Vec<String> {
        std::fs::read_to_string(binary.with_file_name(format!(
            "{}.args",
            binary.file_name().unwrap().to_str().unwrap()
        )))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
    }

    fn vnc_socket(&self) -> PathBuf {
        let dir = self.machine_dir();
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("vnc.sock")
    }
}

#[tokio::test]
async fn console_and_gui_launch_native_from_status_without_waiting_for_sockets() {
    for gui in [false, true] {
        let fixture = Fixture::new("vmlab", gui).await;
        let native = fixture.viewer("native viewer");
        fixture.config(&format!(
            "vmlab_hv_viewer = {}\nviewer_release_chord = \"ctrl_r+alt_r\"",
            json!(native)
        ));
        assert!(!fixture.machine_dir().join("display.sock").exists());
        assert!(!fixture.machine_dir().join("vnc.sock").exists());
        fixture
            .run(if gui {
                &["up"]
            } else {
                &["console", "console-test/vm"]
            })
            .await;
        assert_eq!(
            fixture.recorded(&native),
            [
                "--display".into(),
                fixture
                    .machine_dir()
                    .join("display.sock")
                    .display()
                    .to_string(),
                "--lab-socket".into(),
                vmlab::paths::lab_runtime_dir_in(&fixture.runtime, "console-test")
                    .join("control.sock")
                    .display()
                    .to_string(),
                "--machine".into(),
                "vm".into(),
                "--release-chord".into(),
                "ctrl_r+alt_r".into(),
            ]
        );
        let output = fixture.run(&["status", "--json"]).await;
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["machines"][0]["hypervisor"], "vmlab");
    }
}

#[tokio::test]
async fn qemu_keeps_vnc_and_missing_native_falls_back_with_one_notice() {
    for hypervisor in ["qemu", "vmlab"] {
        let fixture = Fixture::new(hypervisor, false).await;
        let native = fixture.viewer("native");
        let vnc = fixture.viewer("vnc");
        let native_path = if hypervisor == "qemu" {
            native.clone()
        } else {
            fixture.root.path().join("missing")
        };
        fixture.config(&format!(
            "vmlab_hv_viewer = {}\nviewer = {}",
            json!(native_path),
            json!(format!("{} {{}}", vnc.display()))
        ));
        let socket = fixture.vnc_socket();
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let output = fixture.run(&["console", "console-test/vm"]).await;
        assert_eq!(fixture.recorded(&vnc), [socket.display().to_string()]);
        assert!(!native.with_file_name("native.args").exists());
        let stderr = String::from_utf8(output.stderr).unwrap();
        if hypervisor == "vmlab" {
            assert_eq!(stderr.lines().count(), 1, "{stderr}");
            assert!(
                stderr.contains("install it from the vmlab-hv release bundle"),
                "{stderr}"
            );
            assert!(
                stderr.contains("host.vmlab_hv_viewer; using VNC"),
                "{stderr}"
            );
        } else {
            assert!(stderr.is_empty(), "{stderr}");
        }
    }
}

#[tokio::test]
async fn tcp_override_bridges_vnc_on_both_hypervisors() {
    for hypervisor in ["qemu", "vmlab"] {
        let fixture = Fixture::new(hypervisor, false).await;
        let native = fixture.viewer("native");
        fixture.config(&format!("vmlab_hv_viewer = {}", json!(native)));
        let listener = tokio::net::UnixListener::bind(fixture.vnc_socket()).unwrap();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4];
            stream.read_exact(&mut bytes).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
        });
        let mut child = fixture
            .command(&["console", "--tcp", "console-test/vm"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let address = line.strip_prefix("VNC for console-test/vm on ").unwrap();
        let mut tcp = tokio::net::TcpStream::connect(address).await.unwrap();
        tcp.write_all(b"test").await.unwrap();
        let mut reply = [0; 4];
        tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply, b"test");
        echo.await.unwrap();
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id().unwrap() as i32),
            nix::sys::signal::Signal::SIGINT,
        )
        .unwrap();
        let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!native.with_file_name("native.args").exists());
    }
}
