//! The windows of a boot against QEMU (card C13b, #1144; `oracle::window`).
//! `just tcg-a64-window-lockstep` in `hv/` builds the plugin and runs this.
//!
//! Usage:
//!   a64_window_lockstep --plugin <a64lock.so> --dir <scratch directory>
//!       --windows <directory of VMLAB_HV_A64_WINDOWS> --ram-mib N
//!       [--qemu <binary>] [--budget N] [--ignore <register>]...
//!       [--system core|all] [--keep 0|1]
//!
//! For every `window-<n>.txt` of the directory, in the order of n: QEMU
//! starts at EL2 from the prelude of the window with our RAM, and our CPU
//! starts from the same state with the same RAM. Before every instruction
//! the follower compares X0 to X30, SP, PC, PSTATE, V0 to V31, FPSR, FPCR
//! and the system registers, and after it the memory writes, as the boot
//! lockstep does. The leader leads the counter, the timer status and the ID
//! registers that differ by decision (G2).
//!
//! A window ends at `--budget` instructions (default 20 million), at the
//! first access outside RAM, at `WFI`, at `HVC` or `SMC`, at an interrupt
//! of the leader, or at the first CPU difference. The run goes on with the
//! next window after a difference, and exits with 1 when any window had
//! one. Without `--keep 1` each RAM file is removed after its window.
//!
//! The CPU runs the JIT unless `VMLAB_HV_JIT=0` (3e DESIGN.md K4): each
//! instruction is a compiled block of one.
//!
//! QEMU is the test oracle only (S6). Nothing here is a product path.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vmlab_hv_tcg::FlatMemory;
use vmlab_hv_tcg_aarch64::exec::{Access, machine_jit_from_environment};
use vmlab_hv_tcg_aarch64::oracle::boot::{self, Adopt, DeviceRead, Leader, Record, WindowImage};
use vmlab_hv_tcg_aarch64::oracle::window::{self, Start};
use vmlab_hv_tcg_aarch64::oracle::{
    Core, Trace, Vectors, compared, normalise, system_difference, system_of,
};
use vmlab_hv_tcg_aarch64::sysreg::reg;
use vmlab_hv_tcg_aarch64::timer::Clock;
use vmlab_hv_tcg_aarch64::{Cpu, Stop, decode};

type Error = Box<dyn std::error::Error>;

/// RAM of the `virt` machine starts here.
const RAM_BASE: u64 = 0x4000_0000;
/// The system registers of every run, as in the boot lockstep.
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
/// The instructions that the leader may run before the first one of the
/// window: the prelude, with its copy of the first MiB of RAM in four
/// instructions for every eight bytes.
const PRELUDE_LIMIT: u64 = 4 * window::LOW / 8 + 10_000;
const SILENCE: Duration = Duration::from_secs(60);
const KEPT: usize = 256;

struct Arguments {
    qemu: String,
    plugin: PathBuf,
    directory: PathBuf,
    windows: PathBuf,
    ram_mib: u32,
    budget: u64,
    ignore: Vec<String>,
    all: bool,
    keep: bool,
}

fn arguments() -> Result<Arguments, Error> {
    let mut parsed = Arguments {
        qemu: "qemu-system-aarch64".into(),
        plugin: PathBuf::new(),
        directory: PathBuf::new(),
        windows: PathBuf::new(),
        ram_mib: 0,
        budget: 20_000_000,
        ignore: Vec::new(),
        all: false,
        keep: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(name) = args.next() {
        let value = args.next().ok_or_else(|| format!("{name} needs a value"))?;
        match name.as_str() {
            "--qemu" => parsed.qemu = value,
            "--plugin" => parsed.plugin = value.into(),
            "--dir" => parsed.directory = value.into(),
            "--windows" => parsed.windows = value.into(),
            "--ram-mib" => parsed.ram_mib = value.parse()?,
            "--budget" => parsed.budget = value.parse()?,
            "--ignore" => parsed.ignore.push(value),
            "--system" => parsed.all = value == "all",
            "--keep" => parsed.keep = value == "1",
            _ => return Err(format!("unknown argument {name}").into()),
        }
    }
    for (name, path) in [
        ("--plugin", &parsed.plugin),
        ("--dir", &parsed.directory),
        ("--windows", &parsed.windows),
    ] {
        if path.as_os_str().is_empty() {
            return Err(format!("give {name}").into());
        }
    }
    if parsed.ram_mib == 0 {
        return Err("give --ram-mib, the RAM of the machine that wrote the windows".into());
    }
    Ok(parsed)
}

fn name_of(encoding: u32) -> String {
    decode(encoding).map_or_else(
        || "an undefined word".to_string(),
        |decoded| decoded.record.name().to_string(),
    )
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

/// What one window gave.
#[derive(Default)]
struct Outcome {
    instructions: u64,
    differences: u64,
    exceptions: u64,
    /// Exceptions that the leader took at the fetch of an instruction.
    fetch_aborts: u64,
    counters: u64,
    timed: u64,
    identity: BTreeMap<String, u64>,
    ended: String,
}

/// The registers of one instruction record.
struct Before {
    encoding: u32,
    core: Core,
    vectors: Vectors,
    system: Vec<u64>,
}

/// The next instruction of the leader, with what it read and wrote and the
/// discontinuities after the one before.
fn next_instruction(
    leader: &mut Leader,
    reads: &mut Vec<DeviceRead>,
    writes: &mut Vec<(u64, u128, usize)>,
    discontinuities: &mut Vec<(u32, u64, u64)>,
) -> Result<Option<Before>, Error> {
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
}

/// Whether the exception that our CPU just took is an external abort: an
/// access outside our RAM, where the leader has a device or nothing.
fn external_abort(cpu: &Cpu<FlatMemory>) -> bool {
    if cpu.taken.is_none() {
        return false;
    }
    let esr = cpu.state.system.regs[reg::ESR_EL1];
    let class = esr >> 26 & 0x3f;
    let status = esr & 0x3f;
    matches!(class, 0x20 | 0x21 | 0x24 | 0x25) && (0x10..=0x17).contains(&status)
}

/// The descriptors of the walk of `va` in our memory, for the 4K granule.
fn walk(cpu: &mut Cpu<FlatMemory>, va: u64) -> Vec<(u32, u64, u64)> {
    use vmlab_hv_tcg::Memory;
    let regs = &cpu.state.system.regs;
    let tcr = regs[reg::TCR_EL1];
    let (table, size) = if va >> 63 == 0 {
        (regs[reg::TTBR0_EL1], tcr & 63)
    } else {
        (regs[reg::TTBR1_EL1], tcr >> 16 & 63)
    };
    let bits = 64 - size;
    let mut level = 4 - (bits - 12).div_ceil(9) as u32;
    let mut table = table & 0x0000_ffff_ffff_fffe;
    let mut found = Vec::new();
    while level <= 3 {
        let shift = 12 + 9 * (3 - level);
        let index = va >> shift & 0x1ff;
        let at = table + index * 8;
        let mut bytes = [0u8; 8];
        if cpu.memory.read(at, &mut bytes).is_err() {
            break;
        }
        let descriptor = u64::from_le_bytes(bytes);
        found.push((level, at, descriptor));
        if descriptor & 1 == 0 || (level < 3 && descriptor & 2 == 0) {
            break;
        }
        table = descriptor & 0x0000_ffff_ffff_f000;
        level += 1;
    }
    found
}

/// Write `bytes` to `path` with a hole for every page of zeros.
fn write_sparse(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::create(path)?;
    file.set_len(bytes.len() as u64)?;
    for (index, page) in bytes.chunks(4096).enumerate() {
        if page.iter().any(|byte| *byte != 0) {
            file.write_all_at(page, index as u64 * 4096)?;
        }
    }
    Ok(())
}

fn run_window(
    arguments: &Arguments,
    number: u32,
    start: &Start,
    ram: &Path,
) -> Result<Outcome, Error> {
    let directory = &arguments.directory;
    let list: Vec<_> = compared()
        .into_iter()
        .filter(|register| arguments.all || CORE.contains(&register.qemu))
        .filter(|register| !arguments.ignore.iter().any(|name| name == register.qemu))
        .collect();
    let mut data = std::fs::read(ram)?;
    data.resize(arguments.ram_mib as usize * 1024 * 1024, 0);
    // QEMU puts its device tree into the first bytes of RAM: the prelude
    // copies ours there, and the leader loads the rest as a file.
    let low = window::LOW as usize;
    let prelude = directory.join("prelude.bin");
    std::fs::write(
        &prelude,
        window::prelude(start, &data[..low]).ok_or("the window file misses a register")?,
    )?;
    let high = directory.join("ram-high.bin");
    write_sparse(&high, &data[low..])?;
    let options = boot::Options {
        qemu: arguments.qemu.clone(),
        plugin: arguments.plugin.clone(),
        directory: directory.clone(),
        cpu: "cortex-a53".into(),
        ram_mib: arguments.ram_mib,
        code: PathBuf::new(),
        vars: PathBuf::new(),
        system: list
            .iter()
            .map(|register| register.qemu.to_string())
            .collect(),
        window: Some(WindowImage {
            ram: high.clone(),
            prelude,
        }),
    };
    let state = start.state().ok_or("the window has no state of our CPU")?;
    let mut cpu = Cpu::new(FlatMemory::new(RAM_BASE, data), state);
    if machine_jit_from_environment() && !cpu.jit_enabled() {
        cpu.set_jit(true)?;
    }
    cpu.clock = Clock::Manual(start.counter);
    cpu.log_accesses(true);

    let mut leader = Leader::start(&options)?;
    let (mut reads, mut writes, mut discontinuities) = (Vec::new(), Vec::new(), Vec::new());
    // The prelude of the leader, up to the first instruction of the window.
    let mut skipped = 0u64;
    let mut current = loop {
        let Some(record) =
            next_instruction(&mut leader, &mut reads, &mut writes, &mut discontinuities)?
        else {
            return Err(format!("window {number}: the leader ended in its prelude").into());
        };
        if record.core.pc == start.core.pc && record.core.cpsr >> 2 & 3 == start.el() {
            break record;
        }
        skipped += 1;
        if skipped > PRELUDE_LIMIT {
            return Err(format!(
                "window {number}: the leader did not reach {:#x} after {skipped} instructions, it is at {:#x} with PSTATE {:#x}",
                start.core.pc, record.core.pc, record.core.cpsr
            )
            .into());
        }
        reads.clear();
        writes.clear();
        discontinuities.clear();
    };

    let mut outcome = Outcome::default();
    let mut kept: VecDeque<(u64, u32)> = VecDeque::with_capacity(KEPT);
    let mut last_write_check = true;
    loop {
        let mut found = {
            let state = &cpu.state;
            let mut found = Core::of(state).difference(&current.core);
            found.extend(Vectors::of(state).difference(&current.vectors));
            let mut theirs = current.system.clone();
            normalise(&mut theirs, &list, &current.core);
            found.extend(system_difference(&list, &system_of(state, &list), &theirs));
            found
        };
        if !last_write_check {
            found.push("the memory writes of the instruction differ (above)".into());
        }
        if !found.is_empty() {
            outcome.differences += 1;
            let (pc, encoding) = kept.back().copied().unwrap_or((0, 0));
            println!(
                "window {number}: CPU difference after {} instructions of the window ({} of the boot), after {} ({encoding:08x}) at {pc:#x}; ours / leader:",
                outcome.instructions,
                start.instructions + outcome.instructions,
                name_of(encoding)
            );
            for line in &found {
                println!("  {line}");
            }
            // Where our tables put the PC of each side, for a difference of
            // a translation.
            for (side, address) in [("ours", cpu.state.pc()), ("the leader's", current.core.pc)] {
                println!("  the walk of {side} PC {address:#x} in our RAM:");
                for (level, at, descriptor) in walk(&mut cpu, address) {
                    println!("    level {level}: {descriptor:#018x} at {at:#x}");
                }
            }
            let mut text = String::new();
            for (pc, encoding) in &kept {
                text.push_str(&format!("{pc:#x} {encoding:08x} {}\n", name_of(*encoding)));
            }
            std::fs::write(
                directory.join(format!("window-{number}-last-instructions.txt")),
                text,
            )?;
            outcome.ended = "a CPU difference".into();
            return Ok(outcome);
        }
        if kept.len() == KEPT {
            kept.pop_front();
        }
        kept.push_back((current.core.pc, current.encoding));
        if outcome.instructions >= arguments.budget {
            outcome.ended = "the budget".into();
            return Ok(outcome);
        }

        reads.clear();
        writes.clear();
        discontinuities.clear();
        let next = next_instruction(&mut leader, &mut reads, &mut writes, &mut discontinuities)?;

        match cpu.step() {
            Ok(()) => {}
            Err(Stop::Halt) => {
                outcome.ended = "WFI".into();
                return Ok(outcome);
            }
            Err(Stop::Hypercall { .. }) => {
                outcome.ended = "a host call".into();
                return Ok(outcome);
            }
            Err(Stop::Unimplemented { pc, record }) => {
                println!("window {number}: our CPU has no instruction {record:?} yet, at {pc:#x}");
                outcome.differences += 1;
                outcome.ended = "an instruction that our CPU does not have".into();
                return Ok(outcome);
            }
            Err(stop) => return Err(format!("our CPU stopped: {stop:?}").into()),
        }
        if external_abort(&cpu) {
            outcome.ended = format!(
                "an access outside RAM by {} at {:#x}",
                name_of(current.encoding),
                current.core.pc
            );
            return Ok(outcome);
        }
        if cpu.taken.is_some() {
            outcome.exceptions += 1;
        }
        outcome.instructions += 1;
        if discontinuities.iter().any(|(kind, _, _)| *kind == 1) {
            outcome.ended = "an interrupt of the leader".into();
            return Ok(outcome);
        }
        if !reads.is_empty() {
            outcome.ended = format!(
                "a device read of the leader at {:#x} that our RAM serves",
                reads[0].address
            );
            return Ok(outcome);
        }

        // The memory writes, as the boot lockstep compares them.
        let accesses: Vec<Access> = cpu.take_accesses();
        let ours = || {
            accesses
                .iter()
                .filter(|access| access.write)
                .map(|access| (access.address, access.value, usize::from(access.bytes)))
        };
        if !writes.is_empty() && !ours().eq(writes.iter().copied()) {
            let (mine, theirs) = (written(ours()), written(writes.iter().copied()));
            if mine != theirs {
                println!(
                    "window {number}: memory writes of {} ({:08x}) at {:#x} differ: ours {mine:x?}, the leader {theirs:x?}",
                    name_of(current.encoding),
                    current.encoding,
                    current.core.pc
                );
                last_write_check = false;
            }
        }

        let Some(next) = next else {
            outcome.ended = "the stream of the leader ended".into();
            return Ok(outcome);
        };
        match boot::adoption(current.encoding) {
            Some(Adopt::Counter(rt)) => {
                outcome.counters += 1;
                cpu.state.set_x(rt, next.core.x[rt]);
                cpu.clock = Clock::Manual(next.core.x[rt]);
            }
            Some(Adopt::Timed(rt)) => {
                if cpu.state.x(rt) != next.core.x[rt] {
                    outcome.timed += 1;
                    cpu.state.set_x(rt, next.core.x[rt]);
                }
            }
            Some(Adopt::Identity(rt, encoding)) => {
                let (ours, theirs) = (cpu.state.x(rt), next.core.x[rt]);
                if ours != theirs && cpu.taken.is_none() {
                    *outcome
                        .identity
                        .entry(boot::register_name(encoding))
                        .or_default() += 1;
                    cpu.state.set_x(rt, theirs);
                }
            }
            Some(Adopt::Call) | None => {}
        }
        // The leader took an exception at the fetch of the next instruction
        // (an instruction abort): it has no record of its own, and the next
        // record is at the vector. Our CPU takes it in its next step.
        if cpu.taken.is_none()
            && cpu.state.pc() != next.core.pc
            && discontinuities
                .iter()
                .any(|(kind, from, _)| *kind == 2 && *from == cpu.state.pc())
        {
            outcome.fetch_aborts += 1;
            if cpu.step().is_ok() && external_abort(&cpu) {
                outcome.ended = "a fetch outside RAM".into();
                return Ok(outcome);
            }
            if cpu.taken.is_some() {
                outcome.exceptions += 1;
            }
            let _ = cpu.take_accesses();
        }
        current = next;
    }
}

fn main() -> Result<(), Error> {
    let arguments = arguments()?;
    let mut numbers: Vec<u32> = std::fs::read_dir(&arguments.windows)?
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.strip_prefix("window-")?
                .strip_suffix(".txt")?
                .parse()
                .ok()
        })
        .collect();
    numbers.sort_unstable();
    if numbers.is_empty() {
        return Err("the directory holds no window".into());
    }
    let begun = Instant::now();
    let (mut total, mut differences, mut failed) = (0u64, 0u64, 0u32);
    let mut ends: BTreeMap<String, u32> = BTreeMap::new();
    let mut identity: BTreeMap<String, u64> = BTreeMap::new();
    let mut counters = 0u64;
    for number in &numbers {
        let text = std::fs::read_to_string(arguments.windows.join(format!("window-{number}.txt")))?;
        let start = Start::parse(&text)?;
        let ram = arguments.windows.join(format!("window-{number}.ram"));
        let clock = Instant::now();
        match run_window(&arguments, *number, &start, &ram) {
            Ok(outcome) => {
                println!(
                    "window {number}: {} instructions of the boot, EL{} at {:#x}: {} instructions compared in {:.1} s, {} exceptions ({} at a fetch), {} counter reads; ended: {}",
                    start.instructions,
                    start.el(),
                    start.core.pc,
                    outcome.instructions,
                    clock.elapsed().as_secs_f64(),
                    outcome.exceptions,
                    outcome.fetch_aborts,
                    outcome.counters,
                    outcome.ended
                );
                total += outcome.instructions;
                differences += outcome.differences;
                counters += outcome.counters + outcome.timed;
                let kind = outcome
                    .ended
                    .split(" by ")
                    .next()
                    .unwrap_or_default()
                    .split(" at 0x")
                    .next()
                    .unwrap_or_default()
                    .to_string();
                *ends.entry(kind).or_default() += 1;
                for (name, count) in outcome.identity {
                    *identity.entry(name).or_default() += count;
                }
            }
            Err(error) => {
                failed += 1;
                println!("window {number}: did not run: {error}");
            }
        }
        if !arguments.keep {
            let _ = std::fs::remove_file(&ram);
        }
    }
    let jit = if machine_jit_from_environment() {
        "on"
    } else {
        "off"
    };
    println!(
        "windows: {}, {} of them run; {total} instructions compared in {:.0} s; CPU differences: {differences}; JIT {jit}; adopted from the leader: {counters} counter and timer reads",
        numbers.len(),
        numbers.len() as u32 - failed,
        begun.elapsed().as_secs_f64()
    );
    for (kind, count) in &ends {
        println!("  ended at {kind}: {count}");
    }
    for (name, count) in &identity {
        println!(
            "  ID register that differs from the leader (the allow-list): {name}, {count} reads"
        );
    }
    if differences > 0 || failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
