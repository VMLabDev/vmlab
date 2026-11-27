//! K2 and K3 of 3e DESIGN.md in the merge bar: the block differential of
//! the JIT on a bounded number of rounds with a fixed seed, the same
//! streams with K3 checking every compiled block, and a defect seeded into
//! the emitter that each of the two must catch (J2). `just
//! tcg-a64-jit-diff` runs K2 longer.

use vmlab_hv_tcg_aarch64::exec::JitDefect;
use vmlab_hv_tcg_aarch64::oracle::jitdiff::{self, Options};

const SEED: u64 = 0x3e_0001;

#[test]
fn compiled_blocks_leave_what_the_interpreter_leaves() {
    match jitdiff::run(SEED, 3_000) {
        Ok(report) => {
            eprintln!("K2: seed {SEED:#x}: {report:?}");
            assert!(report.compiled_blocks > report.refused_blocks);
            // The inlined loads and stores hit the fast TLB, and miss it.
            assert!(report.tlb_accesses > report.tlb_misses && report.tlb_misses > 0);
        }
        Err(difference) => panic!("{difference}"),
    }
}

#[test]
fn the_replay_check_finds_no_difference_on_the_streams() {
    let options = Options {
        check: true,
        ..Options::default()
    };
    match jitdiff::run_with(SEED, 1_000, options) {
        Ok(report) => {
            eprintln!("K3: seed {SEED:#x}: {report:?}");
            assert!(report.checked_blocks > 0);
        }
        Err(difference) => panic!("{difference}"),
    }
}

#[test]
fn the_differential_catches_a_seeded_defect() {
    let options = Options {
        defect: JitDefect::AddSubtracts,
        ..Options::default()
    };
    let difference =
        jitdiff::run_with(SEED, 1_000, options).expect_err("K2 does not see an ADD that subtracts");
    eprintln!("K2 with the defect: {difference}");
}

#[test]
fn the_replay_check_catches_a_seeded_defect() {
    let options = Options {
        check: true,
        defect: JitDefect::AddSubtracts,
    };
    let difference =
        jitdiff::run_with(SEED, 1_000, options).expect_err("K3 does not see an ADD that subtracts");
    // The compiled side stopped with the report of K3; what it kept is what
    // the interpreter did.
    assert!(
        difference.what.contains("K3: the compiled block"),
        "{difference}"
    );
    eprintln!("K3 with the defect: {difference}");
}
