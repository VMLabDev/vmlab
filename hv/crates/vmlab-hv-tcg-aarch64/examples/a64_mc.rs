//! The decoder against `llvm-mc --show-inst` (DESIGN.md G1, M1): LLVM prints
//! the record name of an encoding, so it is an exact oracle for the decoder.
//!
//! Usage:
//!   a64_mc --llvm-mc <bin> [--count N] [--seed S] [--jobs J]
//!       compare `--count` random words of the selected sets.
//!   a64_mc --llvm-mc <bin> --vectors-out <file>
//!       write the committed vector file: words of every selected record,
//!       and random words, each with the name that LLVM gives.
//!   a64_mc --llvm-mc <bin> --vectors-check <file>
//!       make the vector file again in memory and compare.
//!
//! A word where LLVM names a record and our decoder says Undefined is
//! allowed only where an entry of `tables/constraints.toml` refuses that
//! record (G1d). The run counts those per record. Any other difference fails.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use vmlab_hv_tcg_aarch64::generated::decode::{FEATURE_ATTRIBUTES, RECORDS};
use vmlab_hv_tcg_aarch64::oracle::stream::Rng;
use vmlab_hv_tcg_aarch64::{decode, decode_unconstrained};

type Failure = Box<dyn std::error::Error + Send + Sync>;

/// The record names that `llvm-mc` gives for `words`. `None`: LLVM calls the
/// encoding invalid.
fn llvm_names(llvm_mc: &str, words: &[u32]) -> Result<Vec<Option<String>>, Failure> {
    let mut input = String::with_capacity(words.len() * 20);
    for word in words {
        let [a, b, c, d] = word.to_le_bytes();
        input.push_str(&format!("{a:#04x} {b:#04x} {c:#04x} {d:#04x}\n"));
    }
    let attributes: Vec<String> = FEATURE_ATTRIBUTES
        .iter()
        .map(|name| format!("+{name}"))
        .collect();
    let mut child = Command::new(llvm_mc)
        .args(["--disassemble", "--show-inst", "--triple=aarch64"])
        .arg(format!("-mattr={}", attributes.join(",")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("llvm-mc has no stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| "the writer of llvm-mc's input panicked")??;
    if !output.status.success() {
        return Err(format!(
            "llvm-mc failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("")
        )
        .into());
    }
    // A warning names the line of an invalid encoding: `<stdin>:LINE:COL: warning: invalid ...`.
    let mut invalid = vec![false; words.len()];
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        if !line.contains("invalid instruction encoding") {
            continue;
        }
        let number: usize = line
            .split(':')
            .nth(1)
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| format!("a warning of llvm-mc without a line number: {line}"))?;
        *invalid
            .get_mut(number.wrapping_sub(1))
            .ok_or("a warning of llvm-mc for a line that was not sent")? = true;
    }
    // Each valid word gives one `<MCInst #number NAME` in the order of the input.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut names = stdout.lines().filter_map(|line| {
        let rest = line.split("<MCInst #").nth(1)?;
        rest.split_whitespace().nth(1).map(|name| name.trim_end_matches('>').to_string())
    });
    let mut result = Vec::with_capacity(words.len());
    for bad in invalid {
        if bad {
            result.push(None);
        } else {
            result.push(Some(names.next().ok_or(
                "llvm-mc printed fewer instructions than it took valid words",
            )?));
        }
    }
    if names.next().is_some() {
        return Err("llvm-mc printed more instructions than it took words".into());
    }
    Ok(result)
}

/// The fixed bits of a record with random free bits.
fn of_record(rng: &mut Rng, index: usize) -> u32 {
    let info = &RECORDS[index];
    info.value | (rng.next_u64() as u32 & !info.mask)
}

#[derive(Default)]
struct Tally {
    words: u64,
    same_record: u64,
    both_undefined: u64,
    /// Our constraint refuses what LLVM names, by record.
    constrained: BTreeMap<&'static str, u64>,
    different: u64,
    /// The first differences, for the report.
    examples: Vec<String>,
}

impl Tally {
    fn add(&mut self, word: u32, llvm: Option<&str>) {
        self.words += 1;
        let ours = decode(word).map(|decoded| decoded.record.name());
        match (ours, llvm) {
            (Some(a), Some(b)) if a == b => self.same_record += 1,
            (None, None) => self.both_undefined += 1,
            _ => {
                // LLVM names a record that one of our constraints refuses.
                let refused = decode_unconstrained(word)
                    .filter(|record| record.info().constrained && Some(record.name()) == llvm);
                if let (None, Some(record)) = (ours, refused) {
                    *self.constrained.entry(record.name()).or_default() += 1;
                    return;
                }
                self.different += 1;
                if self.examples.len() < 40 {
                    self.examples.push(format!(
                        "{word:08x}: ours {} / llvm-mc {}",
                        ours.unwrap_or("(undefined)"),
                        llvm.unwrap_or("(undefined)")
                    ));
                }
            }
        }
    }

    fn merge(&mut self, other: Tally) {
        self.words += other.words;
        self.same_record += other.same_record;
        self.both_undefined += other.both_undefined;
        self.different += other.different;
        for (record, count) in other.constrained {
            *self.constrained.entry(record).or_default() += count;
        }
        for example in other.examples {
            if self.examples.len() < 40 {
                self.examples.push(example);
            }
        }
    }

    fn report(&self) {
        println!(
            "words={} same_record={} both_undefined={} refused_by_a_constraint={} different={}",
            self.words,
            self.same_record,
            self.both_undefined,
            self.constrained.values().sum::<u64>(),
            self.different
        );
        for (record, count) in &self.constrained {
            println!("  constraint refuses {record}: {count} words that llvm-mc names");
        }
        for example in &self.examples {
            println!("  different {example}");
        }
    }
}

/// The words of the vector file: for each selected record its fixed bits
/// with the free bits zero, all ones, and twice random. Then random words.
fn vector_words(seed: u64) -> Vec<u32> {
    let mut rng = Rng(seed);
    let mut words = Vec::new();
    for (index, info) in RECORDS.iter().enumerate() {
        words.push(info.value);
        words.push(info.value | !info.mask);
        words.push(of_record(&mut rng, index));
        words.push(of_record(&mut rng, index));
    }
    for _ in 0..2000 {
        words.push(rng.next_u64() as u32);
    }
    words
}

fn vector_file(llvm_mc: &str) -> Result<String, Failure> {
    let words = vector_words(VECTOR_SEED);
    let names = llvm_names(llvm_mc, &words)?;
    let mut text = String::from(
        "# Encodings with the record name that `llvm-mc --show-inst` of the pinned LLVM\n\
         # gives, or `-` for an encoding that it calls invalid. Made by\n\
         # `just tcg-a64-generate` (examples/a64_mc.rs). Do not edit.\n\
         #\n\
         # Derived from the LLVM Project, under the Apache License v2.0 with LLVM\n\
         # Exceptions. SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception\n",
    );
    for (word, name) in words.iter().zip(&names) {
        text.push_str(&format!("{word:08x} {}\n", name.as_deref().unwrap_or("-")));
    }
    Ok(text)
}

/// The seed of the vector file. A change of it changes the committed file.
const VECTOR_SEED: u64 = 0x6136_3476_6563;

fn compare(llvm_mc: &str, count: u64, seed: u64, jobs: u64) -> Result<Tally, Failure> {
    const CHUNK: u64 = 250_000;
    let chunks = count.div_ceil(CHUNK);
    let next = std::sync::atomic::AtomicU64::new(0);
    let total = std::sync::Mutex::new(Tally::default());
    let failure = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| {
                loop {
                    let chunk = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if chunk >= chunks {
                        return;
                    }
                    // Each chunk has a seed of its own, so the words do not
                    // depend on the count of jobs.
                    let mut rng = Rng(seed ^ chunk.wrapping_mul(0x9e37_79b9_7f4a_7c15));
                    let size = CHUNK.min(count - chunk * CHUNK);
                    let words: Vec<u32> = (0..size)
                        .map(|_| {
                            // Nine words in ten are of a selected record.
                            if rng.below(10) == 0 {
                                rng.next_u64() as u32
                            } else {
                                let index = rng.below(RECORDS.len() as u64) as usize;
                                of_record(&mut rng, index)
                            }
                        })
                        .collect();
                    match llvm_names(llvm_mc, &words) {
                        Ok(names) => {
                            let mut tally = Tally::default();
                            for (word, name) in words.iter().zip(&names) {
                                tally.add(*word, name.as_deref());
                            }
                            if let Ok(mut total) = total.lock() {
                                total.merge(tally);
                            }
                        }
                        Err(error) => {
                            if let Ok(mut failure) = failure.lock() {
                                *failure = Some(error.to_string());
                            }
                            return;
                        }
                    }
                }
            });
        }
    });
    if let Some(error) = failure.into_inner().map_err(|_| "a worker panicked")? {
        return Err(error.into());
    }
    Ok(total.into_inner().map_err(|_| "a worker panicked")?)
}

fn main() -> Result<(), Failure> {
    let mut llvm_mc = String::from("llvm-mc");
    let (mut count, mut seed, mut jobs) = (10_000_000u64, 1u64, 8u64);
    let (mut vectors_out, mut vectors_check) = (None, None);
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().ok_or(format!("{argument} needs a value"));
        match argument.as_str() {
            "--llvm-mc" => llvm_mc = value()?,
            "--count" => count = value()?.parse()?,
            "--seed" => seed = value()?.parse()?,
            "--jobs" => jobs = value()?.parse()?,
            "--vectors-out" => vectors_out = Some(value()?),
            "--vectors-check" => vectors_check = Some(value()?),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if let Some(path) = vectors_out {
        let text = vector_file(&llvm_mc)?;
        std::fs::write(&path, &text)?;
        println!("wrote {} vectors to {path}", text.lines().count() - 6);
        return Ok(());
    }
    if let Some(path) = vectors_check {
        if std::fs::read_to_string(&path)? != vector_file(&llvm_mc)? {
            return Err(format!("{path} differs from what llvm-mc gives: run `just tcg-a64-generate`").into());
        }
        println!("{path} is what llvm-mc gives");
        return Ok(());
    }
    let tally = compare(&llvm_mc, count, seed, jobs)?;
    println!("seed={seed} records={}", RECORDS.len());
    tally.report();
    if tally.different != 0 {
        return Err(format!("{} words decode to another record than llvm-mc names", tally.different).into());
    }
    Ok(())
}
