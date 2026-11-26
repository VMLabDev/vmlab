//! The boot lockstep against QEMU (DESIGN.md O4). `just tcg-a64-boot-lockstep`
//! in `hv/` builds the plugin and runs this.
//!
//! Usage:
//!   a64_boot_lockstep --plugin <a64lock.so> --dir <scratch directory>
//!       --code <AAVMF_CODE.fd> --vars <AAVMF_VARS.fd>
//!       [--qemu <binary>] [--ram-mib N] [--until <text>]
//!       [--max-instructions N] [--resync N] [--ignore <register>]...
//!       [--system core|all] [--key <text>=<bytes>]...
//!
//! QEMU boots the firmware at `-smp 1` with the plugin, and our whole `virt`
//! machine follows its stream from the reset. Both start from the same
//! firmware and the same variables (each side gets its own copy), the device
//! tree that the leader writes, and the same memory map. Before every
//! instruction the follower compares X0 to X30, SP, PC, PSTATE, V0 to V31,
//! FPSR, FPCR and system registers, and after it the memory writes. The
//! system registers are those of an exception and of the translation
//! (`CORE`), or with `--system all` every one of `oracle::compared`, which
//! makes the leader three times slower.
//!
//! The leader leads for what is not a function of the CPU state:
//! - an interrupt is taken where the stream says. The follower says so when
//!   our interrupt controller does not have it pending there;
//! - a device read takes the value that the leader read. Our device answers
//!   too, and a different answer is a device finding, not a CPU difference;
//! - a read of the counter, of a timer or of the pending interrupts takes
//!   the value of the leader, and our clock follows the counter;
//! - a read of an ID register that differs is a line of the allow-list;
//! - the answer to `HVC` and `SMC` is compared, and a different one is a
//!   finding about the machine.
//!
//! This AAVMF starts no shell by itself. Without a disk it waits for a key,
//! and its boot menu has the shell as the second entry. So the leader gets
//! keys on its serial port: each `--key` is sent when the serial log of the
//! leader holds its text, after the keys before it (`\e` is escape, `\r`
//! return). Without `--key` the keys are those that lead to the shell
//! (`TO_THE_SHELL`). Our machine gets no key of its own: it reads the PL011
//! of the leader, as it reads every device.
//!
//! The run ends when our serial log holds `--until` (default `Shell>`), at
//! `--max-instructions`, or at the first CPU difference with exit code 1.
//! With `--resync N` a CPU difference is printed, our CPU takes the state of
//! the leader, and the run goes on until N differences: one run then shows
//! more than the first defect. The last instructions before a difference go
//! to `last-instructions.txt` in the scratch directory.
//!
//! QEMU is the test oracle only (S6). Nothing here is a product path.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use vmlab_hv_devices::gic;
use vmlab_hv_proto::machine::MachineDescription;
use vmlab_hv_tcg_aarch64::exec::Access;
use vmlab_hv_tcg_aarch64::oracle::boot::{self, Adopt, DeviceRead, Leader, Record};
use vmlab_hv_tcg_aarch64::oracle::{
    Core, Trace, Vectors, apply_system, compared, normalise, system_difference, system_of,
};
use vmlab_hv_tcg_aarch64::timer::Clock;
use vmlab_hv_tcg_aarch64::{Stop, decode, sysreg};
use vmlab_hv_vmm::NoDevices;
use vmlab_hv_vmm::vcpu::{Conduit, Hypercall, HypercallOutcome, Kick, VcpuHost};
use vmlab_hv_vmm::virt;
use vmlab_hv_vmm::vm::ExitReason;

type Error = Box<dyn std::error::Error>;

/// The system registers that every run compares: those of an exception,
/// of the translation and of the timers, and the two stack pointers.
const CORE: [&str; 20] = [
    "ESR_EL1",
    "FAR_EL1",
    "ELR_EL1",
    "SPSR_EL1",
    "VBAR_EL1",
    "SCTLR_EL1",
    "TCR_EL1",
    "TTBR0_EL1",
    "TTBR1_EL1",
    "MAIR_EL1",
    "CPACR_EL1",
    "CONTEXTIDR_EL1",
    "TPIDR_EL0",
    "TPIDR_EL1",
    "CNTKCTL_EL1",
    "CNTV_CTL_EL0",
    "CNTV_CVAL_EL0",
    "CNTP_CTL_EL0",
    "SP_EL0",
    "SP_EL1",
];

/// The keys that take AAVMF from its boot manager to its shell, each with
/// the text that the serial log of the leader holds before it is sent.
const TO_THE_SHELL: [(&str, &str); 4] = [
    ("Press any key to enter the Boot Manager Menu", "x"),
    ("EFI Internal Shell", "\\e[B"),
    ("EFI Internal Shell", "\\r"),
    ("any other key to continue", "\\r"),
];
/// The instructions between two keys: the menu draws what a key changed.
const KEY_GAP: u64 = 1 << 21;

/// The instructions that the follower keeps for the report of a difference.
const KEPT: usize = 4096;
/// How long the leader may write nothing before the run gives up.
const SILENCE: Duration = Duration::from_secs(60);

/// What our devices answered differently from those of the leader, by the
/// page of the address.
#[derive(Default)]
struct DeviceFindings {
    pages: BTreeMap<u64, (u64, String)>,
    reads: u64,
}

impl DeviceFindings {
    fn note(&mut self, address: u64, what: String) {
        let entry = self.pages.entry(address >> 12).or_insert((0, what));
        entry.0 += 1;
    }
}

/// The machine as our CPU sees it, with the device reads of the leader.
struct LockHost {
    inner: Arc<dyn VcpuHost>,
    /// The device reads of the instruction that runs now.
    reads: Mutex<VecDeque<DeviceRead>>,
    findings: Mutex<DeviceFindings>,
    exit: Mutex<Option<ExitReason>>,
    reset: AtomicBool,
}

impl VcpuHost for LockHost {
    fn should_run(&self, id: u8) -> bool {
        self.inner.should_run(id)
    }

    fn exited(&self) -> bool {
        self.inner.exited()
    }

    fn park(&self, id: u8) {
        self.inner.park(id);
    }

    fn register(&self, id: u8, kick: Kick) {
        self.inner.register(id, kick);
    }

    fn unregister(&self, id: u8, kick: &Kick) {
        self.inner.unregister(id, kick);
    }

    fn mmio_read(&self, address: u64, data: &mut [u8]) -> bool {
        let mut ours = [0u8; 8];
        let ours = &mut ours[..data.len()];
        let decoded = self.inner.mmio_read(address, ours);
        let leader = {
            let mut reads = self.reads.lock();
            match reads.front() {
                Some(read) if read.address == address && read.size as usize == data.len() => {
                    reads.pop_front()
                }
                _ => None,
            }
        };
        let mut findings = self.findings.lock();
        let Some(leader) = leader else {
            // The leader read memory here, not a device: a flash bank in
            // its array mode. Our answer stands.
            data.copy_from_slice(ours);
            return decoded;
        };
        findings.reads += 1;
        let bytes = leader.value.to_le_bytes();
        data.copy_from_slice(&bytes[..data.len()]);
        if !decoded {
            findings.note(
                address,
                format!("{address:#x}: the leader has a device and we have none"),
            );
        } else if ours != data {
            findings.note(
                address,
                format!(
                    "{address:#x}: we read {:x?} and the leader {:x?}",
                    ours, data
                ),
            );
        }
        true
    }

    fn mmio_write(&self, address: u64, data: &[u8]) -> bool {
        self.inner.mmio_write(address, data)
    }

    fn hypercall(&self, id: u8, call: &Hypercall) -> HypercallOutcome {
        self.inner.hypercall(id, call)
    }

    fn request_exit(&self, reason: ExitReason) {
        *self.exit.lock() = Some(reason);
    }

    fn request_reset(&self) {
        self.reset.store(true, Ordering::SeqCst);
    }

    fn check(&self) -> Option<ExitReason> {
        self.inner.check()
    }
}

/// The registers of one instruction record.
struct Before {
    encoding: u32,
    core: Core,
    vectors: Vectors,
    system: Vec<u64>,
}

struct Arguments {
    qemu: String,
    plugin: PathBuf,
    directory: PathBuf,
    code: PathBuf,
    vars: PathBuf,
    ram_mib: u32,
    until: String,
    max_instructions: u64,
    resync: u64,
    ignore: Vec<String>,
    all: bool,
    /// Read the stream of the leader and run nothing: how fast it is.
    leader_only: bool,
    keys: Vec<(String, Vec<u8>)>,
}

/// The bytes of a key argument: `\e` is escape and `\r` return.
fn key_bytes(text: &str) -> Vec<u8> {
    text.replace("\\e", "\x1b").replace("\\r", "\r").into_bytes()
}

fn arguments() -> Result<Arguments, Error> {
    let mut parsed = Arguments {
        qemu: "qemu-system-aarch64".into(),
        plugin: PathBuf::new(),
        directory: PathBuf::new(),
        code: PathBuf::new(),
        vars: PathBuf::new(),
        ram_mib: 1024,
        until: "Shell>".into(),
        max_instructions: u64::MAX,
        resync: 0,
        ignore: Vec::new(),
        all: false,
        leader_only: false,
        keys: Vec::new(),
    };
    let mut keys_given = false;
    let mut args = std::env::args().skip(1);
    while let Some(name) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("{name} needs a value"))?;
        match name.as_str() {
            "--qemu" => parsed.qemu = value,
            "--plugin" => parsed.plugin = value.into(),
            "--dir" => parsed.directory = value.into(),
            "--code" => parsed.code = value.into(),
            "--vars" => parsed.vars = value.into(),
            "--ram-mib" => parsed.ram_mib = value.parse()?,
            "--until" => parsed.until = value,
            "--max-instructions" => parsed.max_instructions = value.parse()?,
            "--resync" => parsed.resync = value.parse()?,
            "--ignore" => parsed.ignore.push(value),
            "--system" => parsed.all = value == "all",
            "--leader-only" => parsed.leader_only = value == "1",
            "--key" => {
                keys_given = true;
                // An empty value: the leader gets no key.
                if let Some((text, bytes)) = value.split_once('=') {
                    parsed.keys.push((text.to_string(), key_bytes(bytes)));
                }
            }
            _ => return Err(format!("unknown argument {name}").into()),
        }
    }
    if !keys_given {
        parsed.keys = TO_THE_SHELL
            .iter()
            .map(|(text, bytes)| (text.to_string(), key_bytes(bytes)))
            .collect();
    }
    for (name, path) in [
        ("--plugin", &parsed.plugin),
        ("--dir", &parsed.directory),
        ("--code", &parsed.code),
        ("--vars", &parsed.vars),
    ] {
        if path.as_os_str().is_empty() {
            return Err(format!("give {name}").into());
        }
    }
    Ok(parsed)
}

/// The bytes that a list of writes leaves, by address.
fn written(writes: impl Iterator<Item = (u64, u128, usize)>) -> BTreeMap<u64, u8> {
    let mut bytes = BTreeMap::new();
    for (address, value, size) in writes {
        for (index, byte) in value.to_le_bytes()[..size.min(16)].iter().enumerate() {
            bytes.insert(address.wrapping_add(index as u64), *byte);
        }
    }
    bytes
}

fn name_of(encoding: u32) -> String {
    decode(encoding).map_or_else(
        || "an undefined word".to_string(),
        |decoded| decoded.record.name().to_string(),
    )
}

fn main() -> Result<(), Error> {
    let arguments = arguments()?;
    match run(&arguments) {
        Ok(true) => Ok(()),
        Ok(false) => std::process::exit(1),
        Err(error) => Err(error),
    }
}

/// `true`: the run reached its end without a CPU difference.
fn run(arguments: &Arguments) -> Result<bool, Error> {
    let directory = &arguments.directory;
    // Each side writes its own copy of the variables.
    let leader_vars = directory.join("leader-vars.fd");
    let our_vars = directory.join("follower-vars.fd");
    std::fs::copy(&arguments.vars, &leader_vars)?;
    std::fs::copy(&arguments.vars, &our_vars)?;
    let serial = directory.join("follower-serial.log");
    let _ = std::fs::remove_file(&serial);

    let compared: Vec<_> = compared()
        .into_iter()
        .filter(|register| arguments.all || CORE.contains(&register.qemu))
        .filter(|register| !arguments.ignore.iter().any(|name| name == register.qemu))
        .collect();
    let options = boot::Options {
        qemu: arguments.qemu.clone(),
        plugin: arguments.plugin.clone(),
        directory: directory.clone(),
        cpu: "cortex-a53".into(),
        ram_mib: arguments.ram_mib,
        code: arguments.code.clone(),
        vars: leader_vars,
        system: compared
            .iter()
            .map(|register| register.qemu.to_string())
            .collect(),
    };
    let dtb = boot::dump_device_tree(&options)?;

    let description: MachineDescription = serde_json::from_value(serde_json::json!({
        "name": "lockstep", "arch": "aarch64", "accel": "tcg", "machine": "virt",
        "vcpus": 1, "memory_mib": arguments.ram_mib,
        "boot": {"kind": "firmware", "firmware": "aavmf", "code": arguments.code, "vars": our_vars},
        "disks": [], "nics": [], "ports": [],
        "serial_log": serial,
        // The leader counts its time from here. The clock is read through
        // the device, so the value of the leader is what the guest sees.
        "rtc_base": "2020-01-01T00:00:00Z",
    }))?;
    let host: Arc<Mutex<Option<Arc<LockHost>>>> = Arc::default();
    let slot = host.clone();
    let parts = virt::assemble(
        &description,
        &NoDevices,
        virt::Options {
            dtb: Some(dtb),
            host: Some(Box::new(move |inner| {
                let lock = Arc::new(LockHost {
                    inner,
                    reads: Mutex::default(),
                    findings: Mutex::default(),
                    exit: Mutex::default(),
                    reset: AtomicBool::new(false),
                });
                *slot.lock() = Some(lock.clone());
                lock
            })),
            // The CPU of the leader, with its PMU in the ACPI tables.
            model: Some(&sysreg::QEMU_CORTEX_A53),
            performance_interrupt: Some(virt::map::QEMU_PMU_GSIV),
        },
    )?;
    let host = host.lock().clone().expect("the machine wrapped its host");
    // The registers of the interrupt controller that this thread sees are
    // those of the CPU 0.
    gic::set_current_cpu(0);
    let cell = parts.vcpus[0].clone();
    let mut vcpu = cell.lock();
    vcpu.cpu().clock = Clock::Manual(0);
    vcpu.cpu().log_accesses(true);

    let mut leader = Leader::start(&options)?;
    // The serial port of the leader, for its keys.
    let mut keyboard = {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match std::os::unix::net::UnixStream::connect(options.serial_socket()) {
                Ok(stream) => break stream,
                Err(error) if Instant::now() > deadline => return Err(error.into()),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    };
    keyboard.set_nonblocking(true)?;
    let mut key = 0;
    let mut key_at = 0u64;
    let next_instruction = |leader: &mut Leader,
                            reads: &mut Vec<DeviceRead>,
                            writes: &mut Vec<(u64, u128, usize)>,
                            discontinuities: &mut Vec<(u32, u64, u64)>|
     -> Result<Option<Before>, Error> {
        loop {
            match leader.next(SILENCE)? {
                None => return Ok(None),
                Some(Record::Read(read)) => reads.push(read),
                Some(Record::Trace(Trace::Write {
                    address,
                    size,
                    low,
                    high,
                })) => writes.push((
                    address,
                    u128::from(low) | u128::from(high) << 64,
                    size as usize,
                )),
                Some(Record::Trace(Trace::Discontinuity {
                    kind,
                    from_pc,
                    to_pc,
                })) => discontinuities.push((kind, from_pc, to_pc)),
                Some(Record::Trace(Trace::Instruction {
                    encoding,
                    core,
                    vectors,
                    system,
                })) => {
                    return Ok(Some(Before {
                        encoding,
                        core,
                        vectors: vectors.ok_or("the stream has no vector registers")?,
                        system,
                    }));
                }
            }
        }
    };

    let (mut reads, mut writes, mut discontinuities) = (Vec::new(), Vec::new(), Vec::new());
    let Some(mut current) =
        next_instruction(&mut leader, &mut reads, &mut writes, &mut discontinuities)?
    else {
        return Err("the leader wrote no instruction".into());
    };

    let start = Instant::now();
    let mut instructions = 0u64;
    if arguments.leader_only {
        while instructions < arguments.max_instructions
            && next_instruction(&mut leader, &mut reads, &mut writes, &mut discontinuities)?
                .is_some()
        {
            instructions += 1;
            reads.clear();
            writes.clear();
            discontinuities.clear();
        }
        println!(
            "the leader alone: {instructions} instructions, {:.2} million a second",
            instructions as f64 / start.elapsed().as_secs_f64() / 1e6
        );
        return Ok(true);
    }
    let mut differences = 0u64;
    let mut by_record: BTreeMap<String, u64> = BTreeMap::new();
    let mut interrupts = 0u64;
    let mut interrupts_not_pending = 0u64;
    let mut exceptions = 0u64;
    let mut calls = 0u64;
    let mut call_findings: BTreeMap<String, u64> = BTreeMap::new();
    let mut counters = 0u64;
    let mut timed = 0u64;
    let mut identity: BTreeMap<u16, (u64, u64, u64)> = BTreeMap::new();
    let mut resets = 0u64;
    let mut kept: VecDeque<(u64, u32)> = VecDeque::with_capacity(KEPT);
    let mut last_write_check = true;
    let mut reached = false;
    let mut ended = "the stream of the leader ended";

    'run: loop {
        // The state before the instruction.
        let mut found = {
            let state = &vcpu.cpu().state;
            let mut found = Core::of(state).difference(&current.core);
            found.extend(Vectors::of(state).difference(&current.vectors));
            let mut theirs = current.system.clone();
            normalise(&mut theirs, &compared, &current.core);
            found.extend(system_difference(
                &compared,
                &system_of(state, &compared),
                &theirs,
            ));
            found
        };
        if !last_write_check {
            found.push("the memory writes of the instruction differ (above)".into());
            last_write_check = true;
        }
        if !found.is_empty() {
            differences += 1;
            let (pc, encoding) = kept.back().copied().unwrap_or((0, 0));
            *by_record.entry(name_of(encoding)).or_default() += 1;
            println!(
                "CPU difference {differences} after {instructions} instructions, after {} ({encoding:08x}) at {pc:#x}; ours / leader:",
                name_of(encoding)
            );
            for line in &found {
                println!("  {line}");
            }
            let mut text = String::new();
            for (pc, encoding) in &kept {
                text.push_str(&format!("{pc:#x} {encoding:08x} {}\n", name_of(*encoding)));
            }
            std::fs::write(directory.join("last-instructions.txt"), text)?;
            if differences > arguments.resync {
                ended = "a CPU difference";
                break 'run;
            }
            let state = &mut vcpu.cpu().state;
            if !current.core.apply(state) {
                ended = "a PSTATE of the leader that our CPU does not have";
                break 'run;
            }
            current.vectors.apply(state);
            let mut theirs = current.system.clone();
            normalise(&mut theirs, &compared, &current.core);
            apply_system(state, &compared, &theirs);
        }
        if kept.len() == KEPT {
            kept.pop_front();
        }
        kept.push_back((current.core.pc, current.encoding));

        // What the leader did in this instruction, up to its next one.
        reads.clear();
        writes.clear();
        discontinuities.clear();
        let next = next_instruction(&mut leader, &mut reads, &mut writes, &mut discontinuities)?;
        if !reads.is_empty() {
            *host.reads.lock() = reads.iter().copied().collect();
        }

        // Our instruction.
        let cpu = vcpu.cpu();
        cpu.irq = false;
        cpu.fiq = false;
        match cpu.step() {
            Ok(()) | Err(Stop::Halt) => {}
            Err(Stop::Hypercall {
                conduit,
                immediate,
                pc,
            }) => {
                calls += 1;
                let call = Hypercall {
                    conduit: match conduit {
                        vmlab_hv_tcg_aarch64::Conduit::Hvc => Conduit::Hvc,
                        vmlab_hv_tcg_aarch64::Conduit::Smc => Conduit::Smc,
                    },
                    immediate,
                    arguments: std::array::from_fn(|n| cpu.state.x(n)),
                };
                match host.hypercall(0, &call) {
                    HypercallOutcome::Return(values) => {
                        for (n, value) in values.into_iter().enumerate() {
                            cpu.state.set_x(n, value);
                        }
                    }
                    HypercallOutcome::Refused => cpu.refuse_hypercall(pc),
                }
            }
            Err(Stop::Unimplemented { pc, record }) => {
                println!("our CPU has no instruction {record:?} yet, at {pc:#x}");
                differences += 1;
                ended = "an instruction that our CPU does not have";
                break 'run;
            }
            Err(stop) => return Err(format!("our CPU stopped: {stop:?}").into()),
        }
        if cpu.taken.is_some() {
            exceptions += 1;
        }
        instructions += 1;
        let left = if reads.is_empty() {
            VecDeque::new()
        } else {
            std::mem::take(&mut *host.reads.lock())
        };
        if let Some(read) = left.front() {
            host.findings.lock().note(
                read.address,
                format!(
                    "{:#x}: the leader read a device in {} at {:#x} and we did not",
                    read.address,
                    name_of(current.encoding),
                    current.core.pc
                ),
            );
        }

        // The memory writes. The leader does not report every kind of
        // write (`DC ZVA`), so only an instruction that it reports is
        // compared.
        let accesses: Vec<Access> = cpu.take_accesses();
        let same = accesses
            .iter()
            .filter(|access| access.write)
            .map(|access| (access.address, access.value, usize::from(access.bytes)))
            .eq(writes.iter().copied());
        if !writes.is_empty() && !same {
            let ours = written(
                accesses
                    .iter()
                    .filter(|access| access.write)
                    .map(|access| (access.address, access.value, usize::from(access.bytes))),
            );
            let theirs = written(writes.iter().copied());
            if ours != theirs {
                println!(
                    "memory writes of {} ({:08x}) at {:#x} differ: ours {:x?}, the leader {:x?}",
                    name_of(current.encoding),
                    current.encoding,
                    current.core.pc,
                    ours,
                    theirs
                );
                last_write_check = false;
            }
        }

        // The interrupts that the leader took after the instruction.
        for (kind, from, to) in discontinuities.iter().copied() {
            if kind != 1 {
                continue;
            }
            interrupts += 1;
            vcpu.drive_timer();
            let lines = vcpu.lines();
            let fiq = to & 0x180 == 0x100;
            let line = if fiq { &lines.fiq } else { &lines.irq };
            if !line.level() {
                // The timer of the leader fired: bring our clock to the
                // deadline of ours.
                if let Some(deadline) = vcpu.cpu().timer_deadline() {
                    vcpu.cpu().clock = Clock::Manual(deadline);
                    vcpu.drive_timer();
                }
            }
            if !line.level() {
                interrupts_not_pending += 1;
                if interrupts_not_pending <= 8 {
                    println!(
                        "the leader took an interrupt at {from:#x} that our interrupt controller does not have pending"
                    );
                }
            }
            let cpu = vcpu.cpu();
            cpu.irq = !fiq;
            cpu.fiq = fiq;
            let stepped = cpu.step();
            cpu.irq = false;
            cpu.fiq = false;
            let _ = cpu.take_accesses();
            if stepped.is_err() || cpu.taken.is_none_or(|taken| taken.to != to) {
                println!(
                    "the leader took an interrupt from {from:#x} to {to:#x}, and our CPU did not: {:?}, PSTATE {:#x}",
                    cpu.taken,
                    cpu.state.cpsr()
                );
            }
        }

        // The machine ended, or reset.
        if host.exit.lock().is_some() {
            ended = "the guest powered the machine off";
            break 'run;
        }
        if host.reset.swap(false, Ordering::SeqCst) {
            resets += 1;
            // The reset takes each vCPU.
            drop(vcpu);
            parts.reset().map_err(|e| format!("reset: {e}"))?;
            vcpu = cell.lock();
            vcpu.cpu().clock = Clock::Manual(0);
        }

        let Some(next) = next else {
            break 'run;
        };

        // What the leader leads.
        let cpu = vcpu.cpu();
        match boot::adoption(current.encoding) {
            Some(Adopt::Counter(rt)) => {
                counters += 1;
                cpu.state.set_x(rt, next.core.x[rt]);
                cpu.clock = Clock::Manual(next.core.x[rt]);
            }
            Some(Adopt::Timed(rt)) => {
                if cpu.state.x(rt) != next.core.x[rt] {
                    timed += 1;
                    cpu.state.set_x(rt, next.core.x[rt]);
                }
            }
            Some(Adopt::Identity(rt, encoding)) => {
                let (ours, theirs) = (cpu.state.x(rt), next.core.x[rt]);
                if ours != theirs && cpu.taken.is_none() {
                    identity.entry(encoding).or_insert((ours, theirs, 0)).2 += 1;
                    cpu.state.set_x(rt, theirs);
                }
            }
            Some(Adopt::Call) => {
                let function = current.core.x[0];
                for n in 0..4 {
                    let (ours, theirs) = (cpu.state.x(n), next.core.x[n]);
                    if ours != theirs && cpu.state.pc() == next.core.pc {
                        *call_findings
                            .entry(format!(
                                "call {function:#x}: we answer x{n}={ours:#x} and the leader {theirs:#x}"
                            ))
                            .or_default() += 1;
                        cpu.state.set_x(n, theirs);
                    }
                }
            }
            None => {}
        }
        current = next;

        if instructions.is_multiple_of(1 << 16) {
            // What the leader writes to its serial port is in its log
            // file. The socket is emptied, so that the leader never waits
            // for a reader.
            {
                use std::io::Read;
                let mut sink = [0u8; 4096];
                while matches!(keyboard.read(&mut sink), Ok(count) if count > 0) {}
            }
            if let Some((text, bytes)) = arguments.keys.get(key)
                && instructions >= key_at + KEY_GAP
                && String::from_utf8_lossy(&std::fs::read(options.serial()).unwrap_or_default())
                    .contains(text.as_str())
            {
                use std::io::Write;
                keyboard.write_all(bytes)?;
                println!(
                    "key {} of {} to the leader after {instructions} instructions",
                    key + 1,
                    arguments.keys.len()
                );
                key += 1;
                key_at = instructions;
            }
            let log = std::fs::read(&description.serial_log).unwrap_or_default();
            if String::from_utf8_lossy(&log).contains(&arguments.until) {
                reached = true;
                ended = "our serial log holds the text";
                break 'run;
            }
            if instructions.is_multiple_of(1 << 26) {
                println!(
                    "{instructions} instructions, {:.2} million a second, {} bytes on our serial log",
                    instructions as f64 / start.elapsed().as_secs_f64() / 1e6,
                    log.len()
                );
            }
        }
        if instructions >= arguments.max_instructions {
            ended = "the instruction limit";
            break 'run;
        }
    }

    let seconds = start.elapsed().as_secs_f64();
    println!(
        "ended: {ended}, after {instructions} instructions in {seconds:.0} s ({:.2} million a second)",
        instructions as f64 / seconds / 1e6
    );
    println!(
        "CPU differences: {differences}. Exceptions {exceptions}, interrupts {interrupts} ({interrupts_not_pending} not pending in our controller), host calls {calls}, resets {resets}."
    );
    for (record, count) in &by_record {
        println!("  differences after {record}: {count}");
    }
    println!(
        "Adopted from the leader: {counters} counter reads, {timed} timer and interrupt status reads that differed."
    );
    println!("ID registers that differ from the leader (the allow-list):");
    for (encoding, (ours, theirs, count)) in &identity {
        println!(
            "  {}: ours {ours:#x}, the leader {theirs:#x}, {count} reads",
            boot::register_name(*encoding)
        );
    }
    println!("Machine findings (the answer to a host call):");
    for (what, count) in &call_findings {
        println!("  {what} ({count} times)");
    }
    let findings = host.findings.lock();
    println!(
        "Device reads taken from the leader: {}. Device findings, by page ({} pages):",
        findings.reads,
        findings.pages.len()
    );
    for (page, (count, example)) in &findings.pages {
        println!("  page {:#x}: {count} reads, the first: {example}", page << 12);
    }
    let log = std::fs::read(&description.serial_log).unwrap_or_default();
    let text = String::from_utf8_lossy(&log);
    let tail = text.len().saturating_sub(400);
    println!(
        "Our serial log, {} bytes, its end:\n{}",
        log.len(),
        &text[text.ceil_char_boundary(tail)..]
    );
    if !reached && arguments.max_instructions == u64::MAX && differences == 0 {
        println!("the run did not reach {:?}", arguments.until);
        return Ok(false);
    }
    Ok(differences == 0)
}
