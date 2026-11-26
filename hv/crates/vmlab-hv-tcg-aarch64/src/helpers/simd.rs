//! The lane helpers of AdvSIMD (DESIGN.md G3d, card C7): one function,
//! `eval`, that runs an operation over the lanes of up to three 128-bit
//! values. A lifter passes the operation, the lane size and the lane count
//! as one constant (`Code`) and does not unroll lanes into the graph.
//!
//! `eval` is pure: FPCR is an argument, and the bits for FPSR come back
//! beside the value, in the positions of FPSR, with the saturation bit QC
//! moved down to `QC`. Three pure helpers call it: `SIMD` gives the value,
//! `SIMD_FLAGS` the bits, and `SIMD_LOW` both for a result of 64 bits or
//! fewer.
//!
//! The size of a code is that of the narrow lanes: a widening operation
//! reads lanes of that size and writes lanes of twice the size, and a
//! narrowing operation the other way round. An operand that a "2" form takes
//! from the upper half of a register is moved down by the lifter.
//!
//! Sources read (DESIGN.md "Sources"): S1, the LLVM tables for the names of
//! the operations. S3, dynarmic's A64 translator (`simd_*.cpp`, 0BSD) for
//! which operand is which, where the lockstep did not say it sooner. S6, the
//! lockstep against QEMU, for the behaviour. Not read: QEMU's source, Arm's
//! ASL or pseudocode (X1, X2).

use super::fp::vector::{self as fpv, Mask};
use super::fp::{self, Binary as FpBinary, DOUBLE, Format, HALF, SINGLE, Unary as FpUnary};
use super::softfloat::Rounding;
use vmlab_hv_tcg_ir::native::{Effects, Helper, HelperFn};

/// The saturation bit among the bits that `eval` gives: FPSR.QC is bit 27,
/// and the bits 5 and 6 of FPSR are free.
pub const QC: u8 = 1 << 5;

macro_rules! operations {
    ($($name:ident),* $(,)?) => {
        /// One operation on lanes.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u16)]
        pub enum Op { $($name),* }

        impl Op {
            pub const ALL: &'static [Op] = &[$(Op::$name),*];

            pub fn name(self) -> &'static str {
                match self { $(Op::$name => stringify!($name)),* }
            }
        }
    };
}

operations! {
    // Lanes of one size: `x` of the first operand, `y` of the second.
    Add, Sub, Mul, PMul, CmEq, CmGe, CmGt, CmHi, CmHs, CmTst,
    SHAdd, UHAdd, SRHAdd, URHAdd, SHSub, UHSub,
    SQAdd, UQAdd, SQSub, UQSub, SMax, UMax, SMin, UMin, SAbd, UAbd,
    // A shift by the signed low byte of `y`: left, or right when negative.
    SShl, UShl, SRShl, URShl, SQShl, UQShl, SQRShl, UQRShl, SQShlU,
    SQDMulH, SQRDMulH,
    // `x` is the accumulator and is signed, `y` is unsigned, and the other
    // way round.
    SuQAdd, USqAdd,
    // A shift of `x` by the immediate that keeps the other bits of `z`.
    Sli, Sri,
    Abs, Neg, SQAbs, SQNeg, Cls, Clz, Cnt, RBit, URecpE, URSqrtE,
    // From lanes of the size to lanes of twice the size.
    SAddL, UAddL, SSubL, USubL, SAbdL, UAbdL, SMulL, UMulL, SQDMulL, PMulL,
    SShlL, UShlL, FCvtL,
    // From lanes of twice the size to lanes of the size.
    AddHn, SubHn, RAddHn, RSubHn,
    ShrN, RShrN, SQShrN, UQShrN, SQRShrN, UQRShrN, SQShrUN, SQRShrUN,
    FCvtN, FCvtXN,
    // Two neighbours of the size to one lane of twice the size.
    SAddLP, UAddLP,
    // Every lane of the first operand to one lane.
    AddV, SAddLV, UAddLV, SMaxV, SMinV, UMaxV, UMinV,
    FMaxV, FMinV, FMaxNmV, FMinNmV,
    // Lanes moved.
    Zip1, Zip2, Uzp1, Uzp2, Trn1, Trn2, Ext, Rev, Dup,
    // Floating point, lanes of 32 or 64 bits.
    FAdd, FSub, FMul, FDiv, FMax, FMin, FMaxNm, FMinNm, FMulX, FAbd,
    FRecpS, FRSqrtS, FCmEq, FCmGe, FCmGt, FAcGe, FAcGt,
    // `z + x * y` and `z - x * y`, rounded once.
    FMla, FMls,
    FAbs, FNeg, FSqrt, FRintN, FRintP, FRintM, FRintZ, FRintA, FRintX, FRintI,
    // To and from a fixed-point integer. The immediate holds the count of
    // fraction bits, and for the first two the rounding above it.
    FCvtS, FCvtU, SCvtF, UCvtF,
    FRecpE, FRSqrtE, FRecpX,
}

/// What the lanes of an operation are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Same,
    Long,
    Narrow,
    PairLong,
    Reduce,
    Whole,
}

impl Op {
    fn shape(self) -> Shape {
        use Op::*;
        match self {
            SAddL | UAddL | SSubL | USubL | SAbdL | UAbdL | SMulL | UMulL | SQDMulL | PMulL
            | SShlL | UShlL | FCvtL => Shape::Long,
            AddHn | SubHn | RAddHn | RSubHn | ShrN | RShrN | SQShrN | UQShrN | SQRShrN
            | UQRShrN | SQShrUN | SQRShrUN | FCvtN | FCvtXN => Shape::Narrow,
            SAddLP | UAddLP => Shape::PairLong,
            AddV | SAddLV | UAddLV | SMaxV | SMinV | UMaxV | UMinV | FMaxV | FMinV | FMaxNmV
            | FMinNmV => Shape::Reduce,
            Zip1 | Zip2 | Uzp1 | Uzp2 | Trn1 | Trn2 | Ext | Rev | Dup => Shape::Whole,
            _ => Shape::Same,
        }
    }

    /// Whether the operation can set a bit of FPSR.
    pub fn raises(self) -> bool {
        use Op::*;
        match self {
            SQAdd | UQAdd | SQSub | UQSub | SQShl | UQShl | SQRShl | UQRShl | SQShlU | SQDMulH
            | SQRDMulH | SuQAdd | USqAdd | SQAbs | SQNeg | SQDMulL | SQShrN | UQShrN | SQRShrN
            | UQRShrN | SQShrUN | SQRShrUN => true,
            FAbs | FNeg => false,
            other => other.is_fp(),
        }
    }

    fn is_fp(self) -> bool {
        use Op::*;
        matches!(
            self,
            FCvtL | FCvtN | FCvtXN | FMaxV | FMinV | FMaxNmV | FMinNmV
        ) || self as u16 >= FAdd as u16
    }
}

/// An operation with its lanes, as the constant that a lifter passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Code {
    pub op: Op,
    /// The lane size as the power of two of its bytes: 0 to 3.
    pub size: u8,
    /// The count of lanes of the result. For an operation that takes every
    /// lane to one, the count of lanes that it reads.
    pub lanes: u8,
    /// The operands are neighbours: lane `i` of the result comes from the
    /// lanes `2i` and `2i + 1` of the first operand followed by the second.
    pub pairwise: bool,
    /// A shift amount, an index or a count of fraction bits.
    pub imm: u16,
}

impl Code {
    pub const fn new(op: Op, size: u8, lanes: u8) -> Self {
        Self {
            op,
            size,
            lanes,
            pairwise: false,
            imm: 0,
        }
    }

    pub const fn imm(self, imm: u16) -> Self {
        Self { imm, ..self }
    }

    pub const fn pairwise(self) -> Self {
        Self {
            pairwise: true,
            ..self
        }
    }

    pub const fn encode(self) -> u64 {
        self.op as u64
            | (self.size as u64) << 16
            | (self.lanes as u64) << 18
            | (self.pairwise as u64) << 23
            | (self.imm as u64) << 24
    }

    pub fn decode(code: u64) -> Self {
        Self {
            op: Op::ALL[(code & 0xffff) as usize % Op::ALL.len()],
            size: (code >> 16) as u8 & 3,
            lanes: (code >> 18) as u8 & 31,
            pairwise: code & (1 << 23) != 0,
            imm: (code >> 24) as u16,
        }
    }

    /// The bits of the result that are not zero by construction.
    pub fn result_bits(self) -> u32 {
        let lane = 8u32 << self.size;
        match self.op.shape() {
            Shape::Same | Shape::Narrow | Shape::Whole => lane * u32::from(self.lanes),
            Shape::Long | Shape::PairLong => 2 * lane * u32::from(self.lanes),
            Shape::Reduce => 2 * lane,
        }
    }
}

const fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1 << bits) - 1
    }
}

fn lane(value: u128, bits: u32, index: u32) -> u64 {
    if index * bits >= 128 {
        return 0;
    }
    (value >> (index * bits)) as u64 & mask(bits)
}

fn put(result: &mut u128, bits: u32, index: u32, value: u64) {
    if index * bits < 128 {
        *result |= u128::from(value & mask(bits)) << (index * bits);
    }
}

/// The low `bits` bits of `value` as a signed number.
fn signed(value: u64, bits: u32) -> i128 {
    i128::from(((value << (64 - bits)) as i64) >> (64 - bits))
}

fn saturate_signed(value: i128, bits: u32, flags: &mut u8) -> u64 {
    let max = (1i128 << (bits - 1)) - 1;
    let min = -(1i128 << (bits - 1));
    if value > max {
        *flags |= QC;
        max as u64
    } else if value < min {
        *flags |= QC;
        min as u64 & mask(bits)
    } else {
        value as u64 & mask(bits)
    }
}

fn saturate_unsigned(value: i128, bits: u32, flags: &mut u8) -> u64 {
    if value < 0 {
        *flags |= QC;
        0
    } else if value > i128::from(mask(bits)) {
        *flags |= QC;
        mask(bits)
    } else {
        value as u64
    }
}

/// How a shift treats its operand and its result.
#[derive(Clone, Copy)]
struct Shift {
    unsigned: bool,
    round: bool,
    saturate: bool,
}

/// `x` shifted by `amount`: left, or right when negative.
fn shift(kind: Shift, bits: u32, x: u64, amount: i32, flags: &mut u8) -> u64 {
    let value = if kind.unsigned {
        i128::from(x)
    } else {
        signed(x, bits)
    };
    let shifted = if amount >= bits as i32 {
        // Every bit leaves the lane.
        if !kind.saturate || value == 0 {
            0
        } else if value > 0 {
            i128::MAX
        } else {
            i128::MIN
        }
    } else if amount >= 0 {
        value << amount
    } else {
        // A shift by more than the lane gives what one by 100 gives.
        let amount = amount.unsigned_abs().min(100);
        if kind.round {
            (value + (1i128 << (amount - 1))) >> amount
        } else {
            value >> amount
        }
    };
    match (kind.saturate, kind.unsigned) {
        (false, _) => shifted as u64 & mask(bits),
        (true, true) => saturate_unsigned(shifted, bits, flags),
        (true, false) => saturate_signed(shifted, bits, flags),
    }
}

/// The product of two polynomials over GF(2).
fn polynomial_multiply(a: u64, b: u64) -> u128 {
    let mut product = 0u128;
    for bit in 0..64 {
        if b >> bit & 1 != 0 {
            product ^= u128::from(a) << bit;
        }
    }
    product
}

fn format_of(bits: u32) -> Format {
    match bits {
        16 => HALF,
        32 => SINGLE,
        _ => DOUBLE,
    }
}

fn to_integer_mode(code: u16) -> Rounding {
    match (code >> 7) & 7 {
        0 => Rounding::NearEven,
        1 => Rounding::Max,
        2 => Rounding::Min,
        3 => Rounding::MinMag,
        _ => Rounding::NearMaxMag,
    }
}

/// The immediate of `FCvtS` and `FCvtU`: the count of fraction bits and how
/// to round.
pub const fn to_integer_imm(fraction_bits: u32, rounding: fp::ToInteger) -> u16 {
    fraction_bits as u16 | (rounding as u16) << 7
}

/// One lane of an operation whose lanes all have one size.
#[allow(clippy::too_many_arguments)]
fn same(op: Op, bits: u32, x: u64, y: u64, z: u64, imm: u16, fpcr: u32, flags: &mut u8) -> u64 {
    use Op::*;
    let m = mask(bits);
    let (sx, sy) = (signed(x, bits), signed(y, bits));
    let (ux, uy) = (i128::from(x), i128::from(y));
    let all = |condition: bool| if condition { m } else { 0 };
    let amount = i32::from(y as u8 as i8);
    let shifted = |unsigned: bool, round: bool, saturate: bool, flags: &mut u8| {
        shift(
            Shift {
                unsigned,
                round,
                saturate,
            },
            bits,
            x,
            amount,
            flags,
        )
    };
    let format = format_of(bits);
    let fp = |flags: &mut u8, (value, raised): (u64, u8)| {
        *flags |= raised;
        value
    };
    match op {
        Add => x.wrapping_add(y) & m,
        Sub => x.wrapping_sub(y) & m,
        Mul => x.wrapping_mul(y) & m,
        PMul => polynomial_multiply(x, y) as u64 & m,
        CmEq => all(x == y),
        CmGe => all(sx >= sy),
        CmGt => all(sx > sy),
        CmHi => all(x > y),
        CmHs => all(x >= y),
        CmTst => all(x & y != 0),
        SHAdd => ((sx + sy) >> 1) as u64 & m,
        UHAdd => ((ux + uy) >> 1) as u64 & m,
        SRHAdd => ((sx + sy + 1) >> 1) as u64 & m,
        URHAdd => ((ux + uy + 1) >> 1) as u64 & m,
        SHSub => ((sx - sy) >> 1) as u64 & m,
        UHSub => ((ux - uy) >> 1) as u64 & m,
        SQAdd => saturate_signed(sx + sy, bits, flags),
        UQAdd => saturate_unsigned(ux + uy, bits, flags),
        SQSub => saturate_signed(sx - sy, bits, flags),
        UQSub => saturate_unsigned(ux - uy, bits, flags),
        SMax => sx.max(sy) as u64 & m,
        SMin => sx.min(sy) as u64 & m,
        UMax => x.max(y),
        UMin => x.min(y),
        SAbd => (sx - sy).unsigned_abs() as u64 & m,
        UAbd => x.abs_diff(y),
        SShl => shifted(false, false, false, flags),
        UShl => shifted(true, false, false, flags),
        SRShl => shifted(false, true, false, flags),
        URShl => shifted(true, true, false, flags),
        SQShl => shifted(false, false, true, flags),
        UQShl => shifted(true, false, true, flags),
        SQRShl => shifted(false, true, true, flags),
        UQRShl => shifted(true, true, true, flags),
        // A signed operand, and a result that saturates as an unsigned one.
        // The amount is an immediate from zero up.
        SQShlU => saturate_unsigned(sx << amount.clamp(0, 63), bits, flags),
        SQDMulH => saturate_signed((2 * sx * sy) >> bits, bits, flags),
        SQRDMulH => saturate_signed((2 * sx * sy + (1i128 << (bits - 1))) >> bits, bits, flags),
        SuQAdd => saturate_signed(sx + uy, bits, flags),
        USqAdd => saturate_unsigned(ux + sy, bits, flags),
        Sli => {
            let amount = u32::from(imm);
            ((x << amount) | (z & mask(amount))) & m
        }
        Sri => {
            let amount = u32::from(imm);
            if amount >= bits {
                z
            } else {
                (x >> amount) | (z & !(m >> amount))
            }
        }
        Abs => sx.unsigned_abs() as u64 & m,
        Neg => x.wrapping_neg() & m,
        SQAbs => saturate_signed(sx.abs(), bits, flags),
        SQNeg => saturate_signed(-sx, bits, flags),
        Cls => {
            let folded = if sx < 0 { !x & m } else { x };
            u64::from(folded.leading_zeros() - (64 - bits) - 1)
        }
        Clz => u64::from(x.leading_zeros() - (64 - bits)),
        Cnt => u64::from(x.count_ones()),
        RBit => x.reverse_bits() >> (64 - bits),
        URecpE => u64::from(fpv::unsigned_recip_estimate(x as u32)),
        URSqrtE => u64::from(fpv::unsigned_rsqrt_estimate(x as u32)),
        FAdd => fp(flags, fp::binary(FpBinary::Add, format, x, y, fpcr)),
        FSub => fp(flags, fp::binary(FpBinary::Sub, format, x, y, fpcr)),
        FMul => fp(flags, fp::binary(FpBinary::Mul, format, x, y, fpcr)),
        FDiv => fp(flags, fp::binary(FpBinary::Div, format, x, y, fpcr)),
        FMax | FMaxV => fp(flags, fp::binary(FpBinary::Max, format, x, y, fpcr)),
        FMin | FMinV => fp(flags, fp::binary(FpBinary::Min, format, x, y, fpcr)),
        FMaxNm | FMaxNmV => fp(flags, fp::binary(FpBinary::MaxNumber, format, x, y, fpcr)),
        FMinNm | FMinNmV => fp(flags, fp::binary(FpBinary::MinNumber, format, x, y, fpcr)),
        FMulX => fp(flags, fpv::mulx(format, x, y, fpcr)),
        FAbd => fp(flags, fpv::abd(format, x, y, fpcr)),
        FRecpS => fp(flags, fpv::recip_step(format, x, y, fpcr)),
        FRSqrtS => fp(flags, fpv::rsqrt_step(format, x, y, fpcr)),
        FCmEq => fp(flags, fpv::compare_mask(Mask::Equal, format, x, y, fpcr)),
        FCmGe => fp(flags, fpv::compare_mask(Mask::GreaterEqual, format, x, y, fpcr)),
        FCmGt => fp(flags, fpv::compare_mask(Mask::Greater, format, x, y, fpcr)),
        FAcGe => fp(flags, fpv::compare_mask(
            Mask::MagnitudeGreaterEqual,
            format,
            x,
            y,
            fpcr,
        )),
        FAcGt => fp(flags, fpv::compare_mask(
            Mask::MagnitudeGreater,
            format,
            x,
            y,
            fpcr,
        )),
        FMla => fp(flags, fp::mul_add(format, z, x, y, false, false, fpcr)),
        FMls => fp(flags, fp::mul_add(format, z, x, y, false, true, fpcr)),
        FAbs => x & !format.sign_bit(),
        FNeg => x ^ format.sign_bit(),
        FSqrt => fp(flags, fp::unary(FpUnary::Sqrt, format, x, fpcr)),
        FRintN => fp(flags, fp::unary(FpUnary::RoundNearEven, format, x, fpcr)),
        FRintP => fp(flags, fp::unary(FpUnary::RoundUp, format, x, fpcr)),
        FRintM => fp(flags, fp::unary(FpUnary::RoundDown, format, x, fpcr)),
        FRintZ => fp(flags, fp::unary(FpUnary::RoundToZero, format, x, fpcr)),
        FRintA => fp(flags, fp::unary(FpUnary::RoundTiesAway, format, x, fpcr)),
        FRintX => fp(flags, fp::unary(FpUnary::RoundExact, format, x, fpcr)),
        FRintI => fp(flags, fp::unary(FpUnary::RoundCurrent, format, x, fpcr)),
        FCvtS | FCvtU => fp(flags, fp::to_fixed(
            format,
            x,
            bits,
            op == FCvtU,
            u32::from(imm & 0x7f),
            to_integer_mode(imm),
            fpcr,
        )),
        SCvtF | UCvtF => fp(flags, fp::from_fixed(
            format,
            x,
            bits,
            op == UCvtF,
            u32::from(imm & 0x7f),
            fpcr,
        )),
        FRecpE => fp(flags, fpv::recip_estimate(format, x, fpcr)),
        FRSqrtE => fp(flags, fpv::rsqrt_estimate(format, x, fpcr)),
        FRecpX => fp(flags, fpv::recip_exponent(format, x, fpcr)),
        _ => unreachable!("{} has lanes of more than one size", op.name()),
    }
}

/// One lane of twice `bits` bits from two lanes of `bits` bits.
fn long(op: Op, bits: u32, x: u64, y: u64, imm: u16, fpcr: u32, flags: &mut u8) -> u64 {
    use Op::*;
    let wide = mask(2 * bits);
    let (sx, sy) = (signed(x, bits), signed(y, bits));
    let (ux, uy) = (i128::from(x), i128::from(y));
    match op {
        SAddL => (sx + sy) as u64 & wide,
        UAddL => (ux + uy) as u64 & wide,
        SSubL => (sx - sy) as u64 & wide,
        USubL => (ux - uy) as u64 & wide,
        SAbdL => (sx - sy).unsigned_abs() as u64 & wide,
        UAbdL => (ux - uy).unsigned_abs() as u64 & wide,
        SMulL => (sx * sy) as u64 & wide,
        UMulL => (ux * uy) as u64 & wide,
        SQDMulL => saturate_signed(2 * sx * sy, 2 * bits, flags),
        PMulL => polynomial_multiply(x, y) as u64 & wide,
        SShlL => (sx << imm) as u64 & wide,
        UShlL => (ux << imm) as u64 & wide,
        FCvtL => {
            let (value, raised) = fp::convert(format_of(bits), format_of(2 * bits), x, fpcr);
            *flags |= raised;
            value
        }
        _ => unreachable!("{} does not widen", op.name()),
    }
}

/// One lane of `bits` bits from lanes of twice `bits` bits.
fn narrow(op: Op, bits: u32, x: u64, y: u64, imm: u16, fpcr: u32, flags: &mut u8) -> u64 {
    use Op::*;
    let m = mask(bits);
    let amount = u32::from(imm);
    let sx = signed(x, 2 * bits);
    let ux = i128::from(x);
    let half = if amount == 0 {
        0
    } else {
        1i128 << (amount - 1)
    };
    match op {
        AddHn => (x.wrapping_add(y) >> bits) & m,
        SubHn => (x.wrapping_sub(y) >> bits) & m,
        RAddHn => (x.wrapping_add(y).wrapping_add(1 << (bits - 1)) >> bits) & m,
        RSubHn => (x.wrapping_sub(y).wrapping_add(1 << (bits - 1)) >> bits) & m,
        ShrN => (ux >> amount) as u64 & m,
        RShrN => ((ux + half) >> amount) as u64 & m,
        SQShrN => saturate_signed(sx >> amount, bits, flags),
        UQShrN => saturate_unsigned(ux >> amount, bits, flags),
        SQRShrN => saturate_signed((sx + half) >> amount, bits, flags),
        UQRShrN => saturate_unsigned((ux + half) >> amount, bits, flags),
        SQShrUN => saturate_unsigned(sx >> amount, bits, flags),
        SQRShrUN => saturate_unsigned((sx + half) >> amount, bits, flags),
        FCvtN => {
            let (value, raised) = fp::convert(format_of(2 * bits), format_of(bits), x, fpcr);
            *flags |= raised;
            value
        }
        FCvtXN => {
            let (value, raised) = fpv::convert_to_odd(x, fpcr);
            *flags |= raised;
            value
        }
        _ => unreachable!("{} does not narrow", op.name()),
    }
}

/// Run `code` on `a`, `b` and `c`. Gives the result, with zeros above its
/// lanes, and the bits for FPSR.
pub fn eval(code: Code, a: u128, b: u128, c: u128, fpcr: u32) -> (u128, u8) {
    use Op::*;
    let op = code.op;
    let bits = 8u32 << code.size;
    let lanes = u32::from(code.lanes);
    let mut result = 0u128;
    let mut flags = 0u8;
    match op.shape() {
        Shape::Same => {
            // The lanes that a pairwise operation reads from each operand.
            let each = lanes.max(2);
            let neighbour = |index: u32| {
                if index < each {
                    lane(a, bits, index)
                } else {
                    lane(b, bits, index - each)
                }
            };
            for index in 0..lanes {
                let (x, y) = if code.pairwise {
                    (neighbour(2 * index), neighbour(2 * index + 1))
                } else {
                    (lane(a, bits, index), lane(b, bits, index))
                };
                let z = lane(c, bits, index);
                let value = same(op, bits, x, y, z, code.imm, fpcr, &mut flags);
                put(&mut result, bits, index, value);
            }
        }
        Shape::Long if op == PMulL && bits == 64 => {
            result = polynomial_multiply(a as u64, b as u64);
        }
        Shape::Long => {
            for index in 0..lanes {
                let (x, y) = (lane(a, bits, index), lane(b, bits, index));
                let value = long(op, bits, x, y, code.imm, fpcr, &mut flags);
                put(&mut result, 2 * bits, index, value);
            }
        }
        Shape::Narrow => {
            for index in 0..lanes {
                let (x, y) = (lane(a, 2 * bits, index), lane(b, 2 * bits, index));
                let value = narrow(op, bits, x, y, code.imm, fpcr, &mut flags);
                put(&mut result, bits, index, value);
            }
        }
        Shape::PairLong => {
            for index in 0..lanes {
                let (x, y) = (lane(a, bits, 2 * index), lane(a, bits, 2 * index + 1));
                let sum = if op == SAddLP {
                    signed(x, bits) + signed(y, bits)
                } else {
                    i128::from(x) + i128::from(y)
                };
                put(&mut result, 2 * bits, index, sum as u64);
            }
        }
        Shape::Reduce => result = u128::from(reduce(op, bits, lanes, a, fpcr, &mut flags)),
        Shape::Whole => result = whole(code, bits, lanes, a, b),
    }
    (result, flags)
}

/// Every lane of `a` to one value.
fn reduce(op: Op, bits: u32, lanes: u32, a: u128, fpcr: u32, flags: &mut u8) -> u64 {
    use Op::*;
    let values = (0..lanes).map(|index| lane(a, bits, index));
    let signed_values = (0..lanes).map(|index| signed(lane(a, bits, index), bits));
    match op {
        AddV => values.fold(0u64, u64::wrapping_add) & mask(bits),
        UAddLV => values.sum::<u64>() & mask(2 * bits),
        SAddLV => signed_values.sum::<i128>() as u64 & mask(2 * bits),
        UMaxV => values.max().unwrap_or(0),
        UMinV => values.min().unwrap_or(0),
        SMaxV => signed_values.max().unwrap_or(0) as u64 & mask(bits),
        SMinV => signed_values.min().unwrap_or(0) as u64 & mask(bits),
        // Floating point folds as a tree: the two halves, then their results.
        _ => {
            let mut level: Vec<u64> = values.collect();
            while level.len() > 1 {
                level = level
                    .chunks(2)
                    .map(|pair| same(op, bits, pair[0], pair[1], 0, 0, fpcr, flags))
                    .collect();
            }
            level[0]
        }
    }
}

/// The operations that move lanes.
fn whole(code: Code, bits: u32, lanes: u32, a: u128, b: u128) -> u128 {
    use Op::*;
    let mut result = 0u128;
    let half = lanes / 2;
    // Lane `index` of the first operand followed by the second.
    let joined = |index: u32| {
        if index < lanes {
            lane(a, bits, index)
        } else {
            lane(b, bits, index - lanes)
        }
    };
    for index in 0..lanes {
        let pair = index / 2;
        let from_b = index % 2 == 1;
        let pick = |position: u32| lane(if from_b { b } else { a }, bits, position);
        let value = match code.op {
            Uzp1 => joined(2 * index),
            Uzp2 => joined(2 * index + 1),
            Trn1 => pick(2 * pair),
            Trn2 => pick(2 * pair + 1),
            Zip1 => pick(pair),
            Zip2 => pick(half + pair),
            Ext => joined(index + u32::from(code.imm)),
            // The lanes of each container of `imm` bits in the other order.
            Rev => lane(a, bits, index ^ (u32::from(code.imm) / bits - 1)),
            Dup => lane(a, bits, 0),
            _ => unreachable!("{} does not move lanes", code.op.name()),
        };
        put(&mut result, bits, index, value);
    }
    result
}

fn run(args: &[u128]) -> (u128, u8) {
    eval(
        Code::decode(args[0] as u64),
        args[1],
        args[2],
        args[3],
        args[4] as u32,
    )
}

fn helper_value(args: &[u128]) -> u128 {
    run(args).0
}

fn helper_flags(args: &[u128]) -> u128 {
    u128::from(run(args).1)
}

fn helper_low(args: &[u128]) -> u128 {
    let (value, flags) = run(args);
    u128::from(value as u64) | u128::from(flags) << 64
}

/// `(code, a, b, c, fpcr)`: the 128 bits of the result.
pub static SIMD: Helper = Helper {
    name: "simd",
    args: 5,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_value),
};

/// `(code, a, b, c, fpcr)`: the eight bits for FPSR of the same operation.
pub static SIMD_FLAGS: Helper = Helper {
    name: "simd_flags",
    args: 5,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_flags),
};

/// `(code, a, b, c, fpcr)`: the low 64 bits of the result, and the bits for
/// FPSR in the bits 71 to 64.
pub static SIMD_LOW: Helper = Helper {
    name: "simd_low",
    args: 5,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_low),
};

/// `TBL` and `TBX`: byte `i` of the result is the byte of the table that
/// byte `i` of `indices` names. The table is `registers` registers of 16
/// bytes. An index past it gives zero, or the byte of `fallback` for `TBX`.
pub fn table(
    registers: u32,
    bytes: u32,
    extension: bool,
    fallback: u128,
    indices: u128,
    table: [u128; 4],
) -> u128 {
    let mut result = 0u128;
    for index in 0..bytes {
        let wanted = lane(indices, 8, index) as u32;
        let value = if wanted < 16 * registers {
            lane(table[(wanted / 16) as usize], 8, wanted % 16)
        } else if extension {
            lane(fallback, 8, index)
        } else {
            0
        };
        put(&mut result, 8, index, value);
    }
    result
}

/// The constant of `SIMD_TABLE`.
pub const fn table_code(registers: u32, bytes: u32, extension: bool) -> u64 {
    registers as u64 | (bytes as u64) << 8 | (extension as u64) << 16
}

fn helper_table(args: &[u128]) -> u128 {
    let code = args[0] as u64;
    table(
        (code & 7) as u32,
        ((code >> 8) & 0x1f) as u32,
        code & (1 << 16) != 0,
        args[1],
        args[2],
        [args[3], args[4], args[5], args[6]],
    )
}

/// `(code, fallback, indices, table 0, table 1, table 2, table 3)`.
pub static SIMD_TABLE: Helper = Helper {
    name: "simd_table",
    args: 7,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_table),
};

#[cfg(test)]
mod tests {
    use super::*;

    fn value(code: Code, a: u128, b: u128) -> u128 {
        eval(code, a, b, 0, 0).0
    }

    #[test]
    fn a_code_is_its_own_constant() {
        for op in Op::ALL {
            let code = Code::new(*op, 2, 4).imm(0x1ff);
            assert_eq!(Code::decode(code.encode()), code);
            let code = Code::new(*op, 3, 16).pairwise();
            assert_eq!(Code::decode(code.encode()), code);
        }
        assert_eq!(Code::new(Op::Add, 0, 16).result_bits(), 128);
        assert_eq!(Code::new(Op::SMulL, 1, 4).result_bits(), 128);
        assert_eq!(Code::new(Op::ShrN, 1, 4).result_bits(), 64);
        assert_eq!(Code::new(Op::SAddLV, 0, 16).result_bits(), 16);
        assert!(Op::SQAdd.raises() && Op::FAdd.raises() && Op::FCvtN.raises());
        assert!(Op::FMaxV.raises() && Op::FRecpX.raises());
        assert!(!Op::Add.raises() && !Op::FAbs.raises() && !Op::Zip1.raises());
        assert!(!Op::SAddLV.raises() && !Op::ShrN.raises());
    }

    #[test]
    fn lanes_do_not_carry_into_each_other() {
        let a = 0x00ff_00ff_00ff_00ff_00ff_00ff_00ff_00ffu128;
        let one = 0x0101_0101_0101_0101_0101_0101_0101_0101u128;
        assert_eq!(
            value(Code::new(Op::Add, 0, 16), a, one),
            0x0100_0100_0100_0100_0100_0100_0100_0100
        );
        assert_eq!(
            value(Code::new(Op::Add, 1, 8), a, one),
            0x0200_0200_0200_0200_0200_0200_0200_0200
        );
        // Eight lanes of a 64-bit vector: the upper half is zero.
        assert_eq!(
            value(Code::new(Op::Add, 0, 8), a, one),
            0x0100_0100_0100_0100
        );
    }

    #[test]
    fn saturation_sets_the_bit() {
        let code = Code::new(Op::SQAdd, 0, 2);
        assert_eq!(eval(code, 0x7f7f, 0x0101, 0, 0), (0x7f7f, QC));
        assert_eq!(eval(code, 0x7e80, 0x01ff, 0, 0), (0x7f80, QC));
        assert_eq!(eval(code, 0x7e10, 0x0101, 0, 0), (0x7f11, 0));
        assert_eq!(
            eval(Code::new(Op::UQSub, 1, 1), 1, 2, 0, 0),
            (0, QC)
        );
        assert_eq!(
            eval(Code::new(Op::SQAbs, 3, 1), 1 << 63, 0, 0, 0),
            (u128::from(u64::MAX >> 1), QC)
        );
        // The doubling product saturates only for the most negative pair.
        assert_eq!(
            eval(Code::new(Op::SQDMulH, 1, 1), 0x8000, 0x8000, 0, 0),
            (0x7fff, QC)
        );
        assert_eq!(
            eval(Code::new(Op::SQRDMulH, 1, 1), 0x4000, 0x4000, 0, 0),
            (0x2000, 0)
        );
    }

    #[test]
    fn shifts_by_a_register_go_both_ways() {
        let by = |amount: i8| u128::from(amount as u8);
        let one = |op: Op, x: u128, amount: i8| eval(Code::new(op, 0, 1), x, by(amount), 0, 0);
        assert_eq!(one(Op::UShl, 0x81, 1), (0x02, 0));
        assert_eq!(one(Op::UShl, 0x81, -1), (0x40, 0));
        assert_eq!(one(Op::SShl, 0x81, -1), (0xc0, 0));
        assert_eq!(one(Op::SShl, 0x81, -128), (0xff, 0));
        assert_eq!(one(Op::UShl, 0x81, 8), (0, 0));
        assert_eq!(one(Op::URShl, 0x81, -1), (0x41, 0));
        assert_eq!(one(Op::URShl, 0x80, -8), (1, 0));
        assert_eq!(one(Op::SRShl, 0x80, -8), (0, 0));
        assert_eq!(one(Op::UQShl, 0x81, 1), (0xff, QC));
        assert_eq!(one(Op::SQShl, 0x81, 1), (0x80, QC));
        assert_eq!(one(Op::SQShl, 0x01, 100), (0x7f, QC));
        assert_eq!(one(Op::SQShl, 0x00, 100), (0, 0));
        assert_eq!(one(Op::SQShlU, 0xff, 1), (0, QC));
        assert_eq!(one(Op::SQShlU, 0x41, 1), (0x82, 0));
        // 64-bit lanes: a shift by 63 and by 64.
        let wide = |op: Op, x: u64, amount: i8| {
            eval(Code::new(op, 3, 1), u128::from(x), by(amount), 0, 0)
        };
        assert_eq!(wide(Op::UShl, 1, 63), (1 << 63, 0));
        assert_eq!(wide(Op::UQShl, 1, 63), (1 << 63, 0));
        assert_eq!(wide(Op::UQShl, 1, 64), (u128::from(u64::MAX), QC));
        assert_eq!(wide(Op::URShl, u64::MAX, -64), (1, 0));
        assert_eq!(wide(Op::UShl, u64::MAX, -64), (0, 0));
    }

    #[test]
    fn widening_and_narrowing() {
        assert_eq!(
            value(Code::new(Op::SMulL, 0, 2), 0xff02, 0x02ff),
            0xfffe_fffe
        );
        assert_eq!(
            value(Code::new(Op::UMulL, 0, 2), 0xff02, 0x02ff),
            0x01fe_01fe
        );
        assert_eq!(value(Code::new(Op::PMulL, 0, 1), 0x03, 0x03), 0x05);
        assert_eq!(
            value(Code::new(Op::PMulL, 3, 1), u128::from(u64::MAX), 3),
            u128::from(u64::MAX) ^ u128::from(u64::MAX) << 1
        );
        assert_eq!(
            eval(Code::new(Op::SQDMulL, 1, 1), 0x8000, 0x8000, 0, 0),
            (0x7fff_ffff, QC)
        );
        assert_eq!(value(Code::new(Op::AddHn, 0, 2), 0x12ff_34ff, 0x0001_0001), 0x1335);
        assert_eq!(value(Code::new(Op::RAddHn, 0, 1), 0x127f, 0x0001), 0x13);
        assert_eq!(
            eval(Code::new(Op::SQShrN, 0, 2).imm(4), 0x0ff0_8000, 0, 0, 0),
            (0x7f80, QC)
        );
        assert_eq!(
            eval(Code::new(Op::SQShrUN, 0, 1).imm(0), 0xffff, 0, 0, 0),
            (0, QC)
        );
        assert_eq!(
            eval(Code::new(Op::UQRShrN, 0, 1).imm(1), 0x01ff, 0, 0, 0),
            (0xff, QC)
        );
        assert_eq!(value(Code::new(Op::SAddLP, 0, 1), 0xff7f, 0), 0x007e);
        assert_eq!(value(Code::new(Op::UAddLP, 0, 1), 0xff7f, 0), 0x017e);
        assert_eq!(value(Code::new(Op::SShlL, 0, 1).imm(3), 0xff, 0), 0xfff8);
        assert_eq!(value(Code::new(Op::UShlL, 0, 1).imm(8), 0xff, 0), 0xff00);
    }

    #[test]
    fn lanes_move_as_their_names_say() {
        let a = 0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100u128;
        let b = 0x1f1e_1d1c_1b1a_1918_1716_1514_1312_1110u128;
        let bytes = |op: Op| value(Code::new(op, 0, 16), a, b);
        assert_eq!(bytes(Op::Zip1), 0x1707_1606_1505_1404_1303_1202_1101_1000);
        assert_eq!(bytes(Op::Zip2), 0x1f0f_1e0e_1d0d_1c0c_1b0b_1a0a_1909_1808);
        assert_eq!(bytes(Op::Uzp1), 0x1e1c_1a18_1614_1210_0e0c_0a08_0604_0200);
        assert_eq!(bytes(Op::Uzp2), 0x1f1d_1b19_1715_1311_0f0d_0b09_0705_0301);
        assert_eq!(bytes(Op::Trn1), 0x1e0e_1c0c_1a0a_1808_1606_1404_1202_1000);
        assert_eq!(bytes(Op::Trn2), 0x1f0f_1d0d_1b0b_1909_1707_1505_1303_1101);
        assert_eq!(
            value(Code::new(Op::Ext, 0, 16).imm(3), a, b),
            0x1211_100f_0e0d_0c0b_0a09_0807_0605_0403
        );
        assert_eq!(
            value(Code::new(Op::Ext, 0, 8).imm(7), a, b),
            0x1615_1413_1211_1007
        );
        assert_eq!(
            value(Code::new(Op::Rev, 0, 16).imm(64), a, 0),
            0x0809_0a0b_0c0d_0e0f_0001_0203_0405_0607
        );
        assert_eq!(
            value(Code::new(Op::Rev, 1, 8).imm(32), a, 0),
            0x0d0c_0f0e_0908_0b0a_0504_0706_0100_0302
        );
        assert_eq!(value(Code::new(Op::Dup, 1, 4), 0xabcd, 0), 0xabcd_abcd_abcd_abcd);
        assert_eq!(value(Code::new(Op::AddV, 0, 16), a, 0), 120);
        assert_eq!(value(Code::new(Op::UMaxV, 0, 8), a, 0), 7);
        assert_eq!(value(Code::new(Op::SAddLV, 0, 2), 0xff80, 0), 0xff7f);
        // A pairwise add of four 16-bit lanes from each operand.
        assert_eq!(
            value(Code::new(Op::Add, 1, 4).pairwise(), 0x0004_0003_0002_0001, 0x0040_0030_0020_0010),
            0x0070_0030_0007_0003
        );
        // The scalar form: the two lanes of one operand.
        assert_eq!(
            value(Code::new(Op::Add, 3, 1).pairwise(), 5 | 7 << 64, 0),
            12
        );
    }

    #[test]
    fn a_table_lookup_reads_up_to_four_registers() {
        let t0 = 0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100u128;
        let t1 = t0 | 0x1010_1010_1010_1010_1010_1010_1010_1010;
        let indices = 0x20_1f_10_0fu128;
        assert_eq!(
            table(2, 16, false, u128::MAX, indices, [t0, t1, 0, 0]),
            0x00_1f_10_0f
        );
        assert_eq!(
            table(2, 8, true, 0xaaaa_aaaa_aaaa_aaaa, indices, [t0, t1, 0, 0]),
            0x0000_0000_aa_1f_10_0f
        );
        assert_eq!(table(1, 4, false, 0, indices, [t0, t1, 0, 0]), 0x0f);
    }

    #[test]
    fn floating_point_lanes_collect_their_exceptions() {
        use crate::helpers::fp::fpsr;
        let one = 0x3f80_0000u128;
        let two = 0x4000_0000u128;
        let four = |x: u128| x | x << 32 | x << 64 | x << 96;
        assert_eq!(
            eval(Code::new(Op::FAdd, 2, 4), four(one), four(one), 0, 0),
            (four(two), 0)
        );
        // One lane divides by zero.
        assert_eq!(
            eval(Code::new(Op::FDiv, 2, 2), four(one), one, 0, 0),
            (0x7f80_0000_3f80_0000, fpsr::DZC)
        );
        assert_eq!(
            eval(Code::new(Op::FMla, 2, 1), two, two, one, 0),
            (0x40a0_0000, 0)
        );
        assert_eq!(
            eval(Code::new(Op::FMls, 2, 1), two, two, one, 0),
            (0xc040_0000, 0)
        );
        // 2.5 to an integer, to nearest even and toward plus infinity.
        let near = Code::new(Op::FCvtS, 2, 1).imm(to_integer_imm(0, fp::ToInteger::NearEven));
        assert_eq!(eval(near, 0x4020_0000, 0, 0, 0), (2, fpsr::IXC));
        let up = Code::new(Op::FCvtU, 2, 1).imm(to_integer_imm(1, fp::ToInteger::Up));
        assert_eq!(eval(up, 0x4020_0000, 0, 0, 0), (5, 0));
        assert_eq!(
            eval(Code::new(Op::SCvtF, 3, 1).imm(1), 3, 0, 0, 0),
            (0x3ff8_0000_0000_0000, 0)
        );
        // The maximum of four lanes as a tree, with a quiet NaN in one.
        let lanes = one | two << 32 | 0x7fc0_0000 << 64 | one << 96;
        assert_eq!(
            eval(Code::new(Op::FMaxNmV, 2, 4), lanes, 0, 0, 0),
            (two, 0)
        );
        assert_eq!(
            eval(Code::new(Op::FMaxV, 2, 4), lanes, 0, 0, 0),
            (0x7fc0_0000, 0)
        );
        assert_eq!(
            eval(Code::new(Op::FCvtL, 2, 2), one | two << 32, 0, 0, 0),
            (
                0x3ff0_0000_0000_0000 | 0x4000_0000_0000_0000 << 64,
                0
            )
        );
        assert_eq!(
            eval(
                Code::new(Op::FCvtN, 2, 2),
                0x3ff0_0000_0000_0000 | 0x4000_0000_0000_0000 << 64,
                0,
                0,
                0
            ),
            (one | two << 32, 0)
        );
    }
}
