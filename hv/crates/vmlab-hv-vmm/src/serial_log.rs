//! The serial log file, kept under a size cap.
//!
//! The guest decides how much it writes to its serial port: a container's
//! stdout and stderr land here (PRD §18). Without a cap a workload that
//! writes without pause fills the host disk that holds the lab. The file
//! keeps its newest output: past the cap it is cut to its last half, at a
//! line start, and written on.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// The size a serial log is cut at. A cut keeps the newest half.
pub const SERIAL_LOG_CAP: u64 = 64 * 1024 * 1024;

/// The line a cut leaves at the top of the file.
const CUT_NOTE: &[u8] = b"[vmlab-hv: older serial output was dropped at the log size cap]\n";

/// An append-only log file that never stays above `cap` bytes.
///
/// The file is opened with `O_APPEND`, so a second writer of the same path
/// keeps landing at the end after a cut. The UART and the firmware debug port
/// are two such writers, each through its own `CappedLog`: a writer that
/// appended past this type would never be checked while the other is silent
/// (firmware review F1). The size is read from the file, not counted, so each
/// check sees both writers' bytes. The check runs once per `cap / 16` bytes
/// written here, which is how far one writer can take the file past the cap.
pub struct CappedLog {
    file: File,
    cap: u64,
    since_check: u64,
}

impl CappedLog {
    /// Opens `path` for append, creating it, and cuts a file that a run
    /// before the cap existed left above it.
    pub fn open(path: &Path, cap: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let mut log = Self {
            file,
            cap,
            since_check: 0,
        };
        log.cut_if_over()?;
        Ok(log)
    }

    fn cut_if_over(&mut self) -> io::Result<()> {
        self.since_check = 0;
        let len = self.file.metadata()?.len();
        if len <= self.cap {
            return Ok(());
        }
        let keep = self.cap / 2;
        let mut tail = vec![0u8; keep as usize];
        self.file.read_exact_at(&mut tail, len - keep)?;
        // Start the kept half at a line, where it has one.
        let from = tail
            .iter()
            .position(|&b| b == b'\n')
            .map_or(0, |newline| newline + 1);
        self.file.set_len(0)?;
        self.file.write_all(CUT_NOTE)?;
        self.file.write_all(&tail[from..])
    }
}

impl Write for CappedLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.file.write(buf)?;
        self.since_check += written as u64;
        if self.since_check >= (self.cap / 16).max(1) {
            // The bytes are in the file: a cut that fails costs the cap, not
            // the guest's write.
            let _ = self.cut_if_over();
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 4096;

    fn line(n: usize) -> Vec<u8> {
        format!("line {n:06}\n").into_bytes()
    }

    /// A guest that writes without pause does not grow the file past the cap
    /// (containers review F4), and the newest output is what stays.
    #[test]
    fn a_log_written_without_pause_stays_under_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        let mut log = CappedLog::open(&path, CAP).unwrap();
        let mut largest = 0;
        for n in 0..100_000 {
            log.write_all(&line(n)).unwrap();
            largest = largest.max(std::fs::metadata(&path).unwrap().len());
        }
        assert!(
            largest <= CAP + CAP / 16 + line(0).len() as u64,
            "the file reached {largest} bytes under a cap of {CAP}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next().unwrap().as_bytes(),
            &CUT_NOTE[..CUT_NOTE.len() - 1]
        );
        assert!(text.ends_with("line 099999\n"), "the newest line is kept");
        // What is kept is whole lines, in order, with no gap.
        let kept: Vec<&str> = lines.collect();
        let first: usize = kept[0].strip_prefix("line ").unwrap().parse().unwrap();
        for (i, l) in kept.iter().enumerate() {
            assert_eq!(l.as_bytes(), &line(first + i)[..11]);
        }
    }

    /// A log under the cap is appended to and left alone.
    #[test]
    fn a_log_under_its_cap_is_not_cut() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serial.log");
        std::fs::write(&path, b"an earlier boot\n").unwrap();
        let mut log = CappedLog::open(&path, CAP).unwrap();
        log.write_all(b"this boot\n").unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"an earlier boot\nthis boot\n"
        );
    }

    /// A file left above the cap by an earlier run is cut when it is opened,
    /// and a second appender of the same file lands after the cut.
    #[test]
    fn an_oversized_log_is_cut_at_open_and_other_appenders_follow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serial.log");
        let mut other = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        for n in 0..1000 {
            other.write_all(&line(n)).unwrap();
        }
        let _log = CappedLog::open(&path, CAP).unwrap();
        let cut = std::fs::metadata(&path).unwrap().len();
        assert!(cut <= CAP / 2 + CUT_NOTE.len() as u64, "cut to {cut} bytes");
        other.write_all(b"firmware\n").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.len() as u64, cut + 9, "no hole after the cut");
        assert!(text.ends_with("line 000999\nfirmware\n"));
    }

    /// A guest that writes to the firmware debug port alone, and nothing to
    /// the UART, does not grow the file past the cap (firmware review F1).
    #[test]
    fn the_debug_port_cannot_pass_the_serial_log_cap() {
        use vmlab_hv_devices::BusDevice;

        const CAP: u64 = 64 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serial.log");
        let _uart = CappedLog::open(&path, CAP).unwrap();
        let mut port = crate::q35::DebugPort::open(&path, CAP).unwrap();
        let page = [b'x'; 4096];
        let mut largest = 0;
        for _ in 0..256 {
            port.write(0, &page);
            largest = largest.max(std::fs::metadata(&path).unwrap().len());
        }
        assert!(
            largest <= CAP + CAP / 16,
            "the file reached {largest} bytes under a cap of {CAP}"
        );
    }
}
