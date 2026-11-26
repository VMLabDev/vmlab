//! The stream generator of the lockstep (DESIGN.md O3, step 1): random
//! instructions of chosen format classes from our own decode tables, then
//! the constraints, then boundary values.
//!
//! An image is a run of such instructions. The generator shapes the
//! PC-relative fields so that most branches land inside the image, because a
//! branch that leaves it ends the round.

use crate::fields::sign_extend;
use crate::generated::decode::{FormatClass, RECORDS, Record, decode};
use crate::{field, field_width, with_field};

/// SplitMix64: small, seedable, and the same on every host.
#[derive(Clone, Debug)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value below `bound`, which is not 0.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// True `numerator` times out of `denominator`.
    pub fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.below(denominator) < numerator
    }
}

/// The selected records of a format class.
pub fn records_of(format: FormatClass) -> Vec<Record> {
    RECORDS
        .iter()
        .enumerate()
        .filter(|(_, info)| info.format == format)
        .filter_map(|(index, _)| Record::from_index(index))
        .collect()
}

/// The name and the width of the field that holds a PC-relative offset in
/// instructions, for the records that have one.
fn branch_field(record: Record) -> Option<(&'static str, u32)> {
    let name = match record.info().format {
        FormatClass::BranchImm | FormatClass::CallImm => "addr",
        FormatClass::BranchCond | FormatClass::BaseCmpBranch | FormatClass::BaseTestBranch => {
            "target"
        }
        _ => return None,
    };
    Some((name, field_width(record, name)?))
}

/// How a word was made, for the counts of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// It decodes to the record that it was made for.
    Valid,
    /// Random free bits with no constraint applied: it may decode to
    /// another record or to none. Both sides must agree on which.
    Unconstrained,
}

pub struct Generator {
    pub rng: Rng,
    /// One word in this many is made without the constraints. 0: none.
    pub unconstrained_one_in: u64,
}

impl Generator {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: Rng(seed),
            unconstrained_one_in: 200,
        }
    }

    /// The fixed bits of `record` with random free bits.
    fn raw(&mut self, record: Record) -> u32 {
        let info = record.info();
        info.value | (self.rng.next_u64() as u32 & !info.mask)
    }

    /// Put boundary values into some fields: zero, all ones (register 31
    /// for a register field), and one.
    fn boundaries(&mut self, record: Record, mut word: u32) -> u32 {
        for info in record.info().fields {
            if !self.rng.chance(1, 4) {
                continue;
            }
            let Some(width) = field_width(record, info.name) else {
                continue;
            };
            let ones = if width >= 32 {
                u32::MAX
            } else {
                (1 << width) - 1
            };
            let value = match self.rng.below(4) {
                0 => 0,
                1 | 2 => ones,
                _ => 1,
            };
            word = with_field(record, info.name, word, value).unwrap_or(word);
        }
        word
    }

    /// A random instruction of `record` for the address `at`. A PC-relative
    /// field lands inside `image` (first address, length in bytes) nine
    /// times out of ten, when the field reaches it.
    pub fn instruction(&mut self, record: Record, at: u64, image: (u64, u64)) -> (u32, Kind) {
        if self.unconstrained_one_in != 0 && self.rng.chance(1, self.unconstrained_one_in) {
            return (self.raw(record), Kind::Unconstrained);
        }
        for _ in 0..1000 {
            let mut word = self.raw(record);
            word = self.boundaries(record, word);
            if let Some((name, width)) = branch_field(record)
                && self.rng.chance(9, 10)
            {
                let target = image.0 + 4 * self.rng.below(image.1 / 4);
                let offset = (target.wrapping_sub(at) as i64) >> 2;
                let encoded = offset as u64 & ((1u64 << width) - 1);
                if sign_extend(encoded, width) == offset {
                    word = with_field(record, name, word, encoded as u32).unwrap_or(word);
                }
            }
            if decode(word).is_some_and(|decoded| decoded.record == record) {
                return (word, Kind::Valid);
            }
        }
        // No free bits satisfy the constraints: the caller sees it decode
        // to something else and counts it as unconstrained.
        (self.raw(record), Kind::Unconstrained)
    }

    /// `ADR Xd, target`, to give a register branch a target inside the image.
    pub fn adr(&mut self, register: u32, at: u64, target: u64) -> Option<u32> {
        let offset = target.wrapping_sub(at) as i64;
        if !(-(1 << 20)..(1 << 20)).contains(&offset) {
            return None;
        }
        let word = Record::ADR.info().value;
        let word = with_field(Record::ADR, "label", word, offset as u32 & 0x1f_ffff)?;
        with_field(Record::ADR, "xd", word, register)
    }

    /// An image of `count` instructions at `base`, each of a format class
    /// that `choose` gives. Returns the words and the count of unconstrained
    /// ones.
    pub fn image(
        &mut self,
        base: u64,
        count: usize,
        mut choose: impl FnMut(&mut Rng) -> FormatClass,
    ) -> (Vec<u32>, u64) {
        let size = (count as u64) * 4;
        let mut words = Vec::with_capacity(count);
        let mut unconstrained = 0;
        while words.len() < count {
            let at = base + 4 * words.len() as u64;
            let format = choose(&mut self.rng);
            let records = records_of(format);
            let record = records[self.rng.below(records.len() as u64) as usize];
            let (word, kind) = self.instruction(record, at, (base, size));
            // A register branch goes where its register points. Most of the
            // time an ADR before it points the register into the image.
            if format == FormatClass::BranchReg
                && kind == Kind::Valid
                && words.len() + 2 <= count
                && self.rng.chance(19, 20)
                && let Some(register) = field(record, "rn", word).filter(|n| *n != 31)
            {
                let target = base + 4 * self.rng.below(count as u64);
                if let Some(adr) = self.adr(register, at, target) {
                    words.push(adr);
                    // The branch moves one instruction down. It has no
                    // PC-relative field, so the word stays as it is.
                    words.push(word);
                    continue;
                }
            }
            unconstrained += u64::from(kind == Kind::Unconstrained);
            words.push(word);
        }
        (words, unconstrained)
    }

    /// The registers at the start of a round: a mix of small values, of
    /// addresses inside the image, of boundary values and of random ones.
    pub fn registers(&mut self, image: (u64, u64)) -> ([u64; 31], u64, u8) {
        let value = |rng: &mut Rng| match rng.below(8) {
            0 | 1 => rng.below(17),
            2 | 3 => image.0 + 4 * rng.below(image.1 / 4),
            4 => {
                [0, u64::MAX, 1 << 63, (1 << 63) - 1, 1 << 31, (1 << 32) - 1][rng.below(6) as usize]
            }
            _ => rng.next_u64(),
        };
        let mut x = [0; 31];
        for register in &mut x {
            *register = value(&mut self.rng);
        }
        let sp = value(&mut self.rng);
        let nzcv = self.rng.below(16) as u8;
        (x, sp, nzcv)
    }
}

impl Generator {
    /// 64 bits that an FP instruction reads as a value where its rules
    /// change: zeros, denormals, infinities, both kinds of NaN, the powers of
    /// two that an integer conversion saturates at, ties, and random bits.
    pub fn fp_value(&mut self) -> u64 {
        // One layout: the count of exponent bits and of fraction bits.
        fn value(rng: &mut Rng, exponent_bits: u32, fraction_bits: u32) -> u64 {
            let exponent_max = (1u64 << exponent_bits) - 1;
            let bias = exponent_max >> 1;
            let fraction_mask = (1u64 << fraction_bits) - 1;
            let exponent = match rng.below(10) {
                0 | 1 => 0,
                2 | 3 => exponent_max,
                4 => 1 + rng.below(2),
                5 => exponent_max - 1,
                // Near one, and around 2^31, 2^32, 2^63 and 2^64.
                6 => bias - 2 + rng.below(5),
                7 => bias + [7, 8, 15, 16, 30, 31, 32, 33, 62, 63, 64, 65][rng.below(12) as usize],
                8 => bias.wrapping_sub(70).wrapping_add(rng.below(140)),
                _ => rng.below(exponent_max + 1),
            } & exponent_max;
            let fraction = match rng.below(8) {
                0 | 1 => 0,
                2 => 1,
                3 => fraction_mask,
                // The top bit: a quiet NaN, or one half above a power of two.
                4 => 1 << (fraction_bits - 1),
                5 => (1 << (fraction_bits - 1)) | rng.below(4),
                // A few bits at the top: ties of a conversion to an integer.
                6 => rng.below(1 << 6) << (fraction_bits - 6),
                _ => rng.next_u64() & fraction_mask,
            };
            let sign = rng.below(2) << (exponent_bits + fraction_bits);
            sign | exponent << fraction_bits | fraction
        }
        let rng = &mut self.rng;
        match rng.below(8) {
            0 | 1 => rng.next_u64(),
            2..=4 => value(rng, 11, 52),
            // Single precision in the low half, with zeros or anything above.
            5 | 6 => {
                let high = if rng.chance(1, 2) {
                    0
                } else {
                    rng.next_u64() << 32
                };
                high | value(rng, 8, 23)
            }
            // Half precision in the low 16 bits.
            _ => (rng.next_u64() << 16) | value(rng, 5, 10),
        }
    }

    /// V0 to V31, FPSR and FPCR at the start of a round. The upper half of a
    /// vector register is zero in half of the registers: a scalar write must
    /// clear it. FPCR takes each rounding mode, and flush-to-zero, default
    /// NaN and alternative half precision each one time in four. FPSR starts
    /// clear in half of the rounds, so that the bits of the first
    /// instructions show.
    pub fn vectors(&mut self) -> super::Vectors {
        let mut vectors = super::Vectors::default();
        for register in &mut vectors.v {
            let low = self.fp_value();
            let high = if self.rng.chance(1, 2) {
                0
            } else {
                self.fp_value()
            };
            *register = u128::from(high) << 64 | u128::from(low);
        }
        // RMode in the bits 23 and 22, FZ in 24, DN in 25 and AHP in 26.
        let mut fpcr = (self.rng.below(4) as u32) << 22;
        for bit in [24, 25, 26] {
            if self.rng.chance(1, 4) {
                fpcr |= 1 << bit;
            }
        }
        vectors.fpcr = fpcr;
        // IOC, DZC, OFC, UFC, IXC, IDC and QC.
        if self.rng.chance(1, 2) {
            vectors.fpsr = self.rng.next_u64() as u32 & 0x0800_009f;
        }
        vectors
    }
}

/// The distinct encodings of a trace of the measure plugin
/// (`measure/a64trace.c` of the design card): its lines `enc <hex>`.
pub fn executed_encodings(text: &str) -> Vec<u32> {
    let mut encodings: Vec<u32> = text
        .lines()
        .filter_map(|line| line.strip_prefix("enc "))
        .filter_map(|hex| u32::from_str_radix(hex.trim(), 16).ok())
        .collect();
    encodings.sort_unstable();
    encodings.dedup();
    encodings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lift::LIFTED_FORMATS;

    #[test]
    fn the_generator_is_a_function_of_its_seed() {
        let make = |seed| {
            let mut generator = Generator::new(seed);
            let image = generator.image(0x4200_0000, 256, |rng| {
                LIFTED_FORMATS[rng.below(LIFTED_FORMATS.len() as u64) as usize]
            });
            (image, generator.registers((0x4200_0000, 1024)))
        };
        assert_eq!(make(7), make(7));
        assert_ne!(make(7).0, make(8).0);
    }

    #[test]
    fn a_valid_word_decodes_to_its_record_and_branches_mostly_stay_inside() {
        let mut generator = Generator::new(1);
        generator.unconstrained_one_in = 0;
        let image = (0x4200_0000u64, 4096u64);
        let (mut branches, mut inside) = (0, 0);
        for format in LIFTED_FORMATS {
            for record in records_of(format) {
                for index in 0..200u64 {
                    let at = image.0 + 4 * (index % 1024);
                    let (word, kind) = generator.instruction(record, at, image);
                    assert_eq!(kind, Kind::Valid, "{}", record.name());
                    assert_eq!(decode(word).map(|decoded| decoded.record), Some(record));
                    if let Some((name, width)) = branch_field(record) {
                        let offset = sign_extend(field(record, name, word).unwrap().into(), width);
                        let target = at.wrapping_add((offset << 2) as u64);
                        branches += 1;
                        inside += u64::from((image.0..image.0 + image.1).contains(&target));
                    }
                }
            }
        }
        assert!(branches > 0);
        // Nine in ten are aimed at the image. A few more land there by chance.
        assert!(inside * 100 >= branches * 85, "{inside} of {branches}");
        assert!(inside < branches);
    }

    #[test]
    fn every_record_of_the_lifted_classes_is_generated() {
        let mut generator = Generator::new(3);
        let mut seen = std::collections::BTreeSet::new();
        let (words, _) = generator.image(0x4200_0000, 20_000, |rng| {
            LIFTED_FORMATS[rng.below(LIFTED_FORMATS.len() as u64) as usize]
        });
        for word in words {
            if let Some(decoded) = decode(word) {
                seen.insert(decoded.record);
            }
        }
        for format in LIFTED_FORMATS {
            for record in records_of(format) {
                assert!(seen.contains(&record), "{} is missing", record.name());
            }
        }
    }

    #[test]
    fn the_fp_registers_of_a_round_cover_the_special_values_and_every_mode() {
        let mut generator = Generator::new(9);
        let (mut zeros, mut infinities, mut nans, mut denormals) = (0, 0, 0, 0);
        let mut modes = [0u32; 4];
        let (mut flush, mut clear_status, mut clear_high) = (0, 0, 0);
        for _ in 0..200 {
            let vectors = generator.vectors();
            assert_eq!(vectors.fpcr & !0x07c0_0000, 0);
            assert_eq!(vectors.fpsr & !0x0800_009f, 0);
            modes[(vectors.fpcr >> 22) as usize & 3] += 1;
            flush += u32::from((vectors.fpcr & (1 << 24)) != 0);
            clear_status += u32::from(vectors.fpsr == 0);
            for value in vectors.v {
                clear_high += u32::from(value >> 64 == 0);
                let double = value as u64;
                let exponent = (double >> 52) & 0x7ff;
                let fraction = double & ((1 << 52) - 1);
                zeros += u32::from(double << 1 == 0);
                infinities += u32::from(exponent == 0x7ff && fraction == 0);
                nans += u32::from(exponent == 0x7ff && fraction != 0);
                denormals += u32::from(exponent == 0 && fraction != 0);
            }
        }
        for count in [zeros, infinities, nans, denormals, flush] {
            assert!(
                count > 20,
                "{zeros} {infinities} {nans} {denormals} {flush}"
            );
        }
        assert!(modes.iter().all(|count| *count > 20), "{modes:?}");
        assert!((50..150).contains(&clear_status));
        assert!((2000..4400).contains(&clear_high));
        // The same seed gives the same registers.
        assert_eq!(Generator::new(4).vectors(), Generator::new(4).vectors());
    }

    #[test]
    fn every_record_of_the_fp_classes_is_generated_and_has_a_lifter() {
        use crate::lift::{Builder, FP_FORMATS, Lifted, lift};
        let mut generator = Generator::new(5);
        generator.unconstrained_one_in = 0;
        let context = crate::State::reset(0).context;
        let mut records = 0;
        for format in FP_FORMATS {
            for record in records_of(format) {
                records += 1;
                for _ in 0..50 {
                    let (word, kind) =
                        generator.instruction(record, 0x4200_0000, (0x4200_0000, 64));
                    assert_eq!(kind, Kind::Valid, "{}", record.name());
                    let decoded = decode(word).unwrap();
                    assert_eq!(decoded.record, record);
                    let mut builder = Builder::new(context, 0x4200_0000);
                    assert_eq!(
                        lift(&mut builder, &decoded),
                        Lifted::Next,
                        "{} {word:08x}",
                        record.name()
                    );
                }
            }
        }
        assert_eq!(records, 140);
    }

    #[test]
    fn adr_points_a_register_at_a_target() {
        let mut generator = Generator::new(0);
        // adr x1, .+8 is 0x10000041.
        assert_eq!(
            generator.adr(1, 0x4200_0000, 0x4200_0008),
            Some(0x1000_0041)
        );
        assert_eq!(generator.adr(1, 0x4200_0000, 0x4300_0000), None);
        let back = generator.adr(30, 0x4200_0100, 0x4200_0000).unwrap();
        let label = field(Record::ADR, "label", back).unwrap();
        assert_eq!(sign_extend(label.into(), 21), -0x100);
    }

    #[test]
    fn the_encodings_of_a_trace_are_read_once_each() {
        let text = "count add 5\nenc 91000400\nenc d61f0020\nenc 91000400\nsys x\n";
        assert_eq!(executed_encodings(text), vec![0x9100_0400, 0xd61f_0020]);
    }
}
