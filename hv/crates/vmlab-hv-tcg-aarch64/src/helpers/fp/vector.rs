//! The floating-point operations that only AdvSIMD has (card C7): `FMULX`,
//! `FABD`, the two Newton-Raphson steps, the reciprocal estimates, `FRECPX`,
//! the compares that give a mask, and the narrowing that rounds to odd.
//!
//! Each is a function of its operands and FPCR, as the scalar ones of the
//! parent module, and gives its result with the bits for FPSR.
//!
//! Sources read (DESIGN.md "Sources"): S3, dynarmic `common/fp/op`
//! (`FPRecipEstimate.cpp`, `FPRSqrtEstimate.cpp`, `FPRecipExponent.cpp`,
//! `FPRecipStepFused.cpp`, `FPRSqrtStepFused.cpp`) and
//! `common/math_util.cpp` for the two integer estimates, 0BSD. No code of it
//! is copied. S5, SoftFloat. S6, the lockstep against QEMU. Not read: QEMU's
//! source, Arm's ASL or pseudocode (X1, X2).

use super::{
    Binary, DOUBLE, Format, SINGLE, binary, compare, convert, fpcr, fpsr, nan_of, nzcv, operand,
    operations, result, rounding,
};
use crate::helpers::softfloat::{self as sf, Rounding};

/// `value` with the other sign.
const fn negated(format: Format, value: u64) -> u64 {
    value ^ format.sign_bit()
}

/// The power of two `2^exponent` with the fraction `fraction_top` in the top
/// two bits of the fraction field: 2.0 is `(1, 0)` and 1.5 is `(0, 2)`.
const fn constant(format: Format, exponent: i32, fraction_top: u64) -> u64 {
    ((format.bias() + exponent) as u64) << format.fraction_bits()
        | fraction_top << (format.fraction_bits() - 2)
}

fn infinity_times_zero(format: Format, a: u64, b: u64) -> bool {
    (format.is_infinity(a) && format.is_zero(b)) || (format.is_zero(a) && format.is_infinity(b))
}

/// `FMULX`: the product, where infinity times zero is 2.0 with the sign of
/// the product and no exception.
pub fn mulx(format: Format, a: u64, b: u64, control: u32) -> (u64, u8) {
    let mut flags = 0;
    let a = operand(format, a, control, &mut flags);
    let b = operand(format, b, control, &mut flags);
    if let Some(value) = nan_of(format, &[a, b], control, &mut flags) {
        return (value, flags);
    }
    if infinity_times_zero(format, a, b) {
        let sign = format.sign(a) != format.sign(b);
        return (format.zero(sign) | constant(format, 1, 0), flags);
    }
    let raw = sf::call(operations(format).mul, rounding(control), a, b, 0);
    (result(format, raw, control, &mut flags), flags)
}

/// `FABD`: the magnitude of the difference.
pub fn abd(format: Format, a: u64, b: u64, control: u32) -> (u64, u8) {
    let (value, flags) = binary(Binary::Sub, format, a, b, control);
    (value & !format.sign_bit(), flags)
}

/// `constant - a * b` as one operation, where infinity times zero gives the
/// constant. `halve`: the result is half of that, rounded once.
fn step(format: Format, a: u64, b: u64, control: u32, halve: bool) -> (u64, u8) {
    let mut flags = 0;
    let a = operand(format, negated(format, a), control, &mut flags);
    let b = operand(format, b, control, &mut flags);
    if let Some(value) = nan_of(format, &[a, b], control, &mut flags) {
        return (value, flags);
    }
    // 2.0, or 3.0 / 2.
    let addend = if halve {
        constant(format, 0, 2)
    } else {
        constant(format, 1, 0)
    };
    if infinity_times_zero(format, a, b) {
        return (addend, flags);
    }
    let (mut a, mut b) = (a, b);
    if halve {
        // (3 + a * b) / 2 is 1.5 + (a / 2) * b. Half of an operand is exact
        // where its exponent has room. Where neither has, the product is so
        // far below 1.5 that half of it rounds the same way.
        let halved = |value: u64| {
            let field = format.exponent(value);
            (field >= 2 && field != format.exponent_max())
                .then(|| value - (1 << format.fraction_bits()))
        };
        if let Some(half) = halved(a) {
            a = half;
        } else if let Some(half) = halved(b) {
            b = half;
        }
    }
    let raw = sf::call(operations(format).mul_add, rounding(control), a, b, addend);
    (result(format, raw, control, &mut flags), flags)
}

/// `FRECPS`: `2 - a * b`.
pub fn recip_step(format: Format, a: u64, b: u64, control: u32) -> (u64, u8) {
    step(format, a, b, control, false)
}

/// `FRSQRTS`: `(3 - a * b) / 2`.
pub fn rsqrt_step(format: Format, a: u64, b: u64, control: u32) -> (u64, u8) {
    step(format, a, b, control, true)
}

/// The reciprocal estimate of a 9-bit value in [256, 512), which stands for
/// [0.5, 1): eight bits below an implied one.
pub const fn recip_estimate_integer(a: u64) -> u64 {
    let a = a * 2 + 1;
    let b = (1 << 19) / a;
    ((b + 1) / 2) & 0xff
}

const fn rsqrt_table() -> [u8; 512] {
    let mut table = [0u8; 512];
    let mut index = 128;
    while index < 512 {
        // Ten bits after the point, made odd.
        let a = if index < 256 {
            index * 2 + 1
        } else {
            (index | 1) * 2
        };
        // The largest b, nine bits after the point, below 1 / sqrt(a).
        let mut b = 512u64;
        while a * (b + 1) * (b + 1) < 1 << 28 {
            b += 1;
        }
        table[index as usize] = ((b + 1) / 2) as u8;
        index += 1;
    }
    table
}

static RSQRT_TABLE: [u8; 512] = rsqrt_table();

/// The reciprocal square root estimate of a 9-bit value in [128, 512), which
/// stands for [0.25, 1): eight bits below an implied one.
pub fn rsqrt_estimate_integer(a: u64) -> u64 {
    u64::from(RSQRT_TABLE[(a & 0x1ff) as usize])
}

/// `URECPE` of one 32-bit lane.
pub fn unsigned_recip_estimate(a: u32) -> u32 {
    if a >> 31 == 0 {
        return u32::MAX;
    }
    ((0x100 | recip_estimate_integer(u64::from(a >> 23))) << 23) as u32
}

/// `URSQRTE` of one 32-bit lane.
pub fn unsigned_rsqrt_estimate(a: u32) -> u32 {
    if a >> 30 == 0 {
        return u32::MAX;
    }
    ((0x100 | rsqrt_estimate_integer(u64::from(a >> 23))) << 23) as u32
}

/// A finite value that is not zero as its exponent and its fraction with the
/// leading one at the top and dropped: the value is `1.fraction * 2^exponent`.
fn normalized(format: Format, value: u64) -> (i32, u64) {
    let field = format.exponent(value) as i32;
    let fraction = format.fraction(value);
    if field != 0 {
        return (field - format.bias(), fraction);
    }
    // The leading one of a denormal goes to the place of the implied one.
    let shift = fraction.leading_zeros() - (63 - format.fraction_bits());
    (
        1 - format.bias() - shift as i32,
        (fraction << shift) & format.fraction_mask(),
    )
}

/// `FRECPE`: an estimate of `1 / a` with eight bits of fraction.
pub fn recip_estimate(format: Format, a: u64, control: u32) -> (u64, u8) {
    let mut flags = 0;
    let a = operand(format, a, control, &mut flags);
    if let Some(value) = nan_of(format, &[a], control, &mut flags) {
        return (value, flags);
    }
    let sign = format.sign(a);
    if format.is_infinity(a) {
        return (format.zero(sign), flags);
    }
    if format.is_zero(a) {
        return (format.infinity(sign), flags | fpsr::DZC);
    }
    let (exponent, fraction) = normalized(format, a);
    let exponent_min = 1 - format.bias();
    if exponent < exponent_min - 2 {
        // The reciprocal is above the largest number.
        let to_infinity = match rounding(control) {
            Rounding::NearEven | Rounding::NearMaxMag => true,
            Rounding::Max => !sign,
            Rounding::Min => sign,
            Rounding::MinMag => false,
        };
        let value = if to_infinity {
            format.infinity(sign)
        } else {
            format.max_normal(sign)
        };
        return (value, flags | fpsr::OFC | fpsr::IXC);
    }
    if control & fpcr::FZ != 0 && exponent >= -exponent_min {
        // The reciprocal is a denormal.
        return (format.zero(sign), flags | fpsr::UFC);
    }
    let fraction_bits = format.fraction_bits();
    let scaled = 0x100 | fraction >> (fraction_bits - 8);
    let mut estimate = recip_estimate_integer(scaled) << (fraction_bits - 8);
    let mut field = -(exponent + 1) + format.bias();
    if field < 1 {
        // A denormal result: the implied one becomes a bit of the fraction.
        estimate = (estimate | 1 << fraction_bits) >> (1 - field);
        field = 0;
    }
    (
        format.zero(sign) | (field as u64) << fraction_bits | (estimate & format.fraction_mask()),
        flags,
    )
}

/// `FRSQRTE`: an estimate of `1 / sqrt(a)` with eight bits of fraction.
pub fn rsqrt_estimate(format: Format, a: u64, control: u32) -> (u64, u8) {
    let mut flags = 0;
    let a = operand(format, a, control, &mut flags);
    if let Some(value) = nan_of(format, &[a], control, &mut flags) {
        return (value, flags);
    }
    if format.is_zero(a) {
        return (format.infinity(format.sign(a)), flags | fpsr::DZC);
    }
    if format.sign(a) {
        return (format.default_nan(), flags | fpsr::IOC);
    }
    if format.is_infinity(a) {
        return (format.zero(false), flags);
    }
    let (exponent, fraction) = normalized(format, a);
    let fraction_bits = format.fraction_bits();
    // An even exponent: the value is in [0.25, 0.5) of the next odd power of
    // four, and eight bits name it. An odd one: [0.5, 1), nine bits.
    let scaled = if exponent % 2 == 0 {
        0x80 | fraction >> (fraction_bits - 7)
    } else {
        0x100 | fraction >> (fraction_bits - 8)
    };
    let estimate = rsqrt_estimate_integer(scaled) << (fraction_bits - 8);
    let field = ((-(exponent + 1)) >> 1) + format.bias();
    ((field as u64) << fraction_bits | estimate, flags)
}

/// `FRECPX`: the exponent turned over, with a zero fraction.
pub fn recip_exponent(format: Format, a: u64, control: u32) -> (u64, u8) {
    let mut flags = 0;
    let a = operand(format, a, control, &mut flags);
    if let Some(value) = nan_of(format, &[a], control, &mut flags) {
        return (value, flags);
    }
    let sign = format.zero(format.sign(a));
    let field = format.exponent(a);
    let turned = if field == 0 {
        format.exponent_max() - 1
    } else {
        !field & format.exponent_max()
    };
    (sign | turned << format.fraction_bits(), flags)
}

/// Which compare a mask answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mask {
    Equal,
    GreaterEqual,
    Greater,
    /// The same of the magnitudes (`FACGE`, `FACGT`).
    MagnitudeGreaterEqual,
    MagnitudeGreater,
}

/// All ones where `a` compares with `b` as `mask` asks, else zero. Only the
/// compare for equality is quiet.
pub fn compare_mask(mask: Mask, format: Format, a: u64, b: u64, control: u32) -> (u64, u8) {
    let magnitude = matches!(mask, Mask::MagnitudeGreaterEqual | Mask::MagnitudeGreater);
    let (a, b) = if magnitude {
        (a & !format.sign_bit(), b & !format.sign_bit())
    } else {
        (a, b)
    };
    let (order, flags) = compare(format, a, b, mask != Mask::Equal, control);
    let holds = match mask {
        Mask::Equal => order == nzcv::EQUAL,
        Mask::GreaterEqual | Mask::MagnitudeGreaterEqual => {
            order == nzcv::EQUAL || order == nzcv::GREATER
        }
        Mask::Greater | Mask::MagnitudeGreater => order == nzcv::GREATER,
    };
    (if holds { format.mask() } else { 0 }, flags)
}

/// `FCVTXN`: double to single precision, rounded to odd: toward zero, and
/// the last bit set where the result is not exact.
pub fn convert_to_odd(a: u64, control: u32) -> (u64, u8) {
    let mut flags = 0;
    let a = operand(DOUBLE, a, control, &mut flags);
    if DOUBLE.is_nan(a) {
        return convert(DOUBLE, SINGLE, a, control);
    }
    let (mut value, raised) = sf::call(sf::F64_TO_F32, Rounding::MinMag, a, 0, 0);
    if raised & sf::flag::INEXACT != 0 {
        value |= 1;
    }
    (result(SINGLE, (value, raised), control, &mut flags), flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: u64 = 0x3ff0_0000_0000_0000;
    const TWO: u64 = 0x4000_0000_0000_0000;
    const INFINITY: u64 = 0x7ff0_0000_0000_0000;

    #[test]
    fn the_integer_estimates_have_their_known_ends() {
        // 1 / 0.5 is just below 2: all ones under the implied one.
        assert_eq!(recip_estimate_integer(256), 0xff);
        assert_eq!(recip_estimate_integer(511), 0x00);
        assert_eq!(rsqrt_estimate_integer(128), 0xff);
        assert_eq!(rsqrt_estimate_integer(511), 0x00);
        assert_eq!(unsigned_recip_estimate(0x7fff_ffff), u32::MAX);
        assert_eq!(unsigned_recip_estimate(0x8000_0000), 0xff80_0000);
        assert_eq!(unsigned_rsqrt_estimate(0x3fff_ffff), u32::MAX);
        assert_eq!(unsigned_rsqrt_estimate(0x4000_0000), 0xff80_0000);
    }

    #[test]
    fn mulx_gives_two_for_infinity_times_zero() {
        assert_eq!(mulx(DOUBLE, INFINITY, 0, 0), (TWO, 0));
        assert_eq!(mulx(DOUBLE, INFINITY, 1 << 63, 0), (TWO | 1 << 63, 0));
        assert_eq!(mulx(DOUBLE, TWO, TWO, 0), (0x4010_0000_0000_0000, 0));
        assert_eq!(mulx(SINGLE, 0x7f80_0000, 0, 0), (0x4000_0000, 0));
    }

    #[test]
    fn the_steps_are_fused_and_know_their_constants() {
        // 2 - 1 * 1 and (3 - 1 * 1) / 2.
        assert_eq!(recip_step(DOUBLE, ONE, ONE, 0), (ONE, 0));
        assert_eq!(rsqrt_step(DOUBLE, ONE, ONE, 0), (ONE, 0));
        assert_eq!(recip_step(DOUBLE, INFINITY, 0, 0), (TWO, 0));
        assert_eq!(
            rsqrt_step(DOUBLE, 0, INFINITY, 0),
            (0x3ff8_0000_0000_0000, 0)
        );
        // (3 - 2 * 2) / 2 is -0.5.
        assert_eq!(
            rsqrt_step(DOUBLE, TWO, TWO, 0),
            (0xbfe0_0000_0000_0000, 0)
        );
        // Two denormals: the product only makes the result inexact.
        assert_eq!(
            rsqrt_step(DOUBLE, 1, 1, 0),
            (0x3ff8_0000_0000_0000, fpsr::IXC)
        );
        // Toward zero the same product takes one unit off.
        assert_eq!(
            rsqrt_step(DOUBLE, 1, 1, 3 << 22),
            (0x3ff7_ffff_ffff_ffff, fpsr::IXC)
        );
    }

    #[test]
    fn the_estimates_of_special_values() {
        assert_eq!(recip_estimate(DOUBLE, 0, 0), (INFINITY, fpsr::DZC));
        assert_eq!(recip_estimate(DOUBLE, INFINITY, 0), (0, 0));
        // 1 / 1 is 0.998 by the table: 0x3fef_f000...
        assert_eq!(recip_estimate(DOUBLE, ONE, 0), (0x3fef_f000_0000_0000, 0));
        assert_eq!(recip_estimate(SINGLE, 0x3f80_0000, 0), (0x3f7f_8000, 0));
        // The smallest denormal: the reciprocal overflows.
        assert_eq!(
            recip_estimate(DOUBLE, 1, 0),
            (INFINITY, fpsr::OFC | fpsr::IXC)
        );
        assert_eq!(rsqrt_estimate(DOUBLE, 0, 0), (INFINITY, fpsr::DZC));
        assert_eq!(
            rsqrt_estimate(DOUBLE, ONE | 1 << 63, 0),
            (DOUBLE.default_nan(), fpsr::IOC)
        );
        assert_eq!(rsqrt_estimate(DOUBLE, ONE, 0), (0x3fef_f000_0000_0000, 0));
        // 1 / sqrt(4) is 0.5 by the table: 0x3fdf_f000...
        assert_eq!(
            rsqrt_estimate(DOUBLE, 0x4010_0000_0000_0000, 0),
            (0x3fdf_f000_0000_0000, 0)
        );
        assert_eq!(recip_exponent(DOUBLE, 0, 0), (0x7fe0_0000_0000_0000, 0));
        assert_eq!(recip_exponent(DOUBLE, ONE, 0), (0x4000_0000_0000_0000, 0));
    }

    #[test]
    fn masks_and_the_rounding_to_odd() {
        assert_eq!(
            compare_mask(Mask::Equal, DOUBLE, ONE, ONE, 0),
            (u64::MAX, 0)
        );
        assert_eq!(compare_mask(Mask::Greater, DOUBLE, ONE, ONE, 0), (0, 0));
        assert_eq!(
            compare_mask(Mask::MagnitudeGreater, SINGLE, 0xc000_0000, 0x3f80_0000, 0),
            (0xffff_ffff, 0)
        );
        // A quiet NaN is invalid for every compare but the one for equality.
        let nan = DOUBLE.default_nan();
        assert_eq!(compare_mask(Mask::Equal, DOUBLE, nan, ONE, 0), (0, 0));
        assert_eq!(
            compare_mask(Mask::GreaterEqual, DOUBLE, nan, ONE, 0),
            (0, fpsr::IOC)
        );
        // 1 + 2^-52 is not a single: toward zero, and the last bit set.
        assert_eq!(convert_to_odd(ONE | 1, 0), (0x3f80_0001, fpsr::IXC));
        assert_eq!(convert_to_odd(ONE, 0), (0x3f80_0000, 0));
    }
}
