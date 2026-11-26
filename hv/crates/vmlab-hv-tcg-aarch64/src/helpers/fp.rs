//! Scalar floating point: what FPCR selects, what FPSR collects, and the
//! operations of the scalar FP instructions as functions of their operands
//! and FPCR.
//!
//! Every function here is pure: it takes FPCR as a value and gives back its
//! result with the cumulative exception bits that it raises, in the bit
//! positions of FPSR. The lifter ORs those bits into FPSR. So a block needs
//! no ordering for FP work, and a result that nothing reads is dropped.
//!
//! The arithmetic is Berkeley SoftFloat (`softfloat.rs`). This file adds what
//! the A64 rules ask for around it: which NaN operand a result takes
//! (SoftFloat never sees a NaN operand from here), the default NaN mode,
//! flush-to-zero on inputs and outputs, the minimum and maximum operations,
//! the conversions to a fixed-point integer, and the alternative
//! half-precision format.
//!
//! Sources read (DESIGN.md "Sources"): S3, dynarmic `common/fp` (NaN
//! processing, unpack and round, `FPMulAdd`, `FPToFixed`, `FPRoundInt`,
//! `FPConvert`, 0BSD). S5, SoftFloat 3e and its documentation. S6, the
//! lockstep against QEMU, which decided where S3 and this file differed.
//! Not read: QEMU's source, Arm's ASL or pseudocode (X1, X2).

use super::softfloat::{self as sf, Rounding};
use vmlab_hv_tcg_ir::native::{Effects, Helper, HelperFn};

/// The cumulative exception bits of FPSR.
pub mod fpsr {
    /// Invalid operation.
    pub const IOC: u8 = 1 << 0;
    /// Division by zero.
    pub const DZC: u8 = 1 << 1;
    /// Overflow.
    pub const OFC: u8 = 1 << 2;
    /// Underflow.
    pub const UFC: u8 = 1 << 3;
    /// Inexact.
    pub const IXC: u8 = 1 << 4;
    /// Input denormal: flush-to-zero replaced a denormal operand by zero.
    pub const IDC: u8 = 1 << 7;
}

/// The fields of FPCR that the scalar FP instructions read.
pub mod fpcr {
    /// The rounding mode: bits 23 and 22.
    pub const RMODE_SHIFT: u32 = 22;
    /// Flush-to-zero.
    pub const FZ: u32 = 1 << 24;
    /// Default NaN.
    pub const DN: u32 = 1 << 25;
    /// Alternative half-precision.
    pub const AHP: u32 = 1 << 26;
}

/// The rounding mode that FPCR selects.
pub fn rounding(fpcr: u32) -> Rounding {
    match (fpcr >> fpcr::RMODE_SHIFT) & 3 {
        0 => Rounding::NearEven,
        1 => Rounding::Max,
        2 => Rounding::Min,
        _ => Rounding::MinMag,
    }
}

/// A binary floating-point format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub bits: u32,
    pub exponent_bits: u32,
}

pub const HALF: Format = Format {
    bits: 16,
    exponent_bits: 5,
};
pub const SINGLE: Format = Format {
    bits: 32,
    exponent_bits: 8,
};
pub const DOUBLE: Format = Format {
    bits: 64,
    exponent_bits: 11,
};

impl Format {
    pub const fn fraction_bits(self) -> u32 {
        self.bits - 1 - self.exponent_bits
    }

    pub const fn mask(self) -> u64 {
        if self.bits == 64 {
            u64::MAX
        } else {
            (1 << self.bits) - 1
        }
    }

    pub const fn sign_bit(self) -> u64 {
        1 << (self.bits - 1)
    }

    const fn fraction_mask(self) -> u64 {
        (1 << self.fraction_bits()) - 1
    }

    /// The largest value of the exponent field.
    const fn exponent_max(self) -> u64 {
        (1 << self.exponent_bits) - 1
    }

    const fn bias(self) -> i32 {
        (1 << (self.exponent_bits - 1)) - 1
    }

    const fn exponent(self, value: u64) -> u64 {
        (value >> self.fraction_bits()) & self.exponent_max()
    }

    const fn fraction(self, value: u64) -> u64 {
        value & self.fraction_mask()
    }

    const fn sign(self, value: u64) -> bool {
        value & self.sign_bit() != 0
    }

    /// The top bit of the fraction: set in a quiet NaN.
    const fn quiet_bit(self) -> u64 {
        1 << (self.fraction_bits() - 1)
    }

    pub const fn zero(self, sign: bool) -> u64 {
        if sign { self.sign_bit() } else { 0 }
    }

    pub const fn infinity(self, sign: bool) -> u64 {
        self.zero(sign) | self.exponent_max() << self.fraction_bits()
    }

    /// The NaN that an invalid operation gives, and every NaN result in the
    /// default NaN mode.
    pub const fn default_nan(self) -> u64 {
        self.infinity(false) | self.quiet_bit()
    }

    /// The largest finite value.
    const fn max_normal(self, sign: bool) -> u64 {
        self.infinity(sign) - 1
    }

    pub const fn is_nan(self, value: u64) -> bool {
        self.exponent(value) == self.exponent_max() && self.fraction(value) != 0
    }

    const fn is_signalling(self, value: u64) -> bool {
        self.is_nan(value) && value & self.quiet_bit() == 0
    }

    const fn is_quiet(self, value: u64) -> bool {
        self.is_nan(value) && value & self.quiet_bit() != 0
    }

    const fn is_infinity(self, value: u64) -> bool {
        self.exponent(value) == self.exponent_max() && self.fraction(value) == 0
    }

    const fn is_zero(self, value: u64) -> bool {
        value & !self.sign_bit() == 0
    }

    const fn is_denormal(self, value: u64) -> bool {
        self.exponent(value) == 0 && self.fraction(value) != 0
    }

    /// A key that orders two values that are not NaN as their numbers.
    /// Both zeros have the key 0.
    const fn key(self, value: u64) -> i128 {
        let magnitude = (value & !self.sign_bit()) as i128;
        if self.sign(value) {
            -magnitude
        } else {
            magnitude
        }
    }
}

/// The SoftFloat exception flags as bits of FPSR.
pub fn cumulative(flags: u8) -> u8 {
    let mut bits = 0;
    for (from, to) in [
        (sf::flag::INVALID, fpsr::IOC),
        (sf::flag::INFINITE, fpsr::DZC),
        (sf::flag::OVERFLOW, fpsr::OFC),
        (sf::flag::UNDERFLOW, fpsr::UFC),
        (sf::flag::INEXACT, fpsr::IXC),
    ] {
        if flags & from != 0 {
            bits |= to;
        }
    }
    bits
}

/// An operand as the operation sees it. With flush-to-zero a denormal
/// operand is a zero of its sign, and FPSR.IDC records that. A half-precision
/// operand is never flushed.
fn operand(format: Format, value: u64, fpcr: u32, flags: &mut u8) -> u64 {
    if fpcr & fpcr::FZ != 0 && format.bits != 16 && format.is_denormal(value) {
        *flags |= fpsr::IDC;
        format.zero(format.sign(value))
    } else {
        value
    }
}

/// The result for one NaN operand: the operand made quiet, or the default
/// NaN in the default NaN mode. A signalling NaN is an invalid operation.
fn nan(format: Format, value: u64, fpcr: u32, flags: &mut u8) -> u64 {
    if format.is_signalling(value) {
        *flags |= fpsr::IOC;
    }
    if fpcr & fpcr::DN != 0 {
        format.default_nan()
    } else {
        value | format.quiet_bit()
    }
}

/// The NaN result of an operation on `operands`, when one of them is a NaN:
/// the first signalling NaN, else the first quiet NaN.
fn nan_of(format: Format, operands: &[u64], fpcr: u32, flags: &mut u8) -> Option<u64> {
    let chosen = operands
        .iter()
        .find(|value| format.is_signalling(**value))
        .or_else(|| operands.iter().find(|value| format.is_nan(**value)))?;
    Some(nan(format, *chosen, fpcr, flags))
}

/// A SoftFloat result as the result of an A64 operation: its flags as bits
/// of FPSR, and with flush-to-zero a result that is tiny before rounding
/// becomes a zero of its sign with UFC alone.
///
/// SoftFloat raises underflow when a result is tiny before rounding and
/// inexact. A tiny result that is exact is a denormal. So the result was
/// tiny exactly when it is a denormal or underflow is raised.
fn result(format: Format, (value, raised): (u64, u8), fpcr: u32, flags: &mut u8) -> u64 {
    let tiny = format.is_denormal(value) || raised & sf::flag::UNDERFLOW != 0;
    if fpcr & fpcr::FZ != 0 && format.bits != 16 && tiny {
        *flags |= fpsr::UFC;
        return format.zero(format.sign(value));
    }
    *flags |= cumulative(raised);
    value
}

/// The SoftFloat operations of one format.
struct Operations {
    add: u32,
    sub: u32,
    mul: u32,
    div: u32,
    sqrt: u32,
    mul_add: u32,
    round_to_int: u32,
}

const fn operations(format: Format) -> Operations {
    if format.bits == 64 {
        Operations {
            add: sf::F64_ADD,
            sub: sf::F64_SUB,
            mul: sf::F64_MUL,
            div: sf::F64_DIV,
            sqrt: sf::F64_SQRT,
            mul_add: sf::F64_MULADD,
            round_to_int: sf::F64_ROUNDTOINT,
        }
    } else {
        Operations {
            add: sf::F32_ADD,
            sub: sf::F32_SUB,
            mul: sf::F32_MUL,
            div: sf::F32_DIV,
            sqrt: sf::F32_SQRT,
            mul_add: sf::F32_MULADD,
            round_to_int: sf::F32_ROUNDTOINT,
        }
    }
}

/// An operation of two operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Binary {
    Add,
    Sub,
    Mul,
    Div,
    /// The negated product (`FNMUL`).
    NegMul,
    Max,
    Min,
    /// The maximum, where a quiet NaN beside a number is the missing value
    /// (`FMAXNM`).
    MaxNumber,
    MinNumber,
}

impl Binary {
    pub const ALL: [Self; 9] = [
        Self::Add,
        Self::Sub,
        Self::Mul,
        Self::Div,
        Self::NegMul,
        Self::Max,
        Self::Min,
        Self::MaxNumber,
        Self::MinNumber,
    ];
}

/// `a op b` in `format`. Gives the result and the bits for FPSR.
pub fn binary(operation: Binary, format: Format, a: u64, b: u64, fpcr: u32) -> (u64, u8) {
    let mut flags = 0;
    let mut a = operand(format, a, fpcr, &mut flags);
    let mut b = operand(format, b, fpcr, &mut flags);
    let maximum = matches!(operation, Binary::Max | Binary::MaxNumber);
    if matches!(operation, Binary::MaxNumber | Binary::MinNumber) {
        // A quiet NaN beside a number loses against it.
        let loser = format.infinity(maximum);
        if format.is_quiet(a) && !format.is_nan(b) {
            a = loser;
        } else if format.is_quiet(b) && !format.is_nan(a) {
            b = loser;
        }
    }
    let negate = |value: u64| {
        if operation == Binary::NegMul {
            value ^ format.sign_bit()
        } else {
            value
        }
    };
    if let Some(value) = nan_of(format, &[a, b], fpcr, &mut flags) {
        return (negate(value), flags);
    }
    let table = operations(format);
    let mode = rounding(fpcr);
    let value = match operation {
        Binary::Add => result(format, sf::call(table.add, mode, a, b, 0), fpcr, &mut flags),
        Binary::Sub => result(format, sf::call(table.sub, mode, a, b, 0), fpcr, &mut flags),
        Binary::Mul | Binary::NegMul => {
            result(format, sf::call(table.mul, mode, a, b, 0), fpcr, &mut flags)
        }
        Binary::Div => result(format, sf::call(table.div, mode, a, b, 0), fpcr, &mut flags),
        Binary::Max | Binary::Min | Binary::MaxNumber | Binary::MinNumber => {
            if format.is_zero(a) && format.is_zero(b) {
                // Of two zeros the maximum is negative only when both are.
                let sign = if maximum {
                    format.sign(a) && format.sign(b)
                } else {
                    format.sign(a) || format.sign(b)
                };
                format.zero(sign)
            } else if (format.key(a) > format.key(b)) == maximum {
                a
            } else {
                b
            }
        }
    };
    (negate(value), flags)
}

/// An operation of one operand that gives a value of the same format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Unary {
    Sqrt,
    /// Round to an integral value: to nearest with ties to even (`FRINTN`).
    RoundNearEven,
    /// Toward plus infinity (`FRINTP`).
    RoundUp,
    /// Toward minus infinity (`FRINTM`).
    RoundDown,
    /// Toward zero (`FRINTZ`).
    RoundToZero,
    /// To nearest with ties away from zero (`FRINTA`).
    RoundTiesAway,
    /// In the mode of FPCR, and inexact is raised (`FRINTX`).
    RoundExact,
    /// In the mode of FPCR (`FRINTI`).
    RoundCurrent,
}

impl Unary {
    pub const ALL: [Self; 8] = [
        Self::Sqrt,
        Self::RoundNearEven,
        Self::RoundUp,
        Self::RoundDown,
        Self::RoundToZero,
        Self::RoundTiesAway,
        Self::RoundExact,
        Self::RoundCurrent,
    ];
}

pub fn unary(operation: Unary, format: Format, a: u64, fpcr: u32) -> (u64, u8) {
    let mut flags = 0;
    let a = operand(format, a, fpcr, &mut flags);
    if let Some(value) = nan_of(format, &[a], fpcr, &mut flags) {
        return (value, flags);
    }
    let table = operations(format);
    let current = rounding(fpcr);
    let (mode, exact) = match operation {
        Unary::Sqrt => {
            let value = result(
                format,
                sf::call(table.sqrt, current, a, 0, 0),
                fpcr,
                &mut flags,
            );
            return (value, flags);
        }
        Unary::RoundNearEven => (Rounding::NearEven, false),
        Unary::RoundUp => (Rounding::Max, false),
        Unary::RoundDown => (Rounding::Min, false),
        Unary::RoundToZero => (Rounding::MinMag, false),
        Unary::RoundTiesAway => (Rounding::NearMaxMag, false),
        Unary::RoundExact => (current, true),
        Unary::RoundCurrent => (current, false),
    };
    let raw = sf::call(
        table.round_to_int,
        current,
        a,
        mode as u64,
        u64::from(exact),
    );
    (result(format, raw, fpcr, &mut flags), flags)
}

/// `addend + n * m` as one operation with one rounding. `negate_addend` and
/// `negate_product` change the sign of the addend and of `n` before the
/// operation, as `FMSUB`, `FNMADD` and `FNMSUB` do.
pub fn mul_add(
    format: Format,
    addend: u64,
    n: u64,
    m: u64,
    negate_addend: bool,
    negate_product: bool,
    fpcr: u32,
) -> (u64, u8) {
    let mut flags = 0;
    let flip = |value: u64, negate: bool| {
        if negate {
            value ^ format.sign_bit()
        } else {
            value
        }
    };
    let addend = operand(format, flip(addend, negate_addend), fpcr, &mut flags);
    let n = operand(format, flip(n, negate_product), fpcr, &mut flags);
    let m = operand(format, m, fpcr, &mut flags);
    let invalid_product = (format.is_infinity(n) && format.is_zero(m))
        || (format.is_zero(n) && format.is_infinity(m));
    // A quiet NaN addend does not hide an invalid product.
    if format.is_quiet(addend) && invalid_product {
        // A signalling NaN among the factors is impossible here: one is an
        // infinity and one a zero.
        return (format.default_nan(), flags | fpsr::IOC);
    }
    if let Some(value) = nan_of(format, &[addend, n, m], fpcr, &mut flags) {
        return (value, flags);
    }
    let raw = sf::call(operations(format).mul_add, rounding(fpcr), n, m, addend);
    (result(format, raw, fpcr, &mut flags), flags)
}

/// N, Z, C and V of a comparison, as the bits 3 to 0.
pub mod nzcv {
    pub const UNORDERED: u8 = 0b0011;
    pub const EQUAL: u8 = 0b0110;
    pub const LESS: u8 = 0b1000;
    pub const GREATER: u8 = 0b0010;
}

/// Compare `a` with `b`. `signalling`: a quiet NaN is an invalid operation
/// too (`FCMPE`). Gives the flags and the bits for FPSR.
pub fn compare(format: Format, a: u64, b: u64, signalling: bool, fpcr: u32) -> (u8, u8) {
    let mut flags = 0;
    let a = operand(format, a, fpcr, &mut flags);
    let b = operand(format, b, fpcr, &mut flags);
    if format.is_nan(a) || format.is_nan(b) {
        if signalling || format.is_signalling(a) || format.is_signalling(b) {
            flags |= fpsr::IOC;
        }
        return (nzcv::UNORDERED, flags);
    }
    let order = match format.key(a).cmp(&format.key(b)) {
        std::cmp::Ordering::Less => nzcv::LESS,
        std::cmp::Ordering::Equal => nzcv::EQUAL,
        std::cmp::Ordering::Greater => nzcv::GREATER,
    };
    (order, flags)
}

/// How an inexact value becomes one with fewer bits. `sign` and `odd` are of
/// the truncated magnitude, `half` compares what was cut off with one half of
/// the last kept place, and `inexact` says that something was cut off.
fn rounds_up(
    mode: Rounding,
    sign: bool,
    odd: bool,
    half: std::cmp::Ordering,
    inexact: bool,
) -> bool {
    use std::cmp::Ordering::{Equal, Greater};
    match mode {
        Rounding::NearEven => half == Greater || (half == Equal && odd),
        Rounding::NearMaxMag => half != std::cmp::Ordering::Less,
        Rounding::MinMag => false,
        Rounding::Max => inexact && !sign,
        Rounding::Min => inexact && sign,
    }
}

/// A finite value that is not zero as `significand * 2^exponent`.
fn unpack(format: Format, value: u64) -> (u64, i32) {
    let fraction_bits = format.fraction_bits() as i32;
    let exponent = format.exponent(value) as i32;
    if exponent == 0 {
        (format.fraction(value), 1 - format.bias() - fraction_bits)
    } else {
        (
            format.fraction(value) | 1 << fraction_bits,
            exponent - format.bias() - fraction_bits,
        )
    }
}

/// Convert `a` to a fixed-point integer of `integer_bits` bits (32 or 64)
/// with `fraction_bits` bits after the point, rounding in `mode`. A value
/// outside the range gives the nearest end of the range, and a NaN zero,
/// with an invalid operation. The result of 32 bits has zeros above it.
pub fn to_fixed(
    format: Format,
    a: u64,
    integer_bits: u32,
    unsigned: bool,
    fraction_bits: u32,
    mode: Rounding,
    fpcr: u32,
) -> (u64, u8) {
    let mut flags = 0;
    let a = operand(format, a, fpcr, &mut flags);
    let width_mask = if integer_bits == 64 {
        u64::MAX
    } else {
        (1 << integer_bits) - 1
    };
    if format.is_nan(a) {
        return (0, flags | fpsr::IOC);
    }
    if format.is_zero(a) {
        return (0, flags);
    }
    let sign = format.sign(a);
    // The ends of the range, as magnitudes.
    let (positive_max, negative_max): (u128, u128) = if unsigned {
        (u128::from(width_mask), 0)
    } else {
        (u128::from(width_mask >> 1), u128::from(width_mask >> 1) + 1)
    };
    let saturated = |flags: u8| {
        let value = match (unsigned, sign) {
            (true, false) => width_mask,
            (true, true) => 0,
            (false, false) => width_mask >> 1,
            (false, true) => (width_mask >> 1) + 1,
        };
        (value, flags | fpsr::IOC)
    };
    if format.is_infinity(a) {
        return saturated(flags);
    }
    let (significand, exponent) = unpack(format, a);
    let exponent = exponent + fraction_bits as i32;
    let (mut magnitude, half, inexact) = if exponent >= 0 {
        // The significand has at most 53 bits, so a shift by 64 or more is
        // past every range.
        if exponent >= 64 {
            return saturated(flags);
        }
        (
            u128::from(significand) << exponent,
            std::cmp::Ordering::Less,
            false,
        )
    } else {
        let shift = exponent.unsigned_abs();
        if shift > 64 {
            // Less than one half of the last place, and not zero.
            (0, std::cmp::Ordering::Less, true)
        } else {
            let wide = u128::from(significand);
            let cut = wide & ((1u128 << shift) - 1);
            (wide >> shift, cut.cmp(&(1u128 << (shift - 1))), cut != 0)
        }
    };
    if rounds_up(mode, sign, magnitude & 1 != 0, half, inexact) {
        magnitude += 1;
    }
    let limit = if sign { negative_max } else { positive_max };
    if magnitude > limit {
        return saturated(flags);
    }
    if inexact {
        flags |= fpsr::IXC;
    }
    let magnitude = magnitude as u64;
    let value = if sign {
        magnitude.wrapping_neg()
    } else {
        magnitude
    };
    (value & width_mask, flags)
}

/// Convert the fixed-point integer `x` of `integer_bits` bits (32 or 64),
/// with `fraction_bits` bits after the point, to `format`, rounding in the
/// mode of FPCR.
pub fn from_fixed(
    format: Format,
    x: u64,
    integer_bits: u32,
    unsigned: bool,
    fraction_bits: u32,
    fpcr: u32,
) -> (u64, u8) {
    let operation = match (format.bits == 64, integer_bits == 64, unsigned) {
        (false, false, false) => sf::I32_TO_F32,
        (false, false, true) => sf::UI32_TO_F32,
        (false, true, false) => sf::I64_TO_F32,
        (false, true, true) => sf::UI64_TO_F32,
        (true, false, false) => sf::I32_TO_F64,
        (true, false, true) => sf::UI32_TO_F64,
        (true, true, false) => sf::I64_TO_F64,
        (true, true, true) => sf::UI64_TO_F64,
    };
    let x = if integer_bits == 64 {
        x
    } else {
        x & 0xffff_ffff
    };
    let (value, raised) = sf::call(operation, rounding(fpcr), x, 0, 0);
    // The integer is rounded to the significand once. The division by a
    // power of two is then exact: the smallest result, 2^-64, is far above
    // the denormals of both formats. So it is a change of the exponent.
    let value = if format.is_zero(value) {
        value
    } else {
        value - (u64::from(fraction_bits) << format.fraction_bits())
    };
    (value, cumulative(raised))
}

/// A NaN of `from` in the format `to`: the sign, and the top bits of the
/// fraction, quiet.
fn convert_nan(from: Format, to: Format, value: u64) -> u64 {
    let fraction = from.fraction(value);
    let fraction = if to.fraction_bits() >= from.fraction_bits() {
        fraction << (to.fraction_bits() - from.fraction_bits())
    } else {
        fraction >> (from.fraction_bits() - to.fraction_bits())
    };
    to.infinity(from.sign(value)) | fraction | to.quiet_bit()
}

/// Round `significand * 2^exponent`, which is not zero, to half precision.
/// `alternative`: the format has no infinity and no NaN, the exponent field
/// 31 is one more binade, and a value above its range is an invalid operation
/// that gives the largest magnitude.
pub fn round_to_half(
    sign: bool,
    significand: u64,
    exponent: i32,
    mode: Rounding,
    alternative: bool,
) -> (u64, u8) {
    debug_assert_ne!(significand, 0);
    let format = HALF;
    // Put the leading one at bit 63. `top` is the exponent of that bit.
    let shift = significand.leading_zeros();
    let wide = u128::from(significand << shift) << 64;
    let top = exponent + 63 - shift as i32;
    // A normal result keeps 11 bits. Below the smallest normal exponent the
    // result is a denormal and keeps fewer.
    let field = top + format.bias();
    let (kept, field) = if field >= 1 {
        (11, field)
    } else {
        (11 - (1 - field), 0)
    };
    let (mut magnitude, half, inexact) = if kept < 0 {
        (0u64, std::cmp::Ordering::Less, true)
    } else if kept == 0 {
        // The leading one is at one half of the last place.
        (0, wide.cmp(&(1u128 << 127)), true)
    } else {
        // The top `kept` bits of `wide`, and what is below them.
        let drop = 128 - kept as u32;
        let cut = wide & ((1u128 << drop) - 1);
        (
            (wide >> drop) as u64,
            cut.cmp(&(1u128 << (drop - 1))),
            cut != 0,
        )
    };
    let mut flags = 0;
    // Tininess is detected before rounding.
    if field == 0 && inexact {
        flags |= fpsr::UFC;
    }
    if rounds_up(mode, sign, magnitude & 1 != 0, half, inexact) {
        magnitude += 1;
    }
    // A normal significand has its leading one at bit 10, which the sum
    // below turns into the exponent field. A carry out of the significand
    // goes into the exponent the same way.
    let mut packed = if field == 0 {
        magnitude
    } else {
        ((field as u64 - 1) << format.fraction_bits()) + magnitude
    };
    let exponent_field = packed >> format.fraction_bits();
    if alternative {
        if exponent_field > format.exponent_max() {
            return (format.zero(sign) | 0x7fff, fpsr::IOC);
        }
    } else if exponent_field >= format.exponent_max() {
        let to_infinity = match mode {
            Rounding::NearEven | Rounding::NearMaxMag => true,
            Rounding::Max => !sign,
            Rounding::Min => sign,
            Rounding::MinMag => false,
        };
        packed = if to_infinity {
            format.infinity(false)
        } else {
            format.max_normal(false)
        };
        return (format.zero(sign) | packed, fpsr::OFC | fpsr::IXC);
    }
    if inexact {
        flags |= fpsr::IXC;
    }
    (format.zero(sign) | packed, flags)
}

/// Convert `a` from the format `from` to the format `to` (`FCVT`).
pub fn convert(from: Format, to: Format, a: u64, fpcr: u32) -> (u64, u8) {
    let mut flags = 0;
    let alternative = fpcr & fpcr::AHP != 0;
    let a = operand(from, a, fpcr, &mut flags);
    let sign = from.sign(a);
    // The alternative half-precision format has no NaN and no infinity.
    if from.bits == 16 && alternative {
        if from.is_zero(a) {
            return (to.zero(sign), flags);
        }
        // The exponent field 31 is a normal number. A half-precision value
        // is exact in both wider formats.
        let (significand, exponent) = unpack(from, a);
        let shift = significand.leading_zeros() - (63 - to.fraction_bits());
        let field = (exponent + 63 - significand.leading_zeros() as i32 + to.bias()) as u64;
        let fraction = (significand << shift) & to.fraction_mask();
        return (
            to.zero(sign) | field << to.fraction_bits() | fraction,
            flags,
        );
    }
    if from.is_nan(a) {
        if to.bits == 16 && alternative {
            return (to.zero(sign), flags | fpsr::IOC);
        }
        if from.is_signalling(a) {
            flags |= fpsr::IOC;
        }
        let value = if fpcr & fpcr::DN != 0 {
            to.default_nan()
        } else {
            convert_nan(from, to, a)
        };
        return (value, flags);
    }
    if to.bits == 16 && alternative {
        if from.is_infinity(a) {
            return (to.zero(sign) | 0x7fff, flags | fpsr::IOC);
        }
        if from.is_zero(a) {
            return (to.zero(sign), flags);
        }
        let (significand, exponent) = unpack(from, a);
        let (value, raised) = round_to_half(sign, significand, exponent, rounding(fpcr), true);
        return (value, flags | raised);
    }
    let operation = match (from.bits, to.bits) {
        (16, 32) => sf::F16_TO_F32,
        (16, 64) => sf::F16_TO_F64,
        (32, 16) => sf::F32_TO_F16,
        (32, 64) => sf::F32_TO_F64,
        (64, 16) => sf::F64_TO_F16,
        (64, 32) => sf::F64_TO_F32,
        _ => return (a & to.mask(), flags),
    };
    let raw = sf::call(operation, rounding(fpcr), a, 0, 0);
    (result(to, raw, fpcr, &mut flags), flags)
}

// --- the helpers of the lifters ---------------------------------------------
//
// Every helper is pure. Its first argument is a constant that names the
// operation, its last is FPCR. The result has the value in the bits 63 to 0
// and the bits for FPSR in the bits 71 to 64.

/// The width in bits of a helper result: 64 of value and 8 for FPSR.
pub const RESULT_BITS: u16 = 72;

fn pack((value, flags): (u64, u8)) -> u128 {
    u128::from(value) | u128::from(flags) << 64
}

fn format_of(code: u128) -> Format {
    match code & 3 {
        0 => HALF,
        1 => SINGLE,
        _ => DOUBLE,
    }
}

/// The two bits that name a format in an operation constant.
pub const fn format_code(format: Format) -> u64 {
    match format.bits {
        16 => 0,
        32 => 1,
        _ => 2,
    }
}

/// The constant of `FP_BINARY`.
pub const fn binary_code(operation: Binary, format: Format) -> u64 {
    format_code(format) | (operation as u64) << 2
}

fn helper_binary(args: &[u128]) -> u128 {
    let operation = Binary::ALL[(args[0] >> 2) as usize % Binary::ALL.len()];
    pack(binary(
        operation,
        format_of(args[0]),
        args[1] as u64,
        args[2] as u64,
        args[3] as u32,
    ))
}

/// `(code, a, b, fpcr)`.
pub static FP_BINARY: Helper = Helper {
    name: "fp_binary",
    args: 4,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_binary),
};

/// The constant of `FP_UNARY`.
pub const fn unary_code(operation: Unary, format: Format) -> u64 {
    format_code(format) | (operation as u64) << 2
}

fn helper_unary(args: &[u128]) -> u128 {
    let operation = Unary::ALL[(args[0] >> 2) as usize % Unary::ALL.len()];
    pack(unary(
        operation,
        format_of(args[0]),
        args[1] as u64,
        args[2] as u32,
    ))
}

/// `(code, a, fpcr)`.
pub static FP_UNARY: Helper = Helper {
    name: "fp_unary",
    args: 3,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_unary),
};

/// The constant of `FP_MUL_ADD`.
pub const fn mul_add_code(format: Format, negate_addend: bool, negate_product: bool) -> u64 {
    format_code(format) | (negate_addend as u64) << 2 | (negate_product as u64) << 3
}

fn helper_mul_add(args: &[u128]) -> u128 {
    pack(mul_add(
        format_of(args[0]),
        args[1] as u64,
        args[2] as u64,
        args[3] as u64,
        args[0] & 4 != 0,
        args[0] & 8 != 0,
        args[4] as u32,
    ))
}

/// `(code, addend, n, m, fpcr)`.
pub static FP_MUL_ADD: Helper = Helper {
    name: "fp_mul_add",
    args: 5,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_mul_add),
};

/// The constant of `FP_COMPARE`.
pub const fn compare_code(format: Format, signalling: bool) -> u64 {
    format_code(format) | (signalling as u64) << 2
}

fn helper_compare(args: &[u128]) -> u128 {
    let (flags, raised) = compare(
        format_of(args[0]),
        args[1] as u64,
        args[2] as u64,
        args[0] & 4 != 0,
        args[3] as u32,
    );
    pack((u64::from(flags), raised))
}

/// `(code, a, b, fpcr)`. The value is N, Z, C and V in the bits 3 to 0.
pub static FP_COMPARE: Helper = Helper {
    name: "fp_compare",
    args: 4,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_compare),
};

/// The constant of `FP_CONVERT`.
pub const fn convert_code(from: Format, to: Format) -> u64 {
    format_code(from) | format_code(to) << 2
}

fn helper_convert(args: &[u128]) -> u128 {
    pack(convert(
        format_of(args[0]),
        format_of(args[0] >> 2),
        args[1] as u64,
        args[2] as u32,
    ))
}

/// `(code, a, fpcr)`.
pub static FP_CONVERT: Helper = Helper {
    name: "fp_convert",
    args: 3,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_convert),
};

/// How `FP_TO_FIXED` rounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ToInteger {
    /// To nearest, ties to even (`FCVTN*`).
    NearEven,
    /// Toward plus infinity (`FCVTP*`).
    Up,
    /// Toward minus infinity (`FCVTM*`).
    Down,
    /// Toward zero (`FCVTZ*`).
    ToZero,
    /// To nearest, ties away from zero (`FCVTA*`).
    TiesAway,
}

impl ToInteger {
    const fn mode(self) -> Rounding {
        match self {
            Self::NearEven => Rounding::NearEven,
            Self::Up => Rounding::Max,
            Self::Down => Rounding::Min,
            Self::ToZero => Rounding::MinMag,
            Self::TiesAway => Rounding::NearMaxMag,
        }
    }
}

/// The constant of `FP_TO_FIXED`.
pub const fn to_fixed_code(
    format: Format,
    integer_bits: u32,
    unsigned: bool,
    fraction_bits: u32,
    rounding: ToInteger,
) -> u64 {
    format_code(format)
        | ((integer_bits == 64) as u64) << 2
        | (unsigned as u64) << 3
        | (rounding as u64) << 4
        | (fraction_bits as u64) << 8
}

fn helper_to_fixed(args: &[u128]) -> u128 {
    let code = args[0] as u64;
    let rounding = match (code >> 4) & 7 {
        0 => ToInteger::NearEven,
        1 => ToInteger::Up,
        2 => ToInteger::Down,
        3 => ToInteger::ToZero,
        _ => ToInteger::TiesAway,
    };
    pack(to_fixed(
        format_of(args[0]),
        args[1] as u64,
        if code & 4 != 0 { 64 } else { 32 },
        code & 8 != 0,
        (code >> 8) as u32 & 0x7f,
        rounding.mode(),
        args[2] as u32,
    ))
}

/// `(code, a, fpcr)`.
pub static FP_TO_FIXED: Helper = Helper {
    name: "fp_to_fixed",
    args: 3,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_to_fixed),
};

/// The constant of `FP_FROM_FIXED`.
pub const fn from_fixed_code(
    format: Format,
    integer_bits: u32,
    unsigned: bool,
    fraction_bits: u32,
) -> u64 {
    format_code(format)
        | ((integer_bits == 64) as u64) << 2
        | (unsigned as u64) << 3
        | (fraction_bits as u64) << 8
}

fn helper_from_fixed(args: &[u128]) -> u128 {
    let code = args[0] as u64;
    pack(from_fixed(
        format_of(args[0]),
        args[1] as u64,
        if code & 4 != 0 { 64 } else { 32 },
        code & 8 != 0,
        (code >> 8) as u32 & 0x7f,
        args[2] as u32,
    ))
}

/// `(code, x, fpcr)`.
pub static FP_FROM_FIXED: Helper = Helper {
    name: "fp_from_fixed",
    args: 3,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_from_fixed),
};

/// The value of the 8-bit immediate of `FMOV` in `format`: a sign, three
/// bits of exponent and four of fraction.
pub fn expand_immediate(format: Format, immediate: u32) -> u64 {
    let sign = immediate & 0x80 != 0;
    let b = u64::from((immediate >> 6) & 1);
    // The exponent field: NOT(b), then b repeated, then the two bits c and d.
    let repeat = format.exponent_bits - 3;
    let exponent = (b ^ 1) << (format.exponent_bits - 1)
        | (if b != 0 { (1 << repeat) - 1 } else { 0 }) << 2
        | u64::from((immediate >> 4) & 3);
    let fraction = u64::from(immediate & 0xf) << (format.fraction_bits() - 4);
    format.zero(sign) | exponent << format.fraction_bits() | fraction
}

#[cfg(test)]
mod tests;
