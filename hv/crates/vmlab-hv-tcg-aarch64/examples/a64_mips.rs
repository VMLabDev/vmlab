//! The speed of the interpreter loop on a branch-and-add image (card C1,
//! DESIGN.md M6): `add x0, x0, #1 ; subs x1, x1, #1 ; b.ne` for a count of
//! iterations.
//!
//! Usage: `a64_mips [iterations] [repeats]`

use std::time::Instant;
use vmlab_hv_tcg::FlatMemory;
use vmlab_hv_tcg_aarch64::{Cpu, State, Stop};

const BASE: u64 = 0x4000_0000;
const LOOP: [u32; 3] = [0x9100_0400, 0xf100_0421, 0x54ff_ffc1];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let iterations: u64 = arguments
        .next()
        .map_or(Ok(100_000_000), |text| text.parse())?;
    let repeats: usize = arguments.next().map_or(Ok(3), |text| text.parse())?;
    if iterations == 0 || repeats == 0 {
        return Err("iterations and repeats must be positive".into());
    }
    let mut rates = Vec::new();
    for _ in 0..repeats {
        let mut data = vec![0u8; 4096];
        for (index, word) in LOOP.iter().enumerate() {
            data[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let mut cpu = Cpu::new(FlatMemory::new(BASE, data), State::reset(BASE));
        cpu.state.set_x(1, iterations);
        let start = Instant::now();
        let stop = cpu.run(iterations * 3);
        let seconds = start.elapsed().as_secs_f64();
        if stop != Stop::Budget
            || cpu.state.x(0) != iterations
            || cpu.state.x(1) != 0
            || cpu.state.pc() != BASE + 12
            || cpu.state.nzcv() != 0b0110
        {
            return Err(format!("the loop ended wrong: {stop:?} {:?}", cpu.state).into());
        }
        let mips = cpu.instructions as f64 / seconds / 1e6;
        println!(
            "instructions={} seconds={seconds:.6} MIPS={mips:.2} translations={}",
            cpu.instructions, cpu.translations
        );
        rates.push(mips);
    }
    rates.sort_by(f64::total_cmp);
    println!("median MIPS={:.2} of {repeats}", rates[rates.len() / 2]);
    Ok(())
}
