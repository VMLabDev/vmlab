//! Bounds on what a guest can make the interpreter spend or keep.

use vmlab_hv_tcg::{Error, FlatMemory, Interpreter, Memory, Riscv64, TERMINAL_LIMIT, Value};
use vmlab_hv_tcg_ir::Program;

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
        assert!(steps < 1_000_000, "the spinning hart stopped costing operations");
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
    assert_eq!(interpreter.terminal.iter().copied().collect::<Vec<_>>(), b"defg");
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
