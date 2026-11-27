//! Data processing with an immediate: the eight format classes `ADRI`,
//! `AddSubImmShift`, `BaseLogicalImm`, `BaseMoveImmediate`,
//! `BaseInsertImmediate`, `BaseBitfieldImm`, `BaseBitfieldImmWith2RegArgs`
//! and `BaseExtractImm` (32 records).
//!
//! Sources read (DESIGN.md "Sources"): S1, the LLVM AArch64 tables for the
//! records, their fields and their operand classes (which register 31 is the
//! stack pointer and which the zero register). S6, the lockstep against QEMU,
//! for the behaviour. No text of S2 to S4 was opened for this file. Not read:
//! QEMU's source, Arm's ASL or pseudocode (X1, X2).

use super::{Builder, Lifted};
use crate::fields::{self, ones, sign_extend};
use crate::generated::decode::{
    ADRI, AddSubImmShift, BaseBitfieldImm, BaseBitfieldImmWith2RegArgs, BaseExtractImm,
    BaseInsertImmediate, BaseLogicalImm, BaseMoveImmediate, Record,
};
use vmlab_hv_tcg_ir::native::Binary;

/// `ADR` and `ADRP`: a PC-relative address.
pub fn adr(b: &mut Builder, record: Record, f: &ADRI) -> Lifted {
    let offset = sign_extend(f.label.into(), 21);
    let value = match record {
        Record::ADR => b.pc().wrapping_add(offset as u64),
        Record::ADRP => (b.pc() & !0xfff).wrapping_add((offset << 12) as u64),
        _ => return Lifted::Unsupported,
    };
    let value = b.imm(64, value);
    b.set_x(f.xd, 64, value);
    Lifted::Next
}

/// `ADD`, `ADDS`, `SUB` and `SUBS` with a 12-bit immediate, shifted by 0 or 12.
pub fn add_sub_immediate(b: &mut Builder, record: Record, f: &AddSubImmShift) -> Lifted {
    let (width, subtract, set_flags) = match record {
        Record::ADDWri => (32, false, false),
        Record::ADDXri => (64, false, false),
        Record::ADDSWri => (32, false, true),
        Record::ADDSXri => (64, false, true),
        Record::SUBWri => (32, true, false),
        Record::SUBXri => (64, true, false),
        Record::SUBSWri => (32, true, true),
        Record::SUBSXri => (64, true, true),
        _ => return Lifted::Unsupported,
    };
    // The field is shift:imm12. The decoder admits the shifts 0 and 1 only.
    let shift = f.imm >> 12;
    if shift > 1 {
        return Lifted::Unsupported;
    }
    let immediate = u64::from(f.imm & 0xfff) << (12 * shift);
    let operand = b.x_or_sp(f.rn, width);
    let addend = b.imm(width, immediate);
    let (result, flags) = if subtract {
        let carry = b.imm(1, 1);
        b.sub_with_carry(width, operand, addend, carry)
    } else {
        let carry = b.imm(1, 0);
        b.add_with_carry(width, operand, addend, carry)
    };
    if set_flags {
        b.set_flags(flags);
        b.set_x(f.rd, width, result);
    } else {
        b.set_x_or_sp(f.rd, width, result);
    }
    Lifted::Next
}

/// `AND`, `ORR`, `EOR` and `ANDS` with a bitmask immediate.
pub fn logical_immediate(b: &mut Builder, record: Record, f: &BaseLogicalImm) -> Lifted {
    let (width, operation, set_flags) = match record {
        Record::ANDWri => (32, Binary::And, false),
        Record::ANDXri => (64, Binary::And, false),
        Record::ORRWri => (32, Binary::Or, false),
        Record::ORRXri => (64, Binary::Or, false),
        Record::EORWri => (32, Binary::Xor, false),
        Record::EORXri => (64, Binary::Xor, false),
        Record::ANDSWri => (32, Binary::And, true),
        Record::ANDSXri => (64, Binary::And, true),
        _ => return Lifted::Unsupported,
    };
    let Some(immediate) = fields::logical_immediate(f.imm, width.into()) else {
        return Lifted::Unsupported;
    };
    let operand = b.x(f.rn, width);
    let immediate = b.imm(width, immediate);
    let result = b.binary(width, operation, operand, immediate);
    if set_flags {
        let flags = b.logical_flags(width, result);
        b.set_flags(flags);
        b.set_x(f.rd, width, result);
    } else {
        b.set_x_or_sp(f.rd, width, result);
    }
    Lifted::Next
}

/// `MOVZ` and `MOVN`: a 16-bit immediate at a multiple of 16, or its inverse.
/// The raw field `shift` is the shift in bits (0, 16, 32 or 48), as the LLVM
/// record numbers its bits.
pub fn move_wide(b: &mut Builder, record: Record, f: &BaseMoveImmediate) -> Lifted {
    let (width, invert) = match record {
        Record::MOVZWi => (32, false),
        Record::MOVZXi => (64, false),
        Record::MOVNWi => (32, true),
        Record::MOVNXi => (64, true),
        _ => return Lifted::Unsupported,
    };
    if f.shift >= u32::from(width) || !f.shift.is_multiple_of(16) {
        return Lifted::Unsupported;
    }
    let mut value = u64::from(f.imm) << f.shift;
    if invert {
        value = !value & ones(width.into());
    }
    let value = b.imm(width, value);
    b.set_x(f.rd, width, value);
    Lifted::Next
}

/// `MOVK`: replace 16 bits of the register and keep the rest.
pub fn move_keep(b: &mut Builder, record: Record, f: &BaseInsertImmediate) -> Lifted {
    let width = match record {
        Record::MOVKWi => 32,
        Record::MOVKXi => 64,
        _ => return Lifted::Unsupported,
    };
    if f.shift >= u32::from(width) || !f.shift.is_multiple_of(16) {
        return Lifted::Unsupported;
    }
    let position = f.shift;
    let keep = !(0xffffu64 << position) & ones(width.into());
    let old = b.x(f.rd, width);
    let keep = b.imm(width, keep);
    let kept = b.binary(width, Binary::And, old, keep);
    let insert = b.imm(width, u64::from(f.imm) << position);
    let result = b.binary(width, Binary::Or, kept, insert);
    b.set_x(f.rd, width, result);
    Lifted::Next
}

/// Where a bitfield move takes its bits from and puts them.
///
/// With `imms >= immr` the field is the bits `imms` down to `immr` of the
/// source, moved to bit 0. With `imms < immr` it is the low `imms + 1` bits
/// of the source, moved up to bit `width - immr`.
struct Field {
    /// Shift the source left by this to put the top bit of the field at the
    /// top of the register.
    up: u32,
    /// Then shift right by this to put the field in its place.
    down: u32,
    /// The bits of the destination that the field covers.
    mask: u64,
}

fn field(width: u32, immr: u32, imms: u32) -> Field {
    let up = width - 1 - imms;
    if imms >= immr {
        Field {
            up,
            down: up + immr,
            mask: ones(imms - immr + 1),
        }
    } else {
        Field {
            up,
            down: immr - 1 - imms,
            mask: ones(imms + 1) << (width - immr),
        }
    }
}

/// `SBFM` and `UBFM`: the field, extended with its sign or with zeros.
/// The shifts, the extensions, `SBFX`, `UBFX`, `SBFIZ` and `UBFIZ` are
/// aliases of these two.
pub fn bitfield(b: &mut Builder, record: Record, f: &BaseBitfieldImm) -> Lifted {
    let (width, signed) = match record {
        Record::SBFMWri => (32u16, true),
        Record::SBFMXri => (64, true),
        Record::UBFMWri => (32, false),
        Record::UBFMXri => (64, false),
        _ => return Lifted::Unsupported,
    };
    if f.immr >= u32::from(width) || f.imms >= u32::from(width) {
        return Lifted::Unsupported;
    }
    let place = field(width.into(), f.immr, f.imms);
    let source = b.x(f.rn, width);
    let top = b.shl(width, source, place.up);
    let result = if signed {
        b.asr(width, top, place.down)
    } else {
        b.lsr(width, top, place.down)
    };
    b.set_x(f.rd, width, result);
    Lifted::Next
}

/// `BFM`: the field replaces bits of the destination, and the rest stays.
pub fn bitfield_insert(b: &mut Builder, record: Record, f: &BaseBitfieldImmWith2RegArgs) -> Lifted {
    let width = match record {
        Record::BFMWri => 32u16,
        Record::BFMXri => 64,
        _ => return Lifted::Unsupported,
    };
    if f.immr >= u32::from(width) || f.imms >= u32::from(width) {
        return Lifted::Unsupported;
    }
    let place = field(width.into(), f.immr, f.imms);
    let source = b.x(f.rn, width);
    let top = b.shl(width, source, place.up);
    let moved = b.lsr(width, top, place.down);
    let old = b.x(f.rd, width);
    let keep = b.imm(width, !place.mask & ones(width.into()));
    let kept = b.binary(width, Binary::And, old, keep);
    let result = b.binary(width, Binary::Or, kept, moved);
    b.set_x(f.rd, width, result);
    Lifted::Next
}

/// `EXTR`: `width` bits of `Rn:Rm` from the bit `imm` of `Rm`. `ROR` with an
/// immediate is the alias with both registers the same.
pub fn extract(b: &mut Builder, record: Record, f: &BaseExtractImm) -> Lifted {
    let width = match record {
        Record::EXTRWrri => 32u16,
        Record::EXTRXrri => 64,
        _ => return Lifted::Unsupported,
    };
    if f.imm >= u32::from(width) {
        return Lifted::Unsupported;
    }
    let low = b.x(f.rm, width);
    let result = if f.imm == 0 {
        low
    } else {
        let high = b.x(f.rn, width);
        let from_low = b.lsr(width, low, f.imm);
        let from_high = b.shl(width, high, u32::from(width) - f.imm);
        b.binary(width, Binary::Or, from_low, from_high)
    };
    b.set_x(f.rd, width, result);
    Lifted::Next
}
