//! xHCI 1.0 controller and USB HID tablet, implemented from Intel xHCI 1.2b
//! and USB HID 1.11. PCI layout and tablet descriptors are black-box QEMU
//! 11.1.2 observations. No QEMU or crosvm implementation code is included.
//!
//! The integration contract and black-box measurements are in
//! `usb/README.md` beside this module.

mod host;
mod keyboard;
mod ring;
mod state;
mod tablet;
mod uhci;
mod xhci;
pub use host::HostUsbDevice;
pub use keyboard::{KEYBOARD_PORT, KEYBOARD_REPORT_DESCRIPTOR, Keyboard};

pub use tablet::{PointerEvent, TABLET_REPORT_DESCRIPTOR, Tablet};
pub use uhci::{UHCI_BAR_SIZE, UHCI_PORTS, UhciController};
pub use xhci::{BAR_SIZE, TABLET_PORT, XhciController};

use crate::{DeviceError, Snapshot};

/// USB speed IDs used by the xHCI Slot Context and PORTSC registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UsbSpeed {
    Full = 1,
    Low = 2,
    High = 3,
    Super = 4,
}

/// USB SETUP packet, with all multibyte fields in host byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupPacket {
    pub fn from_bytes(b: [u8; 8]) -> Self {
        Self {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }
}

/// A NAK keeps the transfer queued. The VMM calls `XhciController::poll`
/// when an asynchronous backend becomes ready. IN data is returned in `Ack`;
/// OUT data is passed to the backend and acknowledged with an empty vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbTransfer {
    Ack(Vec<u8>),
    Nak,
    Stall,
}

/// Port backend seam for future host USB passthrough. Endpoint addresses
/// have the USB direction bit, not the xHCI endpoint-context index.
/// Implementations must not block. Physical backends can refuse snapshots
/// through `Snapshot`; the controller propagates that refusal.
pub trait UsbDevice: Snapshot {
    fn inject_key(&mut self, _qcode: &str, _down: bool) -> Result<(), DeviceError> {
        Err(DeviceError::Other(
            "USB device does not accept keyboard input".into(),
        ))
    }
    fn speed(&self) -> UsbSpeed;
    fn reset(&mut self);
    fn control(&mut self, setup: SetupPacket, data: &[u8]) -> UsbTransfer;
    fn transfer(&mut self, endpoint: u8, data: &[u8], max_length: usize) -> UsbTransfer;
    /// Isochronous transfer: as `transfer`, for one TD, which is the data of
    /// one service interval: up to three packets on a high-bandwidth
    /// high-speed endpoint and more on SuperSpeed. `packet_size` is the
    /// endpoint's Max Packet Size and does not divide the TD. The default
    /// delegates to `transfer`.
    fn transfer_iso(
        &mut self,
        endpoint: u8,
        data: &[u8],
        max_length: usize,
        packet_size: u32,
    ) -> UsbTransfer {
        let _ = packet_size;
        self.transfer(endpoint, data, max_length)
    }
    /// The guest dropped this endpoint's transfer state: Stop Endpoint,
    /// Reset Endpoint, Set TR Dequeue Pointer, Reset Device, Disable Slot,
    /// a Configure Endpoint that drops, replaces or deconfigures it, a port
    /// reset, a controller reset or a machine reset. Backends with
    /// in-flight work discard it, so a stale completion is never
    /// attributed to a new TD.
    fn endpoint_dropped(&mut self, endpoint: u8) {
        let _ = endpoint;
    }
    /// Whether the physical device went away. The controller reports lost
    /// ports so the machine can detach them.
    fn is_lost(&self) -> bool {
        false
    }
    /// Whether this backend passes a host device through; such machines
    /// refuse online snapshots (D9).
    fn is_host_backed(&self) -> bool {
        false
    }
    /// A full built-in tablet returns Io(WouldBlock) without consuming the event.
    /// Retry that event after the guest has drained interrupt reports.
    fn inject_pointer(&mut self, _event: PointerEvent) -> Result<(), DeviceError> {
        Err(DeviceError::Other(
            "USB device does not accept pointer input".into(),
        ))
    }
}

/// Counts the controller's reads of guest memory on the calling thread, so
/// a test can measure the work one poll or one frame run does (usb review
/// F11). Data buffers of a transfer are not counted.
#[cfg(test)]
pub(crate) mod dma_probe {
    use std::cell::Cell;

    thread_local! {
        static READS: Cell<u64> = const { Cell::new(0) };
    }
    pub fn count() {
        READS.with(|reads| reads.set(reads.get() + 1));
    }
    /// The reads since the last call, and zero from here.
    pub fn take() -> u64 {
        READS.with(|reads| reads.replace(0))
    }
}

#[cfg(test)]
mod tests;
