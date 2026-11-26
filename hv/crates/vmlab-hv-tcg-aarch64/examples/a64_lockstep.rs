//! The instruction lockstep against QEMU for a group of format classes
//! (DESIGN.md O3). `just tcg-a64-lockstep` in `hv/` builds the plugin and
//! runs this.
//!
//! Usage:
//!   a64_lockstep --plugin <a64lock.so> --dir <scratch directory>
//!       [--qemu <binary>] [--seed S] [--per-class N] [--group NAME]
//!       [--encodings <trace.txt>]... [--corpus-out <file>]
//!
//! Two phases. The random phase runs rounds of random instructions until
//! each format class of the group has `--per-class` instructions that both
//! sides ran from the same registers to the same registers. The executed
//! phase runs each distinct encoding of the group from the traces of the
//! template boots (`measure/a64trace.c` of the design card). The first
//! difference stops the run with exit code 1.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;
use vmlab_hv_tcg::FlatMemory;
use vmlab_hv_tcg_aarch64::lift::LIFTED_FORMATS;
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
/// Corpus entries kept per record.
const SAMPLES: usize = 3;

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
        other => {
            return Err(format!("the group {other} is not known: dp-immediate, branch, c1").into());
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
            // Reservoir sampling: each instruction of a record has the same
            // chance to be one of its samples.
            let entry = Entry {
                encoding: item.encoding,
                before: item.before,
                after: item.after,
            };
            let samples = self.samples.entry(item.record).or_default();
            if samples.len() < SAMPLES {
                samples.push(entry);
            } else {
                let slot = rng.below(*count) as usize;
                if slot < SAMPLES {
                    samples[slot] = entry;
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
                        Entry {
                            encoding,
                            before,
                            after
                        }
                        .line()
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
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let plugin = plugin.ok_or("give --plugin <a64lock.so>")?;
    let directory = directory.ok_or("give --dir <scratch directory>")?;
    let formats = group(&group_name)?;
    let started = Instant::now();

    let leader = Leader::start(&qemu, &plugin, &directory, RAM_MIB)?;
    let memory = FlatMemory::new(RAM_BASE, vec![0; RAM_MIB as usize * 1024 * 1024]);
    let mut run = Run {
        follower: Follower::new(leader, Cpu::new(memory, State::reset(RAM_BASE))),
        counts: Counts::default(),
        rng: Rng(seed ^ 0x5eed),
    };
    let mut generator = Generator::new(seed);

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

    // --- the report -----------------------------------------------------------
    println!("lockstep group={group_name} seed={seed} qemu={qemu}");
    println!(
        "random phase: {random_rounds} rounds, {} words made without the constraints",
        unconstrained
    );
    for format in &formats {
        println!(
            "  {:<28} {:>9} instructions verified",
            format.name(),
            run.counts.of_format(*format)
        );
        for record in records_of(*format) {
            println!(
                "    {:<26} {:>9}, {:>9} distinct encodings",
                record.name(),
                run.counts.verified.get(&record).copied().unwrap_or(0),
                run.counts.distinct.get(&record).map_or(0, |set| set.len())
            );
        }
    }
    println!(
        "executed phase: {total_encodings} distinct encodings in {} traces, {} of the group ({} straight, {} branches), {executed_verified} verified",
        traces.len(),
        executed.len(),
        straight.len(),
        branches.len()
    );
    println!("rounds ended by:");
    for (end, count) in &run.counts.ends {
        println!("  {count:>9} {end}");
    }
    let total: u64 = run.counts.verified.values().sum();
    println!(
        "total: {total} instructions verified in {} rounds, no difference, {:.0} s",
        run.counts.rounds,
        started.elapsed().as_secs_f64()
    );
    let missing: Vec<&str> = formats
        .iter()
        .flat_map(|format| records_of(*format))
        .filter(|record| !run.counts.verified.contains_key(record))
        .map(Record::name)
        .collect();
    if !missing.is_empty() {
        return Err(format!("records of the group that no round verified: {missing:?}").into());
    }
    if let Some(path) = corpus_out {
        let mut text = format!(
            "# Instructions of the lockstep group {group_name} with the registers that QEMU had before\n\
             # and after them: {SAMPLES} of each record, sampled from a run with the seed {seed}.\n\
             # Made by `just tcg-a64-lockstep` (examples/a64_lockstep.rs --corpus-out).\n"
        );
        for samples in run.counts.samples.values() {
            for entry in samples {
                text.push_str(&entry.line());
                text.push('\n');
            }
        }
        std::fs::write(&path, text)?;
        println!("wrote the corpus samples to {path}");
    }
    Ok(())
}
