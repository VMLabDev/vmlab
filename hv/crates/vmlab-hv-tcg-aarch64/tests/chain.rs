//! Chaining and the jump cache of the JIT (3e DESIGN.md J4, H5, H6): a
//! store into the target of a linked jump unlinks it, a TLBI sends a branch
//! into another page through the lookup again, and the entry of the jump
//! cache of one context does not serve another. Each program also runs in
//! the interpreter, which must leave the same registers.

use vmlab_hv_tcg::{FlatMemory, Memory};
use vmlab_hv_tcg_aarch64::oracle::asm::{self, op};
use vmlab_hv_tcg_aarch64::state::slot;
use vmlab_hv_tcg_aarch64::sysreg;
use vmlab_hv_tcg_aarch64::{Cpu, State, Stop};

const BASE: u64 = 0x4000_0000;
const HVC: u32 = 0xd400_0002;

fn flat(words: &[u32], jit: bool) -> Cpu<FlatMemory> {
    let mut data = vec![0u8; 0x2000];
    data[..words.len() * 4].copy_from_slice(&asm::bytes(words));
    let mut cpu = Cpu::new(FlatMemory::new(BASE, data), State::reset(BASE));
    cpu.set_jit(jit).expect("a code region");
    cpu.set_chaining(true).expect("a code region");
    cpu
}

#[test]
fn a_chained_pair_is_unlinked_by_a_store_into_the_target() {
    // A loops through B a thousand times, with both of its exits linked.
    // Then the program writes over the first instruction of B and runs A
    // again: a jump of A that still went to the old code of B would count
    // X1 below zero and never reach the HVC.
    const ADD_X6: u32 = 0x9100_04c6;
    let program = [
        0x9100_0400, // 0x00 A: add x0, x0, #1
        0x1400_0003, // 0x04    b B
        0xd503_201f, // 0x08    nop
        0xd503_201f, // 0x0c    nop
        0xf100_0421, // 0x10 B: subs x1, x1, #1
        0x54ff_ff61, // 0x14    b.ne A
        0xb500_0087, // 0x18    cbnz x7, done
        0xd280_0027, // 0x1c    movz x7, #1
        0xb900_0083, // 0x20    str w3, [x4]: B is add x6, x6, #1 now
        0x17ff_fff7, // 0x24    b A
        HVC,         // 0x28 done
    ];
    let mut results = Vec::new();
    for jit in [false, true] {
        let mut cpu = flat(&program, jit);
        cpu.state.set_x(1, 1000);
        cpu.state.set_x(3, u64::from(ADD_X6));
        cpu.state.set_x(4, BASE + 0x10);
        assert!(
            matches!(cpu.run(100_000), Stop::Hypercall { .. }),
            "jit {jit}: {:?}",
            cpu.state
        );
        assert_eq!(cpu.state.x(0), 1001, "jit {jit}");
        assert_eq!(cpu.state.x(1), 0, "jit {jit}");
        assert_eq!(cpu.state.x(6), 1, "jit {jit}");
        if jit {
            let counts = cpu.jit_counts();
            // The loop ran on linked jumps, and the store set them back.
            assert!(counts.chained > 1500, "{counts:?}");
            assert!(counts.links >= 2, "{counts:?}");
            assert!(counts.unlinks >= 2, "{counts:?}");
        }
        results.push((cpu.state.slots, cpu.instructions));
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn the_switch_turns_chaining_off_and_the_result_stays() {
    // add x0, x0, #1 ; subs x1, x1, #1 ; b.ne -8 ; hvc
    let program = [0x9100_0400, 0xf100_0421, 0x54ff_ffc1, HVC];
    let mut results = Vec::new();
    for chaining in [true, false] {
        let mut cpu = flat(&program, true);
        cpu.set_chaining(chaining).expect("a code region");
        cpu.state.set_x(1, 5000);
        assert!(matches!(cpu.run(100_000), Stop::Hypercall { .. }));
        let counts = cpu.jit_counts();
        if chaining {
            assert!(counts.chained > 4000, "{counts:?}");
        } else {
            assert_eq!(
                (counts.chained, counts.jump_hits, counts.links, counts.fills),
                (0, 0, 0, 0),
                "{counts:?}"
            );
            assert!(counts.compiled_runs > 5000, "{counts:?}");
        }
        results.push((cpu.state.slots, cpu.instructions));
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn a_budget_ends_a_chained_loop_where_the_interpreter_ends_it() {
    // add x0, x0, #1 ; subs x1, x1, #1 ; b.ne -8 ; hvc: the budget runs out
    // inside the loop, at one instruction, for each budget.
    let program = [0x9100_0400, 0xf100_0421, 0x54ff_ffc1, HVC];
    for budget in [1, 2, 3, 4, 100, 1001, 3000] {
        let mut results = Vec::new();
        for jit in [false, true] {
            let mut cpu = flat(&program, jit);
            cpu.state.set_x(1, 1_000_000);
            assert_eq!(cpu.run(budget), Stop::Budget);
            assert_eq!(cpu.instructions, budget, "jit {jit}");
            results.push(cpu.state.slots);
        }
        assert_eq!(results[0], results[1], "budget {budget}");
    }
}

// A CPU at EL1 with the MMU on, as in `tests/translate.rs`.
const CODE: u64 = BASE;
const VECTORS: u64 = BASE + 0x1000;
const ROOT: u64 = BASE + 0x2000;
const TABLE_PAGES: u64 = 16;
const DATA: u64 = BASE + 0x2_0000;
const OTHER: u64 = BASE + 0x2_1000;
const RUN: u64 = 0x80_0012_5000;

const VALID_PAGE: u64 = 3;
const AF: u64 = 1 << 10;
const USER: u64 = 1 << 6;
const READ_ONLY: u64 = 1 << 7;
const NO_EXECUTE: u64 = 3 << 53;

struct Machine {
    cpu: Cpu<FlatMemory>,
    next_table: u64,
}

impl Machine {
    fn new(words: &[u32], jit: bool) -> Self {
        let mut data = vec![0u8; 0x10_0000];
        data[..words.len() * 4].copy_from_slice(&asm::bytes(words));
        for word in data[0x1000..0x2000].as_chunks_mut::<4>().0 {
            *word = HVC.to_le_bytes();
        }
        let mut state = State::reset(CODE);
        let regs = &mut state.system.regs;
        regs[sysreg::reg::VBAR_EL1] = VECTORS;
        regs[sysreg::reg::TTBR0_EL1] = ROOT | 1 << 48;
        regs[sysreg::reg::TTBR1_EL1] = ROOT;
        regs[sysreg::reg::TCR_EL1] = 16 | 16 << 16 | 2 << 30 | 2 << 32 | 1 << 36;
        regs[sysreg::reg::MAIR_EL1] = 0x04ff;
        regs[sysreg::reg::SCTLR_EL1] |= sysreg::sctlr::M;
        state.refresh();
        let mut cpu = Cpu::new(FlatMemory::new(BASE, data), state);
        cpu.set_jit(jit).expect("a code region");
        cpu.set_chaining(true).expect("a code region");
        let mut machine = Self {
            cpu,
            next_table: ROOT + 0x1000,
        };
        machine.map(CODE, CODE | AF | USER | READ_ONLY);
        machine.map(VECTORS, VECTORS | AF | READ_ONLY);
        machine
    }

    fn word(&mut self, address: u64) -> u64 {
        let mut bytes = [0u8; 8];
        self.cpu.memory.read(address, &mut bytes).unwrap();
        u64::from_le_bytes(bytes)
    }

    fn map(&mut self, va: u64, leaf: u64) -> u64 {
        let mut table = ROOT;
        for level in 0..3 {
            let entry = table + ((va >> (39 - 9 * level)) & 0x1ff) * 8;
            let mut next = self.word(entry) & 0x0000_ffff_ffff_f000;
            if next == 0 {
                next = self.next_table;
                self.next_table += 0x1000;
                assert!(next < ROOT + TABLE_PAGES * 0x1000, "the tables are full");
                self.cpu
                    .memory
                    .write(entry, &(next | 3).to_le_bytes())
                    .unwrap();
            }
            table = next;
        }
        let entry = table + ((va >> 12) & 0x1ff) * 8;
        self.cpu
            .memory
            .write(entry, &(leaf | VALID_PAGE).to_le_bytes())
            .unwrap();
        entry
    }
}

#[test]
fn a_tlbi_sends_a_branch_into_another_page_through_the_lookup() {
    // RUN maps one page of code, then the other, with a TLBI between. The
    // second call is a hit of the jump cache. The third, after the TLBI,
    // must not be: the entry still names the code of the first page.
    let program = [
        asm::blr(2),
        asm::blr(2),
        0xf900_0003, // str x3, [x0]
        asm::sys(op::TLBI_VMALLE1, 31),
        asm::blr(2),
        HVC,
    ];
    let mut results = Vec::new();
    for jit in [false, true] {
        let mut machine = Machine::new(&program, jit);
        let entry = machine.map(RUN, DATA | AF | READ_ONLY);
        for page in 0..TABLE_PAGES {
            machine.map(
                ROOT + page * 0x1000,
                (ROOT + page * 0x1000) | AF | NO_EXECUTE,
            );
        }
        // add x1, x1, #1 ; ret, and add x1, x1, #5 ; ret.
        machine
            .cpu
            .memory
            .write(DATA, &asm::bytes(&[0x9100_0421, asm::RET]))
            .unwrap();
        machine
            .cpu
            .memory
            .write(OTHER, &asm::bytes(&[0x9100_1421, asm::RET]))
            .unwrap();
        machine.cpu.state.set_x(0, entry);
        machine.cpu.state.set_x(2, RUN);
        machine
            .cpu
            .state
            .set_x(3, OTHER | AF | READ_ONLY | VALID_PAGE);
        assert!(matches!(machine.cpu.run(100), Stop::Hypercall { .. }));
        assert_eq!(machine.cpu.taken, None);
        assert_eq!(machine.cpu.state.x(1), 7, "jit {jit}");
        if jit {
            let counts = machine.cpu.jit_counts();
            assert!(counts.jump_hits >= 1, "{counts:?}");
            assert!(counts.fills >= 3, "{counts:?}");
        }
        results.push(machine.cpu.state.slots);
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn a_context_change_is_never_chained() {
    // The same code runs at EL1 and at EL0: `mov x5, sp` reads SP_EL1 at
    // EL1 and SP_EL0 at EL0, so a block of one context that served the
    // other would leave the wrong stack pointer. ERET ends its block, and
    // the call at EL0 finds no entry of its context in the jump cache.
    let program = [
        asm::blr(2), // 0x00 EL1
        asm::blr(2), // 0x04 EL1: a hit of the jump cache
        asm::ERET,   // 0x08 to 0x0c at EL0
        asm::blr(2), // 0x0c EL0
        HVC,         // 0x10 undefined at EL0: the vector stops the run
    ];
    let mut results = Vec::new();
    for jit in [false, true] {
        let mut machine = Machine::new(&program, jit);
        machine.map(RUN, DATA | AF | USER | READ_ONLY);
        // mov x5, sp ; ret
        machine
            .cpu
            .memory
            .write(DATA, &asm::bytes(&[0x9100_03e5, asm::RET]))
            .unwrap();
        let state = &mut machine.cpu.state;
        state.set_x(2, RUN);
        state.slots[slot::SP_EL0] = 0x2222_0000;
        state.slots[slot::SP_EL1] = 0x1111_0000;
        state.system.regs[sysreg::reg::ELR_EL1] = CODE + 0x0c;
        // EL0t with the interrupts masked.
        state.system.regs[sysreg::reg::SPSR_EL1] = 0x3c0;
        assert!(matches!(machine.cpu.run(100), Stop::Hypercall { .. }));
        assert_eq!(machine.cpu.state.x(5), 0x2222_0000, "jit {jit}");
        if jit {
            let counts = machine.cpu.jit_counts();
            assert!(counts.jump_hits >= 1, "{counts:?}");
            assert!(counts.lookups >= 3, "{counts:?}");
        }
        results.push(machine.cpu.state.slots);
    }
    assert_eq!(results[0], results[1]);
}
