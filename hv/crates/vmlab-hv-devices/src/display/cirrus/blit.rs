//! The CL-GD5446 BitBLT engine.
//!
//! Register layout and semantics follow the CL-GD5446 Technical Reference
//! Manual: GR20-GR2E geometry and addresses, GR2F left-side clipping, GR30
//! mode, GR32 raster operation, GR33 mode extensions, GR34-GR37 transparency,
//! and the extended foreground and background colours in GR0/GR1 and
//! GR10-GR15. The Windows 98 SE capture (docs/hwref/pc-windows-98-se) fixes the
//! source layout of colour expansion from video memory: rows are packed with
//! no source pitch.

use serde::{Deserialize, Serialize};

pub(super) const BACKWARDS: u8 = 0x01;
pub(super) const SYSTEM_DEST: u8 = 0x02;
pub(super) const SYSTEM_SOURCE: u8 = 0x04;
pub(super) const TRANSPARENT: u8 = 0x08;
pub(super) const PATTERN: u8 = 0x40;
pub(super) const EXPAND: u8 = 0x80;
pub(super) const EXT_INVERT: u8 = 0x02;
pub(super) const EXT_SOLID: u8 = 0x04;

/// The sixteen CL-GD5446 raster operations, as GR32 encodes them. An
/// undefined code leaves the destination unchanged.
pub(super) fn rop(code: u8, s: u8, d: u8) -> u8 {
    match code {
        0x00 => 0,
        0x05 => s & d,
        0x06 => d,
        0x09 => s & !d,
        0x0b => !d,
        0x0d => s,
        0x0e => 0xff,
        0x50 => !s & d,
        0x59 => s ^ d,
        0x6d => s | d,
        0x90 => !(s | d),
        0x95 => !(s ^ d),
        0xad => s | !d,
        0xd0 => !s,
        0xd6 => !s | d,
        0xda => !(s & d),
        _ => d,
    }
}

/// The defined GR32 codes.
#[cfg(test)]
pub(super) const ROPS: [u8; 16] = [
    0x00, 0x05, 0x06, 0x09, 0x0b, 0x0d, 0x0e, 0x50, 0x59, 0x6d, 0x90, 0x95, 0xad, 0xd0, 0xd6, 0xda,
];

/// One BitBLT, latched from the registers when it starts. A system-source
/// BitBLT stays latched while the CPU supplies its rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Blit {
    pub width: usize,
    pub height: usize,
    pub dst_pitch: usize,
    pub src_pitch: usize,
    pub dst: usize,
    pub src: usize,
    pub mode: u8,
    pub ext: u8,
    pub rop: u8,
    pub skip: usize,
    pub fg: u32,
    pub bg: u32,
    pub key: u16,
    pub key_mask: u16,
    /// The next row a system source supplies.
    pub row: usize,
    /// Source bytes received for `row`.
    pub pending: Vec<u8>,
}

/// A copy of the video memory a BitBLT touches. Addresses wrap at the size
/// of video memory.
pub(super) struct Window {
    pub base: usize,
    pub data: Vec<u8>,
    mask: usize,
}

impl Window {
    pub fn new(base: usize, data: Vec<u8>, size: usize) -> Self {
        Self {
            base,
            data,
            mask: size - 1,
        }
    }

    fn at(&self, addr: i64) -> usize {
        (addr as usize & self.mask) - self.base
    }

    fn get(&self, addr: i64) -> u8 {
        self.data[self.at(addr)]
    }

    fn set(&mut self, addr: i64, value: u8) {
        let at = self.at(addr);
        self.data[at] = value;
    }
}

impl Blit {
    /// Latch a BitBLT from the graphics controller registers (`gr[i]` is GRi).
    pub fn latch(gr: &[u8]) -> Self {
        let word = |lo: usize, mask: u8| gr[lo] as usize | ((gr[lo + 1] & mask) as usize) << 8;
        let triple = |lo: usize| {
            gr[lo] as usize | (gr[lo + 1] as usize) << 8 | ((gr[lo + 2] & 0x3f) as usize) << 16
        };
        Self {
            width: word(0x20, 0x1f) + 1,
            height: word(0x22, 0x07) + 1,
            dst_pitch: word(0x24, 0x1f),
            src_pitch: word(0x26, 0x1f),
            dst: triple(0x28),
            src: triple(0x2c),
            mode: gr[0x30],
            ext: gr[0x33],
            rop: gr[0x32],
            skip: (gr[0x2f] & 0x07) as usize,
            fg: u32::from_le_bytes([gr[0x01], gr[0x11], gr[0x13], gr[0x15]]),
            bg: u32::from_le_bytes([gr[0x00], gr[0x10], gr[0x12], gr[0x14]]),
            key: u16::from_le_bytes([gr[0x34], gr[0x35]]),
            key_mask: u16::from_le_bytes([gr[0x36], gr[0x37]]),
            row: 0,
            pending: Vec::new(),
        }
    }

    /// Bytes per pixel: GR30 bits 5:4 select 8, 16, 24 or 32 bits.
    pub fn bpp(&self) -> usize {
        1 + ((self.mode >> 4) & 3) as usize
    }

    pub fn system_source(&self) -> bool {
        self.mode & SYSTEM_SOURCE != 0 && self.mode & PATTERN == 0 && self.ext & EXT_SOLID == 0
    }

    fn expand(&self) -> bool {
        self.mode & EXPAND != 0
    }

    fn backwards(&self) -> bool {
        self.mode & BACKWARDS != 0 && !self.expand() && self.mode & PATTERN == 0
    }

    fn pixels(&self) -> usize {
        self.width.div_ceil(self.bpp())
    }

    /// Bytes of one source row: packed monochrome rows for expansion from
    /// video memory, and dword-padded rows from the CPU.
    pub fn source_row_bytes(&self) -> usize {
        let bytes = if self.expand() {
            self.pixels().div_ceil(8)
        } else {
            self.width
        };
        if self.system_source() {
            bytes.next_multiple_of(4)
        } else {
            bytes
        }
    }

    fn pattern_stride(&self) -> usize {
        match self.bpp() {
            3 => 32,
            bpp => 8 * bpp,
        }
    }

    fn pattern_base(&self) -> usize {
        if self.expand() {
            self.src & !7
        } else {
            self.src & !(8 * self.pattern_stride() - 1)
        }
    }

    /// The unwrapped address span rows `rows` read or write, or None when it
    /// leaves `0..size`.
    pub fn span(&self, rows: std::ops::Range<usize>, size: usize) -> Option<(usize, usize)> {
        let (first, last) = (rows.start as i64, rows.end as i64 - 1);
        let width = self.width as i64;
        let mut spans = Vec::new();
        let (dp, sp) = (self.dst_pitch as i64, self.src_pitch as i64);
        let (dst, src) = (self.dst as i64, self.src as i64);
        if self.backwards() {
            spans.push((dst - last * dp - (width - 1), dst - first * dp + 1));
        } else {
            spans.push((dst + first * dp, dst + last * dp + width));
        }
        if self.system_source() || self.ext & EXT_SOLID != 0 {
        } else if self.mode & PATTERN != 0 {
            let base = self.pattern_base() as i64;
            let size = if self.expand() {
                8
            } else {
                8 * self.pattern_stride() as i64
            };
            spans.push((base, base + size));
        } else if self.expand() {
            let row = self.source_row_bytes() as i64;
            spans.push((src + first * row, src + (last + 1) * row));
        } else if self.backwards() {
            spans.push((src - last * sp - (width - 1), src - first * sp + 1));
        } else {
            spans.push((src + first * sp, src + last * sp + width));
        }
        let lo = spans.iter().map(|span| span.0).min()?;
        let hi = spans.iter().map(|span| span.1).max()?;
        (lo >= 0 && hi <= size as i64).then_some((lo as usize, hi as usize))
    }

    /// Run row `y`. `source` is the row's CPU data for a system-source BitBLT.
    pub fn row(&self, vram: &mut Window, y: usize, source: Option<&[u8]>) {
        let bpp = self.bpp();
        let dir: i64 = if self.backwards() { -1 } else { 1 };
        let drow = self.dst as i64 + dir * (y * self.dst_pitch) as i64;
        if self.expand() {
            let row_bytes = self.source_row_bytes();
            let solid = self.ext & EXT_SOLID != 0;
            let invert = self.ext & EXT_INVERT != 0;
            for px in self.skip..self.pixels() {
                let bits = if solid {
                    0xff
                } else if self.mode & PATTERN != 0 {
                    vram.get((self.pattern_base() + ((y + (self.src & 7)) & 7)) as i64)
                } else if let Some(source) = source {
                    source.get(px / 8).copied().unwrap_or(0)
                } else {
                    vram.get((self.src + y * row_bytes + px / 8) as i64)
                };
                let set = (bits & (0x80 >> (px & 7)) != 0) != invert;
                if !set && self.mode & TRANSPARENT != 0 {
                    continue;
                }
                let colour = if set { self.fg } else { self.bg };
                for k in 0..bpp.min(self.width - px * bpp) {
                    let addr = drow + (px * bpp + k) as i64;
                    let d = vram.get(addr);
                    vram.set(addr, rop(self.rop, (colour >> (8 * k)) as u8, d));
                }
            }
            return;
        }
        let srow = self.src as i64 + dir * (y * self.src_pitch) as i64;
        let pattern_row = self.pattern_base() + ((y + (self.src & 7)) & 7) * self.pattern_stride();
        for px in 0..self.pixels() {
            let count = bpp.min(self.width - px * bpp);
            let mut pixel = [0u8; 4];
            for (k, byte) in pixel.iter_mut().enumerate().take(count) {
                let x = px * bpp + k;
                *byte = if self.mode & PATTERN != 0 {
                    vram.get((pattern_row + (px & 7) * bpp + k) as i64)
                } else if let Some(source) = source {
                    source.get(x).copied().unwrap_or(0)
                } else {
                    vram.get(srow + dir * x as i64)
                };
            }
            if self.mode & TRANSPARENT != 0 && bpp <= 2 {
                // A backwards row walks each pixel from its high byte.
                let value = if dir < 0 && bpp == 2 {
                    u16::from_le_bytes([pixel[1], pixel[0]])
                } else {
                    u16::from_le_bytes([pixel[0], pixel[1]])
                };
                let care = !self.key_mask & if bpp == 1 { 0xff } else { 0xffff };
                if value & care == self.key & care {
                    continue;
                }
            }
            for (k, byte) in pixel.iter().enumerate().take(count) {
                let addr = drow + dir * (px * bpp + k) as i64;
                let d = vram.get(addr);
                vram.set(addr, rop(self.rop, *byte, d));
            }
        }
    }
}
