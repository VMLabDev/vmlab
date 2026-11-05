//! The ISA DMA pair (two 8237A controllers and the page registers), the
//! system-control port B and the standard parallel port.
//!
//! The 8237A model follows the Intel 8237A datasheet: base and current
//! address and count registers, mode, mask, request, status and the byte
//! pointer flip-flop. Controller 1 (ports 0x00-0x0f) moves bytes on channels
//! 0-3; controller 2 (ports 0xc0-0xdf) moves 16-bit words on channels 5-7
//! and cascades controller 1 through channel 4. Where QEMU 11.1.2 differs
//! from the datasheet, the qtest oracle in `docs/hwref/pc-fdc-uhci-oracle.json`
//! decides: registers read zero at power-on, terminal count leaves the
//! channel unmasked, and an autoinitialising channel reloads its base
//! registers when its next transfer starts rather than at terminal count.

use crate::{BusDevice, DeviceError, GuestMemory, Snapshot};
use vm_memory::{Bytes, GuestAddress};

/// What one [`IsaDevices::dma_transfer`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DmaProgress {
    /// Bytes counted against the channel. Verify transfers count bytes
    /// without touching guest memory.
    pub bytes: usize,
    /// The channel's count expired during this transfer.
    pub terminal_count: bool,
}

#[derive(Clone, Default)]
struct DmaController {
    base_address: [u16; 4],
    address: [u16; 4],
    base_count: [u16; 4],
    count: [u16; 4],
    mode: [u8; 4],
    mask: u8,
    /// Software requests from the request register.
    request: u8,
    /// Hardware DREQ lines asserted by devices.
    dreq: u8,
    /// Terminal-count bits, cleared when the status register is read.
    terminal: u8,
    /// Autoinitialising channels that reached terminal count and reload
    /// their current registers when the next transfer starts.
    reload: u8,
    command: u8,
    high_byte: bool,
}

impl DmaController {
    fn power_on() -> Self {
        Self {
            mask: 15,
            ..Self::default()
        }
    }

    fn read(&mut self, reg: u8) -> u8 {
        match reg {
            0..=7 => {
                let channel = usize::from(reg / 2);
                let word = if reg & 1 == 0 {
                    self.address[channel]
                } else {
                    self.count[channel]
                };
                let value = (word >> if self.high_byte { 8 } else { 0 }) as u8;
                self.high_byte = !self.high_byte;
                value
            }
            8 => {
                let status = self.terminal | ((self.request | self.dreq) & 15) << 4;
                self.terminal = 0;
                status
            }
            13 => 0,
            15 => self.mask,
            _ => 0xff,
        }
    }

    fn write(&mut self, reg: u8, value: u8) {
        match reg {
            0..=7 => {
                let channel = usize::from(reg / 2);
                let (base, current) = if reg & 1 == 0 {
                    (
                        &mut self.base_address[channel],
                        &mut self.address[channel],
                    )
                } else {
                    (&mut self.base_count[channel], &mut self.count[channel])
                };
                *base = if self.high_byte {
                    (*base & 0xff) | (u16::from(value) << 8)
                } else {
                    (*base & 0xff00) | u16::from(value)
                };
                *current = *base;
                self.reload &= !(1 << channel);
                self.high_byte = !self.high_byte;
            }
            8 => self.command = value,
            9 => {
                let bit = 1 << (value & 3);
                if value & 4 != 0 {
                    self.request |= bit;
                } else {
                    self.request &= !bit;
                }
            }
            10 => {
                let bit = 1 << (value & 3);
                if value & 4 != 0 {
                    self.mask |= bit;
                } else {
                    self.mask &= !bit;
                }
            }
            11 => self.mode[usize::from(value & 3)] = value,
            12 => self.high_byte = false,
            13 => {
                self.command = 0;
                self.request = 0;
                self.terminal = 0;
                self.mask = 15;
                self.high_byte = false;
            }
            14 => self.mask = 0,
            15 => self.mask = value & 15,
            _ => {}
        }
    }

    fn save(&self, state: &mut Vec<u8>) {
        for word in self
            .base_address
            .iter()
            .chain(&self.address)
            .chain(&self.base_count)
            .chain(&self.count)
        {
            state.extend_from_slice(&word.to_le_bytes());
        }
        state.extend_from_slice(&self.mode);
        state.extend_from_slice(&[
            self.mask,
            self.request,
            self.dreq,
            self.terminal,
            self.reload,
            self.command,
            u8::from(self.high_byte),
        ]);
    }

    const SAVED: usize = 32 + 4 + 7;

    fn restore(state: &[u8]) -> Result<Self, DeviceError> {
        let word = |index: usize| u16::from_le_bytes([state[index * 2], state[index * 2 + 1]]);
        let flags = &state[36..43];
        if flags[..5].iter().any(|value| *value > 15) || flags[6] > 1 {
            return Err(DeviceError::State("invalid ISA DMA state".into()));
        }
        let mut controller = Self::default();
        for channel in 0..4 {
            controller.base_address[channel] = word(channel);
            controller.address[channel] = word(4 + channel);
            controller.base_count[channel] = word(8 + channel);
            controller.count[channel] = word(12 + channel);
        }
        controller.mode.copy_from_slice(&state[32..36]);
        controller.mask = flags[0];
        controller.request = flags[1];
        controller.dreq = flags[2];
        controller.terminal = flags[3];
        controller.reload = flags[4];
        controller.command = flags[5];
        controller.high_byte = flags[6] != 0;
        Ok(controller)
    }
}

/// Port addresses are absolute: route 0x00-0x1f, 0x61, 0x80-0x8f,
/// 0xc0-0xdf and 0x378-0x37f to this instance with [`Self::io_read`] and
/// [`Self::io_write`]. `BusDevice` also uses absolute port offsets.
pub struct IsaDevices {
    dma: [DmaController; 2],
    pages: [u8; 16],
    port_b: u8,
    refresh_phase_ns: u64,
    pit2_output: bool,
    parallel_data: u8,
    parallel_control: u8,
    memory: Option<GuestMemory>,
}

impl Default for IsaDevices {
    fn default() -> Self {
        Self::new()
    }
}

/// The page register of each DMA channel, as an index into ports 0x80-0x8f.
const PAGE: [usize; 8] = [7, 3, 1, 2, 0xf, 0xb, 9, 0xa];

impl IsaDevices {
    pub fn new() -> Self {
        Self {
            dma: [DmaController::power_on(), DmaController::power_on()],
            pages: [0; 16],
            port_b: 0,
            refresh_phase_ns: 0,
            pit2_output: false,
            parallel_data: 0,
            parallel_control: 0x0c,
            memory: None,
        }
    }

    /// The guest memory DMA transfers read and write. Without it, transfers
    /// count but move no data.
    pub fn set_memory(&mut self, memory: GuestMemory) {
        self.memory = Some(memory);
    }

    /// Set the PIT channel-2 OUT signal exposed in port 0x61 bit 5.
    pub fn set_pit2_output(&mut self, high: bool) {
        self.pit2_output = high;
    }
    pub fn speaker_enabled(&self) -> bool {
        self.port_b & 2 != 0
    }
    pub fn pit2_gate(&self) -> bool {
        self.port_b & 1 != 0
    }

    /// Advance refresh timing using elapsed guest time, including while no
    /// port accesses occur. Paused guest time must not be included.
    pub fn advance(&mut self, elapsed_ns: u64) {
        self.refresh_phase_ns = (self.refresh_phase_ns + elapsed_ns % 30_170) % 30_170;
    }

    /// A device's DREQ line for `channel` (0-7). The level shows in the
    /// status register's request bits; transfers still happen only through
    /// [`Self::dma_transfer`].
    pub fn set_dreq(&mut self, channel: usize, high: bool) {
        let Some(controller) = self.dma.get_mut(channel / 4) else {
            return;
        };
        let bit = 1 << (channel % 4);
        if high {
            controller.dreq |= bit;
        } else {
            controller.dreq &= !bit;
        }
    }

    /// Whether `channel` would move data now: its controller is enabled,
    /// the channel is unmasked and not in cascade mode.
    pub fn dma_ready(&self, channel: usize) -> bool {
        let Some(controller) = self.dma.get(channel / 4) else {
            return false;
        };
        let index = channel % 4;
        controller.command & 4 == 0
            && controller.mask & (1 << index) == 0
            && controller.mode[index] & 0xc0 != 0xc0
            && channel != 4
    }

    /// Channels whose device asserts DREQ and which [`Self::dma_ready`]
    /// would now serve, as a bit mask over channels 0-7.
    pub fn pending_requests(&self) -> u8 {
        (0..8)
            .filter(|&channel| {
                self.dma[channel / 4].dreq & (1 << (channel % 4)) != 0 && self.dma_ready(channel)
            })
            .fold(0, |mask, channel| mask | 1 << channel)
    }

    /// Move up to `data.len()` bytes on `channel` as its mode register
    /// says. A write transfer stores `data` into guest memory, a read
    /// transfer fills `data` from guest memory, and a verify transfer only
    /// counts. The transfer stops at terminal count. A masked, disabled or
    /// cascade channel moves nothing. Word channels (5-7) move whole words.
    ///
    /// As QEMU 11.1.2 does (oracle sections "page register and 64 KiB
    /// wrap" and "address-decrement mode"), one call moves one linear block
    /// of guest memory: it starts at the page and address registers and
    /// carries past a 64 KiB boundary, and in address-decrement mode it is
    /// the block that ends just below the address register. The address
    /// register itself wraps at 16 bits.
    pub fn dma_transfer(&mut self, channel: usize, data: &mut [u8]) -> DmaProgress {
        if channel >= 8 || !self.dma_ready(channel) {
            return DmaProgress::default();
        }
        let wide = channel >= 4;
        let index = channel % 4;
        let page = self.pages[PAGE[channel]];
        let controller = &mut self.dma[channel / 4];
        if controller.reload & (1 << index) != 0 {
            controller.reload &= !(1 << index);
            controller.address[index] = controller.base_address[index];
            controller.count[index] = controller.base_count[index];
        }
        let mode = controller.mode[index];
        let unit = if wide { 2 } else { 1 };
        let decrement = mode & 0x20 != 0;
        let remaining = usize::from(controller.count[index]) + 1;
        let units = (data.len() / unit).min(remaining);
        let terminal_count = units == remaining;
        let length = units * unit;
        let address = controller.address[index];
        let start = if wide {
            (u64::from(page & 0xfe) << 16) + (u64::from(address) << 1)
        } else {
            (u64::from(page) << 16) + u64::from(address)
        };
        let block = if decrement {
            start.saturating_sub(length as u64)
        } else {
            start
        };
        let chunk = &mut data[..length];
        match (mode >> 2) & 3 {
            1 => {
                if let Some(memory) = &self.memory {
                    let _ = memory.write_slice(chunk, GuestAddress(block));
                }
            }
            2 => {
                let filled = self
                    .memory
                    .as_ref()
                    .is_some_and(|memory| memory.read_slice(chunk, GuestAddress(block)).is_ok());
                if !filled {
                    chunk.fill(0xff);
                }
            }
            _ => {}
        }
        controller.address[index] = if decrement {
            address.wrapping_sub(units as u16)
        } else {
            address.wrapping_add(units as u16)
        };
        controller.count[index] = (remaining - units).wrapping_sub(1) as u16;
        if terminal_count {
            controller.terminal |= 1 << index;
            if mode & 0x10 != 0 {
                controller.reload |= 1 << index;
            }
        }
        DmaProgress {
            bytes: length,
            terminal_count,
        }
    }

    pub fn io_read(&mut self, port: u16) -> u8 {
        match port {
            0x00..=0x0f => self.dma[0].read(port as u8),
            0x80..=0x8f => self.pages[usize::from(port - 0x80)],
            0xc0..=0xdf if port & 1 == 0 => self.dma[1].read(((port - 0xc0) / 2) as u8),
            0x61 => {
                self.port_b
                    | (u8::from(self.refresh_phase_ns >= 15_085) << 4)
                    | (u8::from(self.pit2_output) << 5)
            }
            0x378 => self.parallel_data,
            0x379 => 0xd8,
            0x37a => self.parallel_control | 0xc0,
            _ => 0xff,
        }
    }

    pub fn io_write(&mut self, port: u16, value: u8) {
        match port {
            0x00..=0x0f => self.dma[0].write(port as u8, value),
            0x80..=0x8f => self.pages[usize::from(port - 0x80)] = value,
            0xc0..=0xdf if port & 1 == 0 => self.dma[1].write(((port - 0xc0) / 2) as u8, value),
            0x61 => self.port_b = value & 15,
            0x378 => self.parallel_data = value,
            0x37a => self.parallel_control = value & 0x3f,
            _ => {}
        }
    }
}

impl BusDevice for IsaDevices {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = offset
                .checked_add(i as u64)
                .and_then(|p| u16::try_from(p).ok())
                .map_or(0xff, |p| self.io_read(p));
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        for (i, &byte) in data.iter().enumerate() {
            if let Some(p) = offset
                .checked_add(i as u64)
                .and_then(|p| u16::try_from(p).ok())
            {
                self.io_write(p, byte);
            }
        }
    }
}

const STATE_V2: usize = 4 + 2 * DmaController::SAVED + 16 + 1 + 8 + 3;

impl Snapshot for IsaDevices {
    fn state_id(&self) -> String {
        "chipset.isa".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = b"ISA\x02".to_vec();
        for controller in &self.dma {
            controller.save(&mut state);
        }
        state.extend_from_slice(&self.pages);
        state.push(self.port_b);
        state.extend_from_slice(&self.refresh_phase_ns.to_le_bytes());
        state.extend_from_slice(&[
            u8::from(self.pit2_output),
            self.parallel_data,
            self.parallel_control,
        ]);
        debug_assert_eq!(state.len(), STATE_V2);
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let (dma, ports) = match state.get(..4) {
            Some(b"ISA\x02") if state.len() == STATE_V2 => {
                let first = DmaController::restore(&state[4..4 + DmaController::SAVED])?;
                let second = DmaController::restore(
                    &state[4 + DmaController::SAVED..4 + 2 * DmaController::SAVED],
                )?;
                ([first, second], &state[4 + 2 * DmaController::SAVED..])
            }
            Some(b"ISA\x01") if state.len() == 80 => (Self::restore_v1(state)?, &state[52..]),
            _ => return Err(DeviceError::State("invalid ISA state".into())),
        };
        let phase = u64::from_le_bytes(ports[17..25].try_into().unwrap());
        if phase >= 30_170 || ports[16] > 15 || ports[25] > 1 || ports[27] > 0x3f {
            return Err(DeviceError::State("invalid ISA port state".into()));
        }
        self.dma = dma;
        self.pages.copy_from_slice(&ports[..16]);
        self.port_b = ports[16];
        self.refresh_phase_ns = phase;
        self.pit2_output = ports[25] != 0;
        self.parallel_data = ports[26];
        self.parallel_control = ports[27];
        Ok(())
    }
}

impl IsaDevices {
    /// Phase 4a's probe-level layout: one address and count per channel.
    fn restore_v1(state: &[u8]) -> Result<[DmaController; 2], DeviceError> {
        let mut dmas = [DmaController::default(), DmaController::default()];
        for (i, d) in dmas.iter_mut().enumerate() {
            let s = &state[4 + i * 24..4 + (i + 1) * 24];
            if s[20] > 15 || s[21] > 15 || s[23] > 1 {
                return Err(DeviceError::State("invalid ISA DMA state".into()));
            }
            for channel in 0..4 {
                d.address[channel] = u16::from_le_bytes([s[channel * 2], s[channel * 2 + 1]]);
                d.count[channel] = u16::from_le_bytes([s[8 + channel * 2], s[9 + channel * 2]]);
            }
            d.base_address = d.address;
            d.base_count = d.count;
            d.mode.copy_from_slice(&s[16..20]);
            d.mask = s[20];
            d.request = s[21];
            d.command = s[22];
            d.high_byte = s[23] != 0;
        }
        Ok(dmas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> GuestMemory {
        GuestMemory::from_ranges(&[(GuestAddress(0), 0x40000)]).unwrap()
    }

    /// Program `channel` (0-3 or 5-7) the way an ISA driver does.
    fn program(d: &mut IsaDevices, channel: usize, mode: u8, address: u16, count: u16, page: u8) {
        let (base, step, clear, mask) = if channel < 4 {
            (0u16, 1u16, 0x0c, 0x0a)
        } else {
            (0xc0, 2, 0xd8, 0xd4)
        };
        let index = (channel % 4) as u16;
        d.io_write(mask, 4 | index as u8);
        d.io_write(base + 11 * step, (mode & !3) | index as u8);
        d.io_write(clear, 0);
        d.io_write(base + index * 2 * step, address as u8);
        d.io_write(base + index * 2 * step, (address >> 8) as u8);
        d.io_write(clear, 0);
        d.io_write(base + (index * 2 + 1) * step, count as u8);
        d.io_write(base + (index * 2 + 1) * step, (count >> 8) as u8);
        d.io_write(0x80 + PAGE[channel] as u16, page);
        d.io_write(mask, index as u8);
    }

    fn registers(d: &mut IsaDevices, channel: usize) -> (u16, u16) {
        let (base, step, clear) = if channel < 4 {
            (0u16, 1u16, 0x0c)
        } else {
            (0xc0, 2, 0xd8)
        };
        let index = (channel % 4) as u16;
        d.io_write(clear, 0);
        let address = u16::from_le_bytes([
            d.io_read(base + index * 2 * step),
            d.io_read(base + index * 2 * step),
        ]);
        let count = u16::from_le_bytes([
            d.io_read(base + (index * 2 + 1) * step),
            d.io_read(base + (index * 2 + 1) * step),
        ]);
        (address, count)
    }

    #[test]
    fn dma_programming_and_flip_flop_snapshot() {
        let mut d = IsaDevices::new();
        d.io_write(0x0c, 0);
        d.io_write(2, 0x34);
        let s = d.save_state().unwrap();
        d.io_write(2, 0x12);
        d.io_write(0x0c, 0);
        assert_eq!((d.io_read(2), d.io_read(2)), (0x34, 0x12));
        d.restore_state(&s).unwrap();
        d.io_write(2, 0x56);
        d.io_write(0x0c, 0);
        assert_eq!((d.io_read(2), d.io_read(2)), (0x34, 0x56));
        d.io_write(0x0a, 1);
        assert_eq!(d.io_read(15), 13);
        d.io_write(0x0d, 0);
        assert_eq!(d.io_read(15), 15);
        d.io_write(0xd8, 0);
        d.io_write(0xc4, 0xcd);
        d.io_write(0xc4, 0xab);
        d.io_write(0xd8, 0);
        assert_eq!((d.io_read(0xc4), d.io_read(0xc4)), (0xcd, 0xab));
        d.io_write(0x83, 0x12);
        assert_eq!(d.io_read(0x83), 0x12);
    }

    #[test]
    fn power_on_registers_read_zero_as_qemu_reports() {
        let mut d = IsaDevices::new();
        assert_eq!(d.io_read(8), 0);
        for channel in [0, 1, 2, 3, 5, 6, 7] {
            assert_eq!(registers(&mut d, channel), (0, 0));
        }
    }

    #[test]
    fn write_transfer_stops_at_terminal_count_and_leaves_the_channel_unmasked() {
        let mut d = IsaDevices::new();
        d.set_memory(memory());
        program(&mut d, 2, 0x46, 0x1000, 3, 0);
        let mut data = [1, 2, 3, 4, 5, 6];
        assert_eq!(
            d.dma_transfer(2, &mut data),
            DmaProgress {
                bytes: 4,
                terminal_count: true
            }
        );
        let mut stored = [0; 6];
        d.memory
            .as_ref()
            .unwrap()
            .read_slice(&mut stored, GuestAddress(0x1000))
            .unwrap();
        assert_eq!(stored, [1, 2, 3, 4, 0, 0]);
        assert_eq!(registers(&mut d, 2), (0x1004, 0xffff));
        assert_eq!(d.io_read(8), 0x04, "terminal count bit");
        assert_eq!(d.io_read(8), 0, "status read clears terminal count");
        assert!(d.dma_ready(2), "QEMU does not mask at terminal count");
    }

    #[test]
    fn read_transfer_fills_the_device_buffer_and_verify_moves_nothing() {
        let mut d = IsaDevices::new();
        d.set_memory(memory());
        d.memory
            .as_ref()
            .unwrap()
            .write_slice(&[9, 8, 7, 6], GuestAddress(0x2000))
            .unwrap();
        program(&mut d, 2, 0x4a, 0x2000, 3, 0);
        let mut data = [0; 4];
        assert!(d.dma_transfer(2, &mut data).terminal_count);
        assert_eq!(data, [9, 8, 7, 6]);
        program(&mut d, 2, 0x42, 0x2000, 1, 0);
        let mut data = [0x55; 2];
        assert_eq!(d.dma_transfer(2, &mut data).bytes, 2);
        let mut stored = [0; 4];
        d.memory
            .as_ref()
            .unwrap()
            .read_slice(&mut stored, GuestAddress(0x2000))
            .unwrap();
        assert_eq!(stored, [9, 8, 7, 6]);
        assert_eq!(registers(&mut d, 2), (0x2002, 0xffff));
    }

    #[test]
    fn autoinit_reloads_when_the_next_transfer_starts() {
        let mut d = IsaDevices::new();
        d.set_memory(memory());
        program(&mut d, 1, 0x56, 0x3000, 1, 0);
        assert!(d.dma_transfer(1, &mut [0xa1, 0xa2]).terminal_count);
        assert_eq!(registers(&mut d, 1), (0x3002, 0xffff));
        assert!(d.dma_transfer(1, &mut [0xb1, 0xb2]).terminal_count);
        let mut stored = [0; 4];
        d.memory
            .as_ref()
            .unwrap()
            .read_slice(&mut stored, GuestAddress(0x3000))
            .unwrap();
        assert_eq!(stored, [0xb1, 0xb2, 0, 0]);
        let snapshot = d.save_state().unwrap();
        let mut restored = IsaDevices::new();
        restored.restore_state(&snapshot).unwrap();
        restored.set_memory(memory());
        assert!(restored.dma_transfer(1, &mut [0xc1, 0xc2]).terminal_count);
        assert_eq!(registers(&mut restored, 1), (0x3002, 0xffff));
    }

    #[test]
    fn a_transfer_carries_past_the_page_while_the_address_register_wraps() {
        let mut d = IsaDevices::new();
        d.set_memory(memory());
        program(&mut d, 2, 0x46, 0xfffe, 3, 1);
        assert!(d.dma_transfer(2, &mut [1, 2, 3, 4]).terminal_count);
        let memory = d.memory.clone().unwrap();
        let mut tail = [0; 2];
        memory.read_slice(&mut tail, GuestAddress(0x1fffe)).unwrap();
        let mut head = [0; 2];
        memory.read_slice(&mut head, GuestAddress(0x10000)).unwrap();
        let mut beyond = [0; 2];
        memory.read_slice(&mut beyond, GuestAddress(0x20000)).unwrap();
        assert_eq!((tail, head, beyond), ([1, 2], [0, 0], [3, 4]));
        assert_eq!(registers(&mut d, 2), (0x0002, 0xffff));
    }

    #[test]
    fn decrement_mode_and_word_channels() {
        let mut d = IsaDevices::new();
        d.set_memory(memory());
        // Decrement mode places the block just below the address register.
        program(&mut d, 3, 0x66, 0x4004, 3, 0);
        assert!(d.dma_transfer(3, &mut [1, 2, 3, 4]).terminal_count);
        let memory = d.memory.clone().unwrap();
        let mut stored = [0; 5];
        memory.read_slice(&mut stored, GuestAddress(0x4000)).unwrap();
        assert_eq!(stored, [1, 2, 3, 4, 0]);
        assert_eq!(registers(&mut d, 3), (0x4000, 0xffff));
        let mut stored = [0; 4];
        // Channel 5: word address 0x8000 in page 2 (bit 0 ignored) is
        // byte address 0x30000; a count of 1 moves two words.
        program(&mut d, 5, 0x44, 0x8000, 1, 3);
        let progress = d.dma_transfer(5, &mut [5, 6, 7, 8, 9]);
        assert_eq!(
            progress,
            DmaProgress {
                bytes: 4,
                terminal_count: true
            }
        );
        memory.read_slice(&mut stored, GuestAddress(0x30000)).unwrap();
        assert_eq!(stored, [5, 6, 7, 8]);
        assert_eq!(d.io_read(0xd0), 0x02, "controller 2 TC on channel 5");
    }

    #[test]
    fn masked_disabled_and_cascade_channels_wait_with_dreq_visible() {
        let mut d = IsaDevices::new();
        d.set_memory(memory());
        program(&mut d, 2, 0x46, 0x5000, 0, 0);
        d.io_write(0x0a, 6);
        d.set_dreq(2, true);
        assert_eq!(d.dma_transfer(2, &mut [1]), DmaProgress::default());
        assert_eq!(d.pending_requests(), 0);
        assert_eq!(d.io_read(8), 0x40, "DREQ shows in the request bits");
        d.io_write(0x0a, 2);
        assert_eq!(d.pending_requests(), 1 << 2);
        d.io_write(0x08, 4);
        assert!(!d.dma_ready(2), "command bit 2 disables the controller");
        d.io_write(0x08, 0);
        d.io_write(0x0b, 0xc2);
        assert!(!d.dma_ready(2), "cascade mode moves no data");
        assert!(!d.dma_ready(4));
        d.set_dreq(2, false);
        assert_eq!(d.io_read(8), 0);
    }

    #[test]
    fn version_one_snapshots_restore() {
        let mut v1 = b"ISA\x01".to_vec();
        for controller in 0..2u8 {
            for word in 0..8u16 {
                v1.extend_from_slice(&(u16::from(controller) * 0x100 + word).to_le_bytes());
            }
            v1.extend_from_slice(&[0x46, 0, 0, 0, 0x0b, 0, 0, 0]);
        }
        v1.extend_from_slice(&[0x11; 16]);
        v1.push(3);
        v1.extend_from_slice(&5u64.to_le_bytes());
        v1.extend_from_slice(&[1, 0x55, 0x0c]);
        assert_eq!(v1.len(), 80);
        let mut d = IsaDevices::new();
        d.restore_state(&v1).unwrap();
        assert_eq!(registers(&mut d, 2), (2, 6));
        assert_eq!(d.io_read(0x378), 0x55);
        assert_eq!(d.io_read(0x81), 0x11);
        let v2 = d.save_state().unwrap();
        assert_eq!(v2.len(), STATE_V2);
        assert!(d.restore_state(&v2[..v2.len() - 1]).is_err());
    }

    #[test]
    fn speaker_refresh_and_parallel_probe() {
        let mut d = IsaDevices::new();
        d.io_write(0x61, 3);
        assert!(d.speaker_enabled());
        assert!(d.pit2_gate());
        d.advance(15_085);
        d.set_pit2_output(true);
        assert_eq!(d.io_read(0x61), 0x33);
        d.io_write(0x378, 0x55);
        assert_eq!(d.io_read(0x378), 0x55);
        assert_eq!(d.io_read(0x379), 0xd8);
        d.io_write(0x37a, 0x0c);
        assert_eq!(d.io_read(0x37a), 0xcc);
        assert_eq!(d.io_read(0x37b), 0xff);
        let s = d.save_state().unwrap();
        d.io_write(0x378, 0xaa);
        d.restore_state(&s).unwrap();
        assert_eq!(d.io_read(0x378), 0x55);
    }
}
