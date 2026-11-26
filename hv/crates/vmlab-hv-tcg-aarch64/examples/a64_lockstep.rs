//! The instruction lockstep against QEMU for a group of format classes
//! (DESIGN.md O3). `just tcg-a64-lockstep` in `hv/` builds the plugin and
//! runs this.
//!
//! Usage:
//!   a64_lockstep --plugin <a64lock.so> --dir <scratch directory>
//!       [--qemu <binary>] [--seed S] [--per-class N] [--group NAME]
//!       [--encodings <trace.txt>]... [--corpus-out <file>] [--samples N]
//!       [--probe <hex word>,<hex word>...] [--probe-offset <hex>]
//!
//! Two phases. The random phase runs rounds of random instructions until
//! each format class of the group has `--per-class` instructions that both
//! sides ran from the same registers to the same registers. The executed
//! phase runs each distinct encoding of the group from the traces of the
//! template boots (`measure/a64trace.c` of the design card). The first
//! difference stops the run with exit code 1.
//!
//! The group `loadstore` runs another way, because its instructions work on
//! memory and on the vector registers. QEMU first runs a prelude that turns
//! the FP and vector registers on and the stack pointer alignment check
//! off. The plugin gives the vector registers with each record. A round is
//! a short straight run of instructions on registers that point into a
//! window of data, and it ends at the first access that the memory does not
//! hold, or at the zero word behind the image. The memory writes and the
//! vector registers are compared after every instruction.
//!
//! `--probe` runs each given word alone, as the executed phase of the group
//! does, and prints how its round ended. Words joined by `+` run one after
//! the other in one round, with the registers made for the first of them. It is for a question to QEMU about
//! one encoding, and it compares nothing else. `--probe-offset` moves the
//! base register and the stack pointer of a probe by that many bytes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;
use vmlab_hv_tcg::FlatMemory;
use vmlab_hv_tcg_aarch64::lift::{LIFTED_FORMATS, LOAD_STORE_FORMATS};
use vmlab_hv_tcg_aarch64::oracle::corpus::Entry;
use vmlab_hv_tcg_aarch64::oracle::follow::{End, Follower, Outcome, Round, Verified};
use vmlab_hv_tcg_aarch64::oracle::leader::{Leader, RAM_BASE};
use vmlab_hv_tcg_aarch64::oracle::stream::{Generator, Rng, executed_encodings, records_of};
use vmlab_hv_tcg_aarch64::oracle::{CPSR_EL1H, Core};
use vmlab_hv_tcg_aarch64::{Cpu, FormatClass, Record, State, decode};

type Failure = Box<dyn std::error::Error>;

const RAM_MIB: u32 = 64;
/// The image sits in the middle of the RAM, so that a branch of up to 32 MiB
/// in either direction stays in memory.
const IMAGE_BASE: u64 = RAM_BASE + 32 * 1024 * 1024;
/// Instructions in the image of a random round.
const IMAGE_WORDS: usize = 1024;
/// Instructions in the image of a random round of loads and stores. A
/// round ends at the first access outside the memory, so a long image is
/// made for nothing.
const MEMORY_IMAGE_WORDS: usize = 48;
/// The data that the loads and stores work on: 64 KiB, 1 MiB above the
/// image. Every offset of an instruction from a pointer into it stays in
/// the RAM.
const WINDOW: (u64, u64) = (IMAGE_BASE + 0x10_0000, 0x1_0000);

/// What QEMU runs before the first round of loads and stores, which the
/// gdbstub cannot set: `CPACR_EL1.FPEN` on, so that an FP or vector
/// instruction does not trap, and `SCTLR_EL1.SA` and `SA0` off, so that a
/// stack pointer that is not a multiple of 16 is a base like another. Our
/// side has neither register yet (card C6) and behaves that way.
const PRELUDE: [u32; 7] = [
    0xd2a0_0600, // movz x0, #0x30, lsl #16
    0xd518_1040, // msr cpacr_el1, x0
    0xd538_1000, // mrs x0, sctlr_el1
    0xd280_0301, // movz x1, #0x18
    0x8a21_0000, // bic x0, x0, x1
    0xd518_1000, // msr sctlr_el1, x0
    0xd503_3fdf, // isb
];

fn group(name: &str) -> Result<Vec<FormatClass>, Failure> {
    use FormatClass::*;
    let immediate = [
        ADRI,
        AddSubImmShift,
        BaseLogicalImm,
        BaseMoveImmediate,
        BaseInsertImmediate,
        BaseBitfieldImm,
        BaseBitfieldImmWith2RegArgs,
        BaseExtractImm,
    ];
    let branch = [
        BranchImm,
        CallImm,
        BranchCond,
        BaseCmpBranch,
        BaseTestBranch,
        BranchReg,
    ];
    Ok(match name {
        "dp-immediate" => immediate.to_vec(),
        "branch" => branch.to_vec(),
        "c1" => LIFTED_FORMATS.to_vec(),
        "loadstore" => LOAD_STORE_FORMATS.to_vec(),
        other => {
            return Err(format!(
                "the group {other} is not known: dp-immediate, branch, c1, loadstore"
            )
            .into());
        }
    })
}

#[derive(Default)]
struct Counts {
    rounds: u64,
    verified: BTreeMap<Record, u64>,
    /// The distinct encodings among the verified instructions.
    distinct: BTreeMap<Record, std::collections::HashSet<u32>>,
    ends: BTreeMap<String, u64>,
    samples: BTreeMap<Record, Vec<Entry>>,
    /// Corpus entries kept per record.
    keep: usize,
    /// Exclusive stores that wrote, and that did not.
    exclusive_wrote: u64,
    exclusive_failed: u64,
    /// Verified instructions that wrote memory, and the bytes.
    stores: u64,
    stored_bytes: u64,
}

/// The corpus entry of a verified instruction.
fn entry(item: &Verified) -> Entry {
    let mut entry = Entry::core(item.encoding, item.before, item.after);
    for access in &item.accesses {
        let bytes = access.value.to_le_bytes()[..usize::from(access.bytes)].to_vec();
        if access.write {
            entry.writes.push((access.address, bytes));
        } else {
            entry.reads.push((access.address, bytes));
        }
    }
    if let Some((before, after)) = &item.vectors {
        // The vector registers that the instruction can name, and every one
        // that it changed. A replay starts the others at zero.
        let has_field = |name: &str| vmlab_hv_tcg_aarch64::field(item.record, name, 0).is_some();
        let vector_data = has_field("vt") || item.encoding & (1 << 26) != 0;
        let first = item.encoding & 31;
        let second = (item.encoding >> 10) & 31;
        for n in 0..32u32 {
            let named = vector_data && ((n.wrapping_sub(first) & 31) < 4 || n == second);
            let (was, is) = (before.v[n as usize], after.v[n as usize]);
            if was != is || (named && was != 0) {
                entry.vectors.push((n as u8, was, is));
            }
        }
    }
    entry
}

fn is_exclusive_store(record: Record) -> bool {
    matches!(
        record.info().format,
        FormatClass::StoreExclusive | FormatClass::StoreExclusivePair
    )
}

impl Counts {
    fn add(&mut self, rng: &mut Rng, verified: &[Verified]) {
        for item in verified {
            self.distinct
                .entry(item.record)
                .or_default()
                .insert(item.encoding);
            let count = self.verified.entry(item.record).or_default();
            *count += 1;
            let wrote: u64 = item
                .accesses
                .iter()
                .filter(|access| access.write)
                .map(|access| u64::from(access.bytes))
                .sum();
            self.stores += u64::from(wrote != 0);
            self.stored_bytes += wrote;
            if is_exclusive_store(item.record) {
                if wrote != 0 {
                    self.exclusive_wrote += 1;
                    // The store wrote because of the exclusive load before
                    // it. Alone, as the corpus replays it, it does not.
                    continue;
                }
                self.exclusive_failed += 1;
            }
            // Reservoir sampling: each instruction of a record has the same
            // chance to be one of its samples.
            let samples = self.samples.entry(item.record).or_default();
            if samples.len() < self.keep {
                samples.push(entry(item));
            } else {
                let slot = rng.below(*count) as usize;
                if slot < self.keep {
                    samples[slot] = entry(item);
                }
            }
        }
    }

    fn of_format(&self, format: FormatClass) -> u64 {
        records_of(format)
            .iter()
            .map(|record| self.verified.get(record).copied().unwrap_or(0))
            .sum()
    }

    fn end(&mut self, end: End) {
        let name = match end {
            End::Budget => "budget".to_string(),
            End::Loop => "a loop of the stream".to_string(),
            End::Undefined => "undefined on both sides".to_string(),
            End::FetchAbort => "fetch abort on both sides".to_string(),
            End::PcAlignment => "pc alignment on both sides".to_string(),
            End::OutsideMemory => {
                "a branch out of the mirrored RAM into QEMU's flash or devices".to_string()
            }
            End::DataAbort => "a load or store outside the memory on both sides".to_string(),
            End::Alignment => "an access that is not aligned to its size on both sides".to_string(),
            End::Device => {
                "a load or store that reaches QEMU's flash or devices (not compared)".to_string()
            }
            End::UnliftedException(record) => format!("exception at {} (no lifter)", record.name()),
            End::Unlifted(record) => format!("QEMU ran {} (no lifter)", record.name()),
        };
        *self.ends.entry(name).or_default() += 1;
        self.rounds += 1;
    }
}

struct Run {
    follower: Follower,
    counts: Counts,
    rng: Rng,
}

impl Run {
    /// One round. `Err`: the two sides differed, and the report is printed.
    fn round(&mut self, round: &Round) -> Result<(End, usize), Failure> {
        match self.follower.run(round)? {
            Outcome::Agreed { end, verified } => {
                self.counts.add(&mut self.rng, &verified);
                self.counts.end(end);
                Ok((end, verified.len()))
            }
            Outcome::Differed {
                difference,
                verified,
            } => {
                self.counts.add(&mut self.rng, &verified);
                eprint!("{difference}");
                if let (Some((encoding, before)), Some(after)) =
                    (difference.instruction, difference.qemu)
                {
                    eprintln!(
                        "  corpus line: {}",
                        Entry::core(encoding, before, after).line()
                    );
                }
                Err("the two sides differ".into())
            }
        }
    }
}

fn start(generator: &mut Generator, pc: u64) -> Core {
    let (x, sp, nzcv) = generator.registers((IMAGE_BASE, IMAGE_WORDS as u64 * 4));
    Core {
        x,
        sp,
        pc,
        cpsr: CPSR_EL1H | u32::from(nzcv) << 28,
    }
}

fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

fn main() -> Result<(), Failure> {
    let mut qemu = String::from("qemu-system-aarch64");
    let (mut plugin, mut directory, mut corpus_out) = (None, None, None);
    let (mut seed, mut per_class, mut group_name) = (1u64, 100_000u64, String::from("c1"));
    let mut samples = 3usize;
    let mut probe: Vec<Vec<u32>> = Vec::new();
    let mut probe_offset = 0u64;
    let mut traces = Vec::new();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().ok_or(format!("{argument} needs a value"));
        match argument.as_str() {
            "--qemu" => qemu = value()?,
            "--plugin" => plugin = Some(PathBuf::from(value()?)),
            "--dir" => directory = Some(PathBuf::from(value()?)),
            "--seed" => seed = value()?.parse()?,
            "--per-class" => per_class = value()?.parse()?,
            "--group" => group_name = value()?,
            "--encodings" => traces.push(value()?),
            "--corpus-out" => corpus_out = Some(value()?),
            "--samples" => samples = value()?.parse()?,
            "--probe-offset" => {
                probe_offset = u64::from_str_radix(value()?.trim_start_matches("0x"), 16)?;
            }
            "--probe" => {
                for sequence in value()?.split(',') {
                    let mut words = Vec::new();
                    for word in sequence.split('+') {
                        words.push(u32::from_str_radix(word.trim_start_matches("0x"), 16)?);
                    }
                    probe.push(words);
                }
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let plugin = plugin.ok_or("give --plugin <a64lock.so>")?;
    let directory = directory.ok_or("give --dir <scratch directory>")?;
    let formats = group(&group_name)?;
    let started = Instant::now();

    let memory_group = group_name == "loadstore";
    let mut leader = Leader::start_with(&qemu, &plugin, &directory, RAM_MIB, memory_group)?;
    if memory_group {
        leader.prelude(IMAGE_BASE, &PRELUDE)?;
    }
    // Our memory starts as a copy of the RAM of QEMU.
    let ram = leader.memory(RAM_BASE, RAM_MIB as usize * 1024 * 1024)?;
    let memory = FlatMemory::new(RAM_BASE, ram);
    let mut run = Run {
        follower: Follower::new(leader, Cpu::new(memory, State::reset(RAM_BASE))),
        counts: Counts {
            keep: samples,
            ..Counts::default()
        },
        rng: Rng(seed ^ 0x5eed),
    };
    let mut generator = Generator::new(seed);
    if !probe.is_empty() {
        return probe_rounds(&mut run, &mut generator, &probe, probe_offset, memory_group);
    }
    if memory_group {
        return memory_rounds(
            &mut run,
            &mut generator,
            &formats,
            per_class,
            &traces,
            corpus_out.as_deref(),
            &Header {
                group: &group_name,
                seed,
                qemu: &qemu,
                started,
            },
        );
    }

    // --- the random phase ---------------------------------------------------
    let mut unconstrained = 0;
    loop {
        let short: Vec<FormatClass> = formats
            .iter()
            .copied()
            .filter(|format| run.counts.of_format(*format) < per_class)
            .collect();
        if short.is_empty() {
            break;
        }
        // Three words in four are of a class that is still short of its
        // count. The others keep every class of the group in every image.
        let (words, loose) = generator.image(IMAGE_BASE, IMAGE_WORDS, |rng| {
            if rng.chance(3, 4) {
                short[rng.below(short.len() as u64) as usize]
            } else {
                formats[rng.below(formats.len() as u64) as usize]
            }
        });
        unconstrained += loose;
        let pc = IMAGE_BASE + 4 * generator.rng.below(IMAGE_WORDS as u64);
        let round = Round {
            memory: vec![(IMAGE_BASE, bytes(&words))],
            start: start(&mut generator, pc),
            budget: 2 * IMAGE_WORDS as u64,
            visits: 2,
        };
        run.round(&round)?;
        if run.counts.rounds.is_multiple_of(5000) {
            let least = formats
                .iter()
                .map(|format| run.counts.of_format(*format))
                .min()
                .unwrap_or(0);
            eprintln!(
                "random phase: {} rounds, the class with the fewest has {least} of {per_class}, {:.0} s",
                run.counts.rounds,
                started.elapsed().as_secs_f64()
            );
        }
    }
    let random_rounds = run.counts.rounds;

    // --- the executed phase -------------------------------------------------
    let mut executed: Vec<u32> = Vec::new();
    for path in &traces {
        executed.extend(executed_encodings(&std::fs::read_to_string(path)?));
    }
    executed.sort_unstable();
    executed.dedup();
    let total_encodings = executed.len();
    executed.retain(|word| {
        decode(*word).is_some_and(|decoded| formats.contains(&decoded.record.info().format))
    });
    let is_branch = |word: &u32| {
        decode(*word).is_some_and(|decoded| {
            !matches!(
                decoded.record.info().format,
                FormatClass::ADRI
                    | FormatClass::AddSubImmShift
                    | FormatClass::BaseLogicalImm
                    | FormatClass::BaseMoveImmediate
                    | FormatClass::BaseInsertImmediate
                    | FormatClass::BaseBitfieldImm
                    | FormatClass::BaseBitfieldImmWith2RegArgs
                    | FormatClass::BaseExtractImm
            )
        })
    };
    let (branches, straight): (Vec<u32>, Vec<u32>) =
        executed.iter().copied().partition(|word| is_branch(word));
    let mut executed_verified = 0usize;
    // Instructions that do not branch run many to a round. A zero word ends
    // the image, and both sides stop there.
    for chunk in straight.chunks(IMAGE_WORDS - 1) {
        let mut words = chunk.to_vec();
        words.resize(IMAGE_WORDS, 0);
        let round = Round {
            memory: vec![(IMAGE_BASE, bytes(&words))],
            start: start(&mut generator, IMAGE_BASE),
            budget: chunk.len() as u64,
            visits: 1,
        };
        let (_, verified) = run.round(&round)?;
        if verified != chunk.len() {
            return Err(format!(
                "a round of {} executed encodings verified {verified}",
                chunk.len()
            )
            .into());
        }
        executed_verified += verified;
    }
    // A branch runs alone: one instruction, then the registers are compared
    // at its target, or its target is compared when nothing is there.
    for (index, word) in branches.iter().enumerate() {
        let round = Round {
            memory: vec![(IMAGE_BASE, bytes(&[*word, 0]))],
            start: start(&mut generator, IMAGE_BASE),
            budget: 1,
            visits: 1,
        };
        let (_, verified) = run.round(&round)?;
        if verified != 1 {
            return Err(format!("the executed branch {word:08x} was not verified").into());
        }
        executed_verified += verified;
        if (index + 1).is_multiple_of(50_000) {
            eprintln!(
                "executed phase: {} of {} branches, {:.0} s",
                index + 1,
                branches.len(),
                started.elapsed().as_secs_f64()
            );
        }
    }

    let header = Header {
        group: &group_name,
        seed,
        qemu: &qemu,
        started,
    };
    header.print(random_rounds, unconstrained);
    print_formats(&run.counts, &formats);
    println!(
        "executed phase: {total_encodings} distinct encodings in {} traces, {} of the group ({} straight, {} branches), {executed_verified} verified",
        traces.len(),
        executed.len(),
        straight.len(),
        branches.len()
    );
    finish(&run.counts, &formats, corpus_out.as_deref(), &header)
}

struct Header<'a> {
    group: &'a str,
    seed: u64,
    qemu: &'a str,
    started: Instant,
}

impl Header<'_> {
    fn print(&self, random_rounds: u64, unconstrained: u64) {
        println!(
            "lockstep group={} seed={} qemu={}",
            self.group, self.seed, self.qemu
        );
        println!(
            "random phase: {random_rounds} rounds, {unconstrained} words made without the constraints"
        );
    }
}

fn print_formats(counts: &Counts, formats: &[FormatClass]) {
    for format in formats {
        println!(
            "  {:<28} {:>9} instructions verified",
            format.name(),
            counts.of_format(*format)
        );
        for record in records_of(*format) {
            println!(
                "    {:<26} {:>9}, {:>9} distinct encodings",
                record.name(),
                counts.verified.get(&record).copied().unwrap_or(0),
                counts.distinct.get(&record).map_or(0, |set| set.len())
            );
        }
    }
}

/// The end of the report, the check that every record was verified, and the
/// corpus file.
fn finish(
    counts: &Counts,
    formats: &[FormatClass],
    corpus_out: Option<&str>,
    header: &Header<'_>,
) -> Result<(), Failure> {
    println!("rounds ended by:");
    for (end, count) in &counts.ends {
        println!("  {count:>9} {end}");
    }
    let total: u64 = counts.verified.values().sum();
    println!(
        "total: {total} instructions verified in {} rounds, no difference, {:.0} s",
        counts.rounds,
        header.started.elapsed().as_secs_f64()
    );
    let missing: Vec<&str> = formats
        .iter()
        .flat_map(|format| records_of(*format))
        .filter(|record| !counts.verified.contains_key(record))
        .map(Record::name)
        .collect();
    if !missing.is_empty() {
        return Err(format!("records of the group that no round verified: {missing:?}").into());
    }
    if let Some(path) = corpus_out {
        let mut text = format!(
            "# Instructions of the lockstep group {} with the registers that QEMU had before\n\
             # and after them: {} of each record, sampled from a run with the seed {}.\n\
             # Made by `just tcg-a64-lockstep` (examples/a64_lockstep.rs --corpus-out).\n",
            header.group, counts.keep, header.seed
        );
        let mut refused = 0;
        for samples in counts.samples.values() {
            for entry in samples {
                // An entry must replay alone, which is how `just ci::check`
                // runs it.
                if let Err(error) = entry.replay() {
                    eprintln!("not in the corpus: {error}");
                    refused += 1;
                    continue;
                }
                text.push_str(&entry.line());
                text.push('\n');
            }
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
        println!("wrote the corpus samples to {path}, {refused} did not replay alone");
        if refused != 0 {
            return Err("verified instructions that do not replay alone".into());
        }
    }
    Ok(())
}

/// Run each sequence of words alone and print how its round ended.
fn probe_rounds(
    run: &mut Run,
    generator: &mut Generator,
    sequences: &[Vec<u32>],
    offset: u64,
    memory_group: bool,
) -> Result<(), Failure> {
    if memory_group {
        let data = generator.memory_bytes(WINDOW, WINDOW.1 as usize);
        run.follower.load_memory(WINDOW.0, &data)?;
    }
    for sequence in sequences {
        let first = sequence[0];
        let (mut x, sp, nzcv) = generator.registers_for(first, WINDOW);
        let sp = sp.wrapping_add(offset);
        if let Some(n) = decode(first)
            .and_then(|decoded| vmlab_hv_tcg_aarch64::field(decoded.record, "rn", first))
            .filter(|n| *n < 31)
        {
            x[n as usize] = x[n as usize].wrapping_add(offset);
        }
        let mut words = sequence.clone();
        words.push(0);
        let round = Round {
            memory: vec![(IMAGE_BASE, bytes(&words))],
            start: Core {
                x,
                sp,
                pc: IMAGE_BASE,
                cpsr: CPSR_EL1H | u32::from(nzcv) << 28,
            },
            budget: words.len() as u64,
            visits: 1,
        };
        let names: Vec<String> = sequence
            .iter()
            .map(|word| {
                format!(
                    "{word:08x} {}",
                    decode(*word).map_or("(undefined)", |decoded| decoded.record.name())
                )
            })
            .collect();
        let name = names.join(" + ");
        match run.follower.run(&round)? {
            Outcome::Agreed { end, verified } => {
                println!("{name}: {end:?}, {} verified", verified.len());
                for item in verified {
                    println!("  after {}", item.after);
                    for access in item.accesses {
                        println!("  {access:?}");
                    }
                }
            }
            Outcome::Differed { difference, .. } => print!("{name}: {difference}"),
        }
    }
    Ok(())
}

/// The lockstep of the loads and stores.
fn memory_rounds(
    run: &mut Run,
    generator: &mut Generator,
    formats: &[FormatClass],
    per_class: u64,
    traces: &[String],
    corpus_out: Option<&str>,
    header: &Header<'_>,
) -> Result<(), Failure> {
    // The data on both sides, and vector registers that are not all zero.
    let data = generator.memory_bytes(WINDOW, WINDOW.1 as usize);
    run.follower.load_memory(WINDOW.0, &data)?;
    for n in 0..32 {
        let value =
            u128::from(generator.rng.next_u64()) << 64 | u128::from(generator.rng.next_u64());
        run.follower.set_vector(n, value)?;
    }
    let core = |(x, sp, nzcv): ([u64; 31], u64, u8)| Core {
        x,
        sp,
        pc: IMAGE_BASE,
        cpsr: CPSR_EL1H | u32::from(nzcv) << 28,
    };

    // --- the random phase ---------------------------------------------------
    let mut unconstrained = 0;
    loop {
        let short: Vec<FormatClass> = formats
            .iter()
            .copied()
            .filter(|format| run.counts.of_format(*format) < per_class)
            .collect();
        if short.is_empty() {
            break;
        }
        let (mut words, loose) = generator.image(IMAGE_BASE, MEMORY_IMAGE_WORDS, |rng| {
            if rng.chance(3, 4) {
                short[rng.below(short.len() as u64) as usize]
            } else {
                formats[rng.below(formats.len() as u64) as usize]
            }
        });
        unconstrained += loose;
        // The zero word ends the round on both sides.
        words.push(0);
        let registers = generator.memory_registers(WINDOW);
        let round = Round {
            memory: vec![(IMAGE_BASE, bytes(&words))],
            start: core(registers),
            budget: words.len() as u64,
            visits: 1,
        };
        run.round(&round)?;
        if run.counts.rounds.is_multiple_of(20_000) {
            let least = formats
                .iter()
                .map(|format| run.counts.of_format(*format))
                .min()
                .unwrap_or(0);
            eprintln!(
                "random phase: {} rounds, the class with the fewest has {least} of {per_class}, {:.0} s",
                run.counts.rounds,
                header.started.elapsed().as_secs_f64()
            );
        }
    }
    let random_rounds = run.counts.rounds;

    // --- the executed phase -------------------------------------------------
    let mut executed: Vec<u32> = Vec::new();
    for path in traces {
        executed.extend(executed_encodings(&std::fs::read_to_string(path)?));
    }
    executed.sort_unstable();
    executed.dedup();
    let total_encodings = executed.len();
    executed.retain(|word| {
        decode(*word).is_some_and(|decoded| formats.contains(&decoded.record.info().format))
    });
    // Each encoding runs alone, with a base register that points into the
    // data, so that it runs to its end.
    let (mut executed_verified, mut executed_faulted) = (0usize, 0usize);
    for (index, word) in executed.iter().enumerate() {
        let registers = generator.registers_for(*word, WINDOW);
        let round = Round {
            memory: vec![(IMAGE_BASE, bytes(&[*word, 0]))],
            start: core(registers),
            budget: 2,
            visits: 1,
        };
        let (_, verified) = run.round(&round)?;
        if verified == 1 {
            executed_verified += 1;
        } else {
            executed_faulted += 1;
        }
        if (index + 1).is_multiple_of(50_000) {
            eprintln!(
                "executed phase: {} of {} encodings, {:.0} s",
                index + 1,
                executed.len(),
                header.started.elapsed().as_secs_f64()
            );
        }
    }

    // --- the report -----------------------------------------------------------
    header.print(random_rounds, unconstrained);
    print_formats(&run.counts, formats);
    println!(
        "executed phase: {total_encodings} distinct encodings in {} traces, {} of the group, {executed_verified} verified, {executed_faulted} ended in the same exception on both sides",
        traces.len(),
        executed.len()
    );
    println!(
        "memory: {} verified instructions wrote {} bytes, every byte compared",
        run.counts.stores, run.counts.stored_bytes
    );
    println!(
        "exclusive stores: {} wrote, {} did not",
        run.counts.exclusive_wrote, run.counts.exclusive_failed
    );
    println!(
        "stores of several parts that failed after a different count of parts on the two sides: {}",
        run.follower.partial_stores
    );
    finish(&run.counts, formats, corpus_out, header)
}
