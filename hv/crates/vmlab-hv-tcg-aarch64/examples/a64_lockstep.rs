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
//! The group `system` (card C6) runs streams of system instructions with a
//! vector table: both sides take the exceptions, change the level and
//! return. Each of its rounds starts from a reset of both sides, and also
//! compares the system registers of `oracle::compared` before every
//! instruction, among them ESR_EL1, FAR_EL1, ELR_EL1 and SPSR_EL1.
//!
//! The group `fp` (card C4) also loads and compares V0 to V31, FPSR and
//! FPCR. Its rounds are short, because FPSR only collects: after a few
//! instructions most of its bits are set, and an instruction that sets one
//! more or one less no longer shows.
//! The groups `simd`, `crypto` and `c7` (card C7) run as `fp` does, from
//! vector registers whose lanes hold the ends of each integer range, shift
//! amounts and FP values.
//! The group `loadstore` runs another way, because its instructions work on
//! memory and on the vector registers. QEMU first runs a prelude that turns
//! the FP and vector registers on and the stack pointer alignment check
//! off. The plugin gives the vector registers with each record. A round is
//! a short straight run of instructions on registers that point into a
//! window of data, and it ends at the first access that the memory does not
//! hold, or at the zero word behind the image. The memory writes and the
//! vector registers are compared after every instruction.
//!
//! The group `mmu` (card C8) runs streams with the MMU on, over random
//! translation tables for the 4K and the 64K granule (`oracle/translate.rs`):
//! loads, stores and calls at addresses that the tables map, do not map or
//! map without the permission, address translation instructions, `DC ZVA`,
//! cache and TLB operations. It follows as `system` does, and also compares
//! the memory writes. The group `walk` runs the same streams for the 16K and
//! the 64K granule against QEMU's `-cpu max`, inside what an Armv8.0 walk
//! and the walk of that CPU agree on.
//!
//! `--probe` runs each given word alone, as the executed phase of the group
//! does, and prints how its round ended. Words joined by `+` run one after
//! the other in one round, with the registers made for the first of them.
//! It is for a question to QEMU about one encoding, and it compares
//! nothing else. `--probe-offset` moves the
//! base register and the stack pointer of a probe by that many bytes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;
use vmlab_hv_tcg::FlatMemory;
use vmlab_hv_tcg_aarch64::lift::{
    BRANCH_FORMATS, CRYPTO_FORMATS, FP_FORMATS, IMMEDIATE_FORMATS, LIFTED_FORMATS,
    LOAD_STORE_FORMATS, REGISTER_FORMATS, SIMD_FORMATS,
};
use vmlab_hv_tcg_aarch64::oracle::corpus::Entry;
use vmlab_hv_tcg_aarch64::oracle::follow::{End, Follower, Outcome, Round, Verified};
use vmlab_hv_tcg_aarch64::oracle::leader::{Leader, Options, RAM_BASE};
use vmlab_hv_tcg_aarch64::oracle::stream::{Generator, Rng, executed_encodings, records_of};
use vmlab_hv_tcg_aarch64::oracle::system::{
    STREAM_FORMATS, TABLE_BYTES, agreed, directed, vector_table,
};
use vmlab_hv_tcg_aarch64::oracle::translate::{self, Layout, Profile};
use vmlab_hv_tcg_aarch64::oracle::{CPSR_EL1H, Compared, Core, Vectors, asm, compared};
use vmlab_hv_tcg_aarch64::sysreg;
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
/// The same for a group that compares FPSR.
const FP_IMAGE_WORDS: usize = 12;
/// Instructions in the stream of a system round, and the most that a round
/// follows: the handlers run many more than the stream holds.
const SYSTEM_IMAGE_WORDS: usize = 400;
const SYSTEM_BUDGET: u64 = 3000;
/// Items in the stream of a round with the MMU on, and the most
/// instructions that a round follows.
const TRANSLATED_ITEMS: usize = 260;
const TRANSLATED_BUDGET: u64 = 6000;

fn group(name: &str) -> Result<Vec<FormatClass>, Failure> {
    Ok(match name {
        "dp-immediate" => IMMEDIATE_FORMATS.to_vec(),
        "branch" => BRANCH_FORMATS.to_vec(),
        "c1" => [IMMEDIATE_FORMATS.as_slice(), BRANCH_FORMATS.as_slice()].concat(),
        "dp-register" | "c2" => REGISTER_FORMATS.to_vec(),
        "fp" => FP_FORMATS.to_vec(),
        "all" => LIFTED_FORMATS.to_vec(),
        "loadstore" => LOAD_STORE_FORMATS.to_vec(),
        "system" | "c6" => STREAM_FORMATS.to_vec(),
        "mmu" | "c8" | "walk" => vec![FormatClass::SystemXtI],
        "simd" => SIMD_FORMATS.to_vec(),
        "crypto" => CRYPTO_FORMATS.to_vec(),
        "c7" => [SIMD_FORMATS.as_slice(), CRYPTO_FORMATS.as_slice()].concat(),
        // One format class of AdvSIMD or crypto by its name, for a longer
        // run of a class with many records.
        other
            if SIMD_FORMATS
                .iter()
                .chain(&CRYPTO_FORMATS)
                .any(|format| format.name() == other) =>
        {
            SIMD_FORMATS
                .iter()
                .chain(&CRYPTO_FORMATS)
                .copied()
                .filter(|format| format.name() == other)
                .collect()
        }
        other => {
            return Err(format!(
                "the group {other} is not known: dp-immediate, branch, c1, dp-register, c2, fp, all, loadstore, system, c6, mmu, c8, walk, simd, crypto, c7, or one format class of c7 by its name"
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
    /// By the record, and for `MRS` and `MSR` of a register with a row by
    /// the register too.
    samples: BTreeMap<(Record, u16), Vec<Entry>>,
    /// Instructions that were verified and cannot be replayed without QEMU:
    /// counter reads and host calls.
    led: u64,
    /// The classes whose instructions are sampled. Empty: all. A system
    /// stream also holds instructions of the earlier groups, which have
    /// their own corpus.
    sampled: Vec<FormatClass>,
    /// The system registers that a system round compares.
    compared: Vec<Compared>,
    /// Corpus entries kept per record.
    keep: usize,
    /// A corpus entry holds the vector registers that its instruction can
    /// name or that it changed, and zero for the others. Without this it
    /// holds all of them.
    few_vectors: bool,
    /// How many instructions each key of `samples` has seen, where a key is
    /// not a record.
    sample_counts: BTreeMap<(Record, u16), u64>,
    /// Exclusive stores that wrote, and that did not.
    exclusive_wrote: u64,
    exclusive_failed: u64,
    /// Verified instructions that wrote memory, and the bytes.
    stores: u64,
    stored_bytes: u64,
    /// The rounds have the MMU on: the first address of the vector table
    /// and the place of ESR_EL1 among the compared registers. A sample is
    /// then kept for each format class and each kind of exception.
    translated: Option<(u64, usize)>,
    /// Verified instructions that took an exception, by ESR_EL1 without the
    /// bits of the instruction: the class and the low byte.
    syndromes: BTreeMap<u64, u64>,
}

/// The key of the samples of an instruction, and how many are kept of the
/// `keep` of a record.
fn sample_key(record: Record, encoding: u32, keep: usize) -> ((Record, u16), usize) {
    if matches!(record, Record::MRS | Record::MSR) {
        let register = (encoding >> 5) as u16;
        if sysreg::find(register).is_some() {
            return ((record, register), 1);
        }
    }
    ((record, 0), keep)
}

/// The corpus entry of a verified instruction. `few_vectors`: the entry
/// holds the vector registers that the instruction can name and every one
/// that it changed, and a replay starts the others at zero.
fn entry(item: &Verified, few_vectors: bool) -> Entry {
    let mut entry = Entry::core(item.encoding, item.before, item.after);
    for access in &item.accesses {
        let bytes = access.value.to_le_bytes()[..usize::from(access.bytes)].to_vec();
        if access.write {
            entry.writes.push((access.address, bytes));
        } else {
            entry.reads.push((access.address, bytes));
        }
    }
    let Some((before, after)) = &item.vectors else {
        return entry;
    };
    if !few_vectors {
        entry.vectors = item.vectors;
        return entry;
    }
    let has_field = |name: &str| vmlab_hv_tcg_aarch64::field(item.record, name, 0).is_some();
    let format = item.record.info().format;
    // An AdvSIMD or crypto instruction names Rd, Rn, Rm and, for a table,
    // the three registers after Rn.
    let lanes = SIMD_FORMATS.contains(&format) || CRYPTO_FORMATS.contains(&format);
    let vector_data = lanes || has_field("vt") || item.encoding & (1 << 26) != 0;
    let first = item.encoding & 31;
    let second = (item.encoding >> 10) & 31;
    let (rn, rm) = ((item.encoding >> 5) & 31, (item.encoding >> 16) & 31);
    let mut kept = (
        Vectors {
            v: [0; 32],
            ..*before
        },
        Vectors {
            v: [0; 32],
            ..*after
        },
    );
    let mut any = before.fpsr != 0 || before.fpcr != 0 || after.fpsr != 0 || after.fpcr != 0;
    for n in 0..32usize {
        let named = vector_data
            && ((n as u32).wrapping_sub(first) & 31 < 4
                || n as u32 == second
                // An indexed form of 16-bit lanes has four bits of Rm.
                || (lanes
                    && ((n as u32).wrapping_sub(rn) & 31 < 4
                        || n as u32 == rm
                        || n as u32 == rm & 15)));
        if before.v[n] != after.v[n] || (named && before.v[n] != 0) {
            kept.0.v[n] = before.v[n];
            kept.1.v[n] = after.v[n];
            any = true;
        }
    }
    entry.vectors = any.then_some(kept);
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
            if !item.replayable {
                self.led += 1;
                continue;
            }
            if !self.sampled.is_empty() && !self.sampled.contains(&item.record.info().format) {
                continue;
            }
            // An instruction of a system round has its system registers.
            let make = |item: &Verified| match item.system {
                Some(_) => Entry::of(item, &self.compared),
                None => entry(item, self.few_vectors),
            };
            // Reservoir sampling: each instruction of a record has the same
            // chance to be one of its samples.
            let (key, keep) = match self.translated {
                None => sample_key(item.record, item.encoding, self.keep),
                Some((table, esr)) => {
                    // An exception leaves the stream for an entry of the
                    // vector table.
                    let to = item.after.pc;
                    let taken = (table..table + translate::VECTORS_BYTES).contains(&to)
                        && to & 0x7f == 0
                        && item.before.pc.wrapping_add(4) != to;
                    let outcome = match (&item.system, taken) {
                        (Some((_, after)), true) => {
                            let syndrome = after[esr] & 0xfc00_00ff;
                            *self.syndromes.entry(syndrome).or_default() += 1;
                            (syndrome >> 18) as u16 | syndrome as u16 & 0xff
                        }
                        _ => 0,
                    };
                    // One sample for each outcome of a load or a store, of
                    // a system operation, and of everything else.
                    let format = item.record.info().format;
                    let kind = if LOAD_STORE_FORMATS.contains(&format) {
                        FormatClass::BaseLoadStoreUI
                    } else if matches!(format, FormatClass::SystemXtI | FormatClass::SystemLXtI) {
                        FormatClass::SystemXtI
                    } else {
                        FormatClass::ADRI
                    };
                    ((records_of(kind)[0], outcome), self.keep)
                }
            };
            let seen = if self.translated.is_some() {
                let seen = self.sample_counts.entry(key).or_default();
                *seen += 1;
                *seen
            } else {
                *count
            };
            let samples = self.samples.entry(key).or_default();
            if samples.len() < keep {
                samples.push(make(item));
            } else {
                let slot = rng.below(seen) as usize;
                if slot < keep {
                    samples[slot] = make(item);
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
            End::StaleCode => {
                "a store into the code of the round, where QEMU runs the old word (not compared)"
                    .to_string()
            }
            End::DataAbort => "a load or store outside the memory on both sides".to_string(),
            End::Alignment => "an access that is not aligned to its size on both sides".to_string(),
            End::Device => {
                "a load or store that reaches QEMU's flash or devices (not compared)".to_string()
            }
            End::Exception => {
                "another exception on both sides, to a vector outside the RAM".to_string()
            }
            End::Unfollowed(why) => format!("not followed: {why}"),
            End::UnliftedException(record) => format!("exception at {} (no lifter)", record.name()),
            End::Unlifted(record) => format!("QEMU ran {} (no lifter)", record.name()),
        };
        *self.ends.entry(name).or_default() += 1;
        self.rounds += 1;
    }
}

struct Run {
    /// The rounds are of loads and stores: after a round in which QEMU ran
    /// ahead of our side, the exclusive monitors are made the same again.
    memory: bool,
    follower: Follower,
    counts: Counts,
    rng: Rng,
}

impl Run {
    /// One round. `Err`: the two sides differed, and the report is printed.
    fn round(&mut self, round: &Round) -> Result<(End, usize), Failure> {
        let outcome = self.follower.run(round)?;
        if self.memory && self.follower.monitor_stale && matches!(outcome, Outcome::Agreed { .. }) {
            self.follower.sync_monitor(IMAGE_BASE, WINDOW.0)?;
        }
        match outcome {
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
                // The start of the round, to run it again with `--probe`.
                // A round with the MMU on loads its tables and its data too:
                // the code is the first part.
                let parts = if self.follower.translated {
                    1
                } else {
                    usize::MAX
                };
                for (address, data) in round.memory.iter().take(parts) {
                    let words: Vec<String> = data
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .take((verified.len() + 2).min(if self.follower.translated {
                            0
                        } else {
                            usize::MAX
                        }))
                        .map(|word| format!("{:08x}", u32::from_le_bytes(*word)))
                        .collect();
                    eprintln!("  the round from {address:#x}: {}", words.join("+"));
                }
                if let (Some((encoding, before)), Some(after)) =
                    (difference.instruction, difference.qemu)
                {
                    let vectors = difference
                        .vectors_before
                        .zip(difference.vectors.map(|(_, qemu)| qemu));
                    eprintln!(
                        "  corpus line: {}",
                        Entry {
                            vectors,
                            ..Entry::core(encoding, before, after)
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
    // A group of FP instructions compares the vector and FP registers.
    // A group of AdvSIMD or crypto instructions does too, from vector
    // registers that are made for lanes.
    let lanes = formats
        .iter()
        .any(|format| SIMD_FORMATS.contains(format) || CRYPTO_FORMATS.contains(format));
    let vectors = lanes || formats.iter().any(|format| FP_FORMATS.contains(format));
    let start_vectors = |generator: &mut Generator| {
        if lanes {
            generator.simd_vectors()
        } else {
            generator.vectors()
        }
    };
    let image_words = if vectors { FP_IMAGE_WORDS } else { IMAGE_WORDS };

    // The group of the loads and stores compares them too, and its rounds
    // work on memory.
    let memory_group = group_name == "loadstore";
    // A group with the MMU on runs as a group of system instructions, with
    // its own streams. The walk test runs against another CPU of QEMU.
    let walk = group_name == "walk";
    let translated = walk || matches!(group_name.as_str(), "mmu" | "c8");
    // A group of system instructions also compares the system registers, and
    // its streams hold FP instructions too.
    let system = formats == STREAM_FORMATS || translated;
    let mut compared = if system { compared() } else { Vec::new() };
    if walk {
        // QEMU's `max` is a later CPU than ours: its control registers hold
        // other bits. The registers of a fault and of a translation are
        // what the walk test is about.
        compared.retain(|register| {
            matches!(
                register.qemu,
                "ESR_EL1"
                    | "FAR_EL1"
                    | "ELR_EL1"
                    | "SPSR_EL1"
                    | "PAR_EL1"
                    | "TTBR0_EL1"
                    | "TTBR1_EL1"
                    | "MAIR_EL1"
                    | "VBAR_EL1"
                    | "SP_EL0"
                    | "SP_EL1"
            )
        });
    }
    let mut leader = if system {
        let options = Options {
            vectors: true,
            system: compared
                .iter()
                .map(|register| register.qemu.to_string())
                .collect(),
            names: None,
            // Without SVE and SME the plugin API offers V0 to V31 by the
            // names that the plugin asks for.
            cpu: walk.then(|| "max,sve=off,sme=off".to_string()),
        };
        let mut leader = Leader::start_options(&qemu, &plugin, &directory, RAM_MIB, &options)?;
        // The streams have their own vector table.
        leader.clear_vector_break()?;
        leader
    } else {
        Leader::start_with(&qemu, &plugin, &directory, RAM_MIB, vectors || memory_group)?
    };
    if memory_group {
        leader.prelude(IMAGE_BASE, &PRELUDE)?;
    }
    // Our memory starts as a copy of the RAM of QEMU.
    let ram = leader.memory(RAM_BASE, RAM_MIB as usize * 1024 * 1024)?;
    let memory = FlatMemory::new(RAM_BASE, ram);
    let mut follower = Follower::new(leader, Cpu::new(memory, State::reset(RAM_BASE)));
    follower.compared = compared.clone();
    follower.translated = translated;
    if walk {
        follower.model = &sysreg::QEMU_MAX_WALK;
        // N, Z, C and V, IL, D, A, I and F, the level and the stack
        // pointer: the bits of PSTATE that an Armv8.0 CPU has.
        follower.pstate_mask = 0xf010_03cf;
    }
    let esr = compared
        .iter()
        .position(|register| register.qemu == "ESR_EL1")
        .unwrap_or(0);
    let mut run = Run {
        memory: memory_group,
        follower,
        counts: Counts {
            keep: samples,
            few_vectors: memory_group || lanes,
            sampled: if system && !translated {
                formats.clone()
            } else {
                Vec::new()
            },
            compared,
            translated: translated.then_some((IMAGE_BASE, esr)),
            ..Counts::default()
        },
        rng: Rng(seed ^ 0x5eed),
    };
    let mut generator = Generator::new(seed);
    if !probe.is_empty() {
        return probe_rounds(&mut run, &mut generator, &probe, probe_offset, memory_group);
    }
    if translated {
        let profile = if walk {
            translate::MAX_WALK
        } else {
            translate::CORTEX_A53
        };
        return translated_rounds(
            &mut run,
            &mut generator,
            profile,
            per_class,
            // A corpus entry replays as QEMU's `cortex-a53`.
            corpus_out.as_deref().filter(|_| !walk),
            &Header {
                group: &group_name,
                seed,
                qemu: &qemu,
                started,
            },
        );
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
        if system {
            let words = generator.system_image(IMAGE_BASE, SYSTEM_IMAGE_WORDS);
            let round = Round {
                memory: vec![(IMAGE_BASE, bytes(&words))],
                start: start(&mut generator, IMAGE_BASE + TABLE_BYTES),
                // The FP group has the FP values (card C4). Here the vector
                // registers start at zero, and the stream writes FPCR.
                vectors: Some(Default::default()),
                budget: SYSTEM_BUDGET,
                visits: 4,
                reset: true,
                exempt: (IMAGE_BASE, IMAGE_BASE + TABLE_BYTES),
            };
            run.round(&round)?;
            if run.counts.rounds.is_multiple_of(200) {
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
            continue;
        }
        // Three words in four are of a class that is still short of its
        // count. The others keep every class of the group in every image.
        let (words, loose) = generator.image(IMAGE_BASE, image_words, |rng| {
            if rng.chance(3, 4) {
                short[rng.below(short.len() as u64) as usize]
            } else {
                formats[rng.below(formats.len() as u64) as usize]
            }
        });
        unconstrained += loose;
        // A round without branches runs its image from the start.
        let pc = if vectors {
            IMAGE_BASE
        } else {
            IMAGE_BASE + 4 * generator.rng.below(image_words as u64)
        };
        let round = Round {
            memory: vec![(IMAGE_BASE, bytes(&words))],
            start: start(&mut generator, pc),
            vectors: vectors.then(|| start_vectors(&mut generator)),
            budget: image_words as u64 * if vectors { 1 } else { 2 },
            visits: 2,
            exempt: (0, 0),
            reset: false,
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
        decode(*word).is_some_and(|decoded| BRANCH_FORMATS.contains(&decoded.record.info().format))
    };
    let (branches, straight): (Vec<u32>, Vec<u32>) = if system {
        (Vec::new(), Vec::new())
    } else {
        executed.iter().copied().partition(|word| is_branch(word))
    };
    let mut executed_verified = 0usize;
    // A system instruction runs alone, after a reset, with the vector table
    // of the streams and every register zero: an `MSR` then writes zero,
    // which turns nothing on. WFI is left out: it would stop QEMU.
    let mut waits = 0;
    // `MRS` and `MSR` of a register that QEMU's CPU has and ours leaves out
    // by decision: the PMU (DESIGN.md V5, the allow-list of `sysreg.rs`).
    let mut left_out = 0;
    if system {
        let mut missed = Vec::new();
        // The rounds that the random streams cannot hold.
        for (body, must) in directed() {
            let mut words = vector_table(0x3c5);
            words.extend(asm::load(16, IMAGE_BASE));
            words.push(asm::msr(asm::reg::VBAR_EL1, 16));
            words.extend(&body);
            words.push(asm::b(0));
            let round = Round {
                memory: vec![(IMAGE_BASE, bytes(&words))],
                start: Core {
                    x: [0; 31],
                    sp: 0,
                    pc: IMAGE_BASE + TABLE_BYTES,
                    cpsr: CPSR_EL1H,
                },
                vectors: Some(Default::default()),
                budget: 200,
                visits: 2,
                reset: true,
                exempt: (IMAGE_BASE, IMAGE_BASE + TABLE_BYTES),
            };
            run.round(&round)?;
            for word in must {
                if !run.counts.distinct.values().any(|set| set.contains(&word)) {
                    return Err(format!("a directed round did not verify {word:08x}").into());
                }
            }
        }
        for word in &executed {
            if *word == 0xd503_207f {
                waits += 1;
                continue;
            }
            if word & 0xffc0_0000 == 0xd500_0000
                && word & (1 << 20) != 0
                && !agreed((word >> 5) as u16)
            {
                left_out += 1;
                continue;
            }
            let mut words = vector_table(0x3c5);
            words.extend(asm::load(16, IMAGE_BASE));
            words.push(asm::msr(asm::reg::VBAR_EL1, 16));
            words.push(asm::movz(16, 0, 0));
            words.extend([*word, asm::NOP, asm::b(0)]);
            let round = Round {
                memory: vec![(IMAGE_BASE, bytes(&words))],
                start: Core {
                    x: [0; 31],
                    sp: 0,
                    pc: IMAGE_BASE + TABLE_BYTES,
                    cpsr: CPSR_EL1H,
                },
                vectors: Some(Default::default()),
                budget: 7,
                visits: 2,
                reset: true,
                exempt: (IMAGE_BASE, IMAGE_BASE + TABLE_BYTES),
            };
            run.round(&round)?;
            let verified = run.counts.distinct.values().any(|set| set.contains(word));
            if verified {
                executed_verified += 1;
            } else {
                missed.push(*word);
            }
        }
        if !missed.is_empty() {
            return Err(format!(
                "{} executed encodings were not verified, the first {:08x?}",
                missed.len(),
                &missed[..missed.len().min(8)]
            )
            .into());
        }
    }
    // Instructions that do not branch run many to a round. A zero word ends
    // the image, and both sides stop there.
    for chunk in straight.chunks(image_words - 1) {
        let mut words = chunk.to_vec();
        words.resize(image_words, 0);
        let round = Round {
            memory: vec![(IMAGE_BASE, bytes(&words))],
            start: start(&mut generator, IMAGE_BASE),
            vectors: vectors.then(|| start_vectors(&mut generator)),
            budget: chunk.len() as u64,
            visits: 1,
            exempt: (0, 0),
            reset: false,
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
            vectors: vectors.then(|| start_vectors(&mut generator)),
            budget: 1,
            visits: 1,
            exempt: (0, 0),
            reset: false,
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
    if !run.follower.exceptions.is_empty() {
        println!("exceptions that both sides took, with the same registers after them:");
        for (kind, count) in &run.follower.exceptions {
            println!("  {count:>9} {kind}");
        }
    }
    if system {
        println!(
            "{waits} WFI encodings left out, {left_out} encodings of registers that the 3d CPU leaves out (the PMU), {} verified instructions that QEMU leads in (counter reads, host calls)",
            run.counts.led
        );
    }
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
                    // An instruction with the MMU on that changes its own
                    // translation names memory that a replay cannot hold.
                    if counts.translated.is_none() {
                        eprintln!("not in the corpus: {error}");
                    }
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
        if refused != 0 && counts.translated.is_none() {
            return Err("verified instructions that do not replay alone".into());
        }
    }
    Ok(())
}

/// The rounds of a group with the MMU on (card C8): streams over random
/// translation tables, until `per_class` cache, TLB and address translation
/// operations are verified.
fn translated_rounds(
    run: &mut Run,
    generator: &mut Generator,
    profile: Profile,
    per_class: u64,
    corpus_out: Option<&str>,
    header: &Header<'_>,
) -> Result<(), Failure> {
    let layout = Layout { base: IMAGE_BASE };
    let mut granules: BTreeMap<u32, u64> = BTreeMap::new();
    while run.counts.of_format(FormatClass::SystemXtI) < per_class {
        let image = generator.translated_image(layout, profile, TRANSLATED_ITEMS);
        for granule in image.granules {
            *granules.entry(granule).or_default() += 1;
        }
        let (x, sp, nzcv) = generator.registers((image.start, 0x1000));
        let round = Round {
            memory: image.memory,
            start: Core {
                x,
                sp,
                pc: image.start,
                cpsr: CPSR_EL1H | u32::from(nzcv) << 28,
            },
            vectors: Some(Default::default()),
            budget: TRANSLATED_BUDGET,
            visits: 4,
            reset: true,
            exempt: (layout.base, layout.base + translate::VECTORS_BYTES),
        };
        run.round(&round)?;
        if run.counts.rounds.is_multiple_of(100) {
            eprintln!(
                "{} rounds, {} of {per_class} system operations, {:.0} s",
                run.counts.rounds,
                run.counts.of_format(FormatClass::SystemXtI),
                header.started.elapsed().as_secs_f64()
            );
        }
    }
    header.print(run.counts.rounds, 0);
    for (granule, halves) in &granules {
        println!(
            "  {halves:>9} halves of an address space with the {}K granule",
            (1u32 << granule) / 1024
        );
    }
    print_formats(
        &run.counts,
        &[FormatClass::SystemXtI, FormatClass::SystemLXtI],
    );
    let loads_and_stores: u64 = LOAD_STORE_FORMATS
        .iter()
        .map(|format| run.counts.of_format(*format))
        .sum();
    let total: u64 = run.counts.verified.values().sum();
    println!(
        "{loads_and_stores} loads and stores verified, and {} other instructions",
        total - loads_and_stores - run.counts.of_format(FormatClass::SystemXtI)
    );
    println!(
        "memory: {} verified instructions wrote {} bytes, every byte compared",
        run.counts.stores, run.counts.stored_bytes
    );
    println!(
        "stores of several parts that faulted after a different count of parts on the two sides: {}",
        run.follower.partial_stores
    );
    println!(
        "loads of several vector registers that faulted with other values in those registers on the two sides: {}",
        run.follower.partial_loads
    );
    println!("exceptions that both sides took, with the same registers after them:");
    for (kind, count) in &run.follower.exceptions {
        println!("  {count:>9} {kind}");
    }
    println!(
        "the syndromes of the verified instructions that took an exception (class and low byte):"
    );
    for (syndrome, count) in &run.counts.syndromes {
        println!("  {count:>9} {syndrome:#010x}");
    }
    finish(&run.counts, &[], corpus_out, header)
}

/// Move the base register of `word`, or the stack pointer when that is its
/// base, by `offset` bytes.
fn move_base(x: &mut [u64; 31], sp: &mut u64, word: u32, offset: u64) {
    match decode(word).and_then(|decoded| vmlab_hv_tcg_aarch64::field(decoded.record, "rn", word)) {
        Some(31) => *sp = sp.wrapping_add(offset),
        Some(n) => x[n as usize] = x[n as usize].wrapping_add(offset),
        None => {}
    }
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
        let (mut x, mut sp, nzcv) = generator.registers_for(first, WINDOW);
        move_base(&mut x, &mut sp, first, offset);
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
            vectors: None,
            budget: words.len() as u64,
            visits: 1,
            exempt: (0, 0),
            reset: false,
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
    let mut vectors = Vectors::default();
    for value in &mut vectors.v {
        *value = u128::from(generator.rng.next_u64()) << 64 | u128::from(generator.rng.next_u64());
    }
    run.follower.set_vectors(&vectors)?;
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
            vectors: None,
            budget: words.len() as u64,
            visits: 1,
            exempt: (0, 0),
            reset: false,
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
    // data, so that it runs to its end. Where its address is not aligned
    // that way, it runs again at another place of the image (a literal) and
    // then with the base moved by 1 to 15 bytes: the guest that executed it
    // had an aligned address.
    let mut places: Vec<(u64, u64)> = vec![(0, 0), (4, 0), (8, 0), (12, 0)];
    places.extend((1..16).map(|offset| (0, offset)));
    let (mut executed_verified, mut executed_faulted) = (0usize, 0usize);
    let mut faulted: Vec<u32> = Vec::new();
    for (index, word) in executed.iter().enumerate() {
        let mut verified = false;
        for (pc_offset, base_offset) in &places {
            let (mut x, mut sp, nzcv) = generator.registers_for(*word, WINDOW);
            move_base(&mut x, &mut sp, *word, *base_offset);
            let pc = IMAGE_BASE + pc_offset;
            let round = Round {
                memory: vec![(pc, bytes(&[*word, 0]))],
                start: Core {
                    pc,
                    ..core((x, sp, nzcv))
                },
                vectors: None,
                budget: 2,
                visits: 1,
                exempt: (0, 0),
                reset: false,
            };
            if run.round(&round)?.1 == 1 {
                verified = true;
                break;
            }
        }
        if verified {
            executed_verified += 1;
        } else {
            executed_faulted += 1;
            faulted.push(*word);
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
        "executed phase: {total_encodings} distinct encodings in {} traces, {} of the group, {executed_verified} verified, {executed_faulted} ended in the same exception on both sides at every place that was tried",
        traces.len(),
        executed.len()
    );
    for word in faulted.iter().take(20) {
        println!(
            "  not run to its end: {word:08x} {}",
            decode(*word).map_or("(undefined)", |decoded| decoded.record.name())
        );
    }
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
