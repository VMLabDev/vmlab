//! The perf map of the process (3e DESIGN.md H13e): `/tmp/perf-<pid>.map`,
//! one line per piece of code, `<start> <length> <name>` with the two
//! numbers in hexadecimal, which `perf report` reads for the addresses of
//! code that no file holds.
//!
//! Every JIT of the process writes to the one file, a line at a time under
//! a lock. A region that is full starts again from its front (H13c), so an
//! address can be named twice: the later line names the code that is there
//! now.
//!
//! Sources read: the format of the file as the documentation of `perf`
//! gives it. It is always under `/tmp`, where `perf` looks.

use std::fs::File;
use std::io::{self, Write};
use std::sync::Mutex;

static MAP: Mutex<Option<File>> = Mutex::new(None);

/// The path of the map of this process.
pub fn path() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/perf-{}.map", std::process::id()))
}

/// One line of the map.
pub fn line(address: u64, bytes: u64, name: &str) -> String {
    // A name holds no line break.
    let name: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("{address:x} {bytes:x} {name}\n")
}

/// Name `bytes` bytes of code at `address`.
pub fn name(address: u64, bytes: u64, name: &str) -> io::Result<()> {
    let mut map = MAP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.is_none() {
        *map = Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path())?,
        );
    }
    let file = map.as_mut().expect("the map is open");
    file.write_all(line(address, bytes, name).as_bytes())
}
