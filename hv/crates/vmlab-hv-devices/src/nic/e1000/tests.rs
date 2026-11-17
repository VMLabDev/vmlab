//! P4 tests at the guest interface: what a driver programs and reads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use vm_memory::{Bytes, GuestAddress};

use super::*;

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x14, 0x9a, 0xbb];
const TX_RING: u64 = 0x1_0000;
const RX_RING: u64 = 0x2_0000;
const BUFFERS: u64 = 0x3_0000;

#[derive(Default)]
struct Line(AtomicBool);

impl IrqLine for Line {
    fn set_level(&self, high: bool) {
        self.0.store(high, Ordering::SeqCst);
    }
}

struct Nic {
    dev: E1000,
    mem: GuestMemory,
    line: Arc<Line>,
}

impl Nic {
    fn new() -> Self {
        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let line = Arc::new(Line::default());
        let mut dev = E1000::new("e1000", MAC, mem.clone(), line.clone()).unwrap();
        dev.config_write(1, 0, &[0x07]); // I/O, memory, bus master
        dev.link_changed(true);
        let mut nic = Self { dev, mem, line };
        nic.read(ICR);
        nic
    }

    fn read(&mut self, offset: usize) -> u32 {
        let mut data = [0; 4];
        self.dev.bar_read(0, offset as u64, &mut data);
        u32::from_le_bytes(data)
    }

    fn write(&mut self, offset: usize, value: u32) {
        self.dev.bar_write(0, offset as u64, &value.to_le_bytes());
    }

    fn irq(&self) -> bool {
        self.line.0.load(Ordering::SeqCst)
    }

    fn tx_ring(&mut self, entries: u32) {
        self.write(TDBAL, TX_RING as u32);
        self.write(TDBAH, 0);
        self.write(TDLEN, entries * 16);
        self.write(TDH, 0);
        self.write(TDT, 0);
        self.write(TCTL, TCTL_EN | 0x0f << 4 | 0x40 << 12);
    }

    fn put_tx(&self, index: u64, desc: [u8; 16]) {
        self.mem
            .write_slice(&desc, GuestAddress(TX_RING + index * 16))
            .unwrap();
    }

    fn rx_ring(&mut self, entries: u32, rctl: u32) {
        for i in 0..u64::from(entries) {
            let mut desc = [0u8; 16];
            desc[..8].copy_from_slice(&(BUFFERS + i * 0x1000).to_le_bytes());
            self.mem
                .write_slice(&desc, GuestAddress(RX_RING + i * 16))
                .unwrap();
        }
        self.write(RDBAL, RX_RING as u32);
        self.write(RDLEN, entries * 16);
        self.write(RDH, 0);
        self.write(RDT, entries - 1);
        self.write(RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC | rctl);
    }

    fn rx_desc(&self, index: u64) -> [u8; 16] {
        let mut desc = [0u8; 16];
        self.mem
            .read_slice(&mut desc, GuestAddress(RX_RING + index * 16))
            .unwrap();
        desc
    }
}

fn legacy(buffer: u64, length: u16, cmd: u8) -> [u8; 16] {
    let mut desc = [0u8; 16];
    desc[..8].copy_from_slice(&buffer.to_le_bytes());
    desc[8..10].copy_from_slice(&length.to_le_bytes());
    desc[11] = cmd;
    desc
}

/// A UDP/IPv4 frame to us with correct checksums, `payload` bytes long.
fn udp_frame(payload: usize) -> Vec<u8> {
    let mut f = MAC.to_vec();
    f.extend_from_slice(&[0x52, 0x54, 0, 0, 0, 1, 0x08, 0x00]);
    let total = (20 + 8 + payload) as u16;
    let mut ip = vec![
        0x45, 0, 0, 0, 0, 1, 0, 0, 64, 17, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2,
    ];
    ip[2..4].copy_from_slice(&total.to_be_bytes());
    let csum = !fold(ones_sum(0, &ip));
    ip[10..12].copy_from_slice(&csum.to_be_bytes());
    f.extend_from_slice(&ip);
    let mut udp = vec![0x04, 0x00, 0x00, 0x35, 0, 0, 0, 0];
    udp[4..6].copy_from_slice(&((8 + payload) as u16).to_be_bytes());
    udp.extend((0..payload).map(|i| i as u8));
    let mut sum = ones_sum(0, &ip[12..20]) + 17 + udp.len() as u32;
    sum = ones_sum(sum, &udp);
    udp[6..8].copy_from_slice(&(!fold(sum)).to_be_bytes());
    f.extend_from_slice(&udp);
    f
}

#[test]
fn unreadable_rx_descriptor_does_not_dma_to_zero() {
    let mut n = Nic::new();
    n.rx_ring(8, RCTL_UPE);
    n.write(RDBAL, 0x20_0000);
    n.mem.write_slice(&[0x5a; 128], GuestAddress(0)).unwrap();
    assert!(n.dev.receive_frame(&udp_frame(16), Instant::now()));
    let mut low = [0; 128];
    n.mem.read_slice(&mut low, GuestAddress(0)).unwrap();
    assert_eq!(low, [0x5a; 128]);
    assert_eq!(n.read(RDH), 0);
}

#[test]
fn restore_rejects_invalid_eeprom_wire_fields() {
    let mut n = Nic::new();
    let original = n.dev.save_state().unwrap();
    for (field, value) in [("count", 10), ("out_bits", 17), ("address", 64)] {
        let mut state: serde_json::Value = serde_json::from_slice(&original).unwrap();
        state["wire"][field] = value.into();
        assert!(
            n.dev
                .restore_state(&serde_json::to_vec(&state).unwrap())
                .is_err()
        );
        assert_eq!(n.dev.save_state().unwrap(), original);
    }
}

#[test]
fn non_read_eeprom_command_ignores_extra_clocks() {
    let mut n = Nic::new();
    for bit in [1, 0, 0, 0, 0, 0, 0, 0, 0].into_iter().chain([1; 32]) {
        n.write(EECD, EECD_CS | if bit != 0 { EECD_DI } else { 0 });
        n.write(EECD, EECD_CS | EECD_SK | if bit != 0 { EECD_DI } else { 0 });
    }
    assert_eq!(n.dev.wire.count, 9);
    let saved = n.dev.save_state().unwrap();
    n.dev.restore_state(&saved).unwrap();
}

#[test]
fn pci_identity_and_bars_match_the_capture() {
    let mut nic = Nic::new();
    // pc-seabios-legacy lspci: 86 80 0e 10 ... rev 03, class 0200,
    // subsystem 1af4:1100, pin A.
    assert_eq!(nic.dev.config_read(0), 0x100e_8086);
    assert_eq!(nic.dev.config_read(2), 0x0200_0003);
    assert_eq!(nic.dev.config_read(11), 0x1100_1af4);
    assert_eq!(nic.dev.config_read(15) >> 8 & 0xff, 1);
    assert_eq!(
        nic.dev.bars(),
        vec![
            BarInfo {
                index: 0,
                size: 128 << 10,
                kind: BarKind::Mmio32
            },
            BarInfo {
                index: 1,
                size: 64,
                kind: BarKind::Io
            },
        ]
    );
    // The values the Windows 7 capture read first.
    assert_eq!(nic.read(CTRL), 0x0014_0240);
    assert_eq!(nic.read(STATUS), 0x8008_0783);
    assert_eq!(nic.read(EECD), 0x188);
    assert_eq!(nic.read(LEDCTL), 0x602);
    assert_eq!(nic.read(MANC), 0x0022_2300);
    assert_eq!(nic.read(TCTL), 0x8);
}

#[test]
fn io_window_reaches_registers_through_ioaddr_and_iodata() {
    let mut nic = Nic::new();
    nic.dev.bar_write(1, 0, &(IMS as u32).to_le_bytes());
    nic.dev.bar_write(1, 4, &0x9du32.to_le_bytes());
    assert_eq!(nic.read(IMS), 0x9d);
    let mut data = [0; 4];
    nic.dev.bar_read(1, 4, &mut data);
    assert_eq!(u32::from_le_bytes(data), 0x9d);
    // The capture's reset through the window: IOADDR=CTRL, IODATA=RST|...
    nic.dev.bar_write(1, 0, &0u32.to_le_bytes());
    nic.dev.bar_write(1, 4, &0x0414_0240u32.to_le_bytes());
    assert_eq!(nic.read(IMS), 0);
    assert_eq!(nic.read(CTRL), 0x0014_0240);
}

fn eeprom_read_bitbang(nic: &mut Nic, address: u8) -> u16 {
    let out = |nic: &mut Nic, value: u32| nic.write(EECD, value);
    out(nic, EECD_CS);
    // Start bit, READ (10), six address bits.
    let bits = 0b110 << 6 | u32::from(address);
    for i in (0..9).rev() {
        let di = if bits >> i & 1 != 0 { EECD_DI } else { 0 };
        out(nic, EECD_CS | di);
        out(nic, EECD_CS | di | EECD_SK);
        out(nic, EECD_CS | di);
    }
    assert_eq!(nic.read(EECD) & 8, 0, "dummy zero precedes the data");
    let mut word = 0u16;
    for _ in 0..16 {
        out(nic, EECD_CS | EECD_SK);
        word = word << 1 | ((nic.read(EECD) >> 3) & 1) as u16;
        out(nic, EECD_CS);
    }
    out(nic, 0);
    word
}

#[test]
fn eeprom_holds_the_mac_ids_and_checksum_over_both_interfaces() {
    let mut nic = Nic::new();
    let mut sum = 0u16;
    for address in 0..64u8 {
        let word = eeprom_read_bitbang(&mut nic, address);
        nic.write(EERD, u32::from(address) << 8 | 1);
        let eerd = nic.read(EERD);
        assert_ne!(eerd & 1 << 4, 0, "EERD done");
        assert_eq!((eerd >> 16) as u16, word, "word {address}");
        sum = sum.wrapping_add(word);
    }
    assert_eq!(sum, 0xbaba);
    assert_eq!(eeprom_read_bitbang(&mut nic, 0), 0x5452);
    assert_eq!(eeprom_read_bitbang(&mut nic, 2), 0xbb9a);
    assert_eq!(eeprom_read_bitbang(&mut nic, 0x0d), 0x100e);
}

#[test]
fn phy_answers_through_mdic_and_negotiation_drops_then_restores_link() {
    let mut nic = Nic::new();
    let phy = |nic: &mut Nic, value: u32| {
        nic.write(MDIC, value);
        nic.read(MDIC)
    };
    // The capture's reads: read op, PHY 1, registers 2 and 3.
    assert_eq!(phy(&mut nic, 0x0822_0000), 0x1822_0141);
    assert_eq!(phy(&mut nic, 0x0823_0000), 0x1823_0c20);
    assert_eq!(phy(&mut nic, 0x0821_0000), 0x1821_796d);
    // A wrong PHY address errors.
    assert_ne!(phy(&mut nic, 0x0842_0000) & 1 << 30, 0);
    nic.write(IMS, ICR_LSC);
    // Restart negotiation (the capture's 0x1340 write).
    assert_eq!(phy(&mut nic, 0x0420_1340), 0x1420_1340);
    assert_eq!(phy(&mut nic, 0x0821_0000), 0x1821_7949);
    assert_eq!(nic.read(STATUS), 0x8008_0781);
    let later = Instant::now() + Duration::from_secs(1);
    nic.dev.poll_timers(later);
    assert_eq!(nic.read(STATUS), 0x8008_0783);
    assert!(nic.irq());
    assert_eq!(nic.read(ICR) & ICR_LSC, ICR_LSC);
    assert!(!nic.irq());
    assert_eq!(phy(&mut nic, 0x0820_0000), 0x1820_1140);
}

#[test]
fn legacy_transmit_gathers_buffers_inserts_checksum_and_writes_back() {
    let mut nic = Nic::new();
    nic.write(IMS, ICR_TXDW | ICR_TXQE);
    nic.tx_ring(8);
    let frame = udp_frame(40);
    let mut broken = frame.clone();
    broken[40..42].fill(0); // UDP checksum field, to be inserted
    // Pseudo-header sum in the field, as a driver leaves it.
    let pseudo = fold(ones_sum(0, &broken[26..34]) + 17 + 48);
    broken[40..42].copy_from_slice(&pseudo.to_be_bytes());
    nic.mem
        .write_slice(&broken[..20], GuestAddress(BUFFERS))
        .unwrap();
    nic.mem
        .write_slice(&broken[20..], GuestAddress(BUFFERS + 0x100))
        .unwrap();
    nic.put_tx(0, legacy(BUFFERS, 20, 0));
    let mut last = legacy(
        BUFFERS + 0x100,
        (broken.len() - 20) as u16,
        TXD_EOP | TXD_IC | TXD_RS,
    );
    last[10] = 40; // CSO
    last[13] = 34; // CSS
    nic.put_tx(1, last);
    nic.write(TDT, 2);
    assert_eq!(nic.dev.link().take_sent(), vec![frame]);
    assert_eq!(nic.read(TDH), 2);
    let mut status = [0u8; 1];
    nic.mem
        .read_slice(&mut status, GuestAddress(TX_RING + 16 + 12))
        .unwrap();
    assert_eq!(status[0] & 1, 1, "DD written back on RS");
    assert!(nic.irq());
    assert_eq!(nic.read(ICR), ICR_TXDW | ICR_TXQE);
    assert_eq!(nic.read(GPTC), 1);
    assert_eq!(nic.read(GPTC), 0, "statistics clear on read");
}

fn context(
    ipcss: u8,
    ipcso: u8,
    tucss: u8,
    tucso: u8,
    tucmd: u8,
    hdrlen: u8,
    mss: u16,
) -> [u8; 16] {
    let mut desc = [0u8; 16];
    desc[0] = ipcss;
    desc[1] = ipcso;
    // IPCSE: the IP header's last byte (0 would mean the end of the packet).
    desc[2..4].copy_from_slice(&u16::from(tucss - 1).to_le_bytes());
    desc[4] = tucss;
    desc[5] = tucso;
    desc[11] = tucmd | TXD_DEXT;
    desc[13] = hdrlen;
    desc[14..16].copy_from_slice(&mss.to_le_bytes());
    desc
}

fn data(buffer: u64, length: u32, dcmd: u8, popts: u8) -> [u8; 16] {
    let mut desc = [0u8; 16];
    desc[..8].copy_from_slice(&buffer.to_le_bytes());
    desc[8..12].copy_from_slice(&(length | 1 << 20).to_le_bytes());
    desc[11] = dcmd | TXD_DEXT;
    desc[13] = popts;
    desc
}

#[test]
fn extended_descriptors_offload_ip_and_udp_checksums() {
    let mut nic = Nic::new();
    nic.tx_ring(8);
    let frame = udp_frame(17);
    let mut zeroed = frame.clone();
    zeroed[24..26].fill(0);
    let pseudo = fold(ones_sum(0, &zeroed[26..34]) + 17 + 25);
    zeroed[40..42].copy_from_slice(&pseudo.to_be_bytes());
    nic.mem.write_slice(&zeroed, GuestAddress(BUFFERS)).unwrap();
    nic.put_tx(0, context(14, 24, 34, 40, TUCMD_IP, 0, 0));
    nic.put_tx(
        1,
        data(
            BUFFERS,
            zeroed.len() as u32,
            TXD_EOP | TXD_RS,
            POPTS_IXSM | POPTS_TXSM,
        ),
    );
    nic.write(TDT, 2);
    assert_eq!(nic.dev.link().take_sent(), vec![frame]);
}

/// Review F1: TSO offsets are the guest's. A context whose IP checksum
/// field and TCP header start lie past every segment must still send the
/// segments, untouched there, and never index or subtract past them.
#[test]
fn segmentation_with_offsets_past_the_segment_sends_without_panicking() {
    let mut nic = Nic::new();
    nic.tx_ring(8);
    let header = 20usize;
    let mut frame = MAC.to_vec();
    frame.extend_from_slice(&[0x52, 0x54, 0, 0, 0, 1, 0x08, 0x00]);
    frame.resize(header, 0xaa);
    frame.extend((0..30).map(|i| i as u8));
    nic.mem.write_slice(&frame, GuestAddress(BUFFERS)).unwrap();
    // IPCSO 250, TUCSS 255, TUCSO 254, header 20 bytes, MSS 10.
    nic.put_tx(
        0,
        context(
            14,
            250,
            255,
            254,
            TUCMD_IP | TUCMD_TCP | TXD_TSE,
            header as u8,
            10,
        ),
    );
    nic.put_tx(
        1,
        data(
            BUFFERS,
            frame.len() as u32,
            TXD_EOP | TXD_RS | TXD_TSE,
            POPTS_IXSM | POPTS_TXSM,
        ),
    );
    nic.write(TDT, 2);
    let sent = nic.dev.link().take_sent();
    assert_eq!(sent.len(), 3);
    for (i, segment) in sent.iter().enumerate() {
        assert_eq!(&segment[..header], &frame[..header], "header untouched");
        assert_eq!(
            &segment[header..],
            &frame[header + 10 * i..header + 10 * (i + 1)]
        );
    }
    assert_eq!(nic.read(TDH), 2);
}

#[test]
fn tcp_segmentation_splits_payload_and_fixes_headers() {
    let mut nic = Nic::new();
    nic.tx_ring(8);
    // Ethernet + IPv4 + TCP header (54 bytes), 3000 payload bytes, MSS 1000.
    let mut frame = MAC.to_vec();
    frame.extend_from_slice(&[0x52, 0x54, 0, 0, 0, 1, 0x08, 0x00]);
    let mut ip = vec![
        0x45, 0, 0, 0, 0x12, 0x34, 0x40, 0, 64, 6, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2,
    ];
    frame.append(&mut ip);
    let mut tcp = vec![0u8; 20];
    tcp[4..8].copy_from_slice(&1000u32.to_be_bytes());
    tcp[12] = 0x50;
    tcp[13] = 0x18 | 0x01; // PSH ACK FIN
    let pseudo = fold(ones_sum(0, &frame[26..34]) + 6);
    tcp[16..18].copy_from_slice(&pseudo.to_be_bytes());
    frame.append(&mut tcp);
    frame.extend((0..3000).map(|i| (i % 251) as u8));
    nic.mem.write_slice(&frame, GuestAddress(BUFFERS)).unwrap();
    nic.put_tx(
        0,
        context(14, 24, 34, 50, TUCMD_IP | TUCMD_TCP | TXD_TSE, 54, 1000),
    );
    nic.put_tx(
        1,
        data(
            BUFFERS,
            frame.len() as u32,
            TXD_EOP | TXD_RS | TXD_TSE,
            POPTS_IXSM | POPTS_TXSM,
        ),
    );
    nic.write(TDT, 2);
    let sent = nic.dev.link().take_sent();
    assert_eq!(sent.len(), 3);
    for (i, segment) in sent.iter().enumerate() {
        assert_eq!(segment.len(), 54 + 1000);
        assert_eq!(u16::from_be_bytes([segment[16], segment[17]]), 1040);
        assert_eq!(
            u16::from_be_bytes([segment[18], segment[19]]),
            0x1234 + i as u16
        );
        assert_eq!(fold(ones_sum(0, &segment[14..34])), 0xffff, "IP checksum");
        let seq = u32::from_be_bytes(segment[38..42].try_into().unwrap());
        assert_eq!(seq, 1000 + 1000 * i as u32);
        let flags = segment[47];
        assert_eq!(flags & 0x09 != 0, i == 2, "FIN/PSH only on the last");
        let sum = ones_sum(0, &segment[26..34]) + 6 + 1020;
        assert_eq!(fold(ones_sum(sum, &segment[34..])), 0xffff, "TCP checksum");
        assert_eq!(&segment[54..], &frame[54 + 1000 * i..54 + 1000 * (i + 1)]);
    }
    assert_eq!(nic.read(TSCTC), 1);
}

#[test]
fn receive_fills_descriptors_checks_sums_and_raises_rxt0() {
    let mut nic = Nic::new();
    nic.write(IMS, ICR_RXT0);
    nic.write(RXCSUM, RXCSUM_IPOFL | RXCSUM_TUOFL);
    nic.rx_ring(8, 0);
    let frame = udp_frame(100);
    assert!(nic.dev.receive(&frame));
    let desc = nic.rx_desc(0);
    assert_eq!(u16::from_le_bytes([desc[8], desc[9]]) as usize, frame.len());
    assert_eq!(desc[12], RXD_DD | RXD_EOP | RXD_IPCS | RXD_TCPCS);
    assert_eq!(desc[13], 0);
    let mut got = vec![0; frame.len()];
    nic.mem.read_slice(&mut got, GuestAddress(BUFFERS)).unwrap();
    assert_eq!(got, frame);
    assert_eq!(nic.read(RDH), 1);
    assert!(nic.irq());
    assert_eq!(nic.read(ICR) & ICR_RXT0, ICR_RXT0);

    // A corrupted UDP checksum is reported, not dropped.
    let mut bad = udp_frame(100);
    bad[41] ^= 0xff;
    assert!(nic.dev.receive(&bad));
    assert_eq!(nic.rx_desc(1)[13], RXE_TCPE);
}

#[test]
fn short_frames_pad_and_crc_is_kept_unless_stripped() {
    let mut nic = Nic::new();
    nic.rx_ring(8, 0);
    nic.write(RCTL, RCTL_EN | RCTL_BAM);
    let frame = [
        &MAC[..],
        &[0x52, 0x54, 0, 0, 0, 1, 0x08, 0x06][..],
        &[1, 2, 3][..],
    ]
    .concat();
    assert!(nic.dev.receive(&frame));
    let desc = nic.rx_desc(0);
    assert_eq!(
        u16::from_le_bytes([desc[8], desc[9]]),
        64,
        "60 padded + 4 FCS"
    );
    let mut got = vec![0; 64];
    nic.mem.read_slice(&mut got, GuestAddress(BUFFERS)).unwrap();
    let mut padded = frame.clone();
    padded.resize(60, 0);
    assert_eq!(&got[..60], &padded[..]);
    assert_eq!(&got[60..], &fcs(&padded));
}

#[test]
fn large_frames_span_descriptors_and_wait_for_enough_of_them() {
    let mut nic = Nic::new();
    // BSIZE 01 = 1024 bytes, LPE for a 3000-byte frame.
    // RDLEN counts in 128-byte (8-descriptor) units.
    nic.rx_ring(8, 1 << 16 | RCTL_LPE);
    let frame = udp_frame(3000 - 42);
    nic.write(RDT, 2); // only two free descriptors: not enough
    assert!(!nic.dev.receive(&frame), "held for buffers");
    nic.write(RDT, 3);
    assert!(nic.dev.receive(&frame));
    let lengths: Vec<u16> = (0..3)
        .map(|i| {
            let d = nic.rx_desc(i);
            u16::from_le_bytes([d[8], d[9]])
        })
        .collect();
    assert_eq!(lengths, vec![1024, 1024, 952]);
    assert_eq!(nic.rx_desc(0)[12] & RXD_EOP, 0);
    assert_ne!(nic.rx_desc(2)[12] & RXD_EOP, 0);
    // Without LPE the same frame is dropped as oversized.
    let mut nic = Nic::new();
    nic.rx_ring(8, 1 << 16);
    assert!(nic.dev.receive(&frame));
    assert_eq!(nic.read(RDH), 0);
    assert_eq!(nic.read(ROC), 1);
}

#[test]
fn receive_filters_follow_ra_mta_broadcast_and_vlan_tables() {
    let mut nic = Nic::new();
    nic.rx_ring(16, 0);
    let mut other = udp_frame(10);
    other[5] ^= 1;
    assert!(nic.dev.receive(&other));
    assert_eq!(nic.read(RDH), 0, "foreign unicast filtered");
    let mut broadcast = udp_frame(10);
    broadcast[..6].fill(0xff);
    assert!(nic.dev.receive(&broadcast));
    assert_eq!(nic.read(RDH), 1);
    // Multicast 01:00:5e:00:00:fb, hashed with MO=00.
    let mut multicast = udp_frame(10);
    multicast[..6].copy_from_slice(&[1, 0, 0x5e, 0, 0, 0xfb]);
    assert!(nic.dev.receive(&multicast));
    assert_eq!(nic.read(RDH), 1, "not in the MTA");
    // MO=00 hashes destination bits 47:36: byte 4's high nibble, byte 5.
    let hash = (u16::from(multicast[4]) >> 4) | u16::from(multicast[5]) << 4;
    nic.write(MTA + 4 * (hash as usize >> 5), 1 << (hash & 31));
    assert!(nic.dev.receive(&multicast));
    assert_eq!(nic.read(RDH), 2);
    // Promiscuous unicast.
    let rctl = nic.read(RCTL);
    nic.write(RCTL, rctl | RCTL_UPE);
    assert!(nic.dev.receive(&other));
    assert_eq!(nic.read(RDH), 3);
    // VLAN filtering and stripping: VID 5 not in the table, then in it.
    let mut tagged = udp_frame(10);
    tagged.splice(12..12, [0x81, 0x00, 0x00, 0x05]);
    let rctl = nic.read(RCTL);
    nic.write(RCTL, rctl | RCTL_VFE);
    let ctrl = nic.read(CTRL);
    nic.write(CTRL, ctrl | CTRL_VME);
    assert!(nic.dev.receive(&tagged));
    assert_eq!(nic.read(RDH), 3);
    nic.write(VFTA, 1 << 5);
    assert!(nic.dev.receive(&tagged));
    let desc = nic.rx_desc(3);
    assert_ne!(desc[12] & RXD_VP, 0);
    assert_eq!(u16::from_le_bytes([desc[14], desc[15]]), 5);
}

#[test]
fn interrupt_mask_set_clear_and_cause_set() {
    let mut nic = Nic::new();
    nic.write(ICS, ICR_RXT0);
    assert!(!nic.irq(), "masked");
    nic.write(IMS, ICR_RXT0 | ICR_TXDW);
    assert!(nic.irq());
    nic.write(IMC, ICR_RXT0);
    assert!(!nic.irq());
    assert_eq!(nic.read(IMS), ICR_TXDW);
    nic.write(IMS, ICR_RXT0);
    // INTx disable in the command register holds the line low.
    nic.dev.config_write(1, 1, &[0x04]);
    assert!(!nic.irq());
    nic.dev.config_write(1, 1, &[0x00]);
    assert!(nic.irq());
    assert_ne!(nic.dev.config_read(1) & 0x0008_0000, 0, "interrupt status");
    assert_eq!(nic.read(ICR), ICR_RXT0);
    assert!(!nic.irq());
}

#[test]
fn receive_delay_and_absolute_timers_defer_rxt0() {
    let mut nic = Nic::new();
    nic.write(IMS, ICR_RXT0);
    nic.rx_ring(8, 0);
    nic.write(RDTR, 100); // ~102 us
    nic.write(RADV, 1000);
    assert!(nic.dev.receive(&udp_frame(10)));
    assert!(!nic.irq(), "delayed");
    let deadline = nic
        .dev
        .poll_timers(Instant::now())
        .expect("a timer is armed");
    nic.dev.poll_timers(deadline + Duration::from_micros(1));
    assert!(nic.irq());
    nic.read(ICR);
    // A flush through RDTR.FPD fires a pending timer at once.
    assert!(nic.dev.receive(&udp_frame(10)));
    assert!(!nic.irq());
    nic.write(RDTR, 100 | 1 << 31);
    assert!(nic.irq());
}

#[test]
fn throttling_holds_a_new_assertion_for_the_itr_interval() {
    let mut nic = Nic::new();
    nic.write(IMS, ICR_TXDW);
    nic.write(ITR, 4000); // ~1 ms
    nic.write(ICS, ICR_TXDW);
    assert!(nic.irq());
    nic.read(ICR);
    nic.write(ICS, ICR_TXDW);
    assert!(!nic.irq(), "inside the interval");
    let next = nic.dev.poll_timers(Instant::now()).unwrap();
    nic.dev.poll_timers(next + Duration::from_micros(1));
    assert!(nic.irq());
}

#[test]
fn snapshot_round_trip_preserves_rings_filters_and_pending_interrupts() {
    let mut nic = Nic::new();
    nic.write(IMS, ICR_RXT0);
    nic.rx_ring(8, 0);
    nic.tx_ring(8);
    nic.write(MTA, 0xdead_beef);
    assert!(nic.dev.receive(&udp_frame(10)));
    assert!(nic.irq());
    Snapshot::quiesce(&mut nic.dev);
    assert!(
        !nic.dev.receive(&udp_frame(10)),
        "quiesced: no guest access"
    );
    let state = nic.dev.save_state().unwrap();
    Snapshot::resume_snapshot(&mut nic.dev);

    let mut restored = Nic::new();
    restored.dev.restore_state(&state).unwrap();
    assert!(restored.irq());
    for offset in [RDH, RDT, RDLEN, TDLEN, MTA, RCTL, IMS, TCTL] {
        assert_eq!(restored.read(offset), nic.read(offset), "{offset:#x}");
    }
    assert!(restored.dev.restore_state(b"{}").is_err());
}

#[test]
fn reset_returns_power_on_state() {
    let mut nic = Nic::new();
    nic.write(IMS, ICR_RXT0);
    nic.rx_ring(8, 0);
    nic.write(ICS, ICR_RXT0);
    assert!(nic.irq());
    nic.dev.reset().unwrap();
    assert!(!nic.irq());
    assert_eq!(nic.dev.config_read(1) & 0xffff, 0);
    nic.dev.config_write(1, 0, &[0x07]);
    assert_eq!(nic.read(RCTL), 0);
    assert_eq!(nic.read(IMS), 0);
    assert_eq!(nic.read(CTRL), 0x0014_0240);
    assert_eq!(nic.read(RAL0), 0x1400_5452);
    assert_eq!(nic.read(RAL0 + 4), 0x8000_bb9a);
}

#[test]
fn descriptors_outside_guest_memory_are_skipped_not_followed() {
    let mut nic = Nic::new();
    nic.tx_ring(8);
    nic.put_tx(0, legacy(0xffff_0000_0000, 1500, TXD_EOP | TXD_RS));
    nic.write(TDT, 1);
    assert!(nic.dev.link().take_sent().is_empty());
    assert_eq!(nic.read(TDH), 1);
    nic.write(TDBAL, 0xfff0_0000);
    nic.write(TDBAH, 0xffff);
    nic.write(TDT, 2);
}
