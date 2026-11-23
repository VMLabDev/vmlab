use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use super::*;

#[derive(Default)]
struct Interrupts {
    level: AtomicBool,
    messages: Mutex<Vec<MsiMessage>>,
}
impl IrqLine for Interrupts {
    fn set_level(&self, high: bool) {
        self.level.store(high, Ordering::SeqCst);
    }
}
impl MsiSink for Interrupts {
    fn signal(&self, message: MsiMessage) {
        self.messages.lock().unwrap().push(message);
    }
}

struct MemoryDisk {
    data: Vec<u8>,
    readonly: bool,
    flushes: Arc<Mutex<usize>>,
}
impl DiskImage for MemoryDisk {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_only(&self) -> bool {
        self.readonly
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> vmlab_disk::Result<()> {
        let end = offset as usize + buf.len();
        let bytes = self
            .data
            .get(offset as usize..end)
            .ok_or_else(|| vmlab_disk::Error::Unsupported("test disk range".into()))?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> vmlab_disk::Result<()> {
        if self.readonly {
            return Err(vmlab_disk::Error::ReadOnly);
        }
        self.data[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> vmlab_disk::Result<()> {
        *self.flushes.lock().unwrap() += 1;
        Ok(())
    }
    fn discard(&mut self, _: u64, _: u64) -> vmlab_disk::Result<()> {
        Ok(())
    }
    fn snapshots(&self) -> vmlab_disk::Result<Vec<vmlab_disk::SnapshotInfo>> {
        Ok(Vec::new())
    }
    fn snapshot_create(&mut self, _: &str, _: u64, _: u64) -> vmlab_disk::Result<()> {
        Err(vmlab_disk::Error::Unsupported("test disk snapshots".into()))
    }
    fn snapshot_apply(&mut self, _: &str) -> vmlab_disk::Result<vmlab_disk::SnapshotInfo> {
        Err(vmlab_disk::Error::Unsupported("test disk snapshots".into()))
    }
    fn snapshot_delete(&mut self, _: &str) -> vmlab_disk::Result<()> {
        Err(vmlab_disk::Error::Unsupported("test disk snapshots".into()))
    }
    fn write_vmstate(&mut self, _: u64, _: &[u8]) -> vmlab_disk::Result<()> {
        Err(vmlab_disk::Error::Unsupported("test disk vmstate".into()))
    }
    fn read_vmstate(&mut self, _: u64, _: &mut [u8]) -> vmlab_disk::Result<()> {
        Err(vmlab_disk::Error::Unsupported("test disk vmstate".into()))
    }
}
fn image(data: Vec<u8>, readonly: bool) -> Box<dyn DiskImage> {
    Box::new(MemoryDisk {
        data,
        readonly,
        flushes: Arc::default(),
    })
}

struct Driver {
    hba: Ahci,
    memory: GuestMemory,
    irq: Arc<Interrupts>,
}
impl Driver {
    fn new() -> Self {
        let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
        let irq = Arc::new(Interrupts::default());
        let hba = Ahci::new("ahci-00:1f.2", memory.clone(), irq.clone(), irq.clone());
        Self { hba, memory, irq }
    }
    /// A register write, then every command it started, as the tests that
    /// read a result on the next line expect.
    fn write(&mut self, offset: usize, value: u32) {
        self.write_only(offset, value);
        self.hba.wait_idle();
    }
    /// A register write alone, as a vCPU performs it.
    fn write_only(&mut self, offset: usize, value: u32) {
        self.hba.bar_write(5, offset as u64, &value.to_le_bytes());
    }
    fn read(&mut self, offset: usize) -> u32 {
        let mut bytes = [0; 4];
        self.hba.bar_read(5, offset as u64, &mut bytes);
        le32(&bytes)
    }
    fn put(&self, address: u64, bytes: &[u8]) {
        self.memory
            .write_slice(bytes, GuestAddress(address))
            .unwrap();
    }
    fn bytes(&self, address: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0; len];
        self.memory
            .read_slice(&mut bytes, GuestAddress(address))
            .unwrap();
        bytes
    }
    fn start(&mut self, port: usize) {
        self.hba.config_write(1, 0, &[6]);
        self.write(4, 0x8000_0001);
        assert_eq!(self.read(4), 0x8000_0000);
        self.write(4, 0x8000_0002);
        let base = port_base(port);
        self.write(base, 0x1000);
        self.write(base + 8, 0x2000);
        self.write(base + 0x14, u32::MAX);
        self.write(base + 0x18, 0x10);
        self.write(base + 0x18, 0x11);
        self.ack(port);
    }
    fn ack(&mut self, port: usize) {
        let cmd = self.read(port_base(port) + 0x18);
        let halted = cmd & 0x8001 == 1;
        if halted {
            self.write(port_base(port) + 0x18, cmd & !1);
        }
        self.write(port_base(port) + 0x10, u32::MAX);
        self.write(8, u32::MAX);
        if halted {
            self.write(port_base(port) + 0x18, cmd | 1);
        }
    }
    fn prepare(
        &mut self,
        command: u8,
        flags: u16,
        lba: u64,
        sectors: u16,
        packet: Option<[u8; 16]>,
        prds: &[(u64, usize, bool)],
    ) {
        let mut header = [0; 32];
        header[..2].copy_from_slice(&(5 | flags).to_le_bytes());
        header[2..4].copy_from_slice(&(prds.len() as u16).to_le_bytes());
        header[8..16].copy_from_slice(&0x3000u64.to_le_bytes());
        self.put(0x1000, &header);
        let mut fis = [0; 64];
        fis[0] = 0x27;
        fis[1] = 0x80;
        fis[2] = command;
        fis[3] = u8::from(command == 0xa0);
        fis[4] = lba as u8;
        fis[5] = (lba >> 8) as u8;
        fis[6] = (lba >> 16) as u8;
        fis[7] = 0x40;
        fis[8] = (lba >> 24) as u8;
        fis[9] = (lba >> 32) as u8;
        fis[10] = (lba >> 40) as u8;
        fis[12..14].copy_from_slice(&sectors.to_le_bytes());
        self.put(0x3000, &fis);
        if let Some(cdb) = packet {
            self.put(0x3040, &cdb);
        }
        for (i, &(address, len, ioc)) in prds.iter().enumerate() {
            let mut prd = [0; 16];
            prd[..8].copy_from_slice(&address.to_le_bytes());
            prd[12..16]
                .copy_from_slice(&((len as u32 - 1) | if ioc { 1 << 31 } else { 0 }).to_le_bytes());
            self.put(0x3080 + i as u64 * 16, &prd);
        }
    }
    fn issue(&mut self, port: usize) -> u32 {
        self.write(port_base(port) + 0x38, 1);
        let errors = self.read(port_base(port) + 0x10) & (TFES | HBFS);
        assert_eq!(self.read(port_base(port) + 0x38), u32::from(errors != 0));
        self.read(port_base(port) + 0x20)
    }
    fn command(&mut self, command: u8, lba: u64, sectors: u16, write: bool) -> u32 {
        self.ack(0);
        self.prepare(
            command,
            if write { 0x40 } else { 0 },
            lba,
            sectors,
            None,
            &[(0x4000, 4096, true)],
        );
        self.issue(0)
    }
    fn packet(&mut self, cdb: &[u8]) -> (u32, Vec<u8>) {
        self.ack(0);
        let mut packet = [0; 16];
        packet[..cdb.len()].copy_from_slice(cdb);
        self.prepare(0xa0, 0x20, 0, 0, Some(packet), &[(0x4000, 8192, false)]);
        let status = self.issue(0);
        let len = le32(&self.bytes(0x1004, 4));
        (status, self.bytes(0x4000, len as usize))
    }
    fn cdrom() -> Self {
        let mut driver = Self::new();
        let mut iso = vec![0; 64 * 2048];
        iso[16 * 2048..16 * 2048 + 7].copy_from_slice(b"\x01CD001\x01");
        driver.hba.attach_cdrom(0, Some(image(iso, true))).unwrap();
        driver.start(0);
        driver
    }
}

#[test]
fn atapi_identify_identity_and_transfer_mode_match_qtest_observations() {
    let mut d = Driver::cdrom();
    d.prepare(0xa1, 0, 0, 0, None, &[(0x4000, 512, false)]);
    assert_eq!(d.issue(0), 0x50);
    let identify = d.bytes(0x4000, 512);
    let word =
        |index: usize| u16::from_le_bytes(identify[index * 2..index * 2 + 2].try_into().unwrap());
    for (index, value) in [
        (0, 0x85c0),
        (20, 3),
        (21, 512),
        (22, 4),
        (48, 1),
        (65, 180),
        (67, 300),
        (75, 31),
        (76, 0x100),
        (80, 0x1e),
        (88, 0x203f),
    ] {
        assert_eq!(word(index), value, "IDENTIFY PACKET word {index}");
    }
    let decode = |offset: usize, len: usize| {
        identify[offset..offset + len]
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|pair| [pair[1], pair[0]])
            .collect::<Vec<_>>()
    };
    assert_eq!(decode(54, 40), b"QEMU DVD-ROM                            ");
    assert_eq!(decode(46, 8), b"2.5+    ");
    d.ack(0);
    d.prepare(0xef, 0, 0, 0x42, None, &[]);
    d.put(0x3003, &[3]);
    assert_eq!(d.issue(0), 0x50);
    let saved = d.hba.save_state().unwrap();
    d.hba.restore_state(&saved).unwrap();
    d.ack(0);
    d.prepare(0xa1, 0, 0, 0, None, &[(0x4000, 512, false)]);
    assert_eq!(d.issue(0), 0x50);
    assert_eq!(
        u16::from_le_bytes(d.bytes(0x4000 + 88 * 2, 2).try_into().unwrap()),
        0x043f
    );
}

#[test]
fn ncq_read_write_tags_set_device_bits_and_error_log_survive_snapshot() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.start(0);
    assert_ne!(d.read(0) & (1 << 30), 0);
    assert_eq!(d.read(0) & (1 << 24), 0);
    d.put(0x4000, &[0x5a; 512]);
    for (command, flags, tag, lba) in [(0x61, 0x40, 5, 2), (0x60, 0, 13, 2), (0x60, 0, 23, 99)] {
        d.ack(0);
        d.prepare(command, flags, lba, 0, None, &[(0x4000, 512, false)]);
        d.put(0x3003, &[1]);
        d.put(0x300c, &[(tag << 3) as u8]);
        let header = d.bytes(0x1000, 32);
        d.put(0x1000 + tag * 32, &header);
        d.write(port_base(0) + 0x34, 1 << tag);
        d.write(port_base(0) + 0x38, 1 << tag);
        assert_eq!(d.read(port_base(0) + 0x34), 0);
        assert_eq!(d.bytes(0x2058, 1), [0xa1]);
        assert_eq!(le32(&d.bytes(0x205c, 4)), 1 << tag);
        assert_ne!(d.read(port_base(0) + 0x10) & 8, 0);
        if lba == 2 {
            assert_eq!(d.read(port_base(0) + 0x20), 0x50);
            assert_eq!(d.bytes(0x4000, 512), [0x5a; 512]);
        } else {
            assert_ne!(d.read(port_base(0) + 0x10) & TFES, 0);
        }
    }
    let saved = d.hba.save_state().unwrap();
    d.hba.restore_state(&saved).unwrap();
    d.ack(0);
    d.prepare(0x2f, 0, 0x10, 1, None, &[(0x4000, 512, false)]);
    assert_eq!(d.issue(0), 0x50);
    let log = d.bytes(0x4000, 512);
    assert_eq!(log[0], 23);
    assert_eq!(&log[2..5], &[0x51, 0x10, 99]);
    assert_eq!(log.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)), 0);
}

#[test]
fn msi_reports_a_second_port_while_the_first_port_is_pending() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.hba.attach_cdrom(2, None).unwrap();
    d.start(0);
    d.hba.config_write(32, 2, &[0x81]);
    d.prepare(0xec, 0, 0, 0, None, &[(0x4000, 512, false)]);
    d.issue(0);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 1);
    let base = port_base(2);
    d.write(base + 8, 0x2000);
    d.write(base + 0x14, u32::MAX);
    d.write(base + 0x18, 0x10);
    assert_eq!(d.read(8) & 5, 5);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 2);
}

/// The Windows storahci interrupt order with MSI: clear PxIS, issue the next
/// command (vmlab-hv completes it at once), then acknowledge IS. The second
/// completion is still pending in PxIS when IS is acknowledged, so it must
/// raise a new MSI. Without it, every CD-ROM read after the first waited for
/// the driver's timeout, and Windows Setup stalled applying install.wim.
#[test]
fn msi_signals_a_completion_that_lands_before_the_is_acknowledgement() {
    let mut d = Driver::cdrom();
    d.hba.config_write(32, 2, &[0x81]);
    let messages = |d: &Driver| d.irq.messages.lock().unwrap().len();
    let tur = |d: &mut Driver| {
        let mut packet = [0; 16];
        packet[0] = 0; // TEST UNIT READY
        d.prepare(0xa0, 0x20, 0, 0, Some(packet), &[]);
        assert_eq!(d.issue(0), 0x50);
    };
    tur(&mut d);
    assert_eq!(messages(&d), 1);
    let base = port_base(0);
    // The handler clears PxIS, then the next request goes out before IS is
    // acknowledged.
    d.write(base + 0x10, u32::MAX);
    tur(&mut d);
    assert_eq!(messages(&d), 1, "IS is still set: no edge yet");
    assert_ne!(d.read(base + 0x10) & DHRS, 0);
    d.write(8, 1);
    assert_eq!(messages(&d), 2, "the pending completion is signalled");
    assert_eq!(d.read(8), 1);
    // A clean acknowledgement sends nothing more.
    d.write(base + 0x10, u32::MAX);
    d.write(8, 1);
    assert_eq!(messages(&d), 2);
    assert_eq!(d.read(8), 0);
}

/// Windows storahci serves one port for each interrupt and acknowledges only
/// that port's IS bit. Two ports that complete before the handler runs send
/// two MSIs, which the guest's interrupt controller can deliver as one. The
/// acknowledgement of the first port must then send a new MSI for the port
/// that is left. Without it, the second port waited for the next completion
/// on any port, and Windows Setup read its install media at about one command
/// a second (the stall at 5 percent).
#[test]
fn msi_signals_the_ports_left_after_a_partial_is_acknowledgement() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.hba.attach_cdrom(1, None).unwrap();
    d.start(0);
    d.hba.config_write(32, 2, &[0x81]);
    let messages = |d: &Driver| d.irq.messages.lock().unwrap().len();
    d.prepare(0xec, 0, 0, 0, None, &[(0x4000, 512, false)]);
    d.issue(0);
    assert_eq!(messages(&d), 1);
    // Port 1 raises its own interrupt before the handler has run.
    let base = port_base(1);
    d.write(base + 8, 0x2000);
    d.write(base + 0x14, u32::MAX);
    d.write(base + 0x18, 0x10);
    assert_eq!(d.read(8), 3);
    assert_eq!(messages(&d), 2);
    // The handler serves port 0 alone: port 1 is still in IS.
    d.write(port_base(0) + 0x10, u32::MAX);
    d.write(8, 1);
    assert_eq!(d.read(8), 2);
    assert_eq!(messages(&d), 3, "the port left in IS is signalled again");
    // Port 1's own acknowledgement leaves nothing and sends nothing.
    d.write(base + 0x10, u32::MAX);
    d.write(8, 2);
    assert_eq!(d.read(8), 0);
    assert_eq!(messages(&d), 3);
}

#[test]
fn pci_identity_capabilities_and_bar_probes() {
    let mut d = Driver::new();
    assert_eq!(d.hba.config_read(0), 0x2922_8086);
    assert_eq!(d.hba.config_read(2), 0x0106_0102);
    assert_eq!(d.hba.config_read(11), 0x1100_1af4);
    assert_eq!(d.hba.config_read(32), 0x0080_a805);
    assert_eq!(d.hba.config_read(42), 0x0010_0012);
    assert_eq!(d.hba.config_read(43), 0x48);
    assert_eq!(d.hba.bars()[0].index, 4);
    assert_eq!(d.hba.bars()[1].size, 4096);
    d.hba.config_write(8, 0, &u32::MAX.to_le_bytes());
    d.hba.config_write(9, 0, &u32::MAX.to_le_bytes());
    assert_eq!(d.hba.config_read(8), 0xffff_ffe1);
    assert_eq!(d.hba.config_read(9), 0xffff_f000);
    d.hba.config_write(0, 0, &[0; 4]);
    assert_eq!(d.hba.config_read(0), 0x2922_8086);
    assert_eq!(d.hba.config_read(usize::MAX), 0);
}

#[test]
fn hba_reset_six_ports_and_indexed_io() {
    let mut d = Driver::new();
    d.hba.attach_disk(5, image(vec![0; 4096], false)).unwrap();
    d.start(5);
    assert_eq!(d.read(0xc), 0x3f);
    assert_eq!(d.read(0x10), 0x10300);
    assert_eq!(d.read(port_base(5) + 0x24), 0x101);
    assert_eq!(d.read(port_base(4) + 0x28), 0);
    assert_eq!(d.read(port_base(5) + 0x18) & 0xc011, 0xc011);
    d.hba.bar_write(4, 0x10, &0xcu32.to_le_bytes());
    let mut data = [0; 4];
    d.hba.bar_read(4, 0x14, &mut data);
    assert_eq!(le32(&data), 0x3f);
    d.write(4, 1);
    assert_eq!(d.read(port_base(5) + 0x18), 6);
    assert_eq!(d.read(4), 1 << 31);
}

#[test]
fn system_reset_clears_pci_and_hba_state_and_keeps_attached_drives() {
    let mut d = Driver::new();
    d.hba.attach_cdrom(2, None).unwrap();
    let optical_mode = {
        let mut core = d.hba.idle();
        let Some(Drive::Cdrom(optical)) = &mut core.drives[2] else {
            panic!("CD-ROM missing")
        };
        let optical_mode = optical.transfer_mode;
        optical.transfer_mode = 0x20;
        optical_mode
    };
    d.hba
        .attach_disk(5, image(vec![0x42; 4096], false))
        .unwrap();
    d.hba.config_write(1, 0, &7u16.to_le_bytes());
    d.start(5);
    crate::Resettable::reset(&mut d.hba).unwrap();
    assert_eq!(d.hba.config_read(1) & 0xffff, 0);
    assert_eq!(d.read(4), 1 << 31);
    assert_eq!(d.read(port_base(5) + 0x18), 6);
    assert_eq!(d.read(port_base(5) + 0x24), 0x101);
    let mut core = d.hba.idle();
    let Some(Drive::Disk(disk)) = &mut core.drives[5] else {
        panic!("drive detached")
    };
    let mut sector = [0; 512];
    disk.image.read_at(0, &mut sector).unwrap();
    assert_eq!(sector, [0x42; 512]);
    let Some(Drive::Cdrom(optical)) = &core.drives[2] else {
        panic!("CD-ROM detached")
    };
    assert_eq!(optical.transfer_mode, optical_mode);
}

#[test]
fn p4_identify_dma_write_read_flush_and_fis() {
    let mut d = Driver::new();
    let flushes = Arc::new(Mutex::new(0));
    d.hba
        .attach_disk(
            0,
            Box::new(MemoryDisk {
                data: vec![0; 128 * 512],
                readonly: false,
                flushes: flushes.clone(),
            }),
        )
        .unwrap();
    d.start(0);
    assert_eq!(d.command(0xec, 0, 0, false), 0x50);
    assert_eq!(le32(&d.bytes(0x1004, 4)), 512);
    let identify = d.bytes(0x4000, 512);
    assert_eq!(le64(&identify[200..]), 128);
    assert_eq!(identify[153] & 1, 1);
    assert_eq!(d.bytes(0x2020, 1), [0x5f]);
    let payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    d.put(0x4000, &payload);
    assert_eq!(d.command(0x35, 7, 2, true), 0x50);
    d.put(0x4000, &[0; 1024]);
    assert_eq!(d.command(0x25, 7, 2, false), 0x50);
    assert_eq!(d.bytes(0x4000, 1024), payload);
    assert_eq!(d.bytes(0x2040, 4), [0x34, 0x40, 0x50, 0]);
    assert_eq!(le32(&d.bytes(0x1004, 4)), 1024);
    assert_eq!(d.command(0xea, 0, 0, false), 0x50);
    assert_eq!(*flushes.lock().unwrap(), 1);
    assert!(d.irq.level.load(Ordering::SeqCst));
    d.ack(0);
    assert!(!d.irq.level.load(Ordering::SeqCst));
}

#[test]
fn scatter_gather_ioc_and_nonzero_slot() {
    let mut d = Driver::new();
    d.hba
        .attach_disk(0, image(vec![0x5a; 4096], false))
        .unwrap();
    d.start(0);
    d.prepare(
        0x25,
        0,
        0,
        2,
        None,
        &[(0x5000, 256, true), (0x6000, 768, false)],
    );
    let header = d.bytes(0x1000, 32);
    d.put(0x1000 + 31 * 32, &header);
    d.write(0x138, 1 << 31);
    assert_eq!(d.read(0x138), 0);
    assert_eq!(d.bytes(0x5000, 256), vec![0x5a; 256]);
    assert_eq!(d.bytes(0x6000, 768), vec![0x5a; 768]);
    assert_eq!(le32(&d.bytes(0x1004 + 31 * 32, 4)), 1024);
    assert_ne!(d.read(0x110) & DSS, 0);
}

#[test]
fn msi_and_interrupt_acknowledgement() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.start(0);
    d.hba.config_write(33, 0, &0xfee0_1000u32.to_le_bytes());
    d.hba.config_write(34, 0, &1u32.to_le_bytes());
    d.hba.config_write(35, 0, &0x42u16.to_le_bytes());
    d.hba.config_write(32, 2, &[0x81]);
    d.command(0xec, 0, 0, false);
    assert_eq!(
        *d.irq.messages.lock().unwrap(),
        [MsiMessage {
            address: 0x1_fee0_1000,
            data: 0x42
        }]
    );
    assert!(!d.irq.level.load(Ordering::SeqCst));
    d.command(0xec, 0, 0, false);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 2);
    d.ack(0);
    d.write(4, 0x8000_0000);
    d.command(0xec, 0, 0, false);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 2);
    d.write(4, 0x8000_0002);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 3);
}

#[test]
fn command_waits_for_bus_master_and_port_start() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.start(0);
    d.hba.config_write(1, 0, &[2]);
    d.prepare(0xec, 0, 0, 0, None, &[(0x4000, 512, false)]);
    d.write(0x138, 1);
    assert_eq!(d.read(0x138), 1);
    d.hba.config_write(1, 0, &[6]);
    assert_eq!(d.read(0x138), 0);
    d.write(0x118, 0x10);
    d.write(0x138, 1);
    assert_eq!(d.read(0x138), 1);
    d.write(0x118, 0x11);
    assert_eq!(d.read(0x138), 0);
}

#[test]
fn task_file_errors_for_readonly_range_and_unknown_command() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], true)).unwrap();
    d.start(0);
    assert_eq!(d.command(0x35, 0, 1, true), 0x451);
    assert_ne!(d.read(0x110) & TFES, 0);
    assert_eq!(d.command(0x25, 8, 1, false), 0x1051);
    assert_eq!(d.command(0xff, 0, 0, false), 0x451);
    assert_eq!(d.command(0x25, 0, 1, true), 0x451);
}

#[test]
fn malformed_dma_does_not_write_disk_or_panic() {
    let mut d = Driver::new();
    d.hba
        .attach_disk(0, image(vec![0x55; 4096], false))
        .unwrap();
    d.start(0);
    for prds in [
        vec![(0x1ff00, 512, false)],
        vec![(u64::MAX - 1, 512, false)],
        vec![(0x4001, 512, false)],
        vec![(0x4000, 256, false)],
    ] {
        d.ack(0);
        d.prepare(0x35, 0x40, 0, 1, None, &prds);
        d.issue(0);
        assert_ne!(d.read(0x110) & HBFS, 0);
        assert_eq!(le32(&d.bytes(0x1004, 4)), 0);
    }
    assert_eq!(d.command(0x25, 0, 1, false), 0x50);
    assert_eq!(d.bytes(0x4000, 512), vec![0x55; 512]);
    d.hba.write(u64::MAX, &[0xff; 8]);
    let mut bytes = [1; 8];
    d.hba.read(u64::MAX, &mut bytes);
    assert_eq!(bytes, [0; 8]);
}

#[test]
fn p4_atapi_iso_read10_read12_capacity_and_identify() {
    let mut d = Driver::cdrom();
    assert_eq!(d.read(0x124), 0xeb14_0101);
    assert_eq!(d.command(0xa1, 0, 0, false), 0x50);
    assert_eq!(&d.bytes(0x4000, 2), &[0xc0, 0x85]);
    let (status, capacity) = d.packet(&[0x25]);
    assert_eq!(status, 0x50);
    assert_eq!(capacity, [0, 0, 0, 63, 0, 0, 8, 0]);
    for cdb in [
        vec![0x28, 0, 0, 0, 0, 16, 0, 0, 1, 0],
        vec![0xa8, 0, 0, 0, 0, 16, 0, 0, 0, 1, 0, 0],
    ] {
        let (status, data) = d.packet(&cdb);
        assert_eq!(status, 0x50);
        assert_eq!(data.len(), 2048);
        assert_eq!(&data[..7], b"\x01CD001\x01");
    }
}

#[test]
fn atapi_inquiry_mode_sense_configuration_and_sense() {
    let mut d = Driver::cdrom();
    let (status, data) = d.packet(&[0x12, 0, 0, 0, 36]);
    assert_eq!(status, 0x50);
    assert_eq!(data[0], 5);
    assert_eq!(data[1], 0x80);
    assert_eq!(&data[8..16], b"QEMU    ");
    let (_, data) = d.packet(&[0x5a, 0, 0x2a, 0, 0, 0, 0, 0, 64]);
    assert_eq!(data[3], 0x80);
    assert_eq!(data[8], 0x2a);
    let (_, data) = d.packet(&[0x1a, 0, 0x3f, 0, 64]);
    assert_eq!(data[2], 0x80);
    let (_, data) = d.packet(&[0x46, 0, 0, 0, 0, 0, 0, 0, 128]);
    assert_eq!(&data[6..8], &[0, 8]);
    assert_eq!(d.packet(&[0xff]).0, 0x5451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[2], 5);
    assert_eq!(sense[12], 0x20);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[2], 0);
    assert_eq!(d.packet(&[0x12, 1, 0, 0, 36]).0, 0x5451);
    let (_, data) = d.packet(&[0x12, 0, 0, 0, 4]);
    assert_eq!(data.len(), 4);
}

#[test]
fn p4_eject_insert_unit_attention_and_media_events() {
    let mut d = Driver::cdrom();
    assert_eq!(d.packet(&[0]).0, 0x50);
    d.hba.eject_media(0).unwrap();
    assert_eq!(d.packet(&[0]).0, 0x6451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(&sense[12..14], &[0x28, 0]);
    assert_eq!(d.packet(&[0]).0, 0x2451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(&sense[12..14], &[0x3a, 2]);
    let (_, event) = d.packet(&[0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8]);
    assert_eq!(&event[4..6], &[3, 1]);
    d.hba
        .insert_media(0, image(vec![0xa5; 4096], true))
        .unwrap();
    assert_eq!(d.packet(&[0]).0, 0x6451);
    assert_eq!(d.packet(&[0]).0, 0x50);
    let (_, data) = d.packet(&[0x28, 0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(data, vec![0xa5; 2048]);
    let (_, event) = d.packet(&[0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8]);
    assert_eq!(&event[4..6], &[2, 2]);
}

/// The host's eject goes through a guest lock and leaves the tray closed on
/// an empty drive, which is what QEMU's forced `eject` plus
/// `blockdev-close-tray` leave.
#[test]
fn forced_eject_overrides_the_guest_lock_and_closes_the_tray() {
    let mut d = Driver::cdrom();
    assert_eq!(d.packet(&[0x1e, 0, 0, 0, 1]).0, 0x50);
    assert!(d.hba.eject_media(0).is_err());
    assert_eq!(
        d.hba.force_eject_media(0).unwrap().unwrap().size(),
        64 * 2048
    );
    assert_eq!(
        d.hba.media_state(0).unwrap(),
        MediaState {
            present: false,
            tray_open: false,
            locked: false
        }
    );
    assert_eq!(d.packet(&[0]).0, 0x6451);
    assert_eq!(d.packet(&[0]).0, 0x2451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(&sense[12..14], &[0x3a, 1]);
    let (_, event) = d.packet(&[0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8]);
    assert_eq!(&event[4..6], &[3, 0]);
    assert_eq!(d.packet(&[0x28, 0, 0, 0, 0, 0, 0, 0, 1]).0, 0x2451);
    assert!(d.hba.force_eject_media(0).unwrap().is_none());
    assert!(d.hba.force_eject_media(1).is_err());
    d.hba
        .insert_media(0, image(vec![0xa5; 4096], true))
        .unwrap();
    assert!(d.hba.media_state(0).unwrap().present);
}

#[test]
fn guest_tray_lock_eject_and_reload() {
    let mut d = Driver::cdrom();
    assert_eq!(
        d.hba.media_state(0).unwrap(),
        MediaState {
            present: true,
            tray_open: false,
            locked: false
        }
    );
    assert_eq!(d.packet(&[0x1e, 0, 0, 0, 1]).0, 0x50);
    assert!(d.hba.eject_media(0).is_err());
    assert!(d.hba.media_state(0).unwrap().locked);
    assert_eq!(d.packet(&[0x1b, 0, 0, 0, 2]).0, 0x5451);
    assert_eq!(d.packet(&[0x1e, 0, 0, 0, 0]).0, 0x50);
    assert_eq!(d.packet(&[0x1b, 0, 0, 0, 2]).0, 0x50);
    assert_eq!(
        d.hba.media_state(0).unwrap(),
        MediaState {
            present: false,
            tray_open: true,
            locked: false
        }
    );
    assert_eq!(d.packet(&[0]).0, 0x6451);
    assert_eq!(d.packet(&[0]).0, 0x2451);
    assert_eq!(d.packet(&[0x1b, 0, 0, 0, 3]).0, 0x50);
    assert_eq!(d.packet(&[0]).0, 0x6451);
    assert_eq!(d.packet(&[0]).0, 0x50);
}

#[test]
fn snapshot_round_trip_and_media_mismatch_are_atomic() {
    let mut d = Driver::cdrom();
    d.packet(&[0x1e, 0, 0, 0, 1]);
    d.packet(&[0xff]);
    let state = d.hba.save_state().unwrap();
    d.packet(&[3, 0, 0, 0, 18]);
    d.packet(&[0x1e, 0, 0, 0, 0]);
    d.write(4, 1);
    d.hba.restore_state(&state).unwrap();
    assert!(d.hba.eject_media(0).is_err());
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[2], 5);
    d.packet(&[0x1e, 0, 0, 0, 0]);
    d.hba.insert_media(0, image(vec![0; 4096], true)).unwrap();
    let before = d.hba.save_state().unwrap();
    assert!(d.hba.restore_state(&state).is_err());
    assert_eq!(before, d.hba.save_state().unwrap());
    for len in [0, 3, state.len() - 1] {
        assert!(d.hba.restore_state(&state[..len]).is_err());
    }
}

#[test]
fn comreset_and_software_reset_deliver_signatures() {
    let mut d = Driver::cdrom();
    d.write(0x12c, 1);
    assert_eq!(d.read(0x128), 1);
    d.write(0x12c, 0);
    assert_eq!(d.read(0x128), 0x113);
    assert_eq!(d.read(0x124), 0xeb14_0101);
    d.ack(0);
    d.prepare(0, 0, 0, 0, None, &[]);
    d.put(0x3001, &[0]);
    d.put(0x300f, &[4]);
    assert_eq!(d.issue(0), 0x80);
    d.put(0x300f, &[0]);
    assert_eq!(d.issue(0), 0x150);
    assert_eq!(d.bytes(0x2044, 3), [1, 0x14, 0xeb]);
}

#[test]
fn fatal_errors_retain_slots_until_port_restart() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.start(0);
    d.prepare(0xff, 0, 0, 0, None, &[]);
    d.issue(0);
    assert_eq!(d.read(0x138), 1);
    assert_eq!(d.read(0x118) & 0x8001, 1);
    // Acknowledging interrupts cannot restart a halted command engine.
    d.write(0x110, u32::MAX);
    d.write(8, u32::MAX);
    d.prepare(0xec, 0, 0, 0, None, &[(0x4000, 512, false)]);
    d.write(0x138, 1);
    assert_eq!(d.read(0x138), 1);
    assert_eq!(le32(&d.bytes(0x1004, 4)), 0);
    d.ack(0);
    assert_eq!(d.read(0x138), 0);
    assert_eq!(d.issue(0), 0x50);
}

#[test]
fn dma_above_four_gib_and_fis_address_validation() {
    let high = 1u64 << 32;
    let memory =
        GuestMemory::from_ranges(&[(GuestAddress(0), 0x10000), (GuestAddress(high), 0x10000)])
            .unwrap();
    let irq = Arc::new(Interrupts::default());
    let hba = Ahci::new("ahci", memory.clone(), irq.clone(), irq.clone());
    let mut d = Driver { hba, memory, irq };
    d.hba
        .attach_disk(0, image(vec![0xa5; 4096], false))
        .unwrap();
    d.start(0);
    d.write(0x118, 0);
    d.write(0x104, 1);
    d.write(0x10c, 1);
    d.write(0x118, 0x11);
    d.ack(0);
    d.prepare(0x25, 0, 0, 1, None, &[(high + 0x4000, 512, true)]);
    let mut header = d.bytes(0x1000, 32);
    header[8..16].copy_from_slice(&(high + 0x3000).to_le_bytes());
    d.put(high + 0x1000, &header);
    d.put(high + 0x3000, &d.bytes(0x3000, 0x90));
    assert_eq!(d.issue(0), 0x50);
    assert_eq!(d.bytes(high + 0x4000, 512), vec![0xa5; 512]);
    assert_eq!(d.bytes(high + 0x2040, 1), [0x34]);
    assert_eq!(le32(&d.bytes(high + 0x1004, 4)), 512);
    d.write(0x118, 0);
    d.write(0x104, 0);
    d.write(0x10c, 2);
    d.write(0x118, 0x11);
    d.ack(0);
    d.prepare(0x35, 0x40, 0, 1, None, &[(0x4000, 512, false)]);
    d.write(0x138, 1);
    assert_ne!(d.read(0x110) & HBFS, 0);
    let mut disk = [0; 512];
    d.hba
        .with_disk(0, |image| image.read_at(0, &mut disk))
        .unwrap()
        .unwrap();
    assert_eq!(disk, [0xa5; 512]);
}

#[test]
fn set_features_cache_policy_and_transfer_mode_survive_snapshot() {
    let mut d = Driver::new();
    let flushes = Arc::new(Mutex::new(0));
    d.hba
        .attach_disk(
            0,
            Box::new(MemoryDisk {
                data: vec![0; 4096],
                readonly: false,
                flushes: flushes.clone(),
            }),
        )
        .unwrap();
    d.start(0);
    d.prepare(0xef, 0, 0, 0, None, &[]);
    d.put(0x3003, &[0x82]);
    assert_eq!(d.issue(0), 0x50);
    assert_eq!(*flushes.lock().unwrap(), 1);
    d.command(0x35, 0, 1, true);
    assert_eq!(*flushes.lock().unwrap(), 2);
    d.ack(0);
    d.prepare(0xef, 0, 0, 0x45, None, &[]);
    d.put(0x3003, &[3]);
    assert_eq!(d.issue(0), 0x50);
    let saved = d.hba.save_state().unwrap();
    d.command(0xec, 0, 0, false);
    assert_eq!(d.bytes(0x4000 + 170, 1)[0] & 0x20, 0);
    assert_eq!(d.bytes(0x4000 + 177, 1)[0] & 0x20, 0x20);
    d.ack(0);
    d.prepare(0xef, 0, 0, 0, None, &[]);
    d.put(0x3003, &[2]);
    d.issue(0);
    d.hba.restore_state(&saved).unwrap();
    d.command(0xec, 0, 0, false);
    assert_eq!(d.bytes(0x4000 + 170, 1)[0] & 0x20, 0);
}

#[test]
fn atapi_range_zero_count_readonly_and_toc() {
    let mut d = Driver::cdrom();
    assert_eq!(d.packet(&[0x28, 0, 0, 0, 0, 64, 0, 0, 1]).0, 0x5451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[12], 0x21);
    let (status, data) = d.packet(&[0x28]);
    assert_eq!(status, 0x50);
    assert!(data.is_empty());
    assert_eq!(d.packet(&[0x2a]).0, 0x7451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[2], 7);
    assert_eq!(sense[12], 0x27);
    let (_, toc) = d.packet(&[0x43, 0, 0, 0, 0, 0, 0, 0, 32]);
    assert_eq!(&toc[..4], &[0, 18, 1, 1]);
    assert_eq!(toc[6], 1);
    assert_eq!(toc[14], 0xaa);
    assert_eq!(u32::from_be_bytes(toc[16..20].try_into().unwrap()), 64);
}

/// The CD-ROM probe Windows Setup (WinPE 26100) sends before it mounts a
/// disc, in order, with its exact CDBs. READ TOC session information and READ
/// DISC INFORMATION must succeed: when they fail, Windows does not mount the
/// UDF file system of the install DVD, sees only its ISO 9660 stub and Setup
/// finds no install image ("a media driver your computer needs is missing").
#[test]
fn winpe_disc_probe_sequence_reports_one_complete_session() {
    let mut d = Driver::cdrom();
    let ok = |d: &mut Driver, cdb: &[u8]| {
        let (status, data) = d.packet(cdb);
        assert_eq!(status, 0x50, "CDB {cdb:02x?}");
        data
    };
    ok(&mut d, &[0]);
    ok(&mut d, &[0x25]);
    let config = ok(&mut d, &[0x46, 0, 0, 0, 0, 0, 0, 0, 8]);
    assert_eq!(&config[6..8], &[0, 8]);
    ok(&mut d, &[0x5a, 8, 0x2a, 0, 0, 0, 0, 0, 0x28]);

    // READ TOC, format 1 (multi-session information), allocation 12.
    let session = ok(&mut d, &[0x43, 0, 1, 0, 0, 0, 0, 0, 0x0c]);
    assert_eq!(session, [0, 10, 1, 1, 0, 0x14, 1, 0, 0, 0, 0, 0]);
    // The same in MSF form: track 1 starts at 00:02:00.
    let session = ok(&mut d, &[0x43, 2, 1, 0, 0, 0, 0, 0, 0x0c]);
    assert_eq!(&session[8..12], &[0, 0, 2, 0]);

    // READ DISC INFORMATION, standard disc information, allocation 256.
    let disc = ok(&mut d, &[0x51, 0, 0, 0, 0, 0, 0, 1, 0]);
    assert_eq!(disc.len(), 34);
    assert_eq!(&disc[..8], &[0, 32, 0x0e, 1, 1, 1, 1, 0x20]);
    assert_eq!(&disc[16..24], &[0xff; 8]);
    assert_eq!(d.packet(&[0x51, 1, 0, 0, 0, 0, 0, 1, 0]).0, 0x5451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(&sense[12..14], &[0x24, 0]);

    // READ TOC, format 0, allocation 0x554: unchanged, track 1 and lead-out.
    let toc = ok(&mut d, &[0x43, 0, 0, 0, 0, 0, 0, 5, 0x54]);
    assert_eq!(&toc[..4], &[0, 18, 1, 1]);
    assert_eq!(u32::from_be_bytes(toc[16..20].try_into().unwrap()), 64);

    // READ TOC, format 2 (raw TOC): A0, A1, A2 and track 1 of session 1.
    let raw = ok(&mut d, &[0x43, 2, 2, 0, 0, 0, 1, 0, 0x40]);
    assert_eq!(&raw[..4], &[0, 46, 1, 1]);
    let points: Vec<_> = raw[4..].chunks(11).map(|e| (e[0], e[1], e[3])).collect();
    assert_eq!(
        points,
        [
            (1, 0x14, 0xa0),
            (1, 0x14, 0xa1),
            (1, 0x14, 0xa2),
            (1, 0x14, 1)
        ]
    );
    // Lead-out at LBA 64: 64 + 150 frames = 00:02:64.
    assert_eq!(&raw[4 + 2 * 11 + 8..4 + 2 * 11 + 11], &[0, 2, 64]);
    assert_eq!(d.packet(&[0x43, 2, 2, 0, 0, 0, 2, 0, 0x40]).0, 0x5451);
    // Formats 3 and up (PMA, ATIP, CD-TEXT) stay unsupported.
    assert_eq!(d.packet(&[0x43, 0, 3, 0, 0, 0, 0, 0, 0x40]).0, 0x5451);
    // The format in the old position, the top bits of byte 9.
    let session = ok(&mut d, &[0x43, 0, 0, 0, 0, 0, 0, 0, 0x0c, 0x40]);
    assert_eq!(&session[..4], &[0, 10, 1, 1]);
}

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        let result = std::process::Command::new("mktemp")
            .args(["-d", "--tmpdir", "vmlab-ahci-XXXXXX"])
            .output()
            .unwrap();
        assert!(result.status.success());
        Self(std::path::PathBuf::from(
            String::from_utf8(result.stdout).unwrap().trim(),
        ))
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn p4_real_iso_via_raw_disk_image() {
    if std::process::Command::new("xorriso")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("SKIP p4_real_iso_via_raw_disk_image: xorriso is not installed");
        return;
    }
    let temp = TempDir::new();
    let source = temp.0.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(
        source.join("HELLO.TXT"),
        b"AHCI optical DMA from a real ISO image\n",
    )
    .unwrap();
    let iso_path = temp.0.join("test.iso");
    let output = std::process::Command::new("xorriso")
        .args(["-as", "mkisofs", "-quiet", "-o"])
        .arg(&iso_path)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = std::fs::read(&iso_path).unwrap();
    let mut d = Driver::new();
    d.hba
        .attach_cdrom(
            0,
            Some(vmlab_disk::open(&iso_path, vmlab_disk::Format::Raw, true).unwrap()),
        )
        .unwrap();
    d.start(0);
    let mut actual = Vec::new();
    for lba in (0..expected.len() / 2048).step_by(4) {
        let sectors = (expected.len() / 2048 - lba).min(4) as u16;
        let mut cdb = [0; 12];
        cdb[0] = 0x28;
        cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
        cdb[7..9].copy_from_slice(&sectors.to_be_bytes());
        let (status, data) = d.packet(&cdb);
        assert_eq!(status, 0x50);
        actual.extend(data);
    }
    assert_eq!(actual, expected);
}

#[test]
fn p4_qcow2_disk_and_controller_snapshot_transaction() {
    let temp = TempDir::new();
    let path = temp.0.join("disk.qcow2");
    vmlab_disk::ops::create_qcow2(&path, 2 * 1024 * 1024, None, None).unwrap();
    let mut d = Driver::new();
    d.hba
        .attach_disk(
            0,
            vmlab_disk::open(&path, vmlab_disk::Format::Qcow2, false).unwrap(),
        )
        .unwrap();
    d.start(0);
    d.put(0x4000, &[0x51; 512]);
    assert_eq!(d.command(0x35, 3, 1, true), 0x50);
    let state = d.hba.save_state().unwrap();
    let ram = d.bytes(0, 0x20000);
    d.hba
        .with_disk(0, |disk| {
            disk.write_vmstate(0, &state).unwrap();
            disk.snapshot_create("before", state.len() as u64, 0)
                .unwrap();
        })
        .unwrap();
    d.put(0x4000, &[0xa2; 512]);
    assert_eq!(d.command(0x35, 3, 1, true), 0x50);
    assert_eq!(d.command(0x25, 3, 1, false), 0x50);
    assert_eq!(d.bytes(0x4000, 512), vec![0xa2; 512]);
    d.write(4, 1);
    let mut restored = vec![0; state.len()];
    d.hba
        .with_disk(0, |disk| {
            disk.snapshot_apply("before").unwrap();
            disk.read_vmstate(0, &mut restored).unwrap();
        })
        .unwrap();
    d.put(0, &ram);
    d.hba.restore_state(&restored).unwrap();
    assert_eq!(d.command(0x25, 3, 1, false), 0x50);
    assert_eq!(d.bytes(0x4000, 512), vec![0x51; 512]);
    d.hba.with_disk(0, |disk| disk.flush()).unwrap().unwrap();
    if let Ok(output) = std::process::Command::new("qemu-img")
        .arg("check")
        .arg(&path)
        .output()
    {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        eprintln!("SKIP qcow2 external check: qemu-img is not installed");
    }
}

#[test]
fn full_size_ata_zero_count_and_large_atapi_read12() {
    let transfer = 33 * 1024 * 1024;
    let memory = GuestMemory::from_ranges(&[(GuestAddress(0), transfer + 0x10000)]).unwrap();
    let irq = Arc::new(Interrupts::default());
    let hba = Ahci::new("ahci", memory.clone(), irq.clone(), irq.clone());
    let mut d = Driver { hba, memory, irq };
    d.hba
        .attach_disk(0, image(vec![0x59; transfer], false))
        .unwrap();
    d.hba
        .attach_cdrom(1, Some(image(vec![0xa6; transfer], true)))
        .unwrap();
    for (port, size, opcode) in [(0, 32 * 1024 * 1024, 0x25), (1, transfer, 0xa0)] {
        d.start(port);
        let prds: Vec<_> = (0..size)
            .step_by(4 * 1024 * 1024)
            .map(|n| (0x10000 + n as u64, (size - n).min(4 * 1024 * 1024), true))
            .collect();
        let mut cdb = [0; 16];
        cdb[0] = 0xa8;
        cdb[6..10].copy_from_slice(&((transfer / 2048) as u32).to_be_bytes());
        d.prepare(
            opcode,
            if port == 1 { 0x20 } else { 0 },
            0,
            0,
            Some(cdb),
            &prds,
        );
        assert_eq!(d.issue(port), 0x50);
        assert_eq!(le32(&d.bytes(0x1004, 4)), size as u32);
        let byte = if port == 0 { 0x59 } else { 0xa6 };
        for offset in (0..size).step_by(4096) {
            assert_eq!(d.bytes(0x10000 + offset as u64, 4096), vec![byte; 4096]);
        }
    }
}

#[test]
fn media_validation_and_ports_are_checked() {
    let mut d = Driver::new();
    assert!(d.hba.attach_disk(6, image(vec![0; 4096], false)).is_err());
    assert!(
        d.hba
            .attach_cdrom(0, Some(image(vec![0; 4096], false)))
            .is_err()
    );
    assert!(
        d.hba
            .attach_cdrom(0, Some(image(vec![0; 2049], true)))
            .is_err()
    );
    d.hba.attach_cdrom(0, None).unwrap();
    d.start(0);
    assert!(d.hba.attach_disk(0, image(vec![0; 4096], false)).is_err());
    assert_eq!(d.packet(&[0]).0, 0x2451);
    assert!(d.hba.insert_media(1, image(vec![0; 4096], true)).is_err());
}

/// Card #709's measurement: how long the register write that issues a
/// command holds its caller, which is a vCPU thread. Run it alone, with
/// `AHCI_MEASURE_DIR` on the file system to measure:
/// `cargo test -p vmlab-hv-devices measure_issue_block_time -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement on real storage, with no assertion"]
fn measure_issue_block_time() {
    let dir = std::env::var_os("AHCI_MEASURE_DIR").map(std::path::PathBuf::from);
    let temp = TempDir::new();
    let path = dir
        .unwrap_or_else(|| temp.0.clone())
        .join(format!("ahci-measure-{}.raw", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    drop(file);
    let mut d = Driver::new();
    d.hba
        .attach_disk(
            0,
            vmlab_disk::open(&path, vmlab_disk::Format::Raw, false).unwrap(),
        )
        .unwrap();
    d.start(0);
    d.put(0x4000, &[0x6b; 0x8000]);
    let mut issue = Vec::new();
    let mut complete = Vec::new();
    for round in 0..200u64 {
        for (command, flags) in [(0x35, 0x40), (0xe7, 0)] {
            d.ack(0);
            d.prepare(command, flags, round * 64, 64, None, &[(0x4000, 0x8000, true)]);
            let started = std::time::Instant::now();
            d.write_only(0x138, 1);
            issue.push((command, started.elapsed()));
            while d.read(0x138) != 0 {
                std::thread::yield_now();
            }
            complete.push((command, started.elapsed()));
        }
    }
    std::fs::remove_file(&path).unwrap();
    for (name, command) in [("write", 0x35), ("flush", 0xe7)] {
        for (what, samples) in [("issue", &issue), ("complete", &complete)] {
            let mut times: Vec<_> = samples
                .iter()
                .filter(|(c, _)| *c == command)
                .map(|(_, t)| *t)
                .collect();
            times.sort();
            eprintln!(
                "MEASURE {name} {what}: median {:?} p99 {:?} max {:?} total {:?}",
                times[times.len() / 2],
                times[times.len() * 99 / 100],
                times[times.len() - 1],
                times.iter().sum::<std::time::Duration>(),
            );
        }
    }
}

/// Holds every write and flush of a disk until the test opens it, and counts
/// the calls that have arrived.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
    arrived: AtomicUsize,
}
impl Gate {
    fn pass(&self) {
        self.arrived.fetch_add(1, Ordering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
    }
    fn set(&self, open: bool) {
        *self.open.lock().unwrap() = open;
        self.opened.notify_all();
    }
    /// Wait until the worker is inside the disk with `count` calls.
    fn wait_arrived(&self, count: usize) {
        while self.arrived.load(Ordering::SeqCst) < count {
            std::thread::yield_now();
        }
    }
}

struct GatedDisk {
    disk: MemoryDisk,
    gate: Arc<Gate>,
}
impl DiskImage for GatedDisk {
    fn size(&self) -> u64 {
        self.disk.size()
    }
    fn read_only(&self) -> bool {
        self.disk.read_only()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> vmlab_disk::Result<()> {
        self.disk.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> vmlab_disk::Result<()> {
        self.gate.pass();
        self.disk.write_at(offset, buf)
    }
    fn flush(&mut self) -> vmlab_disk::Result<()> {
        self.gate.pass();
        self.disk.flush()
    }
    fn discard(&mut self, offset: u64, len: u64) -> vmlab_disk::Result<()> {
        self.disk.discard(offset, len)
    }
    fn snapshots(&self) -> vmlab_disk::Result<Vec<vmlab_disk::SnapshotInfo>> {
        self.disk.snapshots()
    }
    fn snapshot_create(&mut self, name: &str, size: u64, clock: u64) -> vmlab_disk::Result<()> {
        self.disk.snapshot_create(name, size, clock)
    }
    fn snapshot_apply(&mut self, name: &str) -> vmlab_disk::Result<vmlab_disk::SnapshotInfo> {
        self.disk.snapshot_apply(name)
    }
    fn snapshot_delete(&mut self, name: &str) -> vmlab_disk::Result<()> {
        self.disk.snapshot_delete(name)
    }
    fn write_vmstate(&mut self, offset: u64, buf: &[u8]) -> vmlab_disk::Result<()> {
        self.disk.write_vmstate(offset, buf)
    }
    fn read_vmstate(&mut self, offset: u64, buf: &mut [u8]) -> vmlab_disk::Result<()> {
        self.disk.read_vmstate(offset, buf)
    }
}

/// A started controller with a gated disk on port 0 and a CD-ROM on port 1.
fn gated() -> (Driver, Arc<Gate>) {
    let gate = Arc::new(Gate::default());
    let mut d = Driver::new();
    d.hba
        .attach_disk(
            0,
            Box::new(GatedDisk {
                disk: MemoryDisk {
                    data: vec![0x11; 64 * 512],
                    readonly: false,
                    flushes: Arc::default(),
                },
                gate: gate.clone(),
            }),
        )
        .unwrap();
    d.hba
        .attach_cdrom(1, Some(image(vec![0; 64 * 2048], true)))
        .unwrap();
    d.start(0);
    // Port 1 by hand: `start` resets the whole HBA.
    let base = port_base(1);
    d.write(base, 0x1000);
    d.write(base + 8, 0x2000);
    d.write(base + 0x14, u32::MAX);
    d.write(base + 0x18, 0x10);
    d.write(base + 0x18, 0x11);
    d.ack(1);
    (d, gate)
}

#[test]
fn a_flush_in_the_disk_does_not_hold_the_register_write() {
    let (mut d, gate) = gated();
    d.prepare(0xe7, 0, 0, 0, None, &[]);
    // The write returns while the flush is still inside the disk: this is
    // the vCPU going back to the guest, where it can answer an IPI.
    d.write_only(0x138, 1);
    gate.wait_arrived(1);
    assert_eq!(d.read(0x138), 1);
    assert_eq!(d.read(0x110), 0);
    assert!(!d.irq.level.load(Ordering::SeqCst));
    // Registers answer and another port completes a command meanwhile.
    d.prepare(0xa1, 0, 0, 0, None, &[(0x4000, 512, false)]);
    d.write_only(port_base(1) + 0x38, 1);
    assert_eq!(d.read(port_base(1) + 0x38), 0);
    assert_eq!(d.read(port_base(1) + 0x20), 0x50);
    assert_eq!(d.read(0x138), 1);
    d.write_only(port_base(1) + 0x10, u32::MAX);
    d.write_only(8, 2);
    assert!(!d.irq.level.load(Ordering::SeqCst));
    // The flush ends: the command completes with an interrupt.
    gate.set(true);
    d.hba.wait_idle();
    assert_eq!(d.read(0x138), 0);
    assert_eq!(d.read(0x120), 0x50);
    assert_ne!(d.read(0x110) & DHRS, 0);
    assert_eq!(d.read(8), 1);
    assert!(d.irq.level.load(Ordering::SeqCst));
}

#[test]
fn slots_issued_together_run_in_order_behind_a_blocked_write() {
    let (mut d, gate) = gated();
    // Slot 1 reads the sector slot 0 writes.
    d.prepare(0x25, 0, 3, 1, None, &[(0x6000, 512, true)]);
    let mut header = d.bytes(0x1000, 32);
    header[8..16].copy_from_slice(&0x5000u64.to_le_bytes());
    d.put(0x1020, &header);
    d.put(0x5000, &d.bytes(0x3000, 0x90));
    d.put(0x4000, &[0x77; 512]);
    d.prepare(0x35, 0x40, 3, 1, None, &[(0x4000, 512, true)]);
    d.write_only(0x138, 3);
    gate.wait_arrived(1);
    assert_eq!(d.read(0x138), 3);
    assert_eq!(d.bytes(0x6000, 512), vec![0; 512]);
    gate.set(true);
    d.hba.wait_idle();
    assert_eq!(d.read(0x138), 0);
    assert_eq!(d.bytes(0x6000, 512), vec![0x77; 512]);
    assert_eq!(le32(&d.bytes(0x1004, 4)), 512);
    assert_eq!(le32(&d.bytes(0x1024, 4)), 512);
}

#[test]
fn a_snapshot_waits_for_the_command_in_flight_and_saves_it_completed() {
    let (mut d, gate) = gated();
    d.put(0x4000, &[0x5a; 512]);
    d.prepare(0x35, 0x40, 7, 1, None, &[(0x4000, 512, true)]);
    d.write_only(0x138, 1);
    gate.wait_arrived(1);
    let saved = AtomicBool::new(false);
    let state = std::thread::scope(|scope| {
        let snapshot = scope.spawn(|| {
            PciSnapshot::quiesce(&mut d.hba);
            let state = d.hba.save_function_state().unwrap();
            saved.store(true, Ordering::SeqCst);
            state
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!saved.load(Ordering::SeqCst));
        gate.set(true);
        snapshot.join().unwrap()
    });
    // The saved state holds the finished command, and the disk its data.
    let mut sector = [0; 512];
    d.hba
        .with_disk(0, |disk| disk.read_at(7 * 512, &mut sector))
        .unwrap()
        .unwrap();
    assert_eq!(sector, [0x5a; 512]);
    let (mut restored, _gate) = gated();
    restored.put(0, &d.bytes(0, 0x20000));
    restored.hba.restore_function_state(&state).unwrap();
    assert_eq!(restored.read(0x138), 0);
    assert_eq!(restored.read(0x120), 0x50);
    assert_ne!(restored.read(0x110) & DHRS, 0);
    assert!(restored.irq.level.load(Ordering::SeqCst));
    assert_eq!(restored.hba.save_function_state().unwrap(), state);
}

#[test]
fn stopping_a_port_discards_the_command_in_flight_after_its_io_ends() {
    let (mut d, gate) = gated();
    d.put(0x4000, &[0x3c; 512]);
    d.prepare(0x35, 0x40, 2, 1, None, &[(0x4000, 512, true)]);
    d.write_only(0x138, 1);
    gate.wait_arrived(1);
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let stop = scope.spawn(|| {
            // Clearing PxCMD.ST returns only when the write has left the disk.
            d.write_only(0x118, 0x10);
            stopped.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!stopped.load(Ordering::SeqCst));
        gate.set(true);
        stop.join().unwrap();
    });
    assert_eq!(d.read(0x138), 0);
    assert_eq!(d.read(0x110), 0);
    assert_eq!(le32(&d.bytes(0x1004, 4)), 0);
    assert!(!d.irq.level.load(Ordering::SeqCst));
    // The port takes commands again once it is restarted.
    d.write(0x118, 0x11);
    assert_eq!(d.command(0x25, 2, 1, false), 0x50);
    assert_eq!(d.bytes(0x4000, 512), vec![0x3c; 512]);
}

#[test]
fn a_machine_reset_waits_for_the_command_in_flight() {
    let (mut d, gate) = gated();
    d.prepare(0xe7, 0, 0, 0, None, &[]);
    d.write_only(0x138, 1);
    gate.wait_arrived(1);
    let reset = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let thread = scope.spawn(|| {
            crate::Resettable::reset(&mut d.hba).unwrap();
            reset.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!reset.load(Ordering::SeqCst));
        gate.set(true);
        thread.join().unwrap();
    });
    assert_eq!(d.read(0x138), 0);
    assert_eq!(d.read(0x118), 6);
    assert!(!d.irq.level.load(Ordering::SeqCst));
}
