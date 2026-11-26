//! The aarch64 emulator core of vmlab-hv (phase 3d).
//!
//! The decoder comes from the LLVM AArch64 TableGen tables (`generated`).
//! The semantics are our own code: lifters that append to a block of the
//! typed IR of `vmlab-hv-tcg-ir` (`lift`), run by a small interpreter loop
//! (`exec`). `oracle` is the test harness that follows a running QEMU
//! instruction by instruction. QEMU is the test oracle only: nothing in this
//! crate starts it outside that harness.
//!
//! The rules for what an author may read to write the semantics are in
//! `hv/docs/plans/phase-3d-aarch64/DESIGN.md` (S1 to S8, X1 to X3). Each
//! semantics file names its sources in its header.

pub mod exec;
pub mod fields;
pub mod generated;
pub mod lift;
pub mod oracle;
pub mod state;

pub use exec::{Cpu, Stop};
pub use generated::decode::{Decoded, Fields, FormatClass, Record, decode};
pub use state::State;

use generated::decode::RECORDS;

/// The reference decoder: every record in the order of its priority, by
/// mask and value alone. The generated decision tree must agree with it.
pub fn decode_linear(word: u32) -> Option<Decoded> {
    let mut best: Option<(u16, Decoded)> = None;
    for (index, info) in RECORDS.iter().enumerate() {
        if word & info.mask != info.value {
            continue;
        }
        if best.is_some_and(|(priority, _)| priority < info.priority) {
            continue;
        }
        let Some(record) = Record::from_index(index) else {
            continue;
        };
        if let Some(fields) = generated::decode::extract(record, word) {
            best = Some((info.priority, Decoded { record, fields }));
        }
    }
    best.map(|(_, decoded)| decoded)
}

/// The record that matches `word` on its fixed bits and has the best
/// priority, with the constraints of `tables/constraints.toml` left out.
/// The decoder check uses it to tell a word that a constraint refused from a
/// word that no record matches.
pub fn decode_unconstrained(word: u32) -> Option<Record> {
    RECORDS
        .iter()
        .enumerate()
        .filter(|(_, info)| word & info.mask == info.value)
        .min_by_key(|(_, info)| info.priority)
        .and_then(|(index, _)| Record::from_index(index))
}

/// The value of the raw field `name` of `record` in `word`.
pub fn field(record: Record, name: &str, word: u32) -> Option<u32> {
    let info = record
        .info()
        .fields
        .iter()
        .find(|field| field.name == name)?;
    let mut value = 0;
    for part in info.parts {
        let mask = (1u64 << part.len) as u32 - 1;
        value |= ((word >> part.word_lsb) & mask) << part.field_lsb;
    }
    Some(value)
}

/// `word` with the raw field `name` of `record` set to `value`.
pub fn with_field(record: Record, name: &str, word: u32, value: u32) -> Option<u32> {
    let info = record
        .info()
        .fields
        .iter()
        .find(|field| field.name == name)?;
    let mut word = word;
    for part in info.parts {
        let mask = (1u64 << part.len) as u32 - 1;
        word &= !(mask << part.word_lsb);
        word |= ((value >> part.field_lsb) & mask) << part.word_lsb;
    }
    Some(word)
}

/// The width in bits of the raw field `name` of `record`.
pub fn field_width(record: Record, name: &str) -> Option<u32> {
    let info = record
        .info()
        .fields
        .iter()
        .find(|field| field.name == name)?;
    info.parts
        .iter()
        .map(|part| u32::from(part.field_lsb) + u32::from(part.len))
        .max()
}
