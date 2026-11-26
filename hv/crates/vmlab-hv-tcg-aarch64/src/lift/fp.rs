//! Scalar floating point: the 16 format classes of the `FPARMv8` records that
//! are not loads and stores (140 records). Arithmetic, compare, convert,
//! round, fused multiply, move and select.
//!
//! A move, a select, `FABS` and `FNEG` are scalar operations of the graph.
//! Every operation that rounds or raises an exception is a call of a pure
//! helper of `helpers/fp.rs` (DESIGN.md G3d, R10): FPCR goes in as an
//! argument, and the bits for FPSR come back above the value.
//!
//! What is not here: the check that FP is enabled (CPACR_EL1.FPEN), which
//! belongs to the exception card (C6). These lifters run as if it were.
//!
//! Sources read (DESIGN.md "Sources"): S1, the LLVM AArch64 tables for the
//! records, their fields and their fixed bits. S3, dynarmic's A64 translator
//! for the FP groups (`floating_point_*.cpp`, 0BSD), for which operand is
//! which. S6, the lockstep against QEMU, for the behaviour. Not read: QEMU's
//! source, Arm's ASL or pseudocode (X1, X2).

use super::{Builder, Flags, Lifted};
use crate::fields::Condition;
use crate::generated::decode::{
    BaseFPCondComparison, BaseFPCondSelect, BaseFPConversion, BaseFPMoveImmediate,
    BaseFPToInteger, BaseFPToIntegerUnscaled, BaseIntegerToFP, BaseIntegerToFPUnscaled,
    BaseOneOperandFPComparison, BaseSingleOperandFPData, BaseThreeOperandFPData,
    BaseTwoOperandFPComparison, BaseTwoOperandFPData, BaseUnscaledConversion,
    BaseUnscaledConversionFromHigh, BaseUnscaledConversionToHigh, Record,
};
use crate::helpers::fp::{
    self, Binary as FpBinary, DOUBLE, FP_BINARY, FP_COMPARE, FP_CONVERT, FP_FROM_FIXED,
    FP_MUL_ADD, FP_TO_FIXED, FP_UNARY, Format, HALF, RESULT_BITS, SINGLE, ToInteger,
    Unary as FpUnary,
};
use vmlab_hv_tcg_ir::native::{Binary, Helper, ScalarId};

fn width(format: Format) -> u16 {
    format.bits as u16
}

/// The value of a helper result, as the bits of `format`.
fn value(b: &mut Builder, packed: ScalarId, bits: u16) -> ScalarId {
    b.slice(packed, 0, bits)
}

/// Put the exception bits of a helper result into FPSR.
fn raise(b: &mut Builder, packed: ScalarId) {
    let bits = b.slice(packed, 64, 8);
    b.raise(bits);
}

/// Call `helper` with the constant `code` first and FPCR last.
fn call(b: &mut Builder, helper: &'static Helper, code: u64, operands: &[ScalarId]) -> ScalarId {
    let mut args = vec![b.imm(32, code)];
    args.extend_from_slice(operands);
    args.push(b.fpcr());
    b.call(RESULT_BITS, helper, &args)
}

/// Write a helper result to the FP register `rd` and its bits to FPSR.
fn finish(b: &mut Builder, rd: u32, format: Format, packed: ScalarId) {
    let result = value(b, packed, width(format));
    b.set_v(rd, width(format), result);
    raise(b, packed);
}

/// What a record of `BaseSingleOperandFPData` does.
enum Single {
    Move,
    Abs,
    Neg,
    Helper(FpUnary),
}

/// `FMOV`, `FABS`, `FNEG`, `FSQRT` and the seven `FRINT` forms.
pub fn single_operand(b: &mut Builder, record: Record, f: &BaseSingleOperandFPData) -> Lifted {
    use Record::*;
    let (format, operation) = match record {
        FMOVSr => (SINGLE, Single::Move),
        FMOVDr => (DOUBLE, Single::Move),
        FABSSr => (SINGLE, Single::Abs),
        FABSDr => (DOUBLE, Single::Abs),
        FNEGSr => (SINGLE, Single::Neg),
        FNEGDr => (DOUBLE, Single::Neg),
        FSQRTSr => (SINGLE, Single::Helper(FpUnary::Sqrt)),
        FSQRTDr => (DOUBLE, Single::Helper(FpUnary::Sqrt)),
        FRINTNSr => (SINGLE, Single::Helper(FpUnary::RoundNearEven)),
        FRINTNDr => (DOUBLE, Single::Helper(FpUnary::RoundNearEven)),
        FRINTPSr => (SINGLE, Single::Helper(FpUnary::RoundUp)),
        FRINTPDr => (DOUBLE, Single::Helper(FpUnary::RoundUp)),
        FRINTMSr => (SINGLE, Single::Helper(FpUnary::RoundDown)),
        FRINTMDr => (DOUBLE, Single::Helper(FpUnary::RoundDown)),
        FRINTZSr => (SINGLE, Single::Helper(FpUnary::RoundToZero)),
        FRINTZDr => (DOUBLE, Single::Helper(FpUnary::RoundToZero)),
        FRINTASr => (SINGLE, Single::Helper(FpUnary::RoundTiesAway)),
        FRINTADr => (DOUBLE, Single::Helper(FpUnary::RoundTiesAway)),
        FRINTXSr => (SINGLE, Single::Helper(FpUnary::RoundExact)),
        FRINTXDr => (DOUBLE, Single::Helper(FpUnary::RoundExact)),
        FRINTISr => (SINGLE, Single::Helper(FpUnary::RoundCurrent)),
        FRINTIDr => (DOUBLE, Single::Helper(FpUnary::RoundCurrent)),
        _ => return Lifted::Unsupported,
    };
    let bits = width(format);
    let operand = b.v(f.rn, bits);
    match operation {
        Single::Move => b.set_v(f.rd, bits, operand),
        // The sign is one bit, whatever the value is: no exception, and a
        // NaN keeps its payload.
        Single::Abs => {
            let mask = b.imm(bits, !format.sign_bit() & format.mask());
            let result = b.binary(bits, Binary::And, operand, mask);
            b.set_v(f.rd, bits, result);
        }
        Single::Neg => {
            let sign = b.imm(bits, format.sign_bit());
            let result = b.binary(bits, Binary::Xor, operand, sign);
            b.set_v(f.rd, bits, result);
        }
        Single::Helper(operation) => {
            let packed = call(b, &FP_UNARY, fp::unary_code(operation, format), &[operand]);
            finish(b, f.rd, format, packed);
        }
    }
    Lifted::Next
}

/// `FADD`, `FSUB`, `FMUL`, `FDIV`, `FNMUL` and the four minimum and maximum
/// forms.
pub fn two_operand(b: &mut Builder, record: Record, f: &BaseTwoOperandFPData) -> Lifted {
    use Record::*;
    let (format, operation) = match record {
        FADDSrr => (SINGLE, FpBinary::Add),
        FADDDrr => (DOUBLE, FpBinary::Add),
        FSUBSrr => (SINGLE, FpBinary::Sub),
        FSUBDrr => (DOUBLE, FpBinary::Sub),
        FMULSrr => (SINGLE, FpBinary::Mul),
        FMULDrr => (DOUBLE, FpBinary::Mul),
        FDIVSrr => (SINGLE, FpBinary::Div),
        FDIVDrr => (DOUBLE, FpBinary::Div),
        FNMULSrr => (SINGLE, FpBinary::NegMul),
        FNMULDrr => (DOUBLE, FpBinary::NegMul),
        FMAXSrr => (SINGLE, FpBinary::Max),
        FMAXDrr => (DOUBLE, FpBinary::Max),
        FMINSrr => (SINGLE, FpBinary::Min),
        FMINDrr => (DOUBLE, FpBinary::Min),
        FMAXNMSrr => (SINGLE, FpBinary::MaxNumber),
        FMAXNMDrr => (DOUBLE, FpBinary::MaxNumber),
        FMINNMSrr => (SINGLE, FpBinary::MinNumber),
        FMINNMDrr => (DOUBLE, FpBinary::MinNumber),
        _ => return Lifted::Unsupported,
    };
    let bits = width(format);
    let n = b.v(f.rn, bits);
    let m = b.v(f.rm, bits);
    let packed = call(b, &FP_BINARY, fp::binary_code(operation, format), &[n, m]);
    finish(b, f.rd, format, packed);
    Lifted::Next
}

/// `FMADD`, `FMSUB`, `FNMADD` and `FNMSUB`: `Ra` plus or minus `Rn * Rm`,
/// rounded once.
pub fn three_operand(b: &mut Builder, record: Record, f: &BaseThreeOperandFPData) -> Lifted {
    use Record::*;
    // (the format, the addend is negated, the product is negated)
    let (format, negate_addend, negate_product) = match record {
        FMADDSrrr => (SINGLE, false, false),
        FMADDDrrr => (DOUBLE, false, false),
        FMSUBSrrr => (SINGLE, false, true),
        FMSUBDrrr => (DOUBLE, false, true),
        FNMADDSrrr => (SINGLE, true, true),
        FNMADDDrrr => (DOUBLE, true, true),
        FNMSUBSrrr => (SINGLE, true, false),
        FNMSUBDrrr => (DOUBLE, true, false),
        _ => return Lifted::Unsupported,
    };
    let bits = width(format);
    let addend = b.v(f.ra, bits);
    let n = b.v(f.rn, bits);
    let m = b.v(f.rm, bits);
    let code = fp::mul_add_code(format, negate_addend, negate_product);
    let packed = call(b, &FP_MUL_ADD, code, &[addend, n, m]);
    finish(b, f.rd, format, packed);
    Lifted::Next
}

/// N, Z, C and V of a compare helper result.
fn compare_flags(b: &mut Builder, packed: ScalarId) -> Flags {
    Flags {
        n: b.bit(packed, 3),
        z: b.bit(packed, 2),
        c: b.bit(packed, 1),
        v: b.bit(packed, 0),
    }
}

fn compare(b: &mut Builder, format: Format, signalling: bool, n: ScalarId, m: ScalarId) {
    let code = fp::compare_code(format, signalling);
    let packed = call(b, &FP_COMPARE, code, &[n, m]);
    let flags = compare_flags(b, packed);
    b.set_flags(flags);
    raise(b, packed);
}

/// `FCMP` and `FCMPE` of two registers.
pub fn compare_registers(
    b: &mut Builder,
    record: Record,
    f: &BaseTwoOperandFPComparison,
) -> Lifted {
    use Record::*;
    let (format, signalling) = match record {
        FCMPSrr => (SINGLE, false),
        FCMPDrr => (DOUBLE, false),
        FCMPESrr => (SINGLE, true),
        FCMPEDrr => (DOUBLE, true),
        _ => return Lifted::Unsupported,
    };
    let n = b.v(f.rn, width(format));
    let m = b.v(f.rm, width(format));
    compare(b, format, signalling, n, m);
    Lifted::Next
}

/// `FCMP` and `FCMPE` of a register with zero.
pub fn compare_zero(b: &mut Builder, record: Record, f: &BaseOneOperandFPComparison) -> Lifted {
    use Record::*;
    let (format, signalling) = match record {
        FCMPSri => (SINGLE, false),
        FCMPDri => (DOUBLE, false),
        FCMPESri => (SINGLE, true),
        FCMPEDri => (DOUBLE, true),
        _ => return Lifted::Unsupported,
    };
    let n = b.v(f.rn, width(format));
    let zero = b.imm(width(format), 0);
    compare(b, format, signalling, n, zero);
    Lifted::Next
}

/// `FCCMP` and `FCCMPE`: the flags of the compare where the condition holds,
/// else the immediate. Where it does not hold nothing is compared, and no
/// exception bit is set.
pub fn conditional_compare(b: &mut Builder, record: Record, f: &BaseFPCondComparison) -> Lifted {
    use Record::*;
    let (format, signalling) = match record {
        FCCMPSrr => (SINGLE, false),
        FCCMPDrr => (DOUBLE, false),
        FCCMPESrr => (SINGLE, true),
        FCCMPEDrr => (DOUBLE, true),
        _ => return Lifted::Unsupported,
    };
    let holds = b.condition(Condition::from_bits(f.cond));
    let n = b.v(f.rn, width(format));
    let m = b.v(f.rm, width(format));
    let packed = call(b, &FP_COMPARE, fp::compare_code(format, signalling), &[n, m]);
    let compared = compare_flags(b, packed);
    let mut pick = |compared: ScalarId, bit: u32| {
        let immediate = b.imm(1, u64::from((f.nzcv >> bit) & 1));
        b.select(1, holds, compared, immediate)
    };
    let flags = Flags {
        n: pick(compared.n, 3),
        z: pick(compared.z, 2),
        c: pick(compared.c, 1),
        v: pick(compared.v, 0),
    };
    b.set_flags(flags);
    let raised = b.slice(packed, 64, 8);
    let none = b.imm(8, 0);
    let raised = b.select(8, holds, raised, none);
    b.raise(raised);
    Lifted::Next
}

/// `FCSEL`: `Rn` where the condition holds, else `Rm`. No exception.
pub fn conditional_select(b: &mut Builder, record: Record, f: &BaseFPCondSelect) -> Lifted {
    let format = match record {
        Record::FCSELSrrr => SINGLE,
        Record::FCSELDrrr => DOUBLE,
        _ => return Lifted::Unsupported,
    };
    let bits = width(format);
    let holds = b.condition(Condition::from_bits(f.cond));
    let n = b.v(f.rn, bits);
    let m = b.v(f.rm, bits);
    let result = b.select(bits, holds, n, m);
    b.set_v(f.rd, bits, result);
    Lifted::Next
}

/// `FCVT` between half, single and double precision. The record names the
/// destination first: `FCVTDHr` is half to double.
pub fn convert(b: &mut Builder, record: Record, f: &BaseFPConversion) -> Lifted {
    use Record::*;
    let (from, to) = match record {
        FCVTDHr => (HALF, DOUBLE),
        FCVTSHr => (HALF, SINGLE),
        FCVTDSr => (SINGLE, DOUBLE),
        FCVTHSr => (SINGLE, HALF),
        FCVTSDr => (DOUBLE, SINGLE),
        FCVTHDr => (DOUBLE, HALF),
        _ => return Lifted::Unsupported,
    };
    let operand = b.v(f.rn, width(from));
    let packed = call(b, &FP_CONVERT, fp::convert_code(from, to), &[operand]);
    finish(b, f.rd, to, packed);
    Lifted::Next
}

/// `FMOV` with an 8-bit immediate.
pub fn move_immediate(b: &mut Builder, record: Record, f: &BaseFPMoveImmediate) -> Lifted {
    let format = match record {
        Record::FMOVSi => SINGLE,
        Record::FMOVDi => DOUBLE,
        _ => return Lifted::Unsupported,
    };
    let result = b.imm(width(format), fp::expand_immediate(format, f.imm));
    b.set_v(f.rd, width(format), result);
    Lifted::Next
}

/// What the fixed bits of a record of the group "conversion between floating
/// point and integer" say. The group has one layout: the width of the general
/// register in bit 31, the FP format in the bits 23 and 22, `rmode` in 20
/// and 19, and `opcode` in 18 to 16.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegerForm {
    /// The width of the general register: 32 or 64.
    pub integer_bits: u32,
    pub format: Format,
    pub kind: IntegerKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegerKind {
    /// `FCVT[NPMZA][SU]`: FP to integer.
    ToInteger { unsigned: bool, rounding: ToInteger },
    /// `SCVTF` and `UCVTF`: integer to FP.
    FromInteger { unsigned: bool },
    /// `FMOV` from an FP register to a general register, bit for bit.
    MoveToGeneral,
    /// `FMOV` from a general register to an FP register.
    MoveFromGeneral,
}

/// The form of a record of that group, from its fixed bits. `None` for a
/// combination that the group does not define, or a half-precision one.
pub fn integer_form(record: Record) -> Option<IntegerForm> {
    let word = record.info().value;
    let integer_bits = if word >> 31 != 0 { 64 } else { 32 };
    let format = match (word >> 22) & 3 {
        0 => SINGLE,
        1 => DOUBLE,
        _ => return None,
    };
    let rmode = (word >> 19) & 3;
    let opcode = (word >> 16) & 7;
    let kind = match (rmode, opcode) {
        (_, 0 | 1) => IntegerKind::ToInteger {
            unsigned: opcode == 1,
            rounding: match rmode {
                0 => ToInteger::NearEven,
                1 => ToInteger::Up,
                2 => ToInteger::Down,
                _ => ToInteger::ToZero,
            },
        },
        (0, 2 | 3) => IntegerKind::FromInteger {
            unsigned: opcode == 3,
        },
        (0, 4 | 5) => IntegerKind::ToInteger {
            unsigned: opcode == 5,
            rounding: ToInteger::TiesAway,
        },
        (0, 6) => IntegerKind::MoveToGeneral,
        (0, 7) => IntegerKind::MoveFromGeneral,
        _ => return None,
    };
    Some(IntegerForm {
        integer_bits,
        format,
        kind,
    })
}

fn to_integer(b: &mut Builder, record: Record, rd: u32, rn: u32, fraction_bits: u32) -> Lifted {
    let Some(IntegerForm {
        integer_bits,
        format,
        kind: IntegerKind::ToInteger { unsigned, rounding },
    }) = integer_form(record)
    else {
        return Lifted::Unsupported;
    };
    let operand = b.v(rn, width(format));
    let code = fp::to_fixed_code(format, integer_bits, unsigned, fraction_bits, rounding);
    let packed = call(b, &FP_TO_FIXED, code, &[operand]);
    let result = value(b, packed, integer_bits as u16);
    b.set_x(rd, integer_bits as u16, result);
    raise(b, packed);
    Lifted::Next
}

fn from_integer(b: &mut Builder, record: Record, rd: u32, rn: u32, fraction_bits: u32) -> Lifted {
    let Some(IntegerForm {
        integer_bits,
        format,
        kind: IntegerKind::FromInteger { unsigned },
    }) = integer_form(record)
    else {
        return Lifted::Unsupported;
    };
    let operand = b.x(rn, integer_bits as u16);
    let code = fp::from_fixed_code(format, integer_bits, unsigned, fraction_bits);
    let packed = call(b, &FP_FROM_FIXED, code, &[operand]);
    finish(b, rd, format, packed);
    Lifted::Next
}

/// The count of fraction bits that the raw field `scale` names. The field is
/// 64 minus the count. A 32-bit form has the top bit of the field among its
/// fixed bits, so its raw field has five bits.
fn fraction_bits(integer_bits: u32, scale: u32) -> u32 {
    if integer_bits == 64 {
        64 - scale
    } else {
        32 - scale
    }
}

/// `FCVT[NPMZA][SU]` to a general register.
pub fn to_integer_unscaled(
    b: &mut Builder,
    record: Record,
    f: &BaseFPToIntegerUnscaled,
) -> Lifted {
    to_integer(b, record, f.rd, f.rn, 0)
}

/// `FCVTZS` and `FCVTZU` to a fixed-point value in a general register.
pub fn to_integer_scaled(b: &mut Builder, record: Record, f: &BaseFPToInteger) -> Lifted {
    let Some(form) = integer_form(record) else {
        return Lifted::Unsupported;
    };
    let fraction_bits = fraction_bits(form.integer_bits, f.scale);
    // A 64-bit form with the field 0 would name 64 fraction bits, which the
    // helper takes. The decoder gives no other count.
    to_integer(b, record, f.rd, f.rn, fraction_bits)
}

/// `SCVTF` and `UCVTF` from a general register.
pub fn from_integer_unscaled(
    b: &mut Builder,
    record: Record,
    f: &BaseIntegerToFPUnscaled,
) -> Lifted {
    from_integer(b, record, f.rd, f.rn, 0)
}

/// `SCVTF` and `UCVTF` from a fixed-point value in a general register.
pub fn from_integer_scaled(b: &mut Builder, record: Record, f: &BaseIntegerToFP) -> Lifted {
    let Some(form) = integer_form(record) else {
        return Lifted::Unsupported;
    };
    let fraction_bits = fraction_bits(form.integer_bits, f.scale);
    from_integer(b, record, f.rd, f.rn, fraction_bits)
}

/// `FMOV` between a general register and an FP register of the same width,
/// bit for bit.
pub fn move_general(b: &mut Builder, record: Record, f: &BaseUnscaledConversion) -> Lifted {
    let Some(form) = integer_form(record) else {
        return Lifted::Unsupported;
    };
    let bits = form.integer_bits as u16;
    if bits != width(form.format) {
        return Lifted::Unsupported;
    }
    match form.kind {
        IntegerKind::MoveToGeneral => {
            let value = b.v(f.rn, bits);
            b.set_x(f.rd, bits, value);
        }
        IntegerKind::MoveFromGeneral => {
            let value = b.x(f.rn, bits);
            b.set_v(f.rd, bits, value);
        }
        _ => return Lifted::Unsupported,
    }
    Lifted::Next
}

/// `FMOV Xd, Vn.D[1]`: the upper half of a vector register.
pub fn move_from_high(
    b: &mut Builder,
    record: Record,
    f: &BaseUnscaledConversionFromHigh,
) -> Lifted {
    if record != Record::FMOVDXHighr {
        return Lifted::Unsupported;
    }
    let whole = b.v(f.rn, 128);
    let high = b.slice(whole, 64, 64);
    b.set_x(f.rd, 64, high);
    Lifted::Next
}

/// `FMOV Vd.D[1], Xn`: the upper half of a vector register. The lower half
/// stays as it is.
pub fn move_to_high(b: &mut Builder, record: Record, f: &BaseUnscaledConversionToHigh) -> Lifted {
    if record != Record::FMOVXDHighr {
        return Lifted::Unsupported;
    }
    let low = b.v(f.rd, 64);
    let high = b.x(f.rn, 64);
    let whole = b.concat(high, low, 64, 128);
    b.set_v(f.rd, 128, whole);
    Lifted::Next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FormatClass;
    use crate::oracle::stream::records_of;

    /// The form that the name of a record says, in LLVM's naming of this
    /// group: `FCVT<mode><S|U>[S|U]<W|X><S|D>r`, `<S|U>CVTF<S|U><W|X><S|D>ri`
    /// and `FMOV<from><to>r`.
    fn named(name: &str) -> Option<IntegerForm> {
        let register = |letter: char| match letter {
            'W' => Some(32),
            'X' => Some(64),
            _ => None,
        };
        let format = |letter: char| match letter {
            'S' => Some(SINGLE),
            'D' => Some(DOUBLE),
            _ => None,
        };
        if let Some(rest) = name.strip_prefix("FCVT") {
            let letters: Vec<char> = rest.chars().collect();
            let rounding = match letters[0] {
                'N' => ToInteger::NearEven,
                'P' => ToInteger::Up,
                'M' => ToInteger::Down,
                'Z' => ToInteger::ToZero,
                'A' => ToInteger::TiesAway,
                _ => return None,
            };
            // The sign, then S for scaled or U for unscaled.
            return Some(IntegerForm {
                integer_bits: register(letters[3])?,
                format: format(letters[4])?,
                kind: IntegerKind::ToInteger {
                    unsigned: letters[1] == 'U',
                    rounding,
                },
            });
        }
        if let Some(rest) = name.strip_prefix("SCVTF").or(name.strip_prefix("UCVTF")) {
            let letters: Vec<char> = rest.chars().collect();
            return Some(IntegerForm {
                integer_bits: register(letters[1])?,
                format: format(letters[2])?,
                kind: IntegerKind::FromInteger {
                    unsigned: name.starts_with('U'),
                },
            });
        }
        let rest = name.strip_prefix("FMOV")?;
        let letters: Vec<char> = rest.chars().collect();
        if let (Some(format), Some(integer_bits)) = (format(letters[0]), register(letters[1])) {
            return Some(IntegerForm {
                integer_bits,
                format,
                kind: IntegerKind::MoveToGeneral,
            });
        }
        Some(IntegerForm {
            integer_bits: register(letters[0])?,
            format: format(letters[1])?,
            kind: IntegerKind::MoveFromGeneral,
        })
    }

    #[test]
    fn the_fixed_bits_of_an_integer_conversion_say_what_its_name_says() {
        let mut checked = 0;
        for class in [
            FormatClass::BaseFPToIntegerUnscaled,
            FormatClass::BaseFPToInteger,
            FormatClass::BaseIntegerToFPUnscaled,
            FormatClass::BaseIntegerToFP,
            FormatClass::BaseUnscaledConversion,
        ] {
            for record in records_of(class) {
                let name = record.name();
                assert_eq!(integer_form(record), named(name), "{name}");
                assert!(integer_form(record).is_some(), "{name}");
                checked += 1;
            }
        }
        assert_eq!(checked, 40 + 8 + 8 + 8 + 4);
    }

    #[test]
    fn the_scale_field_counts_down_from_the_width() {
        assert_eq!(fraction_bits(64, 63), 1);
        assert_eq!(fraction_bits(64, 0), 64);
        assert_eq!(fraction_bits(32, 31), 1);
        assert_eq!(fraction_bits(32, 0), 32);
    }
}
