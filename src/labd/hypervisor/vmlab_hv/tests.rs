//! The adapter end to end, with a shell script standing in for the
//! `vmlab-hv` binary and a fake control server built on the protocol crate's
//! own `Server` standing in for its control socket: what is spawned, with
//! what, and how its control socket is reached.

use std::os::fd::OwnedFd;
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

fn tap_spec(mac: u8, taps: Vec<i32>, vhost: Vec<i32>) -> crate::qemu::NicSpec {
    crate::qemu::NicSpec {
        mac: crate::config::model::MacAddr([0x52, 0x54, 0, 0, 0, mac]),
        mtu: Some(1500),
        backend: crate::qemu::NicBackend::TapQueues {
            tap_fds: taps,
            vhost_fds: vhost,
        },
    }
}

fn mapped_fd(child: i32) -> crate::qemu::process::ChildFd {
    let (fd, _) = std::os::unix::net::UnixDatagram::pair().unwrap();
    crate::qemu::process::ChildFd {
        parent: OwnedFd::from(fd),
        child,
    }
}

#[test]
fn tap_descriptor_mappings_are_explicit_and_consumed_exactly_once() {
    let nics = [tap_spec(1, vec![17, 11], vec![31, 29])];
    let mut mappings = Vec::new();
    let mut peers = Vec::new();
    for child in [11, 17, 29, 31] {
        let (fd, peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
        peer.send(&[child as u8]).unwrap();
        peers.push(peer);
        mappings.push(crate::qemu::process::ChildFd {
            parent: fd.into(),
            child,
        });
    }
    let mut attachments = net_attachments(&nics, mappings).unwrap();
    let attachment = attachments.pop().unwrap();
    assert_eq!(attachment.queue_pairs, 2);
    assert!(attachment.vhost);
    let mut markers = Vec::new();
    for fd in &attachment.fds {
        let mut marker = [0];
        assert_eq!(rustix::io::read(fd, &mut marker).unwrap(), 1);
        markers.push(marker[0]);
    }
    assert_eq!(markers, [17, 11, 31, 29]);
    assert!(net_attachments(&nics, vec![mapped_fd(17), mapped_fd(17)]).is_err());
    assert!(net_attachments(&nics, vec![mapped_fd(17)]).is_err());
    assert!(net_attachments(&[], vec![mapped_fd(11)]).is_err());
    assert!(net_attachments(&[tap_spec(1, vec![11, 11], vec![])], vec![mapped_fd(11)]).is_err());
    assert!(
        net_attachments(
            &[tap_spec(1, vec![11, 12], vec![13])],
            vec![mapped_fd(11), mapped_fd(12), mapped_fd(13)]
        )
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tap_descriptors_are_not_inherited_and_attach_precedes_resume() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path();
    let argv = run.join("argv");
    let bin = fake_binary(
        run,
        &format!(
            "test ! -e /proc/self/fd/117 || exit 3\necho started > {}\nexec sleep 30",
            argv.display()
        ),
    );
    let firmware = run.join("firmware/x86_64");
    std::fs::create_dir_all(&firmware).unwrap();
    std::fs::write(firmware.join("bios-vmlab-q35.bin"), b"").unwrap();
    std::fs::write(firmware.join("vgabios-stdvga.bin"), b"").unwrap();
    let cfg = crate::qemu::resolve::testing::vm(
        r#"vm "web" { template = "scratch" arch = "x86_64" profile = "linux-modern" firmware = "seabios" cpus = 1 memory = 512MiB disk = 1GiB }"#,
    );
    let resolved =
        crate::qemu::resolve_vm(&cfg, None, &crate::profiles::ProfileSet::shipped().unwrap())
            .unwrap();
    let launch = Launch::Vm {
        lab: "lab1".into(),
        resolved: Box::new(resolved),
        paths: Box::new(crate::qemu::VmPaths {
            nics: vec![tap_spec(1, vec![117], vec![])],
            primary_disk: run.join("disk.qcow2"),
            agent_sock: run.join("agent.sock"),
            qmp_sock: run.join("control.sock"),
            display_sock: run.join("display.sock"),
            vnc_sock: run.join("vnc.sock"),
            serial_log: Some(run.join("serial.log")),
            ..Default::default()
        }),
        accel: crate::qemu::Accel::Kvm,
        emulator: None,
    };
    let socket = run.join("control.sock");
    let marker = argv.clone();
    let (tx, seen) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..500 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let server = Server::with_fds("fake-hv", vec![Capability::NetFds], move |command, fds| {
            if matches!(command, Command::NetAttach { .. }) {
                assert_eq!(fds.len(), 1);
                let mut packet = [0; 4];
                assert_eq!(rustix::io::read(&fds[0], &mut packet).unwrap(), 4);
                assert_eq!(&packet, b"TAP0");
            } else {
                assert!(fds.is_empty());
            }
            tx.send(command).unwrap();
            Outcome::Ok { status: None }
        });
        let listener = UnixListener::bind(socket).unwrap();
        let _ = server.serve(listener);
    });
    let mut launch_spec = spec(run, launch);
    let (fd, peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
    peer.send(b"TAP0").unwrap();
    launch_spec.fds.push(crate::qemu::process::ChildFd {
        parent: fd.into(),
        child: 117,
    });
    let running = VmlabHv::for_vm(Some(bin), Some(run.join("firmware")))
        .start_emulator(launch_spec)
        .await
        .unwrap();
    assert!(matches!(
        seen.recv_timeout(Duration::from_secs(2)).unwrap(),
        Command::NetAttach {
            queue_pairs: 1,
            vhost: false,
            ..
        }
    ));
    running.control.resume().await.unwrap();
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(2)).unwrap(),
        Command::Resume
    );
    running.proc.kill().await;
    running
        .proc
        .wait_exit(Duration::from_secs(5))
        .await
        .unwrap();
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

/// A VM with an install CD-ROM launches with it as `cd0`, and its media
/// capability, where the handshake advertises it, empties that drive with
/// `eject_media`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vms_install_cdrom_is_ejected_through_the_media_capability() {
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
    let firmware = run.join("firmware/x86_64");
    std::fs::create_dir_all(&firmware).unwrap();
    std::fs::write(firmware.join("bios-vmlab-q35.bin"), b"").unwrap();
    std::fs::write(firmware.join("vgabios-stdvga.bin"), b"").unwrap();
    let cfg = crate::qemu::resolve::testing::vm(
        r#"vm "web" { template = "scratch" arch = "x86_64" profile = "linux-modern" firmware = "seabios" cpus = 1 memory = 512MiB disk = 1GiB }"#,
    );
    let resolved =
        crate::qemu::resolve_vm(&cfg, None, &crate::profiles::ProfileSet::shipped().unwrap())
            .unwrap();
    let install = run.join("install.iso");
    let launch = Launch::Vm {
        lab: "lab1".into(),
        resolved: Box::new(resolved),
        paths: Box::new(crate::qemu::VmPaths {
            primary_disk: run.join("disk.qcow2"),
            cdroms: vec![install.clone(), run.join("media.iso")],
            install_cdrom: true,
            agent_sock: run.join("agent.sock"),
            qmp_sock: run.join("control.sock"),
            display_sock: run.join("display.sock"),
            vnc_sock: run.join("vnc.sock"),
            serial_log: Some(run.join("serial.log")),
            ..Default::default()
        }),
        accel: crate::qemu::Accel::Kvm,
        emulator: None,
    };
    let seen = serve_once_spawned(
        argv.clone(),
        run.join("control.sock"),
        vec![Capability::Media],
    );
    let running = VmlabHv::for_vm(Some(bin), Some(run.join("firmware")))
        .start_emulator(spec(run, launch))
        .await
        .unwrap();

    let description: vmlab_hv_proto::MachineDescription =
        serde_json::from_slice(&std::fs::read(run.join("machine.json")).unwrap()).unwrap();
    let cd = description
        .disks
        .iter()
        .find(|d| d.id == "cd0")
        .expect("the install CD-ROM");
    assert!(cd.cdrom && cd.path.as_deref() == Some(install.as_path()), "{cd:?}");

    let media = running.control.media().expect("advertised");
    media.eject_install_cdrom().await.unwrap();
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(2)).unwrap(),
        Command::EjectMedia { id: "cd0".into() }
    );
    running.proc.kill().await;
    running
        .proc
        .wait_exit(Duration::from_secs(5))
        .await
        .unwrap();
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
