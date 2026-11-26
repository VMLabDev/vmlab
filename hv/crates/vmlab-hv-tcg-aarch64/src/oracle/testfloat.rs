//! Berkeley TestFloat against the FP helpers (DESIGN.md O5, source S5).
//!
//! `testfloat_gen` writes test cases with the results of a SoftFloat that is
//! built from the upstream sources with the `ARM-VFPv2` specialisation: the
//! operands, the result and the exception flags, in hexadecimal. This module
//! runs one such case through `helpers/fp.rs` and compares. It proves two
//! things: that the copy of SoftFloat that `build.rs` compiles under a symbol
//! prefix computes what the upstream build computes, and that the code
//! around it (the rounding mode from FPCR, the exception bits for FPSR, the
//! NaN rules, the conversions to an integer) gives the same.
//!
//! `just tcg-a64-testfloat` in `hv/` builds the generator and runs every
//! case. `tests/testfloat.rs` replays a committed sample without it.

use crate::helpers::fp::{
    self, Binary, DOUBLE, Format, HALF, SINGLE, Unary, cumulative, fpcr, fpsr, nzcv,
};
use crate::helpers::softfloat::Rounding;

/// One run of the generator: a function, a rounding mode, and whether a
/// rounding to an integer raises inexact (`-exact`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    pub function: String,
    pub mode: Rounding,
    pub exact: bool,
}

/// What a test case did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Same,
    /// The case is one where the A64 rule differs from SoftFloat's by
    /// design, and it is not compared.
    Skipped,
}

/// The name that `testfloat_gen` takes for a rounding mode, after `-r`.
pub fn mode_name(mode: Rounding) -> &'static str {
    match mode {
        Rounding::NearEven => "near_even",
        Rounding::MinMag => "minMag",
        Rounding::Min => "min",
        Rounding::Max => "max",
        Rounding::NearMaxMag => "near_maxMag",
    }
}

pub fn mode_of(name: &str) -> Option<Rounding> {
    [
        Rounding::NearEven,
        Rounding::MinMag,
        Rounding::Min,
        Rounding::Max,
        Rounding::NearMaxMag,
    ]
    .into_iter()
    .find(|mode| mode_name(*mode) == name)
}

/// FPCR with the rounding mode `mode`. `None`: FPCR has no such mode.
fn fpcr_of(mode: Rounding) -> Option<u32> {
    let bits = match mode {
        Rounding::NearEven => 0,
        Rounding::Max => 1,
        Rounding::Min => 2,
        Rounding::MinMag => 3,
        Rounding::NearMaxMag => return None,
    };
    Some(bits << fpcr::RMODE_SHIFT)
}

fn format_of(name: &str) -> Option<Format> {
    match name {
        "f16" => Some(HALF),
        "f32" => Some(SINGLE),
        "f64" => Some(DOUBLE),
        _ => None,
    }
}

/// The width and the signedness of an integer type of SoftFloat.
fn integer_of(name: &str) -> Option<(u32, bool)> {
    match name {
        "i32" => Some((32, false)),
        "ui32" => Some((32, true)),
        "i64" => Some((64, false)),
        "ui64" => Some((64, true)),
        _ => None,
    }
}

const MODES: [Rounding; 4] = [
    Rounding::NearEven,
    Rounding::MinMag,
    Rounding::Min,
    Rounding::Max,
];

/// Every run of the generator that the helpers are compared with.
pub fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    let mut add = |function: String, modes: &[Rounding], exact: bool| {
        for mode in modes {
            cases.push(Case {
                function: function.clone(),
                mode: *mode,
                exact,
            });
        }
    };
    let all = [
        Rounding::NearEven,
        Rounding::MinMag,
        Rounding::Min,
        Rounding::Max,
        Rounding::NearMaxMag,
    ];
    for width in [32, 64] {
        for name in ["add", "sub", "mul", "div", "sqrt", "mulAdd"] {
            add(format!("f{width}_{name}"), &MODES, true);
        }
        // `FRINTX` rounds in the mode of FPCR and is exact. The five forms
        // with a mode of their own are not.
        add(format!("f{width}_roundToInt"), &MODES, true);
        add(format!("f{width}_roundToInt"), &all, false);
        for name in ["eq", "le", "lt", "eq_signaling", "le_quiet", "lt_quiet"] {
            add(format!("f{width}_{name}"), &[Rounding::NearEven], true);
        }
        for integer in ["i32", "ui32", "i64", "ui64"] {
            add(format!("f{width}_to_{integer}"), &all, true);
            add(format!("{integer}_to_f{width}"), &MODES, true);
        }
    }
    for (from, to) in [(16, 32), (16, 64), (32, 16), (32, 64), (64, 16), (64, 32)] {
        add(format!("f{from}_to_f{to}"), &MODES, true);
    }
    cases
}

/// The arguments of `testfloat_gen` for `case`.
pub fn generator_arguments(case: &Case, level: u32) -> Vec<String> {
    vec![
        "-level".into(),
        level.to_string(),
        "-tininessbefore".into(),
        format!("-r{}", mode_name(case.mode)),
        if case.exact { "-exact" } else { "-notexact" }.into(),
        case.function.clone(),
    ]
}

/// What the helpers give for the operands of a case: the result, and the
/// bits for FPSR. `Ok(None)`: the case is not compared.
fn ours(case: &Case, operands: &[u64]) -> Result<Option<(u64, u8)>, String> {
    let unknown = || format!("the function {} is not known", case.function);
    let (head, tail) = case.function.split_once('_').ok_or_else(unknown)?;
    let operand = |index: usize| {
        operands
            .get(index)
            .copied()
            .ok_or_else(|| format!("{} needs more operands", case.function))
    };
    // An integer to a value.
    if let Some((bits, unsigned)) = integer_of(head) {
        let format = tail
            .strip_prefix("to_")
            .and_then(format_of)
            .ok_or_else(unknown)?;
        let fpcr = fpcr_of(case.mode).ok_or_else(unknown)?;
        return Ok(Some(fp::from_fixed(
            format,
            operand(0)?,
            bits,
            unsigned,
            0,
            fpcr,
        )));
    }
    let format = format_of(head).ok_or_else(unknown)?;
    if let Some(target) = tail.strip_prefix("to_") {
        // A value to an integer, in the mode of the case.
        if let Some((bits, unsigned)) = integer_of(target) {
            return Ok(Some(fp::to_fixed(
                format,
                operand(0)?,
                bits,
                unsigned,
                0,
                case.mode,
                0,
            )));
        }
        let to = format_of(target).ok_or_else(unknown)?;
        let fpcr = fpcr_of(case.mode).ok_or_else(unknown)?;
        return Ok(Some(fp::convert(format, to, operand(0)?, fpcr)));
    }
    let compare = |signalling: bool, accept: &[u8]| -> Result<Option<(u64, u8)>, String> {
        let (flags, raised) = fp::compare(format, operand(0)?, operand(1)?, signalling, 0);
        Ok(Some((u64::from(accept.contains(&flags)), raised)))
    };
    match tail {
        "eq" => return compare(false, &[nzcv::EQUAL]),
        "eq_signaling" => return compare(true, &[nzcv::EQUAL]),
        "lt" => return compare(true, &[nzcv::LESS]),
        "lt_quiet" => return compare(false, &[nzcv::LESS]),
        "le" => return compare(true, &[nzcv::LESS, nzcv::EQUAL]),
        "le_quiet" => return compare(false, &[nzcv::LESS, nzcv::EQUAL]),
        _ => {}
    }
    if tail == "roundToInt" {
        let (operation, fpcr) = if case.exact {
            (Unary::RoundExact, fpcr_of(case.mode).ok_or_else(unknown)?)
        } else {
            let operation = match case.mode {
                Rounding::NearEven => Unary::RoundNearEven,
                Rounding::MinMag => Unary::RoundToZero,
                Rounding::Min => Unary::RoundDown,
                Rounding::Max => Unary::RoundUp,
                Rounding::NearMaxMag => Unary::RoundTiesAway,
            };
            // The mode of FPCR is another one, and must not matter.
            (operation, 1 << fpcr::RMODE_SHIFT)
        };
        return Ok(Some(fp::unary(operation, format, operand(0)?, fpcr)));
    }
    let fpcr = fpcr_of(case.mode).ok_or_else(unknown)?;
    let binary = |operation: Binary| -> Result<Option<(u64, u8)>, String> {
        Ok(Some(fp::binary(
            operation,
            format,
            operand(0)?,
            operand(1)?,
            fpcr,
        )))
    };
    match tail {
        "add" => binary(Binary::Add),
        "sub" => binary(Binary::Sub),
        "mul" => binary(Binary::Mul),
        "div" => binary(Binary::Div),
        "sqrt" => Ok(Some(fp::unary(Unary::Sqrt, format, operand(0)?, fpcr))),
        "mulAdd" => {
            let (a, b, c) = (operand(0)?, operand(1)?, operand(2)?);
            // SoftFloat takes the NaN of a fused multiply-add from the
            // factors first. A64 takes it from the addend first. With a NaN
            // on both sides the two differ by design.
            if format.is_nan(c) && (format.is_nan(a) || format.is_nan(b)) {
                return Ok(None);
            }
            Ok(Some(fp::mul_add(format, c, a, b, false, false, fpcr)))
        }
        _ => Err(unknown()),
    }
}

/// Run one line of `testfloat_gen` for `case`: the operands, the expected
/// result and the expected flags, each in hexadecimal.
pub fn check(case: &Case, line: &str) -> Result<Verdict, String> {
    let numbers: Vec<u64> = line
        .split_whitespace()
        .map(|text| u64::from_str_radix(text, 16).map_err(|_| format!("`{text}` is not hex")))
        .collect::<Result<_, _>>()?;
    let [operands @ .., expected, flags] = numbers.as_slice() else {
        return Err(format!("`{line}` is not a test case"));
    };
    let Some((value, raised)) = ours(case, operands)? else {
        return Ok(Verdict::Skipped);
    };
    let expected_bits = cumulative(*flags as u8);
    // No test case is run with flush-to-zero, so IDC is never expected.
    debug_assert_eq!(raised & fpsr::IDC, 0);
    if value != *expected || raised != expected_bits {
        return Err(format!(
            "{} -r{}{}: {line}: ours {value:x} with the FPSR bits {raised:#x}, expected the FPSR bits {expected_bits:#x}",
            case.function,
            mode_name(case.mode),
            if case.exact { "" } else { " -notexact" },
        ));
    }
    Ok(Verdict::Same)
}

/// A case and a line of it, as one line of the committed sample.
pub fn sample_line(case: &Case, line: &str) -> String {
    format!(
        "{} {} {} {}",
        case.function,
        mode_name(case.mode),
        if case.exact { "exact" } else { "notexact" },
        line.trim()
    )
}

/// The case and the test line of one line of the committed sample.
pub fn parse_sample(line: &str) -> Result<(Case, &str), String> {
    let mut parts = line.splitn(4, ' ');
    let mut next = || parts.next().ok_or_else(|| format!("`{line}` is too short"));
    let function = next()?.to_string();
    let mode = next()?;
    let mode = mode_of(mode).ok_or_else(|| format!("the mode `{mode}` is not known"))?;
    let exact = match next()? {
        "exact" => true,
        "notexact" => false,
        other => return Err(format!("`{other}` is not exact or notexact")),
    };
    Ok((
        Case {
            function,
            mode,
            exact,
        },
        next()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(function: &str, mode: Rounding, exact: bool) -> Case {
        Case {
            function: function.into(),
            mode,
            exact,
        }
    }

    #[test]
    fn a_line_of_the_generator_is_checked_against_the_helpers() {
        // 1 + 2 = 3, and 1 / 3 in two modes: inexact is the flag 01.
        let add = case("f32_add", Rounding::NearEven, true);
        assert_eq!(check(&add, "3F800000 40000000 40400000 00"), Ok(Verdict::Same));
        assert!(check(&add, "3F800000 40000000 40400001 00").is_err());
        assert!(check(&add, "3F800000 40000000 40400000 01").is_err());
        assert!(check(&add, "3F800000").is_err());
        assert!(check(&add, "3F800000 zz 0 0").is_err());
        let div = |mode| case("f64_div", mode, true);
        let third = "3FF0000000000000 4008000000000000";
        assert_eq!(
            check(&div(Rounding::Min), &format!("{third} 3FD5555555555555 01")),
            Ok(Verdict::Same)
        );
        assert_eq!(
            check(&div(Rounding::Max), &format!("{third} 3FD5555555555556 01")),
            Ok(Verdict::Same)
        );
        // A conversion in the mode of the case, a compare, and a fused
        // multiply-add whose operands are the factors and then the addend.
        assert_eq!(
            check(&case("f32_to_i32", Rounding::NearMaxMag, true), "40200000 00000003 01"),
            Ok(Verdict::Same)
        );
        assert_eq!(
            check(&case("f32_lt", Rounding::NearEven, true), "3F800000 7FC00000 0 10"),
            Ok(Verdict::Same)
        );
        assert_eq!(
            check(&case("f32_lt_quiet", Rounding::NearEven, true), "3F800000 7FC00000 0 00"),
            Ok(Verdict::Same)
        );
        assert_eq!(
            check(
                &case("f32_mulAdd", Rounding::NearEven, true),
                "40000000 40400000 3F800000 40E00000 00"
            ),
            Ok(Verdict::Same)
        );
        assert_eq!(
            check(
                &case("f32_mulAdd", Rounding::NearEven, true),
                "7FC00001 40400000 7FC00002 7FC00001 00"
            ),
            Ok(Verdict::Skipped)
        );
        assert_eq!(
            check(&case("ui64_to_f32", Rounding::MinMag, true), "FFFFFFFFFFFFFFFF 5F7FFFFF 01"),
            Ok(Verdict::Same)
        );
        assert_eq!(
            check(&case("f64_roundToInt", Rounding::NearMaxMag, false), "4004000000000000 4008000000000000 00"),
            Ok(Verdict::Same)
        );
        assert_eq!(
            check(&case("f64_to_f16", Rounding::NearEven, true), "3FF0000000000000 3C00 00"),
            Ok(Verdict::Same)
        );
        assert!(check(&case("f80_add", Rounding::NearEven, true), "0 0 0 0").is_err());
        assert!(check(&case("f32_add", Rounding::NearMaxMag, true), "0 0 0 0").is_err());
    }

    #[test]
    fn a_sample_line_goes_to_text_and_back() {
        let case = case("f64_roundToInt", Rounding::NearMaxMag, false);
        let line = sample_line(&case, "4004000000000000 4008000000000000 00\n");
        assert_eq!(line, "f64_roundToInt near_maxMag notexact 4004000000000000 4008000000000000 00");
        assert_eq!(
            parse_sample(&line),
            Ok((case.clone(), "4004000000000000 4008000000000000 00"))
        );
        assert!(parse_sample("f32_add near_even").is_err());
        assert!(parse_sample("f32_add odd exact 0 0 0 0").is_err());
        assert_eq!(
            generator_arguments(&case, 1),
            ["-level", "1", "-tininessbefore", "-rnear_maxMag", "-notexact", "f64_roundToInt"]
        );
    }

    #[test]
    fn the_cases_cover_every_helper_and_no_case_twice() {
        let cases = cases();
        let mut seen = std::collections::BTreeSet::new();
        for case in &cases {
            assert!(
                seen.insert((case.function.clone(), mode_name(case.mode), case.exact)),
                "{case:?}"
            );
        }
        // Per width: 6 arithmetic functions in 4 modes, roundToInt in 4 and
        // in 5, 6 compares, 4 integers out in 5 modes and in in 4.
        assert_eq!(cases.len(), 2 * (6 * 4 + 4 + 5 + 6 + 4 * 5 + 4 * 4) + 6 * 4);
        for name in ["f32_mulAdd", "f64_to_ui64", "i32_to_f32", "f16_to_f64", "f64_lt_quiet"] {
            assert!(cases.iter().any(|case| case.function == name), "{name}");
        }
    }
}
