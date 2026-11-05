//! Intel 82540EM gigabit Ethernet controller (e1000), as QEMU's pc machine
//! presents it (SPEC phase 4b).
//!
//! The register model follows the Intel PCI/PCI-X Family of Gigabit Ethernet
//! Controllers Software Developer's Manual (8254x, rev 4.0): the MMIO and I/O
//! windows, the Microwire EEPROM behind EECD and EERD, the PHY behind MDIC,
//! legacy receive descriptors, legacy and extended (context and data)
//! transmit descriptors with checksum insertion and TCP segmentation, the
//! receive filters, ICR/IMS/IMC with the interrupt delay and throttling
//! timers, and the statistics block. Reset values and the PHY's answers are
//! the ones the `docs/hwref/pc-windows-7` capture recorded.
//!
//! Every descriptor and buffer access goes through [`GuestMemory`]: an
//! address outside guest RAM fails that access and is skipped, never a host
//! access.

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vm_memory::{Address, Bytes, GuestAddress};

use super::{Link, LinkClient, fcs, fold, ones_sum};
use crate::{
    BarInfo, BarKind, DeviceError, GuestMemory, IrqLine, PciDevice, PciSnapshot, Resettable,
    Snapshot,
};

/// PCI identity: 8086:100e rev 3, subsystem 1af4:1100 (the capture).
const VENDOR_DEVICE: u32 = 0x100e_8086;
const SUBSYSTEM: u32 = 0x1100_1af4;
const MMIO_SIZE: u64 = 128 << 10;
const IO_SIZE: u64 = 64;

// Register offsets (SDM section 13).
const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EECD: usize = 0x0010;
const EERD: usize = 0x0014;
const MDIC: usize = 0x0020;
const VET: usize = 0x0038;
const ICR: usize = 0x00c0;
const ITR: usize = 0x00c4;
const ICS: usize = 0x00c8;
const IMS: usize = 0x00d0;
const IMC: usize = 0x00d8;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const LEDCTL: usize = 0x0e00;
const PBA: usize = 0x1000;
const RDBAL: usize = 0x2800;
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const RDTR: usize = 0x2820;
const RADV: usize = 0x282c;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const TIDV: usize = 0x3820;
const TADV: usize = 0x382c;
const STATS: std::ops::Range<usize> = 0x4000..0x4100;
const RXCSUM: usize = 0x5000;
const MTA: usize = 0x5200;
const RAL0: usize = 0x5400;
const VFTA: usize = 0x5600;
const MANC: usize = 0x5820;

// Statistics (all clear on read).
const GPRC: usize = 0x4074;
const BPRC: usize = 0x4078;
const MPRC: usize = 0x407c;
const GPTC: usize = 0x4080;
const GORCL: usize = 0x4088;
const GOTCL: usize = 0x4090;
const ROC: usize = 0x40ac;
const TORL: usize = 0x40c0;
const TOTL: usize = 0x40c8;
const TPR: usize = 0x40d0;
const TPT: usize = 0x40d4;
const MPTC: usize = 0x40f0;
const BPTC: usize = 0x40f4;
const TSCTC: usize = 0x40f8;

// Bits.
const CTRL_RST: u32 = 1 << 26;
const CTRL_VME: u32 = 1 << 30;
const CTRL_PHY_RST: u32 = 1 << 31;
const STATUS_LU: u32 = 1 << 1;
const EECD_SK: u32 = 1 << 0;
const EECD_CS: u32 = 1 << 1;
const EECD_DI: u32 = 1 << 2;
const EECD_WRITABLE: u32 = 0x77;
const EECD_FIXED: u32 = 0x180; // EE_PRES | EE_GNT
const ICR_TXDW: u32 = 1 << 0;
const ICR_TXQE: u32 = 1 << 1;
const ICR_LSC: u32 = 1 << 2;
const ICR_RXDMT0: u32 = 1 << 4;
const ICR_RXT0: u32 = 1 << 7;
const ICR_MDAC: u32 = 1 << 9;
const RCTL_EN: u32 = 1 << 1;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_LPE: u32 = 1 << 5;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_VFE: u32 = 1 << 18;
const RCTL_BSEX: u32 = 1 << 25;
const RCTL_SECRC: u32 = 1 << 26;
const TCTL_EN: u32 = 1 << 1;
const RXCSUM_IPOFL: u32 = 1 << 8;
const RXCSUM_TUOFL: u32 = 1 << 9;

// Receive descriptor status and errors.
const RXD_DD: u8 = 1 << 0;
const RXD_EOP: u8 = 1 << 1;
const RXD_IXSM: u8 = 1 << 2;
const RXD_VP: u8 = 1 << 3;
const RXD_TCPCS: u8 = 1 << 5;
const RXD_IPCS: u8 = 1 << 6;
const RXE_TCPE: u8 = 1 << 5;
const RXE_IPE: u8 = 1 << 6;

// Transmit descriptor command bits (legacy CMD and extended DCMD/TUCMD).
const TXD_EOP: u8 = 1 << 0;
const TXD_IC: u8 = 1 << 2;
const TXD_RS: u8 = 1 << 3;
const TXD_DEXT: u8 = 1 << 5;
const TXD_VLE: u8 = 1 << 6;
const TXD_IDE: u8 = 1 << 7;
const TXD_TSE: u8 = 1 << 2;
const TUCMD_TCP: u8 = 1 << 0;
const TUCMD_IP: u8 = 1 << 1;
const POPTS_IXSM: u8 = 1 << 0;
const POPTS_TXSM: u8 = 1 << 1;

/// The PHY's auto-negotiation after a restart or reset, until link is
/// reported again.
const NEGOTIATION: Duration = Duration::from_millis(500);
/// A frame being gathered from transmit descriptors cannot grow past this.
const MAX_TX_FRAME: usize = 64 << 10;

/// Power-on values that differ from zero (the capture's first reads).
const RESET_VALUES: [(usize, u32); 8] = [
    (CTRL, 0x0014_0240),
    (STATUS, 0x8008_0781),
    (TCTL, 0x0000_0008),
    (LEDCTL, 0x0000_0602),
    (PBA, 0x0000_0030),
    (VET, 0x0000_8100),
    (MANC, 0x0022_2300),
    (RXCSUM, 0x0000_0300),
];

/// The M88E1011-style PHY's registers at power-on, as the capture's MDIC
/// reads returned them (link up, negotiation complete).
fn phy_reset() -> [u16; 32] {
    let mut phy = [0u16; 32];
    for (reg, value) in [
        (0, 0x1140),
        (1, 0x796d),
        (2, 0x0141),
        (3, 0x0c20),
        (4, 0x0de1),
        (5, 0x41e0),
        (9, 0x0e00),
        (10, 0x3c00),
        (0x10, 0x0360),
        (0x11, 0xac00),
        (0x14, 0x0d60),
    ] {
        phy[reg] = value;
    }
    phy
}

/// PHY status bits that drop while negotiating: link status, AN complete.
const PHY_LINK_BITS: u16 = 0x0024;
const PHY_WRITABLE: [usize; 5] = [0, 4, 9, 0x10, 0x14];

/// The 64-word EEPROM image, laid out as SDM table 5-2 describes, with the
/// MAC, the IDs the capture shows and a checksum word making the sum 0xbaba.
fn eeprom_image(mac: [u8; 6]) -> [u16; 64] {
    let mut words = [0u16; 64];
    for i in 0..3 {
        words[i] = u16::from_le_bytes([mac[2 * i], mac[2 * i + 1]]);
    }
    words[0x05] = 0x0000;
    words[0x0a] = 0x0003; // load subsystem and vendor IDs
    words[0x0b] = (SUBSYSTEM >> 16) as u16;
    words[0x0c] = SUBSYSTEM as u16;
    words[0x0d] = (VENDOR_DEVICE >> 16) as u16;
    words[0x0e] = VENDOR_DEVICE as u16;
    words[0x0f] = 0x3000;
    let sum = words[..63].iter().fold(0u16, |a, w| a.wrapping_add(*w));
    words[63] = 0xbabau16.wrapping_sub(sum);
    words
}

/// The Microwire wire protocol a driver bit-bangs through EECD.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct EepromWire {
    /// Bits clocked in since chip select: start, opcode, address.
    command: u32,
    count: u32,
    /// Word being shifted out and its remaining bits.
    out: u16,
    out_bits: u32,
    address: u8,
    reading: bool,
    data_out: bool,
    /// The clock line as last written, for edge detection.
    sk: bool,
}

/// Offload state carried from a context descriptor to its data descriptors.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct TxContext {
    ipcss: u8,
    ipcso: u8,
    ipcse: u16,
    tucss: u8,
    tucso: u8,
    tucse: u16,
    tucmd: u8,
    hdrlen: u8,
    mss: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TxFrame {
    data: Vec<u8>,
    /// Legacy checksum insertion: start and field offset.
    legacy_csum: Option<(u8, u8)>,
    popts: u8,
    tse: bool,
    vlan: Option<u16>,
    extended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    Mmio,
    Io,
}

pub struct E1000 {
    id: String,
    memory: GuestMemory,
    irq: Arc<dyn IrqLine>,
    link: Link,
    mac: [u8; 6],
    config: [u8; 256],
    regs: Vec<u32>,
    phy: [u16; 32],
    eeprom: [u16; 64],
    wire: EepromWire,
    io_address: u32,
    context: TxContext,
    frame: TxFrame,
    carrier: bool,
    negotiation: Option<Instant>,
    rx_timer: Option<Instant>,
    rx_absolute: Option<Instant>,
    tx_timer: Option<Instant>,
    tx_absolute: Option<Instant>,
    throttle_until: Option<Instant>,
    level: bool,
    quiesced: bool,
}

impl E1000 {
    pub fn new(
        id: impl Into<String>,
        mac: [u8; 6],
        memory: GuestMemory,
        irq: Arc<dyn IrqLine>,
    ) -> std::io::Result<Self> {
        let mut config = [0u8; 256];
        config[..4].copy_from_slice(&VENDOR_DEVICE.to_le_bytes());
        config[8] = 0x03;
        config[0x0b] = 0x02;
        config[0x2c..0x30].copy_from_slice(&SUBSYSTEM.to_le_bytes());
        config[0x3d] = 1;
        let mut device = Self {
            id: id.into(),
            memory,
            irq,
            link: Link::new()?,
            mac,
            config,
            regs: vec![0; (MMIO_SIZE / 4) as usize],
            phy: phy_reset(),
            eeprom: eeprom_image(mac),
            wire: EepromWire::default(),
            io_address: 0,
            context: TxContext::default(),
            frame: TxFrame::default(),
            carrier: false,
            negotiation: None,
            rx_timer: None,
            rx_absolute: None,
            tx_timer: None,
            tx_absolute: None,
            throttle_until: None,
            level: false,
            quiesced: false,
        };
        device.reset_registers();
        Ok(device)
    }

    /// Connect to the switch at `socket` and start passing frames.
    pub fn start(device: &Arc<parking_lot::Mutex<Self>>, socket: std::path::PathBuf) {
        let mut link = std::mem::replace(
            &mut device.lock().link,
            Link::new().expect("socketpair for the e1000 link"),
        );
        link.start(device, socket);
        device.lock().link = link;
    }

    /// The link this model sends through (tests read frames off it).
    pub fn link(&self) -> &Link {
        &self.link
    }

    fn reg(&self, offset: usize) -> u32 {
        self.regs[offset / 4]
    }

    fn set(&mut self, offset: usize, value: u32) {
        self.regs[offset / 4] = value;
    }

    /// Software reset (CTRL.RST) and power-on: every MAC register to its
    /// reset value, the receive address from the EEPROM, the PHY reset.
    fn reset_registers(&mut self) {
        self.regs.fill(0);
        for (offset, value) in RESET_VALUES {
            self.set(offset, value);
        }
        let mac = self.mac;
        self.set(RAL0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        self.set(
            RAL0 + 4,
            u32::from(u16::from_le_bytes([mac[4], mac[5]])) | 1 << 31,
        );
        self.phy = phy_reset();
        self.context = TxContext::default();
        self.frame = TxFrame::default();
        self.rx_timer = None;
        self.rx_absolute = None;
        self.tx_timer = None;
        self.tx_absolute = None;
        self.throttle_until = None;
        self.negotiation = None;
        self.update_link_status();
        self.update_irq(Instant::now());
    }

    fn link_up(&self) -> bool {
        self.carrier && self.negotiation.is_none()
    }

    fn update_link_status(&mut self) {
        let up = self.link_up();
        let status = self.reg(STATUS);
        self.set(
            STATUS,
            if up {
                status | STATUS_LU
            } else {
                status & !STATUS_LU
            },
        );
        if up {
            self.phy[1] |= PHY_LINK_BITS;
        } else {
            self.phy[1] &= !PHY_LINK_BITS;
        }
    }

    fn bus_master(&self) -> bool {
        self.config[4] & 4 != 0
    }

    // ---- interrupts ---------------------------------------------------------

    fn raise(&mut self, cause: u32, now: Instant) {
        let icr = self.reg(ICR) | cause;
        self.set(ICR, icr);
        self.update_irq(now);
    }

    /// Drive INTx from ICR & IMS, holding a new assertion back until the
    /// throttling interval (ITR, 256 ns units) since the last one passes.
    fn update_irq(&mut self, now: Instant) {
        let want = self.reg(ICR) & self.reg(IMS) != 0;
        if want && !self.level {
            if self.throttle_until.is_some_and(|until| now < until) {
                return;
            }
            self.level = true;
            let itr = u64::from(self.reg(ITR) & 0xffff);
            self.throttle_until = (itr != 0).then(|| now + Duration::from_nanos(itr * 256));
        } else if !want && self.level {
            self.level = false;
        } else {
            return;
        }
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

    // ---- register access ------------------------------------------------------

    fn read_register(&mut self, offset: usize) -> u32 {
        match offset {
            EECD => {
                // DO idles high while the EEPROM is not selected.
                let data_out = self.reg(EECD) & EECD_CS == 0 || self.wire.data_out;
                (self.reg(EECD) & EECD_WRITABLE) | EECD_FIXED | u32::from(data_out) << 3
            }
            ICR => {
                let value = self.reg(ICR);
                self.set(ICR, 0);
                self.update_irq(Instant::now());
                value
            }
            ICS | IMC => 0,
            o if STATS.contains(&o) => {
                let value = self.reg(o);
                self.set(o, 0);
                value
            }
            o if o < MMIO_SIZE as usize => self.reg(o),
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: usize, value: u32) {
        let now = Instant::now();
        match offset {
            CTRL => {
                if value & CTRL_RST != 0 {
                    self.reset_registers();
                    return;
                }
                if value & CTRL_PHY_RST != 0 {
                    self.phy = phy_reset();
                    self.start_negotiation(now);
                }
                self.set(CTRL, value & !(CTRL_RST | CTRL_PHY_RST));
            }
            STATUS | ICS => {
                if offset == ICS {
                    self.raise(value, now);
                }
            }
            EECD => {
                self.set(EECD, value & EECD_WRITABLE);
                self.clock_eeprom(value);
            }
            EERD => {
                if value & 1 != 0 {
                    let address = (value >> 8) & 0xff;
                    let data = self.eeprom.get(address as usize).copied().unwrap_or(0);
                    self.set(EERD, (address << 8) | 1 << 4 | u32::from(data) << 16);
                }
            }
            MDIC => self.mdic(value, now),
            ICR => {
                let icr = self.reg(ICR) & !value;
                self.set(ICR, icr);
                self.update_irq(now);
            }
            IMS => {
                let ims = self.reg(IMS) | value;
                self.set(IMS, ims);
                self.update_irq(now);
            }
            IMC => {
                let ims = self.reg(IMS) & !value;
                self.set(IMS, ims);
                self.update_irq(now);
            }
            RCTL => {
                self.set(RCTL, value);
                self.link.kick();
            }
            RDT => {
                self.set(RDT, value & 0xffff);
                self.link.kick();
            }
            RDH | TDH => self.set(offset, value & 0xffff),
            RDLEN | TDLEN => self.set(offset, value & 0x000f_ff80),
            RDBAL | TDBAL => self.set(offset, value & !0xf),
            RDTR => {
                self.set(RDTR, value & 0xffff);
                if value & 1 << 31 != 0 && (self.rx_timer.is_some() || self.rx_absolute.is_some()) {
                    // Flush partial descriptor block: fire the timer now.
                    self.rx_timer = None;
                    self.rx_absolute = None;
                    self.raise(ICR_RXT0, now);
                }
            }
            TCTL => {
                self.set(TCTL, value);
                self.transmit(now);
            }
            TDT => {
                self.set(TDT, value & 0xffff);
                self.transmit(now);
            }
            o if STATS.contains(&o) => {}
            o if o < MMIO_SIZE as usize => self.set(o, value),
            _ => {}
        }
    }

    fn mdic(&mut self, value: u32, now: Instant) {
        let data = value & 0xffff;
        let reg = ((value >> 16) & 0x1f) as usize;
        let phy = (value >> 21) & 0x1f;
        let op = (value >> 26) & 3;
        let mut result = value & !(0xffff | 1 << 28 | 1 << 30);
        if phy != 1 || !matches!(op, 1 | 2) {
            result |= 1 << 30;
        } else if op == 2 {
            result |= u32::from(self.phy[reg]);
        } else {
            result |= data;
            if PHY_WRITABLE.contains(&reg) {
                if reg == 0 {
                    // Reset and restart-negotiation self-clear.
                    self.phy[0] = (data as u16) & !0x8200;
                    if data & 0x8200 != 0 {
                        self.start_negotiation(now);
                    }
                } else {
                    self.phy[reg] = data as u16;
                }
            }
        }
        self.set(MDIC, result | 1 << 28);
        if value & 1 << 29 != 0 {
            self.raise(ICR_MDAC, now);
        }
    }

    fn start_negotiation(&mut self, now: Instant) {
        self.negotiation = Some(now + NEGOTIATION);
        self.update_link_status();
        self.link.kick();
    }

    /// One edge of the bit-banged EECD lines (93C46-style Microwire: start
    /// bit, opcode 10 = READ, 6-bit address, then 16 data bits MSB first,
    /// continuing to the next word while chip select stays high).
    fn clock_eeprom(&mut self, value: u32) {
        if value & EECD_CS == 0 {
            self.wire = EepromWire {
                data_out: true,
                ..Default::default()
            };
            return;
        }
        let clock = value & EECD_SK != 0;
        let rising = clock && !self.wire.sk;
        self.wire.sk = clock;
        if !rising {
            return;
        }
        if self.wire.reading {
            if self.wire.out_bits == 0 {
                self.wire.address = (self.wire.address + 1) & 0x3f;
                self.wire.out = self.eeprom[self.wire.address as usize];
                self.wire.out_bits = 16;
            }
            self.wire.out_bits -= 1;
            self.wire.data_out = self.wire.out >> self.wire.out_bits & 1 != 0;
            return;
        }
        let bit = u32::from(value & EECD_DI != 0);
        if self.wire.count == 0 && bit == 0 {
            // Waiting for the start bit.
            return;
        }
        self.wire.command = self.wire.command << 1 | bit;
        self.wire.count += 1;
        if self.wire.count == 9 {
            let opcode = (self.wire.command >> 6) & 3;
            self.wire.address = (self.wire.command & 0x3f) as u8;
            if opcode == 2 {
                self.wire.reading = true;
                self.wire.out = self.eeprom[self.wire.address as usize];
                self.wire.out_bits = 16;
                // The dummy zero that precedes the data.
                self.wire.data_out = false;
            }
        }
    }

    fn access(&mut self, window: Window, offset: u64, data: &mut [u8], write: Option<&[u8]>) {
        let offset = offset as usize;
        let (register, shift) = match window {
            Window::Mmio => (offset & !3, (offset & 3) * 8),
            Window::Io => {
                // IOADDR at 0, IODATA at 4; the rest of the window is reserved.
                if offset < 4 {
                    let mut bytes = self.io_address.to_le_bytes();
                    if let Some(input) = write {
                        for (i, b) in input.iter().enumerate() {
                            if let Some(slot) = bytes.get_mut(offset + i) {
                                *slot = *b;
                            }
                        }
                        self.io_address = u32::from_le_bytes(bytes) & 0x1_fffc;
                    } else {
                        for (i, b) in data.iter_mut().enumerate() {
                            *b = bytes.get(offset + i).copied().unwrap_or(0);
                        }
                    }
                    return;
                }
                if !(4..8).contains(&offset) {
                    data.fill(0);
                    return;
                }
                (self.io_address as usize, (offset - 4) * 8)
            }
        };
        match write {
            Some(input) => {
                let mut value = if input.len() == 4 && shift == 0 {
                    0
                } else {
                    self.reg(register.min(MMIO_SIZE as usize - 4))
                };
                for (i, b) in input.iter().enumerate() {
                    let bit = shift + 8 * i;
                    if bit < 32 {
                        value = (value & !(0xff << bit)) | u32::from(*b) << bit;
                    }
                }
                self.write_register(register, value);
            }
            None => {
                let value = self.read_register(register);
                for (i, b) in data.iter_mut().enumerate() {
                    let bit = shift + 8 * i;
                    *b = if bit < 32 { (value >> bit) as u8 } else { 0 };
                }
            }
        }
    }

    // ---- transmit ---------------------------------------------------------------

    fn ring(&self, base_low: usize, base_high: usize, length: usize) -> (u64, u32) {
        let base = u64::from(self.reg(base_low)) | u64::from(self.reg(base_high)) << 32;
        (base, self.reg(length) / 16)
    }

    fn transmit(&mut self, now: Instant) {
        if self.reg(TCTL) & TCTL_EN == 0 || !self.bus_master() || self.quiesced {
            return;
        }
        let (base, count) = self.ring(TDBAL, TDBAH, TDLEN);
        if count == 0 {
            return;
        }
        let mut written_back = false;
        let mut delayed = false;
        let mut budget = count;
        while self.reg(TDH) != self.reg(TDT) && budget > 0 {
            budget -= 1;
            let head = self.reg(TDH) % count;
            let address = GuestAddress(base.wrapping_add(u64::from(head) * 16));
            let mut desc = [0u8; 16];
            if self.memory.read_slice(&mut desc, address).is_err() {
                break;
            }
            let (report, ide) = self.transmit_descriptor(&desc);
            if report {
                desc[12] |= 1; // DD
                let _ = self
                    .memory
                    .write_slice(&desc[12..13], address.unchecked_add(12));
                written_back = true;
                delayed |= ide;
            }
            self.set(TDH, (head + 1) % count);
        }
        let mut causes = 0;
        if written_back {
            if delayed && self.reg(TIDV) & 0xffff != 0 {
                self.tx_timer = Some(now + units(self.reg(TIDV)));
                if self.tx_absolute.is_none() && self.reg(TADV) & 0xffff != 0 {
                    self.tx_absolute = Some(now + units(self.reg(TADV)));
                }
            } else {
                causes |= ICR_TXDW;
            }
        }
        if self.reg(TDH) == self.reg(TDT) {
            causes |= ICR_TXQE;
        }
        if causes != 0 {
            self.raise(causes, now);
        }
    }

    /// Consume one descriptor. Returns (report status, interrupt delay).
    fn transmit_descriptor(&mut self, desc: &[u8; 16]) -> (bool, bool) {
        let buffer = u64::from_le_bytes(desc[..8].try_into().unwrap());
        let command = desc[11];
        if command & TXD_DEXT == 0 {
            let length = u16::from_le_bytes([desc[8], desc[9]]) as usize;
            self.gather(buffer, length);
            self.frame.extended = false;
            if command & TXD_IC != 0 {
                self.frame.legacy_csum = Some((desc[13], desc[10]));
            }
            if command & TXD_VLE != 0 {
                self.frame.vlan = Some(u16::from_le_bytes([desc[14], desc[15]]));
            }
            if command & TXD_EOP != 0 {
                self.finish_frame();
            }
            return (command & TXD_RS != 0, command & TXD_IDE != 0);
        }
        let kind = (desc[10] >> 4) & 0xf;
        let dword = u32::from_le_bytes(desc[8..12].try_into().unwrap());
        match kind {
            0 => {
                self.context = TxContext {
                    ipcss: desc[0],
                    ipcso: desc[1],
                    ipcse: u16::from_le_bytes([desc[2], desc[3]]),
                    tucss: desc[4],
                    tucso: desc[5],
                    tucse: u16::from_le_bytes([desc[6], desc[7]]),
                    tucmd: command,
                    hdrlen: desc[13],
                    mss: u16::from_le_bytes([desc[14], desc[15]]),
                };
                (command & TXD_RS != 0, command & TXD_IDE != 0)
            }
            1 => {
                let length = (dword & 0xf_ffff) as usize;
                if self.frame.data.is_empty() {
                    self.frame.popts = desc[13];
                    self.frame.tse = command & TXD_TSE != 0;
                    self.frame.extended = true;
                }
                self.gather(buffer, length);
                if command & TXD_VLE != 0 {
                    self.frame.vlan = Some(u16::from_le_bytes([desc[14], desc[15]]));
                }
                if command & TXD_EOP != 0 {
                    self.finish_frame();
                }
                (command & TXD_RS != 0, command & TXD_IDE != 0)
            }
            _ => (command & TXD_RS != 0, false),
        }
    }

    fn gather(&mut self, buffer: u64, length: usize) {
        let length = length.min(MAX_TX_FRAME.saturating_sub(self.frame.data.len()));
        let start = self.frame.data.len();
        self.frame.data.resize(start + length, 0);
        if self
            .memory
            .read_slice(&mut self.frame.data[start..], GuestAddress(buffer))
            .is_err()
        {
            self.frame.data.truncate(start);
        }
    }

    fn finish_frame(&mut self) {
        let frame = std::mem::take(&mut self.frame);
        let ctx = self.context;
        let vlan = frame.vlan.filter(|_| self.reg(CTRL) & CTRL_VME != 0);
        let mut out = Vec::new();
        if frame.extended && frame.tse && ctx.mss != 0 && ctx.hdrlen != 0 {
            for segment in segment(&frame.data, &ctx, frame.popts) {
                out.push(segment);
            }
        } else {
            let mut data = frame.data;
            if let Some((start, field)) = frame.legacy_csum {
                insert_checksum(&mut data, start as usize, field as usize, 0, 0);
            }
            if frame.extended {
                if frame.popts & POPTS_IXSM != 0 {
                    insert_checksum(
                        &mut data,
                        ctx.ipcss as usize,
                        ctx.ipcso as usize,
                        ctx.ipcse as usize,
                        0,
                    );
                }
                if frame.popts & POPTS_TXSM != 0 {
                    insert_checksum(
                        &mut data,
                        ctx.tucss as usize,
                        ctx.tucso as usize,
                        ctx.tucse as usize,
                        0,
                    );
                    if ctx.tucmd & TUCMD_TCP == 0 {
                        // UDP sends a computed zero as all ones.
                        let field = ctx.tucso as usize;
                        if data.get(field..field + 2) == Some(&[0, 0]) {
                            data[field..field + 2].copy_from_slice(&[0xff, 0xff]);
                        }
                    }
                }
            }
            out.push(data);
        }
        if frame.tse {
            self.count(TSCTC, 1);
        }
        for mut data in out {
            if data.len() < 14 {
                continue;
            }
            if let Some(tag) = vlan {
                let tpid = (self.reg(VET) & 0xffff) as u16;
                let mut tagged = data[..12].to_vec();
                tagged.extend_from_slice(&tpid.to_be_bytes());
                tagged.extend_from_slice(&tag.to_be_bytes());
                tagged.extend_from_slice(&data[12..]);
                data = tagged;
            }
            self.count_tx(&data);
            if self.link_up() {
                self.link.send(data);
            }
        }
    }

    fn count(&mut self, offset: usize, by: u32) {
        let value = self.reg(offset).wrapping_add(by);
        self.set(offset, value);
    }

    fn count64(&mut self, low: usize, by: u64) {
        let value =
            (u64::from(self.reg(low)) | u64::from(self.reg(low + 4)) << 32).wrapping_add(by);
        self.set(low, value as u32);
        self.set(low + 4, (value >> 32) as u32);
    }

    fn count_tx(&mut self, data: &[u8]) {
        let length = data.len() as u64 + 4;
        self.count(GPTC, 1);
        self.count(TPT, 1);
        self.count64(GOTCL, length);
        self.count64(TOTL, length);
        if data[0] == 0xff && data[..6].iter().all(|b| *b == 0xff) {
            self.count(BPTC, 1);
        } else if data[0] & 1 != 0 {
            self.count(MPTC, 1);
        }
        self.count(size_bucket(0x40d8, length), 1);
    }

    // ---- receive ----------------------------------------------------------------

    fn buffer_size(&self) -> usize {
        let rctl = self.reg(RCTL);
        let size = 2048 >> ((rctl >> 16) & 3);
        if rctl & RCTL_BSEX != 0 && (rctl >> 16) & 3 != 0 {
            size * 16
        } else {
            size
        }
    }

    fn accepts(&self, frame: &[u8]) -> bool {
        let rctl = self.reg(RCTL);
        let destination = &frame[..6];
        if rctl & RCTL_VFE != 0
            && frame.len() >= 16
            && u16::from_be_bytes([frame[12], frame[13]]) == (self.reg(VET) & 0xffff) as u16
        {
            let vid = u16::from_be_bytes([frame[14], frame[15]]) & 0xfff;
            if self.reg(VFTA + 4 * (vid as usize >> 5)) & 1 << (vid & 31) == 0 {
                return false;
            }
        }
        if destination.iter().all(|b| *b == 0xff) {
            return rctl & RCTL_BAM != 0 || rctl & RCTL_MPE != 0;
        }
        if destination[0] & 1 != 0 {
            if rctl & RCTL_MPE != 0 {
                return true;
            }
            let hash = match (rctl >> 12) & 3 {
                0 => (u16::from(destination[4]) >> 4) | u16::from(destination[5]) << 4,
                1 => (u16::from(destination[4]) >> 3) | u16::from(destination[5]) << 5,
                2 => (u16::from(destination[4]) >> 2) | u16::from(destination[5]) << 6,
                _ => u16::from(destination[4]) | u16::from(destination[5]) << 8,
            } & 0xfff;
            return self.reg(MTA + 4 * (hash as usize >> 5)) & 1 << (hash & 31) != 0;
        }
        if rctl & RCTL_UPE != 0 {
            return true;
        }
        (0..16).any(|i| {
            let low = self.reg(RAL0 + 8 * i);
            let high = self.reg(RAL0 + 8 * i + 4);
            high & 1 << 31 != 0
                && destination[..4] == low.to_le_bytes()
                && destination[4..6] == high.to_le_bytes()[..2]
        })
    }

    fn receive_frame(&mut self, input: &[u8], now: Instant) -> bool {
        if self.quiesced || !self.bus_master() {
            return false;
        }
        let rctl = self.reg(RCTL);
        if rctl & RCTL_EN == 0 {
            return true;
        }
        if input.len() < 14 || !self.accepts(input) {
            return true;
        }
        let mut frame = input.to_vec();
        let mut special = None;
        if self.reg(CTRL) & CTRL_VME != 0
            && frame.len() >= 18
            && u16::from_be_bytes([frame[12], frame[13]]) == (self.reg(VET) & 0xffff) as u16
        {
            special = Some(u16::from_be_bytes([frame[14], frame[15]]));
            frame.drain(12..16);
        }
        let limit = if rctl & RCTL_LPE != 0 { 16384 } else { 1522 };
        if frame.len() > limit {
            self.count(ROC, 1);
            return true;
        }
        if frame.len() < 60 {
            frame.resize(60, 0);
        }
        let (csum, status_bits, errors) = self.receive_checksums(&frame);
        if rctl & RCTL_SECRC == 0 {
            let fcs = fcs(&frame);
            frame.extend_from_slice(&fcs);
        }
        let (base, count) = self.ring(RDBAL, RDBAH, RDLEN);
        if count == 0 {
            return false;
        }
        let size = self.buffer_size();
        let needed = frame.len().div_ceil(size) as u32;
        let head = self.reg(RDH) % count;
        let tail = self.reg(RDT) % count;
        let available = (tail + count - head) % count;
        if available < needed {
            return false;
        }
        let mut written = 0usize;
        let mut index = head;
        while written < frame.len() {
            let address = GuestAddress(base.wrapping_add(u64::from(index) * 16));
            let mut desc = [0u8; 16];
            let _ = self.memory.read_slice(&mut desc[..8], address);
            let buffer = u64::from_le_bytes(desc[..8].try_into().unwrap());
            let chunk = (frame.len() - written).min(size);
            let _ = self
                .memory
                .write_slice(&frame[written..written + chunk], GuestAddress(buffer));
            written += chunk;
            let last = written == frame.len();
            let mut status = RXD_DD;
            let mut error = 0;
            if last {
                status |= RXD_EOP | status_bits;
                error = errors;
                if special.is_some() {
                    status |= RXD_VP;
                }
            }
            desc[8..10].copy_from_slice(&(chunk as u16).to_le_bytes());
            desc[10..12].copy_from_slice(&csum.to_le_bytes());
            desc[12] = status;
            desc[13] = error;
            desc[14..16].copy_from_slice(&special.unwrap_or(0).to_le_bytes());
            let _ = self
                .memory
                .write_slice(&desc[8..], address.unchecked_add(8));
            index = (index + 1) % count;
        }
        self.set(RDH, index);
        self.count_rx(input, frame.len() as u64);
        let mut causes = 0;
        let free = (tail + count - index) % count;
        let threshold = count >> (((rctl >> 8) & 3) + 1);
        if free <= threshold {
            causes |= ICR_RXDMT0;
        }
        let delay = self.reg(RDTR) & 0xffff;
        if delay == 0 {
            causes |= ICR_RXT0;
        } else {
            self.rx_timer = Some(now + units(delay));
            if self.rx_absolute.is_none() && self.reg(RADV) & 0xffff != 0 {
                self.rx_absolute = Some(now + units(self.reg(RADV)));
            }
        }
        if causes != 0 {
            self.raise(causes, now);
        }
        true
    }

    /// The packet checksum and the IP/TCP/UDP checksum results RXCSUM asks
    /// for: (descriptor csum, status bits, error bits).
    fn receive_checksums(&self, frame: &[u8]) -> (u16, u8, u8) {
        let rxcsum = self.reg(RXCSUM);
        let start = (rxcsum & 0xff) as usize;
        let csum = frame
            .get(start..)
            .map_or(0, |bytes| fold(ones_sum(0, bytes)));
        if rxcsum & (RXCSUM_IPOFL | RXCSUM_TUOFL) == 0 {
            return (csum, RXD_IXSM, 0);
        }
        let Some((ip, l4)) = parse_ipv4(frame) else {
            return (csum, 0, 0);
        };
        let mut status = 0;
        let mut errors = 0;
        if rxcsum & RXCSUM_IPOFL != 0 {
            status |= RXD_IPCS;
            if fold(ones_sum(0, &frame[ip.start..ip.header_end])) != 0xffff {
                errors |= RXE_IPE;
            }
        }
        if rxcsum & RXCSUM_TUOFL != 0
            && let Some((protocol, l4_end)) = l4
        {
            let segment = &frame[ip.header_end..l4_end];
            let udp_unchecked = protocol == 17 && segment.get(6..8) == Some(&[0, 0]);
            if !udp_unchecked {
                status |= RXD_TCPCS;
                let mut sum = ones_sum(0, &frame[ip.start + 12..ip.start + 20]);
                sum += u32::from(protocol) + segment.len() as u32;
                if fold(ones_sum(sum, segment)) != 0xffff {
                    errors |= RXE_TCPE;
                }
            }
        }
        (csum, status, errors)
    }

    fn count_rx(&mut self, frame: &[u8], length: u64) {
        let length = length.max(64);
        self.count(GPRC, 1);
        self.count(TPR, 1);
        self.count64(GORCL, length);
        self.count64(TORL, length);
        if frame[..6].iter().all(|b| *b == 0xff) {
            self.count(BPRC, 1);
        } else if frame[0] & 1 != 0 {
            self.count(MPRC, 1);
        }
        self.count(size_bucket(0x405c, length), 1);
    }

    fn timers(&mut self, now: Instant) -> Option<Instant> {
        if self.negotiation.is_some_and(|at| now >= at) {
            self.negotiation = None;
            self.update_link_status();
            if self.carrier {
                self.raise(ICR_LSC, now);
            }
        }
        let rx_due = [self.rx_timer, self.rx_absolute]
            .into_iter()
            .flatten()
            .any(|at| now >= at);
        if rx_due {
            self.rx_timer = None;
            self.rx_absolute = None;
            self.raise(ICR_RXT0, now);
        }
        let tx_due = [self.tx_timer, self.tx_absolute]
            .into_iter()
            .flatten()
            .any(|at| now >= at);
        if tx_due {
            self.tx_timer = None;
            self.tx_absolute = None;
            self.raise(ICR_TXDW, now);
        }
        if self.throttle_until.is_some_and(|at| now >= at) {
            self.throttle_until = None;
            self.update_irq(now);
        }
        [
            self.negotiation,
            self.rx_timer,
            self.rx_absolute,
            self.tx_timer,
            self.tx_absolute,
            self.throttle_until,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// Delay registers count 1.024 us units.
fn units(value: u32) -> Duration {
    Duration::from_nanos(u64::from(value & 0xffff) * 1024)
}

/// The 64, 65-127, ... 1024-1522 byte size counter for `length`.
fn size_bucket(first: usize, length: u64) -> usize {
    let index = match length {
        ..=64 => 0,
        65..=127 => 1,
        128..=255 => 2,
        256..=511 => 3,
        512..=1023 => 4,
        _ => 5,
    };
    first + 4 * index
}

struct Ipv4 {
    start: usize,
    header_end: usize,
}

/// The IPv4 header of an untagged Ethernet frame and, when it is not a
/// fragment, its TCP or UDP payload's protocol and end.
fn parse_ipv4(frame: &[u8]) -> Option<(Ipv4, Option<(u8, usize)>)> {
    if frame.len() < 34 || frame[12..14] != [0x08, 0x00] || frame[14] >> 4 != 4 {
        return None;
    }
    let start = 14;
    let header_end = start + usize::from(frame[14] & 0xf) * 4;
    let total = usize::from(u16::from_be_bytes([frame[16], frame[17]]));
    if header_end > frame.len() || total < header_end - start || start + total > frame.len() {
        return None;
    }
    let fragment = u16::from_be_bytes([frame[20], frame[21]]) & 0x3fff != 0;
    let protocol = frame[23];
    let l4 = (!fragment && matches!(protocol, 6 | 17)).then_some((protocol, start + total));
    Some((Ipv4 { start, header_end }, l4))
}

/// Ones-complement checksum of `data[start..end]` (0 = to the end) stored
/// at `field`, with `extra` added to the sum (a pseudo-header part the
/// driver left out). Out-of-range offsets leave the frame as it is.
fn insert_checksum(data: &mut [u8], start: usize, field: usize, end: usize, extra: u32) {
    let end = if end == 0 {
        data.len()
    } else {
        (end + 1).min(data.len())
    };
    if start >= end || field + 2 > data.len() {
        return;
    }
    let sum = fold(ones_sum(extra, &data[start..end]));
    data[field..field + 2].copy_from_slice(&(!sum).to_be_bytes());
}

/// TCP segmentation: the header (`hdrlen` bytes) repeated before each
/// `mss`-sized piece of the payload, with IP length and ID, TCP sequence
/// and flags, and both checksums fixed per segment.
fn segment(data: &[u8], ctx: &TxContext, popts: u8) -> Vec<Vec<u8>> {
    let header = usize::from(ctx.hdrlen);
    if data.len() <= header {
        return vec![];
    }
    let (head, payload) = data.split_at(header);
    let ipcss = usize::from(ctx.ipcss);
    let tucss = usize::from(ctx.tucss);
    let tcp = ctx.tucmd & TUCMD_TCP != 0;
    let ipv4 = ctx.tucmd & TUCMD_IP != 0;
    let chunks: Vec<&[u8]> = payload.chunks(usize::from(ctx.mss)).collect();
    let mut out = Vec::with_capacity(chunks.len());
    let mut offset = 0u32;
    for (index, chunk) in chunks.iter().enumerate() {
        let mut segment = head.to_vec();
        segment.extend_from_slice(chunk);
        let last = index + 1 == chunks.len();
        if ipv4 && ipcss + 20 <= header {
            let total = (segment.len() - ipcss) as u16;
            segment[ipcss + 2..ipcss + 4].copy_from_slice(&total.to_be_bytes());
            let id = u16::from_be_bytes([segment[ipcss + 4], segment[ipcss + 5]])
                .wrapping_add(index as u16);
            segment[ipcss + 4..ipcss + 6].copy_from_slice(&id.to_be_bytes());
        } else if !ipv4 && ipcss + 40 <= header {
            let length = (segment.len() - ipcss - 40) as u16;
            segment[ipcss + 4..ipcss + 6].copy_from_slice(&length.to_be_bytes());
        }
        // Every offset here is the guest's: one past the segment leaves the
        // segment as it is rather than underflowing or indexing past it.
        let l4_length = segment.len().saturating_sub(tucss) as u32;
        if tcp && tucss + 20 <= header {
            let sequence = u32::from_be_bytes(segment[tucss + 4..tucss + 8].try_into().unwrap())
                .wrapping_add(offset);
            segment[tucss + 4..tucss + 8].copy_from_slice(&sequence.to_be_bytes());
            if !last {
                segment[tucss + 13] &= !0x09; // FIN, PSH only on the last
            }
        } else if !tcp && tucss + 8 <= header {
            segment[tucss + 4..tucss + 6].copy_from_slice(&(l4_length as u16).to_be_bytes());
        }
        if popts & POPTS_IXSM != 0 && ipv4 {
            let ipcso = usize::from(ctx.ipcso);
            if let Some(field) = segment.get_mut(ipcso..ipcso + 2) {
                field.fill(0);
            }
            insert_checksum(
                &mut segment,
                ipcss,
                usize::from(ctx.ipcso),
                usize::from(ctx.ipcse),
                0,
            );
        }
        if popts & POPTS_TXSM != 0 {
            // The driver left the pseudo-header sum without the length in
            // the field; each segment adds its own length.
            insert_checksum(&mut segment, tucss, usize::from(ctx.tucso), 0, l4_length);
        }
        offset = offset.wrapping_add(chunk.len() as u32);
        out.push(segment);
    }
    out
}

impl LinkClient for E1000 {
    fn link_changed(&mut self, up: bool) {
        if self.carrier == up {
            return;
        }
        self.carrier = up;
        self.update_link_status();
        self.raise(ICR_LSC, Instant::now());
    }

    fn receive(&mut self, frame: &[u8]) -> bool {
        self.receive_frame(frame, Instant::now())
    }

    fn poll_timers(&mut self, now: Instant) -> Option<Instant> {
        if self.quiesced {
            return None;
        }
        self.timers(now)
    }
}

impl PciDevice for E1000 {
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
        // INTx disable and bus mastering take effect at once.
        self.drive_line();
        self.link.kick();
        self.transmit(Instant::now());
    }

    fn bars(&self) -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 0,
                size: MMIO_SIZE,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: 1,
                size: IO_SIZE,
                kind: BarKind::Io,
            },
        ]
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        match bar {
            0 => self.access(Window::Mmio, offset, data, None),
            1 => self.access(Window::Io, offset, data, None),
            _ => data.fill(0xff),
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        let mut scratch = [0u8; 8];
        let window = match bar {
            0 => Window::Mmio,
            1 => Window::Io,
            _ => return,
        };
        let len = data.len().min(8);
        self.access(window, offset, &mut scratch[..len], Some(data));
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    config: Vec<u8>,
    /// Non-zero registers: (dword index, value).
    regs: Vec<(u32, u32)>,
    phy: Vec<u16>,
    wire: EepromWire,
    io_address: u32,
    context: TxContext,
    frame: TxFrame,
    negotiating: bool,
    rx_timer: bool,
    tx_timer: bool,
    level: bool,
}

impl Snapshot for E1000 {
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
            regs: self
                .regs
                .iter()
                .enumerate()
                .filter(|(_, v)| **v != 0)
                .map(|(i, v)| (i as u32, *v))
                .collect(),
            phy: self.phy.to_vec(),
            wire: self.wire.clone(),
            io_address: self.io_address,
            context: self.context,
            frame: self.frame.clone(),
            negotiating: self.negotiation.is_some(),
            rx_timer: self.rx_timer.is_some() || self.rx_absolute.is_some(),
            tx_timer: self.tx_timer.is_some() || self.tx_absolute.is_some(),
            level: self.level,
        })
        .map_err(|e| DeviceError::State(e.to_string()))
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let saved: Saved =
            serde_json::from_slice(state).map_err(|e| DeviceError::State(e.to_string()))?;
        if saved.version != 1 || saved.config.len() != 256 || saved.phy.len() != 32 {
            return Err(DeviceError::State("invalid e1000 state".into()));
        }
        let mut regs = vec![0; self.regs.len()];
        for (index, value) in saved.regs {
            *regs
                .get_mut(index as usize)
                .ok_or_else(|| DeviceError::State("e1000 register out of range".into()))? = value;
        }
        self.config.copy_from_slice(&saved.config);
        self.regs = regs;
        self.phy.copy_from_slice(&saved.phy);
        self.wire = saved.wire;
        self.io_address = saved.io_address & 0x1_fffc;
        self.context = saved.context;
        self.frame = saved.frame;
        let now = Instant::now();
        // Pending delays fire on the first poll after the restore.
        self.negotiation = saved.negotiating.then(|| now + NEGOTIATION);
        self.rx_timer = saved.rx_timer.then_some(now);
        self.rx_absolute = None;
        self.tx_timer = saved.tx_timer.then_some(now);
        self.tx_absolute = None;
        self.throttle_until = None;
        self.level = saved.level;
        self.update_link_status();
        self.drive_line();
        self.link.kick();
        Ok(())
    }
}

impl Resettable for E1000 {
    fn reset(&mut self) -> Result<(), DeviceError> {
        let mut config = [0u8; 256];
        config[..4].copy_from_slice(&VENDOR_DEVICE.to_le_bytes());
        config[8] = 0x03;
        config[0x0b] = 0x02;
        config[0x2c..0x30].copy_from_slice(&SUBSYSTEM.to_le_bytes());
        config[0x3d] = 1;
        self.config = config;
        self.wire = EepromWire::default();
        self.io_address = 0;
        self.level = false;
        self.reset_registers();
        self.drive_line();
        Ok(())
    }
}

impl PciSnapshot for E1000 {
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
            Err(DeviceError::State("unexpected e1000 wrapped state".into()))
        }
    }
}
