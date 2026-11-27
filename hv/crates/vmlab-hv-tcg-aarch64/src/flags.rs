//! Lazy condition flags (card #1142). A flag-setting instruction does not
//! compute N, Z, C and V: it writes what they follow from into three slots,
//! a kind and two operands, and whoever reads the flags computes them. Inside
//! a block the builder knows the kind and lifts each condition straight from
//! the operands (`lift::builder`); a block that reads flags it did not set
//! calls the helpers of this file, and Rust reads them through
//! `State::nzcv`.
//!
//! The kind has four bits: the operation in the bits 1 to 0, the width in
//! bit 2 (set: 64 bits, clear: 32) and the carry in of a sum in bit 3.
//!
//! - `EXPLICIT`: the flags themselves, NZCV in the bits 3 to 0 of `a`.
//! - `ADD`: the flags of `a + b + carry`.
//! - `SUB`: the flags of `a + NOT(b) + carry`, a subtraction when the carry
//!   is set. `b` is the subtrahend as the instruction names it.
//! - `LOGICAL`: N and Z of the result `a`, C and V clear.
//!
//! Operands are kept zero-extended to 64 bits. The bits above the width are
//! ignored.
//!
//! Sources read: none. The carry and overflow rules are the two's complement
//! identities of `Builder::add_with_carry`.

use crate::fields::Condition;
use vmlab_hv_tcg_ir::native::{Effects, Helper, HelperFn};

pub const EXPLICIT: u8 = 0;
pub const ADD: u8 = 1;
pub const SUB: u8 = 2;
pub const LOGICAL: u8 = 3;
/// The width bit of a kind: 64 bits.
pub const WIDE: u8 = 4;
/// The carry-in bit of a kind.
pub const CARRY: u8 = 8;

/// The kind of an operation of `width` bits (32 or 64).
pub const fn kind(operation: u8, width: u16) -> u8 {
    operation | if width == 64 { WIDE } else { 0 }
}

/// N, Z, C and V as the bits 3 to 0, from the three slots.
pub fn nzcv(kind: u8, a: u64, b: u64) -> u8 {
    let (width, mask) = if kind & WIDE != 0 {
        (64, u64::MAX)
    } else {
        (32, u64::from(u32::MAX))
    };
    let negative_zero =
        |result: u64| ((result >> (width - 1) & 1) as u8) << 3 | u8::from(result == 0) << 2;
    match kind & 3 {
        EXPLICIT => a as u8 & 15,
        LOGICAL => negative_zero(a & mask),
        operation => {
            let a = a & mask;
            let b = if operation == SUB {
                !b & mask
            } else {
                b & mask
            };
            let carry_in = u128::from(kind >> 3 & 1);
            let sum = u128::from(a) + u128::from(b) + carry_in;
            let result = sum as u64 & mask;
            let carry = (sum >> width) as u8 & 1;
            let overflow = ((a ^ result) & (b ^ result)) >> (width - 1) & 1;
            negative_zero(result) | carry << 1 | overflow as u8
        }
    }
}

/// Whether `condition` (its four bits) holds for the flags NZCV.
pub fn holds(condition: u8, nzcv: u8) -> bool {
    Condition::from_bits(u32::from(condition)).holds(
        nzcv & 8 != 0,
        nzcv & 4 != 0,
        nzcv & 2 != 0,
        nzcv & 1 != 0,
    )
}

fn helper_nzcv(args: &[u128]) -> u128 {
    nzcv(args[0] as u8, args[1] as u64, args[2] as u64).into()
}

fn helper_condition(args: &[u128]) -> u128 {
    let flags = nzcv(args[0] as u8, args[1] as u64, args[2] as u64);
    holds(args[3] as u8, flags).into()
}

/// The flags of (kind, a, b) as four bits.
pub static NZCV: Helper = Helper {
    name: "a64_nzcv",
    args: 3,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_nzcv),
};

/// Whether the condition of the fourth argument holds for the flags of
/// (kind, a, b). A Bool.
pub static CONDITION: Helper = Helper {
    name: "a64_condition",
    args: 4,
    results: 1,
    effects: Effects::NONE,
    function: HelperFn::Pure(helper_condition),
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The flags of `a + b + carry` in `width` bits, computed bit by bit.
    fn reference(width: u32, a: u64, b: u64, carry: bool) -> u8 {
        let mask = if width == 64 {
            u64::MAX
        } else {
            u64::from(u32::MAX)
        };
        let (a, b) = (a & mask, b & mask);
        let result = a.wrapping_add(b).wrapping_add(u64::from(carry)) & mask;
        let unsigned = u128::from(a) + u128::from(b) + u128::from(carry);
        let sign = |value: u64| -> i128 {
            if width == 64 {
                i128::from(value as i64)
            } else {
                i128::from(value as u32 as i32)
            }
        };
        let signed = sign(a) + sign(b) + i128::from(carry);
        let n = result >> (width - 1) & 1 == 1;
        let z = result == 0;
        let c = unsigned != u128::from(result);
        let v = signed != sign(result);
        u8::from(n) << 3 | u8::from(z) << 2 | u8::from(c) << 1 | u8::from(v)
    }

    const VALUES: [u64; 12] = [
        0,
        1,
        2,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_ffff,
        0x1_0000_0000,
        0x7fff_ffff_ffff_ffff,
        0x8000_0000_0000_0000,
        0xffff_ffff_ffff_fffe,
        u64::MAX,
        0x1234_5678_9abc_def0,
    ];

    #[test]
    fn sums_give_the_flags_of_the_reference() {
        for width in [32u16, 64] {
            for &a in &VALUES {
                for &b in &VALUES {
                    for carry in [false, true] {
                        let carry_bit = if carry { CARRY } else { 0 };
                        assert_eq!(
                            nzcv(kind(ADD, width) | carry_bit, a, b),
                            reference(width.into(), a, b, carry),
                            "add {width} {a:#x} {b:#x} {carry}"
                        );
                        assert_eq!(
                            nzcv(kind(SUB, width) | carry_bit, a, b),
                            reference(width.into(), a, !b, carry),
                            "sub {width} {a:#x} {b:#x} {carry}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn logical_and_explicit_kinds() {
        assert_eq!(nzcv(kind(LOGICAL, 64), 0, 0), 0b0100);
        assert_eq!(nzcv(kind(LOGICAL, 64), 1 << 63, 0), 0b1000);
        assert_eq!(nzcv(kind(LOGICAL, 32), 1 << 32, 0), 0b0100);
        assert_eq!(nzcv(kind(LOGICAL, 32), 1 << 31, 0), 0b1000);
        for value in 0..16 {
            assert_eq!(nzcv(EXPLICIT, value, 0), value as u8);
        }
    }

    #[test]
    fn the_condition_helper_is_the_condition_of_the_flags() {
        let HelperFn::Pure(function) = CONDITION.function else {
            panic!("a pure helper");
        };
        for &a in &VALUES {
            for &b in &VALUES {
                let flags = nzcv(kind(SUB, 64) | CARRY, a, b);
                for condition in 0..16u8 {
                    let args = [
                        u128::from(kind(SUB, 64) | CARRY),
                        a.into(),
                        b.into(),
                        condition.into(),
                    ];
                    assert_eq!(function(&args) == 1, holds(condition, flags));
                }
            }
        }
    }
}
