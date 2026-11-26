//! The leader of the boot lockstep (DESIGN.md O4): a QEMU that boots a real
//! firmware with the plugin, and the stream that the follower reads. The
//! follower is the whole machine of `vmlab-hv-vmm`
//! (`examples/a64_boot_lockstep.rs`). What is here needs no machine: the
//! argv of the leader as a value, its records with the device reads, and
//! which results of an instruction the follower takes from the leader.
//!
//! QEMU is a black box here (S6). Nothing in this module is part of a
//! product path.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{STREAM_HEADER_BYTES, STREAM_MAGIC, Trace, parse};
use crate::sysreg;

/// A device read of the leader: the physical address, the size in bytes and
/// the value, little-endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRead {
    pub address: u64,
    pub size: u32,
    pub value: u128,
}

/// One record of the stream of a boot.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Trace(Trace),
    Read(DeviceRead),
}

/// Parse the record at the start of `bytes`, as `oracle::parse` does, with
/// the device reads that `reads=io` adds.
pub fn parse_record(
    bytes: &[u8],
    vectors: bool,
    system: usize,
) -> Result<Option<(Record, usize)>, String> {
    if bytes.len() >= 4 && bytes[..4] == 4u32.to_le_bytes() {
        if bytes.len() < 32 {
            return Ok(None);
        }
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        return Ok(Some((
            Record::Read(DeviceRead {
                address: word(8),
                size,
                value: u128::from(word(16)) | u128::from(word(24)) << 64,
            }),
            32,
        )));
    }
    Ok(parse(bytes, vectors, system)?.map(|(trace, len)| (Record::Trace(trace), len)))
}

/// What the leader of a boot is started with.
#[derive(Clone, Debug)]
pub struct Options {
    pub qemu: String,
    pub plugin: PathBuf,
    /// The directory of the FIFO and of the serial log of the leader.
    pub directory: PathBuf,
    pub cpu: String,
    pub ram_mib: u32,
    /// The two flash banks: the firmware code, and a copy of its variables
    /// that the leader may write.
    pub code: PathBuf,
    pub vars: PathBuf,
    /// The names of the system registers in each instruction record.
    pub system: Vec<String>,
}

impl Options {
    /// The machine of the leader: a `virt` machine with one CPU and no
    /// device that the user did not name, the random seed of its device
    /// tree off, and a clock that counts instructions (DESIGN.md M4).
    fn machine(&self) -> Vec<String> {
        [
            "-machine",
            "virt,dtb-randomness=off",
            "-cpu",
            &self.cpu,
            "-accel",
            "tcg",
            "-smp",
            "1",
            "-m",
            &self.ram_mib.to_string(),
            "-icount",
            "shift=0,sleep=off",
            "-rtc",
            "base=2020-01-01T00:00:00,clock=vm",
            "-nodefaults",
            "-display",
            "none",
            "-monitor",
            "none",
        ]
        .into_iter()
        .map(String::from)
        .chain([
            "-drive".into(),
            format!(
                "if=pflash,format=raw,readonly=on,file={}",
                self.code.display()
            ),
            "-drive".into(),
            format!("if=pflash,format=raw,file={}", self.vars.display()),
        ])
        .collect()
    }

    pub fn fifo(&self) -> PathBuf {
        self.directory.join("a64boot.fifo")
    }

    pub fn serial(&self) -> PathBuf {
        self.directory.join("leader-serial.log")
    }

    /// The socket of the serial port of the leader: what a client sends
    /// there, the guest receives as keys.
    pub fn serial_socket(&self) -> PathBuf {
        self.directory.join("leader-serial.sock")
    }

    /// The argv of the leader, after the name of the program.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = self.machine();
        argv.extend([
            "-chardev".into(),
            format!(
                "socket,id=serial,path={},server=on,wait=off,logfile={}",
                self.serial_socket().display(),
                self.serial().display()
            ),
            "-serial".into(),
            "chardev:serial".into(),
            "-plugin".into(),
            format!(
                "{},out={},vectors=on,reads=io,pipe=1048576{}",
                self.plugin.display(),
                self.fifo().display(),
                if self.system.is_empty() {
                    String::new()
                } else {
                    format!(",sysregs={}", self.system.join(":"))
                }
            ),
        ]);
        argv
    }

    /// The argv of a QEMU that writes the device tree of the same machine
    /// to `path` and ends.
    pub fn dump_argv(&self, path: &Path) -> Vec<String> {
        let mut argv = self.machine();
        argv[1] = format!("virt,dtb-randomness=off,dumpdtb={}", path.display());
        argv
    }
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// The device tree that the leader gives its firmware.
pub fn dump_device_tree(options: &Options) -> io::Result<Vec<u8>> {
    let path = options.directory.join("leader.dtb");
    let status = Command::new(&options.qemu)
        .args(options.dump_argv(&path))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(other(format!(
            "QEMU did not write its device tree: {status}"
        )));
    }
    std::fs::read(&path)
}

/// fw_cfg of `virt`: the data register, and the selector register after it
/// (QEMU's interface document `docs/specs/fw_cfg.rst`, S7).
const FW_CFG_DATA: u64 = 0x0902_0000;
const FW_CFG_SELECTOR: u64 = FW_CFG_DATA + 8;
/// The item that holds the directory of the files.
const FW_CFG_DIRECTORY: u16 = 0x19;

/// A QEMU of the machine of the leader that runs no guest: its devices
/// answer over the qtest protocol.
struct Qtest {
    child: Child,
    reader: io::BufReader<std::os::unix::net::UnixStream>,
}

impl Qtest {
    fn start(options: &Options) -> io::Result<Self> {
        let socket = options.directory.join("qtest.sock");
        match std::fs::remove_file(&socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // The same machine without a CPU that runs: no accelerator and no
        // instruction clock.
        let mut argv = Vec::new();
        let mut machine = options.machine().into_iter();
        while let Some(word) = machine.next() {
            match word.as_str() {
                "-accel" | "-icount" => {
                    machine.next();
                }
                _ => argv.push(word),
            }
        }
        argv.extend([
            "-accel".into(),
            "qtest".into(),
            "-qtest".into(),
            format!("unix:{},server=on,wait=off", socket.display()),
            "-qtest-log".into(),
            "/dev/null".into(),
        ]);
        let mut child = Command::new(&options.qemu)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let stream = loop {
            match std::os::unix::net::UnixStream::connect(&socket) {
                Ok(stream) => break stream,
                Err(error) if Instant::now() > deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        Ok(Self {
            child,
            reader: io::BufReader::new(stream),
        })
    }

    /// Send the commands, and give the value of each answer.
    fn answers(&mut self, commands: &[String]) -> io::Result<Vec<u64>> {
        use std::io::{BufRead, Write};
        let mut text = String::new();
        for command in commands {
            text.push_str(command);
            text.push('\n');
        }
        self.reader.get_mut().write_all(text.as_bytes())?;
        let mut values = Vec::with_capacity(commands.len());
        while values.len() < commands.len() {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Err(other("QEMU closed its qtest socket"));
            }
            let mut words = line.split_whitespace();
            match words.next() {
                Some("OK") => values.push(match words.next() {
                    Some(value) => u64::from_str_radix(value.trim_start_matches("0x"), 16)
                        .map_err(|_| other(format!("qtest answered {line:?}")))?,
                    None => 0,
                }),
                // A line that QEMU sends by itself, such as an interrupt.
                Some("IRQ") | None => {}
                Some(_) => return Err(other(format!("qtest answered {line:?}"))),
            }
        }
        Ok(values)
    }

    /// The first `length` bytes of the fw_cfg item `selector`.
    fn item(&mut self, selector: u16, length: usize) -> io::Result<Vec<u8>> {
        // The selector register is big-endian, and qtest writes the word
        // in the byte order of the guest.
        self.answers(&[format!(
            "writew {FW_CFG_SELECTOR:#x} {:#x}",
            selector.swap_bytes()
        )])?;
        let mut data = Vec::with_capacity(length);
        while data.len() < length {
            let count = (length - data.len()).min(256);
            let commands = vec![format!("readb {FW_CFG_DATA:#x}"); count];
            data.extend(self.answers(&commands)?.into_iter().map(|byte| byte as u8));
        }
        Ok(data)
    }
}

impl Drop for Qtest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The names and the sizes of the files of a fw_cfg directory, with the
/// selector of each: 64 bytes for a file, after a count.
pub fn directory_entries(directory: &[u8]) -> Vec<(String, u16, usize)> {
    directory
        .chunks_exact(64)
        .map(|entry| {
            let size = u32::from_be_bytes(entry[..4].try_into().unwrap()) as usize;
            let selector = u16::from_be_bytes(entry[4..6].try_into().unwrap());
            let name = entry[8..].split(|byte| *byte == 0).next().unwrap_or(&[]);
            (String::from_utf8_lossy(name).into_owned(), selector, size)
        })
        .collect()
}

/// The fw_cfg files that the leader gives its firmware, in directory order.
/// The follower gets the same files: what the firmware reads through DMA
/// lands in memory without a device read that the follower could take from
/// the leader.
pub fn capture_fw_cfg(options: &Options) -> io::Result<Vec<(String, Vec<u8>)>> {
    let mut qtest = Qtest::start(options)?;
    let count = qtest.item(FW_CFG_DIRECTORY, 4)?;
    let count = u32::from_be_bytes(count.try_into().unwrap()) as usize;
    if count > 4096 {
        return Err(other(format!("a fw_cfg directory of {count} files")));
    }
    let directory = qtest.item(FW_CFG_DIRECTORY, 4 + 64 * count)?;
    let mut files = Vec::with_capacity(count);
    for (name, selector, size) in directory_entries(&directory[4..]) {
        files.push((name, qtest.item(selector, size)?));
    }
    Ok(files)
}

/// A running leader and its stream.
pub struct Leader {
    child: Child,
    stream: std::fs::File,
    system: usize,
    pending: Vec<u8>,
    parsed: usize,
    ahead: VecDeque<Record>,
}

impl Leader {
    /// Start QEMU. It runs at once and blocks when the FIFO is full, so it
    /// is never more than a FIFO ahead of its follower.
    pub fn start(options: &Options) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let fifo = options.fifo();
        match std::fs::remove_file(&fifo) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if !Command::new("mkfifo").arg(&fifo).status()?.success() {
            return Err(other("mkfifo failed"));
        }
        // A reader must exist before the plugin opens the FIFO for writing.
        let stream = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc_o_nonblock())
            .open(&fifo)?;
        let child = Command::new(&options.qemu)
            .args(options.argv())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut leader = Self {
            child,
            stream,
            system: options.system.len(),
            pending: Vec::new(),
            parsed: 0,
            ahead: VecDeque::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while leader.pending.len() < STREAM_HEADER_BYTES {
            if !leader.fill()? {
                if Instant::now() > deadline {
                    return Err(other("QEMU did not load the lockstep plugin in 30 seconds"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let header = &leader.pending[..STREAM_HEADER_BYTES];
        let flags = header[8];
        let system = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        if &header[..8] != STREAM_MAGIC || flags & 5 != 5 || system != leader.system {
            return Err(other(
                "the plugin stream does not carry the vectors, the device reads and the registers",
            ));
        }
        leader.parsed = STREAM_HEADER_BYTES;
        Ok(leader)
    }

    /// Read what the FIFO holds. `false`: nothing.
    fn fill(&mut self) -> io::Result<bool> {
        if self.parsed > 1 << 20 {
            self.pending.drain(..self.parsed);
            self.parsed = 0;
        }
        let mut buffer = [0u8; 1 << 16];
        match self.stream.read(&mut buffer) {
            Ok(0) => Ok(false),
            Ok(count) => {
                self.pending.extend_from_slice(&buffer[..count]);
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// The next record, waiting at most `timeout` for it. `None`: the
    /// leader ended, or wrote nothing in that time.
    pub fn next(&mut self, timeout: Duration) -> io::Result<Option<Record>> {
        if let Some(record) = self.ahead.pop_front() {
            return Ok(Some(record));
        }
        let mut deadline = None;
        let mut idle = 0u32;
        loop {
            if let Some((record, len)) =
                parse_record(&self.pending[self.parsed..], true, self.system).map_err(other)?
            {
                self.parsed += len;
                return Ok(Some(record));
            }
            if self.fill()? {
                idle = 0;
                deadline = None;
                continue;
            }
            idle += 1;
            if idle < 2000 {
                std::hint::spin_loop();
                continue;
            }
            if self.child.try_wait()?.is_some() && !self.fill()? {
                return Ok(None);
            }
            let deadline = *deadline.get_or_insert_with(|| Instant::now() + timeout);
            if Instant::now() > deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_micros(50));
        }
    }

    /// Put a record back: the next `next` gives it again.
    pub fn unread(&mut self, record: Record) {
        self.ahead.push_front(record);
    }
}

impl Drop for Leader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `O_NONBLOCK` of Linux, without a dependency for one constant.
fn libc_o_nonblock() -> i32 {
    0o4000
}

/// A result of an instruction that is not a function of the CPU state: the
/// follower takes it from the leader (DESIGN.md O4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adopt {
    /// `MRS` of the system counter into this register. The clock of the
    /// follower takes the value.
    Counter(usize),
    /// `MRS` of a register whose value follows from the time or from the
    /// interrupt lines: a timer value, a timer control with its status bit,
    /// the pending interrupts.
    Timed(usize),
    /// `MRS` of an ID register into this register: the CPU of the leader is
    /// not the CPU of the product (DESIGN.md G2), and a difference is a
    /// line of the allow-list.
    Identity(usize, u16),
    /// X0 to X3 after `HVC` or `SMC`: the firmware interface of the machine
    /// answered, and a difference is a finding about the machine.
    Call,
}

/// What the follower takes from the leader after the instruction `word`.
pub fn adoption(word: u32) -> Option<Adopt> {
    // HVC and SMC, with any immediate.
    if word & 0xffe0_001f == 0xd400_0002 || word & 0xffe0_001f == 0xd400_0003 {
        return Some(Adopt::Call);
    }
    // MRS Xt, <register>.
    if word & 0xffe0_0000 != 0xd520_0000 {
        return None;
    }
    let rt = (word & 31) as usize;
    if rt == 31 {
        return None;
    }
    let encoding = ((word >> 5) & 0xffff) as u16;
    let (op1, crn, crm) = (encoding >> 11 & 7, encoding >> 7 & 15, encoding >> 3 & 15);
    match encoding {
        // CNTPCT_EL0 and CNTVCT_EL0.
        0xdf01 | 0xdf02 => Some(Adopt::Counter(rt)),
        // CNTP_TVAL_EL0, CNTP_CTL_EL0, CNTV_TVAL_EL0, CNTV_CTL_EL0, and
        // ISR_EL1.
        0xdf10 | 0xdf11 | 0xdf18 | 0xdf19 | 0xc608 => Some(Adopt::Timed(rt)),
        // The ID space: op0 3, CRn 0, and op1 0 (MIDR to the feature
        // registers), 1 (the cache registers) or 3 (CTR_EL0, DCZID_EL0).
        _ if encoding >> 14 == 3 && crn == 0 && matches!(op1, 0 | 1 | 3) && crm < 8 => {
            Some(Adopt::Identity(rt, encoding))
        }
        _ => None,
    }
}

/// The name of a system register for a report.
pub fn register_name(encoding: u16) -> String {
    sysreg::name(encoding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::asm;

    #[test]
    fn a_device_read_is_a_record_of_its_own() {
        let mut bytes = Vec::new();
        bytes.extend(4u32.to_le_bytes());
        bytes.extend(4u32.to_le_bytes());
        bytes.extend(0x0900_0018u64.to_le_bytes());
        bytes.extend(0x90u64.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        assert_eq!(parse_record(&bytes[..31], true, 0), Ok(None));
        assert_eq!(
            parse_record(&bytes, true, 0),
            Ok(Some((
                Record::Read(DeviceRead {
                    address: 0x0900_0018,
                    size: 4,
                    value: 0x90
                }),
                32
            )))
        );
        // A discontinuity goes through the parser of the instruction
        // lockstep.
        let mut bytes = Vec::new();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(0x1000u64.to_le_bytes());
        bytes.extend(0x2280u64.to_le_bytes());
        assert_eq!(
            parse_record(&bytes, true, 0),
            Ok(Some((
                Record::Trace(Trace::Discontinuity {
                    kind: 1,
                    from_pc: 0x1000,
                    to_pc: 0x2280
                }),
                24
            )))
        );
    }

    #[test]
    fn a_fw_cfg_directory_names_its_files() {
        let mut directory = Vec::new();
        for (name, selector, size) in [("bootorder", 0x21u16, 0u32), ("etc/acpi/rsdp", 0x22, 36)] {
            directory.extend(size.to_be_bytes());
            directory.extend(selector.to_be_bytes());
            directory.extend([0; 2]);
            let mut field = [0u8; 56];
            field[..name.len()].copy_from_slice(name.as_bytes());
            directory.extend(field);
        }
        assert_eq!(
            directory_entries(&directory),
            [
                ("bootorder".to_string(), 0x21, 0),
                ("etc/acpi/rsdp".to_string(), 0x22, 36)
            ]
        );
    }

    #[test]
    fn what_is_not_a_function_of_the_cpu_state_is_adopted() {
        assert_eq!(adoption(asm::mrs(3, 0xdf02)), Some(Adopt::Counter(3)));
        assert_eq!(adoption(asm::mrs(4, 0xdf01)), Some(Adopt::Counter(4)));
        assert_eq!(adoption(asm::mrs(5, 0xdf19)), Some(Adopt::Timed(5)));
        assert_eq!(adoption(asm::mrs(5, 0xc608)), Some(Adopt::Timed(5)));
        // MIDR_EL1, ID_AA64DFR0_EL1, CLIDR_EL1, CTR_EL0.
        for encoding in [0xc000, 0xc028, 0xc801, 0xd801] {
            assert_eq!(
                adoption(asm::mrs(7, encoding)),
                Some(Adopt::Identity(7, encoding))
            );
        }
        assert_eq!(adoption(asm::hvc(0)), Some(Adopt::Call));
        assert_eq!(adoption(0xd400_0003), Some(Adopt::Call));
        // SCTLR_EL1, an MSR, a read into the zero register, an SVC, and
        // CNTFRQ_EL0, which is a stored register.
        assert_eq!(adoption(asm::mrs(1, asm::reg::SCTLR_EL1)), None);
        assert_eq!(adoption(asm::msr(0xdf02, 1)), None);
        assert_eq!(adoption(asm::mrs(31, 0xdf02)), None);
        assert_eq!(adoption(asm::svc(0)), None);
        assert_eq!(adoption(asm::mrs(1, 0xdf00)), None);
        assert_eq!(register_name(0xdf02), "CNTVCT_EL0");
    }

    #[test]
    fn the_leader_is_a_bare_virt_machine_with_a_counted_clock() {
        let options = Options {
            qemu: "qemu-system-aarch64".into(),
            plugin: "/p/a64lock.so".into(),
            directory: "/d".into(),
            cpu: "cortex-a53".into(),
            ram_mib: 1024,
            code: "/f/code.fd".into(),
            vars: "/d/vars.fd".into(),
            system: vec!["ESR_EL1".into(), "FAR_EL1".into()],
        };
        let argv = options.argv().join(" ");
        assert!(argv.starts_with(
            "-machine virt,dtb-randomness=off -cpu cortex-a53 -accel tcg -smp 1 -m 1024 \
             -icount shift=0,sleep=off -rtc base=2020-01-01T00:00:00,clock=vm -nodefaults"
        ));
        assert!(argv.contains("-drive if=pflash,format=raw,readonly=on,file=/f/code.fd"));
        assert!(argv.contains("-drive if=pflash,format=raw,file=/d/vars.fd"));
        assert!(argv.contains(
            "-chardev socket,id=serial,path=/d/leader-serial.sock,server=on,wait=off,logfile=/d/leader-serial.log -serial chardev:serial"
        ));
        assert!(argv.ends_with(
            "-plugin /p/a64lock.so,out=/d/a64boot.fifo,vectors=on,reads=io,pipe=1048576,sysregs=ESR_EL1:FAR_EL1"
        ));
        let dump = options.dump_argv(Path::new("/d/x.dtb")).join(" ");
        assert!(dump.contains("-machine virt,dtb-randomness=off,dumpdtb=/d/x.dtb -cpu"));
        assert!(!dump.contains("-plugin"));
    }
}
