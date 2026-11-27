//! The block differential of the JIT (3e DESIGN.md K2) for as many rounds
//! as asked: `just tcg-a64-jit-diff` runs this.
//!
//! Usage: `a64_jitdiff [ROUNDS] [SEED] [flags]`. With `flags` it runs the
//! rounds of the flags (card J5): the integer and branch classes on
//! registers at the edges. It prints one line of JSON with the seed and the
//! counts, or the first difference and exit code 1.

use vmlab_hv_tcg_aarch64::oracle::jitdiff;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let rounds = arguments
        .first()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1_000_000u64);
    let seed = arguments
        .get(1)
        .and_then(|value| {
            let value = value.trim_start_matches("0x");
            u64::from_str_radix(value, 16).ok()
        })
        .unwrap_or(0x3e_0001);
    let flags = arguments.get(2).is_some_and(|mode| mode == "flags");
    let started = std::time::Instant::now();
    let result = if flags {
        jitdiff::run_flags(seed, rounds)
    } else {
        jitdiff::run(seed, rounds)
    };
    match result {
        Ok(report) => println!(
            "{{\"seed\": \"{seed:#x}\", \"flags\": {flags}, \"rounds\": {}, \"differences\": 0, \
             \"instructions\": {}, \"interpreted_instructions\": {}, \
             \"compiled_blocks\": {}, \"refused_blocks\": {}, \"fused\": {}, \"lazy\": {}, \
             \"seconds\": {:.1}}}",
            report.rounds,
            report.instructions,
            report.interpreted,
            report.compiled_blocks,
            report.refused_blocks,
            report.fused,
            report.lazy,
            started.elapsed().as_secs_f64()
        ),
        Err(difference) => {
            eprintln!("{difference}");
            std::process::exit(1);
        }
    }
}
