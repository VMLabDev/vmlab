//! Replay the committed sample of Berkeley TestFloat cases: the results of
//! upstream SoftFloat with the ARM-VFPv2 specialisation, against the FP
//! helpers. No generator runs here. `just tcg-a64-testfloat` runs them all.

use vmlab_hv_tcg_aarch64::oracle::testfloat::{Verdict, cases, check, parse_sample};

#[test]
fn the_committed_testfloat_sample_gives_the_same_on_the_helpers() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testfloat.txt");
    let text = std::fs::read_to_string(&path).expect("the sample file");
    let mut same = 0;
    let mut seen = Vec::new();
    let mut failures = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let (case, vector) =
            parse_sample(line).unwrap_or_else(|error| panic!("line {}: {error}", number + 1));
        match check(&case, vector) {
            Ok(Verdict::Same) => same += 1,
            Ok(Verdict::Skipped) => panic!("line {}: a case that is not compared", number + 1),
            Err(error) => failures.push(format!("line {}: {error}", number + 1)),
        }
        if !seen.contains(&case) {
            seen.push(case);
        }
    }
    assert!(
        failures.is_empty(),
        "{} cases differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
    // Every run of the generator has cases in the sample.
    for case in cases() {
        assert!(seen.contains(&case), "the sample has no case of {case:?}");
    }
    assert!(same >= 16 * cases().len(), "{same} cases");
    println!("checked {same} TestFloat cases");
}
