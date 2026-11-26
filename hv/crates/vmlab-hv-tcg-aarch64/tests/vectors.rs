//! The decoder against the committed vector file: encodings with the record
//! name that `llvm-mc --show-inst` of the pinned LLVM gave (DESIGN.md G1).
//! This test needs no LLVM.

use std::collections::BTreeSet;
use vmlab_hv_tcg_aarch64::generated::decode::RECORDS;
use vmlab_hv_tcg_aarch64::oracle::stream::Rng;
use vmlab_hv_tcg_aarch64::{Record, decode, decode_linear, decode_unconstrained};

const VECTORS: &str = include_str!("vectors.txt");

/// Selected records that no word decodes to: a constraint of
/// tables/constraints.toml never holds in the selected sets.
/// `MSRpstatesvcrImm1` has no feature predicate in LLVM, and its operand is
/// an SVCR field, which needs SME.
const NEVER_MATCHES: [&str; 1] = ["MSRpstatesvcrImm1"];

fn vectors() -> Vec<(u32, Option<&'static str>)> {
    VECTORS
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let (word, name) = line.split_once(' ').expect("a vector is `word name`");
            (
                u32::from_str_radix(word, 16).expect("a hex word"),
                (name != "-").then_some(name),
            )
        })
        .collect()
}

#[test]
fn the_decoder_names_the_record_that_llvm_names() {
    let mut refused = 0;
    let mut named = BTreeSet::new();
    for (word, llvm) in vectors() {
        let ours = decode(word).map(|decoded| decoded.record.name());
        if ours == llvm {
            named.extend(ours);
            continue;
        }
        // The one allowed difference (G1d): LLVM names a record, and an
        // entry of tables/constraints.toml refuses it.
        let record = decode_unconstrained(word);
        assert!(
            ours.is_none()
                && record
                    .is_some_and(|record| record.info().constrained && Some(record.name()) == llvm),
            "{word:08x}: ours {ours:?}, llvm-mc {llvm:?}"
        );
        refused += 1;
    }
    // The vectors reach every selected record that can match.
    let missing: Vec<_> = RECORDS
        .iter()
        .map(|info| info.name)
        .filter(|name| !named.contains(name) && !NEVER_MATCHES.contains(name))
        .collect();
    assert!(missing.is_empty(), "no vector decodes to {missing:?}");
    println!(
        "{} vectors, {refused} refused by a constraint",
        vectors().len()
    );
}

#[test]
fn the_decision_tree_agrees_with_the_linear_decoder() {
    let mut rng = Rng(0xa64);
    let mut words: Vec<u32> = vectors().into_iter().map(|(word, _)| word).collect();
    for _ in 0..200_000 {
        words.push(rng.next_u64() as u32);
        let info = &RECORDS[rng.below(RECORDS.len() as u64) as usize];
        words.push(info.value | (rng.next_u64() as u32 & !info.mask));
    }
    for word in words {
        assert_eq!(decode(word), decode_linear(word), "{word:08x}");
    }
}

#[test]
fn the_record_table_is_consistent() {
    assert_eq!(RECORDS.len(), Record::COUNT);
    let mut priorities = BTreeSet::new();
    for (index, info) in RECORDS.iter().enumerate() {
        let record = Record::from_index(index).expect("a record for each table row");
        assert_eq!(record as usize, index);
        assert_eq!(record.name(), info.name);
        assert_eq!(info.value & !info.mask, 0, "{}", info.name);
        assert!(priorities.insert(info.priority), "{}", info.name);
        // The free bits that a field names do not overlap the fixed bits.
        for field in info.fields {
            for part in field.parts {
                let bits = (((1u64 << part.len) - 1) as u32) << part.word_lsb;
                assert_eq!(bits & info.mask, 0, "{} {}", info.name, field.name);
            }
        }
    }
    assert!(Record::from_index(Record::COUNT).is_none());
}
