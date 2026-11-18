//! An 82078-style floppy controller in PC-AT mode at 0x3f0-0x3f5 and 0x3f7,
//! on IRQ 6 and ISA DMA channel 2, with up to two drives.
//!
//! Register layout, command set and the command/execution/result phases
//! follow the Intel 82078 datasheet. QEMU 11.1.2 is the reference where it
//! departs from the datasheet: the qtest oracle in
//! `docs/hwref/pc-fdc-uhci-oracle.json` (recorded by
//! `scripts/pc-fdc-uhci-oracle.py`, black-box, no QEMU source) pins the
//! power-on registers, the reset interrupt, DUMPREG's layout, implied seeks
//! and the result bytes of every data command. The unit tests replay it.
//!
//! The controller moves data synchronously: a read or write runs when its
//! last parameter byte arrives, or when the guest unmasks DMA channel 2
//! while the command waits on it. READ ID completes after
//! [`READ_ID_NS`] of running guest time, as QEMU's does after a delay.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use vmlab_disk::DiskImage;

use crate::chipset::isa::IsaDevices;
use crate::{BusDevice, DeviceError, IrqLine, Resettable, Snapshot};

/// The ISA DMA channel the controller uses.
pub const DMA_CHANNEL: usize = 2;
/// Guest time a READ ID command takes, within the window the oracle shows
/// for QEMU (more than 11 ms, at most 111 ms).
pub const READ_ID_NS: u64 = 20_000_000;
const SECTOR: usize = 512;

/// A medium the drive accepts, chosen by the image size (SPEC: 1.44 MB or
/// 2.88 MB).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    /// 3.5" 1.44 MB: 80 cylinders, 2 heads, 18 sectors at 500 kbit/s.
    Hd144,
    /// 3.5" 2.88 MB: 80 cylinders, 2 heads, 36 sectors at 1 Mbit/s.
    Ed288,
}

impl MediaKind {
    /// The kind for an image of `bytes`, if the drive accepts it.
    pub fn for_size(bytes: u64) -> Option<Self> {
        match bytes {
            1_474_560 => Some(Self::Hd144),
            2_949_120 => Some(Self::Ed288),
            _ => None,
        }
    }
    fn sectors(self) -> u8 {
        match self {
            Self::Hd144 => 18,
            Self::Ed288 => 36,
        }
    }
    fn rate(self) -> u8 {
        match self {
            Self::Hd144 => 0,
            Self::Ed288 => 3,
        }
    }
    /// The CMOS drive type (register 0x10 nibble): 4 for 1.44 MB, 5 for
    /// 2.88 MB.
    pub fn cmos_type(self) -> u8 {
        match self {
            Self::Hd144 => 4,
            Self::Ed288 => 5,
        }
    }
}

/// CMOS drive type of an empty drive. QEMU's drive falls back to 2.88 MB
/// (the captures read CMOS 0x10 as 0x50 with no medium).
pub const EMPTY_CMOS_TYPE: u8 = 5;
const CYLINDERS: u8 = 80;

struct Media {
    image: Box<dyn DiskImage>,
    kind: MediaKind,
    read_only: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Position {
    track: u8,
    head: u8,
    sect: u8,
}

struct Drive {
    media: Option<Media>,
    at: Position,
    /// DIR bit 7: set at power-on, cleared by a head step on a medium.
    changed: bool,
}

impl Drive {
    fn empty() -> Self {
        Self {
            media: None,
            at: Position {
                track: 0,
                head: 0,
                sect: 1,
            },
            changed: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Operation {
    Read,
    Write,
    Verify,
    Format,
    ReadId,
}

/// A command past its command phase that has not produced its result.
#[derive(Clone, Serialize, Deserialize)]
struct Execution {
    operation: Operation,
    command: Vec<u8>,
    /// The drive's track changed during the command (ST0 SE).
    seeked: bool,
    /// Non-DMA data: the sector being moved through the FIFO.
    pio: Vec<u8>,
    pio_position: usize,
    /// Remaining guest time for READ ID.
    remaining_ns: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Command,
    Execution,
    Result,
}

/// One floppy controller with two drive positions.
pub struct Floppy {
    id: String,
    isa: Arc<Mutex<IsaDevices>>,
    irq: Arc<dyn IrqLine>,
    drives: [Drive; 2],
    dor: u8,
    in_reset: bool,
    rate: u8,
    phase: Phase,
    fifo: Vec<u8>,
    result_position: usize,
    execution: Option<Execution>,
    pending: bool,
    reset_sense: u8,
    seek_status: Option<(u8, u8)>,
    srt: u8,
    timer1: u8,
    dma: bool,
    config: u8,
    pretrk: u8,
    lock: bool,
    perpendicular: u8,
}

impl Floppy {
    pub fn new(id: impl Into<String>, isa: Arc<Mutex<IsaDevices>>, irq: Arc<dyn IrqLine>) -> Self {
        let floppy = Self {
            id: id.into(),
            isa,
            irq,
            drives: [Drive::empty(), Drive::empty()],
            dor: 0x0c,
            in_reset: false,
            rate: 0,
            phase: Phase::Command,
            fifo: Vec::new(),
            result_position: 0,
            execution: None,
            pending: false,
            reset_sense: 0,
            seek_status: None,
            srt: 0,
            timer1: 0,
            dma: true,
            config: 0x60,
            pretrk: 0,
            lock: false,
            perpendicular: 0,
        };
        floppy.irq.set_level(false);
        floppy
    }

    /// Insert `image` into `drive` (0 or 1). The image size selects the
    /// medium; a read-only image is write-protected.
    pub fn insert(&mut self, drive: usize, image: Box<dyn DiskImage>) -> Result<(), DeviceError> {
        let kind = MediaKind::for_size(image.size()).ok_or_else(|| {
            DeviceError::Other(format!(
                "a floppy image is 1474560 (1.44 MB) or 2949120 (2.88 MB) bytes, not {}",
                image.size()
            ))
        })?;
        let slot = self
            .drives
            .get_mut(drive)
            .ok_or_else(|| DeviceError::Other(format!("floppy drive {drive} does not exist")))?;
        slot.media = Some(Media {
            read_only: image.read_only(),
            image,
            kind,
        });
        slot.changed = true;
        Ok(())
    }

    /// The CMOS 0x10 byte for the drives: drive 0 in the high nibble. An
    /// empty drive 0 reports the 2.88 MB fallback; drive 1 is absent.
    pub fn cmos_drive_types(&self) -> u8 {
        self.drives[0]
            .media
            .as_ref()
            .map_or(EMPTY_CMOS_TYPE, |media| media.kind.cmos_type())
            << 4
    }

    fn update_irq(&self) {
        self.irq
            .set_level(self.pending && self.dor & 8 != 0 && !self.in_reset);
    }

    fn raise(&mut self) {
        self.pending = true;
        self.update_irq();
    }

    fn lower(&mut self) {
        self.pending = false;
        self.update_irq();
    }

    /// The controller's own reset: DOR bit 2 low, or DSR bit 7.
    fn reset_controller(&mut self) {
        if self.execution.take().is_some() {
            self.isa.lock().set_dreq(DMA_CHANNEL, false);
        }
        self.phase = Phase::Command;
        self.fifo.clear();
        self.result_position = 0;
        self.seek_status = None;
        self.reset_sense = 0;
        self.rate = 0;
        if !self.lock {
            self.config = 0x60;
            self.pretrk = 0;
        }
        for drive in &mut self.drives {
            drive.at.track = 0;
        }
        self.lower();
    }

    /// Leave reset with the polling interrupt: four SENSE INTERRUPT
    /// results, one per drive select.
    fn reset_interrupt(&mut self) {
        self.reset_sense = 4;
        self.raise();
    }

    fn msr(&mut self) -> u8 {
        if self.in_reset {
            // QEMU leaves reset without the polling interrupt when the
            // guest reads MSR before releasing DOR's reset bit.
            self.in_reset = false;
            return 0x80;
        }
        match self.phase {
            Phase::Command if self.fifo.is_empty() => 0x80,
            Phase::Command => 0x90,
            Phase::Result => 0xd0,
            Phase::Execution => match &self.execution {
                Some(execution) if !execution.pio.is_empty() => {
                    if execution.operation == Operation::Write {
                        0xb0
                    } else {
                        0xf0
                    }
                }
                _ => 0x10,
            },
        }
    }

    pub fn io_read(&mut self, offset: u16) -> u8 {
        match offset {
            0 => 0xff,
            1 => 0xc0,
            2 => self.dor,
            3 => 0,
            4 => self.msr(),
            5 => self.read_fifo(),
            7 => u8::from(self.drives[usize::from(self.dor & 1)].changed) << 7,
            _ => 0xff,
        }
    }

    pub fn io_write(&mut self, offset: u16, value: u8) {
        match offset {
            2 => {
                let previous = self.dor;
                self.dor = value;
                if value & 4 == 0 {
                    if !self.in_reset {
                        self.reset_controller();
                    }
                    self.in_reset = true;
                } else if self.in_reset {
                    self.in_reset = false;
                    self.reset_interrupt();
                } else if (previous ^ value) & 8 != 0 {
                    self.update_irq();
                }
            }
            4 => {
                self.rate = value & 3;
                if value & 0x80 != 0 {
                    self.reset_controller();
                    self.in_reset = false;
                    self.reset_interrupt();
                }
            }
            5 => self.write_fifo(value),
            7 => self.rate = value & 3,
            _ => {}
        }
    }

    fn read_fifo(&mut self) -> u8 {
        match self.phase {
            Phase::Result => {
                let value = self.fifo.get(self.result_position).copied().unwrap_or(0);
                self.result_position += 1;
                if self.result_position >= self.fifo.len() {
                    self.fifo.clear();
                    self.result_position = 0;
                    self.phase = Phase::Command;
                    self.lower();
                }
                value
            }
            Phase::Execution => {
                let Some(execution) = self.execution.as_mut() else {
                    return 0;
                };
                if execution.pio.is_empty() || execution.operation == Operation::Write {
                    return 0;
                }
                let value = execution.pio[execution.pio_position];
                execution.pio_position += 1;
                if execution.pio_position == execution.pio.len() {
                    self.pio_sector_done();
                }
                value
            }
            Phase::Command => 0,
        }
    }

    fn write_fifo(&mut self, value: u8) {
        match self.phase {
            Phase::Command => {
                if self.fifo.is_empty() && value == 0x08 {
                    // SENSE INTERRUPT acknowledges at its command byte.
                    self.lower();
                }
                self.fifo.push(value);
                if self.fifo.len() >= command_length(self.fifo[0]) {
                    let command = std::mem::take(&mut self.fifo);
                    self.execute(command);
                }
            }
            Phase::Execution => {
                let Some(execution) = self.execution.as_mut() else {
                    return;
                };
                if execution.pio.is_empty() || execution.operation != Operation::Write {
                    return;
                }
                execution.pio[execution.pio_position] = value;
                execution.pio_position += 1;
                if execution.pio_position == execution.pio.len() {
                    self.pio_sector_done();
                }
            }
            Phase::Result => {}
        }
    }

    fn result(&mut self, bytes: Vec<u8>) {
        self.fifo = bytes;
        self.result_position = 0;
        self.phase = Phase::Result;
    }

    fn idle(&mut self) {
        self.fifo.clear();
        self.phase = Phase::Command;
    }

    fn execute(&mut self, command: Vec<u8>) {
        let opcode = command[0];
        let drive = usize::from(command.get(1).copied().unwrap_or(0) & 1);
        match opcode {
            0x03 => {
                self.srt = command[1] >> 4;
                self.timer1 = command[2] >> 1;
                self.dma = command[2] & 1 == 0;
                self.idle();
            }
            0x04 => {
                let d = &self.drives[drive];
                let protected = d.media.as_ref().is_none_or(|media| media.read_only);
                let st3 = u8::from(protected) << 6
                    | 0x20
                    | u8::from(d.at.track == 0) << 4
                    | 0x08
                    | (command[1] & 7);
                self.result(vec![st3]);
            }
            0x07 => self.seek(drive, 0),
            0x08 => {
                let bytes = if self.reset_sense > 0 {
                    let index = 4 - self.reset_sense;
                    self.reset_sense -= 1;
                    let track = self
                        .drives
                        .get(usize::from(index))
                        .map_or(0, |d| d.at.track);
                    vec![0xc0 | index, track]
                } else if let Some((st0, track)) = self.seek_status.take() {
                    vec![st0, track]
                } else {
                    vec![0x80]
                };
                self.result(bytes);
            }
            0x0e => {
                let current = &self.drives[usize::from(self.dor & 1)];
                let eot = current
                    .media
                    .as_ref()
                    .map_or(0xff, |media| media.kind.sectors());
                self.result(vec![
                    self.drives[0].at.track,
                    self.drives[1].at.track,
                    0,
                    0,
                    self.srt,
                    self.timer1 << 1 | u8::from(self.dma),
                    eot,
                    u8::from(self.lock) << 7 | (self.perpendicular & 0x3f),
                    self.config,
                    self.pretrk,
                ]);
            }
            0x0f => self.seek(drive, command[2]),
            0x8f | 0xcf => {
                let track = self.drives[drive].at.track;
                let target = if opcode & 0x40 != 0 {
                    track.saturating_add(command[2])
                } else {
                    track.saturating_sub(command[2])
                };
                self.seek(drive, target);
            }
            0x10 => self.result(vec![0x90]),
            0x12 => {
                if command[1] & 0x80 != 0 {
                    self.perpendicular = command[1] & 0x3f;
                } else {
                    self.perpendicular = (self.perpendicular & 0x3c) | (command[1] & 3);
                }
                self.idle();
            }
            0x13 => {
                self.config = command[2];
                self.pretrk = command[3];
                self.idle();
            }
            0x14 | 0x94 => {
                self.lock = opcode & 0x80 != 0;
                self.result(vec![u8::from(self.lock) << 4]);
            }
            0x18 => self.result(vec![0x41]),
            _ if opcode & 0xbf == 0x0a => {
                self.start(Operation::ReadId, command);
            }
            _ if opcode & 0xbf == 0x0d => {
                self.start(Operation::Format, command);
            }
            _ => match opcode & 0x1f {
                0x02 | 0x06 | 0x0c => self.start(Operation::Read, command),
                0x05 | 0x09 => self.start(Operation::Write, command),
                0x16 => self.start(Operation::Verify, command),
                _ => self.result(vec![0x80]),
            },
        }
    }

    fn seek(&mut self, drive: usize, target: u8) {
        let target = target.min(CYLINDERS);
        let d = &mut self.drives[drive];
        if d.media.is_some() && d.at.track != target {
            d.changed = false;
        }
        d.at.track = target;
        self.seek_status = Some((0x20 | drive as u8, target));
        self.idle();
        self.raise();
    }

    fn start(&mut self, operation: Operation, command: Vec<u8>) {
        self.phase = Phase::Execution;
        self.execution = Some(Execution {
            operation,
            command,
            seeked: false,
            pio: Vec::new(),
            pio_position: 0,
            remaining_ns: if operation == Operation::ReadId {
                READ_ID_NS
            } else {
                0
            },
        });
        if operation == Operation::ReadId {
            return;
        }
        let execution = self.execution.as_ref().unwrap();
        let drive = usize::from(execution.command[1] & 1);
        let (c, h, r) = (
            execution.command[2],
            execution.command[3],
            execution.command[4],
        );
        let rate = self.rate;
        let d = &mut self.drives[drive];
        if operation != Operation::Format && d.media.as_ref().is_some_and(|m| m.kind.rate() == rate)
        {
            // The implied seek to the command's cylinder and sector.
            let track = c.min(CYLINDERS);
            let seeked = d.at.track != track;
            d.at = Position {
                track,
                head: h & 1,
                sect: r,
            };
            self.execution.as_mut().unwrap().seeked = seeked;
        }
        self.run();
    }

    /// Advance READ ID by `ns` of running guest time.
    pub fn advance(&mut self, ns: u64) {
        let Some(execution) = self.execution.as_mut() else {
            return;
        };
        if execution.operation != Operation::ReadId || self.phase != Phase::Execution {
            return;
        }
        execution.remaining_ns = execution.remaining_ns.saturating_sub(ns);
        if execution.remaining_ns == 0 {
            self.finish_read_id();
        }
    }

    /// DMA channel 2 may have become ready: continue a waiting command.
    pub fn dma_ready(&mut self) {
        if self.phase == Phase::Execution
            && self.dma
            && self.execution.as_ref().is_some_and(|execution| {
                execution.operation != Operation::ReadId && execution.pio.is_empty()
            })
        {
            self.run();
        }
    }

    fn finish_read_id(&mut self) {
        let execution = self.execution.take().unwrap();
        let drive = usize::from(execution.command[1] & 1);
        let head = (execution.command[1] >> 2) & 1;
        let d = &self.drives[drive];
        let at = d.at;
        let (st0, st1, sect) = match &d.media {
            Some(media) if media.kind.rate() == self.rate => (0, 0, at.sect + 1),
            Some(_) => (0x40, 0x01, at.sect + 1),
            None => (0x40, 0x01, at.sect),
        };
        self.result(vec![
            st0 | head << 2 | drive as u8,
            st1,
            0,
            at.track,
            head,
            sect,
            2,
        ]);
        self.raise();
    }

    fn finish(&mut self, st0: u8, st1: u8, at: Position, n: u8) {
        let execution = self.execution.take().unwrap();
        let drive = execution.command[1] & 1;
        self.isa.lock().set_dreq(DMA_CHANNEL, false);
        self.result(vec![
            st0 | at.head << 2 | drive,
            st1,
            0,
            at.track,
            at.head,
            at.sect,
            n,
        ]);
        self.raise();
    }

    /// Run the current data command as far as DMA and the medium allow.
    fn run(&mut self) {
        let execution = self.execution.as_ref().unwrap();
        let command = execution.command.clone();
        let operation = execution.operation;
        let drive = usize::from(command[1] & 1);
        if operation == Operation::Format {
            // QEMU's format leaves the medium untouched, moves no DMA data
            // and reports the drive's position (oracle "format track").
            let at = self.drives[drive].at;
            self.finish(0, 0, at, command[2]);
            return;
        }
        let (c, h, r, n, eot) = (command[2], command[3], command[4], command[5], command[6]);
        let multitrack = command[0] & 0x80 != 0;
        let media_rate = self.drives[drive].media.as_ref().map(|m| m.kind.rate());
        if media_rate.is_none_or(|rate| rate != self.rate) {
            self.finish(
                0x40,
                0x01,
                Position {
                    track: c,
                    head: h & 1,
                    sect: r,
                },
                n,
            );
            return;
        }
        if !self.dma {
            self.start_pio();
            return;
        }
        loop {
            let at = self.drives[drive].at;
            let sectors = self.drives[drive].media.as_ref().unwrap().kind.sectors();
            let seeked = self.execution.as_ref().unwrap().seeked;
            if at.sect == 0 || at.sect > sectors || at.track >= CYLINDERS {
                self.finish(0x40 | se(seeked), 0x80, at, n);
                return;
            }
            let lba = lba(at, sectors);
            let mut buffer = [0u8; SECTOR];
            let mut isa = self.isa.lock();
            if !isa.dma_ready(DMA_CHANNEL) {
                isa.set_dreq(DMA_CHANNEL, true);
                return;
            }
            let media = self.drives[drive].media.as_mut().unwrap();
            if matches!(operation, Operation::Read | Operation::Verify)
                && media.image.read_at(lba, &mut buffer).is_err()
            {
                drop(isa);
                self.finish(0x40 | se(seeked), 0x20, at, n);
                return;
            }
            let progress = isa.dma_transfer(DMA_CHANNEL, &mut buffer);
            drop(isa);
            if operation == Operation::Write {
                let media = self.drives[drive].media.as_mut().unwrap();
                if media.read_only {
                    self.finish(0x60, 0x02, at, n);
                    return;
                }
                if media.image.write_at(lba, &buffer).is_err() {
                    self.finish(0x40 | se(seeked), 0x20, at, n);
                    return;
                }
            }
            let end = self.step(drive, eot, multitrack);
            let seeked = self.execution.as_ref().unwrap().seeked;
            if progress.terminal_count || end {
                let at = self.drives[drive].at;
                self.finish(se(seeked), 0, at, n);
                return;
            }
        }
    }

    /// Move to the next sector; true at the end of the cylinder.
    fn step(&mut self, drive: usize, eot: u8, multitrack: bool) -> bool {
        let at = &mut self.drives[drive].at;
        if at.sect < eot {
            at.sect += 1;
            return false;
        }
        at.sect = 1;
        if multitrack && at.head == 0 {
            at.head = 1;
            return false;
        }
        at.head = 0;
        at.track = at.track.saturating_add(1);
        self.execution.as_mut().unwrap().seeked = true;
        true
    }

    fn start_pio(&mut self) {
        let command = self.execution.as_ref().unwrap().command.clone();
        let drive = usize::from(command[1] & 1);
        let at = self.drives[drive].at;
        let sectors = self.drives[drive].media.as_ref().unwrap().kind.sectors();
        let seeked = self.execution.as_ref().unwrap().seeked;
        if at.sect == 0 || at.sect > sectors || at.track >= CYLINDERS {
            self.finish(0x40 | se(seeked), 0x80, at, command[5]);
            return;
        }
        let mut buffer = vec![0u8; SECTOR];
        let operation = self.execution.as_ref().unwrap().operation;
        if operation != Operation::Write {
            let media = self.drives[drive].media.as_mut().unwrap();
            if media.image.read_at(lba(at, sectors), &mut buffer).is_err() {
                self.finish(0x40 | se(seeked), 0x20, at, command[5]);
                return;
            }
        }
        let execution = self.execution.as_mut().unwrap();
        execution.pio = buffer;
        execution.pio_position = 0;
        self.raise();
    }

    fn pio_sector_done(&mut self) {
        let execution = self.execution.as_ref().unwrap();
        let command = execution.command.clone();
        let operation = execution.operation;
        let drive = usize::from(command[1] & 1);
        let at = self.drives[drive].at;
        let sectors = self.drives[drive].media.as_ref().unwrap().kind.sectors();
        let seeked = execution.seeked;
        if operation == Operation::Write {
            let data = execution.pio.clone();
            let media = self.drives[drive].media.as_mut().unwrap();
            if media.read_only {
                self.finish(0x60, 0x02, at, command[5]);
                return;
            }
            if media.image.write_at(lba(at, sectors), &data).is_err() {
                self.finish(0x40 | se(seeked), 0x20, at, command[5]);
                return;
            }
        }
        if at.sect >= command[6] && !(command[0] & 0x80 != 0 && at.head == 0) {
            // QEMU ends a non-DMA transfer at EOT without stepping past it.
            self.finish(se(seeked), 0, at, command[5]);
            return;
        }
        self.step(drive, command[6], command[0] & 0x80 != 0);
        self.execution.as_mut().unwrap().pio.clear();
        self.start_pio();
    }
}

fn se(seeked: bool) -> u8 {
    if seeked { 0x20 } else { 0 }
}

fn lba(at: Position, sectors: u8) -> u64 {
    ((u64::from(at.track) * 2 + u64::from(at.head)) * u64::from(sectors) + u64::from(at.sect) - 1)
        * SECTOR as u64
}

/// Bytes in the command phase, opcode included.
fn command_length(opcode: u8) -> usize {
    match opcode {
        0x03 | 0x0f | 0x8f | 0xcf => 3,
        0x04 | 0x07 | 0x12 => 2,
        0x13 => 4,
        0x08 | 0x0e | 0x10 | 0x14 | 0x94 | 0x18 => 1,
        _ if opcode & 0xbf == 0x0a => 2,
        _ if opcode & 0xbf == 0x0d => 6,
        _ => match opcode & 0x1f {
            0x02 | 0x05 | 0x06 | 0x09 | 0x0c | 0x16 => 9,
            _ => 1,
        },
    }
}

/// Port window of a shared controller: 0x3f0-0x3f5 at offset 0, or 0x3f7
/// registered on its own with `base` 7.
pub struct FloppyPorts {
    controller: Arc<Mutex<Floppy>>,
    base: u16,
}

impl FloppyPorts {
    pub fn new(controller: Arc<Mutex<Floppy>>, base: u16) -> Self {
        Self { controller, base }
    }
}

impl BusDevice for FloppyPorts {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let mut controller = self.controller.lock();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = controller.io_read(self.base + offset as u16 + i as u16);
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut controller = self.controller.lock();
        for (i, byte) in data.iter().enumerate() {
            controller.io_write(self.base + offset as u16 + i as u16, *byte);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SavedDrive {
    media: Option<(MediaKind, bool)>,
    at: Position,
    changed: bool,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    drives: Vec<SavedDrive>,
    dor: u8,
    in_reset: bool,
    rate: u8,
    phase: Phase,
    fifo: Vec<u8>,
    result_position: usize,
    execution: Option<Execution>,
    pending: bool,
    reset_sense: u8,
    seek_status: Option<(u8, u8)>,
    srt: u8,
    timer1: u8,
    dma: bool,
    config: u8,
    pretrk: u8,
    lock: bool,
    perpendicular: u8,
}

impl Snapshot for Floppy {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        serde_json::to_vec(&Saved {
            version: 1,
            drives: self
                .drives
                .iter()
                .map(|drive| SavedDrive {
                    media: drive
                        .media
                        .as_ref()
                        .map(|media| (media.kind, media.read_only)),
                    at: drive.at,
                    changed: drive.changed,
                })
                .collect(),
            dor: self.dor,
            in_reset: self.in_reset,
            rate: self.rate,
            phase: self.phase,
            fifo: self.fifo.clone(),
            result_position: self.result_position,
            execution: self.execution.clone(),
            pending: self.pending,
            reset_sense: self.reset_sense,
            seek_status: self.seek_status,
            srt: self.srt,
            timer1: self.timer1,
            dma: self.dma,
            config: self.config,
            pretrk: self.pretrk,
            lock: self.lock,
            perpendicular: self.perpendicular,
        })
        .map_err(|e| DeviceError::State(e.to_string()))
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let invalid =
            || DeviceError::State("floppy snapshot version, media or phase mismatch".into());
        let saved: Saved = serde_json::from_slice(state).map_err(|_| invalid())?;
        if saved.version != 1
            || saved.drives.len() != 2
            || saved.reset_sense > 4
            || saved.rate > 3
            || saved.fifo.len() > 16
            || saved.result_position > saved.fifo.len()
            || saved
                .drives
                .iter()
                .any(|drive| drive.at.track > CYLINDERS || drive.at.head > 1)
        {
            return Err(invalid());
        }
        for (drive, state) in self.drives.iter().zip(&saved.drives) {
            if drive
                .media
                .as_ref()
                .map(|media| (media.kind, media.read_only))
                != state.media
            {
                return Err(invalid());
            }
        }
        if let Some(execution) = &saved.execution {
            let valid = execution.command.len() >= 2
                && execution.command.len() == command_length(execution.command[0])
                && (execution.pio.is_empty() || execution.pio.len() == SECTOR)
                && (if execution.pio.is_empty() {
                    execution.pio_position == 0
                } else {
                    execution.pio_position < execution.pio.len()
                })
                && execution.remaining_ns <= READ_ID_NS
                && saved.phase == Phase::Execution;
            if !valid {
                return Err(invalid());
            }
        } else if saved.phase == Phase::Execution {
            return Err(invalid());
        }
        for (drive, state) in self.drives.iter_mut().zip(saved.drives) {
            drive.at = state.at;
            drive.changed = state.changed;
        }
        self.dor = saved.dor;
        self.in_reset = saved.in_reset;
        self.rate = saved.rate;
        self.phase = saved.phase;
        self.fifo = saved.fifo;
        self.result_position = saved.result_position;
        self.execution = saved.execution;
        self.pending = saved.pending;
        self.reset_sense = saved.reset_sense;
        self.seek_status = saved.seek_status;
        self.srt = saved.srt;
        self.timer1 = saved.timer1;
        self.dma = saved.dma;
        self.config = saved.config;
        self.pretrk = saved.pretrk;
        self.lock = saved.lock;
        self.perpendicular = saved.perpendicular;
        self.update_irq();
        Ok(())
    }
}

impl Resettable for Floppy {
    /// Power-on state; the media stay in their drives.
    fn reset(&mut self) -> Result<(), DeviceError> {
        if self.execution.take().is_some() {
            self.isa.lock().set_dreq(DMA_CHANNEL, false);
        }
        self.dor = 0x0c;
        self.in_reset = false;
        self.rate = 0;
        self.phase = Phase::Command;
        self.fifo.clear();
        self.result_position = 0;
        self.pending = false;
        self.reset_sense = 0;
        self.seek_status = None;
        self.srt = 0;
        self.timer1 = 0;
        self.dma = true;
        self.config = 0x60;
        self.pretrk = 0;
        self.lock = false;
        self.perpendicular = 0;
        for drive in &mut self.drives {
            drive.at = Position {
                track: 0,
                head: 0,
                sect: 1,
            };
            drive.changed = true;
        }
        self.update_irq();
        Ok(())
    }
}
