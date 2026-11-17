//! P4 tests at the guest interface: what a driver programs and reads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use vm_memory::{Bytes, GuestAddress};

use super::*;

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x09, 0x78, 0xdf];
const INIT_BLOCK: u64 = 0x2_44b4;
const RX_RING: u64 = 0x2_4460;
const TX_RING: u64 = 0x2_4458;
const BUFFERS: u64 = 0x3_0000;

#[test]
fn guest_ring_lengths_are_clamped_at_use() {
    let mut n = Nic::new();
    for length in [0, 1, 512, 513, u16::MAX] {
        n.set_csr(76, length);
        n.set_csr(78, length);
        assert_eq!(n.dev.ring(true).1, u32::from(length).clamp(1, 512));
        assert_eq!(n.dev.ring(false).1, u32::from(length).clamp(1, 512));
    }
}

#[derive(Default)]
struct Line(AtomicBool);

impl IrqLine for Line {
    fn set_level(&self, high: bool) {
        self.0.store(high, Ordering::SeqCst);
    }
}

struct Nic {
    dev: Pcnet,
    mem: GuestMemory,
    line: Arc<Line>,
}

impl Nic {
    fn new() -> Self {
        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let line = Arc::new(Line::default());
        let mut dev = Pcnet::new("pcnet", MAC, mem.clone(), line.clone()).unwrap();
        dev.config_write(1, 0, &[0x07]);
        dev.link_changed(true);
        Self { dev, mem, line }
    }

    fn csr(&mut self, index: u16) -> u16 {
        self.dev.bar_write(0, 0x12, &index.to_le_bytes());
        let mut data = [0; 2];
        self.dev.bar_read(0, 0x10, &mut data);
        u16::from_le_bytes(data)
    }

    fn set_csr(&mut self, index: u16, value: u16) {
        self.dev.bar_write(0, 0x12, &index.to_le_bytes());
        self.dev.bar_write(0, 0x10, &value.to_le_bytes());
    }

    fn bcr(&mut self, index: u16) -> u16 {
        self.dev.bar_write(0, 0x12, &index.to_le_bytes());
        let mut data = [0; 2];
        self.dev.bar_read(0, 0x16, &mut data);
        u16::from_le_bytes(data)
    }

    fn set_bcr(&mut self, index: u16, value: u16) {
        self.dev.bar_write(0, 0x12, &index.to_le_bytes());
        self.dev.bar_write(0, 0x16, &value.to_le_bytes());
    }

    fn irq(&self) -> bool {
        self.line.0.load(Ordering::SeqCst)
    }

    /// The FreeDOS driver's style-0 setup: 8 receive and 1 transmit
    /// descriptors (`pcnet_rlen_tlen rlen=3 tlen=0` in the capture).
    fn init_style0(&mut self, mode: u16, ladrf: [u16; 4], rx_entries_log2: u16, tx_log2: u16) {
        let mut block = Vec::new();
        block.extend_from_slice(&mode.to_le_bytes());
        block.extend_from_slice(&MAC);
        for word in ladrf {
            block.extend_from_slice(&word.to_le_bytes());
        }
        block.extend_from_slice(&(RX_RING as u16).to_le_bytes());
        block.extend_from_slice(&((RX_RING >> 16) as u16 | rx_entries_log2 << 13).to_le_bytes());
        block.extend_from_slice(&(TX_RING as u16).to_le_bytes());
        block.extend_from_slice(&((TX_RING >> 16) as u16 | tx_log2 << 13).to_le_bytes());
        self.mem
            .write_slice(&block, GuestAddress(INIT_BLOCK))
            .unwrap();
        for i in 0..(1u64 << rx_entries_log2) {
            self.rx16(i, BUFFERS + i * 0x800, 1536, true);
        }
        self.set_csr(1, INIT_BLOCK as u16);
        self.set_csr(2, (INIT_BLOCK >> 16) as u16);
        // A driver keeps IENA as it is: writing it 0 clears it.
        let iena = self.csr(0) & IENA;
        self.set_csr(0, INIT | iena);
    }

    fn rx16(&self, index: u64, buffer: u64, length: u16, own: bool) {
        let mut desc = [0u8; 8];
        desc[..2].copy_from_slice(&(buffer as u16).to_le_bytes());
        desc[2] = (buffer >> 16) as u8;
        desc[3] = if own { 0x80 } else { 0 };
        desc[4..6].copy_from_slice(&(0u16.wrapping_sub(length) | 0xf000).to_le_bytes());
        self.mem
            .write_slice(&desc, GuestAddress(RX_RING + index * 8))
            .unwrap();
    }

    fn tx16(&self, ring: u64, index: u64, buffer: u64, length: u16, flags: u8) {
        let mut desc = [0u8; 8];
        desc[..2].copy_from_slice(&(buffer as u16).to_le_bytes());
        desc[2] = (buffer >> 16) as u8;
        desc[3] = flags;
        desc[4..6].copy_from_slice(&(0u16.wrapping_sub(length) | 0xf000).to_le_bytes());
        self.mem
            .write_slice(&desc, GuestAddress(ring + index * 8))
            .unwrap();
    }

    fn rx16_status(&self, index: u64) -> (u8, u16) {
        let mut desc = [0u8; 8];
        self.mem
            .read_slice(&mut desc, GuestAddress(RX_RING + index * 8))
            .unwrap();
        (desc[3], u16::from_le_bytes([desc[6], desc[7]]))
    }
}

fn frame_to(destination: [u8; 6], length: usize) -> Vec<u8> {
    let mut frame = destination.to_vec();
    frame.extend_from_slice(&[0x52, 0x54, 0, 0, 0, 1, 0x08, 0x00]);
    frame.extend((0..length - 14).map(|i| i as u8));
    frame
}

#[test]
fn pci_identity_matches_the_capture() {
    let mut nic = Nic::new();
    // pc-seabios-legacy lspci: 22 10 00 20, status 0280, rev 10, class
    // 0200, no subsystem, pin A, MIN_GNT 06, MAX_LAT ff.
    let mut fresh = Pcnet::new("p", MAC, nic.mem.clone(), nic.line.clone()).unwrap();
    assert_eq!(fresh.config_read(0), 0x2000_1022);
    assert_eq!(fresh.config_read(1), 0x0280_0000);
    assert_eq!(fresh.config_read(2), 0x0200_0010);
    assert_eq!(fresh.config_read(11), 0);
    assert_eq!(fresh.config_read(15), 0xff06_0100);
    assert_eq!(
        nic.dev.bars(),
        vec![
            BarInfo {
                index: 0,
                size: 32,
                kind: BarKind::Io
            },
            BarInfo {
                index: 1,
                size: 32,
                kind: BarKind::Mmio32
            },
        ]
    );
    assert_eq!(nic.bcr(22), 0xff06);
}

#[test]
fn address_prom_matches_the_capture() {
    let mut nic = Nic::new();
    // pcnet_aprom_readb values from the FreeDOS capture.
    let expected = [
        0x52, 0x54, 0x00, 0x09, 0x78, 0xdf, 0, 0, 0, 0x11, 0, 0, 0xc5, 0x02, 0x57, 0x57,
    ];
    for (i, byte) in expected.iter().enumerate() {
        let mut data = [0];
        nic.dev.bar_read(0, i as u64, &mut data);
        assert_eq!(data[0], *byte, "PROM byte {i}");
    }
    let mut word = [0; 4];
    nic.dev.bar_read(1, 0, &mut word);
    assert_eq!(word, [0x52, 0x54, 0x00, 0x09], "MMIO alias");
}

#[test]
fn reset_register_stops_the_chip_and_chip_id_reads_back() {
    let mut nic = Nic::new();
    assert_eq!(nic.csr(0), STOP);
    assert_eq!(nic.csr(88), 0x1003);
    assert_eq!(nic.csr(89), 0x0262);
    assert_eq!(nic.csr(4), 0x0115);
    nic.set_csr(15, 0x8000);
    let mut data = [0; 2];
    nic.dev.bar_read(0, 0x14, &mut data);
    assert_eq!(nic.csr(15), 0, "S_RESET");
    assert_eq!(nic.bcr(2), 2);
}

#[test]
fn dword_write_to_rdp_switches_to_dword_io() {
    let mut nic = Nic::new();
    nic.dev.bar_write(0, 0x10, &0u32.to_le_bytes());
    // Now RAP is at 0x14 and BDP at 0x1c, both 32 bits wide.
    nic.dev.bar_write(0, 0x14, &20u32.to_le_bytes());
    nic.dev.bar_write(0, 0x1c, &2u32.to_le_bytes());
    let mut data = [0; 4];
    nic.dev.bar_read(0, 0x1c, &mut data);
    assert_eq!(
        u32::from_le_bytes(data),
        0x0302,
        "style 2: SSIZE32, CSRPCNET"
    );
    nic.dev.bar_write(0, 0x14, &58u32.to_le_bytes());
    nic.dev.bar_read(0, 0x10, &mut data);
    assert_eq!(u32::from_le_bytes(data), 0x0302, "CSR58 mirrors BCR20");
    // Word accesses no longer reach the registers.
    let mut word = [0; 2];
    nic.dev.bar_read(0, 0x16, &mut word);
    assert_eq!(word, [0xff, 0xff]);
}

#[test]
fn style0_init_block_loads_mode_address_and_rings_and_raises_idon() {
    let mut nic = Nic::new();
    nic.set_csr(0, IENA);
    nic.init_style0(0, [0x1234, 0, 0, 0x8000], 3, 0);
    assert_eq!(nic.csr(0), IDON | INTR | IENA, "init done, STOP cleared");
    assert!(nic.irq());
    assert_eq!(nic.csr(12), 0x5452);
    assert_eq!(nic.csr(14), 0xdf78);
    assert_eq!(nic.csr(8), 0x1234);
    assert_eq!(nic.csr(24), RX_RING as u16);
    assert_eq!(nic.csr(25), (RX_RING >> 16) as u16);
    assert_eq!(nic.csr(76), 8);
    assert_eq!(nic.csr(78), 1);
    // IDON acknowledged by writing it back; IDONM masks it otherwise.
    nic.set_csr(0, IDON | IENA);
    assert!(!nic.irq());
    nic.set_csr(3, IDON);
    nic.set_csr(0, INIT | IENA);
    assert!(!nic.irq(), "masked by CSR3.IDONM");
}

#[test]
fn style0_transmit_and_receive_with_chained_buffers() {
    let mut nic = Nic::new();
    nic.init_style0(0, [0; 4], 3, 1);
    nic.set_csr(0, IDON | STRT | IENA);
    assert_eq!(nic.csr(0) & (TXON | RXON | STRT), TXON | RXON | STRT);

    // A frame in two buffers: STP on the first, ENP on the second.
    let frame = frame_to([0xff; 6], 100);
    nic.mem
        .write_slice(&frame[..40], GuestAddress(BUFFERS + 0x8000))
        .unwrap();
    nic.mem
        .write_slice(&frame[40..], GuestAddress(BUFFERS + 0x9000))
        .unwrap();
    nic.tx16(TX_RING, 0, BUFFERS + 0x8000, 40, 0x80 | 0x02);
    nic.tx16(TX_RING, 1, BUFFERS + 0x9000, 60, 0x80 | 0x01);
    nic.set_csr(0, TDMD | IENA);
    assert_eq!(nic.dev.link().take_sent(), vec![frame]);
    let mut flags = [0u8; 1];
    nic.mem
        .read_slice(&mut flags, GuestAddress(TX_RING + 3))
        .unwrap();
    assert_eq!(flags[0] & 0x80, 0, "OWN returned to the host");
    assert_ne!(nic.csr(0) & TINT, 0);
    assert!(nic.irq());
    nic.set_csr(0, TINT | IENA);
    assert!(!nic.irq());

    // Receive into 64-byte buffers: a 200-byte frame + FCS spans four.
    for i in 0..8u64 {
        nic.rx16(i, BUFFERS + i * 0x100, 64, true);
    }
    let incoming = frame_to(MAC, 200);
    assert!(nic.dev.receive(&incoming));
    let flags: Vec<u8> = (0..4).map(|i| nic.rx16_status(i).0).collect();
    assert_eq!(flags[0], 0x02, "STP, not owned");
    assert_eq!(flags[1], 0x00);
    assert_eq!(flags[3] & 0x01, 0x01, "ENP");
    assert_eq!(nic.rx16_status(3).1, 204, "MCNT counts the FCS");
    let mut got = vec![0u8; 204];
    for i in 0..4u64 {
        let take = if i == 3 { 12 } else { 64 };
        nic.mem
            .read_slice(
                &mut got[i as usize * 64..i as usize * 64 + take],
                GuestAddress(BUFFERS + i * 0x100),
            )
            .unwrap();
    }
    assert_eq!(&got[..200], &incoming[..]);
    assert_eq!(&got[200..], &fcs(&incoming));
    assert_ne!(nic.csr(0) & RINT, 0);
    assert!(nic.irq());
}

#[test]
fn receive_waits_for_owned_descriptors() {
    let mut nic = Nic::new();
    nic.init_style0(0, [0; 4], 3, 0);
    nic.set_csr(0, IDON | STRT);
    for i in 0..8 {
        nic.rx16(i, BUFFERS + i * 0x800, 1536, false);
    }
    assert!(!nic.dev.receive(&frame_to(MAC, 60)), "no buffer: held");
    nic.rx16(0, BUFFERS, 1536, true);
    assert!(nic.dev.receive(&frame_to(MAC, 60)));
}

#[test]
fn filters_physical_broadcast_logical_and_promiscuous() {
    let mut nic = Nic::new();
    // Logical filter bit for 01:00:5e:00:00:01.
    let multicast = [1, 0, 0x5e, 0, 0, 1];
    let hash = (!u32::from_le_bytes(fcs(&multicast))) >> 26;
    let mut ladrf = [0u16; 4];
    ladrf[hash as usize >> 4] |= 1 << (hash & 15);
    nic.init_style0(0, ladrf, 3, 0);
    nic.set_csr(0, IDON | STRT);
    let mut delivered = 0;
    for (destination, accepted) in [
        (MAC, true),
        ([0x02, 0, 0, 0, 0, 9], false),
        ([0xff; 6], true),
        (multicast, true),
        ([1, 0, 0x5e, 0, 0, 2], false),
    ] {
        assert!(nic.dev.receive(&frame_to(destination, 60)));
        if accepted {
            delivered += 1;
        }
        assert_eq!(
            (0..8).filter(|i| nic.rx16_status(*i).0 & 0x80 == 0).count(),
            delivered,
            "{destination:x?}"
        );
    }
    // Promiscuous mode takes the foreign unicast; DRCVBC drops broadcast.
    nic.set_csr(0, STOP);
    nic.init_style0(PROM, [0; 4], 3, 0);
    nic.set_csr(0, IDON | STRT);
    assert!(nic.dev.receive(&frame_to([0x02, 0, 0, 0, 0, 9], 60)));
    assert_eq!(nic.rx16_status(0).0 & 0x80, 0);
    nic.set_csr(0, STOP);
    nic.init_style0(DRCVBC, [0; 4], 3, 0);
    nic.set_csr(0, IDON | STRT);
    assert!(nic.dev.receive(&frame_to([0xff; 6], 60)));
    assert_ne!(nic.rx16_status(0).0 & 0x80, 0, "broadcast refused");
}

/// Styles 2 and 3: 32-bit init block and 16-byte descriptors, style 3
/// with the address and status words swapped.
fn run_style32(style: u16) {
    let mut nic = Nic::new();
    nic.set_bcr(20, style);
    assert_eq!(nic.bcr(20), 0x0300 | style);
    let rx_ring = 0x5_0000u64;
    let tx_ring = 0x5_1000u64;
    let init = 0x5_2000u64;
    let mut block = Vec::new();
    block.extend_from_slice(&(2u32 << 20 | 1 << 28).to_le_bytes()); // RLEN 4, TLEN 2
    block.extend_from_slice(&MAC);
    block.extend_from_slice(&[0, 0]);
    block.extend_from_slice(&[0u8; 8]);
    block.extend_from_slice(&(rx_ring as u32).to_le_bytes());
    block.extend_from_slice(&(tx_ring as u32).to_le_bytes());
    nic.mem.write_slice(&block, GuestAddress(init)).unwrap();
    let (address_offset, misc_offset) = if style == 3 { (8, 0) } else { (0, 8) };
    for i in 0..4u64 {
        let mut desc = [0u8; 16];
        desc[address_offset..address_offset + 4]
            .copy_from_slice(&((BUFFERS + i * 0x800) as u32).to_le_bytes());
        desc[4..8]
            .copy_from_slice(&(0x8000_f000u32 | (0u32.wrapping_sub(1536) & 0xfff)).to_le_bytes());
        nic.mem
            .write_slice(&desc, GuestAddress(rx_ring + i * 16))
            .unwrap();
    }
    nic.set_csr(1, init as u16);
    nic.set_csr(2, (init >> 16) as u16);
    nic.set_csr(0, INIT);
    assert_eq!(nic.csr(76), 4);
    assert_eq!(nic.csr(78), 2);
    nic.set_csr(0, IDON | STRT);
    let incoming = frame_to(MAC, 300);
    assert!(nic.dev.receive(&incoming));
    let mut desc = [0u8; 16];
    nic.mem
        .read_slice(&mut desc, GuestAddress(rx_ring))
        .unwrap();
    let flags = u32::from_le_bytes(desc[4..8].try_into().unwrap());
    assert_eq!(flags >> 24 & 0x83, 0x03, "STP|ENP, not owned");
    assert_ne!(flags & 1 << 22, 0, "PAM: physical address match");
    let misc = u32::from_le_bytes(desc[misc_offset..misc_offset + 4].try_into().unwrap());
    assert_eq!(misc & 0xfff, 304);
    let mut got = vec![0; 300];
    nic.mem.read_slice(&mut got, GuestAddress(BUFFERS)).unwrap();
    assert_eq!(got, incoming);

    let out = frame_to([0xff; 6], 64);
    nic.mem
        .write_slice(&out, GuestAddress(BUFFERS + 0x4000))
        .unwrap();
    let mut tx = [0u8; 16];
    tx[address_offset..address_offset + 4]
        .copy_from_slice(&((BUFFERS + 0x4000) as u32).to_le_bytes());
    tx[4..8].copy_from_slice(&(0x8300_f000u32 | (0u32.wrapping_sub(64) & 0xfff)).to_le_bytes());
    nic.mem.write_slice(&tx, GuestAddress(tx_ring)).unwrap();
    nic.set_csr(0, TDMD);
    assert_eq!(nic.dev.link().take_sent(), vec![out]);
}

#[test]
fn style2_rings() {
    run_style32(2);
}

#[test]
fn style3_rings_with_swapped_words() {
    run_style32(3);
}

#[test]
fn transmit_polling_finds_new_descriptors_without_tdmd() {
    let mut nic = Nic::new();
    nic.init_style0(0, [0; 4], 3, 0);
    nic.set_csr(0, IDON | STRT);
    let out = frame_to([0xff; 6], 60);
    nic.mem
        .write_slice(&out, GuestAddress(BUFFERS + 0x8000))
        .unwrap();
    nic.tx16(TX_RING, 0, BUFFERS + 0x8000, 60, 0x80 | 0x03);
    let deadline = nic.dev.poll_timers(Instant::now()).expect("polling armed");
    nic.dev.poll_timers(deadline + Duration::from_micros(1));
    assert_eq!(nic.dev.link().take_sent(), vec![out]);
    // CSR4.TXDPOLL disables it.
    nic.set_csr(0, STOP);
    let csr4 = nic.csr(4);
    nic.set_csr(4, csr4 | TXDPOLL);
    nic.init_style0(0, [0; 4], 3, 0);
    nic.set_csr(0, IDON | STRT);
    assert_eq!(nic.dev.poll_timers(Instant::now()), None);
}

#[test]
fn stop_clears_everything_and_snapshot_round_trips() {
    let mut nic = Nic::new();
    nic.init_style0(0, [0; 4], 3, 0);
    nic.set_csr(0, IDON | STRT | IENA);
    assert!(nic.dev.receive(&frame_to(MAC, 60)));
    assert!(nic.irq());
    Snapshot::quiesce(&mut nic.dev);
    assert!(!nic.dev.receive(&frame_to(MAC, 60)), "quiesced");
    let state = nic.dev.save_state().unwrap();
    Snapshot::resume_snapshot(&mut nic.dev);

    // The same guest memory, as a restore in place has.
    let line = Arc::new(Line::default());
    let mut restored = Nic {
        dev: Pcnet::new("pcnet", MAC, nic.mem.clone(), line.clone()).unwrap(),
        mem: nic.mem.clone(),
        line,
    };
    restored.dev.link_changed(true);
    restored.dev.restore_state(&state).unwrap();
    assert!(restored.irq());
    assert_eq!(restored.csr(0), nic.csr(0));
    assert_eq!(restored.csr(76), 8);
    assert!(restored.dev.receive(&frame_to(MAC, 60)));
    assert_eq!(restored.rx16_status(1).0 & 0x80, 0, "continues at index 1");

    restored.set_csr(0, STOP);
    assert_eq!(restored.csr(0), STOP);
    assert!(!restored.irq());
    assert!(restored.dev.restore_state(b"{}").is_err());

    restored.dev.reset().unwrap();
    assert_eq!(restored.dev.config_read(1) & 0xffff, 0);
    assert_eq!(restored.bcr(20), 0x0200);
}
