//! Data processing with registers: the nineteen format classes
//! `BaseAddSubSReg`, `BaseAddSubEReg`, `BaseAddSubEReg64`, `BaseAddSubCarry`,
//! `BaseAddSubCarrySetFlags`, `BaseLogicalSReg`, `BaseCondComparisonImm`,
//! `BaseCondComparisonReg`, `BaseCondSelect`, `BaseCondSelectOp`, `BaseShift`,
//! `BaseDiv`, `BaseMulAccum`, `WideMulAccum`, `MulHi`, `BaseOneOperandData`,
//! `OneWRegData`, `OneXRegData` and `BaseCRC32` (101 records).
//!
//! Sources read (DESIGN.md "Sources"): S1, the LLVM AArch64 tables for the
//! records, their fields and their operand classes (which register 31 is the
//! stack pointer and which the zero register, and the layout of the shift and
//! extend fields). S6, the lockstep against QEMU, for the behaviour. No text
//! of S2 to S4 was opened for this file. The two CRC-32 polynomials are the
//! published ones of IEEE 802.3 and of Castagnoli. Not read: QEMU's source,
//! Arm's ASL or pseudocode (X1, X2).
//!
//! Everything here is pure IR (G3d), CRC32 included: the helper call of R10
//! does not exist before card C4.

use super::{Builder, Flags, Lifted};
use crate::fields::{Condition, ones};
use crate::generated::decode::{
    BaseAddSubCarry, BaseAddSubCarrySetFlags, BaseAddSubEReg, BaseAddSubEReg64, BaseAddSubSReg,
    BaseCRC32, BaseCondComparisonImm, BaseCondComparisonReg, BaseCondSelect, BaseCondSelectOp,
    BaseDiv, BaseLogicalSReg, BaseMulAccum, BaseOneOperandData, BaseShift, MulHi, OneWRegData,
    OneXRegData, Record, WideMulAccum,
};
use crate::state::slot;
use vmlab_hv_tcg_ir::native::{Binary, ScalarId, Unary};

// --- operands -----------------------------------------------------------------

/// Register `rm` shifted by a constant. The raw field is `kind:amount` with
/// two bits of kind (LSL, LSR, ASR, ROR) and six of amount. `None`: the
/// amount does not fit the register.
fn shifted_register(b: &mut Builder, width: u16, rm: u32, field: u32) -> Option<ScalarId> {
    let amount = field & 0x3f;
    if amount >= u32::from(width) {
        return None;
    }
    let value = b.x(rm, width);
    Some(match (field >> 6) & 3 {
        0 => b.shl(width, value, amount),
        1 => b.lsr(width, value, amount),
        2 => b.asr(width, value, amount),
        _ => b.ror(width, value, amount),
    })
}

/// Register `rm` extended and shifted left. `option` picks the low 8, 16, 32
/// or 64 bits and whether their sign is kept (bit 2). `shift` is 0 to 4.
fn extended_register(b: &mut Builder, width: u16, rm: u32, option: u32, shift: u32) -> ScalarId {
    let from = (8u16 << (option & 3)).min(width);
    let source = b.x(rm, width);
    let extended = if option & 4 != 0 {
        b.sign_extend(source, from, width)
    } else {
        let low = b.low(source, from);
        b.zero_extend(low, width)
    };
    b.shl(width, extended, shift)
}

/// `a + b` or `a - b` with the flags of the result.
fn add_or_subtract(
    b: &mut Builder,
    width: u16,
    subtract: bool,
    left: ScalarId,
    right: ScalarId,
) -> (ScalarId, Flags) {
    if subtract {
        let inverted = b.not(width, right);
        let carry = b.imm(1, 1);
        b.add_with_carry(width, left, inverted, carry)
    } else {
        let carry = b.imm(1, 0);
        b.add_with_carry(width, left, right, carry)
    }
}

// --- add and subtract -----------------------------------------------------------

/// `ADD`, `ADDS`, `SUB` and `SUBS` with a shifted register. Register 31 is
/// the zero register in every place.
pub fn add_sub_shifted(b: &mut Builder, record: Record, f: &BaseAddSubSReg) -> Lifted {
    let (width, subtract, set_flags) = match record {
        Record::ADDWrs => (32, false, false),
        Record::ADDXrs => (64, false, false),
        Record::ADDSWrs => (32, false, true),
        Record::ADDSXrs => (64, false, true),
        Record::SUBWrs => (32, true, false),
        Record::SUBXrs => (64, true, false),
        Record::SUBSWrs => (32, true, true),
        Record::SUBSXrs => (64, true, true),
        _ => return Lifted::Unsupported,
    };
    // The decoder refuses the kind 3 for an add or a subtract.
    if f.shift >> 6 == 3 {
        return Lifted::Unsupported;
    }
    let Some(right) = shifted_register(b, width, f.rm, f.shift) else {
        return Lifted::Unsupported;
    };
    let left = b.x(f.rn, width);
    let (result, flags) = add_or_subtract(b, width, subtract, left, right);
    if set_flags {
        b.set_flags(flags);
    }
    b.set_x(f.rd, width, result);
    Lifted::Next
}

/// The body of an add or subtract with an extended register. Register 31 is
/// the stack pointer as `rn`, and as `rd` when the flags stay.
fn add_sub_extended_body(
    b: &mut Builder,
    (width, subtract, set_flags): (u16, bool, bool),
    (rd, rn, rm): (u32, u32, u32),
    option: u32,
    shift: u32,
) -> Lifted {
    if shift > 4 {
        return Lifted::Unsupported;
    }
    let right = extended_register(b, width, rm, option, shift);
    let left = b.x_or_sp(rn, width);
    let (result, flags) = add_or_subtract(b, width, subtract, left, right);
    if set_flags {
        b.set_flags(flags);
        b.set_x(rd, width, result);
    } else {
        b.set_x_or_sp(rd, width, result);
    }
    Lifted::Next
}

/// `ADD`, `ADDS`, `SUB` and `SUBS` with an extended register. The raw field
/// is `option:imm3`.
pub fn add_sub_extended(b: &mut Builder, record: Record, f: &BaseAddSubEReg) -> Lifted {
    let kind = match record {
        Record::ADDWrx => (32, false, false),
        Record::ADDXrx => (64, false, false),
        Record::ADDSWrx => (32, false, true),
        Record::ADDSXrx => (64, false, true),
        Record::SUBWrx => (32, true, false),
        Record::SUBXrx => (64, true, false),
        Record::SUBSWrx => (32, true, true),
        Record::SUBSXrx => (64, true, true),
        _ => return Lifted::Unsupported,
    };
    add_sub_extended_body(b, kind, (f.rd, f.rn, f.rm), f.extend >> 3, f.extend & 7)
}

/// The same with `UXTX` or `SXTX`, which LLVM keeps as records of their own
/// because `rm` is an X register. The raw field has `imm3` in its bits 2 to 0
/// and the sign bit of the option in its bit 5.
pub fn add_sub_extended64(b: &mut Builder, record: Record, f: &BaseAddSubEReg64) -> Lifted {
    let kind = match record {
        Record::ADDXrx64 => (64, false, false),
        Record::ADDSXrx64 => (64, false, true),
        Record::SUBXrx64 => (64, true, false),
        Record::SUBSXrx64 => (64, true, true),
        _ => return Lifted::Unsupported,
    };
    let option = 3 | (((f.ext >> 5) & 1) << 2);
    add_sub_extended_body(b, kind, (f.rd, f.rn, f.rm), option, f.ext & 7)
}

/// The body of `ADC`, `SBC`, `ADCS` and `SBCS`: the carry flag is the carry
/// in, and a subtraction adds the inverse of `rm`.
fn carry_body(
    b: &mut Builder,
    (width, subtract, set_flags): (u16, bool, bool),
    (rd, rn, rm): (u32, u32, u32),
) -> Lifted {
    let left = b.x(rn, width);
    let mut right = b.x(rm, width);
    if subtract {
        right = b.not(width, right);
    }
    let carry = b.read(slot::C);
    let (result, flags) = b.add_with_carry(width, left, right, carry);
    if set_flags {
        b.set_flags(flags);
    }
    b.set_x(rd, width, result);
    Lifted::Next
}

/// `ADC` and `SBC`.
pub fn add_sub_carry(b: &mut Builder, record: Record, f: &BaseAddSubCarry) -> Lifted {
    let kind = match record {
        Record::ADCWr => (32, false, false),
        Record::ADCXr => (64, false, false),
        Record::SBCWr => (32, true, false),
        Record::SBCXr => (64, true, false),
        _ => return Lifted::Unsupported,
    };
    carry_body(b, kind, (f.rd, f.rn, f.rm))
}

/// `ADCS` and `SBCS`.
pub fn add_sub_carry_flags(b: &mut Builder, record: Record, f: &BaseAddSubCarrySetFlags) -> Lifted {
    let kind = match record {
        Record::ADCSWr => (32, false, true),
        Record::ADCSXr => (64, false, true),
        Record::SBCSWr => (32, true, true),
        Record::SBCSXr => (64, true, true),
        _ => return Lifted::Unsupported,
    };
    carry_body(b, kind, (f.rd, f.rn, f.rm))
}

// --- logical ----------------------------------------------------------------------

/// `AND`, `BIC`, `ORR`, `ORN`, `EOR`, `EON`, `ANDS` and `BICS` with a shifted
/// register. `MOV` and `MVN` between registers and `TST` are aliases.
pub fn logical_shifted(b: &mut Builder, record: Record, f: &BaseLogicalSReg) -> Lifted {
    let (width, operation, invert, set_flags) = match record {
        Record::ANDWrs => (32, Binary::And, false, false),
        Record::ANDXrs => (64, Binary::And, false, false),
        Record::BICWrs => (32, Binary::And, true, false),
        Record::BICXrs => (64, Binary::And, true, false),
        Record::ORRWrs => (32, Binary::Or, false, false),
        Record::ORRXrs => (64, Binary::Or, false, false),
        Record::ORNWrs => (32, Binary::Or, true, false),
        Record::ORNXrs => (64, Binary::Or, true, false),
        Record::EORWrs => (32, Binary::Xor, false, false),
        Record::EORXrs => (64, Binary::Xor, false, false),
        Record::EONWrs => (32, Binary::Xor, true, false),
        Record::EONXrs => (64, Binary::Xor, true, false),
        Record::ANDSWrs => (32, Binary::And, false, true),
        Record::ANDSXrs => (64, Binary::And, false, true),
        Record::BICSWrs => (32, Binary::And, true, true),
        Record::BICSXrs => (64, Binary::And, true, true),
        _ => return Lifted::Unsupported,
    };
    let Some(mut right) = shifted_register(b, width, f.rm, f.shift) else {
        return Lifted::Unsupported;
    };
    if invert {
        right = b.not(width, right);
    }
    let left = b.x(f.rn, width);
    let result = b.binary(width, operation, left, right);
    if set_flags {
        let flags = b.logical_flags(width, result);
        b.set_flags(flags);
    }
    b.set_x(f.rd, width, result);
    Lifted::Next
}

// --- conditional compare and select -------------------------------------------------

/// The body of `CCMP` and `CCMN`: the flags of the comparison where the
/// condition holds, else the four bits of the instruction.
fn conditional_compare(
    b: &mut Builder,
    width: u16,
    negative: bool,
    condition: u32,
    nzcv: u32,
    left: ScalarId,
    right: ScalarId,
) -> Lifted {
    // The condition reads the flags before the instruction writes them.
    let holds = b.condition(Condition::from_bits(condition));
    // CCMP compares by subtracting, CCMN by adding.
    let (_, compared) = add_or_subtract(b, width, !negative, left, right);
    let mut pick = |flag: ScalarId, bit: u32| {
        let given = b.imm(1, u64::from((nzcv >> bit) & 1));
        b.select(1, holds, flag, given)
    };
    let flags = Flags {
        n: pick(compared.n, 3),
        z: pick(compared.z, 2),
        c: pick(compared.c, 1),
        v: pick(compared.v, 0),
    };
    b.set_flags(flags);
    Lifted::Next
}

/// `CCMP` and `CCMN` with a 5-bit immediate.
pub fn conditional_compare_immediate(
    b: &mut Builder,
    record: Record,
    f: &BaseCondComparisonImm,
) -> Lifted {
    let (width, negative) = match record {
        Record::CCMPWi => (32, false),
        Record::CCMPXi => (64, false),
        Record::CCMNWi => (32, true),
        Record::CCMNXi => (64, true),
        _ => return Lifted::Unsupported,
    };
    let left = b.x(f.rn, width);
    let right = b.imm(width, u64::from(f.imm & 31));
    conditional_compare(b, width, negative, f.cond, f.nzcv, left, right)
}

/// `CCMP` and `CCMN` with a register.
pub fn conditional_compare_register(
    b: &mut Builder,
    record: Record,
    f: &BaseCondComparisonReg,
) -> Lifted {
    let (width, negative) = match record {
        Record::CCMPWr => (32, false),
        Record::CCMPXr => (64, false),
        Record::CCMNWr => (32, true),
        Record::CCMNXr => (64, true),
        _ => return Lifted::Unsupported,
    };
    let left = b.x(f.rn, width);
    let right = b.x(f.rm, width);
    conditional_compare(b, width, negative, f.cond, f.nzcv, left, right)
}

/// `CSEL`: `rn` where the condition holds, else `rm`.
pub fn conditional_select(b: &mut Builder, record: Record, f: &BaseCondSelect) -> Lifted {
    let width = match record {
        Record::CSELWr => 32,
        Record::CSELXr => 64,
        _ => return Lifted::Unsupported,
    };
    let holds = b.condition(Condition::from_bits(f.cond));
    let when_true = b.x(f.rn, width);
    let when_false = b.x(f.rm, width);
    let result = b.select(width, holds, when_true, when_false);
    b.set_x(f.rd, width, result);
    Lifted::Next
}

/// `CSINC`, `CSINV` and `CSNEG`: `rn` where the condition holds, else `rm`
/// plus one, inverted, or negated. `CSET`, `CINC` and the like are aliases.
pub fn conditional_select_op(b: &mut Builder, record: Record, f: &BaseCondSelectOp) -> Lifted {
    #[derive(Clone, Copy)]
    enum Else {
        Increment,
        Invert,
        Negate,
    }
    let (width, otherwise) = match record {
        Record::CSINCWr => (32, Else::Increment),
        Record::CSINCXr => (64, Else::Increment),
        Record::CSINVWr => (32, Else::Invert),
        Record::CSINVXr => (64, Else::Invert),
        Record::CSNEGWr => (32, Else::Negate),
        Record::CSNEGXr => (64, Else::Negate),
        _ => return Lifted::Unsupported,
    };
    let holds = b.condition(Condition::from_bits(f.cond));
    let when_true = b.x(f.rn, width);
    let source = b.x(f.rm, width);
    let when_false = match otherwise {
        Else::Increment => {
            let one = b.imm(width, 1);
            b.binary(width, Binary::Add, source, one)
        }
        Else::Invert => b.not(width, source),
        Else::Negate => b.negate(width, source),
    };
    let result = b.select(width, holds, when_true, when_false);
    b.set_x(f.rd, width, result);
    Lifted::Next
}

// --- shifts by a register -------------------------------------------------------------

/// `LSLV`, `LSRV`, `ASRV` and `RORV`: the amount is `rm` modulo the width.
pub fn shift_variable(b: &mut Builder, record: Record, f: &BaseShift) -> Lifted {
    #[derive(Clone, Copy)]
    enum Kind {
        Left,
        Right,
        Arithmetic,
        Rotate,
    }
    let (width, kind) = match record {
        Record::LSLVWr => (32u16, Kind::Left),
        Record::LSLVXr => (64, Kind::Left),
        Record::LSRVWr => (32, Kind::Right),
        Record::LSRVXr => (64, Kind::Right),
        Record::ASRVWr => (32, Kind::Arithmetic),
        Record::ASRVXr => (64, Kind::Arithmetic),
        Record::RORVWr => (32, Kind::Rotate),
        Record::RORVXr => (64, Kind::Rotate),
        _ => return Lifted::Unsupported,
    };
    let value = b.x(f.rn, width);
    let raw = b.x(f.rm, width);
    let mask = b.imm(width, u64::from(width) - 1);
    let amount = b.binary(width, Binary::And, raw, mask);
    let result = match kind {
        Kind::Left => b.shl_by(width, value, amount),
        Kind::Right => b.lsr_by(width, value, amount),
        Kind::Arithmetic => b.asr_by(width, value, amount),
        Kind::Rotate => {
            // With the amount 0 the left shift is by the width, which gives
            // zero, and the right shift gives the value.
            let low = b.lsr_by(width, value, amount);
            let size = b.imm(width, width.into());
            let up = b.binary(width, Binary::Sub, size, amount);
            let high = b.shl_by(width, value, up);
            b.binary(width, Binary::Or, low, high)
        }
    };
    b.set_x(f.rd, width, result);
    Lifted::Next
}

// --- divide and multiply ---------------------------------------------------------------

/// `UDIV` and `SDIV`. A division by zero gives zero and no exception. The
/// most negative number divided by minus one gives itself.
pub fn divide(b: &mut Builder, record: Record, f: &BaseDiv) -> Lifted {
    let (width, signed) = match record {
        Record::UDIVWr => (32u16, false),
        Record::UDIVXr => (64, false),
        Record::SDIVWr => (32, true),
        Record::SDIVXr => (64, true),
        _ => return Lifted::Unsupported,
    };
    let dividend = b.x(f.rn, width);
    let divisor = b.x(f.rm, width);
    let by_zero = b.is_zero(width, divisor);
    // The graph has no quotient for a zero divisor: divide by one there, and
    // drop the quotient.
    let one = b.imm(width, 1);
    let safe = b.select(width, by_zero, one, divisor);
    let quotient = if signed {
        // In 128 bits the one quotient that does not fit, of the most
        // negative number by minus one, is exact, and its low bits are the
        // most negative number again.
        let wide_dividend = b.sign_extend(dividend, width, 128);
        let wide_divisor = b.sign_extend(safe, width, 128);
        let wide = b.binary(128, Binary::DivideSigned, wide_dividend, wide_divisor);
        b.low(wide, width)
    } else {
        b.binary(width, Binary::DivideUnsigned, dividend, safe)
    };
    let zero = b.imm(width, 0);
    let result = b.select(width, by_zero, zero, quotient);
    b.set_x(f.rd, width, result);
    Lifted::Next
}

/// `MADD` and `MSUB`: `ra` plus or minus `rn * rm`. `MUL` and `MNEG` are the
/// aliases with the zero register as `ra`.
pub fn multiply_add(b: &mut Builder, record: Record, f: &BaseMulAccum) -> Lifted {
    let (width, subtract) = match record {
        Record::MADDWrrr => (32, false),
        Record::MADDXrrr => (64, false),
        Record::MSUBWrrr => (32, true),
        Record::MSUBXrrr => (64, true),
        _ => return Lifted::Unsupported,
    };
    let left = b.x(f.rn, width);
    let right = b.x(f.rm, width);
    let product = b.binary(width, Binary::Mul, left, right);
    let addend = b.x(f.ra, width);
    let operation = if subtract { Binary::Sub } else { Binary::Add };
    let result = b.binary(width, operation, addend, product);
    b.set_x(f.rd, width, result);
    Lifted::Next
}

/// `SMADDL`, `SMSUBL`, `UMADDL` and `UMSUBL`: the 64-bit `ra` plus or minus
/// the product of the 32-bit `rn` and `rm`, extended to 64 bits first.
pub fn multiply_add_long(b: &mut Builder, record: Record, f: &WideMulAccum) -> Lifted {
    let (signed, subtract) = match record {
        Record::SMADDLrrr => (true, false),
        Record::SMSUBLrrr => (true, true),
        Record::UMADDLrrr => (false, false),
        Record::UMSUBLrrr => (false, true),
        _ => return Lifted::Unsupported,
    };
    let wide = |b: &mut Builder, n: u32| {
        let narrow = b.x(n, 32);
        if signed {
            b.sign_extend(narrow, 32, 64)
        } else {
            b.zero_extend(narrow, 64)
        }
    };
    let left = wide(b, f.rn);
    let right = wide(b, f.rm);
    let product = b.binary(64, Binary::Mul, left, right);
    let addend = b.x(f.ra, 64);
    let operation = if subtract { Binary::Sub } else { Binary::Add };
    let result = b.binary(64, operation, addend, product);
    b.set_x(f.rd, 64, result);
    Lifted::Next
}

/// `SMULH` and `UMULH`: the high 64 bits of the 128-bit product.
pub fn multiply_high(b: &mut Builder, record: Record, f: &MulHi) -> Lifted {
    let signed = match record {
        Record::SMULHrr => true,
        Record::UMULHrr => false,
        _ => return Lifted::Unsupported,
    };
    let wide = |b: &mut Builder, n: u32| {
        let narrow = b.x(n, 64);
        if signed {
            b.sign_extend(narrow, 64, 128)
        } else {
            b.zero_extend(narrow, 128)
        }
    };
    let left = wide(b, f.rn);
    let right = wide(b, f.rm);
    let product = b.binary(128, Binary::Mul, left, right);
    let result = b.slice(product, 64, 64);
    b.set_x(f.rd, 64, result);
    Lifted::Next
}

// --- bit operations -----------------------------------------------------------------------

/// `RBIT`, `CLZ` and `CLS`.
pub fn one_operand(b: &mut Builder, record: Record, f: &BaseOneOperandData) -> Lifted {
    #[derive(Clone, Copy)]
    enum Kind {
        ReverseBits,
        LeadingZeros,
        LeadingSigns,
    }
    let (width, kind) = match record {
        Record::RBITWr => (32u16, Kind::ReverseBits),
        Record::RBITXr => (64, Kind::ReverseBits),
        Record::CLZWr => (32, Kind::LeadingZeros),
        Record::CLZXr => (64, Kind::LeadingZeros),
        Record::CLSWr => (32, Kind::LeadingSigns),
        Record::CLSXr => (64, Kind::LeadingSigns),
        _ => return Lifted::Unsupported,
    };
    let value = b.x(f.rn, width);
    let result = match kind {
        Kind::ReverseBits => b.unary(width, Unary::ReverseBits(width), value),
        Kind::LeadingZeros => b.unary(width, Unary::LeadingZeros(width), value),
        Kind::LeadingSigns => {
            // A bit of `value ^ (value >> 1)` is set where a bit differs
            // from the one above it. The top bit never is, so the count of
            // leading zeros is one more than the count of the bits below the
            // sign that repeat it.
            let shifted = b.asr(width, value, 1);
            let changes = b.binary(width, Binary::Xor, value, shifted);
            let zeros = b.unary(width, Unary::LeadingZeros(width), changes);
            let one = b.imm(width, 1);
            b.binary(width, Binary::Sub, zeros, one)
        }
    };
    b.set_x(f.rd, width, result);
    Lifted::Next
}

/// The bytes of each 16-bit part of `value` swapped.
fn swap_in_halfwords(b: &mut Builder, width: u16, value: ScalarId) -> ScalarId {
    let low_bytes = b.imm(width, 0x00ff_00ff_00ff_00ff & ones(width.into()));
    let low = b.binary(width, Binary::And, value, low_bytes);
    let up = b.shl(width, low, 8);
    let down = b.lsr(width, value, 8);
    let high = b.binary(width, Binary::And, down, low_bytes);
    b.binary(width, Binary::Or, up, high)
}

/// `REV` and `REV16` on 32 bits.
pub fn reverse32(b: &mut Builder, record: Record, f: &OneWRegData) -> Lifted {
    let value = b.x(f.rn, 32);
    let result = match record {
        Record::REVWr => b.unary(32, Unary::ReverseBytes(32), value),
        Record::REV16Wr => swap_in_halfwords(b, 32, value),
        _ => return Lifted::Unsupported,
    };
    b.set_x(f.rd, 32, result);
    Lifted::Next
}

/// `REV`, `REV32` and `REV16` on 64 bits.
pub fn reverse64(b: &mut Builder, record: Record, f: &OneXRegData) -> Lifted {
    let value = b.x(f.rn, 64);
    let result = match record {
        Record::REVXr => b.unary(64, Unary::ReverseBytes(64), value),
        Record::REV32Xr => {
            // All eight bytes reversed, then the two halves put back.
            let reversed = b.unary(64, Unary::ReverseBytes(64), value);
            b.ror(64, reversed, 32)
        }
        Record::REV16Xr => swap_in_halfwords(b, 64, value),
        _ => return Lifted::Unsupported,
    };
    b.set_x(f.rd, 64, result);
    Lifted::Next
}

// --- CRC32 ----------------------------------------------------------------------------------

/// The CRC-32 polynomial of IEEE 802.3 (`CRC32B` to `CRC32X`), bit-reversed.
pub const CRC32_POLYNOMIAL: u32 = 0xedb8_8320;
/// The Castagnoli polynomial (`CRC32CB` to `CRC32CX`), bit-reversed.
pub const CRC32C_POLYNOMIAL: u32 = 0x82f6_3b78;

/// One step of the bit-reversed CRC: the low bit leaves, and takes the
/// polynomial with it when it is set.
const fn crc_step(state: u32, polynomial: u32) -> u32 {
    (state >> 1) ^ if state & 1 != 0 { polynomial } else { 0 }
}

/// Entry `n`, for 1 to 64: what a lone bit 0 becomes after `n` steps.
const fn crc_powers(polynomial: u32) -> [u32; 65] {
    let mut table = [0; 65];
    table[0] = 1;
    let mut n = 1;
    while n <= 64 {
        table[n] = crc_step(table[n - 1], polynomial);
        n += 1;
    }
    table
}

static CRC32_POWERS: [u32; 65] = crc_powers(CRC32_POLYNOMIAL);
static CRC32C_POWERS: [u32; 65] = crc_powers(CRC32C_POLYNOMIAL);

/// `CRC32B`, `CRC32H`, `CRC32W`, `CRC32X` and the four of CRC-32C: the CRC in
/// `rn` taken over the low 8, 16, 32 or 64 bits of `rm`, without the
/// inversions before and after that a whole checksum has.
///
/// The CRC is linear over its bits. After `size` steps the state is what is
/// left of it above the bits that the steps consumed, exclusive-or the
/// contribution of each consumed bit, and the contribution of bit `i` is the
/// constant `powers[size - i]` when the bit is set. That is one multiply by a
/// constant and one exclusive-or per bit, as a tree and not as a chain of
/// `size` steps.
pub fn crc32(b: &mut Builder, record: Record, f: &BaseCRC32) -> Lifted {
    let (size, powers) = match record {
        Record::CRC32Brr => (8u16, &CRC32_POWERS),
        Record::CRC32Hrr => (16, &CRC32_POWERS),
        Record::CRC32Wrr => (32, &CRC32_POWERS),
        Record::CRC32Xrr => (64, &CRC32_POWERS),
        Record::CRC32CBrr => (8, &CRC32C_POWERS),
        Record::CRC32CHrr => (16, &CRC32C_POWERS),
        Record::CRC32CWrr => (32, &CRC32C_POWERS),
        Record::CRC32CXrr => (64, &CRC32C_POWERS),
        _ => return Lifted::Unsupported,
    };
    // The data enters at the low end of the state. With 64 bits of data the
    // state is 64 bits wide and the CRC is its low half.
    let state_width = size.max(32);
    let accumulator = b.x(f.rn, 32);
    let accumulator = b.zero_extend(accumulator, state_width);
    let data = b.x(f.rm, state_width);
    let data = if size < state_width {
        let low = b.low(data, size);
        b.zero_extend(low, state_width)
    } else {
        data
    };
    let state = b.binary(state_width, Binary::Xor, accumulator, data);
    let mut terms = Vec::with_capacity(usize::from(size) + 1);
    if size < 32 {
        let rest = b.lsr(32, state, size.into());
        terms.push(rest);
    }
    for index in 0..size {
        let bit = b.bit(state, index);
        let bit = b.zero_extend(bit, 32);
        let power = b.imm(32, powers[usize::from(size - index)].into());
        terms.push(b.binary(32, Binary::Mul, bit, power));
    }
    // A balanced tree keeps the graph shallow.
    while terms.len() > 1 {
        let mut next = Vec::with_capacity(terms.len().div_ceil(2));
        for pair in terms.chunks(2) {
            next.push(match *pair {
                [left, right] => b.binary(32, Binary::Xor, left, right),
                [single] => single,
                _ => unreachable!("chunks of two"),
            });
        }
        terms = next;
    }
    let result = terms[0];
    b.set_x(f.rd, 32, result);
    Lifted::Next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_crc_tables_are_the_bitwise_definition() {
        // The check value of CRC-32 over "123456789" is 0xcbf43926, and of
        // CRC-32C 0xe3069283, with the state inverted before and after.
        for (polynomial, powers, check) in [
            (CRC32_POLYNOMIAL, &CRC32_POWERS, 0xcbf4_3926u32),
            (CRC32C_POLYNOMIAL, &CRC32C_POWERS, 0xe306_9283),
        ] {
            let mut state = u32::MAX;
            for byte in b"123456789" {
                state ^= u32::from(*byte);
                // Eight steps through the table, as the lifter does them.
                let mut next = state >> 8;
                for index in 0..8 {
                    if (state >> index) & 1 != 0 {
                        next ^= powers[8 - index];
                    }
                }
                let mut bitwise = state;
                for _ in 0..8 {
                    bitwise = crc_step(bitwise, polynomial);
                }
                assert_eq!(next, bitwise);
                state = next;
            }
            assert_eq!(!state, check);
        }
    }
}
