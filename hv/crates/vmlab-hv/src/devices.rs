//! The real device wiring: builds the devices `vmlab-hv-devices` provides
//! from a machine description. One seam, one module; the VMM's builder does
//! the placement.

use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;

use vmlab_hv_devices::serial::{Serial, SerialOut};
use vmlab_hv_devices::virtio::block::Block;
use vmlab_hv_devices::virtio::console::{Console, PortConfig};
use vmlab_hv_devices::virtio::fs::{Fs, FsConfig, InProcessFs, InProcessFsConfig};
use vmlab_hv_devices::virtio::net::{Net, NetBackendRegistry, NetConfig};
use vmlab_hv_devices::virtio::pci::VirtioPciDevice;
use vmlab_hv_devices::virtio::worker::QueueWorker;
use vmlab_hv_devices::{GuestMemory, IrqLine, VirtioDevice};
use vmlab_hv_proto::machine::{Disk, FsBacking, FsShare, Nic, NicAttachment, SerialPort};
use vmlab_hv_vmm::DeviceUnavailable;
use vmlab_hv_vmm::factory::{BuiltBlk, BuiltPci, BuiltSerial, SharedDisk};
use vmlab_hv_vmm::serial_log::{CappedLog, SERIAL_LOG_CAP};

/// The device set the binary ships.
#[derive(Debug, Default)]
pub struct DeviceSet {
    pub network: NetBackendRegistry,
}

impl DeviceSet {
    fn wrap(
        &self,
        id: String,
        device: Box<dyn VirtioDevice>,
        mem: &GuestMemory,
        msi: vmlab_hv_vmm::factory::PciWiring,
        rom: Option<&Path>,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        let mut transport = if msi.transitional {
            VirtioPciDevice::new_transitional(
                id,
                device,
                mem.clone(),
                Some(msi.msi),
                msi.intx,
                msi.msix_vectors,
            )
            .map_err(|e| DeviceUnavailable::new("virtio-pci", e.to_string()))?
        } else {
            VirtioPciDevice::new(id, device, mem.clone(), Some(msi.msi), msi.intx)
        };
        if let Some(doorbells) = msi.doorbells {
            transport.set_queue_doorbells(doorbells);
        }
        let transport = if let Some(path) = rom {
            let image = std::fs::read(path).map_err(|e| {
                DeviceUnavailable::new("virtio-net ROM", format!("{}: {e}", path.display()))
            })?;
            transport
                .with_option_rom(&image)
                .map_err(|e| DeviceUnavailable::new("virtio-net ROM", e.to_string()))?
        } else {
            transport
        };
        let dev = Arc::new(Mutex::new(transport));
        Ok(BuiltPci {
            function: dev.clone(),
            snapshot: Some(dev),
        })
    }
}

impl vmlab_hv_vmm::DeviceFactory for DeviceSet {
    fn serial(
        &self,
        log_path: &Path,
        irq: Arc<dyn IrqLine>,
    ) -> Result<BuiltSerial, DeviceUnavailable> {
        // The guest decides how much lands here, so the file has a cap.
        let file = CappedLog::open(log_path, SERIAL_LOG_CAP).map_err(|e| {
            DeviceUnavailable::new(
                "serial-16550a",
                format!("cannot open serial log {}: {e}", log_path.display()),
            )
        })?;
        let dev = Arc::new(Mutex::new(Serial::new(
            "serial0",
            SerialOut::new(Box::new(file)),
            irq,
        )));
        Ok(BuiltSerial {
            clock: Some(dev.clone()),
            device: dev.clone(),
            snapshot: Some(dev),
        })
    }

    fn virtio_blk(
        &self,
        disk: &Disk,
        mem: &GuestMemory,
        msi: vmlab_hv_vmm::factory::PciWiring,
    ) -> Result<BuiltBlk, DeviceUnavailable> {
        // One open image shared by the device and the VMM's snapshot
        // orchestration (D14): the block device runs on a SharedDisk
        // forwarding to the same handle the registry keeps.
        let image = vmlab_hv_vmm::factory::open_image(disk)?;
        let block = Block::new(disk.id.clone(), Box::new(SharedDisk(image.clone())))
            .with_serial(&disk_serial(disk));
        let block = if msi.transitional {
            block
                .with_num_queues(msi.msix_vectors.saturating_sub(1))
                .map_err(|e| DeviceUnavailable::new("virtio-blk", e.to_string()))?
        } else {
            block
        };
        let block = QueueWorker::new(Box::new(block))
            .map_err(|e| DeviceUnavailable::new("virtio-blk worker", e.to_string()))?;
        let pci = self.wrap(disk.id.clone(), Box::new(block), mem, msi, None)?;
        Ok(BuiltBlk { pci, disk: image })
    }

    fn virtio_console(
        &self,
        ports: &[SerialPort],
        mem: &GuestMemory,
        msi: vmlab_hv_vmm::factory::PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        let ports = ports
            .iter()
            .map(|p| PortConfig {
                name: p.name.clone(),
                path: p.socket.clone(),
            })
            .collect();
        let console = Console::new("console0", ports).map_err(|e| {
            DeviceUnavailable::new(
                "virtio-console",
                format!("cannot listen on port sockets: {e}"),
            )
        })?;
        self.wrap("console0".into(), Box::new(console), mem, msi, None)
    }

    fn virtio_net(
        &self,
        nic: &Nic,
        mem: &GuestMemory,
        msi: vmlab_hv_vmm::factory::PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        let mac = parse_mac(&nic.mac)?;
        let id = format!("nic-{}", nic.mac);
        let switch = match &nic.attachment {
            NicAttachment::Stream { socket } | NicAttachment::StreamVnet { socket, .. } => {
                socket.clone()
            }
            NicAttachment::Tap { .. } | NicAttachment::TapRaw { .. } => Default::default(),
        };
        let config = NetConfig {
            id: id.clone(),
            mac,
            mtu: nic.mtu.unwrap_or(1500),
            switch,
        };
        let net = match nic.attachment {
            NicAttachment::Stream { .. } => Net::new(config),
            NicAttachment::StreamVnet { .. } => {
                Net::with_offload(config, nic.attachment.queue_pairs())
            }
            NicAttachment::Tap { .. } | NicAttachment::TapRaw { .. } => {
                let build = if matches!(nic.attachment, NicAttachment::TapRaw { .. }) {
                    Net::with_tap_raw
                } else {
                    Net::with_tap
                };
                let (device, handle) = build(config, nic.attachment.queue_pairs(), mem.clone())
                    .map_err(|e| DeviceUnavailable::new("virtio-net TAP", e.to_string()))?;
                self.network
                    .register(nic.mac.clone(), handle)
                    .map_err(|e| DeviceUnavailable::new("virtio-net TAP", e.to_string()))?;
                Ok(device)
            }
        }
        .map_err(|e| DeviceUnavailable::new("virtio-net", format!("cannot connect: {e}")))?;
        self.wrap(id, Box::new(net), mem, msi, nic.rom.as_deref())
    }

    fn fs_share(
        &self,
        share: &FsShare,
        mem: &GuestMemory,
        msi: vmlab_hv_vmm::factory::PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        let id = format!("fs-{}", share.tag);
        match share.backing() {
            // An external virtiofsd over vhost-user.
            Some(FsBacking::Socket(socket)) => {
                let fs = Fs::new(
                    id.clone(),
                    FsConfig {
                        tag: share.tag.clone(),
                        socket,
                    },
                )
                .map_err(|e| {
                    DeviceUnavailable::new("vhost-user-fs", format!("share `{}`: {e}", share.tag))
                })?;
                self.wrap(id, Box::new(fs), mem, msi, None)
            }
            // The in-process device rooted at the host directory.
            Some(FsBacking::Directory(path)) => {
                let fs = InProcessFs::new(
                    id.clone(),
                    InProcessFsConfig {
                        tag: share.tag.clone(),
                        path,
                        read_only: share.read_only,
                    },
                )
                .map_err(|e| {
                    DeviceUnavailable::new("virtio-fs", format!("share `{}`: {e}", share.tag))
                })?;
                self.wrap(id, Box::new(fs), mem, msi, None)
            }
            None => Err(DeviceUnavailable::new(
                "virtio-fs",
                format!(
                    "share `{}` must set exactly one of `socket` and `path`",
                    share.tag
                ),
            )),
        }
    }
}

/// `aa:bb:cc:dd:ee:ff` to bytes.
fn parse_mac(mac: &str) -> Result<[u8; 6], DeviceUnavailable> {
    let bytes = mac
        .split(':')
        .map(|part| u8::from_str_radix(part, 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DeviceUnavailable::new("virtio-net", format!("bad MAC {mac:?}: {e}")))?;
    let bytes: [u8; 6] = bytes
        .try_into()
        .map_err(|_| DeviceUnavailable::new("virtio-net", format!("bad MAC {mac:?}")))?;
    Ok(bytes)
}

/// The serial a virtio-blk disk reports (GitHub #1026).
///
/// vmlab's QEMU backend set no `serial=`, so QEMU answered `GET_ID` with an
/// empty string, and the Windows driver (viostor's `ReportDeviceIdentifier`)
/// then made the disk's EUI-64 from its PCI bus and slot, one nibble a byte:
/// the disk at 00:02.0 was `0000000000000002`, the one at 00:03.0
/// `0000000000000003`. A placed disk reports that string as its serial, so a
/// lab that selects a disk by it moves between the backends unchanged, and
/// Linux gets `/dev/disk/by-id/virtio-<serial>` as well. The slot number is
/// Windows' `PCI_SLOT_NUMBER`: the device in bits 0 to 4, the function in 5
/// to 7. A disk the VMM places itself (a container's) reports its id, unique
/// within a machine and the same at each start.
pub fn disk_serial(disk: &Disk) -> String {
    match disk.pci {
        Some(pci) => {
            let slot = u16::from(pci.device & 0x1f) | (u16::from(pci.function & 0x7) << 5);
            let bus = u16::from(pci.bus);
            [
                bus >> 12,
                bus >> 8,
                bus >> 4,
                bus,
                slot >> 12,
                slot >> 8,
                slot >> 4,
                slot,
            ]
            .iter()
            .map(|nibble| format!("{:02X}", nibble & 0xf))
            .collect()
        }
        None => disk.id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmlab_hv_vmm::DeviceFactory;

    struct Msi;
    impl vmlab_hv_devices::MsiSink for Msi {
        fn signal(&self, _: vmlab_hv_devices::MsiMessage) {}
    }

    fn disk_at(id: &str, pci: Option<(u8, u8, u8)>) -> Disk {
        Disk {
            channel: None,
            unit: None,
            pci: pci.map(
                |(bus, device, function)| vmlab_hv_proto::machine::PciAddress {
                    bus,
                    device,
                    function,
                },
            ),
            bus: Default::default(),
            port: None,
            cdrom: false,
            id: id.into(),
            path: None,
            format: vmlab_hv_proto::machine::DiskFormat::Qcow2,
            read_only: false,
            vmstate: false,
        }
    }

    /// The boot disk at 00:02.0 and the data disks after it report what a
    /// Windows guest saw for them on QEMU (GitHub #1026).
    #[test]
    fn a_placed_disk_reports_the_qemu_form_of_its_slot() {
        let serials: Vec<String> = (2..=5)
            .map(|slot| disk_serial(&disk_at(&format!("disk{}", slot - 2), Some((0, slot, 0)))))
            .collect();
        assert_eq!(
            serials,
            [
                "0000000000000002",
                "0000000000000003",
                "0000000000000004",
                "0000000000000005"
            ]
        );
        assert!(serials.iter().all(|s| s.len() <= 20));
    }

    /// Every nibble of the bus and of the slot number is one byte, as
    /// viostor writes the EUI-64; the function sits in bits 5 to 7.
    #[test]
    fn the_serial_spells_each_nibble_of_bus_and_slot() {
        assert_eq!(
            disk_serial(&disk_at("d", Some((0, 0x1f, 0)))),
            "000000000000010F"
        );
        assert_eq!(
            disk_serial(&disk_at("d", Some((0, 2, 1)))),
            "0000000000000202"
        );
        assert_eq!(
            disk_serial(&disk_at("d", Some((0x12, 2, 0)))),
            "0000010200000002"
        );
    }

    /// A disk the VMM places reports its id: one per disk, and the same at
    /// every start.
    #[test]
    fn an_unplaced_disk_reports_its_id() {
        assert_eq!(disk_serial(&disk_at("rootfs", None)), "rootfs");
        assert_ne!(
            disk_serial(&disk_at("rootfs", None)),
            disk_serial(&disk_at("scratch", None))
        );
    }

    #[test]
    fn block_multiqueue_policy_preserves_container_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.raw");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(65536)
            .unwrap();
        let disk: Disk = serde_json::from_value(serde_json::json!({
            "id":"test", "path":path, "format":"raw", "read_only":false
        }))
        .unwrap();
        let mem = GuestMemory::new();
        for (transitional, expected) in [(false, 0u16), (true, 2)] {
            let built = DeviceSet::default()
                .virtio_blk(
                    &disk,
                    &mem,
                    vmlab_hv_vmm::factory::PciWiring {
                        msi: Arc::new(Msi),
                        intx: None,
                        transitional,
                        msix_vectors: 3,
                        doorbells: None,
                    },
                )
                .unwrap();
            let mut count = [0; 2];
            built.pci.function.lock().bar_read(
                4,
                if transitional { 0x2022 } else { 0x3022 },
                &mut count,
            );
            assert_eq!(u16::from_le_bytes(count), expected);
        }
    }
}
