//! A flash bank of the aarch64 `virt` machine: two 16-bit NOR chips with
//! the Intel command set, side by side on a 32-bit bus (phase 3d, F4).
//!
//! The firmware variable store of AAVMF lives in one. Its driver writes each
//! command to both chips at once (`0x00ff00ff` for read array), programs a
//! word or a buffer of words, and erases a block of 256 KiB. A read gives
//! the array, or in every other mode the status of each chip in its low
//! byte.
//!
//! Sources read (phase 3d DESIGN.md, "Sources"): S7, the Common Flash
//! Interface specification (JESD68) and the Intel StrataFlash command set
//! as its data sheet gives it, and S6, what the firmware reads from a
//! running QEMU. No QEMU source was read.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::{BusDevice, DeviceError, Resettable};

/// The size of one bank of `virt`.
pub const BANK_SIZE: usize = 64 << 20;
/// The erase block of a bank.
pub const BLOCK_SIZE: usize = 256 << 10;
/// The bytes of one bus access: two chips of 16 bits.
const BANK_WIDTH: usize = 4;
const DEVICE_WIDTH: usize = 2;
/// The most bus writes of one buffered program.
const BUFFER_WRITES: usize = 512;

const READY: u8 = 0x80;
const ERASE_ERROR: u8 = 0x20;
const PROGRAM_ERROR: u8 = 0x10;
const PROTECTED: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Array,
    Status,
    Id,
    Query,
    /// The next write is the word to program.
    Program,
    /// The next write confirms the erase of the block.
    Erase { block: usize },
    /// The next write is the count of a buffered program, less one.
    BufferCount { block: usize },
    /// `left` more bus writes go into the buffer.
    BufferData { block: usize, left: usize },
    /// The next write confirms the buffered program.
    BufferConfirm,
    /// The next write is a lock, an unlock or a lock-down of the block.
    Lock,
}

pub struct CfiFlash {
    bytes: Vec<u8>,
    /// Where a program and an erase land. `None`: the bank is read-only.
    file: Option<File>,
    mode: Mode,
    status: u8,
    /// The writes of a buffered program, before its confirm.
    buffer: Vec<(usize, Vec<u8>)>,
    dirty: bool,
    last_sync: Instant,
}

impl CfiFlash {
    /// A bank that holds the file `path` from its start, and erased bytes
    /// after it. A writable bank keeps the file in step with every program
    /// and erase inside the length of the file.
    pub fn open(path: &Path, writable: bool) -> Result<Self, DeviceError> {
        let file = OpenOptions::new().read(true).write(writable).open(path)?;
        let len = file.metadata()?.len();
        if len == 0 || len > BANK_SIZE as u64 {
            return Err(DeviceError::Other(format!(
                "a flash bank of virt takes an image of 1 byte to 64 MiB, and {} has {len} bytes",
                path.display()
            )));
        }
        let mut bytes = vec![0xff; BANK_SIZE];
        file.read_exact_at(&mut bytes[..len as usize], 0)?;
        Ok(Self::with(bytes, writable.then_some(file)))
    }

    /// A bank of `bytes` with no file: what it is given is lost with it.
    pub fn in_memory(mut bytes: Vec<u8>) -> Self {
        bytes.resize(BANK_SIZE, 0xff);
        Self::with(bytes, None)
    }

    fn with(bytes: Vec<u8>, file: Option<File>) -> Self {
        Self {
            bytes,
            file,
            mode: Mode::Array,
            status: READY,
            buffer: Vec::new(),
            dirty: false,
            last_sync: Instant::now(),
        }
    }

    /// The array, for a machine that maps a read-only bank as memory.
    pub fn contents(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether a read gives the array now. A machine may then read it
    /// without the device.
    pub fn reads_array(&self) -> bool {
        self.mode == Mode::Array
    }

    fn block_of(offset: usize) -> usize {
        offset / BLOCK_SIZE * BLOCK_SIZE
    }

    /// Put `data` at `start` in the array and in the file.
    fn persist(&mut self, start: usize, data: &[u8], error: u8) {
        self.bytes[start..start + data.len()].copy_from_slice(data);
        let Some(file) = &self.file else {
            return;
        };
        // The part past the end of the file is in the array alone.
        let len = match file.metadata() {
            Ok(meta) => meta.len() as usize,
            Err(e) => {
                log::warn!("flash image: {e}");
                self.status |= error;
                return;
            }
        };
        if start >= len {
            return;
        }
        let end = (start + data.len()).min(len);
        if let Err(e) = file.write_all_at(&data[..end - start], start as u64) {
            log::warn!("flash image: {e}");
            self.status |= error;
            return;
        }
        self.dirty = true;
    }

    /// Program: a NOR cell goes from 1 to 0 only.
    fn program(&mut self, start: usize, data: &[u8]) {
        let new: Vec<u8> = self.bytes[start..start + data.len()]
            .iter()
            .zip(data)
            .map(|(old, new)| old & new)
            .collect();
        self.persist(start, &new, PROGRAM_ERROR);
    }

    fn writable(&mut self, error: u8) -> bool {
        if self.file.is_none() {
            self.status |= error | PROTECTED;
            return false;
        }
        true
    }

    fn command(&mut self, offset: usize, command: u8) {
        self.mode = match command {
            0xff | 0xf0 => Mode::Array,
            0x70 => Mode::Status,
            0x50 => {
                self.status = READY;
                Mode::Array
            }
            0x10 | 0x40 => Mode::Program,
            0x20 | 0x28 => Mode::Erase {
                block: Self::block_of(offset),
            },
            0xe8 => {
                self.status |= READY;
                self.buffer.clear();
                Mode::BufferCount {
                    block: Self::block_of(offset),
                }
            }
            0x60 => Mode::Lock,
            0x90 => Mode::Id,
            0x98 => Mode::Query,
            // A confirm that no command waits for.
            0xd0 => Mode::Array,
            _ => {
                // A command that the chips do not have.
                self.status |= PROGRAM_ERROR | ERASE_ERROR;
                Mode::Status
            }
        };
    }

    /// The device writes the files of what it programmed since the last
    /// call, when a batch interval has passed. A caller that polls bounds
    /// the window in which a power loss of the host loses a variable.
    pub fn sync_due(&mut self) -> Result<(), DeviceError> {
        if self.dirty && self.last_sync.elapsed() >= Duration::from_millis(16) {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), DeviceError> {
        if self.dirty
            && let Some(file) = &self.file
        {
            if let Err(error) = file.sync_data() {
                self.status |= PROGRAM_ERROR;
                return Err(error.into());
            }
            self.dirty = false;
            self.last_sync = Instant::now();
        }
        Ok(())
    }

    /// The CFI query table of one chip, by its index.
    fn query(index: usize) -> u8 {
        // The size of one chip is half of the bank.
        let chip_bits = (BANK_SIZE / (BANK_WIDTH / DEVICE_WIDTH)).trailing_zeros() as u8;
        let chip_block = BLOCK_SIZE / (BANK_WIDTH / DEVICE_WIDTH);
        let blocks = BANK_SIZE / BLOCK_SIZE;
        match index {
            0x10 => b'Q',
            0x11 => b'R',
            0x12 => b'Y',
            // The Intel command set, and its extended table at 0x31.
            0x13 => 0x01,
            0x15 => 0x31,
            // Vcc 4.5 V to 5.5 V.
            0x1b => 0x45,
            0x1c => 0x55,
            // Typical and maximal times, as powers of two.
            0x1f => 0x07,
            0x20 => 0x07,
            0x21 => 0x0a,
            0x23 => 0x04,
            0x24 => 0x04,
            0x25 => 0x04,
            0x27 => chip_bits,
            // A 16-bit interface.
            0x28 => 0x01,
            // The buffer of a buffered program, as a power of two of bytes.
            0x2a => (BUFFER_WRITES * DEVICE_WIDTH).trailing_zeros() as u8,
            // One erase region.
            0x2c => 0x01,
            0x2d => (blocks - 1) as u8,
            0x2e => ((blocks - 1) >> 8) as u8,
            0x2f => (chip_block >> 8) as u8,
            0x30 => (chip_block >> 16) as u8,
            0x31 => b'P',
            0x32 => b'R',
            0x33 => b'I',
            0x34 => b'1',
            0x35 => b'0',
            _ => 0,
        }
    }

    fn read_byte(&self, address: usize) -> u8 {
        if address >= BANK_SIZE {
            return 0xff;
        }
        // Each chip answers in the low byte of its 16 bits.
        let low = address.is_multiple_of(DEVICE_WIDTH);
        let index = address / BANK_WIDTH;
        match self.mode {
            Mode::Array => self.bytes[address],
            Mode::Id => match (low, index & 0xff) {
                (true, 0) => 0x89,
                (true, 1) => 0x18,
                _ => 0,
            },
            Mode::Query if low => Self::query(index & 0xff),
            Mode::Query => 0,
            _ if low => self.status,
            _ => 0,
        }
    }
}

impl BusDevice for CfiFlash {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = match usize::try_from(offset)
                .ok()
                .and_then(|o| o.checked_add(index))
            {
                Some(address) => self.read_byte(address),
                None => 0xff,
            };
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let Ok(start) = usize::try_from(offset) else {
            return;
        };
        let Some(end) = start.checked_add(data.len()) else {
            return;
        };
        if data.is_empty() || end > BANK_SIZE || data.len() > BANK_WIDTH {
            return;
        }
        match self.mode {
            Mode::Program => {
                self.mode = Mode::Status;
                self.status |= READY;
                if self.writable(PROGRAM_ERROR) {
                    self.program(start, data);
                }
            }
            Mode::Erase { block } => {
                self.mode = Mode::Status;
                self.status |= READY;
                if data[0] != 0xd0 {
                    self.status |= ERASE_ERROR | PROGRAM_ERROR;
                } else if self.writable(ERASE_ERROR) {
                    self.persist(block, &vec![0xff; BLOCK_SIZE], ERASE_ERROR);
                }
            }
            Mode::BufferCount { block } => {
                let count = usize::from(data[0])
                    | data.get(1).map_or(0, |high| usize::from(*high) << 8);
                if count >= BUFFER_WRITES {
                    self.status |= PROGRAM_ERROR;
                    self.mode = Mode::Status;
                } else {
                    self.mode = Mode::BufferData {
                        block,
                        left: count + 1,
                    };
                }
            }
            Mode::BufferData { block, left } => {
                if Self::block_of(start) != block {
                    // A buffer stays inside the block that it was set up in.
                    self.status |= PROGRAM_ERROR;
                    self.buffer.clear();
                    self.mode = Mode::Status;
                    return;
                }
                self.buffer.push((start, data.to_vec()));
                self.mode = if left == 1 {
                    Mode::BufferConfirm
                } else {
                    Mode::BufferData {
                        block,
                        left: left - 1,
                    }
                };
            }
            Mode::BufferConfirm => {
                self.mode = Mode::Status;
                self.status |= READY;
                let buffer = std::mem::take(&mut self.buffer);
                if data[0] != 0xd0 {
                    self.status |= PROGRAM_ERROR | ERASE_ERROR;
                } else if self.writable(PROGRAM_ERROR) {
                    for (start, data) in buffer {
                        self.program(start, &data);
                    }
                }
            }
            Mode::Lock => {
                // The blocks of this bank have no lock: each of the three
                // answers is taken and changes nothing.
                self.mode = Mode::Status;
                self.status |= READY;
                if !matches!(data[0], 0xd0 | 0x01 | 0x2f) {
                    self.status |= PROGRAM_ERROR | ERASE_ERROR;
                }
            }
            Mode::Array | Mode::Status | Mode::Id | Mode::Query => self.command(start, data[0]),
        }
    }
}

impl Resettable for CfiFlash {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.flush()?;
        self.mode = Mode::Array;
        self.status = READY;
        self.buffer.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read32(flash: &mut CfiFlash, offset: u64) -> u32 {
        let mut data = [0; 4];
        flash.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(flash: &mut CfiFlash, offset: u64, value: u32) {
        flash.write(offset, &value.to_le_bytes());
    }

    fn bank() -> (tempfile::TempDir, CfiFlash) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vars");
        std::fs::write(&path, vec![0xff; 2 * BLOCK_SIZE]).unwrap();
        let flash = CfiFlash::open(&path, true).unwrap();
        (dir, flash)
    }

    #[test]
    fn a_word_program_lands_in_the_array_and_in_the_file() {
        let (dir, mut flash) = bank();
        write32(&mut flash, 0x100, 0x0040_0040);
        write32(&mut flash, 0x100, 0x1234_5678);
        // Both chips say ready, each in its low byte.
        assert_eq!(read32(&mut flash, 0x100), 0x0080_0080);
        write32(&mut flash, 0, 0x00ff_00ff);
        assert_eq!(read32(&mut flash, 0x100), 0x1234_5678);
        flash.flush().unwrap();
        let file = std::fs::read(dir.path().join("vars")).unwrap();
        assert_eq!(file[0x100..0x104], 0x1234_5678u32.to_le_bytes());
        assert_eq!(file.len(), 2 * BLOCK_SIZE);
    }

    #[test]
    fn a_program_clears_bits_and_an_erase_sets_the_block() {
        let (_dir, mut flash) = bank();
        for value in [0xffff_00ffu32, 0x0f0f_ffff] {
            write32(&mut flash, 0x40, 0x0040_0040);
            write32(&mut flash, 0x40, value);
        }
        write32(&mut flash, 0, 0x00ff_00ff);
        assert_eq!(read32(&mut flash, 0x40), 0x0f0f_00ff);
        write32(&mut flash, BLOCK_SIZE as u64 + 8, 0x0040_0040);
        write32(&mut flash, BLOCK_SIZE as u64 + 8, 0);
        write32(&mut flash, 0x1000, 0x0020_0020);
        write32(&mut flash, 0x1000, 0x00d0_00d0);
        assert_eq!(read32(&mut flash, 0) & 0x00ff_00ff, 0x0080_0080);
        write32(&mut flash, 0, 0x00ff_00ff);
        assert_eq!(read32(&mut flash, 0x40), 0xffff_ffff);
        // The other block keeps what it holds.
        assert_eq!(read32(&mut flash, BLOCK_SIZE as u64 + 8), 0);
    }

    #[test]
    fn a_buffered_program_waits_for_its_confirm() {
        let (_dir, mut flash) = bank();
        write32(&mut flash, 0x200, 0x00e8_00e8);
        assert_eq!(read32(&mut flash, 0x200), 0x0080_0080);
        // Three words: the count is one less.
        write32(&mut flash, 0x200, 2);
        for (index, value) in [0x11u32, 0x22, 0x33].into_iter().enumerate() {
            write32(&mut flash, 0x200 + 4 * index as u64, value);
        }
        assert_eq!(flash.contents()[0x200], 0xff);
        write32(&mut flash, 0x200, 0x00d0_00d0);
        assert_eq!(read32(&mut flash, 0x200), 0x0080_0080);
        write32(&mut flash, 0, 0x00ff_00ff);
        assert_eq!(read32(&mut flash, 0x200), 0x11);
        assert_eq!(read32(&mut flash, 0x204), 0x22);
        assert_eq!(read32(&mut flash, 0x208), 0x33);
    }

    #[test]
    fn a_read_only_bank_refuses_a_program_in_its_status() {
        let mut flash = CfiFlash::in_memory(vec![0xaa; 16]);
        write32(&mut flash, 0, 0x0040_0040);
        write32(&mut flash, 0, 0);
        assert_eq!(
            read32(&mut flash, 0) as u8,
            READY | PROGRAM_ERROR | PROTECTED
        );
        // Clear status puts the chips back to the array.
        write32(&mut flash, 0, 0x0050_0050);
        assert_eq!(read32(&mut flash, 0), 0xaaaa_aaaa);
        assert_eq!(read32(&mut flash, 16), 0xffff_ffff);
    }

    #[test]
    fn the_query_table_names_both_chips() {
        let mut flash = CfiFlash::in_memory(Vec::new());
        write32(&mut flash, 0x55 * 4, 0x0098_0098);
        assert_eq!(read32(&mut flash, 0x10 * 4), 0x0051_0051);
        assert_eq!(read32(&mut flash, 0x11 * 4), 0x0052_0052);
        assert_eq!(read32(&mut flash, 0x12 * 4), 0x0059_0059);
        // Each chip is 32 MiB.
        assert_eq!(read32(&mut flash, 0x27 * 4), 0x0019_0019);
        write32(&mut flash, 0, 0x00f0_00f0);
        write32(&mut flash, 0, 0x0090_0090);
        assert_eq!(read32(&mut flash, 0), 0x0089_0089);
        assert_eq!(read32(&mut flash, 4), 0x0018_0018);
    }

    #[test]
    fn a_reset_ends_a_pending_command() {
        let (_dir, mut flash) = bank();
        write32(&mut flash, 0, 0x0020_0020);
        flash.reset().unwrap();
        write32(&mut flash, 0, 0x00d0_00d0);
        assert!(flash.reads_array());
        assert_eq!(read32(&mut flash, 0), 0xffff_ffff);
    }
}
