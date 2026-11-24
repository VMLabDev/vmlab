//! Bounds on what a guest can make the interpreter spend or keep.

use std::collections::BTreeSet;
use vmlab_hv_tcg::{Error, FlatMemory, Interpreter, Memory, Riscv64, TERMINAL_LIMIT, Value};
use vmlab_hv_tcg_ir::{Op, Program, Target};

/// A hart at 0x8000_0000 in an endless `jal x0, 0`.
fn spinning() -> Riscv64<FlatMemory> {
    let mut memory = FlatMemory::new(0x8000_0000, vec![0; 4096]);
    memory
        .write(0x8000_0000, &0x0000_006fu32.to_le_bytes())
        .unwrap();
    let mut cpu = Riscv64::new(memory).unwrap();
    cpu.set_pc(0x8000_0000);
    cpu
}

fn is_limit(error: &Error) -> bool {
    match error {
        Error::Limit => true,
        Error::At { source, .. } => is_limit(source),
        _ => false,
    }
}

#[test]
fn the_operation_budget_bounds_one_step_and_not_the_guest_lifetime() {
    let mut cpu = spinning();
    let start = cpu.interpreter.operations;
    cpu.step().unwrap();
    let first = cpu.interpreter.operations - start;
    assert!(first > 0);

    // With room for a few of the most expensive steps, the guest still runs
    // for many times that budget in total: each step gets its own.
    let budget = first * 4;
    cpu.interpreter.step_operation_budget = budget;
    let mut steps = 0u32;
    while cpu.interpreter.operations - start <= budget * 4 {
        cpu.step().unwrap();
        steps += 1;
        assert!(
            steps < 1_000_000,
            "the spinning hart stopped costing operations"
        );
    }
    assert_eq!(cpu.pc().unwrap(), 0x8000_0000);

    // The specialising path arms the same budget, per recorded step.
    let mut specialized = spinning();
    specialized.specialize = true;
    specialized.interpreter.step_operation_budget = first * 16;
    let (ran, waiting) = specialized.run(2_000).unwrap();
    assert!(ran >= 2_000 && !waiting);

    // And one step that needs more than its budget fails, however little the
    // guest has run so far.
    let mut starved = spinning();
    starved.interpreter.step_operation_budget = 1;
    assert!(is_limit(&starved.step().unwrap_err()));
}

#[test]
fn the_terminal_keeps_only_the_newest_output() {
    let program = Program::default();
    let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
    assert_eq!(interpreter.terminal_limit, TERMINAL_LIMIT);
    interpreter.terminal_limit = 4;
    for byte in b"abcdefg" {
        interpreter
            .call("zplat_term_write", vec![Value::bits(8, *byte)])
            .unwrap();
    }
    assert_eq!(
        interpreter.terminal.iter().copied().collect::<Vec<_>>(),
        b"defg"
    );
    assert_eq!(interpreter.terminal_dropped, 3);

    // A guest that prints without end cannot grow the buffer past the limit.
    interpreter.terminal_limit = TERMINAL_LIMIT;
    interpreter.terminal.clear();
    for i in 0..TERMINAL_LIMIT + 1000 {
        interpreter
            .call("zplat_term_write", vec![Value::bits(8, (i % 251) as u8)])
            .unwrap();
    }
    assert_eq!(interpreter.terminal.len(), TERMINAL_LIMIT);
    assert_eq!(interpreter.terminal_dropped, 3 + 1000);
    assert_eq!(
        interpreter.terminal.back().copied(),
        Some(((TERMINAL_LIMIT + 999) % 251) as u8)
    );

    // A limit of zero keeps nothing.
    interpreter.terminal_limit = 0;
    interpreter.terminal.clear();
    interpreter
        .call("zplat_term_write", vec![Value::bits(8, 1u8)])
        .unwrap();
    assert!(interpreter.terminal.is_empty());
}

/// A machine-mode hart at 0x8000_0000 holding `csrrw x10, seed, x0`, the one
/// legal access to the Zkr entropy source, twice.
fn reading_seed() -> Riscv64<FlatMemory> {
    let mut memory = FlatMemory::new(0x8000_0000, vec![0; 4096]);
    for address in [0x8000_0000u64, 0x8000_0004] {
        memory
            .write(address, &0x0150_1573u32.to_le_bytes())
            .unwrap();
    }
    let mut cpu = Riscv64::new(memory).unwrap();
    cpu.set_pc(0x8000_0000);
    cpu
}

#[test]
fn the_seed_csr_returns_entropy() {
    let mut cpu = reading_seed();
    cpu.step().unwrap();
    assert_eq!(cpu.pc().unwrap(), 0x8000_0004);
    let first = cpu.gpr(10).unwrap();
    // OPST is ES16: the low 16 bits are entropy, and nothing else is set.
    assert_eq!(first >> 16, 0b10 << 14);
    cpu.step().unwrap();
    assert_eq!(cpu.pc().unwrap(), 0x8000_0008);
    let second = cpu.gpr(10).unwrap();
    assert_eq!(second >> 16, 0b10 << 14);
    // No answer is kept for the next read: a memoised or folded call would
    // hand the guest the same 16 bits again.
    assert_ne!(first, second);

    // The embedder owns the source: the same state gives the same reads.
    let mut again = reading_seed();
    again.interpreter.entropy = 7;
    again.step().unwrap();
    let mut reference = reading_seed();
    reference.interpreter.entropy = 7;
    reference.step().unwrap();
    assert_eq!(again.gpr(10).unwrap(), reference.gpr(10).unwrap());
    assert_ne!(again.gpr(10).unwrap(), first);

    // The trace specialiser takes the same two instructions the same way.
    let mut specialized = reading_seed();
    specialized.specialize = true;
    specialized.step().unwrap();
    assert_eq!(specialized.gpr(10).unwrap(), first);
    specialized.step().unwrap();
    assert_eq!(specialized.pc().unwrap(), 0x8000_0008);
    assert_eq!(specialized.gpr(10).unwrap(), second);
}

#[test]
fn call_serves_every_function_the_model_does_not_define() {
    let mut cpu = reading_seed();
    let program = cpu.interpreter.program;
    let missing: BTreeSet<&str> = program
        .functions
        .values()
        .chain(&program.initializers)
        .flat_map(|function| &function.code)
        .filter_map(|op| match op {
            Op::Call(_, Target::Function(name), _) if !program.functions.contains_key(name) => {
                Some(name.as_str())
            }
            _ => None,
        })
        .collect();
    // The model must still have the platform functions this test is about.
    assert!(missing.contains("zplat_term_write"));
    assert!(missing.contains("zget_16_random_bits"));
    assert!(missing.iter().any(|name| name.starts_with("zriscv_")));

    let mut unserved = Vec::new();
    for name in missing {
        let (types, _) = program
            .declarations
            .get(name)
            .unwrap_or_else(|| panic!("{name} has no declaration"));
        let args = types
            .iter()
            .map(|ty| cpu.interpreter.default_value(ty, 0).unwrap())
            .collect();
        if let Err(error) = cpu.interpreter.call(name, args) {
            unserved.push(format!("{name}: {error}"));
        }
    }
    assert!(
        unserved.is_empty(),
        "the model calls functions it does not define and `call` does not serve: {unserved:#?}"
    );
}

#[test]
fn a_csr_index_past_twelve_bits_is_refused() {
    let mut cpu = spinning();
    assert!(cpu.read_csr(0x1300).is_err());
    assert!(cpu.write_csr(0x1340, 7).is_err());
    assert!(cpu.read_csr(0xffff).is_err());
    assert_eq!(cpu.read_csr(0x340).unwrap(), 0);
    // The whole 12-bit range is still an index: mscratch itself takes a write.
    cpu.write_csr(0x340, 7).unwrap();
    assert_eq!(cpu.read_csr(0x340).unwrap(), 7);
}
