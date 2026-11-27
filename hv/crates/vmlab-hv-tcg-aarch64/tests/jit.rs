//! K2 of 3e DESIGN.md in the merge bar: the block differential of the JIT
//! on a bounded number of rounds with a fixed seed. `just tcg-a64-jit-diff`
//! runs it longer.

use vmlab_hv_tcg_aarch64::oracle::jitdiff;

#[test]
fn compiled_blocks_leave_what_the_interpreter_leaves() {
    let seed = 0x3e_0001;
    match jitdiff::run(seed, 3_000) {
        Ok(report) => {
            eprintln!("K2: seed {seed:#x}: {report:?}");
            assert!(report.compiled_blocks > report.refused_blocks);
        }
        Err(difference) => panic!("{difference}"),
    }
}

/// K2 with the operands at the edges (card J5): the flags that compiled
/// code keeps in host flags, or reads from the lazy record inline, are the
/// flags of the interpreter.
#[test]
fn compiled_flags_on_edge_operands_are_the_interpreters() {
    let seed = 0x3e_0005;
    match jitdiff::run_flags(seed, 3_000) {
        Ok(report) => {
            eprintln!("K2 flags: seed {seed:#x}: {report:?}");
            assert!(report.compiled_blocks > report.refused_blocks);
            assert!(report.fused > 0 && report.lazy > 0, "{report:?}");
        }
        Err(difference) => panic!("{difference}"),
    }
}
