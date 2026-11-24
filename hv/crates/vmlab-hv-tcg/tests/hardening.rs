//! Bounds on what a guest can make the interpreter spend or keep.

use std::collections::BTreeSet;
use vmlab_hv_tcg::{
    Error, FlatMemory, Interpreter, Memory, Privilege, Riscv64, TERMINAL_LIMIT, Value,
};
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
fn restart<M: Memory>(cpu: &mut Riscv64<M>, code: &[u32], x8: u64) {
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
    // From 0x8000_0fc0 the access walks to the end of RAM. From 0x8000_0fc4
    // the last one starts inside RAM and crosses its end.
    let mut reference = on_small_ram(&[], 0, false);
    let mut specialized = on_small_ram(&[], 0, true);
    for access in [LD_X10_X8, SD_X10_X8] {
        for start in [0x8000_0fc0, 0x8000_0fc4u64] {
            for chunk in [1, 7, 64] {
                let code = [access, 0x0084_0413, 0xff9f_f06f];
                restart(&mut reference, &code, start);
                restart(&mut specialized, &code, start);
                let mut faulted = false;
                let mut done = 0;
                while done < 40 {
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
            }
        }
    }
    assert!(specialized.specialization_stats.hits > 0);
}

/// RAM whose upper quarter the host can take away.
struct Hole {
    ram: FlatMemory,
    closed: bool,
}

impl Hole {
    const START: u64 = RAM + 0xc00;

    fn refuses(&self, address: u64, len: usize) -> bool {
        self.closed && address.saturating_add(len as u64) > Self::START
    }
}

impl Memory for Hole {
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> vmlab_hv_tcg::Result<()> {
        if self.refuses(address, bytes.len()) {
            return Err(Error::Memory(address));
        }
        self.ram.read(address, bytes)
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> vmlab_hv_tcg::Result<()> {
        if self.refuses(address, bytes.len()) {
            return Err(Error::Memory(address));
        }
        self.ram.write(address, bytes)
    }
    fn generation(&self) -> u64 {
        self.ram.generation()
    }
    fn code_stamp(&self, address: u64, len: usize) -> Option<vmlab_hv_tcg::CodeStamp> {
        self.ram.code_stamp(address, len)
    }
    fn ram_bounds(&self, address: u64) -> Option<(u64, u64)> {
        self.ram.ram_bounds(address)
    }
    fn reservation_generation(&self, address: u64, len: usize) -> u64 {
        self.ram.reservation_generation(address, len)
    }
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> vmlab_hv_tcg::Result<u64> {
        self.ram.load_reserved(address, bytes)
    }
    fn store_conditional(
        &mut self,
        address: u64,
        bytes: &[u8],
        generation: u64,
    ) -> vmlab_hv_tcg::Result<bool> {
        self.ram.store_conditional(address, bytes, generation)
    }
    fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
        None
    }
}

/// The host refuses an address that a trace has read and written before, so
/// no guard of the trace sees it coming: the access itself fails in the trace.
#[test]
fn a_trace_whose_own_access_is_refused_raises_the_same_guest_fault() {
    let make = |specialize| {
        let mut cpu = Riscv64::new(Hole {
            ram: FlatMemory::new(RAM, vec![0; 4096]),
            closed: false,
        })
        .unwrap();
        cpu.write_csr(MTVEC, HANDLER).unwrap();
        cpu.specialize = specialize;
        cpu
    };
    let mut reference = make(false);
    let mut specialized = make(true);
    for (access, cause) in [(LD_X10_X8, 5), (SD_X10_X8, 7)] {
        // loop: <access> 0(x8); jal x0, loop
        let code = [access, 0xffdf_f06f];
        for cpu in [&mut reference, &mut specialized] {
            cpu.interpreter.memory.closed = false;
            restart(cpu, &code, Hole::START);
        }
        for round in 0..4 {
            assert_eq!(specialized.run(4).unwrap(), (4, false));
            assert_eq!(reference.run(4).unwrap(), (4, false));
            assert_eq!(
                *specialized.interpreter.globals, *reference.interpreter.globals,
                "{access:#010x} round {round}"
            );
        }
        let trained = specialized.specialization_stats.hits;
        assert!(trained > 0);

        reference.interpreter.memory.closed = true;
        specialized.interpreter.memory.closed = true;
        let steps = specialized.steps;
        specialized
            .run(1)
            .unwrap_or_else(|error| panic!("{access:#010x}: {error}"));
        reference.run(1).unwrap();
        assert_eq!(specialized.steps, steps + 1);
        assert_eq!(specialized.pc().unwrap(), HANDLER);
        assert_eq!(specialized.read_csr(MCAUSE).unwrap(), cause);
        assert_eq!(specialized.read_csr(MEPC).unwrap(), RAM);
        assert_eq!(
            *specialized.interpreter.globals, *reference.interpreter.globals,
            "{access:#010x} after the fault"
        );
        assert_eq!(specialized.steps, reference.steps);
    }
}

const SD_X12_X13: u32 = 0x00c6_b023;
const SC_D_X11_X12_X8: u32 = 0x18c4_35af;
const ADD_X14_X14_X11: u32 = 0x00b7_0733;
const SAME_HART_STORE_FLAG: &str = "zplat_reservation_invalidate_on_same_hart_store";
/// The doubleword the LR reserves in the tests below.
const RESERVED: u64 = 0x8000_1000;

/// A hart on 16 KiB of RAM at `lr.d x10, (x8)`, `sd x12, 0(x13)`,
/// `sc.d x11, x12, (x8)`, with `x8` at `RESERVED` and `x13` at `store`.
fn between_lr_and_sc(store: u64, specialize: bool) -> Riscv64<FlatMemory> {
    let mut cpu = Riscv64::new(FlatMemory::new(RAM, vec![0; 4 * 4096])).unwrap();
    cpu.specialize = specialize;
    restart(
        &mut cpu,
        &[LR_D_X10_X8, SD_X12_X13, SC_D_X11_X12_X8],
        RESERVED,
    );
    cpu.set_gpr(11, 0xdead).unwrap();
    cpu.set_gpr(12, 0x1122_3344_5566_7788).unwrap();
    cpu.set_gpr(13, store).unwrap();
    cpu
}

fn doubleword(cpu: &mut Riscv64<FlatMemory>, address: u64) -> u64 {
    let mut bytes = [0; 8];
    cpu.interpreter.memory.read(address, &mut bytes).unwrap();
    u64::from_le_bytes(bytes)
}

/// The model's platform setting `plat_reservation_invalidate_on_same_hart_store`
/// is `false`: a store by the hart that holds a reservation does not cancel
/// it, wherever the store lands. Only the platform reads the setting, and
/// here the interpreter is the platform.
#[test]
fn a_same_hart_store_keeps_the_reservation() {
    assert_eq!(
        between_lr_and_sc(0, false)
            .interpreter
            .globals
            .get(SAME_HART_STORE_FLAG),
        Some(&Value::Bool(false)),
        "the committed model changed its setting"
    );
    // Another page, the reserved page, and the reserved doubleword itself.
    for store in [0x8000_3000, 0x8000_1800, RESERVED] {
        for specialize in [false, true] {
            let case = format!("store to {store:#x}, specialize {specialize}");
            let mut cpu = between_lr_and_sc(store, specialize);
            for _ in 0..3 {
                assert!(!cpu.step().unwrap(), "{case}");
            }
            assert_eq!(cpu.gpr(11).unwrap(), 0, "{case}: the SC failed");
            assert_eq!(
                doubleword(&mut cpu, RESERVED),
                0x1122_3344_5566_7788,
                "{case}"
            );
            assert_eq!(doubleword(&mut cpu, store), 0x1122_3344_5566_7788, "{case}");
        }
    }
}

/// The same sequence in a loop, so that the specialiser runs the store from a
/// trace: no SC may fail, and the hart ends as under the plain interpreter.
#[test]
fn a_same_hart_store_in_a_trace_keeps_the_reservation() {
    // loop: lr.d; sd; sc.d; add x14, x14, x11; jal x0, loop
    let code = [
        LR_D_X10_X8,
        SD_X12_X13,
        SC_D_X11_X12_X8,
        ADD_X14_X14_X11,
        0xff1f_f06f,
    ];
    for store in [0x8000_3000, 0x8000_1800u64] {
        let mut reference = between_lr_and_sc(store, false);
        let mut specialized = between_lr_and_sc(store, true);
        restart(&mut reference, &code, RESERVED);
        restart(&mut specialized, &code, RESERVED);
        let mut done = 0;
        while done < 2_000 {
            let (ran, waiting) = specialized.run(7).unwrap();
            assert!(ran > 0 && !waiting);
            assert_eq!(reference.run(ran).unwrap(), (ran, false));
            done += ran;
            assert_eq!(
                *specialized.interpreter.globals, *reference.interpreter.globals,
                "store to {store:#x} after {done} steps"
            );
        }
        assert_eq!(
            reference.gpr(14).unwrap(),
            0,
            "an SC failed, store to {store:#x}"
        );
        assert_eq!(
            specialized.interpreter.memory.bytes(),
            reference.interpreter.memory.bytes()
        );
        assert!(specialized.specialization_stats.hits > 0);
    }
}

/// With the setting on, the platform cancels the reservation at any store by
/// the hart, as the Sail emulator does.
#[test]
fn a_same_hart_store_cancels_the_reservation_when_the_model_says_so() {
    for specialize in [false, true] {
        let mut cpu = between_lr_and_sc(0x8000_3000, specialize);
        cpu.interpreter
            .globals
            .insert(SAME_HART_STORE_FLAG.into(), Value::Bool(true));
        for _ in 0..3 {
            assert!(!cpu.step().unwrap());
        }
        assert_eq!(cpu.gpr(11).unwrap(), 1, "specialize {specialize}");
        assert_eq!(doubleword(&mut cpu, RESERVED), 0, "specialize {specialize}");
    }
}

/// A store that did not come from the hart still breaks its reservation, and
/// a later store by the hart to the same granule does not bring it back.
#[test]
fn a_same_hart_store_does_not_restore_a_broken_reservation() {
    for specialize in [false, true] {
        let mut cpu = between_lr_and_sc(0x8000_1800, specialize);
        assert!(!cpu.step().unwrap());
        // Another hart, or a device, writes into the reserved granule.
        cpu.interpreter.memory.write(0x8000_1008, &[1]).unwrap();
        for _ in 0..2 {
            assert!(!cpu.step().unwrap());
        }
        assert_eq!(cpu.gpr(11).unwrap(), 1, "specialize {specialize}");
        assert_eq!(doubleword(&mut cpu, RESERVED), 0, "specialize {specialize}");
    }
}
