//! The ACPI generic event device (GED) of the aarch64 `virt` machine: the
//! power button of a machine without ACPI fixed hardware.
//!
//! Sources: the ACPI specification 6.5, section 5.6.9 (interrupt-signaled
//! ACPI events: the GED is a device `ACPI0013` whose `_EVT` method the OS
//! runs on its interrupt), QEMU's own document of the register that its
//! `_EVT` reads (`docs/specs/acpi_hw_reduced_hotplug.rst`: a 32-bit event
//! selector, read to clear), and what a running QEMU 11.1.3 answers
//! (`docs/hwref/virt-devices-oracle.json`, recorded by
//! `scripts/virt-devices-oracle.py`):
//!
//! - The block is the 4 bytes of the selector. The power button is bit 1.
//! - A 32-bit read gives the events and clears them. A narrower read gives
//!   0 and clears nothing. A write does nothing.
//! - An event pulses the interrupt line: it is an edge.
//! - `virt` has no sleep and reset registers beside it. The guest powers
//!   off and resets through PSCI.

use std::sync::Arc;

use crate::{BusDevice, DeviceError, IrqLine, Resettable, Snapshot};

/// The size of the register block.
pub const SIZE: u64 = 4;

/// The power button, as the `_EVT` method of the DSDT must test it.
pub const EVENT_POWER_DOWN: u32 = 1 << 1;

const SNAPSHOT_MAGIC: &[u8; 4] = b"GED\x01";

pub struct Ged {
    id: String,
    events: u32,
    irq: Arc<dyn IrqLine>,
}

impl Ged {
    pub fn new(id: impl Into<String>, irq: Arc<dyn IrqLine>) -> Self {
        Self {
            id: id.into(),
            events: 0,
            irq,
        }
    }

    /// Tell the guest of `events` (a mask of `EVENT_*`).
    pub fn notify(&mut self, events: u32) {
        self.events |= events;
        self.irq.set_level(true);
        self.irq.set_level(false);
    }

    /// Press the power button.
    pub fn power_down(&mut self) {
        self.notify(EVENT_POWER_DOWN);
    }
}

impl BusDevice for Ged {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if offset == 0 && data.len() == 4 {
            data.copy_from_slice(&std::mem::take(&mut self.events).to_le_bytes());
        }
    }

    fn write(&mut self, _offset: u64, _data: &[u8]) {}
}

impl Resettable for Ged {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.events = 0;
        Ok(())
    }
}

impl Snapshot for Ged {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut bytes = SNAPSHOT_MAGIC.to_vec();
        bytes.extend(self.events.to_le_bytes());
        Ok(bytes)
    }

    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        match bytes.strip_prefix(SNAPSHOT_MAGIC).map(<[u8; 4]>::try_from) {
            Some(Ok(events)) => {
                self.events = u32::from_le_bytes(events);
                Ok(())
            }
            _ => Err(DeviceError::State(format!(
                "{}: invalid GED state or version",
                self.id
            ))),
        }
    }
}
