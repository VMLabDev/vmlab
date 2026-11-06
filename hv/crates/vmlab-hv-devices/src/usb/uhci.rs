//! PIIX3 UHCI (8086:7020) at 00:01.2 with two root ports, implemented from
//! the Intel UHCI 1.1 design guide and the 82371SB datasheet.
//!
//! The controller walks the frame list once per millisecond of running
//! guest time ([`UhciController::advance`]): queue heads, depth- and
//! breadth-first transfer descriptors, control, interrupt and bulk
//! transfers, NAK retry, timeouts with the error counter, stalls, short
//! packets, and IOC/error interrupts on INTx. Root-port devices sit behind
//! the same [`UsbDevice`] seam as xHCI; the controller tracks each device's
//! USB address itself, from SET_ADDRESS.
//!
//! QEMU 11.1.2's register quirks (byte reads return the low byte of the
//! containing word, unknown words read 0xff7f, every USBSTS bit is
//! write-one-to-clear) and its configuration space come from the qtest
//! oracle in `docs/hwref/pc-fdc-uhci-oracle.json`; the unit tests replay it.

use std::sync::Arc;

use vm_memory::{Bytes, GuestAddress};

use super::state::{Reader, Writer, invalid};
use super::{PointerEvent, SetupPacket, UsbDevice, UsbTransfer};
use crate::{
    BarInfo, BarKind, DeviceError, GuestMemory, IrqLine, PciDevice, PciSnapshot, Resettable,
    Snapshot,
};

/// Root ports, numbered 1 and 2 like PORTSC1 and PORTSC2.
pub const UHCI_PORTS: usize = 2;
/// The I/O window behind BAR4.
pub const UHCI_BAR_SIZE: u64 = 32;
const FRAME_NS: u64 = 1_000_000;
/// Frames run per `advance` call at most; a longer gap skips frames.
const MAX_FRAMES: u64 = 128;
/// Schedule elements one frame visits at most (cycle guard).
const MAX_ELEMENTS: usize = 2048;

const CMD_RS: u16 = 1;
const CMD_HCRESET: u16 = 2;
const CMD_GRESET: u16 = 4;
const STS_USBINT: u16 = 1;
const STS_ERROR: u16 = 2;
const STS_HALTED: u16 = 0x20;

const PORT_CCS: u16 = 1;
const PORT_CSC: u16 = 2;
const PORT_PE: u16 = 4;
const PORT_PEC: u16 = 8;
const PORT_RESET: u16 = 0x200;
/// Writable PORTSC bits: resume detect, port reset and suspend.
const PORT_WRITABLE: u16 = 0x0040 | PORT_RESET | 0x1000;

const TD_ACTIVE: u32 = 1 << 23;
const TD_STALLED: u32 = 1 << 22;
const TD_NAK: u32 = 1 << 19;
const TD_TIMEOUT: u32 = 1 << 18;
const TD_IOC: u32 = 1 << 24;
const TD_SPD: u32 = 1 << 29;

#[derive(Clone, Default)]
struct Control {
    setup: Option<SetupPacket>,
    /// The device's IN data, or `None` after a stall.
    response: Option<Vec<u8>>,
    position: usize,
    out: Vec<u8>,
}

struct Port {
    status: u16,
    address: u8,
    /// A SET_ADDRESS waiting for its status stage.
    pending_address: Option<u8>,
    control: Control,
    device: Option<Box<dyn UsbDevice>>,
}

impl Port {
    fn new(device: Option<Box<dyn UsbDevice>>) -> Self {
        let mut port = Self {
            status: 0,
            address: 0,
            pending_address: None,
            control: Control::default(),
            device,
        };
        port.power_on();
        port
    }

    fn power_on(&mut self) {
        self.status = if self.device.is_some() {
            PORT_CCS | PORT_CSC
        } else {
            0
        };
        self.address = 0;
        self.pending_address = None;
        self.control = Control::default();
        if let Some(device) = &mut self.device {
            device.reset();
        }
    }
}

struct Registers {
    command: u16,
    status: u16,
    interrupts: u16,
    frame: u16,
    frame_list: u32,
    sof: u8,
}

impl Registers {
    fn power_on() -> Self {
        Self {
            command: 0,
            status: STS_HALTED,
            interrupts: 0,
            frame: 0,
            frame_list: 0,
            sof: 0x40,
        }
    }
}

/// The PIIX3 USB function.
pub struct UhciController {
    id: String,
    memory: GuestMemory,
    intx: Arc<dyn IrqLine>,
    config: [u8; 256],
    registers: Registers,
    ports: [Port; UHCI_PORTS],
    /// Running time toward the next frame.
    elapsed_ns: u64,
    /// Interrupt causes collected during a frame, raised at its end.
    frame_status: u16,
}

fn power_on_config() -> [u8; 256] {
    let mut config = [0; 256];
    config[..4].copy_from_slice(&0x7020_8086u32.to_le_bytes());
    config[8..12].copy_from_slice(&0x0c03_0001u32.to_le_bytes());
    config[0x20] = 1;
    config[0x2c..0x30].copy_from_slice(&0x1100_1af4u32.to_le_bytes());
    config[0x3d] = 4;
    config[0x60] = 0x10;
    config[0x6a] = 1;
    config
}

impl UhciController {
    pub fn new(id: impl Into<String>, memory: GuestMemory, intx: Arc<dyn IrqLine>) -> Self {
        let controller = Self {
            id: id.into(),
            memory,
            intx,
            config: power_on_config(),
            registers: Registers::power_on(),
            ports: [Port::new(None), Port::new(None)],
            elapsed_ns: 0,
            frame_status: 0,
        };
        controller.intx.set_level(false);
        controller
    }

    /// Connect `device` to root port `port` (1 or 2) before the guest runs.
    pub fn attach(&mut self, port: u8, device: Box<dyn UsbDevice>) -> Result<(), DeviceError> {
        let slot = port
            .checked_sub(1)
            .and_then(|index| self.ports.get_mut(usize::from(index)))
            .ok_or_else(|| DeviceError::Other(format!("UHCI has no root port {port}")))?;
        if slot.device.is_some() {
            return Err(DeviceError::Other(format!("UHCI port {port} is occupied")));
        }
        slot.device = Some(device);
        slot.power_on();
        Ok(())
    }

    /// Queue pointer input for the attached tablet.
    pub fn inject_pointer(&mut self, event: PointerEvent) -> Result<(), DeviceError> {
        self.ports
            .iter_mut()
            .filter_map(|port| port.device.as_mut())
            .find(|device| device.state_id() == "usb-tablet")
            .ok_or_else(|| DeviceError::Other("USB tablet disconnected".into()))?
            .inject_pointer(event)
    }

    fn update_irq(&self) {
        let status = self.registers.status;
        let enabled = self.registers.interrupts;
        let level = (status & STS_USBINT != 0 && enabled & 0x0c != 0)
            || (status & STS_ERROR != 0 && enabled & 1 != 0)
            || (status & 4 != 0 && enabled & 2 != 0)
            || status & 0x18 != 0;
        self.intx
            .set_level(level && u16::from_le_bytes([self.config[4], self.config[5]]) & 0x400 == 0);
    }

    fn reset_controller(&mut self) {
        self.registers = Registers::power_on();
        for port in &mut self.ports {
            port.power_on();
        }
        self.elapsed_ns = 0;
        self.frame_status = 0;
        self.update_irq();
    }

    fn read_word(&self, offset: u64) -> u16 {
        let r = &self.registers;
        match offset {
            0 => r.command,
            2 => r.status,
            4 => r.interrupts,
            6 => r.frame,
            8 => r.frame_list as u16,
            0x0a => (r.frame_list >> 16) as u16,
            0x0c => u16::from(r.sof),
            0x10..=0x1f => match self.ports.get(usize::from((offset as u16 - 0x10) >> 1)) {
                Some(port) => port.status | 0x80,
                None => 0xff7f,
            },
            _ => 0xff7f,
        }
    }

    fn write_word(&mut self, offset: u64, value: u16) {
        match offset {
            0 => {
                if value & CMD_GRESET != 0 {
                    self.reset_controller();
                    return;
                }
                if value & CMD_HCRESET != 0 {
                    self.reset_controller();
                    return;
                }
                self.registers.command = value;
                if value & CMD_RS != 0 {
                    self.registers.status &= !STS_HALTED;
                } else {
                    self.registers.status |= STS_HALTED;
                }
            }
            2 => {
                self.registers.status &= !value;
                self.update_irq();
            }
            4 => {
                self.registers.interrupts = value;
                self.update_irq();
            }
            6 => self.registers.frame = value & 0x7ff,
            8 => {
                self.registers.frame_list =
                    (self.registers.frame_list & 0xffff_0000) | u32::from(value & 0xf000)
            }
            0x0a => {
                self.registers.frame_list =
                    (self.registers.frame_list & 0xffff) | u32::from(value) << 16
            }
            0x0c => self.registers.sof = value as u8,
            0x10..=0x1f => {
                let Some(port) = self.ports.get_mut(usize::from((offset as u16 - 0x10) >> 1))
                else {
                    return;
                };
                let resetting = port.status & PORT_RESET != 0;
                port.status &= !(value & (PORT_CSC | PORT_PEC));
                port.status = (port.status & !PORT_WRITABLE) | (value & PORT_WRITABLE);
                if value & PORT_RESET != 0 {
                    port.status &= !PORT_PE;
                } else if resetting {
                    port.address = 0;
                    port.pending_address = None;
                    port.control = Control::default();
                    if let Some(device) = &mut port.device {
                        device.reset();
                    }
                }
                if value & PORT_PE != 0 && port.status & PORT_CCS != 0 {
                    port.status |= PORT_PE;
                } else if value & PORT_PE == 0 {
                    port.status &= !PORT_PE;
                }
            }
            _ => {}
        }
    }

    /// Port I/O within BAR4, as QEMU decodes it: byte reads return the low
    /// byte of the word at the same offset, dword accesses at 0x08 reach
    /// the frame-list base and elsewhere split into words.
    pub fn read(&mut self, offset: u64, data: &mut [u8]) {
        match data.len() {
            1 => data[0] = self.read_word(offset) as u8,
            2 => data.copy_from_slice(&self.read_word(offset).to_le_bytes()),
            4 if offset == 8 => data.copy_from_slice(&self.registers.frame_list.to_le_bytes()),
            4 => {
                let value =
                    u32::from(self.read_word(offset)) | u32::from(self.read_word(offset + 2)) << 16;
                data.copy_from_slice(&value.to_le_bytes());
            }
            _ => data.fill(0xff),
        }
    }

    pub fn write(&mut self, offset: u64, data: &[u8]) {
        match data.len() {
            1 => self.write_word(offset, u16::from(data[0])),
            2 => self.write_word(offset, u16::from_le_bytes([data[0], data[1]])),
            4 if offset == 8 => {
                self.registers.frame_list = u32::from_le_bytes(data.try_into().unwrap()) & !0xfff
            }
            4 => {
                self.write_word(offset, u16::from_le_bytes([data[0], data[1]]));
                self.write_word(offset + 2, u16::from_le_bytes([data[2], data[3]]));
            }
            _ => {}
        }
    }

    /// Run the frames that `ns` of running guest time covers.
    pub fn advance(&mut self, ns: u64) {
        if self.registers.command & CMD_RS == 0 {
            self.elapsed_ns = 0;
            return;
        }
        self.elapsed_ns = self.elapsed_ns.saturating_add(ns);
        let frames = self.elapsed_ns / FRAME_NS;
        self.elapsed_ns %= FRAME_NS;
        for _ in 0..frames.min(MAX_FRAMES) {
            self.frame();
            if self.registers.command & CMD_RS == 0 {
                break;
            }
        }
    }

    fn read32(&self, address: u32) -> Option<u32> {
        self.memory
            .read_obj::<u32>(GuestAddress(u64::from(address)))
            .ok()
            .map(u32::from_le)
    }

    fn write32(&self, address: u32, value: u32) {
        let _ = self
            .memory
            .write_obj(value.to_le(), GuestAddress(u64::from(address)));
    }

    fn frame(&mut self) {
        let index = u32::from(self.registers.frame & 0x3ff);
        if self.config[4] & 4 != 0 {
            let entry = self
                .read32(self.registers.frame_list.wrapping_add(index * 4))
                .unwrap_or(1);
            self.walk(entry);
        }
        self.registers.frame = (self.registers.frame + 1) & 0x7ff;
        if self.frame_status != 0 {
            self.registers.status |= self.frame_status;
            self.frame_status = 0;
            self.update_irq();
        }
    }

    /// Walk one frame's schedule from a frame-list or horizontal pointer.
    fn walk(&mut self, mut link: u32) {
        let mut visited = 0;
        while link & 1 == 0 && visited < MAX_ELEMENTS {
            visited += 1;
            let address = link & !0xf;
            if link & 2 == 0 {
                // A TD directly in the frame list (isochronous position).
                self.transfer(address);
                link = self.read32(address).unwrap_or(1);
                continue;
            }
            let Some(horizontal) = self.read32(address) else {
                return;
            };
            self.queue(address);
            link = horizontal;
        }
    }

    /// Service the queue head at `qh` until it blocks or goes breadth-first.
    fn queue(&mut self, qh: u32) {
        for _ in 0..MAX_ELEMENTS {
            let Some(element) = self.read32(qh.wrapping_add(4)) else {
                return;
            };
            if element & 1 != 0 {
                return;
            }
            if element & 2 != 0 {
                // A nested queue head: walk it once, then continue across.
                self.queue(element & !0xf);
                return;
            }
            let td = element & !0xf;
            match self.transfer(td) {
                Outcome::Done { short } => {
                    let next = self.read32(td).unwrap_or(1);
                    let control = self.read32(td.wrapping_add(4)).unwrap_or(0);
                    if short && control & TD_SPD != 0 {
                        return;
                    }
                    self.write32(qh.wrapping_add(4), next);
                    if next & 4 == 0 {
                        return;
                    }
                }
                Outcome::Blocked | Outcome::Inactive => return,
            }
        }
    }

    /// Execute the TD at `td` once.
    fn transfer(&mut self, td: u32) -> Outcome {
        let (Some(mut control), Some(token), Some(buffer)) = (
            self.read32(td.wrapping_add(4)),
            self.read32(td.wrapping_add(8)),
            self.read32(td.wrapping_add(12)),
        ) else {
            return Outcome::Inactive;
        };
        if control & TD_ACTIVE == 0 {
            return Outcome::Inactive;
        }
        let pid = token as u8;
        let address = ((token >> 8) & 0x7f) as u8;
        let endpoint = ((token >> 15) & 0xf) as u8;
        let max_length = (((token >> 21) + 1) & 0x7ff) as usize;
        let port = self.ports.iter_mut().find(|port| {
            port.device.is_some() && port.status & PORT_PE != 0 && port.address == address
        });
        let result = match port {
            None => None,
            Some(port) => Some(execute(
                port,
                &self.memory,
                pid,
                endpoint,
                buffer,
                max_length,
            )),
        };
        let errors = (control >> 27) & 3;
        let status = match result {
            None => {
                // No device answers: a timeout, retried while the error
                // counter lasts (a zero counter retries without limit).
                control |= TD_TIMEOUT;
                if errors != 0 {
                    control = (control & !(3 << 27)) | (errors - 1) << 27;
                    if errors == 1 {
                        control &= !TD_ACTIVE;
                        self.frame_status |= STS_ERROR;
                        if control & TD_IOC != 0 {
                            self.frame_status |= STS_USBINT;
                        }
                    }
                }
                self.write32(td.wrapping_add(4), control);
                return Outcome::Blocked;
            }
            Some(status) => status,
        };
        match status {
            Status::Nak => {
                self.write32(td.wrapping_add(4), control | TD_NAK);
                Outcome::Blocked
            }
            Status::Stall => {
                control = (control & !(TD_ACTIVE | 0xff << 16)) | TD_STALLED;
                self.frame_status |= STS_ERROR;
                if control & TD_IOC != 0 {
                    self.frame_status |= STS_USBINT;
                }
                self.write32(td.wrapping_add(4), control);
                Outcome::Blocked
            }
            Status::Ack(length) => {
                let actual = (length as u32).wrapping_sub(1) & 0x7ff;
                control = (control & !(0xff << 16) & !0x7ff) | actual;
                if control & TD_IOC != 0 {
                    self.frame_status |= STS_USBINT;
                }
                let short = pid == 0x69 && length < max_length;
                if short && control & TD_SPD != 0 {
                    self.frame_status |= STS_USBINT;
                }
                self.write32(td.wrapping_add(4), control);
                Outcome::Done { short }
            }
        }
    }
}

enum Outcome {
    Done { short: bool },
    Blocked,
    Inactive,
}

enum Status {
    Ack(usize),
    Nak,
    Stall,
}

/// One packet to `port`'s device.
fn execute(
    port: &mut Port,
    memory: &GuestMemory,
    pid: u8,
    endpoint: u8,
    buffer: u32,
    max_length: usize,
) -> Status {
    let device = port.device.as_mut().unwrap();
    let read = |length: usize| {
        let mut data = vec![0; length];
        let _ = memory.read_slice(&mut data, GuestAddress(u64::from(buffer)));
        data
    };
    if endpoint == 0 {
        let control = &mut port.control;
        return match pid {
            0x2d => {
                if max_length != 8 {
                    return Status::Stall;
                }
                let bytes: [u8; 8] = read(8).try_into().unwrap();
                let setup = SetupPacket::from_bytes(bytes);
                *control = Control {
                    setup: Some(setup),
                    ..Control::default()
                };
                if setup.request_type & 0x80 != 0 || setup.length == 0 {
                    control.response = match device.control(setup, &[]) {
                        UsbTransfer::Ack(data) => Some(data),
                        UsbTransfer::Nak | UsbTransfer::Stall => None,
                    };
                    if setup.request_type == 0 && setup.request == 5 && control.response.is_some() {
                        port.pending_address = Some(setup.value as u8 & 0x7f);
                    }
                }
                Status::Ack(8)
            }
            0x69 => {
                let Some(setup) = control.setup else {
                    return Status::Stall;
                };
                if setup.request_type & 0x80 != 0 {
                    // Data stage IN.
                    let Some(response) = &control.response else {
                        return Status::Stall;
                    };
                    let chunk = &response[control.position.min(response.len())..];
                    let length = chunk.len().min(max_length);
                    let _ = memory.write_slice(&chunk[..length], GuestAddress(u64::from(buffer)));
                    control.position += length;
                    return Status::Ack(length);
                }
                // Status stage of an OUT or no-data request.
                if setup.length != 0 && control.response.is_none() {
                    control.response = match device.control(setup, &control.out) {
                        UsbTransfer::Ack(data) => Some(data),
                        UsbTransfer::Nak => return Status::Nak,
                        UsbTransfer::Stall => None,
                    };
                }
                if control.response.is_none() {
                    return Status::Stall;
                }
                if let Some(address) = port.pending_address.take() {
                    port.address = address;
                }
                Status::Ack(0)
            }
            0xe1 => {
                let Some(setup) = control.setup else {
                    return Status::Stall;
                };
                if setup.request_type & 0x80 == 0 {
                    // The data stage holds at most wLength bytes; a packet
                    // beyond it stalls, so a guest cannot grow the buffer.
                    if max_length > usize::from(setup.length).saturating_sub(control.out.len()) {
                        return Status::Stall;
                    }
                    control.out.extend_from_slice(&read(max_length));
                    return Status::Ack(max_length);
                }
                // Status stage of an IN request.
                if control.response.is_none() {
                    return Status::Stall;
                }
                Status::Ack(0)
            }
            _ => Status::Stall,
        };
    }
    match pid {
        0x69 => match device.transfer(0x80 | endpoint, &[], max_length) {
            UsbTransfer::Ack(data) => {
                let length = data.len().min(max_length);
                let _ = memory.write_slice(&data[..length], GuestAddress(u64::from(buffer)));
                Status::Ack(length)
            }
            UsbTransfer::Nak => Status::Nak,
            UsbTransfer::Stall => Status::Stall,
        },
        0xe1 => match device.transfer(endpoint, &read(max_length), 0) {
            UsbTransfer::Ack(_) => Status::Ack(max_length),
            UsbTransfer::Nak => Status::Nak,
            UsbTransfer::Stall => Status::Stall,
        },
        _ => Status::Stall,
    }
}

impl PciDevice for UhciController {
    fn config_read(&mut self, reg: usize) -> u32 {
        reg.checked_mul(4)
            .and_then(|offset| self.config.get(offset..offset.checked_add(4)?))
            .map_or(u32::MAX, |bytes| {
                u32::from_le_bytes(bytes.try_into().unwrap())
            })
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let Some(start) = reg
            .checked_mul(4)
            .and_then(|reg| reg.checked_add(offset as usize))
        else {
            return;
        };
        for (i, &byte) in data.iter().enumerate() {
            let Some(position) = start.checked_add(i).filter(|position| *position < 256) else {
                break;
            };
            let mask = match position {
                4 => 7,
                5 => 5,
                0x0c | 0x3c => 0xff,
                0x20 => 0xe0,
                0x21..=0x23 => 0xff,
                0x40..=0xff => 0xff,
                _ => 0,
            };
            self.config[position] = (self.config[position] & !mask) | (byte & mask);
        }
        self.update_irq();
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![BarInfo {
            index: 4,
            size: UHCI_BAR_SIZE,
            kind: BarKind::Io,
        }]
    }
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar == 4 && offset < UHCI_BAR_SIZE {
            self.read(offset, data);
        } else {
            data.fill(0xff);
        }
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar == 4 && offset < UHCI_BAR_SIZE {
            self.write(offset, data);
        }
    }
}

impl Snapshot for UhciController {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut w = Writer(b"VHVUHC01".to_vec());
        w.bytes(&self.config);
        let r = &self.registers;
        for value in [
            u64::from(r.command),
            u64::from(r.status),
            u64::from(r.interrupts),
            u64::from(r.frame),
            u64::from(r.frame_list),
            u64::from(r.sof),
            self.elapsed_ns,
            u64::from(self.frame_status),
        ] {
            w.u64(value);
        }
        for port in &self.ports {
            w.u64(u64::from(port.status));
            w.u64(u64::from(port.address));
            w.u64(port.pending_address.map_or(u64::MAX, u64::from));
            let control = &port.control;
            match control.setup {
                Some(setup) => {
                    w.u64(1);
                    w.bytes(&[
                        setup.request_type,
                        setup.request,
                        setup.value as u8,
                        (setup.value >> 8) as u8,
                        setup.index as u8,
                        (setup.index >> 8) as u8,
                        setup.length as u8,
                        (setup.length >> 8) as u8,
                    ]);
                }
                None => w.u64(0),
            }
            match &control.response {
                Some(response) => {
                    w.u64(1);
                    w.bytes(response);
                }
                None => w.u64(0),
            }
            w.u64(control.position as u64);
            w.bytes(&control.out);
            match &port.device {
                Some(device) => {
                    w.bytes(device.state_id().as_bytes());
                    w.bytes(&device.save_state()?);
                }
                None => w.bytes(b""),
            }
        }
        Ok(w.0)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let mut r = Reader(state);
        if r.take(8)? != b"VHVUHC01" {
            return Err(invalid());
        }
        let config: [u8; 256] = r.bytes(256)?.try_into().map_err(|_| invalid())?;
        if config[..4] != self.config[..4] {
            return Err(invalid());
        }
        let mut values = [0u64; 8];
        for value in &mut values {
            *value = r.u64()?;
        }
        if values[..4].iter().any(|value| *value > 0xffff)
            || values[3] > 0x7ff
            || values[4] > u64::from(u32::MAX)
            || values[5] > 0xff
            || values[6] >= FRAME_NS
            || values[7] > 0xffff
        {
            return Err(invalid());
        }
        struct Saved {
            status: u16,
            address: u8,
            pending: Option<u8>,
            control: Control,
            device: Vec<u8>,
        }
        let mut saved = Vec::new();
        for port in &self.ports {
            let status = r.u64()?;
            let address = r.u64()?;
            let pending = r.u64()?;
            if status > 0xffff || address > 127 || (pending != u64::MAX && pending > 127) {
                return Err(invalid());
            }
            let setup = match r.u64()? {
                0 => None,
                1 => Some(SetupPacket::from_bytes(
                    r.bytes(8)?.try_into().map_err(|_| invalid())?,
                )),
                _ => return Err(invalid()),
            };
            let response = match r.u64()? {
                0 => None,
                1 => Some(r.bytes(65535)?.to_vec()),
                _ => return Err(invalid()),
            };
            let position = usize::try_from(r.u64()?).map_err(|_| invalid())?;
            let out = r.bytes(65535)?.to_vec();
            if setup.is_none_or(|setup| out.len() > usize::from(setup.length)) && !out.is_empty() {
                return Err(invalid());
            }
            let id = r.bytes(256)?;
            let expected = port.device.as_ref().map(|device| device.state_id());
            if expected.as_deref().map(str::as_bytes).unwrap_or(b"") != id {
                return Err(invalid());
            }
            let device = if expected.is_some() {
                r.bytes(1 << 20)?.to_vec()
            } else {
                Vec::new()
            };
            saved.push(Saved {
                status: status as u16,
                address: address as u8,
                pending: (pending != u64::MAX).then_some(pending as u8),
                control: Control {
                    setup,
                    response,
                    position,
                    out,
                },
                device,
            });
        }
        r.end()?;
        for (port, saved) in self.ports.iter_mut().zip(&saved) {
            if let Some(device) = &mut port.device {
                device.restore_state(&saved.device)?;
            }
        }
        self.config = config;
        self.registers = Registers {
            command: values[0] as u16,
            status: values[1] as u16,
            interrupts: values[2] as u16,
            frame: values[3] as u16,
            frame_list: values[4] as u32,
            sof: values[5] as u8,
        };
        self.elapsed_ns = values[6];
        self.frame_status = values[7] as u16;
        for (port, saved) in self.ports.iter_mut().zip(saved) {
            port.status = saved.status;
            port.address = saved.address;
            port.pending_address = saved.pending;
            port.control = saved.control;
        }
        self.update_irq();
        Ok(())
    }
}

impl Resettable for UhciController {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.config = power_on_config();
        self.reset_controller();
        Ok(())
    }
}

impl PciSnapshot for UhciController {
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
            Err(invalid())
        }
    }
}

#[cfg(test)]
mod tests;
