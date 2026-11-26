//! The stream generator of the lockstep (DESIGN.md O3, step 1): random
//! instructions of chosen format classes from our own decode tables, then
//! the constraints, then boundary values.
//!
//! An image is a run of such instructions. The generator shapes the
//! PC-relative fields so that most branches land inside the image, because a
//! branch that leaves it ends the round.

use crate::fields::sign_extend;
use crate::generated::decode::{Decoded, Fields, FormatClass, RECORDS, Record, decode};
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

/// Whether the architecture leaves the result of this instruction open
/// (DESIGN.md U2): a write back to a base that is also a data register, a
/// load of one register twice, an exclusive store whose status register is
/// one of its operands. The streams leave these out, because the two sides
/// may each do something else and both be right.
///
/// The cases are those that LLVM's decoder marks as soft failures in its
/// decoder methods for the pairs, the writing-back loads and stores and the
/// exclusives (S1).
pub fn unpredictable(decoded: &Decoded) -> bool {
    let value = decoded.record.info().value;
    // Bit 26: the data registers are vector registers, which no base is.
    let integer = value & (1 << 26) == 0;
    let load = value & (1 << 22) != 0;
    match &decoded.fields {
        Fields::LoadPreIdx(f) => integer && f.rt == f.rn && f.rn != 31,
        Fields::LoadPostIdx(f) => integer && f.rt == f.rn && f.rn != 31,
        Fields::StorePreIdx(f) => integer && f.rt == f.rn && f.rn != 31,
        Fields::StorePostIdx(f) => integer && f.rt == f.rn && f.rn != 31,
        Fields::LoadPairPreIdx(f) => {
            f.rt == f.rt2 || (integer && (f.rt == f.rn || f.rt2 == f.rn) && f.rn != 31)
        }
        Fields::LoadPairPostIdx(f) => {
            f.rt == f.rt2 || (integer && (f.rt == f.rn || f.rt2 == f.rn) && f.rn != 31)
        }
        Fields::StorePairPreIdx(f) => integer && (f.rt == f.rn || f.rt2 == f.rn) && f.rn != 31,
        Fields::StorePairPostIdx(f) => integer && (f.rt == f.rn || f.rt2 == f.rn) && f.rn != 31,
        Fields::BaseLoadStorePairOffset(f) => load && f.rt == f.rt2,
        Fields::BaseLoadStorePairNoAlloc(f) => load && f.rt == f.rt2,
        Fields::LoadExclusivePair(f) => f.rt == f.rt2,
        Fields::StoreExclusive(f) => f.ws == f.rt || (f.ws == f.rn && f.rn != 31),
        Fields::StoreExclusivePair(f) => {
            f.ws == f.rt || f.ws == f.rt2 || (f.ws == f.rn && f.rn != 31)
        }
        _ => false,
    }
}

/// The exclusive store that goes with an exclusive load: the same size and
/// the same ordering.
fn exclusive_store(load: Record) -> Option<Record> {
    let name = match load.name() {
        "LDXRB" => "STXRB",
        "LDXRH" => "STXRH",
        "LDXRW" => "STXRW",
        "LDXRX" => "STXRX",
        "LDAXRB" => "STLXRB",
        "LDAXRH" => "STLXRH",
        "LDAXRW" => "STLXRW",
        "LDAXRX" => "STLXRX",
        "LDXPW" => "STXPW",
        "LDXPX" => "STXPX",
        "LDAXPW" => "STLXPW",
        "LDAXPX" => "STLXPX",
        _ => return None,
    };
    RECORDS
        .iter()
        .position(|info| info.name == name)
        .and_then(Record::from_index)
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
            for _ in 0..1000 {
                let word = self.raw(record);
                if !decode(word).is_some_and(|decoded| unpredictable(&decoded)) {
                    return (word, Kind::Unconstrained);
                }
            }
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
            // An offset in bytes keeps the alignment of its base three times
            // out of four: an access that is not aligned ends the round.
            if matches!(
                record.info().format,
                FormatClass::BaseLoadStoreUnscale
                    | FormatClass::BaseLoadStoreUnprivileged
                    | FormatClass::LoadPreIdx
                    | FormatClass::StorePreIdx
                    | FormatClass::LoadPostIdx
                    | FormatClass::StorePostIdx
            ) && self.rng.chance(3, 4)
                && let Some(offset) = field(record, "offset", word)
            {
                word = with_field(record, "offset", word, offset & !0xf).unwrap_or(word);
            }
            if decode(word)
                .is_some_and(|decoded| decoded.record == record && !unpredictable(&decoded))
            {
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
            // An exclusive load is followed, most of the time, by its
            // exclusive store at the same base, at once or one instruction
            // later: an exclusive store alone never writes.
            if kind == Kind::Valid
                && matches!(
                    format,
                    FormatClass::LoadExclusive | FormatClass::LoadExclusivePair
                )
                && self.rng.chance(3, 4)
                && let Some(store) = exclusive_store(record)
                && let Some(rn) = field(record, "rn", word)
            {
                if words.len() + 2 <= count && self.rng.chance(1, 2) {
                    let at = base + 4 * words.len() as u64;
                    let between = choose(&mut self.rng);
                    let records = records_of(between);
                    let record = records[self.rng.below(records.len() as u64) as usize];
                    let (word, kind) = self.instruction(record, at, (base, size));
                    unconstrained += u64::from(kind == Kind::Unconstrained);
                    words.push(word);
                }
                if words.len() < count {
                    for _ in 0..1000 {
                        let at = base + 4 * words.len() as u64;
                        let (word, kind) = self.instruction(store, at, (base, size));
                        let Some(word) = with_field(store, "rn", word, rn) else {
                            break;
                        };
                        if kind == Kind::Valid
                            && decode(word).is_some_and(|decoded| {
                                decoded.record == store && !unpredictable(&decoded)
                            })
                        {
                            words.push(word);
                            break;
                        }
                    }
                }
            }
        }
        (words, unconstrained)
    }

    /// The registers at the start of a round of loads and stores. Most of
    /// them point into `window` (first address, length in bytes), which must
    /// be RAM with room around it for the offsets of the instructions.
    /// Others are small, as an offset register is, and a few are anything.
    pub fn memory_registers(&mut self, window: (u64, u64)) -> ([u64; 31], u64, u8) {
        let pointer = |rng: &mut Rng| {
            let address = window.0 + rng.below(window.1);
            match rng.below(4) {
                0 => address,
                1 => address & !7,
                _ => address & !15,
            }
        };
        let value = |rng: &mut Rng| match rng.below(16) {
            0..=9 => pointer(rng),
            10..=12 => rng.below(64) * [1, 1, 4, 8, 16][rng.below(5) as usize],
            13 => (rng.below(256) as i64 - 128) as u64,
            14 => {
                [0, u64::MAX, 1 << 63, (1 << 63) - 1, 1 << 31, (1 << 32) - 1][rng.below(6) as usize]
            }
            _ => rng.next_u64(),
        };
        let mut x = [0; 31];
        for register in &mut x {
            *register = value(&mut self.rng);
        }
        let sp = pointer(&mut self.rng);
        let nzcv = self.rng.below(16) as u8;
        (x, sp, nzcv)
    }

    /// Bytes for the memory that the loads and stores of a stream work on.
    /// Half of its 8-byte words point into `window`, so that a register
    /// that was loaded from it is a base again.
    pub fn memory_bytes(&mut self, window: (u64, u64), len: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(len + 8);
        while bytes.len() < len {
            let word = match self.rng.below(8) {
                0..=3 => (window.0 + self.rng.below(window.1)) & !15,
                4 => self.rng.below(256),
                _ => self.rng.next_u64(),
            };
            bytes.extend(word.to_le_bytes());
        }
        bytes.truncate(len);
        bytes
    }

    /// The registers for one instruction that a guest executed, so that it
    /// runs here too: its base register points into `window`, and its
    /// offset register is small.
    pub fn registers_for(&mut self, word: u32, window: (u64, u64)) -> ([u64; 31], u64, u8) {
        let (mut x, _, nzcv) = self.memory_registers(window);
        let middle = (window.0 + window.1 / 2) & !15;
        let mut sp = middle;
        if let Some(decoded) = decode(word) {
            let record = decoded.record;
            let offset = self.rng.below(32) * 16;
            for name in ["rm", "xm"] {
                if let Some(n) = field(record, name, word).filter(|n| *n < 31) {
                    x[n as usize] = offset;
                }
            }
            // The base comes last: it wins where one register is both.
            match field(record, "rn", word) {
                Some(31) | None => {}
                Some(n) => x[n as usize] = middle,
            }
            sp = middle;
        }
        (x, sp, nzcv)
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
    fn every_record_of_the_loads_and_stores_is_generated_and_none_is_unpredictable() {
        use crate::lift::LOAD_STORE_FORMATS;
        let mut generator = Generator::new(5);
        generator.unconstrained_one_in = 0;
        let mut seen = std::collections::BTreeSet::new();
        let (words, loose) = generator.image(0x4200_0000, 60_000, |rng| {
            LOAD_STORE_FORMATS[rng.below(LOAD_STORE_FORMATS.len() as u64) as usize]
        });
        assert_eq!(loose, 0);
        let mut pairs = 0;
        for (index, word) in words.iter().enumerate() {
            let decoded = decode(*word).expect("a valid word decodes");
            assert!(!unpredictable(&decoded), "{word:08x}");
            seen.insert(decoded.record);
            // An exclusive load with its store right behind it at the same
            // base. Chance alone gives one base in 32.
            if let Some(store) = exclusive_store(decoded.record)
                && let Some(next) = words.get(index + 1).and_then(|word| decode(*word))
                && next.record == store
                && field(store, "rn", words[index + 1]) == field(decoded.record, "rn", *word)
            {
                pairs += 1;
            }
        }
        assert!(pairs > 100, "{pairs} exclusive pairs");
        let mut records = 0;
        for format in LOAD_STORE_FORMATS {
            for record in records_of(format) {
                assert!(seen.contains(&record), "{} is missing", record.name());
                records += 1;
            }
        }
        assert_eq!(records, 578);
    }

    #[test]
    fn the_unpredictable_cases_are_named() {
        let word = |record: Record, fields: &[(&str, u32)]| {
            let mut word = record.info().value;
            for (name, value) in fields {
                word = with_field(record, name, word, *value).unwrap();
            }
            decode(word).unwrap()
        };
        // ldr x1, [x1], #8 writes X1 twice. With the stack pointer as the
        // base, register 31 as data is the zero register.
        assert!(unpredictable(&word(
            Record::LDRXpost,
            &[("rt", 1), ("rn", 1)]
        )));
        assert!(!unpredictable(&word(
            Record::LDRXpost,
            &[("rt", 31), ("rn", 31)]
        )));
        // A vector register is no base.
        assert!(!unpredictable(&word(
            Record::LDRQpre,
            &[("rt", 1), ("rn", 1)]
        )));
        assert!(unpredictable(&word(
            Record::LDPXi,
            &[("rt", 2), ("rt2", 2), ("rn", 0)]
        )));
        assert!(!unpredictable(&word(
            Record::STPXi,
            &[("rt", 2), ("rt2", 2), ("rn", 0)]
        )));
        assert!(unpredictable(&word(
            Record::STPXpre,
            &[("rt", 2), ("rt2", 0), ("rn", 0)]
        )));
        assert!(unpredictable(&word(
            Record::STXRX,
            &[("ws", 3), ("rt", 3), ("rn", 0)]
        )));
        assert!(unpredictable(&word(
            Record::STXPW,
            &[("ws", 0), ("rt", 3), ("rt2", 4), ("rn", 0)]
        )));
        assert!(!unpredictable(&word(
            Record::STXPW,
            &[("ws", 5), ("rt", 3), ("rt2", 4), ("rn", 0)]
        )));
        assert!(!unpredictable(&word(
            Record::ADDXri,
            &[("rd", 1), ("rn", 1)]
        )));
    }

    #[test]
    fn the_registers_of_a_memory_round_mostly_point_into_the_window() {
        let mut generator = Generator::new(11);
        let window = (0x4210_0000u64, 0x1_0000u64);
        let (mut inside, mut all) = (0, 0);
        for _ in 0..200 {
            let (x, sp, _) = generator.memory_registers(window);
            assert!((window.0..window.0 + window.1).contains(&sp));
            for value in x {
                all += 1;
                inside += u64::from((window.0..window.0 + window.1).contains(&value));
            }
        }
        assert!(inside * 2 > all && inside < all, "{inside} of {all}");
        let bytes = generator.memory_bytes(window, 4099);
        assert_eq!(bytes.len(), 4099);
        // ldr x1, [x2, x3]: the base in the window and a small offset.
        let (x, sp, _) = generator.registers_for(0xf863_6841, window);
        assert_eq!(x[2], 0x4210_8000);
        assert!(x[3] < 512 && x[3] % 16 == 0);
        assert_eq!(sp, 0x4210_8000);
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
