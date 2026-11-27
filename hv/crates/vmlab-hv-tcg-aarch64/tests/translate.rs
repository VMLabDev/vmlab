//! Programs with the MMU on (card C8): translation, permissions and their
//! faults, the unprivileged loads and stores, `DC ZVA`, the address
//! translation instructions, TLBI, and code that changes under an address.
//!
//! What QEMU does in each of these cases is in the lockstep of the card
//! (`VERIFICATION.md`). These tests hold the cases that a stream against
//! QEMU cannot: what a TLB keeps, and code that runs at two addresses.

use vmlab_hv_tcg::{FlatMemory, Memory};
use vmlab_hv_tcg_aarch64::exception::{Cause, Exception};
use vmlab_hv_tcg_aarch64::oracle::asm::{self, op};
use vmlab_hv_tcg_aarch64::sysreg;
use vmlab_hv_tcg_aarch64::{Cpu, State, Stop};

const RAM: u64 = 0x4000_0000;
/// The code, the vector table, the translation tables from their root on,
/// and two pages of data.
const CODE: u64 = RAM;
const VECTORS: u64 = RAM + 0x1000;
const ROOT: u64 = RAM + 0x2000;
const TABLE_PAGES: u64 = 16;
const DATA: u64 = RAM + 0x2_0000;
const OTHER: u64 = RAM + 0x2_1000;

/// Where the tests map their data: a page in the lower half. Its entry
/// of the TLB is not the one of the code, of the vectors or of the tables,
/// so that no fetch replaces it.
const VA: u64 = 0x80_0012_3000;

const VALID_PAGE: u64 = 3;
const AF: u64 = 1 << 10;
const USER: u64 = 1 << 6;
const READ_ONLY: u64 = 1 << 7;
const NOT_GLOBAL: u64 = 1 << 11;
const NO_EXECUTE: u64 = 3 << 53;

const HVC: u32 = 0xd400_0002;

struct Machine {
    cpu: Cpu<FlatMemory>,
    next_table: u64,
}

impl Machine {
    /// A CPU at EL1 with the MMU on, 48 bits in each half, the 4K granule,
    /// and `words` at `CODE`, mapped at its own address for both levels.
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
        // T0SZ and T1SZ 16, TG1 4K, IPS 40 bits, 16 bits of ASID.
        regs[sysreg::reg::TCR_EL1] = 16 | 16 << 16 | 2 << 30 | 2 << 32 | 1 << 36;
        // The index 0 is Normal memory, the index 1 Device memory.
        regs[sysreg::reg::MAIR_EL1] = 0x04ff;
        regs[sysreg::reg::SCTLR_EL1] |= sysreg::sctlr::M;
        state.refresh();
        let mut machine = Self {
            cpu: Cpu::new(FlatMemory::new(RAM, data), state),
            next_table: ROOT + 0x1000,
        };
        // Read-only for both levels: both may execute it.
        machine.map(CODE, CODE | AF | USER | READ_ONLY);
        machine.map(VECTORS, VECTORS | AF | READ_ONLY);
        machine
    }

    fn word(&mut self, address: u64) -> u64 {
        let mut bytes = [0u8; 8];
        self.cpu.memory.read(address, &mut bytes).unwrap();
        u64::from_le_bytes(bytes)
    }

    /// Map the 4K page of `va` with the page descriptor `leaf`, and give
    /// the address of that descriptor.
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

    /// The data abort that the CPU took last: FAR_EL1 and ESR_EL1.
    fn abort(&self) -> Option<(u64, u64)> {
        match self.cpu.taken?.cause {
            Cause::Exception(Exception::DataAbort { .. }) => Some((
                self.cpu.state.system.regs[sysreg::reg::FAR_EL1],
                self.cpu.state.system.regs[sysreg::reg::ESR_EL1],
            )),
            _ => None,
        }
    }

    fn enter_el0(&mut self) {
        assert!(self.cpu.state.set_cpsr(0x3c0));
    }
}

/// `LDR Xt, [Xn]`, `STR Xt, [Xn]`, `LDTR Xt, [Xn]` and `STTR Xt, [Xn]`.
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

#[test]
fn a_load_and_a_store_go_through_the_tables() {
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
    assert!(matches!(machine.cpu.run(10), Stop::Hypercall { .. }));
    assert_eq!(machine.cpu.taken, None);
    assert_eq!(machine.cpu.state.x(1), 0x1122_3344);
    // The store is in the last bytes of the second page: nothing of it is
    // in a third one.
    assert_eq!(machine.word(OTHER + 0xff8), 0x1122_3344);
    assert_eq!(
        machine.cpu.physical_address(VA + 0x1234),
        Some(OTHER + 0x234)
    );
    assert_eq!(machine.cpu.physical_address(VA + 0x2000), None);
    assert!(machine.cpu.tlb_entries() >= 3);
}

#[test]
fn an_access_crosses_into_a_page_with_its_own_translation() {
    // The two pages of the address are the two data pages the other way
    // round.
    let mut machine = Machine::new(&[str(1, 0), ldr(2, 0), HVC]);
    machine.map(VA, OTHER | AF | NO_EXECUTE);
    machine.map(VA + 0x1000, DATA | AF | NO_EXECUTE);
    machine.cpu.state.set_x(0, VA + 0xffd);
    machine.cpu.state.set_x(1, 0x8877_6655_4433_2211);
    // An access that is not aligned faults with SCTLR_EL1.A only, in this
    // Normal memory.
    assert!(matches!(machine.cpu.run(10), Stop::Hypercall { .. }));
    assert_eq!(machine.cpu.taken, None);
    assert_eq!(machine.cpu.state.x(2), 0x8877_6655_4433_2211);
    assert_eq!(machine.word(OTHER + 0xff8) >> 40, 0x33_2211);
    assert_eq!(machine.word(DATA) & 0xff_ffff_ffff, 0x88_7766_5544);
    // A second page without a translation: the fault names its first byte,
    // and nothing is written into the first page.
    let mut machine = Machine::new(&[str(1, 0), HVC]);
    machine.map(VA, DATA | AF | NO_EXECUTE);
    machine.cpu.state.set_x(0, VA + 0xffd);
    machine.cpu.state.set_x(1, u64::MAX);
    machine.cpu.step().unwrap();
    // A data abort from the same level, a write, a translation fault of
    // level 3.
    assert_eq!(machine.abort(), Some((VA + 0x1000, 0x9600_0047)));
    assert_eq!(machine.word(DATA + 0xff8), 0);
}

#[test]
fn each_fault_of_an_access_has_its_syndrome() {
    // What the page descriptor is, and ESR_EL1 of a load and of a store at
    // EL1 and at EL0.
    let cases: [(u64, [Option<u64>; 4]); 6] = [
        // No access flag: an access flag fault of level 3.
        (
            0,
            [
                Some(0x9600_000b),
                Some(0x9600_004b),
                Some(0x9200_000b),
                Some(0x9200_004b),
            ],
        ),
        // For EL1 only.
        (AF, [None, None, Some(0x9200_000f), Some(0x9200_004f)]),
        (
            AF | READ_ONLY,
            [
                None,
                Some(0x9600_004f),
                Some(0x9200_000f),
                Some(0x9200_004f),
            ],
        ),
        (AF | USER, [None, None, None, None]),
        (
            AF | USER | READ_ONLY,
            [None, Some(0x9600_004f), None, Some(0x9200_004f)],
        ),
        // An output address above the 40 bits of IPS.
        (AF | 1 << 44, [Some(0x9600_0003); 4].map(|esr| esr)),
    ];
    for (leaf, expected) in cases {
        for (index, expected) in expected.into_iter().enumerate() {
            let (store, user) = (index & 1 != 0, index & 2 != 0);
            let mut machine = Machine::new(&[if store { str(1, 0) } else { ldr(1, 0) }, HVC]);
            machine.map(VA, DATA | leaf | NO_EXECUTE);
            machine.cpu.state.set_x(0, VA + 0x40);
            if user {
                machine.enter_el0();
            }
            machine.cpu.step().unwrap();
            let expected = expected.map(|esr| {
                // The address size fault is the same from both levels but
                // for the class and the direction.
                let esr = if leaf & (1 << 44) != 0 {
                    0x9200_0003 | u64::from(!user) << 26 | u64::from(store) << 6
                } else {
                    esr
                };
                (VA + 0x40, esr)
            });
            assert_eq!(
                machine.abort(),
                expected,
                "{leaf:#x} store {store} user {user}"
            );
        }
    }
    // No descriptor at level 3, and none at level 1.
    let mut machine = Machine::new(&[ldr(1, 0), ldr(1, 2), HVC]);
    machine.map(VA, DATA | AF);
    machine.cpu.state.set_x(0, VA + 0x1000);
    machine.cpu.state.set_x(2, VA + (1 << 30));
    machine.cpu.step().unwrap();
    assert_eq!(machine.abort(), Some((VA + 0x1000, 0x9600_0007)));
    machine.cpu.state.set_pc(CODE + 4);
    machine.cpu.step().unwrap();
    assert_eq!(machine.abort(), Some((VA + (1 << 30), 0x9600_0005)));
    // An address between the two halves: a translation fault of level 0.
    let mut machine = Machine::new(&[ldr(1, 0), HVC]);
    machine.cpu.state.set_x(0, 1 << 50);
    machine.cpu.step().unwrap();
    assert_eq!(machine.abort(), Some((1 << 50, 0x9600_0004)));
    // A page that the memory of the machine does not hold: an external
    // abort.
    let mut machine = Machine::new(&[ldr(1, 0), HVC]);
    machine.map(VA, 0x7000_0000 | AF);
    machine.cpu.state.set_x(0, VA);
    machine.cpu.step().unwrap();
    assert_eq!(machine.abort(), Some((VA, 0x9600_0010)));
}

#[test]
fn an_unprivileged_access_is_made_as_el0() {
    // ldtr x1, [x0] ; sttr x1, [x0] ; ldr x2, [x0]
    let mut machine = Machine::new(&[ldtr(1, 0), sttr(1, 0), ldr(2, 0), HVC]);
    machine.map(VA, DATA | AF | NO_EXECUTE);
    machine
        .cpu
        .memory
        .write(DATA, &77u64.to_le_bytes())
        .unwrap();
    machine.cpu.state.set_x(0, VA);
    machine.cpu.step().unwrap();
    // A permission fault of level 3 from EL1, as a read and as a write.
    assert_eq!(machine.abort(), Some((VA, 0x9600_000f)));
    machine.cpu.state.set_pc(CODE + 4);
    machine.cpu.step().unwrap();
    assert_eq!(machine.abort(), Some((VA, 0x9600_004f)));
    // EL1 itself may read the page.
    machine.cpu.state.set_pc(CODE + 8);
    machine.cpu.step().unwrap();
    assert_eq!((machine.cpu.taken, machine.cpu.state.x(2)), (None, 77));
}

#[test]
fn alignment_faults_in_device_memory_and_with_sctlr_a() {
    for (attribute, strict, faults) in [(0u64, false, false), (1, false, true), (0, true, true)] {
        let mut machine = Machine::new(&[ldr(1, 0), HVC]);
        machine.map(VA, DATA | AF | attribute << 2);
        if strict {
            machine.cpu.state.system.regs[sysreg::reg::SCTLR_EL1] |= sysreg::sctlr::A;
        }
        machine.cpu.state.set_x(0, VA + 3);
        machine.cpu.step().unwrap();
        let expected = faults.then_some((VA + 3, 0x9600_0021));
        assert_eq!(
            machine.abort(),
            expected,
            "attribute {attribute} strict {strict}"
        );
    }
    // In Device memory the permission fault of the page comes after the
    // alignment fault, and the access flag fault before it.
    for (leaf, esr) in [(AF | READ_ONLY, 0x9600_0061u64), (READ_ONLY, 0x9600_004b)] {
        let mut machine = Machine::new(&[str(1, 0), HVC]);
        machine.map(VA, DATA | leaf | 1 << 2);
        machine.cpu.state.set_x(0, VA + 3);
        machine.cpu.step().unwrap();
        assert_eq!(machine.abort(), Some((VA + 3, esr)));
    }
}

#[test]
fn execution_needs_the_permission_of_its_level() {
    // The page of the data holds the call that ends a run.
    for (leaf, user, status) in [
        (AF, false, None),
        (AF | 1 << 53, false, Some(0x0f)),
        // What EL0 can write, EL1 does not execute.
        (AF | USER, false, Some(0x0f)),
        (AF | USER, true, None),
        (AF | USER | 1 << 54, true, Some(0x0f)),
        (0, false, Some(0x0b)),
    ] {
        let mut machine = Machine::new(&[0xd61f_0000]); // br x0
        machine.map(VA, DATA | leaf);
        machine.cpu.memory.write(DATA, &HVC.to_le_bytes()).unwrap();
        machine.cpu.state.set_x(0, VA);
        if user {
            machine.enter_el0();
        }
        machine.cpu.step().unwrap();
        machine.cpu.step().unwrap_or_default();
        let taken = machine.cpu.taken.map(|taken| taken.cause);
        let expected = status.map(|status| Cause::Exception(Exception::InstructionAbort(status)));
        // At EL0 the call is undefined: it shows that the page ran.
        let ran = Some(Cause::Exception(Exception::Undefined));
        assert_eq!(
            taken,
            expected.or(if user { ran } else { None }),
            "{leaf:#x} {user}"
        );
        if status.is_some() {
            assert_eq!(machine.cpu.state.system.regs[sysreg::reg::FAR_EL1], VA);
        }
    }
}

#[test]
fn a_tagged_address_translates_with_tbi_and_a_branch_drops_the_tag() {
    let tagged = VA | 0x5a << 56;
    let mut machine = Machine::new(&[ldr(1, 0), 0xd61f_0040]); // ldr x1, [x0] ; br x2
    machine.map(VA, DATA | AF);
    machine.cpu.memory.write(DATA, &HVC.to_le_bytes()).unwrap();
    machine.cpu.state.set_x(0, tagged);
    machine.cpu.state.set_x(2, tagged);
    // Without TBI0 the tag is a part of the address, which no half holds.
    machine.cpu.step().unwrap();
    assert_eq!(machine.abort(), Some((tagged, 0x9600_0004)));
    machine.cpu.state.system.regs[sysreg::reg::TCR_EL1] |= 1 << 37;
    machine.cpu.state.refresh();
    machine.cpu.state.set_pc(CODE);
    machine.cpu.step().unwrap();
    assert_eq!(
        (machine.cpu.taken, machine.cpu.state.x(1) as u32),
        (None, HVC)
    );
    machine.cpu.step().unwrap();
    assert_eq!(machine.cpu.state.pc(), VA);
    // With TBID0, as QEMU has it, the branch keeps the tag, and the fetch
    // at that address finds no half.
    machine.cpu.state.system.regs[sysreg::reg::TCR_EL1] |= 1 << 51;
    machine.cpu.state.refresh();
    machine.cpu.state.set_pc(CODE + 4);
    machine.cpu.step().unwrap();
    assert_eq!(machine.cpu.state.pc(), tagged);
    machine.cpu.step().unwrap();
    assert_eq!(
        machine.cpu.taken.map(|taken| taken.cause),
        Some(Cause::Exception(Exception::InstructionAbort(0x04)))
    );
}

#[test]
fn dc_zva_zeroes_its_block_and_faults_as_a_store() {
    let mut machine = Machine::new(&[asm::sys(op::DC_ZVA, 0), HVC]);
    machine.map(VA, DATA | AF | NO_EXECUTE);
    machine.cpu.memory.write(DATA, &[0xaa; 0x100]).unwrap();
    machine.cpu.state.set_x(0, VA + 0x7b);
    assert!(matches!(machine.cpu.run(10), Stop::Hypercall { .. }));
    assert_eq!(machine.cpu.taken, None);
    let mut bytes = [0u8; 0x100];
    machine.cpu.memory.read(DATA, &mut bytes).unwrap();
    // The 64 bytes that hold the address, and nothing beside them.
    assert!(bytes[..0x40].iter().all(|byte| *byte == 0xaa));
    assert!(bytes[0x40..0x80].iter().all(|byte| *byte == 0));
    assert!(bytes[0x80..].iter().all(|byte| *byte == 0xaa));
    // A page that cannot be written: the fault names the address that the
    // instruction gave, and says "write".
    let mut machine = Machine::new(&[asm::sys(op::DC_ZVA, 0), HVC]);
    machine.map(VA, DATA | AF | READ_ONLY);
    machine.cpu.state.set_x(0, VA + 0x7b);
    machine.cpu.step().unwrap();
    assert_eq!(machine.abort(), Some((VA + 0x7b, 0x9600_004f)));
    // At EL0 the instruction traps until SCTLR_EL1.DZE allows it.
    let mut machine = Machine::new(&[asm::sys(op::DC_ZVA, 0), HVC]);
    machine.map(VA, DATA | AF | USER | NO_EXECUTE);
    machine.cpu.state.set_x(0, VA);
    machine.enter_el0();
    machine.cpu.step().unwrap();
    assert_eq!(
        machine.cpu.taken.map(|taken| taken.cause),
        Some(Cause::Exception(Exception::SystemTrap(sysreg::trap_iss(
            0x4000 | op::DC_ZVA,
            0,
            true
        ))))
    );
    // op0 1, op2 1, op1 3, CRn 7, CRm 4, a write, as QEMU reports it.
    assert_eq!(
        machine.cpu.state.system.regs[sysreg::reg::ESR_EL1],
        0x6212_dc08
    );
    machine.cpu.state.system.regs[sysreg::reg::SCTLR_EL1] |= sysreg::sctlr::DZE;
    machine.enter_el0();
    machine.cpu.state.set_pc(CODE);
    machine.cpu.step().unwrap();
    assert_eq!(machine.cpu.taken, None);
}

#[test]
fn an_address_translation_instruction_fills_par() {
    // at s1e1r, x0 ; at s1e1w, x0 ; at s1e0r, x0 ; at s1e1r, x1
    let mut machine = Machine::new(&[
        asm::sys(op::AT_S1E1R, 0),
        asm::sys(op::AT_S1E1R + 1, 0),
        asm::sys(op::AT_S1E1R + 2, 0),
        asm::sys(op::AT_S1E1R, 1),
        HVC,
    ]);
    machine.map(VA, DATA | AF | READ_ONLY | 3 << 8);
    machine.cpu.state.set_x(0, VA + 0x123);
    machine.cpu.state.set_x(1, VA + 0x1000);
    let mut par = Vec::new();
    for _ in 0..4 {
        machine.cpu.step().unwrap();
        assert_eq!(machine.cpu.taken, None);
        par.push(machine.cpu.state.system.regs[sysreg::reg::PAR_EL1]);
    }
    // The page with its attributes, then a permission fault of level 3 for
    // the write and for EL0, then a translation fault of level 3.
    assert_eq!(par, vec![0xff00_0000_0000_0b80 | DATA, 0x81f, 0x81f, 0x80f]);
    // At EL0 the instruction is undefined.
    let mut machine = Machine::new(&[asm::sys(op::AT_S1E1R, 0), HVC]);
    machine.enter_el0();
    machine.cpu.step().unwrap();
    assert_eq!(
        machine.cpu.taken.map(|taken| taken.cause),
        Some(Cause::Exception(Exception::Undefined))
    );
}

#[test]
fn a_changed_descriptor_holds_after_its_tlbi() {
    // ldr x1, [x0] ; tlbi <op>, x3 ; ldr x2, [x0]
    for (operation, argument, flushed) in [
        (op::TLBI_VMALLE1, 0u64, true),
        (op::TLBI_VAAE1, VA >> 12, true),
        (op::TLBI_VAALE1, VA >> 12, true),
        // VAE1 and VALE1 with the ASID of the entry, and with another one.
        (0x0439, VA >> 12 | 1 << 48, true),
        (0x043d, VA >> 12 | 1 << 48, true),
        (0x0439, VA >> 12 | 2 << 48, false),
        // ASIDE1 the same.
        (0x043a, 1 << 48, true),
        (0x043a, 2 << 48, false),
        // Another address.
        (op::TLBI_VAAE1, (VA >> 12) + 1, false),
        // The forms for every CPU: CRm 3.
        (0x0418, 0, true),
        (0x0419, VA >> 12 | 1 << 48, true),
    ] {
        let mut machine = Machine::new(&[ldr(1, 0), asm::sys(operation, 3), ldr(2, 0), HVC]);
        let entry = machine.map(VA, DATA | AF | NOT_GLOBAL);
        machine.cpu.memory.write(DATA, &1u64.to_le_bytes()).unwrap();
        machine
            .cpu
            .memory
            .write(OTHER, &2u64.to_le_bytes())
            .unwrap();
        machine.cpu.state.set_x(0, VA);
        machine.cpu.state.set_x(3, argument);
        machine.cpu.step().unwrap();
        machine
            .cpu
            .memory
            .write(entry, &(OTHER | AF | NOT_GLOBAL | VALID_PAGE).to_le_bytes())
            .unwrap();
        assert!(matches!(machine.cpu.run(10), Stop::Hypercall { .. }));
        assert_eq!(machine.cpu.taken, None);
        let expected = if flushed { 2 } else { 1 };
        assert_eq!(
            machine.cpu.state.x(2),
            expected,
            "{operation:#x} {argument:#x}"
        );
    }
    // A global entry goes with its address, whatever the ASID.
    let mut machine = Machine::new(&[ldr(1, 0), asm::sys(0x0439, 3), ldr(2, 0), HVC]);
    let entry = machine.map(VA, DATA | AF);
    machine
        .cpu
        .memory
        .write(OTHER, &2u64.to_le_bytes())
        .unwrap();
    machine.cpu.state.set_x(0, VA);
    machine.cpu.state.set_x(3, VA >> 12 | 9 << 48);
    machine.cpu.step().unwrap();
    machine
        .cpu
        .memory
        .write(entry, &(OTHER | AF | VALID_PAGE).to_le_bytes())
        .unwrap();
    assert!(matches!(machine.cpu.run(10), Stop::Hypercall { .. }));
    assert_eq!(machine.cpu.state.x(2), 2);
    // TLBI is for EL1, and the operations of later extensions do not exist.
    for (word, user) in [
        (asm::sys(op::TLBI_VMALLE1, 31), true),
        (asm::sys(0x0408, 31), false),                // vmalle1os
        (asm::sys(0x0431, 0), false),                 // rvae1
        (asm::sys(0x2438, 31), false),                // alle2
        (asm::sys(op::AT_S1E1R, 0) | 1 << 21, false), // sysl
    ] {
        let mut machine = Machine::new(&[word, HVC]);
        if user {
            machine.enter_el0();
        }
        machine.cpu.step().unwrap();
        assert_eq!(
            machine.cpu.taken.map(|taken| taken.cause),
            Some(Cause::Exception(Exception::Undefined)),
            "{word:08x}"
        );
    }
}

#[test]
fn the_cache_operations_do_nothing_and_el0_runs_some_with_uci() {
    // ic ivau, dc cvau, dc cvac, dc civac: EL0 with SCTLR_EL1.UCI.
    for operation in [0x1ba9u16, 0x1bd9, 0x1bd1, 0x1bf1] {
        let mut machine = Machine::new(&[asm::sys(operation, 0), HVC]);
        machine.cpu.state.set_x(0, 1 << 50);
        machine.cpu.step().unwrap();
        // No translation and no fault: the caches hold nothing.
        assert_eq!(machine.cpu.taken, None);
        machine.cpu.state.set_pc(CODE);
        machine.enter_el0();
        machine.cpu.step().unwrap();
        assert_eq!(
            machine.cpu.taken.map(|taken| taken.cause),
            Some(Cause::Exception(Exception::SystemTrap(sysreg::trap_iss(
                0x4000 | operation,
                0,
                true
            ))))
        );
        machine.cpu.state.system.regs[sysreg::reg::SCTLR_EL1] |= sysreg::sctlr::UCI;
        machine.cpu.state.set_pc(CODE);
        machine.enter_el0();
        machine.cpu.step().unwrap();
        assert_eq!(machine.cpu.taken, None);
    }
    // ic iallu, ic ialluis, dc ivac, dc isw, dc csw, dc cisw: EL1 only.
    for operation in [0x03a8u16, 0x0388, 0x03b1, 0x03b2, 0x03d2, 0x03f2] {
        let mut machine = Machine::new(&[asm::sys(operation, 0), HVC]);
        machine.cpu.step().unwrap();
        assert_eq!(machine.cpu.taken, None);
        machine.cpu.state.set_pc(CODE);
        machine.enter_el0();
        machine.cpu.step().unwrap();
        assert_eq!(
            machine.cpu.taken.map(|taken| taken.cause),
            Some(Cause::Exception(Exception::Undefined))
        );
    }
    let name = vmlab_hv_tcg_aarch64::exec::operation_name;
    assert_eq!(name(op::DC_ZVA), "DC ZVA");
    assert_eq!(name(op::TLBI_VAAE1), "TLBI VAAE1");
    assert_eq!(name(op::AT_S1E1R), "AT S1E1R");
    assert_eq!(name(0x03a8), "IC IALLU");
    assert_eq!(name(0x3fff), "S1_7_C15_C15_7");
}

#[test]
fn code_that_is_written_through_another_address_runs_as_written() {
    // The page of the data is mapped twice: for writing at VA and for
    // running at VA + 2 pages. add x1, x1, #1 ; ret.
    const ADD_1: u32 = 0x9100_0421;
    const ADD_2: u32 = 0x9100_0821;
    const RUN: u64 = VA + 0x2000;
    // blr x2 ; str w3, [x0] ; blr x2 ; hvc
    let mut machine = Machine::new(&[asm::blr(2), 0xb900_0003, asm::blr(2), HVC]);
    machine.map(VA, DATA | AF | NO_EXECUTE);
    machine.map(RUN, DATA | AF | READ_ONLY);
    machine
        .cpu
        .memory
        .write(DATA, &asm::bytes(&[ADD_1, asm::RET]))
        .unwrap();
    machine.cpu.state.set_x(0, VA);
    machine.cpu.state.set_x(2, RUN);
    machine.cpu.state.set_x(3, u64::from(ADD_2));
    assert!(matches!(machine.cpu.run(100), Stop::Hypercall { .. }));
    assert_eq!(machine.cpu.taken, None);
    // One from the first call, two from the second.
    assert_eq!(machine.cpu.state.x(1), 3);
    // The same when every instruction is stepped.
    let mut machine = Machine::new(&[asm::blr(2), 0xb900_0003, asm::blr(2), HVC]);
    machine.map(VA, DATA | AF | NO_EXECUTE);
    machine.map(RUN, DATA | AF | READ_ONLY);
    machine
        .cpu
        .memory
        .write(DATA, &asm::bytes(&[ADD_1, asm::RET]))
        .unwrap();
    machine.cpu.state.set_x(0, VA);
    machine.cpu.state.set_x(2, RUN);
    machine.cpu.state.set_x(3, u64::from(ADD_2));
    for _ in 0..7 {
        machine.cpu.step().unwrap();
    }
    assert_eq!(machine.cpu.state.x(1), 3);
}

#[test]
fn an_address_that_maps_other_code_runs_the_other_code() {
    // The address RUN maps one page of code, then the other, with a TLBI
    // between. A block is kept under the physical address of its code too.
    const RUN: u64 = VA + 0x2000;
    // blr x2 ; str x3, [x0] ; tlbi vmalle1 ; blr x2 ; hvc
    let program = [
        asm::blr(2),
        str(3, 0),
        asm::sys(op::TLBI_VMALLE1, 31),
        asm::blr(2),
        HVC,
    ];
    let mut machine = Machine::new(&program);
    let entry = machine.map(RUN, DATA | AF | READ_ONLY);
    // The tables are mapped at their own addresses, for EL1.
    for page in 0..TABLE_PAGES {
        machine.map(
            ROOT + page * 0x1000,
            (ROOT + page * 0x1000) | AF | NO_EXECUTE,
        );
    }
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
    // add x1, x1, #1 from the first page, add x1, x1, #5 from the second.
    assert_eq!(machine.cpu.state.x(1), 6);
}

#[test]
fn the_guest_takes_the_abort_and_returns_to_the_access() {
    // The handler makes the descriptor valid and returns: the store runs
    // again. str x1, [x0] ; hvc. The handler: str x3, [x2] ; eret.
    let mut machine = Machine::new(&[str(1, 0), HVC]);
    let entry = machine.map(VA, DATA | NO_EXECUTE);
    for page in 0..TABLE_PAGES {
        machine.map(
            ROOT + page * 0x1000,
            (ROOT + page * 0x1000) | AF | NO_EXECUTE,
        );
    }
    machine
        .cpu
        .memory
        .write(VECTORS + 0x200, &asm::bytes(&[str(3, 2), asm::ERET]))
        .unwrap();
    machine.cpu.state.set_x(0, VA + 8);
    machine.cpu.state.set_x(1, 0x55);
    machine.cpu.state.set_x(2, entry);
    machine
        .cpu
        .state
        .set_x(3, DATA | AF | NO_EXECUTE | VALID_PAGE);
    assert!(matches!(machine.cpu.run(100), Stop::Hypercall { .. }));
    assert_eq!(machine.cpu.exceptions, 1);
    assert_eq!(machine.word(DATA + 8), 0x55);
    // An access flag fault is not kept by the TLB: no TLBI was needed.
    assert_eq!(
        machine.cpu.state.system.regs[sysreg::reg::ESR_EL1],
        0x9600_004b
    );
}

#[test]
fn an_inner_shareable_tlbi_reaches_the_other_cpus() {
    use std::sync::Arc;
    use vmlab_hv_tcg_aarch64::shootdown::Shootdown;
    // The second CPU keeps a translation of VA in its TLB, and its
    // descriptor changes. The first CPU runs `tlbi <op>, x3`: the forms of
    // CRm 3 drop the entry of the second CPU too, those of CRm 7 do not.
    for (operation, argument, flushed) in [
        (0x0418u16, 0u64, true),
        (0x0419, VA >> 12 | 1 << 48, true),
        (0x041a, 1 << 48, true),
        (0x041b, VA >> 12, true),
        (0x041a, 2 << 48, false),
        (op::TLBI_VMALLE1, 0, false),
    ] {
        let shootdown = Arc::new(Shootdown::new(2));
        let mut first = Machine::new(&[asm::sys(operation, 3), HVC]);
        first.cpu.shootdown = Some((shootdown.clone(), 0));
        first.cpu.state.set_x(3, argument);
        let mut second = Machine::new(&[ldr(1, 0), HVC]);
        second.cpu.shootdown = Some((shootdown, 1));
        let entry = second.map(VA, DATA | AF | NOT_GLOBAL);
        second.cpu.memory.write(DATA, &1u64.to_le_bytes()).unwrap();
        second.cpu.memory.write(OTHER, &2u64.to_le_bytes()).unwrap();
        second.cpu.state.set_x(0, VA);
        assert!(matches!(second.cpu.run(10), Stop::Hypercall { .. }));
        assert_eq!(second.cpu.state.x(1), 1);
        second
            .cpu
            .memory
            .write(entry, &(OTHER | AF | NOT_GLOBAL | VALID_PAGE).to_le_bytes())
            .unwrap();
        // The second CPU is outside its run loop: the first does not wait.
        assert!(matches!(first.cpu.run(10), Stop::Hypercall { .. }));
        assert_eq!(first.cpu.taken, None);
        second.cpu.state.set_pc(CODE);
        assert!(matches!(second.cpu.run(10), Stop::Hypercall { .. }));
        assert_eq!(
            second.cpu.state.x(1),
            if flushed { 2 } else { 1 },
            "{operation:#x} {argument:#x}"
        );
    }
}

#[test]
fn a_cpu_waits_until_the_running_cpus_took_its_tlbi() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use vmlab_hv_tcg_aarch64::shootdown::Shootdown;
    // The second CPU spins in a loop of one branch on its own thread. The
    // first runs `tlbi vmalle1is` and comes back only after the second
    // took it, whose run loop then ends at its budget.
    let shootdown = Arc::new(Shootdown::new(2));
    let mut first = Machine::new(&[asm::sys(0x0418, 3), HVC]);
    first.cpu.shootdown = Some((shootdown.clone(), 0));
    let mut second = Machine::new(&[0x1400_0000]);
    second.cpu.shootdown = Some((shootdown, 1));
    let done = Arc::new(AtomicBool::new(false));
    let spinner = {
        let done = done.clone();
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                assert_eq!(second.cpu.run(1000), Stop::Budget);
            }
        })
    };
    assert!(matches!(first.cpu.run(10), Stop::Hypercall { .. }));
    done.store(true, Ordering::SeqCst);
    spinner.join().unwrap();
}
