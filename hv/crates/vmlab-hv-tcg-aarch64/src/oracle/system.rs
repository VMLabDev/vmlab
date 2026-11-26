//! The stream of system instructions (card C6, DESIGN.md O3): an image with
//! a vector table and a run of instructions that take exceptions, change the
//! exception level and return, and read and write the system registers.
//!
//! The stream leaves out what the lockstep cannot follow: `WFI` and `WFE`,
//! which stop QEMU for ever without an interrupt or an event (`directed`
//! runs both at EL0, where SCTLR_EL1 traps them), and the values of a few
//! registers that would turn on what later cards bring (the MMU and tagged
//! addresses, card C8) or what phase 3d does not have (debug exceptions,
//! V3). `safe_value` holds those limits.

use super::asm::{self, reg};
use super::stream::{Generator, Rng, records_of};
use crate::generated::decode::{FormatClass, Record};
use crate::lift::{FP_FORMATS, LIFTED_FORMATS};
use crate::sysreg::{self, Kind, ROWS, Rw, Special};

/// The vector table takes the first 0x800 bytes of an image.
pub const TABLE_BYTES: u64 = 0x800;

/// The immediate of the `SVC` that brings a stream back from EL0 to EL1.
pub const EXIT_CALL: u16 = 0xe1;

/// The registers that QEMU's `cortex-a53` has and our table does not
/// (`sysreg::QEMU_DIFFERENCES`): the stream must not name them.
const QEMU_ONLY: [u16; 9] = [
    0xc602, 0xcd82, 0xcd83, 0xcf80, 0xcf90, 0xcf91, 0xcf92, 0xcf93, 0xcf98,
];

/// `AND Xd, Xn, #imm` for the two masks that the handlers need.
fn and_not_il(rd: u32) -> u32 {
    // All ones but bit 20: 63 ones, rotated right by 43.
    0x9240_0000 | 43 << 16 | 62 << 10 | rd << 5 | rd
}

fn and_aligned(rd: u32) -> u32 {
    // All ones but the bits 1 and 0: 62 ones, rotated right by 62.
    0x9240_0000 | 62 << 16 | 61 << 10 | rd << 5 | rd
}

/// The end of every handler: clear the saved IL, so that the return is to a
/// legal state again, and return to the instruction after the one that
/// faulted, at an address that is a multiple of four.
fn skip_and_return() -> Vec<u32> {
    vec![
        asm::mrs(16, reg::SPSR_EL1),
        and_not_il(16),
        asm::msr(reg::SPSR_EL1, 16),
        asm::mrs(17, reg::ELR_EL1),
        asm::add(17, 17, 4),
        and_aligned(17),
        asm::msr(reg::ELR_EL1, 17),
        asm::ERET,
    ]
}

/// The vector table of a stream: sixteen entries of 0x80 bytes. An exception
/// from EL0 that is the exit call returns to EL1 with `exit_spsr`. Every
/// other entry skips the instruction and returns to where it came from.
pub fn vector_table(exit_spsr: u16) -> Vec<u32> {
    let mut words = Vec::new();
    for entry in 0..16 {
        let mut handler = Vec::new();
        // The synchronous entry from a lower level in AArch64.
        if entry == 8 {
            handler.extend([
                asm::mrs(16, reg::ESR_EL1),
                asm::movz(17, EXIT_CALL, 0),
                asm::movk(17, 0x5600, 1),
                asm::eor(16, 16, 17),
                asm::cbnz(16, 4),
                asm::movz(16, exit_spsr, 0),
                asm::msr(reg::SPSR_EL1, 16),
                asm::ERET,
            ]);
        }
        handler.extend(skip_and_return());
        words.extend(&handler);
        words.resize(words.len() + 32 - handler.len(), asm::NOP);
    }
    words
}

/// A saved PSTATE for an exception return: mostly EL0, some EL1, and some
/// that EL1 cannot return to. Never AArch32 at EL0, which QEMU's CPU has and
/// ours does not (G2).
pub fn spsr(rng: &mut Rng) -> u64 {
    let flags = rng.below(16) << 28;
    let masks = rng.below(16) << 6;
    let il = u64::from(rng.chance(1, 10)) << 20;
    let mode = match rng.below(20) {
        0..=11 => 0b00000,
        12 | 13 => 0b00101,
        14 => 0b00100,
        // EL2 and EL3, the reserved modes, EL0 with its own stack pointer
        // selected, and the AArch32 modes above User.
        15 | 16 => [0b01000, 0b01001, 0b01100, 0b01101][rng.below(4) as usize],
        17 => [0b00001, 0b00010, 0b00011, 0b00110, 0b00111][rng.below(5) as usize],
        18 => 0b10000 | (1 + rng.below(15)),
        // Any bits, to see which of them PSTATE keeps.
        _ => {
            let any = rng.next_u64();
            let mode = any & 0x1f;
            return if mode == 0b10000 { any & !0x10 } else { any };
        }
    };
    flags | masks | il | mode
}

/// A value for a write to the row, inside what the lockstep can follow.
/// `image`: the first address and the length of the stream. `None`: the
/// stream does not write the register.
pub fn safe_value(row: usize, rng: &mut Rng, table: u64, image: (u64, u64)) -> Option<u64> {
    let any = match rng.below(8) {
        0 => 0,
        1 => u64::MAX,
        2 => rng.below(16),
        _ => rng.next_u64(),
    };
    let info = &ROWS[row];
    if matches!(info.kind, Kind::Absent(_)) {
        return None;
    }
    Some(match info.name {
        // No MMU before card C8.
        "SCTLR_EL1" => any & !1,
        // No tagged addresses before card C8: TBI0 and TBI1.
        "TCR_EL1" => any & !(3 << 37),
        // The one table of the image. The low bits are not stored.
        "VBAR_EL1" => table | (any & 0x1f),
        // No software step and no debug exceptions (V3): SS, KDE and MDE.
        "MDSCR_EL1" => any & !(1 | 1 << 13 | 1 << 15),
        "SPSR_EL1" => spsr(rng),
        "ELR_EL1" if rng.chance(9, 10) => image.0 + 4 * rng.below(image.1 / 4),
        // A compare value far below or far above every count that the round
        // sees, so that the status of the timer does not depend on the time.
        "CNTV_CVAL_EL0" | "CNTP_CVAL_EL0" => {
            if rng.chance(1, 2) {
                any & 0xffff
            } else {
                any | 1 << 63
            }
        }
        // A write of a timer value takes the count of that moment.
        "CNTV_TVAL_EL0" | "CNTP_TVAL_EL0" => return None,
        _ => any,
    })
}

/// Whether the instruction that QEMU is about to run takes the round out of
/// what the lockstep follows. `safe_value` keeps the values of the stream
/// inside, but a branch or an exception return can land between the
/// instructions that load a value and the `MSR` that writes it, and the
/// write then takes what the register held. `x`: X0 to X30 before the
/// instruction. `state`: our state, which is QEMU's. The text says why.
pub fn unfollowed(encoding: u32, x: &[u64; 31], state: &crate::state::State) -> Option<&'static str> {
    let el0 = state.context.el == 0;
    let sctlr = state.system.regs[sysreg::reg::SCTLR_EL1];
    match encoding {
        0xd503_207f if !el0 || sctlr & sysreg::sctlr::NTWI != 0 => {
            return Some("WFI waits for an interrupt that no round has");
        }
        0xd503_205f if !el0 || sctlr & sysreg::sctlr::NTWE != 0 => {
            return Some("WFE waits for an event that no round has");
        }
        // AArch32 at EL0, which QEMU's CPU has and ours does not (G2).
        asm::ERET if !el0 && state.system.regs[sysreg::reg::SPSR_EL1] & 0x1f == 0x10 => {
            return Some("a return to AArch32 at EL0");
        }
        _ => {}
    }
    // `MSR <register>, Xt` at EL1.
    if encoding & 0xffe0_0000 != 0xd500_0000 || el0 {
        return None;
    }
    let value = match encoding & 31 {
        31 => 0,
        rt => x[rt as usize],
    };
    let row = sysreg::find((encoding >> 5) as u16)?;
    match ROWS[row].name {
        "SCTLR_EL1" if value & 1 != 0 => Some("the MMU is card C8"),
        "TCR_EL1" if value & (3 << 37) != 0 => Some("tagged addresses are card C8"),
        "MDSCR_EL1" if value & (1 | 1 << 13 | 1 << 15) != 0 => {
            Some("software step and debug exceptions are not in phase 3d (V3)")
        }
        "CNTV_CVAL_EL0" | "CNTP_CVAL_EL0" if (1 << 16..1 << 63).contains(&value) => {
            Some("a timer whose status depends on the time of the host")
        }
        "CNTV_TVAL_EL0" | "CNTP_TVAL_EL0" => Some("a timer value takes the count of the host"),
        _ => None,
    }
}

/// Whether both sides agree about the encoding: we have a row that is not
/// left out, or neither side has the register.
pub fn agreed(encoding: u16) -> bool {
    match sysreg::find(encoding) {
        Some(row) => !matches!(ROWS[row].kind, Kind::Absent(_)),
        None => !QEMU_ONLY.contains(&encoding),
    }
}

/// The classes that a system stream is counted by.
pub const STREAM_FORMATS: [FormatClass; 9] = {
    use FormatClass::*;
    [
        ExceptionGeneration,
        UDFType,
        SpecialReturn,
        HintI,
        SystemNoOperands,
        CRmSystemI,
        MRSI,
        MSRI,
        MSRpstateImm0_15,
    ]
};

impl Generator {
    fn any_record(&mut self, format: FormatClass) -> Record {
        let records = records_of(format);
        records[self.rng.below(records.len() as u64) as usize]
    }

    /// A register that both sides know or both do not, and a random Rt.
    fn register(&mut self) -> (u16, u32) {
        let rt = self.rng.below(32) as u32;
        loop {
            let encoding = if self.rng.chance(6, 7) {
                ROWS[self.rng.below(ROWS.len() as u64) as usize].encoding
            } else {
                0x8000 | self.rng.below(0x8000) as u16
            };
            if agreed(encoding) {
                return (encoding, rt);
            }
        }
    }

    /// Append one item of a system stream at the address `at`.
    fn system_item(&mut self, words: &mut Vec<u32>, at: u64, table: u64, image: (u64, u64)) {
        match self.rng.below(100) {
            // An instruction of the earlier cards: integer, branch and FP.
            0..=12 => {
                let format = if self.rng.chance(1, 4) {
                    FP_FORMATS[self.rng.below(FP_FORMATS.len() as u64) as usize]
                } else {
                    LIFTED_FORMATS[self.rng.below(LIFTED_FORMATS.len() as u64) as usize]
                };
                // A register branch would leave the image most of the time.
                if format != FormatClass::BranchReg {
                    let record = self.any_record(format);
                    words.push(self.instruction(record, at, image).0);
                }
            }
            13..=27 => {
                let (encoding, rt) = self.register();
                words.push(asm::mrs(rt, encoding));
            }
            28..=42 => {
                let (encoding, rt) = self.register();
                let value = match sysreg::find(encoding) {
                    Some(row) => safe_value(row, &mut self.rng, table, image),
                    // Neither side has the register: any value.
                    None => Some(self.rng.next_u64()),
                };
                let Some(value) = value else {
                    return;
                };
                let hazard = sysreg::find(encoding).is_some_and(|row| {
                    matches!(
                        ROWS[row].name,
                        "SCTLR_EL1"
                            | "TCR_EL1"
                            | "VBAR_EL1"
                            | "MDSCR_EL1"
                            | "SPSR_EL1"
                            | "ELR_EL1"
                            | "CNTV_CVAL_EL0"
                            | "CNTP_CVAL_EL0"
                    )
                });
                // Most writes take the value from the stream. Some take what
                // a register holds, where any value can be followed.
                if hazard || rt == 31 || self.rng.chance(3, 4) {
                    let rt = if rt == 31 { 16 } else { rt };
                    words.extend(asm::load(rt, value));
                    words.push(asm::msr(encoding, rt));
                } else {
                    words.push(asm::msr(encoding, rt));
                }
            }
            // SPSel, DAIFSet and DAIFClr with any immediate.
            43..=50 => {
                let field = [0xd500_40bfu32, 0xd503_40df, 0xd503_40ff][self.rng.below(3) as usize];
                words.push(field | (self.rng.below(16) as u32) << 8);
            }
            // The hint space but WFE and WFI, and the hints with their own
            // record.
            51..=58 => {
                let hint = match self.rng.below(127) as u32 {
                    2 | 3 => 0,
                    other => other,
                };
                words.push(0xd503_201f | hint << 5);
            }
            // The hints with a record of their own: all NOPs on this CPU.
            59..=68 => {
                let record = self.any_record(FormatClass::SystemNoOperands);
                words.push(record.info().value);
            }
            69..=76 => {
                let record = self.any_record(FormatClass::CRmSystemI);
                words.push(self.instruction(record, at, image).0);
            }
            77..=80 => words.push(self.rng.below(1 << 16) as u32), // udf
            81..=88 => {
                let immediate = self.rng.below(1 << 16) as u32;
                let word = match self.rng.below(12) {
                    0..=2 => asm::svc(immediate as u16),
                    3 | 7 => 0xd420_0000 | immediate << 5, // brk
                    4 => 0xd440_0000 | immediate << 5, // hlt
                    5 => 0xd4a0_0001 | immediate << 5, // dcps1
                    6 => 0xd4a0_0002 | immediate << 5, // dcps2
                    8 => 0xd6bf_03e0,                  // drps
                    9 => 0xd400_0003 | immediate << 5, // smc
                    10 => asm::ERET,
                    // A call that is no PSCI function: X0 is below 2^16.
                    _ => {
                        words.push(asm::movz(0, self.rng.below(1 << 16) as u16, 0));
                        asm::hvc(immediate as u16)
                    }
                };
                words.push(word);
            }
            // A return to a saved state that the stream makes: to EL0 most
            // of the time. The stream goes on after the ERET.
            89..=93 => {
                let saved = spsr(&mut self.rng);
                words.extend(asm::load(16, saved));
                words.push(asm::msr(reg::SPSR_EL1, 16));
                let here = at + 4 * 5;
                let adr = self.adr(16, here, here + 12);
                words.push(adr.unwrap_or(asm::NOP));
                words.push(asm::msr(reg::ELR_EL1, 16));
                words.push(asm::ERET);
            }
            // Back to EL1. The handler returns to the instruction after the
            // call, and from EL1 the call skips one.
            94..=97 => {
                words.push(asm::svc(EXIT_CALL));
                words.push(asm::NOP);
            }
            // A PC that is not a multiple of four.
            _ => {
                let target = image.0 + 4 * self.rng.below(image.1 / 4);
                if let Some(adr) = self.adr(16, at, target) {
                    words.push(adr);
                    words.push(asm::add(16, 16, 1 + self.rng.below(3) as u32));
                    words.push(0xd61f_0200); // br x16
                }
            }
        }
    }

    /// An image for a round of system instructions at `base`: the vector
    /// table, a prelude that points VBAR_EL1 at it, and about `count`
    /// instructions. The round starts at `base + TABLE_BYTES`.
    pub fn system_image(&mut self, base: u64, count: usize) -> Vec<u32> {
        // Back to EL1 with either stack pointer, masked or not.
        let exit_spsr = [0x3c5, 0x3c4, 0x005, 0x145][self.rng.below(4) as usize];
        let mut words = vector_table(exit_spsr);
        let start = words.len();
        words.extend(asm::load(16, base));
        words.push(asm::msr(reg::VBAR_EL1, 16));
        let first = base + 4 * words.len() as u64;
        let image = (first, 4 * count as u64);
        while words.len() - start < count {
            let at = base + 4 * words.len() as u64;
            self.system_item(&mut words, at, base, image);
        }
        // The stream ends in a call that both sides refuse or serve alike,
        // and a branch to itself: a round that gets here ends as a loop.
        words.push(asm::NOP);
        words.push(asm::b(0));
        words
    }
}

/// Rounds that the random streams cannot hold, as the words after the
/// prelude of `system_image` and the words that the round must verify.
pub fn directed() -> Vec<(Vec<u32>, Vec<u32>)> {
    const WFE: u32 = 0xd503_205f;
    const WFI: u32 = 0xd503_207f;
    // SCTLR_EL1 after the reset without nTWI and nTWE, then EL0 with SP_EL0:
    // both waits trap to EL1, and the handler skips them.
    let mut waits = Vec::new();
    waits.extend(asm::load(16, 0x00c0_0838));
    waits.push(asm::msr(reg::SCTLR_EL1, 16));
    waits.extend(asm::load(16, 0x3c0));
    waits.push(asm::msr(reg::SPSR_EL1, 16));
    // adr x16, .+12
    waits.push(0x1000_0070);
    waits.push(asm::msr(reg::ELR_EL1, 16));
    waits.push(asm::ERET);
    waits.extend([WFI, asm::NOP, WFE, asm::NOP, asm::svc(EXIT_CALL), asm::NOP]);
    vec![(waits, vec![WFI, WFE])]
}

/// Whether the special register has no value that a round can be replayed
/// from: the counters and the timer values.
pub fn reads_the_time(row: usize) -> bool {
    matches!(
        ROWS[row].kind,
        Kind::Special(Special::Counter | Special::TimerValue(_))
    )
}

/// Whether the stream can write the row at EL1.
pub fn writable(row: usize) -> bool {
    matches!(ROWS[row].el1, Rw::Rw | Rw::Wo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exception::{Cause, Exception};
    use crate::exec::{Cpu, Stop};
    use crate::fields::logical_immediate;
    use crate::generated::decode::decode;
    use crate::oracle::follow::ROUND_COUNT;
    use crate::state::State;
    use crate::timer::Clock;
    use std::collections::BTreeSet;
    use vmlab_hv_tcg::FlatMemory;

    const BASE: u64 = 0x4200_0000;

    #[test]
    fn the_masks_of_the_handlers_are_the_logical_immediates_that_they_claim() {
        for (word, mask) in [(and_not_il(16), !(1u64 << 20)), (and_aligned(17), !3u64)] {
            let decoded = decode(word).unwrap();
            assert_eq!(decoded.record, Record::ANDXri);
            let field = crate::field(Record::ANDXri, "imm", word).unwrap();
            assert_eq!(logical_immediate(field, 64), Some(mask));
        }
        // Every entry of the table is 32 words, and each word decodes.
        let table = vector_table(0x3c5);
        assert_eq!(table.len() as u64 * 4, TABLE_BYTES);
        for word in &table {
            assert!(decode(*word).is_some(), "{word:08x}");
        }
        // The handler of EL0 compares the syndrome with that of the exit call.
        assert_eq!(table[8 * 32 + 1], asm::movz(17, EXIT_CALL, 0));
        assert_eq!(table[8 * 32 + 4], asm::cbnz(16, 4));
        assert_eq!(table[8 * 32 + 8], asm::mrs(16, reg::SPSR_EL1));
    }

    #[test]
    fn a_saved_state_is_never_aarch32_at_el0_and_covers_every_kind() {
        let mut rng = Rng(11);
        let (mut el0, mut el1, mut illegal, mut il) = (0, 0, 0, 0);
        for _ in 0..4000 {
            let value = spsr(&mut rng);
            assert_ne!(value & 0x1f, 0b10000, "{value:#x}");
            if crate::exception::return_is_legal(value, 1) {
                if value & 0xc == 0 {
                    el0 += 1;
                } else {
                    el1 += 1;
                }
            } else {
                illegal += 1;
            }
            il += u32::from(value & (1 << 20) != 0);
        }
        assert!(el0 > 2000 && el1 > 400 && illegal > 400 && il > 200);
    }

    #[test]
    fn the_values_of_a_stream_stay_inside_what_the_lockstep_follows() {
        let mut rng = Rng(5);
        let image = (BASE + 0x900, 0x400);
        for _ in 0..2000 {
            let value = |name: &str, rng: &mut Rng| {
                safe_value(sysreg::index(name), rng, BASE, image).unwrap()
            };
            assert_eq!(value("SCTLR_EL1", &mut rng) & 1, 0);
            assert_eq!(value("TCR_EL1", &mut rng) & (3 << 37), 0);
            assert_eq!(value("VBAR_EL1", &mut rng) & !0x1f, BASE);
            assert_eq!(value("MDSCR_EL1", &mut rng) & 0xa001, 0);
            let compare = value("CNTV_CVAL_EL0", &mut rng);
            assert!(!(1 << 16..1 << 63).contains(&compare));
            assert!(!(ROUND_COUNT / 2..=ROUND_COUNT * 2).contains(&compare));
        }
        assert_eq!(
            safe_value(sysreg::index("CNTV_TVAL_EL0"), &mut rng, BASE, image),
            None
        );
        assert_eq!(
            safe_value(sysreg::index("PMCR_EL0"), &mut rng, BASE, image),
            None
        );
        // The registers that only QEMU has are not named.
        assert!(!agreed(0xc602) && !agreed(0xcf98));
        assert!(!agreed(ROWS[sysreg::index("PMCR_EL0")].encoding));
        assert!(agreed(0xc000) && agreed(0xffff) && agreed(0xc01b));
        for difference in sysreg::QEMU_DIFFERENCES {
            let known = sysreg::position(difference.name) < sysreg::COUNT;
            let listed = QEMU_ONLY
                .iter()
                .any(|encoding| sysreg::name(*encoding) == difference.name);
            assert!(known || listed, "{}", difference.name);
        }
        // What a round does not follow, whatever the register held.
        let state = State::reset(0);
        let mut x = [0u64; 31];
        x[3] = 1;
        let msr = |name: &str| asm::msr(ROWS[sysreg::index(name)].encoding, 3);
        assert!(unfollowed(msr("SCTLR_EL1"), &x, &state).is_some());
        assert!(unfollowed(msr("MDSCR_EL1"), &x, &state).is_some());
        assert!(unfollowed(msr("CNTV_TVAL_EL0"), &x, &state).is_some());
        assert!(unfollowed(msr("CNTP_CVAL_EL0"), &x, &state).is_none());
        assert!(unfollowed(msr("TCR_EL1"), &x, &state).is_none());
        assert!(unfollowed(msr("TPIDR_EL1"), &x, &state).is_none());
        x[3] = 1 << 37;
        assert!(unfollowed(msr("TCR_EL1"), &x, &state).is_some());
        assert!(unfollowed(msr("CNTP_CVAL_EL0"), &x, &state).is_some());
        assert!(unfollowed(msr("SCTLR_EL1"), &x, &state).is_none());
        assert!(unfollowed(msr("SCTLR_EL1") | 31, &x, &state).is_none());
        assert!(unfollowed(0xd503_207f, &x, &state).is_some());
        assert!(unfollowed(asm::ERET, &x, &state).is_none());
        let mut user = State::reset(0);
        assert!(user.set_cpsr(0));
        // At EL0 the write is undefined, and the waits trap unless SCTLR_EL1
        // lets them through, as it does after the reset.
        x[3] = 1;
        assert!(unfollowed(msr("SCTLR_EL1"), &x, &user).is_none());
        assert!(unfollowed(0xd503_205f, &x, &user).is_some());
        user.system.regs[sysreg::reg::SCTLR_EL1] = 0;
        assert!(unfollowed(0xd503_205f, &x, &user).is_none());
        let mut aarch32 = State::reset(0);
        aarch32.system.regs[sysreg::reg::SPSR_EL1] = 0x10;
        assert!(unfollowed(asm::ERET, &x, &aarch32).is_some());
        assert!(reads_the_time(sysreg::index("CNTVCT_EL0")));
        assert!(!reads_the_time(sysreg::index("CNTV_CTL_EL0")));
        assert!(writable(sysreg::index("OSLAR_EL1")) && !writable(sysreg::index("MIDR_EL1")));
    }

    #[test]
    fn the_directed_round_traps_both_waits_at_el0() {
        let (body, must) = directed().remove(0);
        let mut words = vector_table(0x3c5);
        words.extend(asm::load(16, BASE));
        words.push(asm::msr(reg::VBAR_EL1, 16));
        words.extend(&body);
        words.push(asm::b(0));
        let mut data = vec![0u8; 0x2000];
        data[..words.len() * 4].copy_from_slice(&asm::bytes(&words));
        let state = State::reset_as(BASE + TABLE_BYTES, &sysreg::QEMU_CORTEX_A53);
        let mut cpu = Cpu::new(FlatMemory::new(BASE, data), state);
        let mut syndromes = Vec::new();
        for _ in 0..60 {
            cpu.step().unwrap();
            if cpu.taken.is_some() {
                syndromes.push(cpu.state.system.regs[sysreg::reg::ESR_EL1]);
            }
        }
        // WFI, WFE, and the call that brings the round back to EL1.
        assert_eq!(syndromes, vec![0x07e0_0000, 0x07e0_0001, 0x5600_00e1]);
        assert_eq!(cpu.state.context.el, 1);
        assert_eq!(must, vec![0xd503_207f, 0xd503_205f]);
    }

    /// Run streams on our interpreter alone. A call leaves the core, and the
    /// test refuses it, as QEMU's machine does for a call that is no PSCI
    /// function.
    #[test]
    fn a_stream_runs_on_our_interpreter_through_exceptions_levels_and_returns() {
        let mut classes = BTreeSet::new();
        let mut causes = std::collections::HashSet::new();
        let (mut at_el0, mut returns, mut exceptions, mut unknown) = (0u64, 0u64, 0u64, 0u64);
        for seed in 0..40 {
            let mut generator = Generator::new(seed);
            let words = generator.system_image(BASE, 600);
            for word in &words {
                if let Some(decoded) = decode(*word) {
                    classes.insert(decoded.record.info().format);
                }
                // No WFI and no WFE: they would stop the leader.
                assert!(*word != 0xd503_207f && *word != 0xd503_205f);
            }
            let mut data = vec![0u8; 0x8000];
            data[..words.len() * 4].copy_from_slice(&asm::bytes(&words));
            let mut state = State::reset_as(BASE + TABLE_BYTES, &sysreg::QEMU_CORTEX_A53);
            let (x, sp, nzcv) = generator.registers((BASE + TABLE_BYTES, 2400));
            for (n, value) in x.iter().enumerate() {
                state.set_x(n, *value);
            }
            state.set_sp(sp);
            state.set_nzcv(nzcv);
            let mut cpu = Cpu::new(FlatMemory::new(BASE, data), state);
            cpu.clock = Clock::Manual(ROUND_COUNT);
            for _ in 0..3000 {
                let level = cpu.state.context.el;
                match cpu.step() {
                    Ok(()) => {}
                    Err(Stop::Hypercall { pc, .. }) => cpu.refuse_hypercall(pc),
                    Err(stop) => panic!("seed {seed}: {stop:?} at {:#x}", cpu.state.pc()),
                }
                at_el0 += u64::from(cpu.state.context.el == 0);
                returns += u64::from(level == 1 && cpu.state.context.el == 0);
                if let Some(taken) = cpu.taken {
                    exceptions += 1;
                    if let Cause::Exception(exception) = taken.cause {
                        causes.insert(std::mem::discriminant(&exception));
                    }
                }
                // No interrupt line is high, and no timer raises one that
                // the stream could see.
                assert!(!cpu.irq && !cpu.fiq);
            }
            unknown += cpu.unknown.len() as u64;
        }
        for format in STREAM_FORMATS {
            assert!(classes.contains(&format), "{}", format.name());
        }
        for exception in [
            Exception::Undefined,
            Exception::FpAccess,
            Exception::IllegalState,
            Exception::SupervisorCall(0),
            Exception::SystemTrap(0),
            Exception::PcAlignment,
            Exception::Breakpoint(0),
        ] {
            assert!(
                causes.contains(&std::mem::discriminant(&exception)),
                "{exception:?}"
            );
        }
        assert!(at_el0 > 5000, "{at_el0}");
        assert!(returns > 100, "{returns}");
        assert!(exceptions > 5000, "{exceptions}");
        assert!(unknown > 40, "{unknown}");
    }
}
