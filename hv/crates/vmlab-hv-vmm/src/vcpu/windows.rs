//! The windows of a boot for the boot lockstep (card C13b, #1144; phase 3d
//! DESIGN.md O4): `VMLAB_HV_A64_WINDOWS=<dir>,<first>,<every>,<count>`
//! makes vCPU 0 of an emulated machine write the state of its CPU and the
//! RAM of the machine to `<dir>` after `<first>` instructions, then after
//! every `<every>` more, `<count>` times. Each window is `window-<n>.txt`
//! (`vmlab_hv_tcg_aarch64::oracle::window::Start`) and `window-<n>.ram`, a
//! file with a hole for every page of zeros. The example
//! `a64_window_lockstep` of `vmlab-hv-tcg-aarch64` follows each one against
//! QEMU.
//!
//! The state is written at the end of a slice, at a block head, where it is
//! exact. The vCPU writes nothing else while it does, so a write holds the
//! guest for the time of a copy of its RAM. The windows are a measurement:
//! nothing reads them in a product path.

use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use vmlab_hv_tcg::Memory;
use vmlab_hv_tcg_aarch64::Cpu;
use vmlab_hv_tcg_aarch64::oracle::window::Start;

use super::guest_bus::GuestBus;

/// Where the RAM of the `virt` machine starts.
const RAM_BASE: u64 = 0x4000_0000;
const PAGE: usize = 4096;
const CHUNK: usize = 1 << 20;

/// The windows that a vCPU still writes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Windows {
    directory: PathBuf,
    next: u64,
    every: u64,
    left: u32,
    written: u32,
}

impl Windows {
    /// The windows of `VMLAB_HV_A64_WINDOWS`, for the vCPU `id`: only vCPU
    /// 0 writes them.
    pub(crate) fn from_environment(id: u8) -> Option<Self> {
        if id != 0 {
            return None;
        }
        let value = std::env::var("VMLAB_HV_A64_WINDOWS").ok()?;
        let windows = Self::parse(&value);
        if windows.is_none() {
            log::warn!(
                "VMLAB_HV_A64_WINDOWS is <dir>,<first>,<every>,<count>, not {value:?}: no window is written"
            );
        }
        windows
    }

    fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split(',');
        let directory = PathBuf::from(parts.next().filter(|dir| !dir.is_empty())?);
        let first = parts.next()?.parse().ok()?;
        let every = parts
            .next()?
            .parse::<u64>()
            .ok()
            .filter(|every| *every > 0)?;
        let left = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            directory,
            next: first,
            every,
            left,
            written: 0,
        })
    }

    /// The instructions from `instructions` to the next window, while one
    /// is left to write.
    pub(crate) fn left(&self, instructions: u64) -> Option<u64> {
        (self.left > 0).then(|| self.next.saturating_sub(instructions))
    }

    /// Whether the CPU, at `instructions`, writes its next window now.
    pub(crate) fn due(&self, instructions: u64) -> bool {
        self.left > 0 && instructions >= self.next
    }

    /// Write the window of the CPU as it is now, and plan the next one.
    pub(crate) fn write(&mut self, cpu: &Cpu<GuestBus>) -> io::Result<()> {
        let number = self.written;
        self.written += 1;
        self.left -= 1;
        self.next = cpu.instructions + self.every;
        let start = Start::of(&cpu.state, cpu.instructions, cpu.clock.count());
        let ram = self.directory.join(format!("window-{number}.ram"));
        let (base, last) = cpu
            .memory
            .ram_bounds(RAM_BASE)
            .ok_or_else(|| io::Error::other("the machine has no RAM at 0x40000000"))?;
        let size = last - base + 1;
        let file = std::fs::File::create(&ram)?;
        file.set_len(size)?;
        let mut chunk = vec![0u8; CHUNK];
        let mut offset = 0u64;
        while offset < size {
            let len = CHUNK.min((size - offset) as usize);
            if !cpu.memory.code_bytes(base + offset, &mut chunk[..len]) {
                return Err(io::Error::other(format!(
                    "the RAM at {:#x} does not read",
                    base + offset
                )));
            }
            for (index, page) in chunk[..len].chunks(PAGE).enumerate() {
                if page.iter().any(|byte| *byte != 0) {
                    file.write_all_at(page, offset + (index * PAGE) as u64)?;
                }
            }
            offset += len as u64;
        }
        // The text last: a window is whole when its text is there.
        std::fs::write(
            self.directory.join(format!("window-{number}.txt")),
            start.to_text(),
        )?;
        log::info!(
            "vcpu 0: window {number} written after {} instructions, at {:#x}, {} MiB of RAM",
            cpu.instructions,
            start.core.pc,
            size >> 20
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_names_a_directory_the_first_window_the_gap_and_the_count() {
        assert_eq!(
            Windows::parse("/tmp/w,1000,500,3"),
            Some(Windows {
                directory: "/tmp/w".into(),
                next: 1000,
                every: 500,
                left: 3,
                written: 0,
            })
        );
        for wrong in [
            "",
            "/tmp/w",
            "/tmp/w,1,0,3",
            ",1,2,3",
            "/tmp/w,1,2,3,4",
            "/w,x,2,3",
        ] {
            assert_eq!(Windows::parse(wrong), None, "{wrong}");
        }
        let windows = Windows::parse("/w,10,5,1").unwrap();
        assert!(!windows.due(9));
        assert!(windows.due(10));
        let done = Windows {
            left: 0,
            ..Windows::parse("/w,10,5,1").unwrap()
        };
        assert!(!done.due(100));
    }
}
