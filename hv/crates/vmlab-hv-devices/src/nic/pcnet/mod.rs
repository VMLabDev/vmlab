//! AMD PCnet-PCI II Am79C970A, as QEMU's pc machine presents it (SPEC
//! phase 4b).
//!
//! Implemented from AMD publication 19436 (rev E): the address PROM, the
//! RAP/RDP/BDP register access in word and dword I/O mode (and the same
//! window in the MMIO BAR), RESET, the CSR and BCR state, initialization
//! blocks, software styles 0 (16-bit LANCE), 2 (32-bit) and 3 (32-bit with
//! the descriptor words reordered), descriptor ownership and chaining
//! (STP/ENP), the address filters, CSR0's interrupt sources with CSR3's
//! masks, and transmit polling. The PROM layout and the PCI identity are the
//! ones the `docs/hwref/pc-freedos-1.3` and `pc-seabios-legacy` captures
//! recorded.
//!
//! Descriptor and buffer accesses go through [`GuestMemory`]: an address
//! outside guest RAM fails that access, never a host access.

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vm_memory::{Address, Bytes, GuestAddress};

use super::{Link, LinkClient, fcs};
use crate::{
    BarInfo, BarKind, DeviceError, GuestMemory, IrqLine, PciDevice, PciSnapshot, Resettable,
    Snapshot,
};

/// 1022:2000 rev 0x10, no subsystem (the capture).
const VENDOR_DEVICE: u32 = 0x2000_1022;
const WINDOW: u64 = 32;

// CSR0 bits.
const INIT: u16 = 1 << 0;
const STRT: u16 = 1 << 1;
const STOP: u16 = 1 << 2;
const TDMD: u16 = 1 << 3;
const TXON: u16 = 1 << 4;
const RXON: u16 = 1 << 5;
const IENA: u16 = 1 << 6;
const INTR: u16 = 1 << 7;
const IDON: u16 = 1 << 8;
const TINT: u16 = 1 << 9;
const RINT: u16 = 1 << 10;
const MERR: u16 = 1 << 11;
const MISS: u16 = 1 << 12;
const CERR: u16 = 1 << 13;
const BABL: u16 = 1 << 14;
const ERR: u16 = 1 << 15;
/// Interrupt flags a CSR0 write clears by writing 1.
const CSR0_ACK: u16 = BABL | CERR | MISS | MERR | RINT | TINT | IDON;

// CSR3 masks (same bit positions as the CSR0 flags they mask).
const CSR3_MASKS: u16 = BABL | MISS | MERR | RINT | TINT | IDON;
// CSR4.
const TXDPOLL: u16 = 1 << 12;
const APAD_XMT: u16 = 1 << 11;
// CSR15 mode.
const DRX: u16 = 1 << 0;
const DTX: u16 = 1 << 1;
const DXMTFCS: u16 = 1 << 3;
const DRCVPA: u16 = 1 << 13;
const DRCVBC: u16 = 1 << 14;
const PROM: u16 = 1 << 15;

// Descriptor flag bits, in the 16-bit flags word (style 0's RMD1/TMD1 high
// byte, style 2/3's RMD1/TMD1 bits 31:16).
const OWN: u16 = 1 << 15;
const DERR: u16 = 1 << 14;
const ADD_FCS: u16 = 1 << 13;
const STP: u16 = 1 << 9;
const ENP: u16 = 1 << 8;
const RX_BUFF: u16 = 1 << 10;
const RX_BAM: u16 = 1 << 4;
const RX_LAFM: u16 = 1 << 5;
const RX_PAM: u16 = 1 << 6;
const TX_BUFF: u32 = 1 << 31;

/// Transmit ring polling interval while TXON (unless CSR4.TXDPOLL).
const TX_POLL: Duration = Duration::from_micros(1600);
const MAX_FRAME: usize = 16 << 10;

/// Power-on BCR values (19436 table "BCR registers", defaults).
const BCR_DEFAULTS: [(usize, u16); 8] = [
    (2, 0x0002),
    (4, 0x00c0),
    (5, 0x0084),
    (6, 0x0088),
    (7, 0x0090),
    (18, 0x9001),
    (20, 0x0200),
    (22, 0xff06),
];

pub struct Pcnet {
    id: String,
    memory: GuestMemory,
    irq: Arc<dyn IrqLine>,
    link: Link,
    config: [u8; 256],
    prom: [u8; 16],
    csr: [u16; 128],
    bcr: [u16; 32],
    rap: u16,
    dwio: bool,
    rx_index: u32,
    tx_index: u32,
    carrier: bool,
    next_poll: Option<Instant>,
    level: bool,
    quiesced: bool,
}

#[derive(Debug, Clone, Copy)]
struct Descriptor {
    address: u32,
    flags: u16,
    /// Two's-complement buffer byte count as stored (12 bits).
    bcnt: u16,
    /// RMD2's message byte count / TMD2's error word.
    misc: u32,
}

impl Pcnet {
    pub fn new(
        id: impl Into<String>,
        mac: [u8; 6],
        memory: GuestMemory,
        irq: Arc<dyn IrqLine>,
    ) -> std::io::Result<Self> {
        let mut prom = [0u8; 16];
        prom[..6].copy_from_slice(&mac);
        prom[9] = 0x11;
        prom[14] = b'W';
        prom[15] = b'W';
        let sum: u16 = prom[..12]
            .iter()
            .chain(&prom[14..])
            .map(|b| u16::from(*b))
            .sum();
        prom[12..14].copy_from_slice(&sum.to_le_bytes());
        let mut device = Self {
            id: id.into(),
            memory,
            irq,
            link: Link::new()?,
            config: Self::power_on_config(),
            prom,
            csr: [0; 128],
            bcr: [0; 32],
            rap: 0,
            dwio: false,
            rx_index: 0,
            tx_index: 0,
            carrier: false,
            next_poll: None,
            level: false,
            quiesced: false,
        };
        device.hardware_reset();
        Ok(device)
    }

    fn power_on_config() -> [u8; 256] {
        let mut config = [0u8; 256];
        config[..4].copy_from_slice(&VENDOR_DEVICE.to_le_bytes());
        config[6..8].copy_from_slice(&0x0280u16.to_le_bytes());
        config[8] = 0x10;
        config[0x0b] = 0x02;
        config[0x3d] = 1;
        config[0x3e] = 0x06;
        config[0x3f] = 0xff;
        config
    }

    /// Connect to the switch at `socket` and start passing frames.
    pub fn start(device: &Arc<parking_lot::Mutex<Self>>, socket: std::path::PathBuf) {
        let mut link = std::mem::replace(
            &mut device.lock().link,
            Link::new().expect("socketpair for the PCnet link"),
        );
        link.start(device, socket);
        device.lock().link = link;
    }

    pub fn link(&self) -> &Link {
        &self.link
    }

    /// H_RESET: BCRs to defaults, word I/O mode, then S_RESET.
    fn hardware_reset(&mut self) {
        self.bcr = [0; 32];
        for (index, value) in BCR_DEFAULTS {
            self.bcr[index] = value;
        }
        self.dwio = false;
        self.software_reset();
    }

    /// S_RESET (a read of the RESET register): the CSRs to their reset
    /// values, the chip stopped, software style 0.
    fn software_reset(&mut self) {
        self.csr = [0; 128];
        self.csr[0] = STOP;
        self.csr[4] = 0x0115;
        self.csr[80] = 0x1410;
        self.csr[88] = 0x1003; // part 0x2621, revision
        self.csr[89] = 0x0262;
        self.rap = 0;
        self.bcr[20] = 0x0200;
        self.csr[58] = self.bcr[20];
        self.rx_index = 0;
        self.tx_index = 0;
        self.next_poll = None;
        self.update_irq();
    }

    fn style(&self) -> u16 {
        self.bcr[20] & 0xff
    }

    /// Software styles 2 and 3 (and ILACC 1) use 32-bit structures.
    fn ssize32(&self) -> bool {
        self.bcr[20] & 1 << 8 != 0
    }

    fn set_style(&mut self, value: u16) {
        let style = value & 0xff;
        let ssize32 = u16::from(style != 0) << 8;
        let csrpcnet = u16::from(style != 1) << 9;
        self.bcr[20] = style | ssize32 | csrpcnet;
        self.csr[58] = self.bcr[20];
    }

    fn bus_master(&self) -> bool {
        self.config[4] & 4 != 0
    }

    // ---- interrupts ---------------------------------------------------------

    fn update_irq(&mut self) {
        let csr0 = self.csr[0];
        let pending = csr0 & CSR3_MASKS & !self.csr[3];
        let mut csr0 = csr0 & !(INTR | ERR);
        if pending != 0 {
            csr0 |= INTR;
        }
        if csr0 & (BABL | CERR | MISS | MERR) != 0 {
            csr0 |= ERR;
        }
        self.csr[0] = csr0;
        self.level = csr0 & INTR != 0 && csr0 & IENA != 0;
        self.drive_line();
    }

    fn drive_line(&mut self) {
        let asserted = self.level && self.config[5] & 4 == 0;
        if asserted {
            self.config[6] |= 0x08;
        } else {
            self.config[6] &= !0x08;
        }
        self.irq.set_level(asserted);
    }

    // ---- CSR / BCR ------------------------------------------------------------

    fn read_csr(&self, index: usize) -> u16 {
        match index {
            58 => self.bcr[20],
            _ => self.csr.get(index).copied().unwrap_or(0),
        }
    }

    fn write_csr(&mut self, index: usize, value: u16) {
        match index {
            0 => self.write_csr0(value),
            // Read-only: chip ID, ring counters' status.
            88 | 89 => {}
            58 => self.set_style(value),
            3 => {
                self.csr[3] = value & 0x5f7c;
                self.update_irq();
            }
            4 => {
                // TXSTRT/RCVCCO-style status bits are write-1-to-clear.
                let status = 0x0a2a;
                self.csr[4] = (self.csr[4] & status & !value) | (value & !status);
            }
            // Registers other than CSR0/3/4/58 are writable only while
            // stopped (19436: "can only be written while STOP is set"),
            // except the few that are always writable.
            _ if index < 128
                && (self.csr[0] & STOP != 0 || matches!(index, 1 | 2 | 15 | 112 | 114)) =>
            {
                self.csr[index] = value;
            }
            _ => {}
        }
    }

    fn write_csr0(&mut self, value: u16) {
        let mut csr0 = self.csr[0];
        csr0 &= !(value & CSR0_ACK);
        csr0 = (csr0 & !IENA) | (value & IENA);
        if value & STOP != 0 {
            // STOP wins and clears everything else, IENA included.
            self.csr[0] = STOP;
            self.next_poll = None;
            self.update_irq();
            return;
        }
        self.csr[0] = csr0;
        if value & INIT != 0 {
            self.initialize();
        }
        if value & STRT != 0 {
            self.start_chip();
        }
        if value & TDMD != 0 && self.csr[0] & TXON != 0 {
            self.poll_transmit();
        }
        self.update_irq();
    }

    fn init_address(&self) -> u32 {
        u32::from(self.csr[1]) | u32::from(self.csr[2]) << 16
    }

    /// The upper address byte style 0's 24-bit pointers take from IADR.
    fn high_byte(&self) -> u32 {
        if self.ssize32() {
            0
        } else {
            u32::from(self.csr[2] & 0xff00) << 16
        }
    }

    /// INIT: read the initialization block and load MODE, PADR, LADRF and
    /// the ring bases and lengths.
    fn initialize(&mut self) {
        if !self.bus_master() {
            return;
        }
        let base = self.init_address();
        if self.ssize32() {
            let mut block = [0u8; 28];
            if self
                .memory
                .read_slice(&mut block, GuestAddress(u64::from(base)))
                .is_err()
            {
                self.csr[0] |= MERR;
                return;
            }
            let word0 = u32::from_le_bytes(block[..4].try_into().unwrap());
            self.csr[15] = word0 as u16;
            let rlen = (word0 >> 20) & 0xf;
            let tlen = (word0 >> 28) & 0xf;
            for i in 0..3 {
                self.csr[12 + i] = u16::from_le_bytes([block[4 + 2 * i], block[5 + 2 * i]]);
            }
            for i in 0..4 {
                self.csr[8 + i] = u16::from_le_bytes([block[12 + 2 * i], block[13 + 2 * i]]);
            }
            let rdra = u32::from_le_bytes(block[20..24].try_into().unwrap());
            let tdra = u32::from_le_bytes(block[24..28].try_into().unwrap());
            self.set_rings(rdra, rlen.min(9), tdra, tlen.min(9));
        } else {
            let mut block = [0u8; 24];
            if self
                .memory
                .read_slice(&mut block, GuestAddress(u64::from(base)))
                .is_err()
            {
                self.csr[0] |= MERR;
                return;
            }
            let word = |i: usize| u16::from_le_bytes([block[2 * i], block[2 * i + 1]]);
            self.csr[15] = word(0);
            for i in 0..3 {
                self.csr[12 + i] = word(1 + i);
            }
            for i in 0..4 {
                self.csr[8 + i] = word(4 + i);
            }
            let high = self.high_byte();
            let rdra = u32::from(word(8)) | u32::from(word(9) & 0xff) << 16 | high;
            let tdra = u32::from(word(10)) | u32::from(word(11) & 0xff) << 16 | high;
            let rlen = u32::from(word(9) >> 13);
            let tlen = u32::from(word(11) >> 13);
            self.set_rings(rdra, rlen, tdra, tlen);
        }
        self.rx_index = 0;
        self.tx_index = 0;
        self.csr[0] = (self.csr[0] & !(INIT | STOP)) | IDON;
    }

    fn set_rings(&mut self, rdra: u32, rlen: u32, tdra: u32, tlen: u32) {
        self.csr[24] = rdra as u16;
        self.csr[25] = (rdra >> 16) as u16;
        self.csr[30] = tdra as u16;
        self.csr[31] = (tdra >> 16) as u16;
        self.csr[76] = (1u32 << rlen) as u16;
        self.csr[78] = (1u32 << tlen) as u16;
    }

    fn start_chip(&mut self) {
        let mode = self.csr[15];
        let mut csr0 = (self.csr[0] & !STOP) | STRT;
        if mode & DTX == 0 {
            csr0 |= TXON;
        }
        if mode & DRX == 0 {
            csr0 |= RXON;
        }
        self.csr[0] = csr0;
        self.arm_poll(Instant::now());
        self.link.kick();
    }

    fn arm_poll(&mut self, now: Instant) {
        self.next_poll =
            (self.csr[0] & TXON != 0 && self.csr[4] & TXDPOLL == 0).then(|| now + TX_POLL);
    }

    fn read_bcr(&self, index: usize) -> u16 {
        self.bcr.get(index).copied().unwrap_or(0)
    }

    fn write_bcr(&mut self, index: usize, value: u16) {
        match index {
            20 => self.set_style(value),
            // MAX_LAT/MIN_GNT mirror and the chip's read-only ones.
            22 | 18 => {}
            _ if index < 32 => self.bcr[index] = value,
            _ => {}
        }
    }

    // ---- I/O window -------------------------------------------------------------

    fn read_window(&mut self, offset: u64, data: &mut [u8]) {
        let offset = offset as usize;
        if offset < 16 {
            for (i, b) in data.iter_mut().enumerate() {
                *b = self.prom.get(offset + i).copied().unwrap_or(0);
            }
            return;
        }
        let value: u32 = match (self.dwio, offset, data.len()) {
            (false, 0x10, 2) | (true, 0x10, 4) => u32::from(self.read_csr(self.rap as usize)),
            (false, 0x12, 2) | (true, 0x14, 4) => u32::from(self.rap),
            (false, 0x14, 2) | (true, 0x18, 4) => {
                self.software_reset();
                0
            }
            (false, 0x16, 2) | (true, 0x1c, 4) => u32::from(self.read_bcr(self.rap as usize)),
            _ => 0xffff_ffff,
        };
        for (i, b) in data.iter_mut().enumerate() {
            *b = (value >> (8 * i)) as u8;
        }
    }

    fn write_window(&mut self, offset: u64, data: &[u8]) {
        let offset = offset as usize;
        let mut bytes = [0u8; 4];
        bytes[..data.len().min(4)].copy_from_slice(&data[..data.len().min(4)]);
        let value = u32::from_le_bytes(bytes);
        if !self.dwio && offset == 0x10 && data.len() == 4 {
            // A dword write to RDP switches to dword I/O mode.
            self.dwio = true;
        }
        match (self.dwio, offset, data.len()) {
            (false, 0x10, 2) | (true, 0x10, 4) => self.write_csr(self.rap as usize, value as u16),
            (false, 0x12, 2) | (true, 0x14, 4) => self.rap = (value & 0x7f) as u16,
            (false, 0x14, 2) | (true, 0x18, 4) => {}
            (false, 0x16, 2) | (true, 0x1c, 4) => self.write_bcr(self.rap as usize, value as u16),
            _ => {}
        }
    }

    // ---- descriptors ------------------------------------------------------------

    fn ring(&self, rx: bool) -> (u32, u32) {
        let (low, length) = if rx { (24, 76) } else { (30, 78) };
        let base = u32::from(self.csr[low]) | u32::from(self.csr[low + 1]) << 16;
        (base, u32::from(self.csr[length]).clamp(1, 512))
    }

    fn descriptor_address(&self, rx: bool, index: u32) -> u64 {
        let (base, _) = self.ring(rx);
        let size = if self.ssize32() { 16 } else { 8 };
        u64::from(base.wrapping_add(index * size))
    }

    fn read_descriptor(&self, rx: bool, index: u32) -> Option<Descriptor> {
        let address = GuestAddress(self.descriptor_address(rx, index));
        if self.ssize32() {
            let mut raw = [0u8; 16];
            self.memory.read_slice(&mut raw, address).ok()?;
            let dword = |i: usize| u32::from_le_bytes(raw[4 * i..4 * i + 4].try_into().unwrap());
            // Style 3 swaps the address and the misc words.
            let (addr, misc) = if self.style() == 3 {
                (dword(2), dword(0))
            } else {
                (dword(0), dword(2))
            };
            let flags_word = dword(1);
            Some(Descriptor {
                address: addr,
                flags: (flags_word >> 16) as u16,
                bcnt: (flags_word & 0xfff) as u16,
                misc,
            })
        } else {
            let mut raw = [0u8; 8];
            self.memory.read_slice(&mut raw, address).ok()?;
            let word = |i: usize| u16::from_le_bytes([raw[2 * i], raw[2 * i + 1]]);
            Some(Descriptor {
                address: u32::from(word(0)) | u32::from(word(1) & 0xff) << 16 | self.high_byte(),
                flags: word(1) & 0xff00,
                bcnt: word(2) & 0xfff,
                misc: u32::from(word(3)),
            })
        }
    }

    /// Write back flags (OWN last) and the misc word.
    fn write_descriptor(&self, rx: bool, index: u32, desc: &Descriptor) {
        let address = GuestAddress(self.descriptor_address(rx, index));
        if self.ssize32() {
            let misc_offset = if self.style() == 3 { 0 } else { 8 };
            let _ = self
                .memory
                .write_slice(&desc.misc.to_le_bytes(), address.unchecked_add(misc_offset));
            let flags = u32::from(desc.flags) << 16 | 0xf000 | u32::from(desc.bcnt);
            let _ = self
                .memory
                .write_slice(&flags.to_le_bytes(), address.unchecked_add(4));
        } else {
            let _ = self
                .memory
                .write_slice(&(desc.misc as u16).to_le_bytes(), address.unchecked_add(6));
            let mut flags = [0u8; 2];
            let _ = self.memory.read_slice(&mut flags, address.unchecked_add(2));
            flags[1] = (desc.flags >> 8) as u8;
            let _ = self.memory.write_slice(&flags, address.unchecked_add(2));
        }
    }

    fn buffer_length(bcnt: u16) -> usize {
        match (0u16.wrapping_sub(bcnt)) & 0xfff {
            0 => 4096,
            n => usize::from(n),
        }
    }

    // ---- transmit ---------------------------------------------------------------

    fn poll_transmit(&mut self) {
        if self.csr[0] & TXON == 0 || !self.bus_master() || self.quiesced {
            return;
        }
        let (_, count) = self.ring(false);
        let mut interrupt = false;
        let mut budget = count;
        for _ in 0..count {
            if budget == 0 {
                break;
            }
            let first = self.tx_index;
            let Some(desc) = self.read_descriptor(false, first) else {
                self.csr[0] |= MERR;
                break;
            };
            if desc.flags & OWN == 0 {
                break;
            }
            // Gather STP..ENP; a chain that is not complete waits.
            let mut frame = Vec::new();
            let mut index = first;
            let mut chain = Vec::new();
            let mut complete = false;
            for _ in 0..count {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                let Some(d) = self.read_descriptor(false, index) else {
                    break;
                };
                if d.flags & OWN == 0 {
                    break;
                }
                let length = Self::buffer_length(d.bcnt).min(MAX_FRAME.saturating_sub(frame.len()));
                let start = frame.len();
                frame.resize(start + length, 0);
                if self
                    .memory
                    .read_slice(&mut frame[start..], GuestAddress(u64::from(d.address)))
                    .is_err()
                {
                    frame.truncate(start);
                }
                chain.push((index, d));
                index = (index + 1) % count;
                if d.flags & ENP != 0 {
                    complete = true;
                    break;
                }
            }
            if !complete {
                if chain.len() as u32 >= count {
                    // A ring of OWNed buffers with no ENP: buffer error.
                    for (i, mut d) in chain {
                        d.flags = (d.flags & !OWN) | DERR;
                        d.misc |= TX_BUFF;
                        self.write_descriptor(false, i, &d);
                    }
                    self.tx_index = index;
                    interrupt = true;
                    continue;
                }
                break;
            }
            let mode = self.csr[15];
            let add_fcs = desc.flags & ADD_FCS != 0;
            if self.csr[4] & APAD_XMT != 0 && frame.len() < 60 {
                frame.resize(60, 0);
            }
            if frame.len() >= 14 && self.carrier {
                if mode & 0x4 != 0 {
                    // Internal loopback: the frame comes back to receive.
                    let mut looped = frame.clone();
                    if mode & DXMTFCS == 0 || add_fcs {
                        looped.extend_from_slice(&fcs(&frame));
                    }
                    self.deliver(&looped, true);
                } else {
                    self.link.send(frame);
                }
            }
            for (i, mut d) in chain {
                d.flags &= !OWN;
                d.misc = 0;
                self.write_descriptor(false, i, &d);
            }
            self.tx_index = index;
            interrupt = true;
        }
        if interrupt {
            self.csr[0] |= TINT;
        }
        self.csr[0] &= !TDMD;
        self.update_irq();
    }

    // ---- receive ----------------------------------------------------------------

    fn accepts(&self, frame: &[u8]) -> Option<u16> {
        let mode = self.csr[15];
        if mode & PROM != 0 {
            return Some(0);
        }
        let destination = &frame[..6];
        if destination.iter().all(|b| *b == 0xff) {
            return (mode & DRCVBC == 0).then_some(RX_BAM);
        }
        if destination[0] & 1 != 0 {
            // Logical address filter: the top six bits of the CRC.
            let crc = !u32::from_le_bytes(fcs(destination));
            let hash = crc >> 26;
            let ladrf = self.csr[8 + (hash as usize >> 4)];
            return (ladrf & 1 << (hash & 15) != 0).then_some(RX_LAFM);
        }
        let padr: Vec<u8> = (0..3)
            .flat_map(|i| self.csr[12 + i].to_le_bytes())
            .collect();
        (mode & DRCVPA == 0 && destination == &padr[..]).then_some(RX_PAM)
    }

    fn deliver(&mut self, input: &[u8], looped: bool) -> bool {
        if self.quiesced || !self.bus_master() {
            return false;
        }
        if self.csr[0] & RXON == 0 || input.len() < 14 {
            return true;
        }
        let Some(match_bits) = self.accepts(input) else {
            return true;
        };
        let mut frame = input.to_vec();
        if !looped {
            if frame.len() < 60 {
                frame.resize(60, 0);
            }
            let fcs = fcs(&frame);
            frame.extend_from_slice(&fcs);
        }
        let (_, count) = self.ring(true);
        // Count the owned descriptors this frame needs before writing any.
        let mut needed = Vec::new();
        let mut space = 0usize;
        let mut index = self.rx_index;
        for _ in 0..count {
            let Some(d) = self.read_descriptor(true, index) else {
                self.csr[0] |= MERR;
                self.update_irq();
                return true;
            };
            if d.flags & OWN == 0 {
                break;
            }
            space += Self::buffer_length(d.bcnt);
            needed.push((index, d));
            index = (index + 1) % count;
            if space >= frame.len() {
                break;
            }
        }
        if space < frame.len() {
            if needed.len() as u32 == count {
                // The whole ring cannot hold it: buffer error, frame lost.
                if let Some((i, mut d)) = needed.first().copied() {
                    d.flags = (d.flags & !OWN) | DERR | RX_BUFF | STP;
                    self.write_descriptor(true, i, &d);
                    self.rx_index = (i + 1) % count;
                }
                self.csr[0] |= RINT;
                self.update_irq();
                return true;
            }
            return false;
        }
        let mut written = 0usize;
        let last = needed.len() - 1;
        for (n, (i, mut d)) in needed.into_iter().enumerate() {
            let length = Self::buffer_length(d.bcnt).min(frame.len() - written);
            let _ = self.memory.write_slice(
                &frame[written..written + length],
                GuestAddress(u64::from(d.address)),
            );
            written += length;
            d.flags &= !(OWN | DERR | STP | ENP | 0x7c00 | RX_BAM | RX_LAFM | RX_PAM);
            if n == 0 {
                d.flags |= STP;
            }
            if n == last {
                d.flags |= ENP | match_bits;
                d.misc = frame.len() as u32 & 0xfff;
            } else {
                d.misc = 0;
            }
            self.write_descriptor(true, i, &d);
            self.rx_index = (i + 1) % count;
        }
        self.csr[0] |= RINT;
        self.update_irq();
        true
    }
}

impl LinkClient for Pcnet {
    fn link_changed(&mut self, up: bool) {
        self.carrier = up;
    }

    fn receive(&mut self, frame: &[u8]) -> bool {
        self.deliver(frame, false)
    }

    fn poll_timers(&mut self, now: Instant) -> Option<Instant> {
        if self.quiesced {
            return None;
        }
        if self.next_poll.is_some_and(|at| now >= at) {
            self.poll_transmit();
            self.arm_poll(now);
        }
        self.next_poll
    }
}

impl PciDevice for Pcnet {
    fn config_read(&mut self, reg: usize) -> u32 {
        reg.checked_mul(4)
            .and_then(|o| self.config.get(o..o + 4))
            .map_or(u32::MAX, |b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let Some(start) = reg
            .checked_mul(4)
            .and_then(|r| r.checked_add(offset as usize))
        else {
            return;
        };
        for (i, &byte) in data.iter().enumerate() {
            let pos = start + i;
            let mask = match pos {
                4 => 0x47,
                5 => 0x05,
                0x0c | 0x0d | 0x3c => 0xff,
                _ => 0,
            };
            if pos < 256 {
                self.config[pos] = (self.config[pos] & !mask) | byte & mask;
            }
        }
        self.drive_line();
        self.link.kick();
    }

    fn bars(&self) -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 0,
                size: WINDOW,
                kind: BarKind::Io,
            },
            BarInfo {
                index: 1,
                size: WINDOW,
                kind: BarKind::Mmio32,
            },
        ]
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        match bar {
            0 | 1 => self.read_window(offset, data),
            _ => data.fill(0xff),
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        // The I/O BAR and the MMIO BAR are the same 32-byte window.
        if matches!(bar, 0 | 1) {
            self.write_window(offset, data);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    config: Vec<u8>,
    csr: Vec<u16>,
    bcr: Vec<u16>,
    rap: u16,
    dwio: bool,
    rx_index: u32,
    tx_index: u32,
}

impl Snapshot for Pcnet {
    fn quiesce(&mut self) {
        self.quiesced = true;
    }

    fn resume_snapshot(&mut self) {
        self.quiesced = false;
        self.link.kick();
    }

    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        serde_json::to_vec(&Saved {
            version: 1,
            config: self.config.to_vec(),
            csr: self.csr.to_vec(),
            bcr: self.bcr.to_vec(),
            rap: self.rap,
            dwio: self.dwio,
            rx_index: self.rx_index,
            tx_index: self.tx_index,
        })
        .map_err(|e| DeviceError::State(e.to_string()))
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let saved: Saved =
            serde_json::from_slice(state).map_err(|e| DeviceError::State(e.to_string()))?;
        if saved.version != 1
            || saved.config.len() != 256
            || saved.csr.len() != 128
            || saved.bcr.len() != 32
        {
            return Err(DeviceError::State("invalid PCnet state".into()));
        }
        self.config.copy_from_slice(&saved.config);
        self.csr.copy_from_slice(&saved.csr);
        self.bcr.copy_from_slice(&saved.bcr);
        self.rap = saved.rap & 0x7f;
        self.dwio = saved.dwio;
        let (_, rx_count) = self.ring(true);
        let (_, tx_count) = self.ring(false);
        self.rx_index = saved.rx_index % rx_count;
        self.tx_index = saved.tx_index % tx_count;
        self.arm_poll(Instant::now());
        self.update_irq();
        self.link.kick();
        Ok(())
    }
}

impl Resettable for Pcnet {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.config = Self::power_on_config();
        self.hardware_reset();
        Ok(())
    }
}

impl PciSnapshot for Pcnet {
    fn quiesce(&mut self) {
        Snapshot::quiesce(self);
    }
    fn resume_snapshot(&mut self) {
        Snapshot::resume_snapshot(self);
    }
    fn snapshot_id(&self) -> String {
        self.state_id()
    }
    fn save_function_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.save_state()
    }
    fn save_wrapped_state(&self) -> Result<Vec<u8>, DeviceError> {
        Ok(Vec::new())
    }
    fn restore_function_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.restore_state(state)
    }
    fn restore_wrapped_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.is_empty() {
            Ok(())
        } else {
            Err(DeviceError::State("unexpected PCnet wrapped state".into()))
        }
    }
}
