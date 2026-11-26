//! The Arm PL011 UART of the aarch64 `virt` machine.
//!
//! Sources: the PrimeCell UART (PL011) Technical Reference Manual, Arm DDI
//! 0183, chapter 3 (the register summary and descriptions), and what a
//! running QEMU 11.1.3 answers (`docs/hwref/virt-devices-oracle.json`,
//! recorded by `scripts/virt-devices-oracle.py`). Where the manual and QEMU
//! differ this model does what QEMU does, because the guests ran there:
//!
//! - A transmitted byte leaves at once and sets the transmit interrupt. The
//!   transmit FIFO is always empty.
//! - The receive interrupt rises when a byte arrives in an empty FIFO, at
//!   every level of `IFLS`, and falls when a read empties the FIFO. There is
//!   no receive timeout interrupt.
//! - The receive FIFO holds 16 bytes, or one with `LCR_H.FEN` clear. A
//!   change of `FEN` empties it.
//! - The enable bits of `CR` gate nothing. The loopback bit is stored and
//!   does nothing: QEMU loops the data back, and no probe covers it.
//! - A read of `DR` with nothing received gives the byte at the read
//!   position again.
//! - An access to any byte of a register is an access to the register.
//!
//! Break and the line errors are not modelled: the host side has no way to
//! make one.

use std::io::Write;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::serial::SerialOut;
use crate::{BusDevice, DeviceError, IrqLine, Resettable, Snapshot};

/// The size of the register block.
pub const SIZE: u64 = 0x1000;

const FIFO_SIZE: usize = 16;
const SNAPSHOT_MAGIC: &[u8; 5] = b"P011\x01";

const DR: u64 = 0x00;
const RSR: u64 = 0x04;
const FR: u64 = 0x18;
const ILPR: u64 = 0x20;
const IBRD: u64 = 0x24;
const FBRD: u64 = 0x28;
const LCR_H: u64 = 0x2c;
const CR: u64 = 0x30;
const IFLS: u64 = 0x34;
const IMSC: u64 = 0x38;
const RIS: u64 = 0x3c;
const MIS: u64 = 0x40;
const ICR: u64 = 0x44;
const DMACR: u64 = 0x48;
const ID: u64 = 0xfe0;

const FR_RXFE: u32 = 1 << 4;
const FR_RXFF: u32 = 1 << 6;
const FR_TXFE: u32 = 1 << 7;
const LCR_FEN: u32 = 1 << 4;
const INT_RX: u32 = 1 << 4;
const INT_TX: u32 = 1 << 5;

/// UARTPeriphID0 to 3 and UARTPCellID0 to 3.
const IDS: [u8; 8] = [0x11, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    fifo: [u8; FIFO_SIZE],
    read_pos: usize,
    read_count: usize,
    ilpr: u32,
    ibrd: u32,
    fbrd: u32,
    lcr_h: u32,
    cr: u32,
    ifls: u32,
    imsc: u32,
    ris: u32,
    dmacr: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            fifo: [0; FIFO_SIZE],
            read_pos: 0,
            read_count: 0,
            ilpr: 0,
            ibrd: 0,
            fbrd: 0,
            lcr_h: 0,
            cr: 0x300,
            ifls: 0x12,
            imsc: 0,
            ris: 0,
            dmacr: 0,
        }
    }
}

impl State {
    fn depth(&self) -> usize {
        if self.lcr_h & LCR_FEN != 0 {
            FIFO_SIZE
        } else {
            1
        }
    }

    fn flags(&self) -> u32 {
        let mut flags = FR_TXFE;
        if self.read_count == 0 {
            flags |= FR_RXFE;
        }
        if self.read_count == self.depth() {
            flags |= FR_RXFF;
        }
        flags
    }
}

pub struct Pl011 {
    id: String,
    state: State,
    out: SerialOut,
    irq: Arc<dyn IrqLine>,
    level: Option<bool>,
}

impl Pl011 {
    pub fn new(id: impl Into<String>, out: SerialOut, irq: Arc<dyn IrqLine>) -> Self {
        let mut uart = Self {
            id: id.into(),
            state: State::default(),
            out,
            irq,
            level: None,
        };
        uart.update_irq();
        uart
    }

    /// How many bytes `receive` takes now.
    pub fn can_receive(&self) -> usize {
        self.state.depth() - self.state.read_count
    }

    /// Bytes from the host for the guest. Returns how many the FIFO took:
    /// the caller keeps the rest and offers it again later.
    pub fn receive(&mut self, bytes: &[u8]) -> usize {
        let count = bytes.len().min(self.can_receive());
        for &byte in &bytes[..count] {
            let depth = self.state.depth();
            let slot = (self.state.read_pos + self.state.read_count) % depth;
            self.state.fifo[slot] = byte;
            self.state.read_count += 1;
            if self.state.read_count == 1 {
                self.state.ris |= INT_RX;
            }
        }
        self.update_irq();
        count
    }

    fn update_irq(&mut self) {
        let level = self.state.ris & self.state.imsc != 0;
        if self.level != Some(level) {
            self.level = Some(level);
            self.irq.set_level(level);
        }
    }

    fn read_register(&mut self, offset: u64) -> u32 {
        let state = &mut self.state;
        match offset {
            DR => {
                let byte = state.fifo[state.read_pos];
                if state.read_count > 0 {
                    state.read_count -= 1;
                    state.read_pos = (state.read_pos + 1) % state.depth();
                }
                if state.read_count == 0 {
                    state.ris &= !INT_RX;
                }
                self.update_irq();
                u32::from(byte)
            }
            RSR => 0,
            FR => state.flags(),
            ILPR => state.ilpr,
            IBRD => state.ibrd,
            FBRD => state.fbrd,
            LCR_H => state.lcr_h,
            CR => state.cr,
            IFLS => state.ifls,
            IMSC => state.imsc,
            RIS => state.ris,
            MIS => state.ris & state.imsc,
            DMACR => state.dmacr,
            ID..=0xffc => u32::from(IDS[((offset - ID) / 4) as usize]),
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        let state = &mut self.state;
        match offset {
            DR => {
                // A failed write loses the byte, as a UART with no cable.
                let _ = self.out.write_all(&[value as u8]);
                let _ = self.out.flush();
                state.ris |= INT_TX;
            }
            RSR => {}
            ILPR => state.ilpr = value,
            IBRD => state.ibrd = value & 0xffff,
            FBRD => state.fbrd = value & 0x3f,
            LCR_H => {
                if (state.lcr_h ^ value) & LCR_FEN != 0 {
                    state.read_pos = 0;
                    state.read_count = 0;
                }
                state.lcr_h = value;
            }
            CR => state.cr = value & 0xffff,
            IFLS => state.ifls = value,
            IMSC => state.imsc = value,
            ICR => state.ris &= !value,
            DMACR => state.dmacr = value,
            _ => {}
        }
        self.update_irq();
    }
}

impl BusDevice for Pl011 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = self.read_register(offset & !3).to_le_bytes();
        for (byte, value) in data.iter_mut().zip(value.iter().chain(std::iter::repeat(&0))) {
            *byte = *value;
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut value = [0; 4];
        for (value, byte) in value.iter_mut().zip(data) {
            *value = *byte;
        }
        self.write_register(offset & !3, u32::from_le_bytes(value));
    }
}

impl Resettable for Pl011 {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.state = State::default();
        self.level = None;
        self.update_irq();
        Ok(())
    }
}

impl Snapshot for Pl011 {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut bytes = SNAPSHOT_MAGIC.to_vec();
        bytes.extend(
            serde_json::to_vec(&self.state)
                .map_err(|error| DeviceError::State(error.to_string()))?,
        );
        Ok(bytes)
    }

    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        let invalid = |what: &str| DeviceError::State(format!("{}: {what}", self.id));
        if bytes.len() > 4096 || !bytes.starts_with(SNAPSHOT_MAGIC) {
            return Err(invalid("invalid PL011 state or version"));
        }
        let state: State = serde_json::from_slice(&bytes[SNAPSHOT_MAGIC.len()..])
            .map_err(|error| invalid(&error.to_string()))?;
        let depth = state.depth();
        if state.read_pos >= depth || state.read_count > depth {
            return Err(invalid("the FIFO position is outside the FIFO"));
        }
        self.state = state;
        self.level = None;
        self.update_irq();
        Ok(())
    }
}
