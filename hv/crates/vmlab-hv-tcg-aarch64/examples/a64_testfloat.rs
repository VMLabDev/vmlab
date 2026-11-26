//! Berkeley TestFloat against the FP helpers (DESIGN.md O5). `just
//! tcg-a64-testfloat` in `hv/` builds `testfloat_gen` from the pinned
//! upstream sources and runs this.
//!
//! Usage:
//!   a64_testfloat --gen <testfloat_gen> [--level 1|2] [--seed S]
//!       [--sample-out <file>] [--samples N]
//!
//! Every function of the helpers runs in every rounding mode that it has.
//! `testfloat_gen` writes the cases with the results of upstream SoftFloat,
//! and each one goes through `helpers/fp.rs`. The first cases that differ
//! are printed and the run ends with exit code 1. `--sample-out` writes N
//! cases of each run, for `tests/testfloat.rs`.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Instant;
use vmlab_hv_tcg_aarch64::oracle::stream::Rng;
use vmlab_hv_tcg_aarch64::oracle::testfloat::{
    Verdict, cases, check, generator_arguments, mode_name, sample_line,
};

type Failure = Box<dyn std::error::Error>;

/// The differences that are printed before the run gives up.
const SHOWN: usize = 10;

fn main() -> Result<(), Failure> {
    let (mut generator, mut sample_out) = (None, None);
    let (mut level, mut seed, mut samples) = (1u32, 1u64, 16usize);
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().ok_or(format!("{argument} needs a value"));
        match argument.as_str() {
            "--gen" => generator = Some(value()?),
            "--level" => level = value()?.parse()?,
            "--seed" => seed = value()?.parse()?,
            "--sample-out" => sample_out = Some(value()?),
            "--samples" => samples = value()?.parse()?,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let generator = generator.ok_or("give --gen <testfloat_gen>")?;
    let started = Instant::now();
    let mut rng = Rng(seed);
    let (mut total, mut skipped, mut differences) = (0u64, 0u64, 0usize);
    let mut sample_text = format!(
        "# Test cases of Berkeley TestFloat (testfloat_gen -level {level} -tininessbefore) with the\n\
         # results of upstream SoftFloat 3e, ARM-VFPv2 specialisation: {samples} of each function\n\
         # and rounding mode. Made by `just tcg-a64-testfloat` (examples/a64_testfloat.rs\n\
         # --sample-out). A line is the function, the mode, exact or notexact, the operands,\n\
         # the result and SoftFloat's exception flags.\n"
    );
    println!("testfloat level={level} generator={generator}");
    for case in cases() {
        let mut child = Command::new(&generator)
            .args(generator_arguments(&case, level))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let output = child.stdout.take().ok_or("the generator has no output")?;
        let (mut same, mut skipped_here) = (0u64, 0u64);
        let mut kept: Vec<String> = Vec::new();
        for line in BufReader::new(output).lines() {
            let line = line?;
            match check(&case, &line) {
                Ok(Verdict::Same) => {
                    same += 1;
                    // Reservoir sampling: each case has the same chance.
                    if kept.len() < samples {
                        kept.push(line);
                    } else {
                        let slot = rng.below(same) as usize;
                        if slot < samples {
                            kept[slot] = line;
                        }
                    }
                }
                Ok(Verdict::Skipped) => skipped_here += 1,
                Err(difference) => {
                    differences += 1;
                    if differences <= SHOWN {
                        eprintln!("difference: {difference}");
                    }
                }
            }
        }
        let status = child.wait()?;
        if !status.success() || same + skipped_here == 0 {
            return Err(format!(
                "testfloat_gen gave no test case for {} ({status})",
                case.function
            )
            .into());
        }
        println!(
            "  {:<16} {:<12} {:<9} {same:>9} the same, {skipped_here:>7} not compared",
            case.function,
            mode_name(case.mode),
            if case.exact { "exact" } else { "notexact" },
        );
        total += same;
        skipped += skipped_here;
        for line in &kept {
            sample_text.push_str(&sample_line(&case, line));
            sample_text.push('\n');
        }
        if differences > SHOWN {
            break;
        }
    }
    if differences > 0 {
        return Err(format!("{differences} test cases differ").into());
    }
    println!(
        "total: {total} test cases the same in {} runs, {skipped} not compared, no difference, {:.0} s",
        cases().len(),
        started.elapsed().as_secs_f64()
    );
    if let Some(path) = sample_out {
        std::fs::write(&path, sample_text)?;
        println!("wrote the sample to {path}");
    }
    Ok(())
}
