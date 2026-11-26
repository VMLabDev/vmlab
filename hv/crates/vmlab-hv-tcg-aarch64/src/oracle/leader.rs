//! The leader: a `qemu-system-aarch64` process with the lockstep plugin and
//! a gdbstub, started for a test run and killed at its end.
//!
//! What this file knows about QEMU is its command line, its gdbstub and the
//! stream of the plugin (S6).

use super::rsp::Rsp;
use super::{Core, STREAM_HEADER_BYTES, STREAM_MAGIC, Trace, parse};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// RAM of QEMU's `virt` machine starts here.
pub const RAM_BASE: u64 = 0x4000_0000;

/// Where an exception from EL1 with SP_EL1 goes while VBAR_EL1 is 0, as it
/// is after the reset. A hardware breakpoint there stops QEMU at the first
/// exception of a round.
pub const VECTOR: u64 = 0x200;

/// `O_NONBLOCK` of Linux, which the FIFO of the plugin is read with.
const O_NONBLOCK: i32 = 0o4000;

pub struct Leader {
    child: Child,
    rsp: Rsp,
    stream: File,
    vectors: bool,
    /// Bytes of the plugin stream that are read and not parsed yet.
    pending: Vec<u8>,
    parsed: usize,
    running: bool,
    directory: PathBuf,
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

impl Leader {
    /// Start QEMU stopped at its reset, with `ram_mib` of RAM at `RAM_BASE`.
    /// `directory` takes the FIFO and the socket. It must exist and be ours.
    pub fn start(qemu: &str, plugin: &Path, directory: &Path, ram_mib: u32) -> io::Result<Self> {
        let fifo = directory.join("a64lock.fifo");
        let socket = directory.join("gdb.sock");
        for path in [&fifo, &socket] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if !Command::new("mkfifo").arg(&fifo).status()?.success() {
            return Err(other("mkfifo failed"));
        }
        // A reader must exist before the plugin opens the FIFO for writing.
        let mut stream = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(&fifo)?;
        let child = Command::new(qemu)
            .args(["-machine", "virt", "-cpu", "cortex-a53", "-accel", "tcg"])
            .args(["-smp", "1", "-m", &ram_mib.to_string()])
            .args(["-display", "none", "-monitor", "none", "-serial", "none", "-S"])
            .arg("-gdb")
            .arg(format!("unix:{},server=on,wait=off", socket.display()))
            .arg("-plugin")
            .arg(format!("{},out={}", plugin.display(), fifo.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        // From here a failure must not leave QEMU behind: `Leader` kills it.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut header = Vec::new();
        while header.len() < STREAM_HEADER_BYTES {
            let mut buffer = [0u8; STREAM_HEADER_BYTES];
            let want = STREAM_HEADER_BYTES - header.len();
            match stream.read(&mut buffer[..want]) {
                Ok(count) if count > 0 => header.extend_from_slice(&buffer[..count]),
                // No writer yet, or nothing written yet.
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    kill(child);
                    return Err(error);
                }
            }
            if header.len() < STREAM_HEADER_BYTES {
                if Instant::now() > deadline {
                    kill(child);
                    return Err(other("QEMU did not load the lockstep plugin in 20 seconds"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if &header[..8] != STREAM_MAGIC {
            kill(child);
            return Err(other("the plugin stream does not start with its magic"));
        }
        let vectors = header[8] & 1 != 0;
        let rsp = loop {
            match Rsp::connect(&socket) {
                Ok(rsp) => break rsp,
                Err(error) if Instant::now() > deadline => {
                    kill(child);
                    return Err(error);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        let mut leader = Self {
            child,
            rsp,
            stream,
            vectors,
            pending: Vec::new(),
            parsed: 0,
            running: false,
            directory: directory.to_path_buf(),
        };
        leader.rsp.break_at(VECTOR)?;
        Ok(leader)
    }

    pub fn load(&mut self, address: u64, data: &[u8]) -> io::Result<()> {
        self.rsp.write_memory(address, data)
    }

    pub fn set_core(&mut self, core: &Core) -> io::Result<()> {
        self.rsp.set_core(core)
    }

    pub fn core(&mut self) -> io::Result<Core> {
        self.rsp.core()
    }

    pub fn resume(&mut self) -> io::Result<()> {
        self.rsp.resume()?;
        self.running = true;
        Ok(())
    }

    /// Read what the FIFO holds now. `false`: it holds nothing.
    fn fill(&mut self) -> io::Result<bool> {
        if self.parsed > 0 && self.parsed == self.pending.len() {
            self.pending.clear();
            self.parsed = 0;
        } else if self.parsed > 1 << 20 {
            self.pending.drain(..self.parsed);
            self.parsed = 0;
        }
        let mut buffer = [0u8; 1 << 16];
        match self.stream.read(&mut buffer) {
            Ok(0) => Err(other("QEMU closed the plugin stream")),
            Ok(count) => {
                self.pending.extend_from_slice(&buffer[..count]);
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The next record of the plugin, waiting at most `timeout` for it.
    pub fn next_record(&mut self, timeout: Duration) -> io::Result<Trace> {
        let deadline = Instant::now() + timeout;
        let mut idle = 0u32;
        loop {
            if let Some((trace, len)) =
                parse(&self.pending[self.parsed..], self.vectors).map_err(other)?
            {
                self.parsed += len;
                return Ok(trace);
            }
            if self.fill()? {
                idle = 0;
                continue;
            }
            // QEMU writes a record in well under a millisecond while it
            // runs, so spin first and sleep only when it stays quiet.
            idle += 1;
            if idle < 2000 {
                std::hint::spin_loop();
            } else {
                if Instant::now() > deadline {
                    return Err(other("QEMU wrote no record before the timeout"));
                }
                std::thread::sleep(Duration::from_micros(50));
            }
        }
    }

    /// Wait until QEMU has stopped, and drop the records that it wrote
    /// until then. With `interrupt` it is asked to stop first. Without, it
    /// is expected to stop at the breakpoint on the exception vector.
    ///
    /// QEMU blocks when the FIFO is full, so the FIFO is drained while the
    /// stop reply is awaited.
    pub fn stop(&mut self, interrupt: bool) -> io::Result<()> {
        if !self.running {
            return Ok(());
        }
        if interrupt {
            self.rsp.interrupt()?;
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            while self.fill()? {
                self.pending.clear();
                self.parsed = 0;
            }
            if let Some(reply) = self.rsp.receive(Duration::from_millis(1))? {
                if !reply.starts_with(b"T") && !reply.starts_with(b"S") {
                    return Err(other(format!(
                        "QEMU answered {:?} where a stop reply was expected",
                        String::from_utf8_lossy(&reply)
                    )));
                }
                break;
            }
            if Instant::now() > deadline {
                return Err(other("QEMU did not stop in 20 seconds"));
            }
        }
        // Every record is in the FIFO before the stop reply is sent.
        while self.fill()? {}
        self.pending.clear();
        self.parsed = 0;
        self.running = false;
        Ok(())
    }
}

fn kill(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for Leader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(self.directory.join("a64lock.fifo"));
        let _ = std::fs::remove_file(self.directory.join("gdb.sock"));
    }
}
