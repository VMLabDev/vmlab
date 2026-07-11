//! Build the QEMU argv for one container micro-VM (PRD §18). Pure function
//! of the container's resolved shape + runtime paths, mirroring
//! [`super::cmdline::build_args`] for full VMs, so it is exhaustively
//! unit-testable.
//!
//! The guest side of this contract is FROZEN in `guest/` (see
//! `guest/cinit/src/cmdline.rs` and `guest/build-asset.sh`): direct kernel
//! boot with `vmlab.root=`/`vmlab.scratch=` naming the first two virtio-blk
//! devices, a read-only 9p config share tagged `vmlab.cfg`, volume shares
//! tagged `vol0..volN`, and three virtio-serial ports —
//! `org.qemu.guest_agent.0` (qemu-ga), `vmlab.ctl.0` (the ndjson ctl
//! channel, [`crate::labd::container_ctl`]) and `vmlab.tty.0` (the raw
//! interactive-shell byte stream, guest/cinit/src/tty.rs).

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
    /// Directory holding `container.json`, exported read-only over 9p as
    /// `vmlab.cfg`.
    pub cfg_dir: PathBuf,
    /// 9p volume exports in declaration order: (mount tag `vol<i>`, host
    /// path, read-only). The tag must match the `VolumeMount::tag` the spec
    /// carries — cinit mounts by tag.
    pub volumes: Vec<(String, PathBuf, bool)>,
    /// One unix socket per NIC, in declaration order: (MAC, socket, segment
    /// MTU) — byte-for-byte the full-VM netdev shape so labd's switch ports
    /// attach unchanged.
    pub nics: Vec<(MacAddr, PathBuf, Option<u16>)>,
    pub qmp_sock: PathBuf,
    pub qga_sock: PathBuf,
    /// The `vmlab.ctl.0` channel socket (QEMU listens; the host connects).
    pub ctl_sock: PathBuf,
    /// The `vmlab.tty.0` interactive-shell socket (raw PTY bytes; QEMU
    /// listens, shell clients connect).
    pub tty_sock: PathBuf,
    /// Serial console log — the container's stdout/stderr land here.
    pub serial_log: PathBuf,
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
    arg(&mut a, "machine", machine.to_string());
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

    // One virtio-serial bus, three ports: the guest agent, the vmlab ctl
    // channel and the interactive-shell tty. QEMU owns all three sockets
    // (server=on,wait=off); the host connects as a client.
    arg(
        &mut a,
        "chardev",
        format!(
            "socket,id=qga0,path={},server=on,wait=off",
            paths.qga_sock.display()
        ),
    );
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
            "socket,id=tty0,path={},server=on,wait=off",
            paths.tty_sock.display()
        ),
    );
    arg(&mut a, "device", "virtio-serial-pci".into());
    arg(
        &mut a,
        "device",
        "virtserialport,chardev=qga0,name=org.qemu.guest_agent.0".into(),
    );
    arg(
        &mut a,
        "device",
        "virtserialport,chardev=ctl0,name=vmlab.ctl.0".into(),
    );
    arg(
        &mut a,
        "device",
        "virtserialport,chardev=tty0,name=vmlab.tty.0".into(),
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

    // 9p shares: the read-only config dir (container.json) plus one export
    // per volume. Tags are the contract with cinit: `vmlab.cfg` and
    // `vol0..volN` (guest/cinit/src/mounts.rs). Volumes use mapped-xattr so
    // in-guest ownership/modes survive on the host without root.
    arg(
        &mut a,
        "virtfs",
        format!(
            "local,id=cfg,path={},mount_tag=vmlab.cfg,security_model=none,readonly=on",
            paths.cfg_dir.display()
        ),
    );
    for (i, (tag, host, read_only)) in paths.volumes.iter().enumerate() {
        let ro = if *read_only { ",readonly=on" } else { "" };
        arg(
            &mut a,
            "virtfs",
            format!(
                "local,id=vol{i},path={},mount_tag={tag},security_model=mapped-xattr{ro}",
                host.display()
            ),
        );
    }

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
            cfg_dir: "/lab/.vmlab/containers/web/cfg".into(),
            volumes: vec![
                ("vol0".into(), "/lab/html".into(), true),
                ("vol1".into(), "/lab/.vmlab/volumes/data".into(), false),
            ],
            nics: vec![(
                "52:54:00:00:00:07".parse().unwrap(),
                PathBuf::from("/run/l/web/nic0.sock"),
                None,
            )],
            qmp_sock: "/run/l/web/qmp.sock".into(),
            qga_sock: "/run/l/web/qga.sock".into(),
            ctl_sock: "/run/l/web/ctl.sock".into(),
            tty_sock: "/run/l/web/tty.sock".into(),
            serial_log: "/logs/l/web/console.log".into(),
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
            s.contains("socket,id=qga0,path=/run/l/web/qga.sock,server=on,wait=off"),
            "{s}"
        );
        assert!(
            s.contains("socket,id=ctl0,path=/run/l/web/ctl.sock,server=on,wait=off"),
            "{s}"
        );
        assert!(
            s.contains("socket,id=tty0,path=/run/l/web/tty.sock,server=on,wait=off"),
            "{s}"
        );
        assert!(s.contains("-device virtio-serial-pci"), "{s}");
        assert!(
            s.contains("virtserialport,chardev=qga0,name=org.qemu.guest_agent.0"),
            "{s}"
        );
        assert!(
            s.contains("virtserialport,chardev=ctl0,name=vmlab.ctl.0"),
            "{s}"
        );
        assert!(
            s.contains("virtserialport,chardev=tty0,name=vmlab.tty.0"),
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
        assert!(
            s.contains(
                "-virtfs local,id=cfg,path=/lab/.vmlab/containers/web/cfg,\
                 mount_tag=vmlab.cfg,security_model=none,readonly=on"
            ),
            "{s}"
        );
        assert!(
            s.contains(
                "local,id=vol0,path=/lab/html,mount_tag=vol0,\
                 security_model=mapped-xattr,readonly=on"
            ),
            "{s}"
        );
        assert!(
            s.contains(
                "local,id=vol1,path=/lab/.vmlab/volumes/data,mount_tag=vol1,\
                 security_model=mapped-xattr"
            ),
            "{s}"
        );
        assert!(!s.contains("mount_tag=vol1,security_model=mapped-xattr,readonly"));
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
        p.volumes.clear();
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
        assert!(!s.contains("mount_tag=vol"), "{s}");
        assert!(s.contains("mount_tag=vmlab.cfg"), "{s}");
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
