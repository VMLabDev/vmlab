use super::*;
use crate::oracle::stream::Rng;

const RN: u32 = 0;
const RP: u32 = 1 << fpcr::RMODE_SHIFT;
const RM: u32 = 2 << fpcr::RMODE_SHIFT;
const RZ: u32 = 3 << fpcr::RMODE_SHIFT;
const MODES: [u32; 4] = [RN, RP, RM, RZ];

/// Values of `format` that sit where the rules change, and random ones.
fn samples(format: Format, rng: &mut Rng, random: usize) -> Vec<u64> {
    let fraction_bits = format.fraction_bits();
    let bias = format.bias() as u64;
    let one = bias << fraction_bits;
    let mut values = vec![
        0,
        1,
        format.fraction_mask(),
        1 << fraction_bits,
        (1 << fraction_bits) + 1,
        format.max_normal(false),
        format.infinity(false),
        format.default_nan(),
        format.infinity(false) | 1,
        format.infinity(false) | format.quiet_bit() | 0x55,
        one,
        one - 1,
        one + 1,
        one | format.quiet_bit(),
        (bias - 1) << fraction_bits,
        ((bias - 1) << fraction_bits) + 1,
        ((bias - 1) << fraction_bits) - 1,
        (bias + 1) << fraction_bits | format.quiet_bit() >> 1,
    ];
    // Around every power of two that an integer conversion saturates at.
    for power in [7u64, 8, 15, 16, 30, 31, 32, 33, 62, 63, 64, 65] {
        let exact = (bias + power) << fraction_bits;
        values.extend([exact - 1, exact, exact + 1, exact | format.fraction_mask()]);
    }
    for _ in 0..random {
        let raw = rng.next_u64() & format.mask();
        values.push(match rng.below(4) {
            // A number with an exponent near the integers.
            0 => {
                let exponent = bias - 3 + rng.below(70);
                (raw & (format.fraction_mask() | format.sign_bit())) | exponent << fraction_bits
            }
            // A number with few set bits in its fraction: ties.
            1 => {
                let exponent = bias - 3 + rng.below(40);
                let top = (raw >> (fraction_bits - 3)) & 7;
                top << (fraction_bits - 3) | exponent << fraction_bits
            }
            _ => raw,
        });
    }
    let negatives: Vec<u64> = values
        .iter()
        .map(|value| value ^ format.sign_bit())
        .collect();
    values.extend(negatives);
    values
}

#[test]
fn a_conversion_to_an_integer_is_softfloats_for_every_mode_and_width() {
    let mut rng = Rng(11);
    let mut compared = 0u64;
    for format in [SINGLE, DOUBLE] {
        let values = samples(format, &mut rng, 4000);
        for (integer_bits, unsigned, name) in [
            (32, false, "i32"),
            (32, true, "ui32"),
            (64, false, "i64"),
            (64, true, "ui64"),
        ] {
            let operation = sf::operation(&format!("f{}_to_{name}", format.bits)).unwrap();
            for mode in [
                Rounding::NearEven,
                Rounding::MinMag,
                Rounding::Min,
                Rounding::Max,
                Rounding::NearMaxMag,
            ] {
                for value in &values {
                    let (expected, raised) = sf::call(operation, mode, *value, 0, 0);
                    let ours = to_fixed(format, *value, integer_bits, unsigned, 0, mode, 0);
                    assert_eq!(
                        ours,
                        (expected, cumulative(raised)),
                        "f{}_to_{name} of {value:#x} in {mode:?}",
                        format.bits
                    );
                    compared += 1;
                }
            }
        }
    }
    assert!(compared > 300_000);
}

#[test]
fn a_fixed_point_conversion_is_the_integer_conversion_of_the_scaled_value() {
    // 1.75 with two fraction bits is 7, exactly.
    let value = 0x3ffc_0000_0000_0000u64;
    assert_eq!(
        to_fixed(DOUBLE, value, 32, false, 2, Rounding::MinMag, 0),
        (7, 0)
    );
    // With one fraction bit it is 3.5, toward zero 3, and inexact.
    assert_eq!(
        to_fixed(DOUBLE, value, 32, false, 1, Rounding::MinMag, 0),
        (3, fpsr::IXC)
    );
    // -1.75 with 64 fraction bits is past the range of 64 bits.
    assert_eq!(
        to_fixed(DOUBLE, value | 1 << 63, 64, false, 64, Rounding::MinMag, 0),
        (1 << 63, fpsr::IOC)
    );
    // 2^-64 with 64 fraction bits is 1.
    let tiny = (1023u64 - 64) << 52;
    assert_eq!(
        to_fixed(DOUBLE, tiny, 64, true, 64, Rounding::MinMag, 0),
        (1, 0)
    );
    // A negative value that rounds to zero is in the unsigned range.
    assert_eq!(
        to_fixed(SINGLE, 0xbe99_999a, 32, true, 0, Rounding::MinMag, 0),
        (0, fpsr::IXC)
    );
    assert_eq!(
        to_fixed(SINGLE, 0xbf80_0000, 32, true, 0, Rounding::MinMag, 0),
        (0, fpsr::IOC)
    );
    // A NaN is zero and invalid. With flush-to-zero a denormal is zero.
    assert_eq!(
        to_fixed(SINGLE, 0x7fc0_0000, 64, false, 0, Rounding::MinMag, 0),
        (0, fpsr::IOC)
    );
    assert_eq!(
        to_fixed(SINGLE, 1, 64, false, 0, Rounding::Max, fpcr::FZ),
        (0, fpsr::IDC)
    );
    assert_eq!(
        to_fixed(SINGLE, 1, 64, false, 0, Rounding::Max, 0),
        (1, fpsr::IXC)
    );
    // The scaled forms agree with a multiplication by a power of two.
    let mut rng = Rng(5);
    for value in samples(DOUBLE, &mut rng, 2000) {
        for fraction_bits in [1u32, 7, 31, 32, 63, 64] {
            if DOUBLE.is_nan(value) {
                continue;
            }
            let scale = (1023 + u64::from(fraction_bits)) << 52;
            let (scaled, raised) = sf::call(sf::F64_MUL, Rounding::NearEven, value, scale, 0);
            // A product that overflows is infinite, and still past the range.
            let _ = raised;
            for mode in [Rounding::MinMag, Rounding::NearEven, Rounding::NearMaxMag] {
                assert_eq!(
                    to_fixed(DOUBLE, value, 64, false, fraction_bits, mode, 0),
                    to_fixed(DOUBLE, scaled, 64, false, 0, mode, 0),
                    "{value:#x} with {fraction_bits} fraction bits in {mode:?}"
                );
            }
        }
    }
}

#[test]
fn rounding_to_half_precision_is_softfloats_in_the_ieee_format() {
    let mut rng = Rng(23);
    let mut compared = 0u64;
    for (format, operation) in [(SINGLE, sf::F32_TO_F16), (DOUBLE, sf::F64_TO_F16)] {
        let mut values = samples(format, &mut rng, 3000);
        // Values in and around the range of half precision.
        for _ in 0..40_000 {
            let exponent = format.bias() as u64 - 30 + rng.below(50);
            let raw = rng.next_u64() & format.mask();
            let fraction = if rng.chance(1, 2) {
                // Few bits: exact values and ties.
                (raw >> (format.fraction_bits() - 12)) << (format.fraction_bits() - 12)
            } else {
                raw
            };
            values.push(
                (fraction & (format.fraction_mask() | format.sign_bit()))
                    | exponent << format.fraction_bits(),
            );
        }
        for (mode, bits) in [
            (Rounding::NearEven, RN),
            (Rounding::Max, RP),
            (Rounding::Min, RM),
            (Rounding::MinMag, RZ),
        ] {
            assert_eq!(rounding(bits), mode);
            for value in &values {
                if format.is_nan(*value) || format.is_infinity(*value) || format.is_zero(*value) {
                    continue;
                }
                let (expected, raised) = sf::call(operation, mode, *value, 0, 0);
                let (significand, exponent) = unpack(format, *value);
                let ours = round_to_half(format.sign(*value), significand, exponent, mode, false);
                assert_eq!(
                    ours,
                    (expected, cumulative(raised)),
                    "{value:#x} of {} bits in {mode:?}",
                    format.bits
                );
                // The conversion itself gives the same.
                assert_eq!(convert(format, HALF, *value, bits), ours);
                compared += 1;
            }
        }
    }
    assert!(compared > 300_000);
}

#[test]
fn the_alternative_half_precision_format_has_one_more_binade_and_no_nan() {
    let ahp = fpcr::AHP;
    // The exponent field 31 is 2^16 times the significand.
    assert_eq!(convert(HALF, SINGLE, 0x7c00, ahp), (0x4780_0000, 0));
    assert_eq!(convert(HALF, SINGLE, 0xfe00, ahp), (0xc7c0_0000, 0));
    assert_eq!(
        convert(HALF, DOUBLE, 0x7fff, ahp),
        (0x40ff_fc00_0000_0000, 0)
    );
    // In the IEEE format the same bits are an infinity and a NaN.
    assert_eq!(convert(HALF, SINGLE, 0x7c00, 0), (0x7f80_0000, 0));
    assert_eq!(convert(HALF, SINGLE, 0xfe00, 0), (0xffc0_0000, 0));
    assert_eq!(convert(HALF, SINGLE, 0x7c01, 0), (0x7fc0_2000, fpsr::IOC));
    // Numbers below the top binade are the same in both formats.
    for value in [0u64, 0x8000, 1, 0x03ff, 0x0400, 0x3c00, 0xbc00, 0x7bff] {
        assert_eq!(
            convert(HALF, SINGLE, value, ahp),
            convert(HALF, SINGLE, value, 0)
        );
        assert_eq!(
            convert(HALF, DOUBLE, value, ahp),
            convert(HALF, DOUBLE, value, 0)
        );
    }
    // To the alternative format: 65536 fits, where the IEEE format overflows.
    assert_eq!(convert(SINGLE, HALF, 0x4780_0000, ahp), (0x7c00, 0));
    assert_eq!(
        convert(SINGLE, HALF, 0x4780_0000, 0),
        (0x7c00, fpsr::OFC | fpsr::IXC)
    );
    // 131072 does not fit: the largest magnitude, and invalid.
    assert_eq!(convert(SINGLE, HALF, 0x4800_0000, ahp), (0x7fff, fpsr::IOC));
    assert_eq!(convert(SINGLE, HALF, 0xc800_0000, ahp), (0xffff, fpsr::IOC));
    // An infinity and a NaN have no encoding there.
    assert_eq!(
        convert(DOUBLE, HALF, DOUBLE.infinity(true), ahp),
        (0xffff, fpsr::IOC)
    );
    assert_eq!(
        convert(DOUBLE, HALF, DOUBLE.default_nan(), ahp),
        (0, fpsr::IOC)
    );
    assert_eq!(
        convert(DOUBLE, HALF, DOUBLE.default_nan() | 1 << 63, ahp),
        (0x8000, fpsr::IOC)
    );
    // A round trip through the top binade is exact.
    for value in [0x7c00u64, 0x7c01, 0xffff, 0xfc00] {
        let (wide, flags) = convert(HALF, DOUBLE, value, ahp);
        assert_eq!(flags, 0);
        assert_eq!(convert(DOUBLE, HALF, wide, ahp), (value, 0));
    }
}

#[test]
fn a_nan_result_is_the_first_signalling_operand_else_the_first_quiet_one() {
    let quiet_a = 0x7fc0_0001u64;
    let quiet_b = 0xffc0_0002u64;
    let signal_a = 0x7f80_0011u64;
    let signal_b = 0xff80_0022u64;
    let one = 0x3f80_0000u64;
    for operation in [
        Binary::Add,
        Binary::Sub,
        Binary::Mul,
        Binary::Div,
        Binary::Max,
        Binary::Min,
    ] {
        assert_eq!(binary(operation, SINGLE, quiet_a, quiet_b, 0), (quiet_a, 0));
        assert_eq!(binary(operation, SINGLE, one, quiet_b, 0), (quiet_b, 0));
        assert_eq!(
            binary(operation, SINGLE, quiet_a, signal_b, 0),
            (signal_b | 0x40_0000, fpsr::IOC)
        );
        assert_eq!(
            binary(operation, SINGLE, signal_a, signal_b, 0),
            (signal_a | 0x40_0000, fpsr::IOC)
        );
        // The default NaN mode gives one NaN for all of them.
        assert_eq!(
            binary(operation, SINGLE, signal_a, quiet_b, fpcr::DN),
            (0x7fc0_0000, fpsr::IOC)
        );
        assert_eq!(
            binary(operation, SINGLE, one, quiet_b, fpcr::DN),
            (0x7fc0_0000, 0)
        );
    }
    // An invalid operation gives the default NaN.
    let infinity = SINGLE.infinity(false);
    assert_eq!(
        binary(Binary::Sub, SINGLE, infinity, infinity, 0),
        (0x7fc0_0000, fpsr::IOC)
    );
    assert_eq!(
        binary(Binary::Mul, SINGLE, infinity, 0, 0),
        (0x7fc0_0000, fpsr::IOC)
    );
    assert_eq!(
        binary(Binary::Div, SINGLE, 0, 0, 0),
        (0x7fc0_0000, fpsr::IOC)
    );
    assert_eq!(
        binary(Binary::Div, SINGLE, one, 0, 0),
        (infinity, fpsr::DZC)
    );
    assert_eq!(
        unary(Unary::Sqrt, DOUBLE, 0xbff0_0000_0000_0000, 0),
        (0x7ff8_0000_0000_0000, fpsr::IOC)
    );
    assert_eq!(unary(Unary::Sqrt, DOUBLE, 1 << 63, 0), (1 << 63, 0));
    // The numeric maximum takes the number beside a quiet NaN, in both places.
    assert_eq!(binary(Binary::MaxNumber, SINGLE, quiet_a, one, 0), (one, 0));
    assert_eq!(binary(Binary::MinNumber, SINGLE, one, quiet_b, 0), (one, 0));
    assert_eq!(
        binary(Binary::MaxNumber, SINGLE, quiet_a, quiet_b, 0),
        (quiet_a, 0)
    );
    assert_eq!(
        binary(Binary::MaxNumber, SINGLE, signal_a, one, 0),
        (signal_a | 0x40_0000, fpsr::IOC)
    );
    assert_eq!(
        binary(Binary::MinNumber, SINGLE, quiet_a, SINGLE.infinity(true), 0),
        (SINGLE.infinity(true), 0)
    );
}

#[test]
fn a_maximum_of_two_zeros_is_negative_only_when_both_are() {
    let (plus, minus) = (0u64, 1u64 << 63);
    assert_eq!(binary(Binary::Max, DOUBLE, plus, minus, 0), (plus, 0));
    assert_eq!(binary(Binary::Max, DOUBLE, minus, plus, 0), (plus, 0));
    assert_eq!(binary(Binary::Max, DOUBLE, minus, minus, 0), (minus, 0));
    assert_eq!(binary(Binary::Min, DOUBLE, plus, minus, 0), (minus, 0));
    assert_eq!(binary(Binary::Min, DOUBLE, plus, plus, 0), (plus, 0));
    let one = 0x3ff0_0000_0000_0000u64;
    let two = 0x4000_0000_0000_0000u64;
    assert_eq!(binary(Binary::Max, DOUBLE, one, two, 0), (two, 0));
    assert_eq!(binary(Binary::Min, DOUBLE, one, two, 0), (one, 0));
    assert_eq!(
        binary(Binary::Max, DOUBLE, one | minus, two | minus, 0),
        (one | minus, 0)
    );
    assert_eq!(
        binary(Binary::Min, DOUBLE, DOUBLE.infinity(true), two, 0),
        (DOUBLE.infinity(true), 0)
    );
    // A denormal stays what it is without flush-to-zero.
    assert_eq!(binary(Binary::Max, DOUBLE, 1, 0, 0), (1, 0));
    assert_eq!(
        binary(Binary::Max, DOUBLE, 1, minus, fpcr::FZ),
        (0, fpsr::IDC)
    );
}

#[test]
fn flush_to_zero_replaces_denormal_operands_and_tiny_results() {
    let fz = fpcr::FZ;
    let smallest_normal = 0x0080_0000u64;
    let half = 0x3f00_0000u64;
    // A denormal operand is zero, and IDC records it.
    assert_eq!(
        binary(Binary::Add, SINGLE, 1, 0x3f80_0000, fz),
        (0x3f80_0000, fpsr::IDC)
    );
    assert_eq!(
        binary(Binary::Add, SINGLE, 1, 0x3f80_0000, 0),
        (0x3f80_0000, fpsr::IXC)
    );
    // A result that is tiny and exact: a denormal without the flag, a zero with UFC.
    assert_eq!(
        binary(Binary::Mul, SINGLE, smallest_normal, half, 0),
        (0x0040_0000, 0)
    );
    assert_eq!(
        binary(Binary::Mul, SINGLE, smallest_normal, half, fz),
        (0, fpsr::UFC)
    );
    assert_eq!(
        binary(Binary::Mul, SINGLE, smallest_normal | 1 << 31, half, fz),
        (1 << 31, fpsr::UFC)
    );
    // A result that is tiny before rounding and rounds up to the smallest
    // normal number is flushed as well.
    assert_eq!(
        binary(Binary::Mul, SINGLE, 0x00ff_ffff, half, 0),
        (smallest_normal, fpsr::UFC | fpsr::IXC)
    );
    assert_eq!(
        binary(Binary::Mul, SINGLE, 0x00ff_ffff, half, fz),
        (0, fpsr::UFC)
    );
    // A conversion to single precision flushes its result, and one to half
    // precision does not.
    let tiny_double = (1023u64 - 130) << 52;
    assert_eq!(convert(DOUBLE, SINGLE, tiny_double, 0), (1 << 19, 0));
    assert_eq!(convert(DOUBLE, SINGLE, tiny_double, fz), (0, fpsr::UFC));
    let tiny_for_half = 0x3800_0000u64;
    assert_eq!(
        convert(SINGLE, HALF, tiny_for_half, fz),
        convert(SINGLE, HALF, tiny_for_half, 0)
    );
    assert_eq!(convert(SINGLE, HALF, tiny_for_half, 0), (1 << 9, 0));
    // A compare sees the flushed operand.
    assert_eq!(compare(SINGLE, 1, 0, false, fz), (nzcv::EQUAL, fpsr::IDC));
    assert_eq!(compare(SINGLE, 1, 0, false, 0), (nzcv::GREATER, 0));
}

#[test]
fn a_fused_multiply_add_takes_its_nan_from_the_addend_first() {
    let quiet = |payload: u64| 0x7ff8_0000_0000_0000u64 | payload;
    let one = 0x3ff0_0000_0000_0000u64;
    let two = 0x4000_0000_0000_0000u64;
    let three = 0x4008_0000_0000_0000u64;
    let infinity = DOUBLE.infinity(false);
    let fused = |a, n, m| mul_add(DOUBLE, a, n, m, false, false, 0);
    assert_eq!(fused(quiet(1), quiet(2), quiet(3)), (quiet(1), 0));
    assert_eq!(fused(one, quiet(2), quiet(3)), (quiet(2), 0));
    assert_eq!(fused(one, two, quiet(3)), (quiet(3), 0));
    let signal = 0x7ff0_0000_0000_0007u64;
    assert_eq!(fused(quiet(1), two, signal), (quiet(7), fpsr::IOC));
    // A quiet addend does not hide infinity times zero.
    assert_eq!(
        fused(quiet(1), infinity, 0),
        (DOUBLE.default_nan(), fpsr::IOC)
    );
    assert_eq!(
        fused(quiet(1), 0, infinity),
        (DOUBLE.default_nan(), fpsr::IOC)
    );
    assert_eq!(fused(one, infinity, 0), (DOUBLE.default_nan(), fpsr::IOC));
    // 1 + 2 * 3, and the three negated forms.
    assert_eq!(fused(one, two, three), (0x401c_0000_0000_0000, 0));
    assert_eq!(
        mul_add(DOUBLE, one, two, three, false, true, 0),
        (0xc014_0000_0000_0000, 0)
    );
    assert_eq!(
        mul_add(DOUBLE, one, two, three, true, true, 0),
        (0xc01c_0000_0000_0000, 0)
    );
    assert_eq!(
        mul_add(DOUBLE, one, two, three, true, false, 0),
        (0x4014_0000_0000_0000, 0)
    );
    // One rounding: (1 + 2^-52) * (1 - 2^-52) - 1 is -2^-104, exactly. With
    // two roundings the product is 1, and the result zero.
    let above_one = one + 1;
    let below_one = one - 2;
    assert_eq!(
        mul_add(DOUBLE, one, above_one, below_one, true, false, 0),
        (0xb970_0000_0000_0000, 0)
    );
}

#[test]
fn a_compare_orders_numbers_and_calls_a_nan_unordered() {
    let one = 0x3f80_0000u64;
    let minus_one = 0xbf80_0000u64;
    let quiet = 0x7fc0_0000u64;
    let signal = 0x7fa0_0000u64;
    assert_eq!(compare(SINGLE, one, one, false, 0), (nzcv::EQUAL, 0));
    assert_eq!(compare(SINGLE, 0, 1 << 31, true, 0), (nzcv::EQUAL, 0));
    assert_eq!(compare(SINGLE, minus_one, one, false, 0), (nzcv::LESS, 0));
    assert_eq!(
        compare(SINGLE, one, minus_one, false, 0),
        (nzcv::GREATER, 0)
    );
    assert_eq!(
        compare(SINGLE, SINGLE.infinity(true), minus_one, false, 0),
        (nzcv::LESS, 0)
    );
    assert_eq!(compare(SINGLE, quiet, one, false, 0), (nzcv::UNORDERED, 0));
    assert_eq!(
        compare(SINGLE, quiet, one, true, 0),
        (nzcv::UNORDERED, fpsr::IOC)
    );
    assert_eq!(
        compare(SINGLE, one, signal, false, 0),
        (nzcv::UNORDERED, fpsr::IOC)
    );
    // The helper packs the flags below the bits for FPSR.
    let HelperFn::Pure(function) = FP_COMPARE.function else {
        panic!("a pure helper");
    };
    let packed = function(&[
        u128::from(compare_code(SINGLE, true)),
        u128::from(quiet),
        u128::from(one),
        0,
    ]);
    assert_eq!(
        packed,
        u128::from(nzcv::UNORDERED) | u128::from(fpsr::IOC) << 64
    );
}

#[test]
fn rounding_to_an_integral_value_follows_its_mode() {
    let value = |text: f64| text.to_bits();
    for (operation, input, expected) in [
        (Unary::RoundNearEven, 2.5, 2.0),
        (Unary::RoundNearEven, 3.5, 4.0),
        (Unary::RoundTiesAway, 2.5, 3.0),
        (Unary::RoundTiesAway, -2.5, -3.0),
        (Unary::RoundUp, 2.1, 3.0),
        (Unary::RoundUp, -2.1, -2.0),
        (Unary::RoundDown, 2.9, 2.0),
        (Unary::RoundDown, -2.1, -3.0),
        (Unary::RoundToZero, -2.9, -2.0),
        (Unary::RoundDown, -0.5, -1.0),
    ] {
        assert_eq!(
            unary(operation, DOUBLE, value(input), RP),
            (value(expected), 0),
            "{operation:?} of {input}"
        );
    }
    // A result of zero keeps the sign of the operand.
    assert_eq!(unary(Unary::RoundUp, DOUBLE, value(-0.5), 0), (1 << 63, 0));
    // The two forms that round in the mode of FPCR. One of them is exact.
    assert_eq!(
        unary(Unary::RoundCurrent, DOUBLE, value(2.5), RP),
        (value(3.0), 0)
    );
    assert_eq!(
        unary(Unary::RoundCurrent, DOUBLE, value(2.5), RZ),
        (value(2.0), 0)
    );
    assert_eq!(
        unary(Unary::RoundExact, DOUBLE, value(2.5), RM),
        (value(2.0), fpsr::IXC)
    );
    assert_eq!(
        unary(Unary::RoundExact, DOUBLE, value(2.0), RM),
        (value(2.0), 0)
    );
    // A signalling NaN is made quiet, with an invalid operation.
    assert_eq!(
        unary(Unary::RoundToZero, SINGLE, 0x7f80_0001, 0),
        (0x7fc0_0001, fpsr::IOC)
    );
    assert_eq!(unary(Unary::Sqrt, DOUBLE, value(2.25), RN), (value(1.5), 0));
}

#[test]
fn an_integer_becomes_a_value_with_one_rounding() {
    let value = |text: f64| text.to_bits();
    assert_eq!(from_fixed(DOUBLE, 7, 32, false, 0, 0), (value(7.0), 0));
    assert_eq!(from_fixed(DOUBLE, 7, 32, false, 2, 0), (value(1.75), 0));
    assert_eq!(
        from_fixed(DOUBLE, 0xffff_ffff, 32, false, 0, 0),
        (value(-1.0), 0)
    );
    assert_eq!(
        from_fixed(DOUBLE, 0xffff_ffff, 32, true, 0, 0),
        (value(4294967295.0), 0)
    );
    // Bits above a 32-bit integer are not part of it.
    assert_eq!(
        from_fixed(DOUBLE, 0xdead_0000_0000_0003, 32, true, 0, 0),
        (value(3.0), 0)
    );
    assert_eq!(from_fixed(DOUBLE, 0, 64, false, 64, 0), (0, 0));
    assert_eq!(
        from_fixed(DOUBLE, 1, 64, true, 64, 0),
        ((1023 - 64) << 52, 0)
    );
    // 2^24 + 1 does not fit single precision: the mode decides.
    let single = |text: f32| u64::from(text.to_bits());
    assert_eq!(
        from_fixed(SINGLE, 16_777_217, 32, false, 0, RN),
        (single(16_777_216.0), fpsr::IXC)
    );
    assert_eq!(
        from_fixed(SINGLE, 16_777_217, 32, false, 0, RP),
        (single(16_777_218.0), fpsr::IXC)
    );
    assert_eq!(
        from_fixed(SINGLE, u64::MAX, 64, true, 0, RZ),
        (single(18_446_742_974_197_923_840.0), fpsr::IXC)
    );
    assert_eq!(
        from_fixed(SINGLE, u64::MAX, 64, true, 64, RN),
        (single(1.0), fpsr::IXC)
    );
    // Against the division that the scaled form stands for.
    let mut rng = Rng(3);
    for _ in 0..20_000 {
        let x = rng.next_u64() >> rng.below(64);
        let fraction_bits = 1 + rng.below(64) as u32;
        for mode in MODES {
            let (whole, _) = sf::call(sf::UI64_TO_F64, rounding(mode), x, 0, 0);
            let scale = (1023 + u64::from(fraction_bits)) << 52;
            let (expected, _) = sf::call(sf::F64_DIV, rounding(mode), whole, scale, 0);
            assert_eq!(
                from_fixed(DOUBLE, x, 64, true, fraction_bits, mode).0,
                expected,
                "{x:#x} with {fraction_bits} fraction bits"
            );
        }
    }
}

#[test]
fn conversions_between_single_and_double_keep_a_nan_payload() {
    assert_eq!(
        convert(SINGLE, DOUBLE, 0x3fc0_0000, 0),
        (0x3ff8_0000_0000_0000, 0)
    );
    assert_eq!(
        convert(DOUBLE, SINGLE, 0x3ff8_0000_0000_0000, 0),
        (0x3fc0_0000, 0)
    );
    // 0.1 is not a single-precision value.
    assert_eq!(
        convert(DOUBLE, SINGLE, 0x3fb9_9999_9999_999a, RN),
        (0x3dcc_cccd, fpsr::IXC)
    );
    assert_eq!(
        convert(DOUBLE, SINGLE, 0x3fb9_9999_9999_999a, RZ),
        (0x3dcc_cccc, fpsr::IXC)
    );
    assert_eq!(
        convert(DOUBLE, SINGLE, 0x7fef_ffff_ffff_ffff, RN),
        (0x7f80_0000, fpsr::OFC | fpsr::IXC)
    );
    assert_eq!(
        convert(DOUBLE, SINGLE, 0x7fef_ffff_ffff_ffff, RZ),
        (0x7f7f_ffff, fpsr::OFC | fpsr::IXC)
    );
    // The sign and the top of the payload go across, and the NaN is quiet.
    assert_eq!(
        convert(SINGLE, DOUBLE, 0xff80_0001, 0),
        (0xfff8_0000_2000_0000, fpsr::IOC)
    );
    assert_eq!(
        convert(SINGLE, DOUBLE, 0x7fc0_0001, 0),
        (0x7ff8_0000_2000_0000, 0)
    );
    assert_eq!(
        convert(DOUBLE, SINGLE, 0x7ff0_0000_2000_0001, 0),
        (0x7fc0_0001, fpsr::IOC)
    );
    assert_eq!(
        convert(SINGLE, DOUBLE, 0xff80_0001, fpcr::DN),
        (0x7ff8_0000_0000_0000, fpsr::IOC)
    );
    assert_eq!(convert(HALF, DOUBLE, 0x3c00, 0), (0x3ff0_0000_0000_0000, 0));
    assert_eq!(convert(DOUBLE, HALF, 0x3ff0_0000_0000_0000, 0), (0x3c00, 0));
}

#[test]
fn the_immediate_of_fmov_expands_to_its_value() {
    assert_eq!(expand_immediate(SINGLE, 0x70), u64::from(1.0f32.to_bits()));
    assert_eq!(expand_immediate(DOUBLE, 0x70), 1.0f64.to_bits());
    assert_eq!(expand_immediate(SINGLE, 0x00), u64::from(2.0f32.to_bits()));
    assert_eq!(expand_immediate(DOUBLE, 0xf0), (-1.0f64).to_bits());
    assert_eq!(expand_immediate(DOUBLE, 0x3f), 31.0f64.to_bits());
    assert_eq!(
        expand_immediate(SINGLE, 0x40),
        u64::from(0.125f32.to_bits())
    );
    // Every immediate is sixteenths of a power of two from 1/8 to 16.
    for immediate in 0..=255u32 {
        let value = f64::from_bits(expand_immediate(DOUBLE, immediate));
        let single = f32::from_bits(expand_immediate(SINGLE, immediate) as u32);
        assert_eq!(f64::from(single), value);
        assert!(
            (0.125..=31.0).contains(&value.abs()),
            "{immediate:#x}: {value}"
        );
    }
}

#[test]
fn the_helpers_decode_their_constants() {
    let run = |helper: &Helper, args: &[u128]| {
        let HelperFn::Pure(function) = helper.function else {
            panic!("a pure helper");
        };
        assert_eq!(usize::from(helper.args), args.len(), "{}", helper.name);
        let packed = function(args);
        (packed as u64, (packed >> 64) as u8)
    };
    let double = |text: f64| u128::from(text.to_bits());
    for operation in Binary::ALL {
        let expected = binary(operation, DOUBLE, 3.0f64.to_bits(), 0.3f64.to_bits(), RP);
        assert_eq!(
            run(
                &FP_BINARY,
                &[
                    u128::from(binary_code(operation, DOUBLE)),
                    double(3.0),
                    double(0.3),
                    u128::from(RP)
                ]
            ),
            expected
        );
    }
    for operation in Unary::ALL {
        let expected = unary(operation, SINGLE, u64::from(2.5f32.to_bits()), RM);
        assert_eq!(
            run(
                &FP_UNARY,
                &[
                    u128::from(unary_code(operation, SINGLE)),
                    u128::from(2.5f32.to_bits()),
                    u128::from(RM)
                ]
            ),
            expected
        );
    }
    assert_eq!(
        run(
            &FP_MUL_ADD,
            &[
                u128::from(mul_add_code(DOUBLE, true, false)),
                double(1.0),
                double(2.0),
                double(3.0),
                0
            ]
        ),
        (5.0f64.to_bits(), 0)
    );
    assert_eq!(
        run(
            &FP_CONVERT,
            &[u128::from(convert_code(DOUBLE, HALF)), double(1.0), 0]
        ),
        (0x3c00, 0)
    );
    for rounding in [
        ToInteger::NearEven,
        ToInteger::Up,
        ToInteger::Down,
        ToInteger::ToZero,
        ToInteger::TiesAway,
    ] {
        let code = to_fixed_code(DOUBLE, 64, true, 3, rounding);
        assert_eq!(
            run(&FP_TO_FIXED, &[u128::from(code), double(0.3125), 0]),
            to_fixed(DOUBLE, 0.3125f64.to_bits(), 64, true, 3, rounding.mode(), 0)
        );
    }
    assert_eq!(
        run(
            &FP_TO_FIXED,
            &[
                u128::from(to_fixed_code(SINGLE, 32, false, 0, ToInteger::ToZero)),
                u128::from((-2.5f32).to_bits()),
                0
            ]
        ),
        (0xffff_fffe, fpsr::IXC)
    );
    assert_eq!(
        run(
            &FP_FROM_FIXED,
            &[
                u128::from(from_fixed_code(DOUBLE, 64, false, 64)),
                1 << 62,
                0
            ]
        ),
        (0.25f64.to_bits(), 0)
    );
}
