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
//! the emitter, its spills and guards, and the thunk with its loads,
//! stores, accesses of 16 bytes, generic operations and helper calls.
//!
//! `ci::check` runs a bounded number of rounds with a fixed seed (the test
//! `tests/jit.rs`); `just tcg-a64-jit-diff` runs as many as it is asked.
//!
//! Sources read: none. This file holds no instruction semantics.

use super::stream::Generator;
use crate::exec::Cpu;
use crate::lift::{
    BRANCH_FORMATS, FP_FORMATS, IMMEDIATE_FORMATS, LOAD_STORE_FORMATS, REGISTER_FORMATS,
    SIMD_FORMATS,
};
use crate::state::State;
use crate::sysreg::reg;
use crate::FormatClass;
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
        let words: Vec<String> = self.words.iter().map(|word| format!("{word:08x}")).collect();
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

fn cpu(memory: FlatMemory, state: State, jit: bool) -> Result<Cpu<FlatMemory>, String> {
    let mut cpu = Cpu::new(memory, state);
    cpu.set_jit(jit)
        .map_err(|error| format!("the JIT does not start: {error}"))?;
    Ok(cpu)
}

/// Run `rounds` rounds from `seed`.
pub fn run(seed: u64, rounds: u64) -> Result<Report, Difference> {
    let mut generator = Generator::new(seed);
    let formats = formats();
    let mut report = Report::default();
    for round in 0..rounds {
        let count = 1 + generator.rng.below(64) as usize;
        let (words, _) = generator.image(IMAGE, count, |rng| {
            formats[rng.below(formats.len() as u64) as usize]
        });
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
        let (x, sp, nzcv) = if generator.rng.chance(1, 2) {
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
        let mut reference = cpu(FlatMemory::new(RAM_BASE, bytes.clone()), state.clone(), false)
            .map_err(difference)?;
        let mut compiled = cpu(FlatMemory::new(RAM_BASE, bytes), state, true).map_err(difference)?;
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
    }
    Ok(report)
}
