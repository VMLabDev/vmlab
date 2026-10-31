//! The launch `vmlab-hv` reads: a [`MachineDescription`] composed from the
//! typed launch, the way the QEMU adapter composes an argv from the same one
//! (ADR-0020). Pure, so the shape is pinned by tests rather than by booting.
//!
//! A container's guest side is the one the QEMU argv serves — `guest/cinit`
//! finds its rootfs and scratch disk by the virtio-blk order and the kernel
//! command line, and its two channels by port name — so [`container`]
//! mirrors [`crate::qemu::container::build_container_args`], device for
//! device. A VM's guest is a template built under QEMU, which has to find the
//! same hardware at the same addresses (HV-D30), so [`vm`] places every PCI
//! device in the slot QEMU's q35 gives the argv [`crate::qemu::build_args`]
//! composes for the same machine.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use vmlab_hv_proto::MachineDescription;
use vmlab_hv_proto::machine::{
    Accel, Arch, Boot, CpuPolicy, Disk, DiskBus, DiskFormat, Display, DisplayDevice, FirmwareKind,
    FsShare, HyperV, Machine, Nic, NicAttachment, PciAddress, SerialPort, Tpm,
};

use crate::profiles::AgentTransport;
use crate::qemu::container::{ContainerVmPaths, kernel_cmdline};
use crate::qemu::{NicBackend, ResolvedContainer, ResolvedVm, VmPaths};

/// The disk whose snapshot carries RAM and device state: the scratch disk,
/// the container's only writable one, as under QEMU.
pub(crate) const VMSTATE_DISK: &str = "scratch";

/// The disk a VM's snapshots carry RAM and device state on: its primary,
/// as QMP `snapshot-save` is asked to under QEMU (§7.3).
pub(crate) const VM_VMSTATE_DISK: &str = "disk0";

/// The host files a VM's description names beyond its own disks: firmware
/// and option ROMs, found by [`super::HostRoms::probe`] — a host probe, so
/// [`vm`] stays a function of what it is handed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HostRoms {
    /// SeaBIOS (`bios-256k.bin`).
    pub seabios: Option<PathBuf>,
    /// SeaVGABIOS for std VGA (`vgabios-stdvga.bin`).
    pub vgabios_std: Option<PathBuf>,
    /// SeaVGABIOS for virtio-vga (`vgabios-virtio.bin`).
    pub vgabios_virtio: Option<PathBuf>,
    /// vmlab-hv's reproducible iPXE build for `1af4:1000` (HV phase-2 D16).
    /// Absent leaves the NICs without an option ROM — no network boot, which
    /// a disk-booting template never asks for.
    pub ipxe_virtio: Option<PathBuf>,
    /// The same build for a VM on OVMF: the combined legacy and EFI image
    /// (`virtio-net.rom`), as QEMU's `efi-virtio.rom` is, else the EFI one.
    pub ipxe_virtio_efi: Option<PathBuf>,
}

/// The QEMU `-rtc base=` form the resolver writes — UTC without a zone — as
/// the RFC 3339 instant the description wants.
fn rtc_base(kind: &str, name: &str, base: &Option<String>) -> Result<Option<String>> {
    match base {
        None => Ok(None),
        Some(base) => match chrono::NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H:%M:%S") {
            Ok(at) => Ok(Some(
                at.and_utc()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            )),
            Err(_) => bail!(
                "{kind} \"{name}\": clock base `{base}` is not a UTC date-time vmlab-hv can start at"
            ),
        },
    }
}

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
    let rtc_base = rtc_base("container", &resolved.name, &resolved.rtc_base)?;
    let disk = |id: &str, path: &Path, format, read_only, vmstate| Disk {
        pci: None,
        bus: DiskBus::Virtio,
        port: None,
        cdrom: false,
        id: id.into(),
        path: Some(path.to_path_buf()),
        format,
        read_only,
        vmstate,
    };
    Ok(MachineDescription {
        machine: Machine::Microvm,
        cpu: CpuPolicy::Host,
        hyperv: Vec::new(),
        display: None,
        tablet: None,
        tpm: None,
        console_pci: None,
        ahci_pci: None,
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
            disk("rootfs", &paths.rootfs_image, DiskFormat::Raw, true, false),
            disk(
                VMSTATE_DISK,
                &paths.scratch_disk,
                DiskFormat::Qcow2,
                false,
                true,
            ),
        ],
        nics: paths
            .nics
            .iter()
            .map(|(mac, socket, mtu)| Nic {
                rom: None,
                pci: None,
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
                pci: None,
                tag: tag.clone(),
                socket: socket.clone(),
            })
            .collect(),
        // A container micro-VM has no sound card.
        audio: None,
        rtc_base,
    })
}

/// Bus-zero slots in the order QEMU's q35 hands them out: an unaddressed
/// `-device` takes the lowest free slot, from 1 (the host bridge holds 0,
/// the ICH9 functions 0x1f).
struct Slots(u8);

impl Slots {
    fn next(&mut self) -> PciAddress {
        let at = PciAddress {
            bus: 0,
            device: self.0,
            function: 0,
        };
        self.0 += 1;
        at
    }
}

/// The ICH9 AHCI controller, fixed at 00:1f.2 on q35.
const AHCI: PciAddress = PciAddress {
    bus: 0,
    device: 0x1f,
    function: 2,
};

/// SeaBIOS's device path for a virtio-blk disk at `at` — the `bootorder`
/// entry QEMU writes for a `bootindex`.
fn virtio_boot_path(at: PciAddress) -> String {
    match at.function {
        0 => format!("/pci@i0cf8/scsi@{:x}/disk@0,0", at.device),
        f => format!("/pci@i0cf8/scsi@{:x},{f:x}/disk@0,0", at.device),
    }
}

/// SeaBIOS's device path for the drive on AHCI port `port`.
fn ahci_boot_path(port: u8) -> String {
    format!(
        "/pci@i0cf8/pci8086,2922@{:x},{:x}/drive@{port:x}/disk@0",
        AHCI.device, AHCI.function
    )
}

/// The enlightenments a `hyperv` VM gets: QEMU's [`crate::qemu::cmdline::HYPERV_FLAGS`],
/// name for name, and `avic` with them where the host virtualises the APIC,
/// as the QEMU path adds [`crate::qemu::cmdline::HYPERV_APICV_FLAG`].
fn hyperv_list(resolved: &ResolvedVm, apic_virtualization: bool) -> Result<Vec<HyperV>> {
    if !resolved.hyperv {
        return Ok(Vec::new());
    }
    let mut flags: Vec<&str> = crate::qemu::cmdline::HYPERV_FLAGS.split(',').collect();
    if apic_virtualization {
        flags.push(crate::qemu::cmdline::HYPERV_APICV_FLAG);
    }
    flags
        .into_iter()
        .map(|flag| {
            // `hv-spinlocks=0x1fff` names `spinlocks`; vmlab-hv fixes the count.
            let name = flag.trim().trim_start_matches("hv-");
            let name = name.split('=').next().unwrap_or(name);
            serde_json::from_value(serde_json::Value::String(name.to_string()))
                .map_err(|_| anyhow::anyhow!("vmlab-hv has no Hyper-V enlightenment `{name}`"))
        })
        .collect()
}

/// The machine description for one VM: q35 on SeaBIOS or OVMF, as QEMU runs
/// the same resolved machine (§22, phases 2a to 2c). Refuses whatever
/// validation refuses too, as the last word before a spawn, and what only the
/// launch knows — a tap NIC, a host without its firmware.
pub(crate) fn vm(
    lab: &str,
    resolved: &ResolvedVm,
    paths: &VmPaths,
    accel: crate::qemu::Accel,
    roms: &HostRoms,
) -> Result<MachineDescription> {
    let name = &resolved.name;
    if crate::qemu::qemu_arch(&resolved.arch) != "x86_64" {
        bail!(
            "vm \"{name}\": vmlab-hv runs x86_64 only so far, and this is {} (PRD §22)",
            resolved.arch
        );
    }
    if accel != crate::qemu::Accel::Kvm {
        bail!("vm \"{name}\": vmlab-hv runs under KVM only, and KVM is not usable here (PRD §22)");
    }
    if !resolved.machine.starts_with("q35") {
        bail!(
            "vm \"{name}\": vmlab-hv runs the q35 machine only so far, and this is `{}` (PRD §22)",
            resolved.machine
        );
    }
    let ovmf = resolved.firmware == Some(crate::profiles::FirmwareKind::Ovmf);
    let display_device = match resolved.display_device.as_deref() {
        // QEMU's q35 adds a std VGA of its own where nothing names one.
        None | Some("VGA") => DisplayDevice::StdVga,
        Some("virtio-vga") => DisplayDevice::VirtioVga,
        Some(other) => {
            bail!(
                "vm \"{name}\": vmlab-hv presents std VGA and virtio-vga only, not `{other}` (PRD §22)"
            )
        }
    };
    // OVMF: the CODE image vmlab's firmware lookup chose for this VM's
    // secure-boot setting, and the VM's own VARS copy — never the template's
    // — exactly as QEMU's two pflash drives get them.
    let firmware = if ovmf {
        let (Some(code), Some(vars)) = (&paths.firmware_code, &paths.ovmf_vars) else {
            bail!("vm \"{name}\": OVMF needs its CODE image and the VM's own VARS file");
        };
        (FirmwareKind::Ovmf, code.clone(), Some(vars.clone()))
    } else {
        let Some(seabios) = &roms.seabios else {
            bail!(
                "vm \"{name}\": vmlab-hv boots it on SeaBIOS, and this host has no bios-256k.bin — \
                 install SeaBIOS (the `seabios` package), or put it under the host config's \
                 `firmware_dir`/x86_64/"
            );
        };
        (FirmwareKind::Seabios, seabios.clone(), None)
    };
    let Some(serial_log) = &paths.serial_log else {
        bail!("vm \"{name}\": vmlab-hv needs a serial log path");
    };

    let mut slots = Slots(1);
    // The default VGA is created with the machine, ahead of every `-device`.
    let default_vga = resolved.display_device.is_none().then(|| slots.next());

    // The agent's virtio-serial controller is the first `-device`.
    let agent = resolved.agent_transport == AgentTransport::VirtioSerial;
    let console_pci = agent.then(|| slots.next());
    let ports = if agent {
        vec![SerialPort {
            name: vmlab_agent_proto::PORT_NAME.into(),
            socket: paths.agent_sock.clone(),
        }]
    } else {
        Vec::new()
    };

    // Disks: disk0 (the primary) then the extras, each `bootindex` 50+i;
    // AHCI disks take the ports from 0, and the CD-ROMs carry on after them.
    let mut disks = Vec::new();
    let mut boot: Vec<(usize, String)> = Vec::new();
    let all_disks = std::iter::once(paths.primary_disk.as_path())
        .chain(paths.extra_disks.iter().map(|(_, p)| p.as_path()));
    let mut ahci_ports = 0u8;
    for (i, path) in all_disks.enumerate() {
        let (pci, bus, port, at) = match resolved.disk_bus {
            crate::profiles::DiskBus::Virtio => {
                let pci = slots.next();
                (Some(pci), DiskBus::Virtio, None, virtio_boot_path(pci))
            }
            // On q35 an `ide-hd` sits on an AHCI port, whichever the profile
            // calls it.
            crate::profiles::DiskBus::Ide | crate::profiles::DiskBus::Sata => {
                let port = ahci_ports;
                ahci_ports += 1;
                (None, DiskBus::Ahci, Some(port), ahci_boot_path(port))
            }
        };
        boot.push((50 + i, at));
        // `disk0`, `disk1`…: the node names QMP snapshots address under
        // QEMU, so a snapshot's disks are named alike on either.
        let id = format!("disk{i}");
        disks.push(Disk {
            pci,
            bus,
            port,
            cdrom: false,
            vmstate: id == VM_VMSTATE_DISK,
            id,
            path: Some(path.to_path_buf()),
            format: DiskFormat::Qcow2,
            read_only: false,
        });
    }
    for (i, iso) in paths.cdroms.iter().enumerate() {
        let port = ahci_ports;
        ahci_ports += 1;
        boot.push((i, ahci_boot_path(port)));
        disks.push(Disk {
            pci: None,
            bus: DiskBus::Ahci,
            port: Some(port),
            cdrom: true,
            id: format!("cd{i}"),
            path: Some(iso.clone()),
            format: DiskFormat::Raw,
            read_only: true,
            vmstate: false,
        });
    }
    if ahci_ports > 6 {
        bail!("vm \"{name}\": q35's AHCI controller has six ports, and this VM needs {ahci_ports}");
    }
    if paths.floppy.is_some() {
        bail!("vm \"{name}\": vmlab-hv has no floppy drive until phase 4 (PRD §22)");
    }
    // SeaBIOS tries them in `bootindex` order: install media, then disks.
    boot.sort_by_key(|(index, _)| *index);

    let mut nics = Vec::new();
    for nic in &paths.nics {
        let NicBackend::Stream { sock } = &nic.backend else {
            bail!(
                "vm \"{name}\": vmlab-hv attaches NICs by stream socket only, not a tap descriptor"
            );
        };
        nics.push(Nic {
            rom: if ovmf {
                roms.ipxe_virtio_efi.clone()
            } else {
                roms.ipxe_virtio.clone()
            },
            pci: Some(slots.next()),
            mac: nic.mac.to_string(),
            mtu: nic.mtu.filter(|m| *m != 1500),
            attachment: NicAttachment::Stream {
                socket: sock.clone(),
            },
        });
    }

    let display_pci = default_vga.unwrap_or_else(|| slots.next());
    let rom = match display_device {
        DisplayDevice::StdVga => roms.vgabios_std.clone(),
        DisplayDevice::VirtioVga => roms.vgabios_virtio.clone(),
    };
    // The tablet's xHCI (HV phase-2 D6) takes the slot after the display,
    // where QEMU's `-usb` puts its ICH9 UHCI/EHCI at 1d.x instead.
    let tablet = Some(slots.next());

    Ok(MachineDescription {
        machine: Machine::Q35,
        cpu: if resolved.nested {
            CpuPolicy::Nested
        } else {
            CpuPolicy::Host
        },
        hyperv: hyperv_list(resolved, paths.apic_virtualization)?,
        // Both named rather than left beside the control socket, so the
        // paths every consumer looks for are the ones this machine has.
        display: Some(Display {
            device: display_device,
            pci: display_pci,
            rom,
            socket: Some(paths.display_sock.clone()),
            vnc_socket: Some(paths.vnc_sock.clone()),
        }),
        tablet,
        // The swtpm vmlab started before the spawn, as QEMU's `tpm-tis`.
        tpm: paths.tpm_sock.clone().map(|socket| Tpm { socket }),
        console_pci,
        ahci_pci: Some(AHCI),
        name: format!("vmlab:{lab}/{name}"),
        arch: Arch::X86_64,
        accel: Accel::Kvm,
        vcpus: resolved.cpus,
        memory_mib: resolved.memory >> 20,
        boot: Boot::Firmware {
            firmware: firmware.0,
            code: firmware.1,
            vars: firmware.2,
            secure_boot: ovmf && resolved.secure_boot,
            // QEMU sets `bootindex` under SeaBIOS only; OVMF keeps its own
            // boot order in the VM's VARS.
            bootorder: if ovmf {
                Vec::new()
            } else {
                boot.into_iter().map(|(_, at)| at).collect()
            },
        },
        disks,
        nics,
        serial_log: serial_log.clone(),
        ports,
        shares: Vec::new(),
        audio: None,
        rtc_base: rtc_base("vm", name, &resolved.rtc_base)?,
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
        } = &d.boot
        else {
            panic!("a container boots its kernel: {:?}", d.boot)
        };
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
                pci: None,
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

    // -- VMs (phases 2a and 2b) ------------------------------------------

    use crate::qemu::resolve::testing::resolved_vm;
    use crate::qemu::{Accel as QemuAccel, NicSpec};

    /// Named apart so a return type is not read as a hand-built struct
    /// literal (`qemu::resolve`'s `resolution_is_never_mirrored`).
    type Resolved = ResolvedVm;

    fn roms() -> HostRoms {
        HostRoms {
            seabios: Some("/usr/share/qemu/bios-256k.bin".into()),
            vgabios_std: Some("/usr/share/qemu/vgabios-stdvga.bin".into()),
            vgabios_virtio: Some("/usr/share/qemu/vgabios-virtio.bin".into()),
            ipxe_virtio: Some("/fw/x86_64/1af41000.rom".into()),
            ipxe_virtio_efi: Some("/fw/x86_64/virtio-net.rom".into()),
        }
    }

    fn vm_paths() -> VmPaths {
        VmPaths {
            qmp_sock: "/run/l/v/control.sock".into(),
            agent_sock: "/run/l/v/agent.sock".into(),
            vnc_sock: "/run/l/v/vnc.sock".into(),
            display_sock: "/run/l/v/display.sock".into(),
            primary_disk: "/lab/.vmlab/vms/v/disk0.qcow2".into(),
            serial_log: Some("/logs/l/v/serial.log".into()),
            nics: vec![NicSpec {
                mac: "52:54:00:00:00:01".parse().unwrap(),
                mtu: None,
                backend: NicBackend::Stream {
                    sock: "/run/l/v/nic0.sock".into(),
                },
            }],
            ..Default::default()
        }
    }

    /// A VM declared with `body` beside its scratch disk, resolved against
    /// the shipped profiles and `extra`.
    fn declared(body: &str, extra: Option<crate::profiles::Profile>) -> Resolved {
        let mut profiles: Vec<_> = [
            "linux-modern",
            "linux-generic",
            "windows-legacy",
            "windows-10",
            "windows-11",
        ]
        .iter()
        .map(|n| {
            crate::profiles::ProfileSet::shipped()
                .unwrap()
                .get(n)
                .unwrap()
                .clone()
        })
        .collect();
        profiles.extend(extra);
        let v = crate::qemu::resolve::testing::vm(&format!(
            "vm \"t\" {{ template = \"scratch\" arch = \"x86_64\" disk = 10GiB {body} }}"
        ));
        crate::qemu::resolve_vm(&v, None, &crate::profiles::ProfileSet::of(profiles)).unwrap()
    }

    /// The `q35-seabios-linux-modern` shape: linux-modern on SeaBIOS, as the
    /// Ubuntu 26.04 and AlmaLinux 10 templates declare it.
    fn seabios_modern() -> Resolved {
        declared(r#"profile = "linux-modern" firmware = "seabios""#, None)
    }

    fn at(device: u8) -> Option<PciAddress> {
        Some(PciAddress {
            bus: 0,
            device,
            function: 0,
        })
    }

    /// vmlab-hv's `docs/hwref/q35-seabios-linux-modern` capture of QEMU's
    /// q35 for the same argv: virtio-serial 00:01.0, virtio-blk 00:02.0,
    /// virtio-net 00:03.0, virtio-vga 00:04.0; the tablet's xHCI takes the
    /// next slot (HV phase-2 D6), as in vmlab-hv's S1 acceptance test.
    #[test]
    fn a_seabios_linux_vm_takes_the_slots_qemu_gives_it() {
        let d = vm("l", &seabios_modern(), &vm_paths(), QemuAccel::Kvm, &roms()).unwrap();
        assert_eq!(d.machine, Machine::Q35);
        assert_eq!(d.name, "vmlab:l/t");
        assert_eq!((d.vcpus, d.memory_mib), (2, 4096));
        assert_eq!(d.cpu, CpuPolicy::Host);
        assert_eq!(d.console_pci, at(1));
        assert_eq!(d.disks[0].pci, at(2));
        assert_eq!(d.nics[0].pci, at(3));
        let display = d.display.as_ref().unwrap();
        assert_eq!(display.device, DisplayDevice::VirtioVga);
        assert_eq!(display.pci, at(4).unwrap());
        assert_eq!(
            display.rom.as_deref(),
            Some(Path::new("/usr/share/qemu/vgabios-virtio.bin"))
        );
        assert_eq!(d.tablet, at(5));
        assert_eq!(d.ahci_pci, Some(AHCI));
        // The sockets every consumer looks for, named explicitly.
        assert_eq!(
            display.socket.as_deref(),
            Some(Path::new("/run/l/v/display.sock"))
        );
        assert_eq!(
            display.vnc_socket.as_deref(),
            Some(Path::new("/run/l/v/vnc.sock"))
        );

        // SeaBIOS from the host, booting the disk the capture's bootorder
        // names.
        assert_eq!(
            d.boot,
            Boot::Firmware {
                firmware: FirmwareKind::Seabios,
                code: "/usr/share/qemu/bios-256k.bin".into(),
                vars: None,
                secure_boot: false,
                bootorder: vec!["/pci@i0cf8/scsi@2/disk@0,0".into()],
            }
        );
        // disk0 is the linked clone and carries the vmstate.
        assert_eq!(d.disks.len(), 1);
        assert_eq!(d.disks[0].id, "disk0");
        assert!(d.disks[0].vmstate && !d.disks[0].read_only);
        assert_eq!(d.disks[0].format, DiskFormat::Qcow2);
        assert_eq!(
            d.disks[0].path.as_deref(),
            Some(Path::new("/lab/.vmlab/vms/v/disk0.qcow2"))
        );
        // The agent port, the NIC's MAC, socket and iPXE ROM.
        assert_eq!(d.ports.len(), 1);
        assert_eq!(d.ports[0].name, "vmlab.agent.0");
        assert_eq!(d.nics[0].mac, "52:54:00:00:00:01");
        assert_eq!(d.nics[0].mtu, None);
        assert_eq!(
            d.nics[0].rom.as_deref(),
            Some(Path::new("/fw/x86_64/1af41000.rom"))
        );
        assert!(d.shares.is_empty() && d.tpm.is_none() && d.hyperv.is_empty());
    }

    /// QEMU's q35 creates its default std VGA with the machine, ahead of
    /// every `-device`, so a profile naming no display shifts the rest by
    /// one; one naming `std` puts the VGA in argv order.
    #[test]
    fn std_vga_sits_where_qemu_puts_it() {
        let r = declared(
            r#"profile = "linux-modern" firmware = "seabios" display = "std""#,
            None,
        );
        let d = vm("l", &r, &vm_paths(), QemuAccel::Kvm, &roms()).unwrap();
        let display = d.display.unwrap();
        assert_eq!(display.device, DisplayDevice::StdVga);
        assert_eq!(display.pci, at(4).unwrap());
        assert_eq!(
            display.rom.as_deref(),
            Some(Path::new("/usr/share/qemu/vgabios-stdvga.bin"))
        );

        // A profile that names no display.
        let silent = crate::profiles::Profile {
            name: "silent".into(),
            machine: Some(crate::profiles::Machine::Q35),
            firmware: Some(crate::profiles::FirmwareKind::Seabios),
            ..Default::default()
        };
        let r = declared(r#"profile = "silent""#, Some(silent));
        assert_eq!(r.display_device, None);
        let d = vm("l", &r, &vm_paths(), QemuAccel::Kvm, &roms()).unwrap();
        assert_eq!(d.display.unwrap().pci, at(1).unwrap());
        assert_eq!(
            (d.console_pci, d.disks[0].pci, d.nics[0].pci, d.tablet),
            (at(2), at(3), at(4), at(5))
        );
    }

    /// AHCI disks take the ports from 0 and the CD-ROMs carry on after
    /// them, as `ide_slot` places them under QEMU; SeaBIOS boots the
    /// install media first (`bootindex` 0…), then the disks (50…).
    #[test]
    fn ahci_disks_cdroms_and_the_boot_order() {
        let sata = crate::profiles::Profile {
            name: "sata".into(),
            machine: Some(crate::profiles::Machine::Q35),
            firmware: Some(crate::profiles::FirmwareKind::Seabios),
            disk_bus: Some(crate::profiles::DiskBus::Sata),
            display: Some("std".into()),
            ..Default::default()
        };
        let r = declared(r#"profile = "sata""#, Some(sata));
        let mut p = vm_paths();
        p.extra_disks = vec![("data".into(), "/lab/.vmlab/vms/v/data.qcow2".into())];
        p.cdroms = vec!["/lab/install.iso".into(), "/lab/.vmlab/media/m.iso".into()];
        let d = vm("l", &r, &p, QemuAccel::Kvm, &roms()).unwrap();
        let disks: Vec<_> = d
            .disks
            .iter()
            .map(|d| (d.id.as_str(), d.bus, d.port, d.cdrom, d.pci, d.vmstate))
            .collect();
        assert_eq!(
            disks,
            vec![
                ("disk0", DiskBus::Ahci, Some(0), false, None, true),
                ("disk1", DiskBus::Ahci, Some(1), false, None, false),
                ("cd0", DiskBus::Ahci, Some(2), true, None, false),
                ("cd1", DiskBus::Ahci, Some(3), true, None, false),
            ]
        );
        assert!(d.disks[2].read_only && d.disks[2].format == DiskFormat::Raw);
        // With no virtio disks the NIC moves up a slot.
        assert_eq!(d.nics[0].pci, at(2));
        let Boot::Firmware { bootorder, .. } = &d.boot else {
            panic!("{:?}", d.boot)
        };
        assert_eq!(
            bootorder,
            &[
                "/pci@i0cf8/pci8086,2922@1f,2/drive@2/disk@0",
                "/pci@i0cf8/pci8086,2922@1f,2/drive@3/disk@0",
                "/pci@i0cf8/pci8086,2922@1f,2/drive@0/disk@0",
                "/pci@i0cf8/pci8086,2922@1f,2/drive@1/disk@0",
            ]
        );
    }

    /// Two virtio disks take consecutive slots; the extras follow disk0 in
    /// the boot order. A jumbo MTU, a fixed clock and `nested` pass through.
    #[test]
    fn virtio_extras_mtu_clock_and_nested() {
        let r = declared(
            r#"profile = "linux-modern" firmware = "seabios" nested = true clock = "2020-01-02T03:04:05+02:00""#,
            None,
        );
        let mut p = vm_paths();
        p.extra_disks = vec![("data".into(), "/lab/.vmlab/vms/v/data.qcow2".into())];
        p.nics[0].mtu = Some(9000);
        let d = vm("l", &r, &p, QemuAccel::Kvm, &roms()).unwrap();
        assert_eq!(
            (d.disks[0].pci, d.disks[1].pci, d.nics[0].pci),
            (at(2), at(3), at(4))
        );
        assert_eq!(d.nics[0].mtu, Some(9000));
        assert_eq!(d.cpu, CpuPolicy::Nested);
        assert_eq!(d.rtc_base.as_deref(), Some("2020-01-02T01:04:05Z"));
        let Boot::Firmware { bootorder, .. } = &d.boot else {
            panic!()
        };
        assert_eq!(
            bootorder,
            &["/pci@i0cf8/scsi@2/disk@0,0", "/pci@i0cf8/scsi@3/disk@0,0"]
        );
    }

    /// Without an iPXE build the NICs carry no option ROM: no network boot.
    #[test]
    fn no_ipxe_build_means_no_nic_rom() {
        let roms = HostRoms {
            ipxe_virtio: None,
            ..roms()
        };
        let d = vm("l", &seabios_modern(), &vm_paths(), QemuAccel::Kvm, &roms).unwrap();
        assert_eq!(d.nics[0].rom, None);
    }

    /// What the launch alone knows, refused before a spawn.
    #[test]
    fn what_the_launch_cannot_describe_is_refused() {
        let refused = |r: &ResolvedVm, p: &VmPaths, accel, roms: &HostRoms| {
            format!("{:#}", vm("l", r, p, accel, roms).unwrap_err())
        };
        let ok = seabios_modern();
        // OVMF without the paths the lab daemon assembles for it.
        let ovmf = resolved_vm("linux-modern", "x86_64");
        assert!(refused(&ovmf, &vm_paths(), QemuAccel::Kvm, &roms()).contains("VARS"));
        assert!(refused(&ok, &vm_paths(), QemuAccel::Tcg, &roms()).contains("KVM"));
        let no_bios = HostRoms {
            seabios: None,
            ..roms()
        };
        assert!(refused(&ok, &vm_paths(), QemuAccel::Kvm, &no_bios).contains("bios-256k.bin"));
        let mut tap = vm_paths();
        tap.nics[0].backend = NicBackend::Tap { child_fd: 10 };
        assert!(refused(&ok, &tap, QemuAccel::Kvm, &roms()).contains("tap"));
        let legacy = resolved_vm("windows-legacy", "x86_64");
        assert!(refused(&legacy, &vm_paths(), QemuAccel::Kvm, &roms()).contains("q35"));
        let qxl = declared(
            r#"profile = "linux-modern" firmware = "seabios" display = "qxl""#,
            None,
        );
        assert!(refused(&qxl, &vm_paths(), QemuAccel::Kvm, &roms()).contains("qxl-vga"));
    }

    /// The file vmlab-hv parses with `deny_unknown_fields`.
    #[test]
    fn a_vm_description_round_trips_as_json() {
        let d = vm("l", &seabios_modern(), &vm_paths(), QemuAccel::Kvm, &roms()).unwrap();
        let json = serde_json::to_string(&d).unwrap();
        let back: MachineDescription = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        assert!(json.contains(r#""machine":"q35""#), "{json}");
    }

    // -- OVMF, secure boot, TPM, Hyper-V (phase 2c) -----------------------

    /// The paths the lab daemon assembles for an OVMF VM: the CODE image its
    /// firmware lookup chose, the VM's own VARS copy, and swtpm's socket.
    fn ovmf_paths(tpm: bool) -> VmPaths {
        VmPaths {
            firmware_code: Some("/fw/x86_64/OVMF_CODE_4M.secboot.fd".into()),
            ovmf_vars: Some("/lab/.vmlab/vms/v/OVMF_VARS.fd".into()),
            tpm_sock: tpm.then(|| "/run/l/v/tpm.sock".into()),
            ..vm_paths()
        }
    }

    const QEMU_HYPERV: [HyperV; 12] = [
        HyperV::Relaxed,
        HyperV::Vapic,
        HyperV::Spinlocks,
        HyperV::Vpindex,
        HyperV::Runtime,
        HyperV::Time,
        HyperV::Synic,
        HyperV::Stimer,
        HyperV::Frequencies,
        HyperV::Reset,
        HyperV::Tlbflush,
        HyperV::Ipi,
    ];

    /// Windows 11 as QEMU runs it: OVMF's secure-boot CODE with the VM's own
    /// VARS, secure boot on, the TPM on swtpm's socket, QEMU's enlightenments
    /// in QEMU's order, no `bootorder` (OVMF keeps its own), and the EFI iPXE
    /// image on the NIC. The slots are SeaBIOS's: firmware changes none.
    #[test]
    fn windows_11_is_ovmf_secure_boot_tpm_and_hyperv() {
        let r = declared(r#"profile = "windows-11""#, None);
        let d = vm("l", &r, &ovmf_paths(true), QemuAccel::Kvm, &roms()).unwrap();
        assert_eq!(
            d.boot,
            Boot::Firmware {
                firmware: FirmwareKind::Ovmf,
                code: "/fw/x86_64/OVMF_CODE_4M.secboot.fd".into(),
                vars: Some("/lab/.vmlab/vms/v/OVMF_VARS.fd".into()),
                secure_boot: true,
                bootorder: Vec::new(),
            }
        );
        assert_eq!(
            d.tpm,
            Some(Tpm {
                socket: "/run/l/v/tpm.sock".into()
            })
        );
        assert_eq!(d.hyperv, QEMU_HYPERV.to_vec());
        assert_eq!(
            d.nics[0].rom.as_deref(),
            Some(Path::new("/fw/x86_64/virtio-net.rom"))
        );
        assert_eq!(
            (d.console_pci, d.disks[0].pci, d.nics[0].pci, d.tablet),
            (at(1), at(2), at(3), at(5))
        );
        assert_eq!(d.display.unwrap().pci, at(4).unwrap());
    }

    /// The APICv/AVIC host adds `avic`, as QEMU's `-cpu` gains `hv-avic`.
    #[test]
    fn avic_follows_the_hosts_apic_virtualization() {
        let r = declared(r#"profile = "windows-10""#, None);
        let mut p = ovmf_paths(false);
        p.apic_virtualization = true;
        let d = vm("l", &r, &p, QemuAccel::Kvm, &roms()).unwrap();
        let mut want = QEMU_HYPERV.to_vec();
        want.push(HyperV::Avic);
        assert_eq!(d.hyperv, want);
        // Windows 10: no TPM, no secure boot.
        assert_eq!(d.tpm, None);
        let Boot::Firmware { secure_boot, .. } = d.boot else {
            panic!()
        };
        assert!(!secure_boot);
    }

    /// The QEMU flags are the one list: each maps onto an enlightenment
    /// vmlab-hv knows, so a flag added there cannot be dropped silently here.
    #[test]
    fn every_qemu_enlightenment_maps_onto_vmlab_hv() {
        let r = declared(r#"profile = "windows-10""#, None);
        assert_eq!(hyperv_list(&r, false).unwrap().len(), QEMU_HYPERV.len());
        // A Linux profile asks for none.
        let linux = declared(r#"profile = "linux-modern""#, None);
        assert!(hyperv_list(&linux, true).unwrap().is_empty());
    }

    /// UEFI Linux without secure boot: OVMF's plain CODE, the VM's VARS, no
    /// TPM, no enlightenments; `secure_boot = true` on the VM turns it on.
    #[test]
    fn uefi_linux_with_and_without_secure_boot() {
        let r = declared(r#"profile = "linux-modern""#, None);
        let mut p = ovmf_paths(false);
        p.firmware_code = Some("/fw/x86_64/OVMF_CODE_4M.fd".into());
        let d = vm("l", &r, &p, QemuAccel::Kvm, &roms()).unwrap();
        let Boot::Firmware {
            firmware,
            code,
            secure_boot,
            ..
        } = &d.boot
        else {
            panic!()
        };
        assert_eq!(*firmware, FirmwareKind::Ovmf);
        assert_eq!(code, &PathBuf::from("/fw/x86_64/OVMF_CODE_4M.fd"));
        assert!(!secure_boot && d.hyperv.is_empty() && d.tpm.is_none());

        let r = declared(r#"profile = "linux-modern" secure_boot = true"#, None);
        let d = vm("l", &r, &ovmf_paths(false), QemuAccel::Kvm, &roms()).unwrap();
        let Boot::Firmware { secure_boot, .. } = d.boot else {
            panic!()
        };
        assert!(secure_boot);
    }
}
