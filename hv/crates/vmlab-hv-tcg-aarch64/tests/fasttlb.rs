//! The fast TLB of compiled code (3e DESIGN.md H7, card J3) on programs with
//! the JIT on: what fills it (H7a, H11a), what empties it (H7b), the watch
//! word of a store through it (card S3), and stores into translated code
//! with the JIT on (K6).

use vmlab_hv_tcg::{FlatMemory, Memory};
use vmlab_hv_tcg_aarch64::oracle::asm::{self, op};
use vmlab_hv_tcg_aarch64::sysreg;
use vmlab_hv_tcg_aarch64::{Cpu, State, Stop};

const RAM: u64 = 0x4000_0000;
const CODE: u64 = RAM;
const VECTORS: u64 = RAM + 0x1000;
const ROOT: u64 = RAM + 0x2000;
const TABLE_PAGES: u64 = 16;
const DATA: u64 = RAM + 0x2_0000;
const OTHER: u64 = RAM + 0x2_1000;
/// Where the tests map their data, in the lower half.
const VA: u64 = 0x80_0012_3000;

const VALID_PAGE: u64 = 3;
const AF: u64 = 1 << 10;
const USER: u64 = 1 << 6;
const READ_ONLY: u64 = 1 << 7;
const NOT_GLOBAL: u64 = 1 << 11;
const NO_EXECUTE: u64 = 3 << 53;

const HVC: u32 = 0xd400_0002;

/// The levels of the tables of the fast TLB.
const EL0: usize = 0;
const EL1: usize = 1;

struct Machine {
    cpu: Cpu<FlatMemory>,
    next_table: u64,
}

impl Machine {
    /// A CPU at EL1 with the MMU on and the JIT on, 48 bits in each half,
    /// the 4K granule, and `words` at `CODE`, mapped at its own address.
    /// Every exception ends a run at the call that fills the vector table.
    fn new(words: &[u32]) -> Self {
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
        let mut cpu = Cpu::new(FlatMemory::new(RAM, data), state);
        cpu.set_jit(true).expect("the JIT starts");
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

    /// Map the 4K page of `va` with the page descriptor `leaf`.
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

    /// Run from the start of the code to the call at its end.
    fn run(&mut self) {
        self.cpu.state.set_pc(CODE);
        assert!(matches!(self.cpu.run(100), Stop::Hypercall { .. }));
    }

    /// Whether the fast TLB holds the page of `va` for `level`.
    fn holds(&self, level: usize, write: bool, va: u64) -> bool {
        self.cpu
            .fast_tlb()
            .expect("the JIT is on")
            .lookup(level, write, va & !0xfff, 8)
            .is_some()
    }

    fn misses(&self) -> u64 {
        self.cpu.jit_counts().tlb_misses
    }
}

fn ldr(rt: u32, rn: u32) -> u32 {
    0xf940_0000 | rn << 5 | rt
}

fn str(rt: u32, rn: u32) -> u32 {
    0xf900_0000 | rn << 5 | rt
}

fn ldtr(rt: u32, rn: u32) -> u32 {
    0xf840_0800 | rn << 5 | rt
}

fn sttr(rt: u32, rn: u32) -> u32 {
    0xf800_0800 | rn << 5 | rt
}

fn ldxr(rt: u32, rn: u32) -> u32 {
    0xc85f_7c00 | rn << 5 | rt
}

#[test]
fn a_load_and_a_store_of_ram_fill_the_tables_of_their_level_and_then_hit() {
    let mut machine = Machine::new(&[ldr(1, 0), str(1, 2), HVC]);
    machine.map(VA, DATA | AF | NO_EXECUTE);
    machine.map(VA + 0x1000, OTHER | AF | NO_EXECUTE);
    machine
        .cpu
        .memory
        .write(DATA + 0x10, &0x1122_3344u64.to_le_bytes())
        .unwrap();
    machine.cpu.state.set_x(0, VA + 0x10);
    machine.cpu.state.set_x(2, VA + 0x1ff8);
    machine.run();
    assert_eq!(machine.cpu.taken, None);
    assert_eq!(machine.cpu.state.x(1), 0x1122_3344);
    assert_eq!(machine.word(OTHER + 0xff8), 0x1122_3344);
    // A read entry for the load, a write entry for the store, at EL1.
    assert!(machine.holds(EL1, false, VA));
    assert!(machine.holds(EL1, true, VA + 0x1000));
    assert!(!machine.holds(EL1, true, VA) && !machine.holds(EL1, false, VA + 0x1000));
    assert!(!machine.holds(EL0, false, VA) && !machine.holds(EL0, true, VA + 0x1000));
    assert_eq!(machine.misses(), 2);
    // Again: both hit, and both reach the memory.
    machine
        .cpu
        .memory
        .write(DATA + 0x10, &0x5566u64.to_le_bytes())
        .unwrap();
    machine.run();
    assert_eq!(machine.misses(), 2);
    assert_eq!(machine.cpu.jit_counts().tlb_accesses, 4);
    assert_eq!(machine.word(OTHER + 0xff8), 0x5566);
}

#[test]
fn an_access_of_el0_fills_the_tables_of_el0() {
    let mut machine = Machine::new(&[ldr(1, 0), str(1, 2), HVC]);
    machine.map(VA, DATA | AF | USER | NO_EXECUTE);
    machine.cpu.state.set_x(0, VA + 0x10);
    machine.cpu.state.set_x(2, VA + 0x20);
    assert!(machine.cpu.state.set_cpsr(0x3c0));
    // The call at the end is undefined at EL0: the run ends in its vector.
    assert!(matches!(machine.cpu.run(100), Stop::Hypercall { .. }));
    assert!(machine.holds(EL0, false, VA) && machine.holds(EL0, true, VA));
    assert!(!machine.holds(EL1, false, VA) && !machine.holds(EL1, true, VA));
}

#[test]
fn an_unprivileged_access_fills_nothing() {
    let mut machine = Machine::new(&[ldtr(1, 0), sttr(1, 2), HVC]);
    machine.map(VA, DATA | AF | USER | NO_EXECUTE);
    machine.cpu.state.set_x(0, VA + 0x10);
    machine.cpu.state.set_x(2, VA + 0x20);
    machine.run();
    assert_eq!(machine.cpu.taken, None);
    assert!(machine.cpu.fast_tlb().expect("the JIT is on").is_empty());
    // They take the thunk, which is not a miss of the fast TLB.
    assert_eq!(machine.misses(), 0);
}

#[test]
fn a_page_without_the_write_permission_takes_only_a_read_entry() {
    let mut machine = Machine::new(&[ldr(1, 0), str(1, 0), HVC]);
    machine.map(VA, DATA | AF | READ_ONLY | NO_EXECUTE);
    machine.cpu.state.set_x(0, VA + 0x10);
    assert!(matches!(machine.cpu.run(100), Stop::Hypercall { .. }));
    // The store took its permission fault.
    assert!(machine.cpu.taken.is_some());
    assert!(machine.holds(EL1, false, VA));
    assert!(!machine.holds(EL1, true, VA));
}

#[test]
fn a_memory_without_host_pages_fills_nothing() {
    /// `FlatMemory` without its host pages: every access is a call, as for
    /// a device.
    struct Calls(FlatMemory);
    impl Memory for Calls {
        fn read(&mut self, address: u64, bytes: &mut [u8]) -> vmlab_hv_tcg::Result<()> {
            self.0.read(address, bytes)
        }
        fn write(&mut self, address: u64, bytes: &[u8]) -> vmlab_hv_tcg::Result<()> {
            self.0.write(address, bytes)
        }
        fn generation(&self) -> u64 {
            self.0.generation()
        }
        fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> vmlab_hv_tcg::Result<u64> {
            self.0.load_reserved(address, bytes)
        }
        fn store_conditional(
            &mut self,
            address: u64,
            bytes: &[u8],
            generation: u64,
        ) -> vmlab_hv_tcg::Result<bool> {
            self.0.store_conditional(address, bytes, generation)
        }
        fn transaction_lock(
            &self,
        ) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
            None
        }
    }
    let mut data = vec![0u8; 0x4000];
    data[..12].copy_from_slice(&asm::bytes(&[ldr(1, 0), str(1, 2), HVC]));
    let mut cpu = Cpu::new(Calls(FlatMemory::new(RAM, data)), State::reset(RAM));
    cpu.set_jit(true).expect("the JIT starts");
    cpu.state.set_x(0, RAM + 0x2000);
    cpu.state.set_x(2, RAM + 0x3000);
    assert!(matches!(cpu.run(100), Stop::Hypercall { .. }));
    assert_eq!(cpu.taken, None);
    assert!(cpu.fast_tlb().expect("the JIT is on").is_empty());
    assert_eq!(cpu.jit_counts().tlb_misses, 2);
}

#[test]
fn a_page_that_gets_translated_code_loses_its_write_entries() {
    // H11a: str x1, [x0] ; blr x3 into the page of the store, whose first
    // word is the call that ends the run.
    let mut machine = Machine::new(&[str(1, 0), asm::blr(3), HVC]);
    machine.map(VA, DATA | AF);
    machine
        .cpu
        .memory
        .write(DATA, &u64::from(HVC).to_le_bytes())
        .unwrap();
    machine.cpu.state.set_x(0, VA + 0x100);
    machine.cpu.state.set_x(3, VA + 0x800);
    // First only the store: the page has a write entry.
    machine.cpu.state.set_pc(CODE);
    machine.cpu.step().unwrap();
    assert!(machine.holds(EL1, true, VA));
    machine
        .cpu
        .memory
        .write(DATA + 0x800, &u64::from(HVC).to_le_bytes())
        .unwrap();
    machine.cpu.state.set_pc(CODE);
    assert!(matches!(machine.cpu.run(100), Stop::Hypercall { pc, .. } if pc == VA + 0x800));
    assert!(!machine.holds(EL1, true, VA));
}

#[test]
fn an_exclusive_mark_keeps_stores_away_from_the_write_table() {
    // ldxr x2, [x0] ; str x1, [x4]
    let mut machine = Machine::new(&[ldxr(2, 0), str(1, 4), HVC]);
    machine.map(VA, DATA | AF | NO_EXECUTE);
    machine.map(VA + 0x1000, OTHER | AF | NO_EXECUTE);
    machine.cpu.state.set_x(0, VA);
    machine.cpu.state.set_x(4, VA + 0x1008);
    machine.run();
    assert_eq!(machine.cpu.taken, None);
    // The mark is set: the store filled no write entry.
    assert!(!machine.holds(EL1, true, VA + 0x1000));
    // Without a mark it does.
    machine.cpu.clear_exclusive();
    machine.cpu.state.set_pc(CODE + 4);
    machine.cpu.step().unwrap();
    assert!(machine.holds(EL1, true, VA + 0x1000));
    // With the entry there, the block that sets the mark makes its store
    // through the thunk all the same: it misses.
    machine.cpu.clear_exclusive();
    let misses = machine.misses();
    machine.run();
    assert_eq!(machine.misses(), misses + 1);
}

#[test]
fn every_flush_of_the_tlb_empties_the_fast_tlb() {
    // ldr x1, [x0] ; <the operation of x3> ; then the end.
    let mut cases: Vec<(&str, u32, u64, bool)> = vec![
        ("TLBI VMALLE1", asm::sys(op::TLBI_VMALLE1, 3), 0, true),
        ("TLBI VAAE1 of the page", asm::sys(op::TLBI_VAAE1, 3), VA >> 12, true),
        (
            "TLBI VAAE1 of another page",
            asm::sys(op::TLBI_VAAE1, 3),
            (VA >> 12) + 1,
            false,
        ),
        // A fast entry has no ASID: an ASID that is not the entry's drops
        // it all the same, which is more than must go.
        ("TLBI ASIDE1", asm::sys(0x043a, 3), 2 << 48, true),
        (
            "TTBR0_EL1 with another ASID",
            asm::msr(op::TTBR0_EL1, 3),
            ROOT | 2 << 48,
            true,
        ),
        (
            "TTBR0_EL1 as it was",
            asm::msr(op::TTBR0_EL1, 3),
            ROOT | 1 << 48,
            false,
        ),
    ];
    let sctlr = State::reset(CODE).system.regs[sysreg::reg::SCTLR_EL1] | sysreg::sctlr::M;
    cases.push((
        "SCTLR_EL1 with A",
        asm::msr(op::SCTLR_EL1, 3),
        sctlr | sysreg::sctlr::A,
        true,
    ));
    let tcr = 16 | 16 << 16 | 2 << 30 | 2 << 32 | 1 << 36;
    cases.push((
        "TCR_EL1 with 8 bits of ASID",
        asm::msr(op::TCR_EL1, 3),
        tcr & !(1 << 36),
        true,
    ));
    for (name, operation, argument, flushed) in cases {
        let mut machine = Machine::new(&[ldr(1, 0), operation, asm::ISB, HVC]);
        machine.map(VA, DATA | AF | NOT_GLOBAL | NO_EXECUTE);
        machine.cpu.state.set_x(0, VA);
        machine.cpu.state.set_x(3, argument);
        machine.cpu.state.set_pc(CODE);
        machine.cpu.step().unwrap();
        assert!(machine.holds(EL1, false, VA), "{name}");
        assert!(matches!(machine.cpu.run(10), Stop::Hypercall { .. }), "{name}");
        assert_eq!(machine.cpu.taken, None, "{name}");
        assert_eq!(!machine.holds(EL1, false, VA), flushed, "{name}");
    }
    // A new state from outside, and a new memory.
    let mut machine = Machine::new(&[ldr(1, 0), HVC]);
    machine.map(VA, DATA | AF | NO_EXECUTE);
    machine.cpu.state.set_x(0, VA);
    machine.run();
    assert!(machine.holds(EL1, false, VA));
    let state = machine.cpu.state.clone();
    machine.cpu.replace_state(state);
    assert!(machine.cpu.fast_tlb().expect("the JIT stays on").is_empty());
}

#[test]
fn a_store_through_the_fast_tlb_into_a_watched_page_moves_its_stamp() {
    let mut machine = Machine::new(&[str(1, 0), HVC]);
    machine.map(VA, DATA | AF | NO_EXECUTE);
    machine.cpu.state.set_x(0, VA + 0x40);
    machine.cpu.state.set_x(1, 7);
    machine.run();
    assert!(machine.holds(EL1, true, VA));
    // Another CPU translates code of the page: its watch word is odd.
    let word = machine.cpu.memory.watch_code(DATA).expect("a watch word");
    let stamp = machine.cpu.memory.code_stamp(DATA, 4).expect("a stamp");
    let misses = machine.misses();
    machine.cpu.state.set_x(1, 8);
    machine.run();
    // The store hit, wrote, and moved the stamp of the page.
    assert_eq!(machine.misses(), misses);
    assert_eq!(machine.word(DATA + 0x40), 8);
    let now = machine.cpu.memory.code_stamp(DATA, 4).expect("a stamp");
    assert_ne!(now.generation, stamp.generation);
    assert!(!machine.cpu.memory.watch_holds(DATA, word));
}

/// A CPU with the MMU off and the JIT on, with `words` at `RAM` and the
/// end word after them.
fn flat(words: &[u32]) -> Cpu<FlatMemory> {
    let mut data = vec![0u8; 0x4000];
    for index in 0..0x2000 / 4 {
        let word = words.get(index).copied().unwrap_or(HVC);
        data[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    let mut state = State::reset(RAM);
    state.system.regs[sysreg::reg::VBAR_EL1] = RAM + 0x1000;
    let mut cpu = Cpu::new(FlatMemory::new(RAM, data), state);
    cpu.set_jit(true).expect("the JIT starts");
    cpu
}

#[test]
fn k6_a_store_into_its_own_block_runs_the_new_code_with_the_jit_on() {
    // str w1, [x0] replaces the third instruction of its own block.
    let mut cpu = flat(&[0xb900_0001, 0x9100_0442, 0x9100_0463]);
    cpu.state.set_x(0, RAM + 8);
    cpu.state.set_x(1, 0x9100_0863);
    assert!(matches!(cpu.run(100), Stop::Hypercall { .. }));
    assert_eq!(cpu.taken, None);
    assert_eq!((cpu.state.x(2), cpu.state.x(3)), (1, 2));
    assert!(cpu.jit_counts().compiled_runs > 0);
}

#[test]
fn k6_a_loop_of_stores_into_data_keeps_its_blocks_with_the_jit_on() {
    // str x1, [x0], #8 ; subs x1, x1, #1 ; b.ne -8
    let mut cpu = flat(&[0xf800_8401, 0xf100_0421, 0x54ff_ffc1]);
    cpu.state.set_x(0, RAM + 0x2000);
    cpu.state.set_x(1, 100);
    assert!(matches!(cpu.run(u64::MAX), Stop::Hypercall { .. }));
    assert_eq!(cpu.taken, None);
    assert_eq!(cpu.translations, 2);
    let mut bytes = [0u8; 8];
    cpu.memory.read(RAM + 0x2000 + 99 * 8, &mut bytes).unwrap();
    assert_eq!(u64::from_le_bytes(bytes), 1);
    // One miss fills the page, and the other 99 stores hit.
    let counts = cpu.jit_counts();
    assert_eq!((counts.tlb_accesses, counts.tlb_misses), (100, 1));
    // A write from outside into the page of the code drops the blocks.
    cpu.memory.write(RAM + 0x800, &HVC.to_le_bytes()).unwrap();
    cpu.state.set_pc(RAM);
    cpu.state.set_x(1, 1);
    assert!(matches!(cpu.run(u64::MAX), Stop::Hypercall { .. }));
    assert_eq!(cpu.translations, 4);
}
