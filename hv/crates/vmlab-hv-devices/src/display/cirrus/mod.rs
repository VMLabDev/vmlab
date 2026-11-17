//! Cirrus Logic CL-GD5446 on the VGA core, independent of the VMM.
//!
//! Route ports 0x3b0..=0x3df through `io_read`/`io_write` and 0xa0000..0xc0000
//! through `legacy_read`/`legacy_write`, as for `StandardVga`. Ports
//! 0x1ce..=0x1d0 read as open bus: the CL-GD5446 has no Bochs DISPI.
//! PCI identity, BARs and the 64 KiB ROM follow docs/hwref/pc-windows-98-se.
//! BAR0 is 32 MiB and prefetchable: video memory (4 MiB, aliased up to 16 MiB)
//! and, from 16 MiB, the BitBLT system-source port. The VMM may map the first
//! `VRAM_SIZE` bytes of BAR0 directly; those accesses are plain memory, as the
//! capture's memory tree shows. BAR1 is 4 KiB: ports 0x3c0.. at offset 0 and
//! the BitBLT registers at 0x100, in the layout of the 0xb8000 MMIO window.
//!
//! Sources: the CL-GD5446 Technical Reference Manual for the extension
//! registers, the FreeVGA register reference for the VGA core it extends,
//! and the Windows 98 SE capture for the subset QEMU presents.

mod blit;
#[cfg(test)]
mod tests;

use super::{CRTC, Frame, GC, Mode, PixelFormat, ROM_BAR, SEQ, StandardVga};
use crate::{BarInfo, BarKind, DeviceError, PciDevice, Snapshot};
use blit::{Blit, Window};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

/// Video memory, from the capture's `vgamem_mb = 4`.
pub const VRAM_SIZE: usize = 4 << 20;
/// BAR0: video memory apertures and the BitBLT port.
pub const BAR0_SIZE: u64 = 32 << 20;
/// BAR1: the MMIO register block.
pub const MMIO_SIZE: u64 = 4096;
pub const ROM_SIZE: usize = 64 << 10;
const BLIT_PORT: u64 = 16 << 20;
/// PCI vendor 0x1013 (Cirrus Logic), device 0x00b8 (CL-GD5446).
pub const PCI_IDENTITY: u32 = 0x00b8_1013;
/// CR27: the CL-GD5446 device ID.
const CHIP_ID: u8 = 0xb8;
/// SR0F bits that report the memory configuration: a 32-bit data bus and two
/// DRAM banks, 4 MiB. The capture reads 0x98.
const SR0F_MEMORY: u8 = 0x98;
/// SR17 bits 5:3 report the host bus: PCI.
const SR17_BUS: u8 = 0x20;
const SR17_BUS_MASK: u8 = 0x38;
const STATE_MAGIC: &[u8; 8] = b"CLGD\x01\0\0\0";

/// The CL-GD5446 BitBLT register offsets in the MMIO block, as graphics
/// controller indices.
fn mmio_register(offset: usize) -> Option<usize> {
    Some(match offset {
        0x00 => 0x00,
        0x01 => 0x10,
        0x02 => 0x12,
        0x03 => 0x14,
        0x04 => 0x01,
        0x05 => 0x11,
        0x06 => 0x13,
        0x07 => 0x15,
        0x08..=0x12 => 0x20 + offset - 0x08,
        0x14..=0x18 => 0x2c + offset - 0x14,
        0x1a..=0x1d => 0x32 + offset - 0x1a,
        0x20 | 0x21 => 0x36 + offset - 0x20,
        0x40 => 0x31,
        _ => return None,
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct State {
    hdr: u8,
    hdr_reads: u8,
    cursor_x: u16,
    cursor_y: u16,
    cursor_dac: [u8; 6],
    latch: [u8; 16],
    blit: Option<Blit>,
}

/// A CL-GD5446 PCI function. Firmware supplies the option ROM bytes.
pub struct CirrusVga {
    vga: StandardVga,
    /// The hidden DAC register, reached by four reads of 0x3c6.
    hdr: u8,
    hdr_reads: u8,
    cursor_x: u16,
    cursor_y: u16,
    /// Hardware cursor background and foreground, six-bit RGB.
    cursor_dac: [u8; 6],
    /// The eight- or sixteen-byte latch of BY8 and BY16 addressing.
    latch: [u8; 16],
    /// A system-source BitBLT waiting for CPU data.
    blit: Option<Blit>,
}

impl CirrusVga {
    /// `rom` is the host's SeaVGABIOS Cirrus image.
    pub fn new(id: impl Into<String>, rom: &[u8]) -> Result<Self, DeviceError> {
        if rom.len() > ROM_SIZE {
            return Err(DeviceError::Other("Cirrus VGA ROM exceeds 64 KiB".into()));
        }
        let mut vga = StandardVga::with_vram(id, rom, VRAM_SIZE)?;
        vga.rom.truncate(ROM_SIZE);
        vga.config = [0; 256];
        vga.config[..4].copy_from_slice(&PCI_IDENTITY.to_le_bytes());
        vga.config[4] = 3;
        vga.config[0x0b] = 3;
        vga.config[0x10] = 8;
        vga.config[0x2c..0x30].copy_from_slice(&[0xf4, 0x1a, 0, 0x11]);
        vga.dispi = [0; 16];
        vga.regs[SEQ + 6] = 0x0f;
        vga.regs[SEQ + 0x0f] = SR0F_MEMORY;
        vga.regs[SEQ + 0x17] = SR17_BUS;
        vga.regs[CRTC + 0x27] = CHIP_ID;
        Ok(Self {
            vga,
            hdr: 0,
            hdr_reads: 0,
            cursor_x: 0,
            cursor_y: 0,
            cursor_dac: [0; 6],
            latch: [0; 16],
            blit: None,
        })
    }

    fn sr(&self, index: usize) -> u8 {
        self.vga.regs[SEQ + index]
    }

    fn gr(&self, index: usize) -> u8 {
        self.vga.regs[GC + index]
    }

    fn cr(&self, index: usize) -> u8 {
        self.vga.regs[CRTC + index]
    }

    /// SR7 bit 0 selects the extended packed-pixel modes.
    fn extended(&self) -> bool {
        self.sr(7) & 1 != 0
    }

    /// In packed-pixel modes SR1 bit 5 (screen off) does not blank: the
    /// Windows 98 SE driver leaves it set at its desktop, which the capture's
    /// desktop.png shows. The attribute index's palette bit still blanks.
    pub fn scanout_active(&self) -> bool {
        if self.extended() {
            self.vga.scanout_programmed && self.vga.attr_index & 0x20 != 0
        } else {
            self.vga.scanout_active()
        }
    }

    pub fn pci_identity(&self) -> u32 {
        PCI_IDENTITY
    }

    /// Page-aligned BAR0 storage. The VMM may map it while memory decoding is on.
    pub fn linear_framebuffer(&self) -> GuestMemoryMmap {
        self.vga.linear_framebuffer()
    }

    pub fn mark_framebuffer_dirty(&mut self) {
        self.vga.mark_framebuffer_dirty();
    }

    pub fn io_read(&mut self, port: u16, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = self.port_read(port.wrapping_add(i as u16));
        }
    }

    pub fn io_write(&mut self, port: u16, data: &[u8]) {
        for (i, byte) in data.iter().enumerate() {
            self.port_write(port.wrapping_add(i as u16), *byte);
        }
    }

    /// With SR12 bit 1, DAC entries 0x00 and 0x0f are the cursor colours.
    fn cursor_slot(&self, index: u8) -> Option<usize> {
        if self.sr(0x12) & 2 == 0 {
            return None;
        }
        match index {
            0x00 => Some(0),
            0x0f => Some(3),
            _ => None,
        }
    }

    fn port_read(&mut self, port: u16) -> u8 {
        if port != 0x3c6 {
            self.hdr_reads = 0;
        }
        if matches!(port, 0x1ce..=0x1d0) || self.vga.inactive_crtc(port) {
            return 0xff;
        }
        match port {
            0x3c5 => {
                let index = (self.vga.seq_index & 0x1f) as usize;
                self.sr(index)
            }
            0x3c6 => {
                if self.hdr_reads == 4 {
                    self.hdr_reads = 0;
                    self.hdr
                } else {
                    self.hdr_reads += 1;
                    self.vga.dac_mask
                }
            }
            0x3c9 => {
                let Some(slot) = self.cursor_slot(self.vga.dac_read) else {
                    return self.vga.port_read(port);
                };
                let value = self.cursor_dac[slot + self.vga.dac_component as usize];
                self.advance_dac(false);
                value
            }
            0x3cf => {
                let index = self.vga.gc_index as usize;
                if index == 0x31 {
                    self.blit_status()
                } else {
                    self.gr(index)
                }
            }
            0x3b5 | 0x3d5 => match self.vga.crtc_index {
                0x22 => self.vga.latch[(self.gr(4) & 3) as usize],
                0x24 => (self.vga.attr_data as u8) << 7,
                0x26 => self.vga.attr_index & 0x3f,
                0x27 => CHIP_ID,
                _ => self.vga.port_read(port),
            },
            _ => self.vga.port_read(port),
        }
    }

    fn advance_dac(&mut self, write: bool) {
        self.vga.dac_component += 1;
        if self.vga.dac_component == 3 {
            self.vga.dac_component = 0;
            if write {
                self.vga.dac_write = self.vga.dac_write.wrapping_add(1);
            } else {
                self.vga.dac_read = self.vga.dac_read.wrapping_add(1);
            }
        }
    }

    fn port_write(&mut self, port: u16, value: u8) {
        if port != 0x3c6 {
            self.hdr_reads = 0;
        }
        if matches!(port, 0x1ce..=0x1d0) || self.vga.inactive_crtc(port) {
            return;
        }
        self.vga.dirty = true;
        match port {
            0x3c5 => self.seq_write(value),
            0x3c6 => {
                if self.hdr_reads == 4 {
                    self.hdr = value;
                } else {
                    self.vga.port_write(port, value);
                }
                self.hdr_reads = 0;
            }
            0x3c9 => {
                let Some(slot) = self.cursor_slot(self.vga.dac_write) else {
                    return self.vga.port_write(port, value);
                };
                self.cursor_dac[slot + self.vga.dac_component as usize] = value;
                self.advance_dac(true);
            }
            0x3cf => self.gr_write(value),
            // CR22 and CR24-CR27 are read-only.
            0x3b5 | 0x3d5 if matches!(self.vga.crtc_index, 0x22 | 0x24..=0x27) => {}
            _ => self.vga.port_write(port, value),
        }
    }

    fn seq_write(&mut self, value: u8) {
        let full = self.vga.seq_index;
        let index = (full & 0x1f) as usize;
        let old = self.sr(index);
        self.vga.regs[SEQ + index] = match index {
            // Writing 0x12 unlocks the extensions; anything else locks them.
            0x06 => {
                if value & 0x17 == 0x12 {
                    0x12
                } else {
                    0x0f
                }
            }
            0x0f => (old & SR0F_MEMORY) | (value & !SR0F_MEMORY),
            0x17 => (old & SR17_BUS_MASK) | (value & !SR17_BUS_MASK),
            _ => value,
        };
        // Index bits 7:5 of SR10 and SR11 carry cursor position bits 2:0.
        match index {
            0x10 => self.cursor_x = (value as u16) << 3 | (full >> 5) as u16,
            0x11 => self.cursor_y = (value as u16) << 3 | (full >> 5) as u16,
            _ => {}
        }
    }

    fn gr_write(&mut self, value: u8) {
        let index = self.vga.gc_index as usize;
        self.set_gr(index, value);
    }

    fn set_gr(&mut self, index: usize, value: u8) {
        if index == 0x31 {
            self.blit_control(value);
            return;
        }
        self.vga.regs[GC + index] = value;
        // With autostart, writing the destination address's high byte starts.
        if index == 0x2a && self.gr(0x31) & 0x80 != 0 {
            self.start_blit();
        }
    }

    /// GR31: bit 0 busy, bit 1 start, bit 2 reset, bit 3 in progress,
    /// bit 7 autostart.
    fn blit_status(&self) -> u8 {
        self.gr(0x31) | if self.blit.is_some() { 0x0b } else { 0 }
    }

    fn blit_control(&mut self, value: u8) {
        self.vga.regs[GC + 0x31] = value & 0xf4;
        if value & 4 != 0 {
            self.blit = None;
        } else if value & 2 != 0 {
            self.start_blit();
        }
    }

    fn start_blit(&mut self) {
        let blit = Blit::latch(&self.vga.regs[GC..GC + 256]);
        self.blit = None;
        self.vga.dirty = true;
        if blit.mode & blit::SYSTEM_DEST != 0 {
            // Screen-to-system transfers are outside the captured QEMU subset.
            return;
        }
        if blit.system_source() {
            self.blit = Some(blit);
            return;
        }
        let mut window = self.window(&blit, 0..blit.height);
        for y in 0..blit.height {
            blit.row(&mut window, y, None);
        }
        self.store(window);
    }

    fn window(&self, blit: &Blit, rows: std::ops::Range<usize>) -> Window {
        let (base, end) = blit.span(rows, VRAM_SIZE).unwrap_or((0, VRAM_SIZE));
        let mut data = vec![0; end - base];
        self.vga
            .vram
            .read_slice(&mut data, GuestAddress(base as u64))
            .expect("Cirrus video memory is mapped");
        Window::new(base, data, VRAM_SIZE)
    }

    fn store(&mut self, window: Window) {
        self.vga
            .vram
            .write_slice(&window.data, GuestAddress(window.base as u64))
            .expect("Cirrus video memory is mapped");
        self.vga.dirty = true;
    }

    /// CPU data for a system-source BitBLT, from the aperture or BAR0's port.
    fn feed(&mut self, data: &[u8]) {
        for byte in data {
            let Some(blit) = self.blit.as_mut() else {
                return;
            };
            blit.pending.push(*byte);
            if blit.pending.len() < blit.source_row_bytes() {
                continue;
            }
            let blit = self.blit.take().expect("pending BitBLT");
            let mut window = self.window(&blit, blit.row..blit.row + 1);
            blit.row(&mut window, blit.row, Some(&blit.pending));
            self.store(window);
            if blit.row + 1 < blit.height {
                self.blit = Some(Blit {
                    row: blit.row + 1,
                    pending: Vec::new(),
                    ..blit
                });
            }
        }
    }

    fn blit_register_read(&self, offset: usize) -> u8 {
        match mmio_register(offset) {
            Some(0x31) => self.blit_status(),
            Some(index) => self.gr(index),
            None => 0xff,
        }
    }

    fn blit_register_write(&mut self, offset: usize, value: u8) {
        if let Some(index) = mmio_register(offset) {
            self.set_gr(index, value);
            self.vga.dirty = true;
        }
    }

    /// SR17 bit 2 maps the BitBLT registers at 0xb8000 unless bit 6 moves them
    /// to the PCI BAR only.
    fn mmio_window(&self, offset: u64) -> Option<usize> {
        (self.sr(0x17) & 0x44 == 0x04 && (0x18000..0x18100).contains(&offset))
            .then(|| (offset - 0x18000) as usize)
    }

    /// The video memory address of an aperture offset in packed-pixel modes:
    /// GR9/GRA banks (GR0B bit 0 dual banks, bit 5 16 KiB granularity), then
    /// BY8 (GR0B bit 1) or BY16 (GR0B bits 4 and 2) scaling.
    fn bank_address(&self, offset: u64) -> Option<usize> {
        let window = self.vga.legacy_offset(offset)?;
        let mode = self.gr(0x0b);
        let (bank, within) = if mode & 1 != 0 && window >= 0x8000 {
            (self.gr(0x0a), window - 0x8000)
        } else {
            (self.gr(0x09), window)
        };
        let shift = if mode & 0x20 != 0 { 14 } else { 12 };
        let address = ((bank as usize) << shift) + within;
        Some((address * self.scale()) & (VRAM_SIZE - 1))
    }

    fn scale(&self) -> usize {
        let mode = self.gr(0x0b);
        if mode & 0x14 == 0x14 {
            16
        } else if mode & 2 != 0 {
            8
        } else {
            1
        }
    }

    fn byte(&self, addr: usize) -> u8 {
        self.vga
            .vram
            .read_obj(GuestAddress(addr as u64))
            .unwrap_or(0)
    }

    fn set_byte(&self, addr: usize, value: u8) {
        let _ = self
            .vga
            .vram
            .write_obj(value, GuestAddress((addr & (VRAM_SIZE - 1)) as u64));
    }

    pub fn legacy_read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = 0xff;
            let Some(offset) = offset.checked_add(i as u64).filter(|v| *v < 0x20000) else {
                continue;
            };
            if self.blit.is_some() {
                continue;
            }
            if let Some(register) = self.mmio_window(offset) {
                *byte = self.blit_register_read(register);
                continue;
            }
            if !self.extended() {
                self.vga.legacy_read(offset, std::slice::from_mut(byte));
                continue;
            }
            let Some(addr) = self.bank_address(offset) else {
                continue;
            };
            let scale = self.scale();
            if scale > 1 {
                for k in 0..scale {
                    self.latch[k] = self.byte((addr + k) & (VRAM_SIZE - 1));
                }
            }
            *byte = self.byte(addr);
        }
    }

    pub fn legacy_write(&mut self, offset: u64, data: &[u8]) {
        if self.blit.is_some() {
            if offset < 0x20000 {
                self.feed(data);
            }
            return;
        }
        for (i, value) in data.iter().copied().enumerate() {
            let Some(offset) = offset.checked_add(i as u64).filter(|v| *v < 0x20000) else {
                continue;
            };
            if let Some(register) = self.mmio_window(offset) {
                self.blit_register_write(register, value);
                continue;
            }
            if !self.extended() {
                self.vga.legacy_write(offset, &[value]);
                continue;
            }
            if let Some(addr) = self.bank_address(offset) {
                self.extended_write(addr, value);
            }
        }
    }

    /// Packed-pixel writes. With GR0B bit 2, write modes 4 and 5 expand each
    /// CPU bit to a pixel of the GR1/GR11 foreground (mode 5 also writes the
    /// GR0/GR10 background for clear bits). SR2 enables the eight pixels, bit 7
    /// first. Write mode 1 with GR0B bit 3 stores the wide latch.
    fn extended_write(&mut self, addr: usize, value: u8) {
        let mode = self.gr(5) & 7;
        let ext = self.gr(0x0b);
        let wide = ext & 0x14 == 0x14;
        if ext & 4 != 0 && matches!(mode, 4 | 5) {
            for x in 0..8 {
                let bit = 0x80 >> x;
                let set = value & bit != 0;
                if self.sr(2) & bit == 0 || (!set && mode == 4) {
                    continue;
                }
                let colour = if set {
                    [self.gr(1), self.gr(0x11)]
                } else {
                    [self.gr(0), self.gr(0x10)]
                };
                if wide {
                    self.set_byte(addr + 2 * x, colour[0]);
                    self.set_byte(addr + 2 * x + 1, colour[1]);
                } else {
                    self.set_byte(addr + x, colour[0]);
                }
            }
        } else if mode == 1 && ext & 8 != 0 && self.scale() > 1 {
            for k in 0..self.scale() {
                self.set_byte(addr + k, self.latch[k]);
            }
        } else {
            self.set_byte(addr, value);
        }
        self.vga.dirty = true;
    }

    fn bpp(&self) -> u32 {
        match (self.sr(7) >> 1) & 7 {
            0 => 8,
            1 | 3 => {
                if self.hdr & 0xcf == 0xc1 {
                    16
                } else {
                    15
                }
            }
            2 => 24,
            4 => 32,
            _ => 8,
        }
    }

    /// The mode of the converted XRGB8888 frame.
    pub fn mode(&self) -> Mode {
        if !self.extended() {
            return self.vga.mode();
        }
        let width = (self.cr(1) as u32 + 1) * 8;
        let overflow = self.cr(7) as u32;
        let height = self.cr(0x12) as u32 + ((overflow & 2) << 7) + ((overflow & 0x40) << 3) + 1;
        Mode {
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Xrgb8888,
        }
    }

    /// Byte offset between scanlines: CR13 with CR1B bit 4 as bit 8, in
    /// units of eight bytes.
    fn pitch(&self) -> usize {
        (self.cr(0x13) as usize | ((self.cr(0x1b) & 0x10) as usize) << 4) * 8
    }

    /// Start address: CR0C/CR0D, CR1B bits 0, 2, 3 and CR1D bit 7, in dwords.
    fn start(&self) -> usize {
        let (b, d) = (self.cr(0x1b) as usize, self.cr(0x1d) as usize);
        ((self.cr(0x0c) as usize) << 8
            | self.cr(0x0d) as usize
            | (b & 1) << 16
            | (b & 0x0c) << 15
            | (d & 0x80) << 12)
            * 4
    }

    fn dac(value: u8) -> u8 {
        ((value & 63) << 2) | ((value & 63) >> 4)
    }

    /// Convert the visible framebuffer and consume tile damage.
    pub fn frame(&mut self) -> Frame {
        if !self.extended() {
            return self.vga.frame();
        }
        let mode = self.mode();
        if !self.vga.dirty
            && let Some(frame) = self.vga.cached_frame.as_ref().filter(|f| f.mode == mode)
        {
            return frame.unchanged();
        }
        let mut pixels = vec![0; mode.stride as usize * mode.height as usize];
        let blank = self.vga.attr_index & 0x20 == 0;
        if !blank {
            let mut vram = vec![0; VRAM_SIZE];
            self.vga
                .vram
                .read_slice(&mut vram, GuestAddress(0))
                .expect("Cirrus video memory is mapped");
            self.render(&vram, mode, &mut pixels);
            self.draw_cursor(&vram, mode, &mut pixels);
        }
        let frame = Frame::compare(mode, pixels, self.vga.cached_frame.as_ref(), (16, 16));
        self.vga.cached_frame = Some(frame.clone());
        self.vga.dirty = false;
        frame
    }

    fn render(&self, vram: &[u8], mode: Mode, pixels: &mut [u8]) {
        let bpp = self.bpp();
        let bytes = bpp.div_ceil(8) as usize;
        let (start, pitch) = (self.start(), self.pitch());
        let palette: Vec<[u8; 4]> = (0..=255)
            .map(|index| self.vga.palette(index, false))
            .collect();
        let at = |addr: usize| vram[addr & (VRAM_SIZE - 1)];
        for y in 0..mode.height as usize {
            let row = start + y * pitch;
            for x in 0..mode.width as usize {
                let addr = row + x * bytes;
                let pixel = match bpp {
                    8 => palette[at(addr) as usize],
                    15 | 16 => {
                        let value = u16::from_le_bytes([at(addr), at(addr + 1)]) as u32;
                        let green = if bpp == 16 { 6 } else { 5 };
                        let r = ((value >> (5 + green)) & 31) * 255 / 31;
                        let g = ((value >> 5) & ((1 << green) - 1)) * 255 / ((1 << green) - 1);
                        let b = (value & 31) * 255 / 31;
                        [b as u8, g as u8, r as u8, 0]
                    }
                    _ => [at(addr), at(addr + 1), at(addr + 2), 0],
                };
                let pos = y * mode.stride as usize + x * 4;
                pixels[pos..pos + 4].copy_from_slice(&pixel);
            }
        }
    }

    /// The hardware cursor (SR12 bit 0): 32x32, or 64x64 with SR12 bit 2, in
    /// the last 16 KiB of video memory at the pattern SR13 selects. Plane 0
    /// and plane 1 bits give transparent (0, 0), inverted (1, 0), background
    /// (0, 1) and foreground (1, 1).
    fn draw_cursor(&self, vram: &[u8], mode: Mode, pixels: &mut [u8]) {
        let attributes = self.sr(0x12);
        if attributes & 1 == 0 {
            return;
        }
        let large = attributes & 4 != 0;
        let size = if large { 64 } else { 32 };
        let base = VRAM_SIZE - 16384
            + if large {
                (self.sr(0x13) as usize & 0x3c) * 256
            } else {
                (self.sr(0x13) as usize & 0x3f) * 256
            };
        let colour = |slot: usize| {
            [
                Self::dac(self.cursor_dac[slot + 2]),
                Self::dac(self.cursor_dac[slot + 1]),
                Self::dac(self.cursor_dac[slot]),
                0,
            ]
        };
        for row in 0..size {
            let y = self.cursor_y as usize + row;
            if y >= mode.height as usize {
                break;
            }
            for col in 0..size {
                let x = self.cursor_x as usize + col;
                if x >= mode.width as usize {
                    break;
                }
                let (plane0, plane1) = if large {
                    (base + row * 16 + col / 8, base + row * 16 + 8 + col / 8)
                } else {
                    (base + row * 4 + col / 8, base + 128 + row * 4 + col / 8)
                };
                let bit = 0x80 >> (col & 7);
                let pos = y * mode.stride as usize + x * 4;
                let pixel = &mut pixels[pos..pos + 4];
                match (vram[plane0] & bit != 0, vram[plane1] & bit != 0) {
                    (false, false) => {}
                    (true, false) => {
                        for byte in &mut pixel[..3] {
                            *byte = !*byte;
                        }
                    }
                    (false, true) => pixel.copy_from_slice(&colour(0)),
                    (true, true) => pixel.copy_from_slice(&colour(3)),
                }
            }
        }
    }
}

impl PciDevice for CirrusVga {
    fn config_read(&mut self, reg: usize) -> u32 {
        self.vga.config_read(reg)
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let Some(start) = reg
            .checked_mul(4)
            .and_then(|v| v.checked_add(usize::try_from(offset).ok()?))
        else {
            return;
        };
        let config = &mut self.vga.config;
        for (i, byte) in data.iter().enumerate() {
            let Some(pos) = start.checked_add(i).filter(|v| *v < 256) else {
                continue;
            };
            match pos {
                4 => config[pos] = byte & 0x23,
                5 => config[pos] = byte & 1,
                0x0c | 0x0d | 0x10..=0x17 | 0x30..=0x33 => config[pos] = *byte,
                _ => {}
            }
        }
        for (off, mask, flags) in [
            (0x10, !(BAR0_SIZE as u32 - 1), 8u32),
            (0x14, !(MMIO_SIZE as u32 - 1), 0),
            (0x30, !(ROM_SIZE as u32 - 1) | 1, 0),
        ] {
            let value = u32::from_le_bytes(config[off..off + 4].try_into().expect("BAR dword"));
            config[off..off + 4].copy_from_slice(&((value & mask) | flags).to_le_bytes());
        }
    }

    fn bars(&self) -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 0,
                size: BAR0_SIZE,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: 1,
                size: MMIO_SIZE,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: ROM_BAR,
                size: ROM_SIZE as u64,
                kind: BarKind::Mmio32,
            },
        ]
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let Some(at) = offset.checked_add(i as u64) else {
                *byte = 0xff;
                continue;
            };
            *byte = match bar {
                0 if at < BLIT_PORT => self.byte(at as usize & (VRAM_SIZE - 1)),
                1 if at < 0x100 => self.port_read(0x3c0 + at as u16),
                1 if at < 0x200 => self.blit_register_read(at as usize - 0x100),
                ROM_BAR => self.vga.rom.get(at as usize).copied().unwrap_or(0xff),
                _ => 0xff,
            };
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar == 0 && offset >= BLIT_PORT {
            self.feed(data);
            return;
        }
        for (i, byte) in data.iter().enumerate() {
            let Some(at) = offset.checked_add(i as u64) else {
                continue;
            };
            match bar {
                0 if at < BLIT_PORT => {
                    self.set_byte(at as usize, *byte);
                    self.vga.dirty = true;
                }
                1 if at < 0x100 => self.port_write(0x3c0 + at as u16, *byte),
                1 if at < 0x200 => self.blit_register_write(at as usize - 0x100, *byte),
                _ => {}
            }
        }
    }
}

impl crate::Resettable for CirrusVga {
    fn reset(&mut self) -> Result<(), DeviceError> {
        let initial = Self::new(self.vga.id.clone(), &self.vga.rom)?.save_state()?;
        self.restore_state(&initial)
    }
}

impl Snapshot for CirrusVga {
    fn state_id(&self) -> String {
        self.vga.state_id()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let core = self.vga.save_state()?;
        let mut state = STATE_MAGIC.to_vec();
        state.extend_from_slice(&(core.len() as u64).to_le_bytes());
        state.extend_from_slice(&core);
        let tail = State {
            hdr: self.hdr,
            hdr_reads: self.hdr_reads,
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
            cursor_dac: self.cursor_dac,
            latch: self.latch,
            blit: self.blit.clone(),
        };
        state.extend(serde_json::to_vec(&tail).map_err(|e| DeviceError::State(e.to_string()))?);
        Ok(state)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let invalid = |what: &str| DeviceError::State(format!("invalid Cirrus snapshot: {what}"));
        if state.len() < 16 || &state[..8] != STATE_MAGIC {
            return Err(invalid("version"));
        }
        let core = u64::from_le_bytes(state[8..16].try_into().expect("length")) as usize;
        let tail_at = 16usize
            .checked_add(core)
            .filter(|end| *end <= state.len())
            .ok_or_else(|| invalid("length"))?;
        let tail: State =
            serde_json::from_slice(&state[tail_at..]).map_err(|_| invalid("register state"))?;
        if tail.hdr_reads > 4
            || tail.blit.as_ref().is_some_and(|blit| {
                !blit.system_source()
                    || blit.row >= blit.height
                    || blit.pending.len() >= blit.source_row_bytes()
            })
        {
            return Err(invalid("register state"));
        }
        // The core state starts with an eight-byte header, then config space.
        if state.get(24..28) != Some(&PCI_IDENTITY.to_le_bytes()[..]) {
            return Err(invalid("PCI identity"));
        }
        self.vga.restore_state(&state[16..tail_at])?;
        self.hdr = tail.hdr;
        self.hdr_reads = tail.hdr_reads;
        self.cursor_x = tail.cursor_x;
        self.cursor_y = tail.cursor_y;
        self.cursor_dac = tail.cursor_dac;
        self.latch = tail.latch;
        self.blit = tail.blit;
        Ok(())
    }
}
