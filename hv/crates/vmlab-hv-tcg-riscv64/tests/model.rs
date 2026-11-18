//! The committed model payload is the one the generated source names.

use sha2::{Digest, Sha256};
use std::io::Read;
use vmlab_hv_tcg_riscv64::{ENCODED_IR_BYTES, INSTRUCTION_TAGS, IR_SHA256, model};

#[test]
fn the_embedded_ir_has_the_digest_and_size_the_generated_source_records() {
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(include_bytes!("../src/model.json.gz").as_slice())
        .read_to_end(&mut json)
        .expect("model gzip");
    assert_eq!(json.len(), ENCODED_IR_BYTES);
    assert_eq!(format!("{:x}", Sha256::digest(&json)), IR_SHA256);
}

#[test]
fn the_instruction_tags_remain_without_the_per_tag_builders() {
    // The interpreter specialises `zexecute` per constructor itself, so the
    // generated crate carries the tag list and no `build_z*` function.
    assert!(model().functions.contains_key("zexecute"));
    assert!(!INSTRUCTION_TAGS.is_empty());
    assert!(INSTRUCTION_TAGS.iter().all(|tag| tag.starts_with('z')));
    assert!(!include_str!("../src/lib.rs").contains("fn build_z"));
}
