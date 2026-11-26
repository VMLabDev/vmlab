//! Replay the committed regression corpus of the lockstep: instructions with
//! the registers that QEMU had before and after them. No QEMU runs here.

use vmlab_hv_tcg_aarch64::oracle::corpus;

#[test]
fn the_committed_corpus_replays_on_our_interpreter() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut files: Vec<_> = std::fs::read_dir(&directory)
        .expect("the corpus directory")
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "txt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "the corpus has no file");
    let mut replayed = 0;
    let mut failures = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("a corpus file");
        let entries =
            corpus::parse(&text).unwrap_or_else(|error| panic!("{}: {error}", file.display()));
        for entry in entries {
            replayed += 1;
            if let Err(error) = entry.replay() {
                failures.push(format!("{}: {error}", file.display()));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {replayed} entries differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(replayed > 0, "the corpus has no entry");
    println!("replayed {replayed} corpus entries");
}
