//! Build the QEMU argv for one container micro-VM (PRD §18). Pure function
//! of the container's resolved shape + runtime paths, mirroring
//! [`super::cmdline::build_args`] for full VMs, so it is exhaustively
//! unit-testable.
//!
//! The guest side of this contract is FROZEN in `guest/` (see
//! `guest/cinit/src/cmdline.rs` and `guest/build-asset.sh`): direct kernel
//! boot with `vmlab.root=`/`vmlab.scratch=` naming the first two virtio-blk
//! devices, and two virtio-serial ports — `vmlab.ctl.0` (the ndjson ctl
//! channel, [`crate::labd::container_ctl`], which also delivers the spec)
//! and `vmlab.agent.0` (vmlab-agent: terminals/exec/files — the same
//! channel full VMs carry; guest/agent-proto). Deliberately NO 9p device — it would add a
//! migration blocker and break online snapshots. Volumes attach as
//! vhost-user-fs devices instead (one virtiofsd per volume, spawned by
//! labd with `--migration-mode` so its state rides the snapshot), which
//! also forces the memory backend to shared memfd; CIFS mounts remain the
//! no-virtiofsd fallback (PRD §18).

use std::path::PathBuf;

use anyhow::{Result, bail};

use super::cmdline::{Accel, pick_accel};
use crate::config::model::MacAddr;

/// Per-container runtime paths and attachments supplied by the lab daemon.
#[derive(Debug, Clone, Default)]
// Consumed by labd::container, which is not wired into the daemon yet.
pub struct ContainerVmPaths {
    /// Guest boot asset kernel (`vmlinuz`), from [`crate::guest_asset`].
    pub kernel: PathBuf,
    /// Guest boot asset initramfs (`initramfs.img`).
    pub initrd: PathBuf,
    /// Flattened image rootfs (`rootfs.sqfs`, raw squashfs, read-only) —
    /// the first virtio-blk device (`/dev/vda` in the guest).
    pub rootfs_image: PathBuf,
    /// Writable scratch qcow2 for the overlay upper layer — the second
    /// virtio-blk device (`/dev/vdb`).
    pub scratch_disk: PathBuf,
    /// One unix socket per NIC, in declaration order: (MAC, socket, segment
    /// MTU) — byte-for-byte the full-VM netdev shape so labd's switch ports
    /// attach unchanged.
    pub nics: Vec<(MacAddr, PathBuf, Option<u16>)>,
    pub qmp_sock: PathBuf,
    /// The `vmlab.ctl.0` channel socket (QEMU listens; the host connects).
    pub ctl_sock: PathBuf,
    /// The `vmlab.agent.0` channel socket (vmlab-agent: terminals, exec,
    /// file transfer — guest/agent-proto; QEMU listens, the host connects).
    pub agent_sock: PathBuf,
    /// Serial console log — the container's stdout/stderr land here.
    pub serial_log: PathBuf,
    /// One vhost-user-fs device per virtiofs volume, in declaration order:
    /// (mount tag, virtiofsd socket). virtiofsd must be listening before
    /// QEMU spawns. Non-empty switches the VM's RAM to a shared
    /// memory-backend-memfd (vhost-user back-ends map guest memory).
    pub volumes: Vec<(String, PathBuf)>,
}

/// Build the full argv (excluding argv[0], [`super::emulator_binary`]),
/// picking the accelerator via [`pick_accel`].
pub fn build_container_args(
    lab: &str,
    name: &str,
    arch: &str,
    cpus: u32,
    memory_bytes: u64,
    paths: &ContainerVmPaths,
) -> Result<Vec<String>> {
    build_container_args_with_accel(lab, name, arch, cpus, memory_bytes, paths, pick_accel(arch))
}

/// The accel-injectable core of [`build_container_args`] (tests pin the
/// accelerator so assertions don't depend on /dev/kvm on the build host).
pub(crate) fn build_container_args_with_accel(
    lab: &str,
    name: &str,
    arch: &str,
    cpus: u32,
    memory_bytes: u64,
    paths: &ContainerVmPaths,
    accel: Accel,
) -> Result<Vec<String>> {
    fn arg(a: &mut Vec<String>, s: &str, v: String) {
        a.push(format!("-{s}"));
        a.push(v);
    }
    let mut a: Vec<String> = Vec::new();

    // Same marker as full VMs so `kill_lab_orphans` reaps containers too.
    arg(&mut a, "name", format!("vmlab:{lab}/{name}"));

    let machine = match arch {
        "x86_64" => "q35",
        "aarch64" => "virt",
        "riscv64" => "virt,acpi=off",
        other => bail!("containers do not run on arch {other} (x86_64, aarch64, riscv64)"),
    };
    // vhost-user-fs back-ends map guest RAM, which requires a shared memory
    // backend. Only volume-carrying containers get one: switching the
    // backend renames the RAM block inside existing snapshots (pc.ram →
    // mem0), so the plain `-m` shape stays untouched for everyone else.
    let machine = if paths.volumes.is_empty() {
        machine.to_string()
    } else {
        format!("{machine},memory-backend=mem0")
    };
    arg(&mut a, "machine", machine);
    if !paths.volumes.is_empty() {
        arg(
            &mut a,
            "object",
            format!(
                "memory-backend-memfd,id=mem0,size={}M,share=on",
                memory_bytes >> 20
            ),
        );
    }
    match accel {
        Accel::Kvm => {
            arg(&mut a, "accel", "kvm".into());
            arg(&mut a, "cpu", "host".into());
        }
        Accel::Tcg => {
            arg(&mut a, "accel", "tcg".into());
            arg(&mut a, "cpu", "max".into());
        }
    }
    arg(&mut a, "smp", cpus.to_string());
    arg(&mut a, "m", format!("{}M", memory_bytes >> 20));

    // Fully headless: no display, and no default VGA either (a micro-VM has
    // no console to look at — the serial log is the console). No USB, no
    // audio, no tablet: nothing a container needs.
    arg(&mut a, "display", "none".into());
    arg(&mut a, "vga", "none".into());

    // Direct kernel boot of the guest asset. The device names in
    // `vmlab.root=`/`vmlab.scratch=` are fixed by the blockdev order below:
    // the first virtio-blk device is the squashfs rootfs (vda), the second
    // the scratch qcow2 (vdb) — see guest/cinit/src/cmdline.rs.
    let console = match arch {
        "aarch64" => "ttyAMA0", // PL011 on the virt machine
        _ => "ttyS0",           // 16550A on q35 and riscv virt
    };
    arg(&mut a, "kernel", paths.kernel.display().to_string());
    arg(&mut a, "initrd", paths.initrd.display().to_string());
    arg(
        &mut a,
        "append",
        format!(
            "console={console} quiet loglevel=3 panic=-1 reboot=t \
             vmlab.root=/dev/vda vmlab.scratch=/dev/vdb"
        ),
    );

    // Serial console → file: kernel messages plus the container's
    // stdout/stderr (cinit leaves them on the console).
    arg(
        &mut a,
        "serial",
        format!("file:{}", paths.serial_log.display()),
    );

    arg(
        &mut a,
        "qmp",
        format!("unix:{},server=on,wait=off", paths.qmp_sock.display()),
    );
    a.push("-monitor".into());
    a.push("none".into());

    // One virtio-serial bus, two ports: the vmlab ctl channel and the
    // vmlab-agent channel (terminals/exec/files — the same vmlab.agent.0 as
    // full VMs). QEMU owns both sockets (server=on,wait=off); the host
    // connects as a client.
    arg(
        &mut a,
        "chardev",
        format!(
            "socket,id=ctl0,path={},server=on,wait=off",
            paths.ctl_sock.display()
        ),
    );
    arg(
        &mut a,
        "chardev",
        format!(
            "socket,id=vagent0,path={},server=on,wait=off",
            paths.agent_sock.display()
        ),
    );
    arg(&mut a, "device", "virtio-serial-pci".into());
    arg(
        &mut a,
        "device",
        "virtserialport,chardev=ctl0,name=vmlab.ctl.0".into(),
    );
    arg(
        &mut a,
        "device",
        format!(
            "virtserialport,chardev=vagent0,name={}",
            vmlab_agent_proto::PORT_NAME
        ),
    );

    // Rootfs (raw squashfs, read-only) then scratch (qcow2): the virtio-blk
    // device order here fixes vda/vdb, which the -append above names.
    a.push("-blockdev".into());
    a.push(format!(
        "driver=raw,node-name=rootfs,read-only=on,file.driver=file,file.filename={}",
        paths.rootfs_image.display()
    ));
    a.push("-device".into());
    a.push("virtio-blk-pci,drive=rootfs".into());
    a.push("-blockdev".into());
    a.push(format!(
        "driver=qcow2,node-name=scratch,file.driver=file,file.filename={}",
        paths.scratch_disk.display()
    ));
    a.push("-device".into());
    a.push("virtio-blk-pci,drive=scratch".into());

    // NICs: identical to full VMs (stream-socket netdev into the segment
    // switch; the daemon listens, QEMU connects). Containers are always
    // virtio-net.
    for (i, (mac, sock, mtu)) in paths.nics.iter().enumerate() {
        arg(
            &mut a,
            "netdev",
            format!(
                "stream,id=net{i},server=off,addr.type=unix,addr.path={}",
                sock.display()
            ),
        );
        let host_mtu = mtu
            .filter(|m| *m != 1500)
            .map(|m| format!(",host_mtu={m}"))
            .unwrap_or_default();
        arg(
            &mut a,
            "device",
            format!("virtio-net-pci,netdev=net{i},mac={mac}{host_mtu}"),
        );
    }
    if paths.nics.is_empty() {
        a.push("-nic".into());
        a.push("none".into());
    }

    // virtiofs volumes: one vhost-user-fs device per export, tag = share
    // name (cinit mounts by tag, guest/cinit/src/mounts.rs). After the
    // virtio-blk devices so vda/vdb enumeration stays fixed.
    for (i, (tag, sock)) in paths.volumes.iter().enumerate() {
        arg(
            &mut a,
            "chardev",
            format!("socket,id=vfs{i},path={}", sock.display()),
        );
        arg(
            &mut a,
            "device",
            format!("vhost-user-fs-pci,chardev=vfs{i},tag={tag}"),
        );
    }

    // Start paused; labd releases the CPUs via QMP `cont` once everything
    // is attached, exactly like full VMs.
    a.push("-S".into());

    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> ContainerVmPaths {
        ContainerVmPaths {
            kernel: "/assets/x86_64/vmlinuz".into(),
            initrd: "/assets/x86_64/initramfs.img".into(),
            rootfs_image: "/cache/oci/sha256-ab/rootfs.sqfs".into(),
            scratch_disk: "/lab/.vmlab/containers/web/scratch.qcow2".into(),
            nics: vec![(
                "52:54:00:00:00:07".parse().unwrap(),
                PathBuf::from("/run/l/web/nic0.sock"),
                None,
            )],
            qmp_sock: "/run/l/web/qmp.sock".into(),
            ctl_sock: "/run/l/web/ctl.sock".into(),
            agent_sock: "/run/l/web/agent.sock".into(),
            serial_log: "/logs/l/web/console.log".into(),
            volumes: vec![],
        }
    }

    fn joined(args: &[String]) -> String {
        args.join(" ")
    }

    #[test]
    fn x86_64_shape() {
        let p = paths();
        let args =
            build_container_args_with_accel("lab1", "web", "x86_64", 2, 256 << 20, &p, Accel::Kvm)
                .unwrap();
        let s = joined(&args);
        assert!(s.contains("-name vmlab:lab1/web"), "{s}");
        assert!(s.contains("-machine q35"), "{s}");
        assert!(s.contains("-accel kvm"), "{s}");
        assert!(s.contains("-cpu host"), "{s}");
        assert!(s.contains("-smp 2"), "{s}");
        assert!(s.contains("-m 256M"), "{s}");
        assert!(s.contains("-display none"), "{s}");
        assert!(s.contains("-vga none"), "{s}");
        assert!(s.contains("-kernel /assets/x86_64/vmlinuz"), "{s}");
        assert!(s.contains("-initrd /assets/x86_64/initramfs.img"), "{s}");
        assert!(
            s.contains(
                "-append console=ttyS0 quiet loglevel=3 panic=-1 reboot=t \
                 vmlab.root=/dev/vda vmlab.scratch=/dev/vdb"
            ),
            "{s}"
        );
        assert!(s.contains("-serial file:/logs/l/web/console.log"), "{s}");
        assert!(
            s.contains("-qmp unix:/run/l/web/qmp.sock,server=on,wait=off"),
            "{s}"
        );
        assert!(s.contains("-monitor none"), "{s}");
        assert!(
            s.contains("socket,id=ctl0,path=/run/l/web/ctl.sock,server=on,wait=off"),
            "{s}"
        );
        assert!(
            s.contains("socket,id=vagent0,path=/run/l/web/agent.sock,server=on,wait=off"),
            "{s}"
        );
        assert!(s.contains("-device virtio-serial-pci"), "{s}");
        assert!(
            s.contains("virtserialport,chardev=ctl0,name=vmlab.ctl.0"),
            "{s}"
        );
        assert!(
            s.contains("virtserialport,chardev=vagent0,name=vmlab.agent.0"),
            "{s}"
        );
        assert!(
            s.contains(
                "driver=raw,node-name=rootfs,read-only=on,file.driver=file,\
                 file.filename=/cache/oci/sha256-ab/rootfs.sqfs"
            ),
            "{s}"
        );
        assert!(
            s.contains(
                "driver=qcow2,node-name=scratch,file.driver=file,\
                 file.filename=/lab/.vmlab/containers/web/scratch.qcow2"
            ),
            "{s}"
        );
        // No 9p, ever: it would add a migration blocker and break online
        // snapshots (PRD §18). Shared folders attach as vhost-user-fs.
        assert!(!s.contains("-virtfs"), "{s}");
        assert!(!s.contains("mount_tag"), "{s}");
        // No volumes → plain anonymous RAM, no vhost-user-fs devices.
        assert!(!s.contains("memory-backend"), "{s}");
        assert!(!s.contains("vhost-user-fs"), "{s}");
        assert!(
            s.contains("stream,id=net0,server=off,addr.type=unix,addr.path=/run/l/web/nic0.sock"),
            "{s}"
        );
        assert!(
            s.contains("virtio-net-pci,netdev=net0,mac=52:54:00:00:00:07"),
            "{s}"
        );
        assert!(!s.contains("host_mtu"), "{s}");
        // Nothing a container doesn't need.
        assert!(!s.contains("usb"), "{s}");
        assert!(!s.contains("-vnc"), "{s}");
        assert!(!s.contains("pflash"), "{s}");
        assert_eq!(args.last().unwrap(), "-S");
    }

    /// The kernel-cmdline device names are positional: `vmlab.root=/dev/vda`
    /// must be the *first* virtio-blk device (rootfs), `vmlab.scratch=/dev/vdb`
    /// the second (scratch).
    #[test]
    fn blockdev_order_matches_cmdline_device_names() {
        let args =
            build_container_args_with_accel("l", "c", "x86_64", 1, 256 << 20, &paths(), Accel::Kvm)
                .unwrap();
        let pos = |needle: &str| {
            args.iter()
                .position(|a| a == needle)
                .unwrap_or_else(|| panic!("{needle} not in argv"))
        };
        let root_dev = pos("virtio-blk-pci,drive=rootfs");
        let scratch_dev = pos("virtio-blk-pci,drive=scratch");
        assert!(
            root_dev < scratch_dev,
            "rootfs (vda) must precede scratch (vdb)"
        );
        let append = args[pos("-append") + 1].clone();
        assert!(append.contains("vmlab.root=/dev/vda"), "{append}");
        assert!(append.contains("vmlab.scratch=/dev/vdb"), "{append}");
    }

    /// An air-gapped aarch64 container: virt machine, PL011 console, no
    /// network hardware at all, TCG fallback.
    #[test]
    fn aarch64_airgapped_shape() {
        let mut p = paths();
        p.nics.clear();
        let args =
            build_container_args_with_accel("lab1", "iso", "aarch64", 1, 128 << 20, &p, Accel::Tcg)
                .unwrap();
        let s = joined(&args);
        assert!(s.contains("-machine virt"), "{s}");
        assert!(!s.contains("acpi=off"), "{s}");
        assert!(s.contains("-accel tcg"), "{s}");
        assert!(s.contains("-cpu max"), "{s}");
        assert!(s.contains("-m 128M"), "{s}");
        assert!(s.contains("console=ttyAMA0"), "{s}");
        assert!(s.contains("-nic none"), "{s}");
        assert!(!s.contains("-netdev"), "{s}");
        assert!(!s.contains("-virtfs"), "{s}");
        assert_eq!(args.last().unwrap(), "-S");
    }

    #[test]
    fn riscv64_pins_acpi_off_and_ttys0() {
        let s = joined(
            &build_container_args_with_accel(
                "l",
                "c",
                "riscv64",
                1,
                256 << 20,
                &paths(),
                Accel::Tcg,
            )
            .unwrap(),
        );
        assert!(s.contains("-machine virt,acpi=off"), "{s}");
        assert!(s.contains("console=ttyS0"), "{s}");
    }

    /// Volumes attach as vhost-user-fs devices and flip the RAM to a shared
    /// memfd backend (vhost-user requirement); still zero 9p.
    #[test]
    fn volumes_add_vhost_user_fs_and_shared_memory() {
        let mut p = paths();
        p.volumes = vec![
            ("vol-web-0".into(), PathBuf::from("/run/l/web/vfs0.sock")),
            ("vol-web-1".into(), PathBuf::from("/run/l/web/vfs1.sock")),
        ];
        let args =
            build_container_args_with_accel("lab1", "web", "x86_64", 1, 512 << 20, &p, Accel::Kvm)
                .unwrap();
        let s = joined(&args);
        assert!(s.contains("-machine q35,memory-backend=mem0"), "{s}");
        assert!(
            s.contains("memory-backend-memfd,id=mem0,size=512M,share=on"),
            "{s}"
        );
        assert!(
            s.contains("socket,id=vfs0,path=/run/l/web/vfs0.sock"),
            "{s}"
        );
        assert!(
            s.contains("vhost-user-fs-pci,chardev=vfs0,tag=vol-web-0"),
            "{s}"
        );
        assert!(
            s.contains("vhost-user-fs-pci,chardev=vfs1,tag=vol-web-1"),
            "{s}"
        );
        assert!(!s.contains("-virtfs"), "{s}");
        assert!(!s.contains("mount_tag"), "{s}");
        // The fs devices come after both virtio-blk devices — vda/vdb
        // enumeration must not shift under the kernel-cmdline names.
        let pos = |needle: &str| {
            args.iter()
                .position(|a| a.contains(needle))
                .unwrap_or_else(|| panic!("{needle} not in argv"))
        };
        assert!(pos("drive=scratch") < pos("vhost-user-fs-pci"), "{s}");
    }

    #[test]
    fn jumbo_mtu_sets_host_mtu() {
        let mut p = paths();
        p.nics[0].2 = Some(9000);
        let s = joined(
            &build_container_args_with_accel("l", "c", "x86_64", 1, 1 << 30, &p, Accel::Kvm)
                .unwrap(),
        );
        assert!(s.contains("host_mtu=9000"), "{s}");

        // 1500 is the default — nothing emitted.
        let mut p = paths();
        p.nics[0].2 = Some(1500);
        let s = joined(
            &build_container_args_with_accel("l", "c", "x86_64", 1, 1 << 30, &p, Accel::Kvm)
                .unwrap(),
        );
        assert!(!s.contains("host_mtu"), "{s}");
    }

    #[test]
    fn unsupported_arch_is_an_error() {
        let err =
            build_container_args_with_accel("l", "c", "s390x", 1, 1 << 30, &paths(), Accel::Tcg)
                .unwrap_err();
        assert!(err.to_string().contains("s390x"), "{err}");
    }
}
