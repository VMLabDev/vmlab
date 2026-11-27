//! K2 of 3e DESIGN.md in the merge bar: the block differential of the JIT
//! on a bounded number of rounds with a fixed seed. `just tcg-a64-jit-diff`
//! runs it longer.

use vmlab_hv_tcg_aarch64::oracle::jitdiff;

#[test]
fn compiled_blocks_leave_what_the_interpreter_leaves() {
    let seed = 0x3e_0001;
    match jitdiff::run(seed, 20_000) {
        Ok(report) => {
            eprintln!("K2: seed {seed:#x}: {report:?}");
            assert!(report.compiled_blocks > report.refused_blocks);
        }
        Err(difference) => panic!("{difference}"),
    }
}
