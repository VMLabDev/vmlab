//! The diagnostic-code registry versus the source that emits codes.
//!
//! `wscript_core::diag::CODES` is the canonical list, and the fixture corpus
//! is gated against it (`diag_snapshots.rs`). That gate is only worth having
//! if the registry is complete, so this one reads the workspace's sources back
//! and insists the two agree: a code emitted but not registered escapes every
//! other check, and a code registered but never emitted sends the next reader
//! looking for something that is not there.

// Only the directory walker is wanted here; the rest of `common` is snapshot
// plumbing this test has no use for.
#[allow(dead_code)]
mod common;

use std::path::{Path, PathBuf};

use wscript_core::diag::CODES;

/// Crate sources, excluding the registry itself — it names every code by
/// definition, so reading it back would make this test vacuous.
fn source_files() -> Vec<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("wscript-compiler sits in the workspace root")
        .to_path_buf();
    let registry = workspace.join("wscript-core/src/diag.rs");

    let mut crates: Vec<PathBuf> = std::fs::read_dir(&workspace)
        .expect("reading the workspace root")
        .flatten()
        .map(|e| e.path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    crates.sort();

    let mut out = Vec::new();
    for dir in crates {
        out.extend(common::files(&dir, "rs"));
    }
    let before = out.len();
    out.retain(|p| *p != registry);
    out.sort();

    // The exclusion is the whole basis of this test: if `diag.rs` moves, the
    // retain quietly removes nothing, every registered code then reads as
    // emitted, and half of the assertions below pass vacuously.
    assert_eq!(
        before - out.len(),
        1,
        "{} is not among the sources read — has the registry moved?",
        registry.display()
    );
    assert!(
        out.len() > 10,
        "found only {} source files — did the workspace layout change?",
        out.len()
    );
    out
}

/// Every `"E1234"` / `"W1234"` string literal in `text`.
///
/// Deliberately lexical rather than syntactic: a code is only ever written as
/// a literal at its emission site, and matching the literal means a new site
/// is caught the moment it is typed, whatever function wraps it.
fn code_literals(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    for i in 0..bytes.len() {
        let Some(window) = bytes.get(i..i + 7) else {
            break;
        };
        if window[0] == b'"'
            && matches!(window[1], b'E' | b'W')
            && window[2..6].iter().all(u8::is_ascii_digit)
            && window[6] == b'"'
        {
            out.push(&text[i + 1..i + 6]);
        }
    }
    out
}

#[test]
fn the_registry_and_the_source_agree_on_the_code_list() {
    let mut emitted: Vec<(String, String)> = Vec::new();
    for file in source_files() {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for code in code_literals(&text) {
            emitted.push((code.to_string(), file.display().to_string()));
        }
    }

    let unregistered: Vec<String> = emitted
        .iter()
        .filter(|(code, _)| !CODES.iter().any(|c| c.code == code))
        .map(|(code, file)| format!("{code} ({file})"))
        .collect();
    assert!(
        unregistered.is_empty(),
        "these codes are emitted but missing from wscript-core's `CODES` — add \
         a row, with fallback help or a fixture:\n  {}",
        unregistered.join("\n  ")
    );

    let unemitted: Vec<&str> = CODES
        .iter()
        .map(|c| c.code)
        .filter(|code| !emitted.iter().any(|(c, _)| c == code))
        .collect();
    assert!(
        unemitted.is_empty(),
        "these codes are registered but no source emits them — drop the \
         row:\n  {}",
        unemitted.join(" ")
    );
}

#[test]
fn code_literals_reads_codes_and_nothing_else() {
    assert_eq!(
        code_literals(r#"self.error("E0221", span, "`break` outside of a loop")"#),
        ["E0221"]
    );
    assert_eq!(code_literals(r#""E022", "E02211", E0221"#), [] as [&str; 0]);
}
