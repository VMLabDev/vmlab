//! Bounds on what a guest can make the interpreter spend or keep.

use vmlab_hv_tcg::{
    Error, FlatMemory, Interpreter, Memory, Privilege, Riscv64, TERMINAL_LIMIT, Value,
};
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

const MTVEC: u16 = 0x305;
const MEPC: u16 = 0x341;
const MCAUSE: u16 = 0x342;
const RAM: u64 = 0x8000_0000;
const HANDLER: u64 = 0x8000_0800;
const LD_X10_X8: u32 = 0x0004_3503;
const SD_X10_X8: u32 = 0x00a4_3023;
const JALR_X8: u32 = 0x0004_0067;
const LR_D_X10_X8: u32 = 0x1004_352f;
const AMOADD_D_X10_X8: u32 = 0x00a4_352f;

/// A hart on 4 KiB of RAM at 0x8000_0000 that holds `code`, with `mtvec` at
/// 0x8000_0800 and `x8` set.
fn on_small_ram(code: &[u32], x8: u64, specialize: bool) -> Riscv64<FlatMemory> {
    let mut cpu = Riscv64::new(FlatMemory::new(RAM, vec![0; 4096])).unwrap();
    cpu.write_csr(MTVEC, HANDLER).unwrap();
    cpu.specialize = specialize;
    restart(&mut cpu, code, x8);
    cpu
}

/// Put the hart back at the start of RAM, in machine mode, with new code.
fn restart(cpu: &mut Riscv64<FlatMemory>, code: &[u32], x8: u64) {
    for (index, instruction) in code.iter().enumerate() {
        cpu.interpreter
            .memory
            .write(RAM + index as u64 * 4, &instruction.to_le_bytes())
            .unwrap();
    }
    cpu.set_privilege(Privilege::Machine);
    cpu.set_pc(RAM);
    cpu.set_gpr(8, x8).unwrap();
}

/// The model's region table says the whole of 0x8000_0000..0x1_0000_0000 is
/// RAM, whatever the host gave the hart. An access the host does not hold is
/// the guest's access fault, not a host error that stops the hart for good.
#[test]
fn an_access_past_host_ram_raises_a_guest_fault() {
    // Past the end, far past the end, across the end, below the RAM in the
    // model's ROM region, and in the model's IO region.
    let plain = [
        0x8000_1000,
        0x8010_0000,
        0x8000_0ffc,
        0x1000,
        0x1000_0000u64,
    ];
    // The exclusive and the atomic accesses take the same way out.
    let atomic = [0x8000_1000, 0x8010_0000u64];
    let cases = [
        (LD_X10_X8, 5, &plain[..]),
        (SD_X10_X8, 7, &plain[..]),
        (LR_D_X10_X8, 5, &atomic[..]),
        (AMOADD_D_X10_X8, 7, &atomic[..]),
    ];
    for specialize in [false, true] {
        // One hart takes every fault in turn: none of them may stop it.
        let mut cpu = on_small_ram(&[], 0, specialize);
        let mut steps = 0;
        for (instruction, cause, addresses) in cases {
            for &address in addresses {
                let case = format!("{instruction:#010x} at {address:#x}, specialize {specialize}");
                restart(&mut cpu, &[instruction], address);
                let waiting = cpu
                    .step()
                    .unwrap_or_else(|error| panic!("{case}: step failed: {error}"));
                steps += 1;
                assert!(!waiting, "{case}");
                assert_eq!(cpu.pc().unwrap(), HANDLER, "{case}");
                assert_eq!(cpu.read_csr(MCAUSE).unwrap(), cause, "{case}");
                assert_eq!(cpu.read_csr(MEPC).unwrap(), RAM, "{case}");
                assert_eq!(cpu.steps, steps, "{case}");
            }
        }

        // A jump out of RAM lands, and the fetch there faults.
        restart(&mut cpu, &[JALR_X8], 0x8000_1000);
        cpu.step().unwrap();
        assert_eq!(cpu.pc().unwrap(), 0x8000_1000);
        cpu.step()
            .unwrap_or_else(|error| panic!("fetch, specialize {specialize}: {error}"));
        assert_eq!(cpu.pc().unwrap(), HANDLER);
        assert_eq!(cpu.read_csr(MCAUSE).unwrap(), 1);
        assert_eq!(cpu.read_csr(MEPC).unwrap(), 0x8000_1000);
        assert_eq!(cpu.steps, steps + 2);
    }
}

/// A trace trained on accesses inside RAM meets one outside it. The hart must
/// end in the same state as under the plain interpreter, at every stop.
#[test]
fn a_trained_trace_that_leaves_host_ram_raises_the_same_guest_fault() {
    // loop: <access> 0(x8); addi x8, x8, 8; jal x0, loop
    // From 0x8000_0fe0 the access walks to the end of RAM. From 0x8000_0fe4
    // the last one starts inside RAM and crosses its end.
    for access in [LD_X10_X8, SD_X10_X8] {
        for start in [0x8000_0fe0, 0x8000_0fe4u64] {
            for chunk in [1, 7] {
                let code = [access, 0x0084_0413, 0xff9f_f06f];
                let mut reference = on_small_ram(&code, start, false);
                let mut specialized = on_small_ram(&code, start, true);
                let mut faulted = false;
                let mut done = 0;
                while done < 20 {
                    let case = format!(
                        "{access:#010x} from {start:#x} in chunks of {chunk}, after {done} steps"
                    );
                    let (ran, waiting) = specialized
                        .run(chunk)
                        .unwrap_or_else(|error| panic!("{case}: {error}"));
                    assert!(ran > 0 && !waiting, "{case}");
                    assert_eq!(reference.run(ran).unwrap(), (ran, false), "{case}");
                    done += ran;
                    assert_eq!(
                        *specialized.interpreter.globals, *reference.interpreter.globals,
                        "{case}"
                    );
                    assert_eq!(
                        specialized.interpreter.memory.bytes(),
                        reference.interpreter.memory.bytes(),
                        "{case}"
                    );
                    assert_eq!(specialized.steps, reference.steps, "{case}");
                    faulted |= reference.pc().unwrap() == HANDLER;
                }
                assert!(faulted, "{access:#010x} from {start:#x} never left RAM");
                assert!(specialized.specialization_stats.hits > 0);
            }
        }
    }
}
