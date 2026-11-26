//! The leader: a `qemu-system-aarch64` process with the lockstep plugin and
//! a gdbstub, started for a test run and killed at its end.
//!
//! What this file knows about QEMU is its command line, its gdbstub and the
//! stream of the plugin (S6).

use super::rsp::Rsp;
use super::{Core, STREAM_HEADER_BYTES, STREAM_MAGIC, Trace, Vectors, parse};
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

/// What the plugin puts into each instruction record beyond the core
/// registers.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// V0 to V31, FPSR and FPCR. FP is then enabled for EL1 and EL0.
    pub vectors: bool,
    /// System registers, by the names of QEMU's plugin API.
    pub system: Vec<String>,
    /// Write the name of every register of the plugin API to this file.
    pub names: Option<PathBuf>,
}

pub struct Leader {
    child: Child,
    rsp: Rsp,
    stream: File,
    vectors: bool,
    system: usize,
    /// Bytes of the plugin stream that are read and not parsed yet.
    pending: Vec<u8>,
    parsed: usize,
    running: bool,
    /// The breakpoint at `VECTOR` is set.
    vector_break: bool,
    directory: PathBuf,
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

impl Leader {
    /// Start QEMU stopped at its reset, with `ram_mib` of RAM at `RAM_BASE`.
    /// `directory` takes the FIFO and the socket. It must exist and be ours.
    pub fn start(qemu: &str, plugin: &Path, directory: &Path, ram_mib: u32) -> io::Result<Self> {
        Self::start_with(qemu, plugin, directory, ram_mib, false)
    }

    /// As `start`. With `vectors` each instruction record of the plugin
    /// carries V0 to V31, FPSR and FPCR, and FP is enabled for EL1 and EL0.
    pub fn start_with(
        qemu: &str,
        plugin: &Path,
        directory: &Path,
        ram_mib: u32,
        vectors: bool,
    ) -> io::Result<Self> {
        let options = Options {
            vectors,
            ..Options::default()
        };
        Self::start_options(qemu, plugin, directory, ram_mib, &options)
    }

    /// As `start`, with what each instruction record carries.
    pub fn start_options(
        qemu: &str,
        plugin: &Path,
        directory: &Path,
        ram_mib: u32,
        options: &Options,
    ) -> io::Result<Self> {
        let vectors = options.vectors;
        let mut extra = String::new();
        if vectors {
            extra.push_str(",vectors=on");
        }
        if !options.system.is_empty() {
            extra.push_str(",sysregs=");
            extra.push_str(&options.system.join(":"));
        }
        if let Some(names) = &options.names {
            extra.push_str(&format!(",names={}", names.display()));
        }
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
            .args([
                "-display", "none", "-monitor", "none", "-serial", "none", "-S",
            ])
            .arg("-gdb")
            .arg(format!("unix:{},server=on,wait=off", socket.display()))
            .arg("-plugin")
            .arg(format!(
                "{},out={}{extra}",
                plugin.display(),
                fifo.display()
            ))
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
        let system = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
        if (header[8] & 1 != 0) != vectors || system != options.system.len() {
            kill(child);
            return Err(other("the plugin stream does not carry what was asked for"));
        }
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
            system,
            pending: Vec::new(),
            parsed: 0,
            running: false,
            vector_break: true,
            directory: directory.to_path_buf(),
        };
        leader.rsp.break_at(VECTOR)?;
        if vectors {
            leader.enable_fp()?;
        }
        Ok(leader)
    }

    /// Let QEMU run a prelude that sets CPACR_EL1.FPEN, so that an FP
    /// instruction does not trap. The gdbstub cannot write a system register
    /// (DESIGN.md O1), so the guest does it. The RAM is put back afterwards.
    fn enable_fp(&mut self) -> io::Result<()> {
        // movz x0, #0x30, lsl #16 ; msr cpacr_el1, x0 ; isb ; b .
        const PRELUDE: [u32; 4] = [0xd2a0_0600, 0xd518_1040, 0xd503_3fdf, 0x1400_0000];
        let at = RAM_BASE + 0x1000;
        let end = at + 12;
        let bytes: Vec<u8> = PRELUDE.iter().flat_map(|word| word.to_le_bytes()).collect();
        let saved = self.rsp.read_memory(at, bytes.len())?;
        self.rsp.write_memory(at, &bytes)?;
        let mut core = self.rsp.core()?;
        core.pc = at;
        self.rsp.set_core(&core)?;
        self.rsp.break_at(end)?;
        self.resume()?;
        self.stop(false)?;
        let stopped = self.rsp.core()?;
        self.rsp.clear_break(end)?;
        self.rsp.write_memory(at, &saved)?;
        if stopped.pc != end {
            return Err(other(format!(
                "the prelude that enables FP stopped at {:#x}, not at its end",
                stopped.pc
            )));
        }
        Ok(())
    }

    /// Whether QEMU stops by itself at `VECTOR`.
    pub fn vector_break(&self) -> bool {
        self.vector_break
    }

    /// Remove the breakpoint at `VECTOR`. A round whose stream sets
    /// VBAR_EL1 does not need it, and a branch of the stream to that address
    /// would stop QEMU there without a record.
    pub fn clear_vector_break(&mut self) -> io::Result<()> {
        if self.vector_break {
            self.rsp.clear_break(VECTOR)?;
            self.vector_break = false;
        }
        Ok(())
    }

    /// Reset the machine of QEMU, as its monitor command `system_reset`
    /// does. QEMU stays stopped, at EL1 with SP_EL1, and every system
    /// register has its reset value. The RAM keeps its content, but the
    /// device tree at its start.
    pub fn reset(&mut self) -> io::Result<()> {
        if self.running {
            return Err(other("QEMU must be stopped for a reset"));
        }
        self.rsp.monitor("system_reset")?;
        let core = self.rsp.core()?;
        if core.cpsr & 0xf != 5 {
            return Err(other(format!(
                "QEMU is at CPSR {:#x} after a reset, not at EL1 with SP_EL1",
                core.cpsr
            )));
        }
        Ok(())
    }

    pub fn set_vectors(&mut self, vectors: &Vectors) -> io::Result<()> {
        self.rsp.set_vectors(vectors)
    }

    pub fn vectors(&mut self) -> io::Result<Vectors> {
        self.rsp.vectors()
    }

    pub fn load(&mut self, address: u64, data: &[u8]) -> io::Result<()> {
        self.rsp.write_memory(address, data)
    }

    /// The memory of QEMU from `address`, `len` bytes. The machine puts
    /// its device tree at the start of the RAM, so the RAM is not all zero
    /// at the reset, and a mirror of it starts from this.
    pub fn memory(&mut self, address: u64, len: usize) -> io::Result<Vec<u8>> {
        const CHUNK: usize = 0x8000;
        let mut data = Vec::with_capacity(len);
        while data.len() < len {
            let want = CHUNK.min(len - data.len());
            let chunk = self.rsp.read_memory(address + data.len() as u64, want)?;
            if chunk.len() != want {
                return Err(other("the gdbstub read less memory than asked"));
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }

    pub fn set_core(&mut self, core: &Core) -> io::Result<()> {
        self.rsp.set_core(core)
    }

    /// A hardware breakpoint that stops QEMU before the instruction at
    /// `address`.
    pub fn break_at(&mut self, address: u64) -> io::Result<()> {
        self.rsp.break_at(address)
    }

    pub fn clear_break(&mut self, address: u64) -> io::Result<()> {
        self.rsp.clear_break(address)
    }

    pub fn core(&mut self) -> io::Result<Core> {
        self.rsp.core()
    }

    /// Whether the instruction records carry the vector registers.
    pub fn has_vectors(&self) -> bool {
        self.vectors
    }

    /// Run `words` on QEMU alone, from `address`, to set system state that
    /// the gdbstub cannot write. A zero word after them ends the run at the
    /// breakpoint on the exception vector. The words stay in QEMU's memory:
    /// take the mirror of the RAM after this.
    pub fn prelude(&mut self, address: u64, words: &[u32]) -> io::Result<()> {
        let mut bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        bytes.extend([0u8; 4]);
        self.load(address, &bytes)?;
        let mut core = self.core()?;
        core.pc = address;
        self.set_core(&core)?;
        self.resume()?;
        self.stop(false)?;
        let stopped = self.core()?;
        if stopped.pc != VECTOR {
            return Err(other(format!(
                "the prelude stopped at {:#x}, not at the exception vector",
                stopped.pc
            )));
        }
        Ok(())
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
                parse(&self.pending[self.parsed..], self.vectors, self.system).map_err(other)?
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

    /// Parse what `pending` holds whole, and keep the memory writes.
    fn drain(&mut self, ahead: &mut Ahead) -> io::Result<()> {
        while let Some((trace, len)) =
            parse(&self.pending[self.parsed..], self.vectors).map_err(other)?
        {
            self.parsed += len;
            match trace {
                Trace::Instruction { .. } => ahead.instructions += 1,
                Trace::Write {
                    address,
                    size,
                    low,
                    high,
                } => ahead.writes.push((address, size, low, high)),
                Trace::Discontinuity { .. } => {}
            }
        }
        Ok(())
    }

    /// Wait until QEMU has stopped, and say what it did that nobody
    /// followed. With `interrupt` it is asked to stop first. Without, it
    /// is expected to stop at the breakpoint on the exception vector.
    ///
    /// QEMU blocks when the FIFO is full, so the FIFO is drained while the
    /// stop reply is awaited.
    pub fn stop(&mut self, interrupt: bool) -> io::Result<Ahead> {
        let mut ahead = Ahead::default();
        if !self.running {
            return Ok(ahead);
        }
        if interrupt {
            self.rsp.interrupt()?;
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            while self.fill()? {
                self.drain(&mut ahead)?;
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
        while self.fill()? {
            self.drain(&mut ahead)?;
        }
        self.drain(&mut ahead)?;
        if self.parsed != self.pending.len() {
            return Err(other("QEMU stopped in the middle of a record"));
        }
        self.pending.clear();
        self.parsed = 0;
        self.running = false;
        Ok(ahead)
    }
}

/// What QEMU did between the last record that the follower read and its
/// stop: it runs ahead of the follower by what the FIFO holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ahead {
    /// Instruction records that nobody followed.
    pub instructions: u64,
    /// The memory writes among them: address, size, and the value as two
    /// halves.
    pub writes: Vec<(u64, u32, u64, u64)>,
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
