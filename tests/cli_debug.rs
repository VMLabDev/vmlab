//! The actual CLI over a typed lab-socket relay to the real hypervisor.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vmlab::proto::server::{Handler, Server, Streamer};
use vmlab::proto::{CommandError, LabRequest};
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Ctrl, ErrorCode, Outcome};
use vmlab_hv_proto::debug::{DebugRequest, DebugResponse};

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.0.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.0.wait();
    }
}

fn cli(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vmlab"));
    command
        .current_dir(root)
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env_remove("VMLAB_INSTANCE");
    command
}

fn run(root: &Path, arguments: &[&str]) -> Output {
    cli(root).args(arguments).output().unwrap()
}

#[test]
fn invalid_debug_arguments_fail_before_contacting_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["debug", "lab/vm", "registers", "rax=xyz"],
        vec!["debug", "lab/vm", "registers", "unknown=1"],
        vec!["debug", "lab/vm", "write", "0x8000", "zz"],
        vec!["debug", "lab/vm", "gdb", "--listen", "0.0.0.0:1234"],
    ] {
        let output = run(root.path(), &arguments);
        assert_eq!(output.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("not running"));
        assert!(!root.path().join("runtime").exists());
    }
    let output = run(root.path(), &["debug", "--help"]);
    assert!(output.status.success());
    for operation in [
        "pause",
        "read",
        "write",
        "registers",
        "break",
        "search",
        "dump",
        "gdb",
    ] {
        assert!(String::from_utf8_lossy(&output.stdout).contains(operation));
    }
}

struct Relay(Arc<Mutex<Client>>);

#[async_trait::async_trait]
impl Handler<LabRequest> for Relay {
    async fn handle(&self, request: LabRequest, _: &Streamer) -> Result<Value, CommandError> {
        match request {
            LabRequest::Ping {} => Ok(json!(true)),
            LabRequest::Version {} => Ok(json!(vmlab::version::BUILD)),
            LabRequest::MachineDebug { machine, request } if machine == "vm" => {
                let client = self.0.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    client.lock().unwrap().call(Ctrl::Debug { request })
                })
                .await
                .map_err(|e| CommandError::internal(e.to_string()))?
                .map_err(|e| CommandError::failed(e.to_string()))?;
                match outcome {
                    Outcome::Debug { response } => Ok(json!(response)),
                    Outcome::Error { code, message } => Err(CommandError::new(
                        match code {
                            ErrorCode::InvalidArgument => vmlab::proto::ErrorCode::InvalidArgument,
                            ErrorCode::InvalidState => vmlab::proto::ErrorCode::Conflict,
                            _ => vmlab::proto::ErrorCode::Failed,
                        },
                        message,
                    )),
                    _ => Err(CommandError::internal("unexpected hypervisor reply")),
                }
            }
            _ => Err(CommandError::unsupported(
                "test relay only serves debug commands",
            )),
        }
    }
}

/// Uses the same four host-wide flock slots as the hypervisor's S1 harness.
fn vm_permit() -> File {
    let directory = PathBuf::from(format!(
        "/tmp/vmlab-hv-vm-slots-{}",
        nix::unistd::Uid::effective()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        for slot in 0..4 {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.join(format!("{slot}.lock")))
                .unwrap();
            if file.try_lock().is_ok() {
                return file;
            }
        }
        assert!(Instant::now() < deadline, "host VM slots remain occupied");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn checked(root: &Path, arguments: &[&str]) -> String {
    let output = run(root, arguments);
    assert!(
        output.status.success(),
        "{arguments:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn wait_child(process: &mut Process) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            return;
        }
        assert!(Instant::now() < deadline, "debug process did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn start_bridge(root: &Path, target: &str) -> (Process, String, std::thread::JoinHandle<()>) {
    let mut bridge = Process(
        cli(root)
            .args(["debug", target, "gdb", "--listen", "127.0.0.1:0"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let stderr = bridge.0.stderr.take().unwrap();
    let (ready, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap();
            if let Some(address) = line.strip_prefix("debug feature listening on ") {
                ready.send(address.to_string()).unwrap();
            }
        }
    });
    let address = receiver.recv_timeout(Duration::from_secs(15)).unwrap();
    (bridge, address, reader)
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires usable /dev/kvm")]
fn debug_feature_cli_and_gdb_roundtrip_against_real_hypervisor() {
    let Some(binary) = std::env::var_os("VMLAB_HV_DEBUG_TEST_BINARY") else {
        eprintln!(
            "SKIP real-hypervisor CLI debug check: set VMLAB_HV_DEBUG_TEST_BINARY to the built vmlab-hv binary"
        );
        return;
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP real-hypervisor CLI debug check: /dev/kvm is absent");
        return;
    }
    assert!(
        Path::new(&binary).is_file(),
        "VMLAB_HV_DEBUG_TEST_BINARY must name an existing binary"
    );
    let _permit = vm_permit();
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let mut rom = vec![0xf4; 0x10000];
    rom[0xfff0..0xfff5].copy_from_slice(&[0xea, 0, 0, 0, 0xf0]);
    let firmware = root.join("firmware.bin");
    std::fs::write(&firmware, rom).unwrap();
    let machine = root.join("machine.json");
    let control = root.join("hv.sock");
    std::fs::write(
        &machine,
        serde_json::to_vec(&json!({
            "name":"debug-cli", "machine":"q35", "arch":"x86_64", "accel":"kvm",
            "vcpus":1, "memory_mib":16,
            "boot":{"kind":"firmware", "firmware":"seabios", "code":firmware},
            "disks":[], "nics":[], "ports":[], "shares":[], "serial_log":root.join("serial.log")
        }))
        .unwrap(),
    )
    .unwrap();
    let mut guest = Process(
        Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "-q",
                "-p",
                "MemoryMax=24G",
                "-p",
                "MemorySwapMax=0",
                "--",
            ])
            .arg(binary)
            .arg("--machine")
            .arg(&machine)
            .arg("--control")
            .arg(&control)
            .arg("--paused")
            .env("VMLAB_HV_LOG", "off")
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(root.join("hv.log")).unwrap()))
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let client = loop {
        if let Ok(client) = Client::connect(&control) {
            break client;
        }
        assert!(
            guest.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(root.join("hv.log")).unwrap()
        );
        assert!(Instant::now() < deadline, "hypervisor startup timeout");
        std::thread::sleep(Duration::from_millis(10));
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let client = Arc::new(Mutex::new(client));
    let socket = vmlab::paths::lab_runtime_dir_in(&root.join("runtime/vmlab"), "debug-cli")
        .join("control.sock");
    let server = runtime
        .block_on(Server::bind(&socket, Arc::new(Relay(client.clone()))))
        .unwrap();
    let target = "debug-cli/vm";
    checked(root, &["debug", target, "pause"]);
    checked(
        root,
        &[
            "debug",
            target,
            "registers",
            "cs=0",
            "ds=0",
            "ss=0",
            "rip=0x8000",
            "rax=0x4567",
        ],
    );
    checked(
        root,
        &["debug", target, "write", "0x8000", "b83412a30090ebfe"],
    );
    checked(root, &["debug", target, "write", "0x9000", "abcd"]);
    let step = checked(root, &["debug", target, "step", "--json"]);
    let step: Value = serde_json::from_str(&step).unwrap();
    assert_eq!(step["status"]["state"], "paused");
    assert_eq!(step["status"]["stepping"], false);
    assert_eq!(step["status"]["stop"]["reason"], "step");
    checked(
        root,
        &["debug", target, "registers", "rip=0x8000", "rax=0x4567"],
    );
    let read = checked(root, &["debug", target, "read", "0x9000", "2", "--json"]);
    assert_eq!(
        serde_json::from_str::<Value>(&read).unwrap()["data"],
        json!([0xab, 0xcd])
    );
    let search = checked(
        root,
        &["debug", target, "search", "0x9000", "16", "abcd", "--json"],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&search).unwrap()["addresses"],
        json!([0x9000])
    );
    let dump = root.join("memory.bin");
    checked(
        root,
        &[
            "debug",
            target,
            "dump",
            "0x9000",
            "65540",
            dump.to_str().unwrap(),
        ],
    );
    assert_eq!(&std::fs::read(&dump).unwrap()[..2], &[0xab, 0xcd]);
    assert!(
        !run(
            root,
            &[
                "debug",
                target,
                "dump",
                "0x9000",
                "2",
                dump.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert_eq!(std::fs::metadata(&dump).unwrap().len(), 65540);
    checked(
        root,
        &[
            "debug", target, "break", "0x9000", "--kind", "write", "--length", "2",
        ],
    );
    checked(root, &["debug", target, "continue"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let Outcome::Debug {
            response: DebugResponse::Status { status },
        } = client
            .lock()
            .unwrap()
            .call(Ctrl::Debug {
                request: DebugRequest::Status,
            })
            .unwrap()
        else {
            panic!()
        };
        if status.stop.is_some_and(|stop| {
            matches!(
                stop.reason,
                vmlab_hv_proto::debug::StopReason::Breakpoint { .. }
            )
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "CLI watchpoint did not stop the guest"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let stopped = checked(root, &["debug", target, "status", "--json"]);
    assert_eq!(
        serde_json::from_str::<Value>(&stopped).unwrap()["status"]["state"],
        "paused"
    );
    checked(
        root,
        &[
            "debug", target, "unbreak", "0x9000", "--kind", "write", "--length", "2",
        ],
    );
    checked(root, &["debug", target, "registers", "rip=0x8000"]);
    if Command::new("gdb")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        let (mut bridge, address, reader) = start_bridge(root, target);
        let output = Command::new("timeout")
            .args([
                "30",
                "gdb",
                "-nx",
                "-nh",
                "--batch",
                "-ex",
                &format!("target remote {address}"),
                "-ex",
                "set $rax = 0x9876",
                "-ex",
                "set {unsigned short}0x9000 = 0xbeef",
                "-ex",
                "p/x $rax",
                "-ex",
                "x/1hx 0x9000",
                "-ex",
                "detach",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("0x9876"));
        assert!(String::from_utf8_lossy(&output.stdout).contains("0xbeef"));
        wait_child(&mut bridge);
        reader.join().unwrap();
    } else {
        eprintln!("SKIP GDB client portion of CLI debug check: gdb is unavailable");
    }
    let (mut bridge, _, reader) = start_bridge(root, target);
    let before = checked(root, &["debug", target, "status", "--json"]);
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(bridge.0.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    wait_child(&mut bridge);
    reader.join().unwrap();
    let after = checked(root, &["debug", target, "status", "--json"]);
    assert_eq!(
        serde_json::from_str::<Value>(&before).unwrap()["status"]["state"],
        serde_json::from_str::<Value>(&after).unwrap()["status"]["state"]
    );
    let (mut bridge, address, reader) = start_bridge(root, target);
    let connection = std::net::TcpStream::connect(address).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = checked(root, &["debug", target, "status", "--json"]);
        if serde_json::from_str::<Value>(&value).unwrap()["status"]["state"] == "paused" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bridge did not pause on attachment"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(bridge.0.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    wait_child(&mut bridge);
    reader.join().unwrap();
    drop(connection);
    let stopped = checked(root, &["debug", target, "status", "--json"]);
    let stopped: Value = serde_json::from_str(&stopped).unwrap();
    assert_eq!(stopped["status"]["state"], "paused");
    assert_eq!(stopped["status"]["breakpoints"], json!([]));
    client.lock().unwrap().call(Ctrl::Quit).unwrap();
    wait_child(&mut guest);
    drop(server);
}
