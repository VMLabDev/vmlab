//! The launch `vmlab-hv` reads: a [`MachineDescription`] composed from the
//! typed container launch, the way the QEMU adapter composes an argv from the
//! same one (ADR-0020). Pure, so the shape is pinned by tests rather than by
//! booting.
//!
//! The guest side is the one the QEMU argv serves — `guest/cinit` finds its
//! rootfs and scratch disk by the virtio-blk order and the kernel command
//! line, and its two channels by port name — so everything here mirrors
//! [`crate::qemu::container::build_container_args`], device for device.

use anyhow::{Result, bail};
use vmlab_hv_proto::MachineDescription;
use vmlab_hv_proto::machine::{
    Accel, Arch, Boot, Disk, DiskFormat, FsShare, Nic, NicAttachment, SerialPort,
};

use crate::qemu::ResolvedContainer;
use crate::qemu::container::{ContainerVmPaths, kernel_cmdline};

/// The disk whose snapshot carries RAM and device state: the scratch disk,
/// the container's only writable one, as under QEMU.
pub(crate) const VMSTATE_DISK: &str = "scratch";

/// The machine description for one container micro-VM. Refuses what
/// validation refuses too (PRD §22), as the last word before a spawn.
pub(crate) fn container(
    lab: &str,
    resolved: &ResolvedContainer,
    paths: &ContainerVmPaths,
) -> Result<MachineDescription> {
    if resolved.arch != "x86_64" {
        bail!(
            "container \"{}\": vmlab-hv runs x86_64 only so far, and this is {} (PRD §22)",
            resolved.name,
            resolved.arch
        );
    }
    let rtc_base = match &resolved.rtc_base {
        None => None,
        // The resolver writes QEMU's `-rtc base=` form, UTC without a zone;
        // the description wants RFC 3339, which is the same instant with one.
        Some(base) => match chrono::NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H:%M:%S") {
            Ok(at) => Some(
                at.and_utc()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            Err(_) => bail!(
                "container \"{}\": clock base `{base}` is not a UTC date-time vmlab-hv can start at",
                resolved.name
            ),
        },
    };
    Ok(MachineDescription {
        // The marker `kill_lab_orphans` knows a QEMU by, so the process
        // title and logs name the lab and machine the same way.
        name: format!("vmlab:{lab}/{}", resolved.name),
        arch: Arch::X86_64,
        accel: Accel::Kvm,
        vcpus: resolved.cpus,
        memory_mib: resolved.memory >> 20,
        boot: Boot::Kernel {
            kernel: paths.kernel.clone(),
            initrd: Some(paths.initrd.clone()),
            cmdline: kernel_cmdline(&resolved.arch),
        },
        // vda then vdb: the order `vmlab.root=`/`vmlab.scratch=` name.
        disks: vec![
            Disk {
                id: "rootfs".into(),
                path: paths.rootfs_image.clone(),
                format: DiskFormat::Raw,
                read_only: true,
                vmstate: false,
            },
            Disk {
                id: VMSTATE_DISK.into(),
                path: paths.scratch_disk.clone(),
                format: DiskFormat::Qcow2,
                read_only: false,
                vmstate: true,
            },
        ],
        nics: paths
            .nics
            .iter()
            .map(|(mac, socket, mtu)| Nic {
                mac: mac.to_string(),
                mtu: *mtu,
                attachment: NicAttachment::Stream {
                    socket: socket.clone(),
                },
            })
            .collect(),
        serial_log: paths.serial_log.clone(),
        ports: vec![
            SerialPort {
                name: "vmlab.ctl.0".into(),
                socket: paths.ctl_sock.clone(),
            },
            SerialPort {
                name: vmlab_agent_proto::PORT_NAME.into(),
                socket: paths.agent_sock.clone(),
            },
        ],
        shares: paths
            .volumes
            .iter()
            .map(|(tag, socket)| FsShare {
                tag: tag.clone(),
                socket: socket.clone(),
            })
            .collect(),
        rtc_base,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::resolve::testing::resolved_container as resolved;
    use std::path::PathBuf;

    fn paths() -> ContainerVmPaths {
        ContainerVmPaths {
            kernel: "/assets/x86_64/vmlinuz".into(),
            initrd: "/assets/x86_64/initramfs.img".into(),
            rootfs_image: "/cache/oci/sha256-ab/rootfs.sqfs".into(),
            scratch_disk: "/lab/.vmlab/containers/web/scratch.qcow2".into(),
            nics: vec![
                (
                    "52:54:00:00:00:07".parse().unwrap(),
                    PathBuf::from("/run/l/web/nic0.sock"),
                    None,
                ),
                (
                    "52:54:00:00:00:08".parse().unwrap(),
                    PathBuf::from("/run/l/web/nic1.sock"),
                    Some(9000),
                ),
            ],
            qmp_sock: "/run/l/web/qmp.sock".into(),
            ctl_sock: "/run/l/web/ctl.sock".into(),
            agent_sock: "/run/l/web/agent.sock".into(),
            serial_log: "/logs/l/web/console.log".into(),
            volumes: vec![("data".into(), "/run/l/web/virtiofs0.sock".into())],
        }
    }

    #[test]
    fn a_container_is_described_device_for_device() {
        let r = resolved(
            r#"container "web" { image = "nginx" cpus = 2 memory = 512MiB }"#,
            "x86_64",
        );
        let d = container("lab1", &r, &paths()).unwrap();
        assert_eq!(d.name, "vmlab:lab1/web");
        assert_eq!((d.arch, d.accel), (Arch::X86_64, Accel::Kvm));
        assert_eq!((d.vcpus, d.memory_mib), (2, 512));
        let Boot::Kernel {
            kernel,
            initrd,
            cmdline,
        } = &d.boot;
        assert_eq!(kernel, &PathBuf::from("/assets/x86_64/vmlinuz"));
        assert_eq!(
            initrd.as_deref(),
            Some(std::path::Path::new("/assets/x86_64/initramfs.img"))
        );
        // The QEMU path's command line, word for word: cinit reads it.
        assert_eq!(cmdline, &kernel_cmdline("x86_64"));
        assert!(cmdline.contains("vmlab.root=/dev/vda vmlab.scratch=/dev/vdb"));

        // vda is the read-only squashfs, vdb the scratch qcow2 that carries
        // the vmstate — the one writable disk.
        let disks: Vec<_> = d
            .disks
            .iter()
            .map(|d| (d.id.as_str(), d.format, d.read_only, d.vmstate))
            .collect();
        assert_eq!(
            disks,
            vec![
                ("rootfs", DiskFormat::Raw, true, false),
                ("scratch", DiskFormat::Qcow2, false, true),
            ]
        );

        assert_eq!(d.nics.len(), 2);
        assert_eq!(d.nics[0].mac, "52:54:00:00:00:07");
        assert_eq!(d.nics[0].mtu, None);
        assert_eq!(d.nics[1].mtu, Some(9000));
        assert_eq!(
            d.nics[1].attachment,
            NicAttachment::Stream {
                socket: "/run/l/web/nic1.sock".into()
            }
        );

        let ports: Vec<_> = d.ports.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(ports, vec!["vmlab.ctl.0", "vmlab.agent.0"]);
        assert_eq!(d.ports[1].socket, PathBuf::from("/run/l/web/agent.sock"));
        assert_eq!(
            d.shares,
            vec![FsShare {
                tag: "data".into(),
                socket: "/run/l/web/virtiofs0.sock".into()
            }]
        );
        assert_eq!(d.serial_log, PathBuf::from("/logs/l/web/console.log"));
        assert_eq!(d.rtc_base, None);
    }

    /// The description is the file vmlab-hv parses with
    /// `deny_unknown_fields`, so it has to survive its own round trip.
    #[test]
    fn the_description_round_trips_as_json() {
        let r = resolved(
            r#"container "web" { image = "nginx" cpus = 1 memory = 256MiB }"#,
            "x86_64",
        );
        let d = container("l", &r, &paths()).unwrap();
        let json = serde_json::to_string(&d).unwrap();
        let back: MachineDescription = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn a_fixed_clock_starts_the_rtc_at_that_utc_instant() {
        let r = resolved(
            r#"container "web" { image = "nginx" cpus = 1 memory = 256MiB clock = "2020-01-02T03:04:05+02:00" }"#,
            "x86_64",
        );
        let d = container("l", &r, &paths()).unwrap();
        assert_eq!(d.rtc_base.as_deref(), Some("2020-01-02T01:04:05Z"));
    }

    #[test]
    fn another_arch_is_refused() {
        let r = resolved(
            r#"container "web" { image = "nginx" cpus = 1 memory = 256MiB }"#,
            "aarch64",
        );
        let err = container("l", &r, &paths()).unwrap_err().to_string();
        assert!(err.contains("x86_64 only"), "{err}");
    }
}
