//! The TSC frequency pin of a `tsc = "host"` VM (§5.2).
//!
//! vmlab-hv reads the host frequency from KVM at the VM's first KVM start
//! (`vmlab-hv --probe`); the lab daemon pins the answer per VM rather than
//! reading it afresh each time: the host kernel recalibrates its TSC at every
//! boot, and an online snapshot restores only at the frequency the VM was
//! started with. KVM runs a pinned frequency within its tolerance of the
//! host's without scaling, so an old pin on a recalibrated host costs nothing.
//! QEMU had a probe of its own here; a pinned TSC is an x86 CPU property, and
//! QEMU runs no x86 guest (§22).

use std::path::Path;

use anyhow::{Context, Result, anyhow};

/// The frequency pinned in `file` — a decimal Hz count — or `None` when
/// nothing is pinned there yet.
pub fn read_pin(file: &Path) -> Result<Option<u64>> {
    match std::fs::read_to_string(file) {
        Ok(text) => text
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|hz| *hz > 0)
            .map(Some)
            .ok_or_else(|| anyhow!("{} holds no TSC frequency", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

/// Pin `hz` in `file`.
pub fn write_pin(file: &Path, hz: u64) -> Result<()> {
    std::fs::write(file, format!("{hz}\n")).with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_round_trips_and_a_missing_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tsc-hz");
        assert_eq!(read_pin(&file).unwrap(), None);
        write_pin(&file, 4_291_956_000).unwrap();
        assert_eq!(read_pin(&file).unwrap(), Some(4_291_956_000));
        std::fs::write(&file, "garbage").unwrap();
        assert!(read_pin(&file).is_err());
    }
}
