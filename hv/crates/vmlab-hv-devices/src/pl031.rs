//! The Arm PL031 real-time clock of the aarch64 `virt` machine.
//!
//! Sources: the PrimeCell Real Time Clock (PL031) Technical Reference
//! Manual, Arm DDI 0224, chapter 3, and what a running QEMU 11.1.3 answers
//! (`docs/hwref/virt-devices-oracle.json`, recorded by
//! `scripts/virt-devices-oracle.py`). What QEMU does and the manual does not
//! say:
//!
//! - The control register always reads 1: the clock cannot be stopped.
//! - The alarm fires when the counter reaches the match value, and at once
//!   when the match value is written as the counter. A match value behind
//!   the counter does not fire.
//! - A load of the counter leaves a raised interrupt as it is, and sets the
//!   alarm again for the new counter.
//! - A narrow access to the first byte of a register is an access to the
//!   register. Other bytes read 0 and ignore a write.
//!
//! The device has no thread. Its owner gives it the time (`Clock`), calls
//! `tick` when `deadline` is reached, and after it moved the clock.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{BusDevice, DeviceError, IrqLine, Resettable, Snapshot};

/// The size of the register block.
pub const SIZE: u64 = 0x1000;

const SNAPSHOT_MAGIC: &[u8; 5] = b"P031\x01";
const SECOND: u64 = 1_000_000_000;

const DR: u64 = 0x00;
const MR: u64 = 0x04;
const LR: u64 = 0x08;
const CR: u64 = 0x0c;
const IMSC: u64 = 0x10;
const RIS: u64 = 0x14;
const MIS: u64 = 0x18;
const ICR: u64 = 0x1c;
const ID: u64 = 0xfe0;

/// RTCPeriphID0 to 3 and RTCPCellID0 to 3.
const IDS: [u8; 8] = [0x31, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

/// The time of the machine in nanoseconds: it stands still while the
/// machine is paused, and a snapshot keeps it.
pub trait Clock: Send + Sync {
    fn now_ns(&self) -> u64;
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    /// The counter is this plus the whole seconds of the clock.
    offset: u32,
    match_value: u32,
    load: u32,
    mask: bool,
    raised: bool,
    /// The second of the clock at which the alarm fires.
    alarm: Option<u64>,
}

pub struct Pl031 {
    id: String,
    state: State,
    /// The counter at the second 0 of the clock, after a reset.
    epoch: u32,
    clock: Arc<dyn Clock>,
    irq: Arc<dyn IrqLine>,
    level: Option<bool>,
}

impl Pl031 {
    /// A clock that reads `epoch` (seconds since 1970) when `clock` is at 0.
    pub fn new(
        id: impl Into<String>,
        epoch: u32,
        clock: Arc<dyn Clock>,
        irq: Arc<dyn IrqLine>,
    ) -> Self {
        let mut rtc = Self {
            id: id.into(),
            state: State {
                offset: epoch,
                ..State::default()
            },
            epoch,
            clock,
            irq,
            level: None,
        };
        rtc.update_irq();
        rtc
    }

    fn seconds(&self) -> u64 {
        self.clock.now_ns() / SECOND
    }

    fn counter(&self) -> u32 {
        self.state.offset.wrapping_add(self.seconds() as u32)
    }

    /// Set the alarm for the match value: now, at a second to come, or not
    /// at all.
    fn arm(&mut self) {
        let ahead = self.state.match_value.wrapping_sub(self.counter());
        if ahead == 0 {
            self.state.alarm = None;
            self.state.raised = true;
        } else if ahead < 1 << 31 {
            self.state.alarm = Some(self.seconds() + u64::from(ahead));
        } else {
            self.state.alarm = None;
        }
    }

    /// Fire the alarm when its second has come, and drive the line.
    pub fn tick(&mut self) {
        if self
            .state
            .alarm
            .is_some_and(|alarm| self.seconds() >= alarm)
        {
            self.state.alarm = None;
            self.state.raised = true;
        }
        self.update_irq();
    }

    /// The time of the clock, in nanoseconds, at which `tick` has work.
    pub fn deadline(&self) -> Option<u64> {
        self.state.alarm.map(|alarm| alarm.saturating_mul(SECOND))
    }

    fn update_irq(&mut self) {
        let level = self.state.raised && self.state.mask;
        if self.level != Some(level) {
            self.level = Some(level);
            self.irq.set_level(level);
        }
    }

    fn read_register(&mut self, offset: u64) -> u32 {
        self.tick();
        match offset {
            DR => self.counter(),
            MR => self.state.match_value,
            LR => self.state.load,
            CR => 1,
            IMSC => u32::from(self.state.mask),
            RIS => u32::from(self.state.raised),
            MIS => u32::from(self.state.raised && self.state.mask),
            ID..=0xffc if offset % 4 == 0 => u32::from(IDS[((offset - ID) / 4) as usize]),
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        self.tick();
        match offset {
            MR => {
                self.state.match_value = value;
                self.arm();
            }
            LR => {
                self.state.load = value;
                self.state.offset = value.wrapping_sub(self.seconds() as u32);
                self.arm();
            }
            IMSC => self.state.mask = value & 1 != 0,
            ICR => {
                if value & 1 != 0 {
                    self.state.raised = false;
                }
            }
            _ => {}
        }
        self.update_irq();
    }
}

impl BusDevice for Pl031 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = self.read_register(offset).to_le_bytes();
        for (byte, value) in data
            .iter_mut()
            .zip(value.iter().chain(std::iter::repeat(&0)))
        {
            *byte = *value;
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut value = [0; 4];
        for (value, byte) in value.iter_mut().zip(data) {
            *value = *byte;
        }
        self.write_register(offset, u32::from_le_bytes(value));
    }
}

impl Resettable for Pl031 {
    fn reset(&mut self) -> Result<(), DeviceError> {
        // The time of day survives a reset of the machine, as on QEMU,
        // where the host clock backs it. What the guest loaded does not.
        self.state = State {
            offset: self.epoch,
            ..State::default()
        };
        self.level = None;
        self.update_irq();
        Ok(())
    }
}

impl Snapshot for Pl031 {
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
        if bytes.len() > 4096 || !bytes.starts_with(SNAPSHOT_MAGIC) {
            return Err(DeviceError::State(format!(
                "{}: invalid PL031 state or version",
                self.id
            )));
        }
        self.state = serde_json::from_slice(&bytes[SNAPSHOT_MAGIC.len()..])
            .map_err(|error| DeviceError::State(format!("{}: {error}", self.id)))?;
        self.level = None;
        self.update_irq();
        Ok(())
    }
}
