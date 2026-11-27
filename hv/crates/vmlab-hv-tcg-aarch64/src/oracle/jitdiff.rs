//! The block differential of the JIT (3e DESIGN.md K2): random streams of
//! the generator of O3 run on two CPUs over two copies of one
//! `FlatMemory`, the interpreter on one and the compiled code on the
//! other. The registers, the system registers, the memory, the way the run
//! stopped and the counts of instructions and exceptions must be the same.
//!
//! A round is an image of 1 to 64 instructions with a vector table of
//! `HVC` behind it, so an exception stops the run as a hypercall does, and
//! a budget that ends a run that loops. The integer, branch, load and
//! store, FP and AdvSIMD classes are in the streams: the inline forms of
//! the emitter, its spills and guards, the loads and stores over its fast
//! TLB with their misses, and the thunk with its accesses of 16 bytes,
//! generic operations and helper calls.
//!
//! Every other round keeps the accesses on both sides, as the lockstep
//! does, which compiles the blocks of the compiled side so that their hits
//! of the fast TLB are kept too, and the two lists of accesses are
//! compared.
//!
//! The rounds of `run_flags` are for the flags (card J5, H8): only the
//! integer and branch classes, which hold every instruction that sets or
//! reads the flags, on registers at the edges of the orders of 32 and 64
//! bits, many of them equal or one apart, so that every condition of a
//! compare flips inside a round (U4).
//!
//! The rounds of `run_joins` are for superblocks (card J14): the images of
//! `run` with direct branches to other instructions of the image put in, so
//! that the blocks of the compiled side join at them and leave through
//! their side exits, while the interpreter, which makes no superblock, runs
//! the same code block by block.
//!
//! `ci::check` runs a bounded number of rounds with a fixed seed (the test
//! `tests/jit.rs`); `just tcg-a64-jit-diff` runs as many as it is asked.
//!
//! `Options` turn K3 on for the compiled side, which then checks each of
//! its blocks against the interpreter as it runs, and put a defect into the
//! emitter, which both proofs must catch (3e DESIGN.md J2).
//!
//! Sources read: none. This file holds no instruction semantics.

use super::stream::Generator;
use crate::FormatClass;
use crate::exec::{Cpu, JitDefect};
use crate::lift::{
    BRANCH_FORMATS, FP_FORMATS, IMMEDIATE_FORMATS, LOAD_STORE_FORMATS, REGISTER_FORMATS,
    SIMD_FORMATS,
};
use crate::state::State;
use crate::sysreg::reg;
use vmlab_hv_tcg::FlatMemory;

const RAM_BASE: u64 = 0x4000_0000;
const RAM_BYTES: usize = 0x6_0000;
/// The vector table: every entry is `HVC #0`.
const VECTORS: u64 = RAM_BASE;
/// The image of a round.
const IMAGE: u64 = RAM_BASE + 0x1_0000;
/// The window of data that the loads and stores mostly reach.
const WINDOW: (u64, u64) = (RAM_BASE + 0x4_0000, 0x1_0000);
const HVC: u32 = 0xd400_0002;
/// The instructions of a run: a round that loops ends here.
const BUDGET: u64 = 300;

/// The counts of a run of rounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub rounds: u64,
    /// Instructions that the compiled side ran, and those of them that ran
    /// in the interpreter because the emitter refused their block.
    pub instructions: u64,
    pub interpreted: u64,
    pub compiled_blocks: u64,
    pub refused_blocks: u64,
    /// Runs of blocks that K3 checked.
    pub checked_blocks: u64,
    /// The inlined loads and stores of the compiled side, and those that
    /// missed its fast TLB (3e DESIGN.md H7).
    pub tlb_accesses: u64,
    pub tlb_misses: u64,
    /// Comparisons compiled as host flags, and lazy conditions inline.
    pub fused: u64,
    pub lazy: u64,
    /// The joins of the superblocks that the compiled side made, and those
    /// with a side exit (J14).
    pub joins: u64,
    pub side_exits: u64,
}

/// The first round where the two sides differ.
#[derive(Debug)]
pub struct Difference {
    pub seed: u64,
    pub round: u64,
    pub words: Vec<u32>,
    pub what: String,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let words: Vec<String> = self
            .words
            .iter()
            .map(|word| format!("{word:08x}"))
            .collect();
        write!(
            f,
            "seed {:#x}, round {}: {}\nimage: {}",
            self.seed,
            self.round,
            self.what,
            words.join(" ")
        )
    }
}

fn formats() -> Vec<FormatClass> {
    [
        IMMEDIATE_FORMATS.as_slice(),
        REGISTER_FORMATS.as_slice(),
        BRANCH_FORMATS.as_slice(),
        LOAD_STORE_FORMATS.as_slice(),
        FP_FORMATS.as_slice(),
        SIMD_FORMATS.as_slice(),
    ]
    .concat()
}

/// The ways a CPU runs its blocks, for the tests that must hold on each
/// (3e DESIGN.md K6): the interpreter, the JIT, and the JIT with K3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runner {
    Interpreter,
    Jit,
    Checked,
}

impl Runner {
    pub const ALL: [Runner; 3] = [Runner::Interpreter, Runner::Jit, Runner::Checked];

    /// Make `cpu` run its blocks this way. Its translated blocks are
    /// dropped when K3 turns on.
    pub fn apply<M: vmlab_hv_tcg::Memory>(self, cpu: &mut Cpu<M>) {
        let started = match self {
            Runner::Interpreter => cpu.set_jit_check(false).and_then(|()| cpu.set_jit(false)),
            Runner::Jit => cpu.set_jit_check(false).and_then(|()| cpu.set_jit(true)),
            Runner::Checked => cpu.set_jit_check(true),
        };
        if let Err(error) = started {
            panic!("the JIT does not start: {error}");
        }
        assert_eq!(cpu.jit_enabled(), self != Runner::Interpreter);
    }
}

/// How the compiled side runs.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// K3 checks every compiled block.
    pub check: bool,
    /// A defect in the emitter.
    pub defect: JitDefect,
}

fn cpu(memory: FlatMemory, state: State, jit: Option<Options>) -> Result<Cpu<FlatMemory>, String> {
    let mut cpu = Cpu::new(memory, state);
    let started = match jit {
        Some(options) if options.check => cpu.set_jit_check(true),
        Some(_) => cpu.set_jit(true),
        None => cpu.set_jit(false).and_then(|()| cpu.set_jit_check(false)),
    };
    started.map_err(|error| format!("the JIT does not start: {error}"))?;
    if let Some(options) = jit {
        cpu.seed_jit_defect(options.defect);
    }
    Ok(cpu)
}

fn flag_formats() -> Vec<FormatClass> {
    [
        IMMEDIATE_FORMATS.as_slice(),
        REGISTER_FORMATS.as_slice(),
        BRANCH_FORMATS.as_slice(),
    ]
    .concat()
}

/// Registers at the edges of the orders: each one an edge, a neighbour of
/// one, or a register before it, so that a compare of two registers is
/// often of equal values or of values one apart.
fn edge_registers(generator: &mut Generator) -> ([u64; 31], u64, u8) {
    const EDGES: [u64; 10] = [
        0,
        1,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_ffff,
        0x1_0000_0000,
        0x7fff_ffff_ffff_ffff,
        0x8000_0000_0000_0000,
        0xffff_ffff_8000_0000,
        u64::MAX,
    ];
    let rng = &mut generator.rng;
    let mut x = [0u64; 31];
    for n in 0..31 {
        x[n] = match rng.below(8) {
            0 | 1 if n > 0 => x[rng.below(n as u64) as usize],
            2 | 3 => EDGES[rng.below(EDGES.len() as u64) as usize]
                .wrapping_add(rng.below(3))
                .wrapping_sub(1),
            4 => rng.below(17),
            _ => EDGES[rng.below(EDGES.len() as u64) as usize],
        };
    }
    let sp = x[rng.below(31) as usize];
    let nzcv = rng.below(16) as u8;
    (x, sp, nzcv)
}

/// Run `rounds` rounds from `seed`.
pub fn run(seed: u64, rounds: u64) -> Result<Report, Difference> {
    run_with(seed, rounds, Options::default())
}

/// Run `rounds` rounds of the flags from `seed`: the integer and branch
/// classes on registers at the edges.
pub fn run_flags(seed: u64, rounds: u64) -> Result<Report, Difference> {
    run_rounds(seed, rounds, Options::default(), Rounds::Flags)
}

/// Run `rounds` rounds from `seed`, the compiled side as `options` say.
pub fn run_with(seed: u64, rounds: u64, options: Options) -> Result<Report, Difference> {
    run_rounds(seed, rounds, options, Rounds::Plain)
}

/// Run `rounds` rounds of superblocks from `seed` (J14): direct branches
/// between the instructions of each image.
pub fn run_joins(seed: u64, rounds: u64, options: Options) -> Result<Report, Difference> {
    run_rounds(seed, rounds, options, Rounds::Joins)
}

/// What the images of a run of rounds hold.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rounds {
    Plain,
    Flags,
    Joins,
}

/// A direct branch from the instruction `from` of an image to its
/// instruction `to`: `B`, `B.cond`, `CBZ`, `CBNZ`, `TBZ` or `TBNZ` on a
/// random condition, register and bit.
fn branch_between(rng: &mut super::stream::Rng, from: usize, to: usize) -> u32 {
    let offset = to as i64 - from as i64;
    let rt = rng.below(31) as u32;
    match rng.below(6) {
        0 => 0x1400_0000 | (offset as u32 & 0x3ff_ffff),
        1 | 2 => 0x5400_0000 | ((offset as u32 & 0x7_ffff) << 5) | rng.below(16) as u32,
        3 | 4 => {
            let sf = rng.below(2) as u32;
            let nonzero = rng.below(2) as u32;
            sf << 31 | 0x3400_0000 | nonzero << 24 | (offset as u32 & 0x7_ffff) << 5 | rt
        }
        _ => {
            let bit = rng.below(64) as u32;
            let nonzero = rng.below(2) as u32;
            (bit >> 5) << 31
                | 0x3600_0000
                | nonzero << 24
                | (bit & 31) << 19
                | (offset as u32 & 0x3fff) << 5
                | rt
        }
    }
}

fn run_rounds(
    seed: u64,
    rounds: u64,
    options: Options,
    kind: Rounds,
) -> Result<Report, Difference> {
    let flags = kind == Rounds::Flags;
    let mut generator = Generator::new(seed);
    let formats = if flags { flag_formats() } else { formats() };
    let mut report = Report::default();
    for round in 0..rounds {
        let count = 1 + generator.rng.below(64) as usize;
        let (mut words, _) = generator.image(IMAGE, count, |rng| {
            formats[rng.below(formats.len() as u64) as usize]
        });
        if kind == Rounds::Joins {
            // About one instruction in five branches to another of the
            // image, before or after it, or to the HVC after its end.
            for (from, word) in words.iter_mut().enumerate().take(count) {
                if generator.rng.chance(1, 5) {
                    let to = generator.rng.below(count as u64 + 1) as usize;
                    *word = branch_between(&mut generator.rng, from, to);
                }
            }
        }
        let difference = |what: String| Difference {
            seed,
            round,
            words: words.clone(),
            what,
        };
        let mut bytes = vec![0u8; RAM_BYTES];
        for entry in 0..16 {
            let at = entry * 0x80;
            bytes[at..at + 4].copy_from_slice(&HVC.to_le_bytes());
        }
        let data = generator.memory_bytes(WINDOW, WINDOW.1 as usize);
        let window = (WINDOW.0 - RAM_BASE) as usize;
        bytes[window..window + data.len()].copy_from_slice(&data);
        let image = (IMAGE - RAM_BASE) as usize;
        for (n, word) in words.iter().enumerate() {
            bytes[image + 4 * n..image + 4 * n + 4].copy_from_slice(&word.to_le_bytes());
        }
        let (x, sp, nzcv) = if flags {
            edge_registers(&mut generator)
        } else if generator.rng.chance(1, 2) {
            generator.memory_registers(WINDOW)
        } else {
            generator.registers((IMAGE, 4 * count as u64))
        };
        let mut state = State::reset(IMAGE);
        state.system.regs[reg::VBAR_EL1] = VECTORS;
        for (n, value) in x.iter().enumerate() {
            state.set_x(n, *value);
        }
        state.set_sp(sp);
        state.set_nzcv(nzcv);
        if generator.rng.chance(1, 2) {
            state.enable_fp();
            let vectors = if generator.rng.chance(1, 2) {
                generator.vectors()
            } else {
                generator.simd_vectors()
            };
            for (n, value) in vectors.v.iter().enumerate() {
                state.set_v(n, *value);
            }
            state.set_fpcr(vectors.fpcr);
            state.set_fpsr(vectors.fpsr);
        }
        let mut reference = cpu(
            FlatMemory::new(RAM_BASE, bytes.clone()),
            state.clone(),
            None,
        )
        .map_err(difference)?;
        let mut compiled =
            cpu(FlatMemory::new(RAM_BASE, bytes), state, Some(options)).map_err(difference)?;
        // Every other round keeps the accesses on both sides, as the
        // lockstep does: the compiled code then keeps those of its hits
        // (`jit::Memory::Logged`), and the two lists must be the same.
        let logged = round % 2 == 1;
        reference.log_accesses(logged);
        compiled.log_accesses(logged);
        let expected = reference.run(BUDGET);
        let got = compiled.run(BUDGET);
        if format!("{got:?}") != format!("{expected:?}") {
            return Err(difference(format!(
                "the run stopped with {got:?}, the interpreter with {expected:?}"
            )));
        }
        if compiled.state != reference.state {
            return Err(difference(format!(
                "the states differ:\ncompiled    {:?}\ninterpreter {:?}",
                compiled.state, reference.state
            )));
        }
        let counts = (compiled.instructions, compiled.exceptions);
        if counts != (reference.instructions, reference.exceptions) {
            return Err(difference(format!(
                "instructions and exceptions {counts:?}, the interpreter {:?}",
                (reference.instructions, reference.exceptions)
            )));
        }
        let (ours, theirs) = (compiled.take_accesses(), reference.take_accesses());
        if ours != theirs {
            let at = ours.iter().zip(&theirs).position(|(a, b)| a != b);
            return Err(difference(format!(
                "the accesses differ{}: compiled {:x?}, the interpreter {:x?}",
                at.map_or(String::new(), |at| format!(" first at {at}")),
                at.map_or(&ours[..], |at| &ours[at..(at + 1).min(ours.len())]),
                at.map_or(&theirs[..], |at| &theirs[at..(at + 1).min(theirs.len())]),
            )));
        }
        let (ours, theirs) = (compiled.memory.bytes(), reference.memory.bytes());
        if let Some(at) = ours.iter().zip(theirs).position(|(a, b)| a != b) {
            return Err(difference(format!(
                "the memory differs first at {:#x}: {:#04x}, the interpreter {:#04x}",
                RAM_BASE + at as u64,
                ours[at],
                theirs[at]
            )));
        }
        let jit = compiled.jit_counts();
        report.rounds += 1;
        report.instructions += compiled.instructions;
        report.interpreted += jit.interpreted_instructions;
        report.compiled_blocks += jit.compiled;
        report.refused_blocks += jit.refused;
        report.checked_blocks += jit.checked_runs;
        report.tlb_accesses += jit.tlb_accesses;
        report.tlb_misses += jit.tlb_misses;
        report.fused += jit.fused;
        report.lazy += jit.lazy;
        report.joins += compiled.joins;
        report.side_exits += compiled.side_exits;
    }
    Ok(report)
}
