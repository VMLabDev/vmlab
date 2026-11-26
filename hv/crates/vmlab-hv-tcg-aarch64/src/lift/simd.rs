//! AdvSIMD (card C7): the 47 format classes of the `NEON` records that are
//! not loads and stores (1,412 records, with `PMULL` of the `AES` set).
//!
//! A lifter here reads the instruction word, not the fields of one record:
//! the encoding groups of AdvSIMD put the vector length in bit 30 (`Q`), the
//! signedness in bit 29 (`U`), the lane size in the bits 23 and 22 and an
//! opcode below them, and the LLVM tables have one record for each
//! combination. `plan` functions turn the fixed bits into an operation, and
//! a test holds each against the name of its record.
//!
//! Bitwise operations, moves of lanes between registers and immediates are
//! scalar operations of the graph. Everything else is one call of the lane
//! helper `helpers::simd::eval` (DESIGN.md G3d): a code that names the
//! operation, the lane size and the lane count, three 128-bit operands and
//! FPCR. An operation that accumulates into its destination is two calls.
//!
//! What is not here: the check that FP and AdvSIMD are enabled
//! (CPACR_EL1.FPEN), which belongs to the exception card (C6).
//!
//! Sources read (DESIGN.md "Sources"): S1, the generated tables of the LLVM
//! AArch64 records for the records, their fixed bits and their names. S6,
//! the lockstep against QEMU, for the behaviour. The opcode tables of the
//! `plan` functions are written from the author's knowledge of the published
//! A64 encoding, and the test against the record names checks each row. No
//! file of S2, S3 or S4 was opened for this file. Not read: QEMU's source,
//! Arm's ASL or pseudocode (X1, X2).

use super::{Builder, Lifted};
use crate::generated::decode::{FormatClass, Record};
use crate::helpers::fp::{self, DOUBLE, SINGLE, ToInteger};
use crate::helpers::simd::{Code, Op, QC, SIMD, SIMD_FLAGS, SIMD_LOW, SIMD_TABLE, table_code};
use crate::state::slot;
use vmlab_hv_tcg_ir::native::{Binary, ScalarId};

fn bit(word: u32, index: u32) -> bool {
    word >> index & 1 != 0
}

fn bits(word: u32, low: u32, count: u32) -> u32 {
    (word >> low) & ((1 << count) - 1)
}

/// The fields that every group has in one place.
#[derive(Clone, Copy)]
struct Word {
    word: u32,
    rd: u32,
    rn: u32,
    rm: u32,
    /// Bit 30: a vector of 128 bits, or the upper half of a source or of a
    /// destination.
    q: bool,
    /// Bit 29.
    u: bool,
    /// The bits 23 and 22.
    size: u32,
    /// Bit 28: one lane.
    scalar: bool,
}

impl Word {
    fn new(word: u32) -> Self {
        Self {
            word,
            rd: bits(word, 0, 5),
            rn: bits(word, 5, 5),
            rm: bits(word, 16, 5),
            q: bit(word, 30),
            u: bit(word, 29),
            size: bits(word, 22, 2),
            scalar: bit(word, 28),
        }
    }

    /// The width of a vector operand: 128 or 64.
    fn width(self) -> u16 {
        if self.q { 128 } else { 64 }
    }

    /// The count of lanes of `size` in a vector operand, or 1.
    fn lanes(self, size: u32) -> u8 {
        if self.scalar {
            1
        } else {
            (self.width() / (8 << size)) as u8
        }
    }

    /// The count of lanes of `size` in half a register, or 1.
    fn half_lanes(self, size: u32) -> u8 {
        if self.scalar {
            1
        } else {
            (64 / (8u32 << size)) as u8
        }
    }
}

// --- values -------------------------------------------------------------------

fn zero(b: &mut Builder) -> ScalarId {
    b.imm(128, 0)
}

fn constant(b: &mut Builder, value: u128) -> ScalarId {
    let high = b.imm(64, (value >> 64) as u64);
    let low = b.imm(64, value as u64);
    b.concat(high, low, 64, 128)
}

/// Every byte of a vector is `byte`: the amount of a shift by an immediate.
fn splat(b: &mut Builder, byte: i32) -> ScalarId {
    constant(b, u128::from_le_bytes([byte as u8; 16]))
}

/// The upper half of `value`, as the lower half of a vector.
fn upper(b: &mut Builder, value: ScalarId) -> ScalarId {
    let half = b.slice(value, 64, 64);
    b.zero_extend(half, 128)
}

/// The operand of a widening operation: the upper half of the register for
/// a "2" form, else the register.
fn half(b: &mut Builder, word: Word, register: u32) -> ScalarId {
    let value = b.v(register, 128);
    if word.q && !word.scalar {
        upper(b, value)
    } else {
        value
    }
}

/// `element`, of `width` bits, in every lane of a vector of `total` bits,
/// with zeros above.
fn replicate(b: &mut Builder, element: ScalarId, width: u16, total: u16) -> ScalarId {
    let mut value = element;
    let mut have = width;
    while have < total {
        value = b.concat(value, value, have, have * 2);
        have *= 2;
    }
    b.zero_extend(value, 128)
}

/// `vector` with the `width` bits at `position` replaced by `element`.
fn insert(
    b: &mut Builder,
    vector: ScalarId,
    position: u16,
    width: u16,
    element: ScalarId,
) -> ScalarId {
    let mut result = element;
    let mut have = width;
    if position > 0 {
        let low = b.slice(vector, 0, position);
        have = position + width;
        result = b.concat(result, low, position, have);
    }
    if have < 128 {
        let high = b.slice(vector, have, 128 - have);
        result = b.concat(high, result, have, 128);
    }
    result
}

/// OR the bits that a lane helper gives into FPSR. The saturation bit comes
/// at `QC` and goes to bit 27.
fn raise(b: &mut Builder, flags: ScalarId) {
    let wide = b.zero_extend(flags, 32);
    let plain_mask = b.imm(32, 0x9f);
    let plain = b.binary(32, Binary::And, wide, plain_mask);
    let moved = b.shl(32, wide, 27 - QC.trailing_zeros());
    let saturation_mask = b.imm(32, 1 << 27);
    let saturation = b.binary(32, Binary::And, moved, saturation_mask);
    let bits = b.binary(32, Binary::Or, plain, saturation);
    let old = b.read(slot::FPSR);
    let new = b.binary(32, Binary::Or, old, bits);
    b.write(slot::FPSR, new);
}

/// Run `code` on three vectors. Gives the result as 128 bits, with zeros
/// above its lanes, and puts the bits that it raises into FPSR.
fn apply(b: &mut Builder, code: Code, x: ScalarId, y: ScalarId, z: ScalarId) -> ScalarId {
    let control = if code.op.is_fp() {
        b.fpcr()
    } else {
        b.imm(32, 0)
    };
    let args = [b.imm(64, code.encode()), x, y, z, control];
    if !code.op.raises() {
        return b.call(128, &SIMD, &args);
    }
    if code.result_bits() <= 64 {
        let packed = b.call(72, &SIMD_LOW, &args);
        let flags = b.slice(packed, 64, 8);
        raise(b, flags);
        let value = b.slice(packed, 0, 64);
        return b.zero_extend(value, 128);
    }
    let value = b.call(128, &SIMD, &args);
    let flags = b.call(8, &SIMD_FLAGS, &args);
    raise(b, flags);
    value
}

fn unary(b: &mut Builder, code: Code, x: ScalarId) -> ScalarId {
    let nothing = zero(b);
    apply(b, code, x, nothing, nothing)
}

fn binary(b: &mut Builder, code: Code, x: ScalarId, y: ScalarId) -> ScalarId {
    let nothing = zero(b);
    apply(b, code, x, y, nothing)
}

/// Write the result of a narrowing operation: the lower half of `rd` with
/// zeros above, or for a "2" form the upper half with the lower half kept.
fn set_narrow(b: &mut Builder, word: Word, value: ScalarId) {
    if word.q && !word.scalar {
        let kept = b.v(word.rd, 64);
        let low = b.slice(value, 0, 64);
        let whole = b.concat(low, kept, 64, 128);
        b.set_v(word.rd, 128, whole);
    } else {
        b.set_v(word.rd, 128, value);
    }
}

// --- three registers of one arrangement ------------------------------------------

/// What an instruction of the group "three same" does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Three {
    /// `Rn op Rm`.
    Lanes(Op),
    /// The same on neighbours of `Rm:Rn`.
    Pairs(Op),
    /// `Rd outer (Rn inner Rm)`.
    Accumulate {
        inner: Op,
        outer: Op,
    },
    /// `Rd` is the third operand of the operation.
    Fused(Op),
    Logic(Logic),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Logic {
    And,
    Bic,
    Orr,
    Orn,
    Eor,
    Bsl,
    Bit,
    Bif,
}

/// The mnemonic and the operation of the fixed bits `U`, `size` and `opcode`
/// (the bits 15 to 11) of the group, and whether its lanes are floating
/// point.
fn three_same_plan(u: bool, size: u32, opcode: u32) -> Option<(&'static str, Three, bool)> {
    use Op::*;
    use Three::*;
    if opcode >= 0b11000 {
        // Bit 23 is a second opcode bit, and bit 22 the lane size.
        let plan = match (u, size >> 1, opcode) {
            (false, 0, 0b11000) => ("FMAXNM", Lanes(FMaxNm)),
            (false, 0, 0b11001) => ("FMLA", Fused(FMla)),
            (false, 0, 0b11010) => ("FADD", Lanes(FAdd)),
            (false, 0, 0b11011) => ("FMULX", Lanes(FMulX)),
            (false, 0, 0b11100) => ("FCMEQ", Lanes(FCmEq)),
            (false, 0, 0b11110) => ("FMAX", Lanes(FMax)),
            (false, 0, 0b11111) => ("FRECPS", Lanes(FRecpS)),
            (false, 1, 0b11000) => ("FMINNM", Lanes(FMinNm)),
            (false, 1, 0b11001) => ("FMLS", Fused(FMls)),
            (false, 1, 0b11010) => ("FSUB", Lanes(FSub)),
            (false, 1, 0b11110) => ("FMIN", Lanes(FMin)),
            (false, 1, 0b11111) => ("FRSQRTS", Lanes(FRSqrtS)),
            (true, 0, 0b11000) => ("FMAXNMP", Pairs(FMaxNm)),
            (true, 0, 0b11010) => ("FADDP", Pairs(FAdd)),
            (true, 0, 0b11011) => ("FMUL", Lanes(FMul)),
            (true, 0, 0b11100) => ("FCMGE", Lanes(FCmGe)),
            (true, 0, 0b11101) => ("FACGE", Lanes(FAcGe)),
            (true, 0, 0b11110) => ("FMAXP", Pairs(FMax)),
            (true, 0, 0b11111) => ("FDIV", Lanes(FDiv)),
            (true, 1, 0b11000) => ("FMINNMP", Pairs(FMinNm)),
            (true, 1, 0b11010) => ("FABD", Lanes(FAbd)),
            (true, 1, 0b11100) => ("FCMGT", Lanes(FCmGt)),
            (true, 1, 0b11101) => ("FACGT", Lanes(FAcGt)),
            (true, 1, 0b11110) => ("FMINP", Pairs(FMin)),
            _ => return None,
        };
        return Some((plan.0, plan.1, true));
    }
    let plan = match (opcode, u) {
        (0b00000, false) => ("SHADD", Lanes(SHAdd)),
        (0b00000, true) => ("UHADD", Lanes(UHAdd)),
        (0b00001, false) => ("SQADD", Lanes(SQAdd)),
        (0b00001, true) => ("UQADD", Lanes(UQAdd)),
        (0b00010, false) => ("SRHADD", Lanes(SRHAdd)),
        (0b00010, true) => ("URHADD", Lanes(URHAdd)),
        (0b00011, false) => match size {
            0 => ("AND", Logic(self::Logic::And)),
            1 => ("BIC", Logic(self::Logic::Bic)),
            2 => ("ORR", Logic(self::Logic::Orr)),
            _ => ("ORN", Logic(self::Logic::Orn)),
        },
        (0b00011, true) => match size {
            0 => ("EOR", Logic(self::Logic::Eor)),
            1 => ("BSL", Logic(self::Logic::Bsl)),
            2 => ("BIT", Logic(self::Logic::Bit)),
            _ => ("BIF", Logic(self::Logic::Bif)),
        },
        (0b00100, false) => ("SHSUB", Lanes(SHSub)),
        (0b00100, true) => ("UHSUB", Lanes(UHSub)),
        (0b00101, false) => ("SQSUB", Lanes(SQSub)),
        (0b00101, true) => ("UQSUB", Lanes(UQSub)),
        (0b00110, false) => ("CMGT", Lanes(CmGt)),
        (0b00110, true) => ("CMHI", Lanes(CmHi)),
        (0b00111, false) => ("CMGE", Lanes(CmGe)),
        (0b00111, true) => ("CMHS", Lanes(CmHs)),
        (0b01000, false) => ("SSHL", Lanes(SShl)),
        (0b01000, true) => ("USHL", Lanes(UShl)),
        (0b01001, false) => ("SQSHL", Lanes(SQShl)),
        (0b01001, true) => ("UQSHL", Lanes(UQShl)),
        (0b01010, false) => ("SRSHL", Lanes(SRShl)),
        (0b01010, true) => ("URSHL", Lanes(URShl)),
        (0b01011, false) => ("SQRSHL", Lanes(SQRShl)),
        (0b01011, true) => ("UQRSHL", Lanes(UQRShl)),
        (0b01100, false) => ("SMAX", Lanes(SMax)),
        (0b01100, true) => ("UMAX", Lanes(UMax)),
        (0b01101, false) => ("SMIN", Lanes(SMin)),
        (0b01101, true) => ("UMIN", Lanes(UMin)),
        (0b01110, false) => ("SABD", Lanes(SAbd)),
        (0b01110, true) => ("UABD", Lanes(UAbd)),
        (0b01111, false) => (
            "SABA",
            Accumulate {
                inner: SAbd,
                outer: Add,
            },
        ),
        (0b01111, true) => (
            "UABA",
            Accumulate {
                inner: UAbd,
                outer: Add,
            },
        ),
        (0b10000, false) => ("ADD", Lanes(Add)),
        (0b10000, true) => ("SUB", Lanes(Sub)),
        (0b10001, false) => ("CMTST", Lanes(CmTst)),
        (0b10001, true) => ("CMEQ", Lanes(CmEq)),
        (0b10010, false) => (
            "MLA",
            Accumulate {
                inner: Mul,
                outer: Add,
            },
        ),
        (0b10010, true) => (
            "MLS",
            Accumulate {
                inner: Mul,
                outer: Sub,
            },
        ),
        (0b10011, false) => ("MUL", Lanes(Mul)),
        (0b10011, true) => ("PMUL", Lanes(PMul)),
        (0b10100, false) => ("SMAXP", Pairs(SMax)),
        (0b10100, true) => ("UMAXP", Pairs(UMax)),
        (0b10101, false) => ("SMINP", Pairs(SMin)),
        (0b10101, true) => ("UMINP", Pairs(UMin)),
        (0b10110, false) => ("SQDMULH", Lanes(SQDMulH)),
        (0b10110, true) => ("SQRDMULH", Lanes(SQRDMulH)),
        (0b10111, false) => ("ADDP", Pairs(Add)),
        _ => return None,
    };
    Some((plan.0, plan.1, false))
}

fn logic(b: &mut Builder, word: Word, operation: Logic) {
    let width = word.width();
    let n = b.v(word.rn, width);
    let m = b.v(word.rm, width);
    let d = b.v(word.rd, width);
    let not_m = b.not(width, m);
    // A select of bits is `else ^ ((else ^ then) & condition)`.
    let select = |b: &mut Builder, condition: ScalarId, then: ScalarId, otherwise: ScalarId| {
        let differ = b.binary(width, Binary::Xor, otherwise, then);
        let taken = b.binary(width, Binary::And, differ, condition);
        b.binary(width, Binary::Xor, otherwise, taken)
    };
    let result = match operation {
        Logic::And => b.binary(width, Binary::And, n, m),
        Logic::Bic => b.binary(width, Binary::And, n, not_m),
        Logic::Orr => b.binary(width, Binary::Or, n, m),
        Logic::Orn => b.binary(width, Binary::Or, n, not_m),
        Logic::Eor => b.binary(width, Binary::Xor, n, m),
        Logic::Bsl => select(b, d, n, m),
        Logic::Bit => select(b, m, n, d),
        Logic::Bif => select(b, not_m, n, d),
    };
    b.set_v(word.rd, width, result);
}

/// `BaseSIMDThreeSameVector`, its tied form and `BaseSIMDThreeScalar`.
fn three_same(b: &mut Builder, word: Word) -> Lifted {
    let Some((_, plan, floating)) = three_same_plan(word.u, word.size, bits(word.word, 11, 5))
    else {
        return Lifted::Unsupported;
    };
    let size = if floating {
        2 + (word.size & 1)
    } else {
        word.size
    };
    let code = |op: Op| Code::new(op, size as u8, word.lanes(size));
    let n = b.v(word.rn, 128);
    let m = b.v(word.rm, 128);
    let result = match plan {
        Three::Logic(operation) => {
            logic(b, word, operation);
            return Lifted::Next;
        }
        Three::Lanes(op) => binary(b, code(op), n, m),
        Three::Pairs(op) => binary(b, code(op).pairwise(), n, m),
        Three::Accumulate { inner, outer } => {
            let d = b.v(word.rd, 128);
            let inner = binary(b, code(inner), n, m);
            binary(b, code(outer), d, inner)
        }
        Three::Fused(op) => {
            let d = b.v(word.rd, 128);
            apply(b, code(op), n, m, d)
        }
    };
    b.set_v(word.rd, 128, result);
    Lifted::Next
}

// --- two registers ------------------------------------------------------------------

/// What an instruction of the group "two-register miscellaneous" does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Two {
    Lanes(Op),
    /// With the immediate of the code.
    LanesImm(Op, u16),
    /// The lanes of each container of this many bits in the other order.
    Reverse(u16),
    /// Neighbours added to one lane of twice the size.
    PairLong(Op),
    /// The same, added to `Rd`.
    PairLongAccumulate(Op),
    /// `Rd` is the first operand and `Rn` the second.
    Tied(Op),
    Not,
    /// The bits of each byte in the other order.
    ReverseBits,
    /// A compare with zero. `swapped`: zero is the first operand.
    Zero {
        op: Op,
        swapped: bool,
    },
    /// To lanes of half the size.
    Narrow(Op),
    /// `SHLL`: to lanes of twice the size, shifted left by the lane size.
    ShiftLong,
    /// Floating point to lanes of half and of twice the size.
    FloatNarrow(Op),
    FloatLong,
}

/// The mnemonic and the operation of the fixed bits `U`, `size` and `opcode`
/// (the bits 16 to 12) of the group, and whether its lanes are floating
/// point.
fn two_misc_plan(u: bool, size: u32, opcode: u32) -> Option<(&'static str, Two, bool)> {
    use Op::*;
    use Two::*;
    let to_integer = |op: Op, rounding: ToInteger| {
        LanesImm(op, crate::helpers::simd::to_integer_imm(0, rounding))
    };
    let zero = |op: Op, swapped: bool| Zero { op, swapped };
    let floating = matches!(opcode, 0b01100..=0b01111 | 0b10110 | 0b10111) || opcode >= 0b11000;
    if floating {
        let plan = match (u, size >> 1, opcode) {
            (false, 0, 0b10110) => ("FCVTN", FloatNarrow(FCvtN)),
            (false, 0, 0b10111) => ("FCVTL", FloatLong),
            (true, 0, 0b10110) => ("FCVTXN", FloatNarrow(FCvtXN)),
            (false, 0, 0b11000) => ("FRINTN", Lanes(FRintN)),
            (false, 0, 0b11001) => ("FRINTM", Lanes(FRintM)),
            (false, 0, 0b11010) => ("FCVTNS", to_integer(FCvtS, ToInteger::NearEven)),
            (false, 0, 0b11011) => ("FCVTMS", to_integer(FCvtS, ToInteger::Down)),
            (false, 0, 0b11100) => ("FCVTAS", to_integer(FCvtS, ToInteger::TiesAway)),
            (false, 0, 0b11101) => ("SCVTF", Lanes(SCvtF)),
            (true, 0, 0b11000) => ("FRINTA", Lanes(FRintA)),
            (true, 0, 0b11001) => ("FRINTX", Lanes(FRintX)),
            (true, 0, 0b11010) => ("FCVTNU", to_integer(FCvtU, ToInteger::NearEven)),
            (true, 0, 0b11011) => ("FCVTMU", to_integer(FCvtU, ToInteger::Down)),
            (true, 0, 0b11100) => ("FCVTAU", to_integer(FCvtU, ToInteger::TiesAway)),
            (true, 0, 0b11101) => ("UCVTF", Lanes(UCvtF)),
            (false, 1, 0b01100) => ("FCMGT", zero(FCmGt, false)),
            (false, 1, 0b01101) => ("FCMEQ", zero(FCmEq, false)),
            (false, 1, 0b01110) => ("FCMLT", zero(FCmGt, true)),
            (false, 1, 0b01111) => ("FABS", Lanes(FAbs)),
            (true, 1, 0b01100) => ("FCMGE", zero(FCmGe, false)),
            (true, 1, 0b01101) => ("FCMLE", zero(FCmGe, true)),
            (true, 1, 0b01111) => ("FNEG", Lanes(FNeg)),
            (false, 1, 0b11000) => ("FRINTP", Lanes(FRintP)),
            (false, 1, 0b11001) => ("FRINTZ", Lanes(FRintZ)),
            (false, 1, 0b11010) => ("FCVTPS", to_integer(FCvtS, ToInteger::Up)),
            (false, 1, 0b11011) => ("FCVTZS", to_integer(FCvtS, ToInteger::ToZero)),
            (false, 1, 0b11100) => ("URECPE", Lanes(URecpE)),
            (false, 1, 0b11101) => ("FRECPE", Lanes(FRecpE)),
            (false, 1, 0b11111) => ("FRECPX", Lanes(FRecpX)),
            (true, 1, 0b11001) => ("FRINTI", Lanes(FRintI)),
            (true, 1, 0b11010) => ("FCVTPU", to_integer(FCvtU, ToInteger::Up)),
            (true, 1, 0b11011) => ("FCVTZU", to_integer(FCvtU, ToInteger::ToZero)),
            (true, 1, 0b11100) => ("URSQRTE", Lanes(URSqrtE)),
            (true, 1, 0b11101) => ("FRSQRTE", Lanes(FRSqrtE)),
            (true, 1, 0b11111) => ("FSQRT", Lanes(FSqrt)),
            _ => return None,
        };
        return Some((plan.0, plan.1, true));
    }
    let plan = match (u, opcode) {
        (false, 0b00000) => ("REV64", Reverse(64)),
        (true, 0b00000) => ("REV32", Reverse(32)),
        (false, 0b00001) => ("REV16", Reverse(16)),
        (false, 0b00010) => ("SADDLP", PairLong(SAddLP)),
        (true, 0b00010) => ("UADDLP", PairLong(UAddLP)),
        (false, 0b00011) => ("SUQADD", Tied(SuQAdd)),
        (true, 0b00011) => ("USQADD", Tied(USqAdd)),
        (false, 0b00100) => ("CLS", Lanes(Cls)),
        (true, 0b00100) => ("CLZ", Lanes(Clz)),
        (false, 0b00101) => ("CNT", Lanes(Cnt)),
        (true, 0b00101) if size == 0 => ("NOT", Not),
        (true, 0b00101) => ("RBIT", ReverseBits),
        (false, 0b00110) => ("SADALP", PairLongAccumulate(SAddLP)),
        (true, 0b00110) => ("UADALP", PairLongAccumulate(UAddLP)),
        (false, 0b00111) => ("SQABS", Lanes(SQAbs)),
        (true, 0b00111) => ("SQNEG", Lanes(SQNeg)),
        (false, 0b01000) => ("CMGT", zero(CmGt, false)),
        (true, 0b01000) => ("CMGE", zero(CmGe, false)),
        (false, 0b01001) => ("CMEQ", zero(CmEq, false)),
        (true, 0b01001) => ("CMLE", zero(CmGe, true)),
        (false, 0b01010) => ("CMLT", zero(CmGt, true)),
        (false, 0b01011) => ("ABS", Lanes(Abs)),
        (true, 0b01011) => ("NEG", Lanes(Neg)),
        (false, 0b10010) => ("XTN", Narrow(ShrN)),
        (true, 0b10010) => ("SQXTUN", Narrow(SQShrUN)),
        (true, 0b10011) => ("SHLL", ShiftLong),
        (false, 0b10100) => ("SQXTN", Narrow(SQShrN)),
        (true, 0b10100) => ("UQXTN", Narrow(UQShrN)),
        _ => return None,
    };
    Some((plan.0, plan.1, false))
}

/// The groups "two-register miscellaneous" of vectors and of scalars: twelve
/// format classes.
fn two_misc(b: &mut Builder, word: Word) -> Lifted {
    let Some((_, plan, floating)) = two_misc_plan(word.u, word.size, bits(word.word, 12, 5)) else {
        return Lifted::Unsupported;
    };
    let size = if floating {
        2 + (word.size & 1)
    } else {
        word.size
    };
    let lanes = word.lanes(size);
    let n = b.v(word.rn, 128);
    let result = match plan {
        Two::Lanes(op) => unary(b, Code::new(op, size as u8, lanes), n),
        Two::LanesImm(op, imm) => unary(b, Code::new(op, size as u8, lanes).imm(imm), n),
        Two::Reverse(container) => {
            unary(b, Code::new(Op::Rev, size as u8, lanes).imm(container), n)
        }
        Two::PairLong(op) => unary(b, Code::new(op, size as u8, lanes / 2), n),
        Two::PairLongAccumulate(op) => {
            let d = b.v(word.rd, 128);
            let sums = unary(b, Code::new(op, size as u8, lanes / 2), n);
            binary(b, Code::new(Op::Add, size as u8 + 1, lanes / 2), d, sums)
        }
        Two::Tied(op) => {
            let d = b.v(word.rd, 128);
            binary(b, Code::new(op, size as u8, lanes), d, n)
        }
        Two::Not => {
            let width = word.width();
            let n = b.v(word.rn, width);
            let result = b.not(width, n);
            b.set_v(word.rd, width, result);
            return Lifted::Next;
        }
        // The size field of RBIT is 1, and its lanes are bytes.
        Two::ReverseBits => unary(b, Code::new(Op::RBit, 0, word.lanes(0)), n),
        Two::Zero { op, swapped } => {
            let nothing = zero(b);
            let code = Code::new(op, size as u8, lanes);
            if swapped {
                binary(b, code, nothing, n)
            } else {
                binary(b, code, n, nothing)
            }
        }
        Two::Narrow(op) => {
            let value = unary(b, Code::new(op, size as u8, word.half_lanes(size)), n);
            set_narrow(b, word, value);
            return Lifted::Next;
        }
        Two::ShiftLong => {
            let n = half(b, word, word.rn);
            let code = Code::new(Op::UShlL, size as u8, word.half_lanes(size)).imm(8 << size);
            unary(b, code, n)
        }
        // The size of a code is that of the narrow lanes: 16 or 32 bits.
        Two::FloatNarrow(op) => {
            let narrow = size - 1;
            let value = unary(b, Code::new(op, narrow as u8, word.half_lanes(narrow)), n);
            set_narrow(b, word, value);
            return Lifted::Next;
        }
        Two::FloatLong => {
            let narrow = size - 1;
            let n = half(b, word, word.rn);
            let code = Code::new(Op::FCvtL, narrow as u8, word.half_lanes(narrow));
            unary(b, code, n)
        }
    };
    b.set_v(word.rd, 128, result);
    Lifted::Next
}

// --- every lane to one ----------------------------------------------------------------

/// The mnemonic and the operation of `BaseSIMDAcrossLanes`, from `U`, bit 23
/// and the opcode (the bits 16 to 12), and whether it is floating point.
fn across_plan(u: bool, high: bool, opcode: u32) -> Option<(&'static str, Op, bool)> {
    use Op::*;
    Some(match (opcode, u, high) {
        (0b00011, false, _) => ("SADDLV", SAddLV, false),
        (0b00011, true, _) => ("UADDLV", UAddLV, false),
        (0b01010, false, _) => ("SMAXV", SMaxV, false),
        (0b01010, true, _) => ("UMAXV", UMaxV, false),
        (0b11010, false, _) => ("SMINV", SMinV, false),
        (0b11010, true, _) => ("UMINV", UMinV, false),
        (0b11011, false, _) => ("ADDV", AddV, false),
        (0b01100, true, false) => ("FMAXNMV", FMaxNmV, true),
        (0b01100, true, true) => ("FMINNMV", FMinNmV, true),
        (0b01111, true, false) => ("FMAXV", FMaxV, true),
        (0b01111, true, true) => ("FMINV", FMinV, true),
        _ => return None,
    })
}

fn across_lanes(b: &mut Builder, word: Word) -> Lifted {
    let opcode = bits(word.word, 12, 5);
    let Some((_, op, floating)) = across_plan(word.u, bit(word.word, 23), opcode) else {
        return Lifted::Unsupported;
    };
    let size = if floating {
        2 + (word.size & 1)
    } else {
        word.size
    };
    let lanes = (word.width() / (8 << size)) as u8;
    let n = b.v(word.rn, 128);
    let result = unary(b, Code::new(op, size as u8, lanes), n);
    b.set_v(word.rd, 128, result);
    Lifted::Next
}

/// `BaseSIMDPairwiseScalar`: the two lanes of `Rn` to one.
fn pairwise_plan(u: bool, high: bool, opcode: u32) -> Option<(&'static str, Op, bool)> {
    use Op::*;
    Some(match (opcode, u, high) {
        (0b11011, false, _) => ("ADDP", Add, false),
        (0b01100, true, false) => ("FMAXNMP", FMaxNm, true),
        (0b01100, true, true) => ("FMINNMP", FMinNm, true),
        (0b01101, true, false) => ("FADDP", FAdd, true),
        (0b01111, true, false) => ("FMAXP", FMax, true),
        (0b01111, true, true) => ("FMINP", FMin, true),
        _ => return None,
    })
}

fn pairwise_scalar(b: &mut Builder, word: Word) -> Lifted {
    let opcode = bits(word.word, 12, 5);
    let Some((_, op, floating)) = pairwise_plan(word.u, bit(word.word, 23), opcode) else {
        return Lifted::Unsupported;
    };
    let size = if floating { 2 + (word.size & 1) } else { 3 };
    let n = b.v(word.rn, 128);
    let result = unary(b, Code::new(op, size as u8, 1).pairwise(), n);
    b.set_v(word.rd, 128, result);
    Lifted::Next
}

// --- three registers of two arrangements ------------------------------------------------

/// What an instruction of the group "three different" does, and of the
/// widening part of the group "indexed element".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Different {
    /// Two halves to lanes of twice the size.
    Long(Op),
    /// `Rn` of wide lanes, with the widened half of `Rm`.
    Wide { extend: Op, op: Op },
    /// `Rd outer (Rn inner Rm)`, on wide lanes.
    LongAccumulate { inner: Op, outer: Op },
    /// Wide lanes to the upper halves of their sum or difference.
    Narrow(Op),
}

/// The mnemonic and the operation of the fixed bits `U` and `opcode` (the
/// bits 15 to 12) of the group "three different".
fn three_different_plan(u: bool, opcode: u32) -> Option<(&'static str, Different)> {
    use Different::*;
    use Op::*;
    let accumulate = |inner: Op, outer: Op| LongAccumulate { inner, outer };
    Some(match (opcode, u) {
        (0b0000, false) => ("SADDL", Long(SAddL)),
        (0b0000, true) => ("UADDL", Long(UAddL)),
        (0b0001, false) => (
            "SADDW",
            Wide {
                extend: SShlL,
                op: Add,
            },
        ),
        (0b0001, true) => (
            "UADDW",
            Wide {
                extend: UShlL,
                op: Add,
            },
        ),
        (0b0010, false) => ("SSUBL", Long(SSubL)),
        (0b0010, true) => ("USUBL", Long(USubL)),
        (0b0011, false) => (
            "SSUBW",
            Wide {
                extend: SShlL,
                op: Sub,
            },
        ),
        (0b0011, true) => (
            "USUBW",
            Wide {
                extend: UShlL,
                op: Sub,
            },
        ),
        (0b0100, false) => ("ADDHN", Narrow(AddHn)),
        (0b0100, true) => ("RADDHN", Narrow(RAddHn)),
        (0b0101, false) => ("SABAL", accumulate(SAbdL, Add)),
        (0b0101, true) => ("UABAL", accumulate(UAbdL, Add)),
        (0b0110, false) => ("SUBHN", Narrow(SubHn)),
        (0b0110, true) => ("RSUBHN", Narrow(RSubHn)),
        (0b0111, false) => ("SABDL", Long(SAbdL)),
        (0b0111, true) => ("UABDL", Long(UAbdL)),
        (0b1000, false) => ("SMLAL", accumulate(SMulL, Add)),
        (0b1000, true) => ("UMLAL", accumulate(UMulL, Add)),
        (0b1001, false) => ("SQDMLAL", accumulate(SQDMulL, SQAdd)),
        (0b1010, false) => ("SMLSL", accumulate(SMulL, Sub)),
        (0b1010, true) => ("UMLSL", accumulate(UMulL, Sub)),
        (0b1011, false) => ("SQDMLSL", accumulate(SQDMulL, SQSub)),
        (0b1100, false) => ("SMULL", Long(SMulL)),
        (0b1100, true) => ("UMULL", Long(UMulL)),
        (0b1101, false) => ("SQDMULL", Long(SQDMulL)),
        (0b1110, false) => ("PMULL", Long(PMulL)),
        _ => return None,
    })
}

/// Run a widening plan on the operands `n` and `m`, which are already the
/// halves that the instruction reads. `size` is that of the narrow lanes.
fn different(
    b: &mut Builder,
    word: Word,
    plan: Different,
    size: u32,
    n: ScalarId,
    m: ScalarId,
) -> Lifted {
    let lanes = word.half_lanes(size);
    let narrow = |op: Op| Code::new(op, size as u8, lanes);
    let wide = |op: Op| Code::new(op, size as u8 + 1, lanes);
    let result = match plan {
        Different::Long(op) => binary(b, narrow(op), n, m),
        Different::Wide { extend, op } => {
            let n = b.v(word.rn, 128);
            let extended = unary(b, narrow(extend), m);
            binary(b, wide(op), n, extended)
        }
        Different::LongAccumulate { inner, outer } => {
            let d = b.v(word.rd, 128);
            let inner = binary(b, narrow(inner), n, m);
            binary(b, wide(outer), d, inner)
        }
        Different::Narrow(op) => {
            let n = b.v(word.rn, 128);
            let m = b.v(word.rm, 128);
            let value = binary(b, narrow(op), n, m);
            set_narrow(b, word, value);
            return Lifted::Next;
        }
    };
    b.set_v(word.rd, 128, result);
    Lifted::Next
}

/// `BaseSIMDDifferentThreeVector`, its tied form and
/// `BaseSIMDThreeScalarMixed`.
fn three_different(b: &mut Builder, word: Word) -> Lifted {
    let Some((_, plan)) = three_different_plan(word.u, bits(word.word, 12, 4)) else {
        return Lifted::Unsupported;
    };
    let n = half(b, word, word.rn);
    let m = half(b, word, word.rm);
    different(b, word, plan, word.size, n, m)
}

// --- a vector and one lane of another ------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Indexed {
    Same(Three),
    Different(Different),
}

/// The mnemonic and the operation of the fixed bits `U` and `opcode` (the
/// bits 15 to 12) of the group "indexed element", and whether its lanes are
/// floating point.
fn indexed_plan(u: bool, opcode: u32) -> Option<(&'static str, Indexed, bool)> {
    use Op::*;
    let lanes = |op: Op| Indexed::Same(Three::Lanes(op));
    let multiply = |outer: Op| Indexed::Same(Three::Accumulate { inner: Mul, outer });
    let long = |op: Op| Indexed::Different(Different::Long(op));
    let accumulate =
        |inner: Op, outer: Op| Indexed::Different(Different::LongAccumulate { inner, outer });
    Some(match (opcode, u) {
        (0b0000, true) => ("MLA", multiply(Add), false),
        (0b0100, true) => ("MLS", multiply(Sub), false),
        (0b1000, false) => ("MUL", lanes(Mul), false),
        (0b1100, false) => ("SQDMULH", lanes(SQDMulH), false),
        (0b1101, false) => ("SQRDMULH", lanes(SQRDMulH), false),
        (0b0010, false) => ("SMLAL", accumulate(SMulL, Add), false),
        (0b0010, true) => ("UMLAL", accumulate(UMulL, Add), false),
        (0b0110, false) => ("SMLSL", accumulate(SMulL, Sub), false),
        (0b0110, true) => ("UMLSL", accumulate(UMulL, Sub), false),
        (0b1010, false) => ("SMULL", long(SMulL), false),
        (0b1010, true) => ("UMULL", long(UMulL), false),
        (0b0011, false) => ("SQDMLAL", accumulate(SQDMulL, SQAdd), false),
        (0b0111, false) => ("SQDMLSL", accumulate(SQDMulL, SQSub), false),
        (0b1011, false) => ("SQDMULL", long(SQDMulL), false),
        (0b0001, false) => ("FMLA", Indexed::Same(Three::Fused(FMla)), true),
        (0b0101, false) => ("FMLS", Indexed::Same(Three::Fused(FMls)), true),
        (0b1001, false) => ("FMUL", lanes(FMul), true),
        (0b1001, true) => ("FMULX", lanes(FMulX), true),
        _ => return None,
    })
}

/// `BaseSIMDIndexed` and its tied form: `Rn` with one lane of `Rm`.
fn indexed(b: &mut Builder, word: Word) -> Lifted {
    let Some((_, plan, floating)) = indexed_plan(word.u, bits(word.word, 12, 4)) else {
        return Lifted::Unsupported;
    };
    let (h, l, m) = (
        u32::from(bit(word.word, 11)),
        u32::from(bit(word.word, 21)),
        u32::from(bit(word.word, 20)),
    );
    // The index takes the bits that the register number does not need.
    let (size, index, rm) = if floating {
        if word.size & 1 == 0 {
            (2, h << 1 | l, word.rm)
        } else {
            (3, h, word.rm)
        }
    } else if word.size == 1 {
        (1, h << 2 | l << 1 | m, word.rm & 15)
    } else {
        (2, h << 1 | l, word.rm)
    };
    let width = 8u16 << size;
    let position = index as u16 * width;
    if position + width > 128 {
        return Lifted::Unsupported;
    }
    let register = b.v(rm, 128);
    let element = b.slice(register, position, width);
    let every = replicate(b, element, width, 128);
    match plan {
        Indexed::Different(plan) => {
            let n = half(b, word, word.rn);
            different(b, word, plan, size, n, every)
        }
        Indexed::Same(plan) => {
            let code = |op: Op| Code::new(op, size as u8, word.lanes(size));
            let n = b.v(word.rn, 128);
            let result = match plan {
                Three::Lanes(op) => binary(b, code(op), n, every),
                Three::Accumulate { inner, outer } => {
                    let d = b.v(word.rd, 128);
                    let inner = binary(b, code(inner), n, every);
                    binary(b, code(outer), d, inner)
                }
                Three::Fused(op) => {
                    let d = b.v(word.rd, 128);
                    apply(b, code(op), n, every, d)
                }
                Three::Pairs(_) | Three::Logic(_) => return Lifted::Unsupported,
            };
            b.set_v(word.rd, 128, result);
            Lifted::Next
        }
    }
}

// --- shifts by an immediate ------------------------------------------------------------------

/// What an instruction of the group "shift by immediate" does. The amount
/// of a right shift counts down from twice the lane size, that of a left
/// shift up from the lane size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShiftBy {
    /// A shift right by the register form of the operation.
    Right(Op),
    /// The same, added to `Rd`.
    RightAccumulate(Op),
    Left(Op),
    /// `SRI` and `SLI`: the other bits of `Rd` stay.
    RightInsert,
    LeftInsert,
    /// Wide lanes shifted right to lanes of the size.
    Narrow(Op),
    /// Lanes shifted left to lanes of twice the size.
    Long(Op),
    /// A fixed-point value to floating point, and back toward zero.
    FromFixed(Op),
    ToFixed(Op),
}

/// The mnemonic and the operation of the fixed bits `U` and `opcode` (the
/// bits 15 to 11) of the group.
fn shift_plan(u: bool, opcode: u32) -> Option<(&'static str, ShiftBy)> {
    use Op::*;
    use ShiftBy::*;
    Some(match (opcode, u) {
        (0b00000, false) => ("SSHR", Right(SShl)),
        (0b00000, true) => ("USHR", Right(UShl)),
        (0b00010, false) => ("SSRA", RightAccumulate(SShl)),
        (0b00010, true) => ("USRA", RightAccumulate(UShl)),
        (0b00100, false) => ("SRSHR", Right(SRShl)),
        (0b00100, true) => ("URSHR", Right(URShl)),
        (0b00110, false) => ("SRSRA", RightAccumulate(SRShl)),
        (0b00110, true) => ("URSRA", RightAccumulate(URShl)),
        (0b01000, true) => ("SRI", RightInsert),
        (0b01010, false) => ("SHL", Left(UShl)),
        (0b01010, true) => ("SLI", LeftInsert),
        (0b01100, true) => ("SQSHLU", Left(SQShlU)),
        (0b01110, false) => ("SQSHL", Left(SQShl)),
        (0b01110, true) => ("UQSHL", Left(UQShl)),
        (0b10000, false) => ("SHRN", Narrow(ShrN)),
        (0b10000, true) => ("SQSHRUN", Narrow(SQShrUN)),
        (0b10001, false) => ("RSHRN", Narrow(RShrN)),
        (0b10001, true) => ("SQRSHRUN", Narrow(SQRShrUN)),
        (0b10010, false) => ("SQSHRN", Narrow(SQShrN)),
        (0b10010, true) => ("UQSHRN", Narrow(UQShrN)),
        (0b10011, false) => ("SQRSHRN", Narrow(SQRShrN)),
        (0b10011, true) => ("UQRSHRN", Narrow(UQRShrN)),
        (0b10100, false) => ("SSHLL", Long(SShlL)),
        (0b10100, true) => ("USHLL", Long(UShlL)),
        (0b11100, false) => ("SCVTF", FromFixed(SCvtF)),
        (0b11100, true) => ("UCVTF", FromFixed(UCvtF)),
        (0b11111, false) => ("FCVTZS", ToFixed(FCvtS)),
        (0b11111, true) => ("FCVTZU", ToFixed(FCvtU)),
        _ => return None,
    })
}

/// `BaseSIMDVectorShift`, `BaseSIMDScalarShift` and their tied forms.
fn shift_immediate(b: &mut Builder, word: Word) -> Lifted {
    let Some((_, plan)) = shift_plan(word.u, bits(word.word, 11, 5)) else {
        return Lifted::Unsupported;
    };
    // The highest set bit of immh names the lane size.
    let immh = bits(word.word, 19, 4);
    if immh == 0 {
        return Lifted::Unsupported;
    }
    let size = 31 - immh.leading_zeros();
    let lane = 8i32 << size;
    let amount = bits(word.word, 16, 7) as i32;
    let (right, left) = (2 * lane - amount, amount - lane);
    let code = |op: Op| Code::new(op, size as u8, word.lanes(size));
    let n = b.v(word.rn, 128);
    let result = match plan {
        ShiftBy::Right(op) => {
            let by = splat(b, -right);
            binary(b, code(op), n, by)
        }
        ShiftBy::RightAccumulate(op) => {
            let d = b.v(word.rd, 128);
            let by = splat(b, -right);
            let shifted = binary(b, code(op), n, by);
            binary(b, code(Op::Add), d, shifted)
        }
        ShiftBy::Left(op) => {
            let by = splat(b, left);
            binary(b, code(op), n, by)
        }
        ShiftBy::RightInsert => {
            let d = b.v(word.rd, 128);
            let nothing = zero(b);
            apply(b, code(Op::Sri).imm(right as u16), n, nothing, d)
        }
        ShiftBy::LeftInsert => {
            let d = b.v(word.rd, 128);
            let nothing = zero(b);
            apply(b, code(Op::Sli).imm(left as u16), n, nothing, d)
        }
        ShiftBy::Narrow(op) => {
            let code = Code::new(op, size as u8, word.half_lanes(size)).imm(right as u16);
            let value = unary(b, code, n);
            set_narrow(b, word, value);
            return Lifted::Next;
        }
        ShiftBy::Long(op) => {
            let n = half(b, word, word.rn);
            let code = Code::new(op, size as u8, word.half_lanes(size)).imm(left as u16);
            unary(b, code, n)
        }
        ShiftBy::FromFixed(op) => unary(b, code(op).imm(right as u16), n),
        ShiftBy::ToFixed(op) => {
            let imm = crate::helpers::simd::to_integer_imm(right as u32, ToInteger::ToZero);
            unary(b, code(op).imm(imm), n)
        }
    };
    b.set_v(word.rd, 128, result);
    Lifted::Next
}

// --- lanes moved -----------------------------------------------------------------------------

/// The lane size and the lane number that `imm5` (the bits 20 to 16) names:
/// the lowest set bit is the size, and the bits above it the number.
fn lane_of(word: u32) -> Option<(u32, u32)> {
    let imm5 = bits(word, 16, 5);
    let size = imm5.trailing_zeros();
    (size <= 3).then(|| (size, imm5 >> (size + 1)))
}

/// The copies: `DUP`, `INS`, `SMOV`, `UMOV` and the scalar `MOV` of a lane.
fn copy(b: &mut Builder, record: Record, word: Word) -> Lifted {
    let Some((size, index)) = lane_of(word.word) else {
        return Lifted::Unsupported;
    };
    let width = 8u16 << size;
    let position = index as u16 * width;
    let element = |b: &mut Builder| {
        let register = b.v(word.rn, 128);
        b.slice(register, position, width)
    };
    let general = |b: &mut Builder| {
        let value = b.x(word.rn, 64);
        b.low(value, width)
    };
    match record.info().format {
        FormatClass::SIMDDupFromMain => {
            let value = general(b);
            let result = replicate(b, value, width, word.width());
            b.set_v(word.rd, 128, result);
        }
        FormatClass::SIMDDup8FromElement
        | FormatClass::SIMDDup16FromElement
        | FormatClass::SIMDDup32FromElement
        | FormatClass::SIMDDup64FromElement => {
            let value = element(b);
            let result = replicate(b, value, width, word.width());
            b.set_v(word.rd, 128, result);
        }
        FormatClass::BaseSIMDScalarDUP => {
            let value = element(b);
            b.set_v(word.rd, width, value);
        }
        FormatClass::SIMDInsFromMain => {
            let value = general(b);
            let vector = b.v(word.rd, 128);
            let result = insert(b, vector, position, width, value);
            b.set_v(word.rd, 128, result);
        }
        FormatClass::SIMDInsFromElement => {
            // The source lane is in the bits 14 to 11, above its unused bits.
            let source = (bits(word.word, 11, 4) >> size) as u16 * width;
            let register = b.v(word.rn, 128);
            let value = b.slice(register, source, width);
            let vector = b.v(word.rd, 128);
            let result = insert(b, vector, position, width, value);
            b.set_v(word.rd, 128, result);
        }
        FormatClass::SIMDSMov => {
            let value = element(b);
            let to = if word.q { 64 } else { 32 };
            let result = b.sign_extend(value, width, to);
            b.set_x(word.rd, to, result);
        }
        FormatClass::SIMDUMov => {
            let value = element(b);
            let to = if word.q { 64 } else { 32 };
            let result = b.zero_extend(value, to);
            b.set_x(word.rd, to, result);
        }
        _ => return Lifted::Unsupported,
    }
    Lifted::Next
}

/// `BaseSIMDZipVector`: the mnemonic and the operation of the bits 14 to 12.
fn zip_plan(opcode: u32) -> Option<(&'static str, Op)> {
    Some(match opcode {
        0b001 => ("UZP1", Op::Uzp1),
        0b010 => ("TRN1", Op::Trn1),
        0b011 => ("ZIP1", Op::Zip1),
        0b101 => ("UZP2", Op::Uzp2),
        0b110 => ("TRN2", Op::Trn2),
        0b111 => ("ZIP2", Op::Zip2),
        _ => return None,
    })
}

fn zip(b: &mut Builder, word: Word) -> Lifted {
    let Some((_, op)) = zip_plan(bits(word.word, 12, 3)) else {
        return Lifted::Unsupported;
    };
    let n = b.v(word.rn, 128);
    let m = b.v(word.rm, 128);
    let code = Code::new(op, word.size as u8, (word.width() / (8 << word.size)) as u8);
    let result = binary(b, code, n, m);
    b.set_v(word.rd, 128, result);
    Lifted::Next
}

/// `EXT`: the bytes of `Rm:Rn` from the byte that the bits 14 to 11 name.
fn extract(b: &mut Builder, word: Word) -> Lifted {
    let n = b.v(word.rn, 128);
    let m = b.v(word.rm, 128);
    let bytes = (word.width() / 8) as u8;
    let code = Code::new(Op::Ext, 0, bytes).imm(bits(word.word, 11, 4) as u16);
    let result = binary(b, code, n, m);
    b.set_v(word.rd, 128, result);
    Lifted::Next
}

/// `TBL` and `TBX`: a table of one to four registers from `Rn` on.
fn table(b: &mut Builder, word: Word) -> Lifted {
    let registers = bits(word.word, 13, 2) + 1;
    let extension = bit(word.word, 12);
    let bytes = u32::from(word.width() / 8);
    let mut args = vec![b.imm(32, table_code(registers, bytes, extension))];
    args.push(b.v(word.rd, 128));
    args.push(b.v(word.rm, 128));
    for index in 0..4 {
        let register = if index < registers {
            b.v((word.rn + index) % 32, 128)
        } else {
            zero(b)
        };
        args.push(register);
    }
    let result = b.call(128, &SIMD_TABLE, &args);
    b.set_v(word.rd, 128, result);
    Lifted::Next
}

// --- immediates ------------------------------------------------------------------------------

/// What a modified immediate does with its 64 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Immediate {
    /// `MOVI`, `MVNI` and `FMOV`: the value, in each half of the register.
    Move(u64),
    Orr(u64),
    Bic(u64),
}

/// The 64 bits of a modified immediate, from `op` (bit 29), `cmode` (the
/// bits 15 to 12) and the eight bits of the immediate.
pub fn modified_immediate(op: bool, cmode: u32, imm8: u32) -> Option<Immediate> {
    let imm8 = u64::from(imm8 & 0xff);
    let twice = |value: u64| value | value << 32;
    let four = |value: u64| value * 0x0001_0001_0001_0001;
    let (value, logical) = match cmode {
        // 32-bit lanes, the immediate shifted left by 0, 8, 16 or 24.
        0b0000..=0b0111 => (twice(imm8 << (8 * (cmode >> 1))), cmode & 1 != 0),
        // 16-bit lanes, shifted left by 0 or 8.
        0b1000..=0b1011 => (four(imm8 << (8 * ((cmode >> 1) & 1))), cmode & 1 != 0),
        // 32-bit lanes, shifted left with ones coming in.
        0b1100 => (twice(imm8 << 8 | 0xff), false),
        0b1101 => (twice(imm8 << 16 | 0xffff), false),
        0b1110 if !op => (imm8 * 0x0101_0101_0101_0101, false),
        // Each bit of the immediate is one byte of the value.
        0b1110 => {
            let mut value = 0;
            for index in 0..8 {
                if imm8 >> index & 1 != 0 {
                    value |= 0xff << (8 * index);
                }
            }
            return Some(Immediate::Move(value));
        }
        _ if !op => {
            return Some(Immediate::Move(twice(fp::expand_immediate(
                SINGLE,
                imm8 as u32,
            ))));
        }
        _ => return Some(Immediate::Move(fp::expand_immediate(DOUBLE, imm8 as u32))),
    };
    Some(match (logical, op) {
        (false, false) => Immediate::Move(value),
        (false, true) => Immediate::Move(!value),
        (true, false) => Immediate::Orr(value),
        (true, true) => Immediate::Bic(value),
    })
}

/// The seven format classes of the modified immediates.
fn immediate(b: &mut Builder, word: Word) -> Lifted {
    let imm8 = bits(word.word, 16, 3) << 5 | bits(word.word, 5, 5);
    let Some(plan) = modified_immediate(word.u, bits(word.word, 12, 4), imm8) else {
        return Lifted::Unsupported;
    };
    let width = word.width();
    let value = |b: &mut Builder, bits: u64| {
        let half = b.imm(64, bits);
        if width == 128 {
            b.concat(half, half, 64, 128)
        } else {
            half
        }
    };
    let result = match plan {
        Immediate::Move(bits) => value(b, bits),
        Immediate::Orr(bits) => {
            let d = b.v(word.rd, width);
            let value = value(b, bits);
            b.binary(width, Binary::Or, d, value)
        }
        Immediate::Bic(bits) => {
            let d = b.v(word.rd, width);
            let value = value(b, !bits);
            b.binary(width, Binary::And, d, value)
        }
    };
    b.set_v(word.rd, width, result);
    Lifted::Next
}

/// Lift the AdvSIMD instruction `word` of `record`.
pub fn lift(b: &mut Builder, record: Record, word: u32) -> Lifted {
    use FormatClass::*;
    let fields = Word::new(word);
    match record.info().format {
        BaseSIMDThreeSameVector | BaseSIMDThreeSameVectorTied | BaseSIMDThreeScalar => {
            three_same(b, fields)
        }
        BaseSIMDTwoSameVector
        | BaseSIMDTwoSameVectorTied
        | BaseSIMDTwoScalar
        | BaseSIMDTwoScalarTied
        | BaseSIMDCmpTwoVector
        | BaseSIMDCmpTwoScalar
        | BaseSIMDMixedTwoVector
        | BaseSIMDMixedTwoVectorTied
        | BaseSIMDFPCvtTwoVector
        | BaseSIMDFPCvtTwoVectorTied
        | SIMDInexactCvtTwoScalar
        | BaseSIMDVectorLShiftLongBySize => two_misc(b, fields),
        BaseSIMDAcrossLanes => across_lanes(b, fields),
        BaseSIMDPairwiseScalar => pairwise_scalar(b, fields),
        BaseSIMDDifferentThreeVector
        | BaseSIMDDifferentThreeVectorTied
        | BaseSIMDThreeScalarMixed => three_different(b, fields),
        BaseSIMDIndexed | BaseSIMDIndexedTied => indexed(b, fields),
        BaseSIMDVectorShift
        | BaseSIMDVectorShiftTied
        | BaseSIMDScalarShift
        | BaseSIMDScalarShiftTied => shift_immediate(b, fields),
        SIMDDupFromMain | SIMDDup8FromElement | SIMDDup16FromElement | SIMDDup32FromElement
        | SIMDDup64FromElement | BaseSIMDScalarDUP | SIMDInsFromMain | SIMDInsFromElement
        | SIMDSMov | SIMDUMov => copy(b, record, fields),
        BaseSIMDZipVector => zip(b, fields),
        BaseSIMDBitwiseExtract => extract(b, fields),
        BaseSIMDTableLookup | BaseSIMDTableLookupTied => table(b, fields),
        BaseSIMDModifiedImmVectorShift
        | BaseSIMDModifiedImmVectorShiftHalf
        | BaseSIMDModifiedImmVectorShiftHalfTied
        | BaseSIMDModifiedImmVectorShiftTied
        | SIMDModifiedImmMoveMSL
        | SIMDModifiedImmScalarNoShift
        | SIMDModifiedImmVectorNoShift => immediate(b, fields),
        _ => Lifted::Unsupported,
    }
}

/// The mnemonic that the plan of `record` names, for the test that holds
/// the plans against the names of the LLVM records. `None`: the class has no
/// plan, or the fixed bits of the record have none.
pub fn planned_mnemonic(record: Record) -> Option<&'static str> {
    use FormatClass::*;
    let word = Word::new(record.info().value);
    let w = word.word;
    match record.info().format {
        BaseSIMDThreeSameVector | BaseSIMDThreeSameVectorTied | BaseSIMDThreeScalar => {
            three_same_plan(word.u, word.size, bits(w, 11, 5)).map(|plan| plan.0)
        }
        BaseSIMDTwoSameVector
        | BaseSIMDTwoSameVectorTied
        | BaseSIMDTwoScalar
        | BaseSIMDTwoScalarTied
        | BaseSIMDCmpTwoVector
        | BaseSIMDCmpTwoScalar
        | BaseSIMDMixedTwoVector
        | BaseSIMDMixedTwoVectorTied
        | BaseSIMDFPCvtTwoVector
        | BaseSIMDFPCvtTwoVectorTied
        | SIMDInexactCvtTwoScalar
        | BaseSIMDVectorLShiftLongBySize => {
            two_misc_plan(word.u, word.size, bits(w, 12, 5)).map(|plan| plan.0)
        }
        BaseSIMDAcrossLanes => across_plan(word.u, bit(w, 23), bits(w, 12, 5)).map(|plan| plan.0),
        BaseSIMDPairwiseScalar => {
            pairwise_plan(word.u, bit(w, 23), bits(w, 12, 5)).map(|plan| plan.0)
        }
        BaseSIMDDifferentThreeVector
        | BaseSIMDDifferentThreeVectorTied
        | BaseSIMDThreeScalarMixed => {
            three_different_plan(word.u, bits(w, 12, 4)).map(|plan| plan.0)
        }
        BaseSIMDIndexed | BaseSIMDIndexedTied => {
            indexed_plan(word.u, bits(w, 12, 4)).map(|plan| plan.0)
        }
        BaseSIMDVectorShift
        | BaseSIMDVectorShiftTied
        | BaseSIMDScalarShift
        | BaseSIMDScalarShiftTied => shift_plan(word.u, bits(w, 11, 5)).map(|plan| plan.0),
        BaseSIMDZipVector => zip_plan(bits(w, 12, 3)).map(|plan| plan.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode;
    use crate::lift::{CRYPTO_FORMATS, SIMD_FORMATS, lift as lift_any};
    use crate::oracle::stream::{Generator, Kind, records_of};

    /// The mnemonic in the name of an LLVM record of AdvSIMD: the name up to
    /// its arrangement (`v16i8`, `i16`), its width (`32`) or its register
    /// letter (`d`).
    fn mnemonic_of(name: &str) -> &str {
        let bytes = name.as_bytes();
        for letter in *b"vi" {
            for index in 0..bytes.len().saturating_sub(1) {
                if bytes[index] == letter && bytes[index + 1].is_ascii_digit() {
                    return &name[..index];
                }
            }
        }
        let digits = name.trim_end_matches(|c: char| c.is_ascii_digit());
        if digits.len() != name.len() {
            return digits;
        }
        name.trim_end_matches(|c: char| c.is_ascii_lowercase())
    }

    #[test]
    fn a_mnemonic_is_the_name_without_its_arrangement() {
        for (name, mnemonic) in [
            ("ADDv16i8", "ADD"),
            ("REV16v16i8", "REV16"),
            ("CMEQv16i8rz", "CMEQ"),
            ("FABD32", "FABD"),
            ("SQDMLALi16", "SQDMLAL"),
            ("FCVTZSd", "FCVTZS"),
            ("SADDLVv16i8v", "SADDLV"),
            ("SQDMULHv1i16_indexed", "SQDMULH"),
            ("ADDHNv2i64_v4i32", "ADDHN"),
        ] {
            assert_eq!(mnemonic_of(name), mnemonic);
        }
    }

    #[test]
    fn the_fixed_bits_of_a_record_say_what_its_name_says() {
        // The classes without a plan move lanes or are immediates.
        let unplanned = |format: FormatClass| {
            let name = format.name();
            [
                "SIMDDup",
                "SIMDIns",
                "SIMDSMov",
                "SIMDUMov",
                "SIMDModifiedImm",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
                || [
                    "BaseSIMDScalarDUP",
                    "BaseSIMDBitwiseExtract",
                    "BaseSIMDTableLookup",
                    "BaseSIMDTableLookupTied",
                ]
                .contains(&name)
                || name.starts_with("BaseSIMDModifiedImm")
        };
        let (mut checked, mut classes) = (0, 0);
        for format in SIMD_FORMATS {
            if unplanned(format) {
                continue;
            }
            classes += 1;
            for record in records_of(format) {
                let planned = planned_mnemonic(record);
                assert_eq!(
                    planned,
                    Some(mnemonic_of(record.name())),
                    "{}",
                    record.name()
                );
                checked += 1;
            }
        }
        assert_eq!((classes, checked), (27, 1323));
    }

    #[test]
    fn every_record_of_advsimd_and_crypto_is_generated_and_has_a_lifter() {
        let mut generator = Generator::new(5);
        generator.unconstrained_one_in = 0;
        let context = crate::State::reset(0).context;
        let mut records = 0;
        for format in SIMD_FORMATS.iter().chain(&CRYPTO_FORMATS) {
            for record in records_of(*format) {
                records += 1;
                for _ in 0..50 {
                    let (word, kind) =
                        generator.instruction(record, 0x4200_0000, (0x4200_0000, 64));
                    assert_eq!(kind, Kind::Valid, "{}", record.name());
                    let decoded = decode(word).unwrap();
                    assert_eq!(decoded.record, record);
                    // The fields of the class give the word back.
                    let mut builder = Builder::new(context, 0x4200_0000);
                    builder.begin(0x4200_0000);
                    assert_eq!(
                        lift_any(&mut builder, &decoded),
                        Lifted::Next,
                        "{} {word:08x}",
                        record.name()
                    );
                }
            }
        }
        assert_eq!(records, 1426);
    }

    #[test]
    fn the_modified_immediates() {
        use Immediate::*;
        assert_eq!(
            modified_immediate(false, 0b0000, 0xab),
            Some(Move(0x0000_00ab_0000_00ab))
        );
        assert_eq!(
            modified_immediate(false, 0b0110, 0xab),
            Some(Move(0xab00_0000_ab00_0000))
        );
        assert_eq!(
            modified_immediate(true, 0b0010, 0xab),
            Some(Move(!0x0000_ab00_0000_ab00))
        );
        assert_eq!(
            modified_immediate(false, 0b0011, 0xab),
            Some(Orr(0x0000_ab00_0000_ab00))
        );
        assert_eq!(
            modified_immediate(true, 0b1011, 0xab),
            Some(Bic(0xab00_ab00_ab00_ab00))
        );
        assert_eq!(
            modified_immediate(false, 0b1000, 0xab),
            Some(Move(0x00ab_00ab_00ab_00ab))
        );
        assert_eq!(
            modified_immediate(false, 0b1100, 0xab),
            Some(Move(0x0000_abff_0000_abff))
        );
        assert_eq!(
            modified_immediate(true, 0b1101, 0xab),
            Some(Move(!0x00ab_ffff_00ab_ffff))
        );
        assert_eq!(
            modified_immediate(false, 0b1110, 0xab),
            Some(Move(0xabab_abab_abab_abab))
        );
        assert_eq!(
            modified_immediate(true, 0b1110, 0xa5),
            Some(Move(0xff00_ff00_00ff_00ff))
        );
        // 1.0 in single and in double precision.
        assert_eq!(
            modified_immediate(false, 0b1111, 0x70),
            Some(Move(0x3f80_0000_3f80_0000))
        );
        assert_eq!(
            modified_immediate(true, 0b1111, 0x70),
            Some(Move(0x3ff0_0000_0000_0000))
        );
    }
}
