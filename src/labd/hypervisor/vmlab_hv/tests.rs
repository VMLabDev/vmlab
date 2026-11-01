//! The adapter end to end, with a shell script standing in for the
//! `vmlab-hv` binary and a fake control server built on the protocol crate's
//! own `Server` standing in for its control socket: what is spawned, with
//! what, and how its control socket is reached.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use vmlab_hv_proto::control::{Capability, Command, Outcome};
use vmlab_hv_proto::server::Server;

use super::*;
use crate::labd::hypervisor::GuestChannels;
use crate::qemu::container::ContainerVmPaths;

/// A stand-in `vmlab-hv`: `body` runs as `sh` with the real argv.
fn fake_binary(dir: &Path, body: &str) -> PathBuf {
    let bin = dir.join("vmlab-hv");
    std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn container_launch(run: &Path) -> Launch {
    let resolved = crate::qemu::resolve::testing::resolved_container(
        r#"container "web" { image = "nginx" cpus = 1 memory = 256MiB }"#,
        "x86_64",
    );
    Launch::Container {
        lab: "lab1".into(),
        resolved: Box::new(resolved),
        paths: Box::new(ContainerVmPaths {
            kernel: "/assets/vmlinuz".into(),
            initrd: "/assets/initramfs.img".into(),
            rootfs_image: "/cache/rootfs.sqfs".into(),
            scratch_disk: run.join("scratch.qcow2"),
            nics: vec![],
            qmp_sock: run.join("qmp.sock"),
            ctl_sock: run.join("ctl.sock"),
            agent_sock: run.join("agent.sock"),
            serial_log: run.join("console.log"),
            volumes: vec![],
        }),
    }
}

fn spec(run: &Path, machine: Launch) -> LaunchSpec {
    LaunchSpec {
        label: "vmlab-hv:web".into(),
        machine,
        env: Default::default(),
        log: run.join("vmlab-hv.log"),
        control_sock: run.join("control.sock"),
        fds: Vec::new(),
        channels: GuestChannels {
            agent: run.join("agent.sock"),
            ctl: Some(run.join("ctl.sock")),
        },
    }
}

/// The real binary binds its control socket itself, after it starts; here
/// the test does, once the stand-in has written its argv.
fn serve_once_spawned(
    argv: PathBuf,
    socket: PathBuf,
    capabilities: Vec<Capability>,
) -> mpsc::Receiver<Command> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..500 {
            if argv.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let server = Server::new("fake-hv", capabilities, move |c: Command| {
            let _ = tx.send(c);
            Outcome::Ok { status: None }
        });
        let listener = UnixListener::bind(&socket).expect("the stale socket was removed");
        let _ = server.serve(listener);
    });
    rx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_container_is_launched_from_its_description_and_controlled() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path();
    let argv = run.join("argv");
    let bin = fake_binary(
        run,
        &format!(
            "echo \"$@\" > {}.tmp && mv {0}.tmp {0}\nexec sleep 30",
            argv.display()
        ),
    );
    // A previous run's leftovers: vmlab-hv refuses a path that exists.
    for stale in ["control.sock", "agent.sock", "ctl.sock"] {
        std::fs::write(run.join(stale), b"").unwrap();
    }
    let seen = serve_once_spawned(
        argv.clone(),
        run.join("control.sock"),
        vec![Capability::Snapshots],
    );

    let hv = VmlabHv::new(Some(bin.clone()));
    assert_eq!(hv.emulator_binary("x86_64"), bin.display().to_string());
    let running = hv
        .start_emulator(spec(run, container_launch(run)))
        .await
        .expect("starts");

    let args = std::fs::read_to_string(&argv).unwrap();
    assert_eq!(
        args.trim(),
        format!(
            "--machine {} --control {} --paused",
            run.join("machine.json").display(),
            run.join("control.sock").display()
        )
    );
    assert!(!run.join("agent.sock").exists() && !run.join("ctl.sock").exists());
    let description: vmlab_hv_proto::MachineDescription =
        serde_json::from_slice(&std::fs::read(run.join("machine.json")).unwrap()).unwrap();
    assert_eq!(description.name, "vmlab:lab1/web");
    // The launch header leads the log, as QEMU's does.
    let log = std::fs::read_to_string(run.join("vmlab-hv.log")).unwrap();
    assert!(
        log.starts_with(&format!("vmlab: argv: {}", bin.display())),
        "{log}"
    );

    assert!(running.proc.is_running());
    running.control.resume().await.unwrap();
    let snaps = running.control.snapshots().expect("advertised");
    snaps.save("s1", "scratch", &["scratch"]).await.unwrap();
    assert_eq!(
        seen.try_iter().collect::<Vec<_>>(),
        vec![Command::Resume, Command::SnapshotSave { name: "s1".into() }]
    );

    running.proc.kill().await;
    let status = running
        .proc
        .wait_exit(Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(
        crate::labd::hypervisor::classify_exit(false, running.control.guest_shutdown(), &status),
        crate::labd::vm::StopReason::Crashed
    );
}

/// What the binary says on its way out is the error, not a timeout.
#[tokio::test]
async fn a_hypervisor_that_exits_during_startup_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path();
    let bin = fake_binary(
        run,
        "echo 'vmlab-hv: building machine `web`: /dev/kvm: permission denied' >&2; exit 2",
    );
    let err = VmlabHv::new(Some(bin))
        .start_emulator(spec(run, container_launch(run)))
        .await
        .err()
        .expect("fails");
    let err = format!("{err:#}");
    assert!(err.contains("vmlab-hv exited during startup"), "{err}");
    assert!(err.contains("/dev/kvm: permission denied"), "{err}");
}

/// A VM the delivered phases cannot run — the i440fx legacy PC, phase 4 —
/// is refused before anything spawns.
#[tokio::test]
async fn a_legacy_pc_vm_is_refused_before_anything_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path();
    let bin = fake_binary(run, &format!("touch {}/spawned", run.display()));
    let launch = Launch::Vm {
        lab: "lab1".into(),
        resolved: Box::new(crate::qemu::resolve::testing::resolved_vm(
            "windows-legacy",
            "x86_64",
        )),
        paths: Box::default(),
        accel: crate::qemu::Accel::Kvm,
        emulator: None,
    };
    let err = VmlabHv::for_vm(Some(bin), None)
        .start_emulator(spec(run, launch))
        .await
        .err()
        .expect("refused");
    assert!(err.to_string().contains("q35"), "{err}");
    assert!(!run.join("spawned").exists());
    assert!(!run.join("machine.json").exists());
}

/// A VM's adapter serves its shares in process (phase 2e): read-only and
/// snapshot-carrying, whatever the host's virtiofsd is — while a
/// container's volumes keep the host's virtiofsd.
#[test]
fn a_vm_serves_its_shares_in_process() {
    let vm = VmlabHv::for_vm(None, None);
    assert!(vm.virtiofs_in_process());
    let device = vm.virtiofsd().expect("the in-process device");
    assert!(device.readonly && device.migration, "{device:?}");
    assert_eq!(device.snapshot_refusal("v"), None);
    let container = VmlabHv::new(None);
    assert!(!container.virtiofs_in_process());
    assert_eq!(
        container.virtiofsd().is_some(),
        crate::qemu::virtiofsd::found().is_some()
    );
}

/// The orphan sweep finds a vmlab-hv by the lab's runtime directory in its
/// argv, which is where the adapter puts the machine file and the socket.
#[test]
fn an_orphaned_vmlab_hv_matches_its_labs_markers() {
    let run =
        crate::paths::machine_runtime_dir("lab1", crate::paths::MachineKind::Container, "web");
    let argv = args(
        &machine_file(&run.join("control.sock")),
        &run.join("control.sock"),
    );
    let mut cmdline = b"vmlab-hv".to_vec();
    for a in &argv {
        cmdline.push(0);
        cmdline.extend_from_slice(a.as_bytes());
    }
    assert!(crate::qemu::process::cmdline_belongs_to(&cmdline, "lab1"));
    assert!(!crate::qemu::process::cmdline_belongs_to(&cmdline, "lab"));
}

fn macos_launch(run: &Path, pin: Option<u64>) -> Launch {
    Launch::Vm {
        lab: "lab1".into(),
        resolved: Box::new(crate::qemu::resolve::testing::resolved_vm(
            "macos", "x86_64",
        )),
        paths: Box::new(crate::qemu::VmPaths {
            primary_disk: run.join("disk0.qcow2"),
            firmware_code: Some(run.join("CODE.fd")),
            ovmf_vars: Some(run.join("VARS.fd")),
            serial_log: Some(run.join("serial.log")),
            tsc_hz: pin,
            apple_smc_osk: Some(
                crate::config::host::AppleSmcOsk::parse("FAKE".repeat(16)).unwrap(),
            ),
            ..Default::default()
        }),
        accel: crate::qemu::Accel::Kvm,
        emulator: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn macos_launch_is_private_and_pins_the_reported_tsc_before_resume() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path();
    let argv = run.join("argv");
    let bin = fake_binary(
        run,
        &format!(
            "cat \"$2\" >&2\necho >&2\necho 'vmlab-hv: tsc_khz=4291934' >&2\necho \"$@\" > {}\nexec sleep 30",
            argv.display()
        ),
    );
    let seen = serve_once_spawned(argv.clone(), run.join("control.sock"), vec![]);
    let key = "FAKE\"\\ ".repeat(8) + "FAKEFAKE";
    let mut launch = macos_launch(run, None);
    let Launch::Vm { paths, .. } = &mut launch else {
        unreachable!()
    };
    paths.apple_smc_osk = Some(crate::config::host::AppleSmcOsk::parse(key.clone()).unwrap());
    let running = VmlabHv::for_vm(Some(bin), None)
        .start_emulator(spec(run, launch))
        .await
        .unwrap();
    assert!(seen.try_iter().next().is_none(), "guest is still paused");
    assert_eq!(
        crate::qemu::tsc::read_pin(&run.join("tsc-hz")).unwrap(),
        Some(4_291_934_000)
    );
    let argv = std::fs::read_to_string(argv).unwrap();
    assert!(argv.starts_with("--machine /proc/"));
    assert!(!argv.contains(&"FAKE".repeat(16)));
    for path in ["machine.json", "vmlab-hv.log"] {
        let dump = std::fs::read_to_string(run.join(path)).unwrap();
        assert!(!dump.contains(&key), "{path} leaked the OSK");
        let escaped = serde_json::to_string(&key).unwrap();
        assert!(
            !dump.contains(&escaped[1..escaped.len() - 1]),
            "{path} leaked the escaped OSK"
        );
        assert!(dump.to_lowercase().contains("redacted"));
    }
    running.proc.kill().await;
    running
        .proc
        .wait_exit(Duration::from_secs(5))
        .await
        .unwrap();
}

#[tokio::test]
async fn macos_startup_errors_cannot_echo_the_private_description() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path();
    let bin = fake_binary(run, "cat \"$2\" >&2; exit 2");
    let error = VmlabHv::for_vm(Some(bin), None)
        .start_emulator(spec(run, macos_launch(run, None)))
        .await
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains(&"FAKE".repeat(16)));
    let log = std::fs::read_to_string(run.join("vmlab-hv.log")).unwrap();
    assert!(!log.contains(&"FAKE".repeat(16)));
}

#[test]
fn tsc_reports_are_positive_and_belong_to_this_start() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("hv.log");
    std::fs::write(&log, "vmlab-hv: tsc_khz=1234\n").unwrap();
    let from = crate::qemu::process::log_offset(&log);
    assert!(reported_tsc_hz(&log, from).is_err());
    for value in ["0", "bad", "4294967296"] {
        std::fs::write(&log, format!("vmlab-hv: tsc_khz={value}\n")).unwrap();
        assert!(reported_tsc_hz(&log, 0).is_err());
    }
}
