//! The code region (DESIGN.md H13b to H13d, Q3): one `memfd` mapped twice,
//! a view that is readable and writable where the emitter writes, and a
//! view that is readable and executable where the CPU runs. No page is
//! writable and executable in one mapping.
//!
//! The region is a bump allocator. When it is full it starts again from
//! the front, after the trampoline, and its generation moves on: a block
//! whose code is of an older generation has none (H13c).
//!
//! Sources read: the Linux man pages of `memfd_create(2)`, `mmap(2)` and
//! `ftruncate(2)`.

use std::io;

/// `MFD_EXEC` of Linux 6.3: the file may be mapped executable also where
/// `vm.memfd_noexec` would refuse it. An older kernel rejects the flag, and
/// the region asks again without it.
const MFD_EXEC: libc::c_uint = 0x10;

pub struct Region {
    fd: libc::c_int,
    write: *mut u8,
    exec: *mut u8,
    len: usize,
    /// The first byte after the code that lives for the whole region.
    floor: usize,
    used: usize,
    generation: u64,
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
        Ok(Self {
            fd,
            write,
            exec,
            len,
            floor: 0,
            used: 0,
            generation: 0,
        })
    }

    /// The generation of the code in the region.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The bytes in use.
    pub fn used(&self) -> usize {
        self.used
    }

    /// The address at which the next code of `len` bytes runs, when it fits
    /// without a reset.
    pub fn next(&self, len: usize) -> Option<u64> {
        let start = self.used.next_multiple_of(16);
        (start + len <= self.len).then(|| self.exec as u64 + start as u64)
    }

    /// Drop every block of the region: the code after the floor is of an
    /// older generation from now on.
    pub fn reset(&mut self) {
        self.used = self.floor;
        self.generation += 1;
    }

    /// Copy `code` to `address`, which `next` gave for its length, and take
    /// its bytes.
    ///
    /// # Panics
    /// When `address` is not what `next` gives for that length.
    pub fn put(&mut self, address: u64, code: &[u8]) {
        assert_eq!(self.next(code.len()), Some(address), "the place of the code");
        let offset = (address - self.exec as u64) as usize;
        // SAFETY: `next` checked that the bytes are inside the writable
        // view, and no executable code of the current generation is there:
        // the bump pointer only moves forward inside a generation.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), self.write.add(offset), code.len());
        }
        self.used = offset + code.len();
    }

    /// Keep the code so far for the whole life of the region: the
    /// trampoline.
    pub fn seal_floor(&mut self) {
        self.floor = self.used.next_multiple_of(64);
        self.used = self.floor;
    }

    /// Whether `address` is inside the executable view.
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
        assert_eq!(region.generation(), generation + 1);
        assert_eq!(region.next(16), Some(next));
    }
}
