//! The code region (DESIGN.md H13b to H13d, Q3): one `memfd` mapped twice,
//! a view that is readable and writable where the emitter writes, and a
//! view that is readable and executable where the CPU runs. No page is
//! writable and executable in one mapping.
//!
//! The region is a bump allocator over `PARTS` parts, one after another.
//! When the part that it writes into is full, it goes on to the next one,
//! the part written longest ago, and drops the code in it: the generation
//! of that part moves on, and a block whose code is of an older generation
//! has none (H13c). The code of the other parts stays (card J11).
//!
//! Sources read: the Linux man pages of `memfd_create(2)`, `mmap(2)` and
//! `ftruncate(2)`.

use std::io;

/// `MFD_EXEC` of Linux 6.3: the file may be mapped executable also where
/// `vm.memfd_noexec` would refuse it. An older kernel rejects the flag, and
/// the region asks again without it.
const MFD_EXEC: libc::c_uint = 0x10;

/// The generations of every region of the process. A region that a CPU
/// makes after it dropped another can be mapped at the same address, so a
/// generation is never used twice: an entry of the dropped region is then
/// of no region.
static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn new_generation() -> u64 {
    GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The parts of a region (3e DESIGN.md H13c, card J11). Code is written
/// into one part after another, and a full region drops only the part that
/// it writes into next, the one written longest ago.
pub const PARTS: usize = 4;

pub struct Region {
    fd: libc::c_int,
    write: *mut u8,
    exec: *mut u8,
    len: usize,
    /// The first byte after the code that lives for the whole region.
    floor: usize,
    /// The first byte of each part, and the first after the code in it.
    /// The last part ends at `len`.
    starts: [usize; PARTS],
    ends: [usize; PARTS],
    /// The generation of the code in each part.
    generations: [u64; PARTS],
    /// The part that code is written into.
    current: usize,
}

impl Region {
    /// A region of `len` bytes, rounded up to whole pages.
    pub fn new(len: usize) -> io::Result<Self> {
        let page = 4096;
        let len = len.div_ceil(page).max(1) * page;
        let name = c"vmlab-hv-jit";
        // SAFETY: the name is a valid C string, and the call has no other
        // input.
        let mut fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | MFD_EXEC) };
        if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
            // SAFETY: as above.
            fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        }
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let close = |error: io::Error| {
            // SAFETY: `fd` is the descriptor made above, and nothing else
            // holds it.
            unsafe { libc::close(fd) };
            Err(error)
        };
        // SAFETY: `fd` is a memfd that this function owns.
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            return close(io::Error::last_os_error());
        }
        let map = |protection| {
            // SAFETY: a new shared mapping of the whole file, at an address
            // that the kernel picks. It aliases no Rust object.
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    protection,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            if address == libc::MAP_FAILED {
                Err(io::Error::last_os_error())
            } else {
                Ok(address.cast::<u8>())
            }
        };
        let write = match map(libc::PROT_READ | libc::PROT_WRITE) {
            Ok(address) => address,
            Err(error) => return close(error),
        };
        let exec = match map(libc::PROT_READ | libc::PROT_EXEC) {
            Ok(address) => address,
            Err(error) => {
                // SAFETY: the mapping made just above, used by nothing.
                unsafe { libc::munmap(write.cast(), len) };
                return close(error);
            }
        };
        let mut region = Self {
            fd,
            write,
            exec,
            len,
            floor: 0,
            starts: [0; PARTS],
            ends: [0; PARTS],
            generations: [0; PARTS],
            current: 0,
        };
        region.split();
        Ok(region)
    }

    /// Cut the code after the floor into its parts, each empty and of a
    /// new generation. Before `seal_floor` the first part is the whole
    /// region.
    fn split(&mut self) {
        let part = if self.floor == 0 {
            0
        } else {
            ((self.len - self.floor) / PARTS) & !63
        };
        for index in 0..PARTS {
            self.starts[index] = if part == 0 {
                if index == 0 { 0 } else { self.len }
            } else {
                self.floor + index * part
            };
            self.ends[index] = self.starts[index];
            self.generations[index] = new_generation();
        }
        self.current = 0;
    }

    /// The first byte after the part `index`.
    fn part_end(&self, index: usize) -> usize {
        if index + 1 < PARTS {
            self.starts[index + 1]
        } else {
            self.len
        }
    }

    /// The part that holds the byte at `offset` of the region, after the
    /// floor.
    fn part_of(&self, offset: usize) -> Option<usize> {
        if offset < self.floor || offset >= self.len {
            return None;
        }
        (0..PARTS).rev().find(|index| self.starts[*index] <= offset)
    }

    /// The generation of the code written now: that of the part it goes
    /// into.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generations[self.current]
    }

    /// The generation of the code at `address`, a byte of the executable
    /// view after the floor. `None`: another address.
    #[inline]
    pub fn generation_of(&self, address: u64) -> Option<u64> {
        let offset = usize::try_from(address.checked_sub(self.exec as u64)?).ok()?;
        self.part_of(offset).map(|index| self.generations[index])
    }

    /// The bytes in use.
    pub fn used(&self) -> usize {
        self.floor
            + (0..PARTS)
                .map(|index| self.ends[index] - self.starts[index].max(self.floor))
                .sum::<usize>()
    }

    /// The address at which the next code of `len` bytes runs, when it fits
    /// into the current part.
    pub fn next(&self, len: usize) -> Option<u64> {
        let start = self.ends[self.current].next_multiple_of(16);
        (start + len <= self.part_end(self.current)).then(|| self.exec as u64 + start as u64)
    }

    /// Drop every block of the region: the code after the floor is of an
    /// older generation from now on.
    pub fn reset(&mut self) {
        self.split();
    }

    /// Go on to the next part, and drop the code in it (H13c, card J11):
    /// the executable addresses that it covers, from the first to the one
    /// after the last, when it held code. Every other part keeps its code.
    pub fn advance(&mut self) -> Option<(u64, u64)> {
        self.current = (self.current + 1) % PARTS;
        let index = self.current;
        let held = self.ends[index] > self.starts[index];
        self.ends[index] = self.starts[index];
        self.generations[index] = new_generation();
        let base = self.exec as u64;
        held.then(|| {
            (
                base + self.starts[index] as u64,
                base + self.part_end(index) as u64,
            )
        })
    }

    /// Copy `code` to `address`, which `next` gave for its length, and take
    /// its bytes.
    ///
    /// # Panics
    /// When `address` is not what `next` gives for that length.
    pub fn put(&mut self, address: u64, code: &[u8]) {
        assert_eq!(
            self.next(code.len()),
            Some(address),
            "the place of the code"
        );
        let offset = (address - self.exec as u64) as usize;
        // SAFETY: `next` checked that the bytes are inside the writable
        // view, and no executable code of the current generation is there:
        // the bump pointer only moves forward inside a generation of a
        // part.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), self.write.add(offset), code.len());
        }
        self.ends[self.current] = offset + code.len();
    }

    /// Write `bytes` over code of the region at `address`: a jump that is
    /// linked or unlinked (DESIGN.md H5). Only the thread that runs the
    /// code of the region writes it, and never while that code runs.
    ///
    /// # Panics
    /// When the bytes are not inside the code in use of one part.
    pub fn patch(&mut self, address: u64, bytes: &[u8]) {
        let start = self.exec as u64;
        let offset = address.wrapping_sub(start) as usize;
        let inside = self.part_of(offset).is_some_and(|index| {
            offset + bytes.len() <= self.ends[index] && offset >= self.starts[index]
        });
        assert!(inside, "a patch outside the code in use");
        // SAFETY: the bytes are inside the writable view (checked above),
        // and no code of the region runs while the owner patches it.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.write.add(offset), bytes.len());
        }
    }

    /// Keep the code so far for the whole life of the region: the
    /// trampoline. The rest is cut into its parts.
    pub fn seal_floor(&mut self) {
        self.floor = self.ends[self.current].next_multiple_of(64);
        self.split();
    }

    /// Whether `address` is inside the executable view.
    #[inline]
    pub fn contains(&self, address: u64) -> bool {
        let start = self.exec as u64;
        (start..start + self.len as u64).contains(&address)
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the two mappings and the descriptor of `new`. Nothing runs
        // in the region once it is dropped: the `Jit` that owns it is gone.
        unsafe {
            libc::munmap(self.write.cast(), self.len);
            libc::munmap(self.exec.cast(), self.len);
            libc::close(self.fd);
        }
    }
}

// SAFETY: the region is plain memory that one owner writes. It moves with
// the CPU that owns it to the thread that runs that CPU.
unsafe impl Send for Region {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_views_alias_and_a_reset_moves_the_generation() {
        let mut region = Region::new(8192).expect("a memfd region");
        let at = region.next(4).expect("room");
        region.put(at, &[1, 2, 3, 4]);
        // SAFETY: `at` is inside the readable executable view.
        let read = unsafe { std::slice::from_raw_parts(at as *const u8, 4) };
        assert_eq!(read, [1, 2, 3, 4]);
        region.seal_floor();
        let generation = region.generation();
        assert!(region.next(8192).is_none());
        let next = region.next(16).expect("room");
        region.put(next, &[0; 16]);
        region.reset();
        assert_ne!(region.generation(), generation);
        assert_eq!(region.next(16), Some(next));
    }

    #[test]
    fn a_full_part_drops_only_the_part_written_longest_ago() {
        let mut region = Region::new(4 * 4096 + 64).expect("a memfd region");
        let at = region.next(64).expect("room");
        region.put(at, &[0xc3; 64]);
        region.seal_floor();
        // One block of 1 KiB in each part, the first one twice.
        let mut blocks = Vec::new();
        for _ in 0..PARTS {
            let at = region.next(1024).expect("room in the part");
            region.put(at, &[0x90; 1024]);
            blocks.push((at, region.generation()));
            assert!(region.next(5000).is_none(), "more than is left of the part");
            while region.next(1024).is_some() {
                let at = region.next(1024).expect("room");
                region.put(at, &[0x90; 1024]);
            }
            let dropped = region.advance();
            assert_eq!(dropped.is_some(), blocks.len() == PARTS);
        }
        // The region wrote into every part and is back in the first one,
        // whose code it dropped. The others keep theirs.
        let (first, generation) = blocks[0];
        assert_ne!(region.generation_of(first), Some(generation));
        assert_eq!(region.next(1024), Some(first));
        for (at, generation) in &blocks[1..] {
            assert_eq!(region.generation_of(*at), Some(*generation));
            region.patch(*at, &[0xcc]);
        }
        // The trampoline is of no part.
        assert_eq!(region.generation_of(at), None);
    }
}
