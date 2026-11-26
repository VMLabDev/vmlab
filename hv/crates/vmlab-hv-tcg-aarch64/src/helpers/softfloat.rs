//! Berkeley SoftFloat 3e with the `ARM-VFPv2` specialisation (DESIGN.md R4,
//! source S5): the copy that `build.rs` compiles under the symbol prefix
//! `a64sf_`, beside the `RISCV` copy of `vmlab-hv-tcg`.
//!
//! The specialisation decides three things: tininess is detected before
//! rounding, a generated NaN is the positive quiet NaN with a zero payload,
//! and a conversion to an integer that is invalid gives the nearest end of
//! the range, or zero for a NaN. NaN operands do not reach SoftFloat from
//! `fp.rs`, which propagates them by the A64 rules first.
//!
//! Sources read: S5, SoftFloat's own headers and documentation.

// The operations of the wrapper by name, as `u32` constants, and `NAMES`.
include!(concat!(env!("OUT_DIR"), "/a64sf_ops.rs"));

unsafe extern "C" {
    fn a64sf_call(operation: u32, mode: u32, a: u64, b: u64, c: u64, flags: *mut u8) -> u64;
}

/// A rounding mode, with the number that SoftFloat gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// To the nearest value, a tie to the even one.
    NearEven = 0,
    /// Toward zero.
    MinMag = 1,
    /// Toward minus infinity.
    Min = 2,
    /// Toward plus infinity.
    Max = 3,
    /// To the nearest value, a tie away from zero.
    NearMaxMag = 4,
}

/// SoftFloat's exception flags.
pub mod flag {
    pub const INEXACT: u8 = 1;
    pub const UNDERFLOW: u8 = 2;
    pub const OVERFLOW: u8 = 4;
    pub const INFINITE: u8 = 8;
    pub const INVALID: u8 = 16;
}

/// Run one operation in the rounding mode `mode`. Gives the result and the
/// exception flags that this call raised.
pub fn call(operation: u32, mode: Rounding, a: u64, b: u64, c: u64) -> (u64, u8) {
    let mut flags = 0u8;
    // SAFETY: `flags` points to a writable byte, and the wrapper checks the
    // index itself. The rounding mode and the exception flags of SoftFloat
    // are thread-local (THREAD_LOCAL in build.rs) and the wrapper sets both
    // on entry, so a call on another thread neither sees nor disturbs this one.
    let value = unsafe { a64sf_call(operation, mode as u32, a, b, c, &mut flags) };
    assert_ne!(flags, 255, "SoftFloat operation {operation} does not exist");
    (value, flags)
}

/// The operation with the name of its SoftFloat function, as `f32_add`.
pub fn operation(name: &str) -> Option<u32> {
    NAMES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, index)| *index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_copy_is_the_arm_specialisation_and_not_the_riscv_one() {
        // A signalling NaN operand: the Arm rule keeps its payload and sets
        // the quiet bit. The RISCV copy would give the canonical NaN.
        let (value, flags) = call(F32_ADD, Rounding::NearEven, 0x7f80_1234, 0x3f80_0000, 0);
        assert_eq!(value, 0x7fc0_1234);
        assert_eq!(flags, flag::INVALID);
        // Tininess before rounding: this product rounds up to the smallest
        // normal number, and is still an underflow. After rounding it is not.
        // 0x00ffffff * 0.5 = 0x007fffff.8, which rounds to 0x00800000.
        let (value, flags) = call(F32_MUL, Rounding::NearEven, 0x00ff_ffff, 0x3f00_0000, 0);
        assert_eq!(value, 0x0080_0000);
        assert_eq!(flags, flag::UNDERFLOW | flag::INEXACT);
    }

    #[test]
    fn operations_take_their_rounding_mode_from_the_call() {
        // 1 / 3 in each mode.
        let one = 0x3ff0_0000_0000_0000u64;
        let three = 0x4008_0000_0000_0000u64;
        let down = 0x3fd5_5555_5555_5555u64;
        for (mode, expected) in [
            (Rounding::NearEven, down),
            (Rounding::MinMag, down),
            (Rounding::Min, down),
            (Rounding::Max, down + 1),
        ] {
            assert_eq!(call(F64_DIV, mode, one, three, 0), (expected, flag::INEXACT));
        }
        // 2.5 to an integer: a tie.
        let two_and_a_half = 0x4020_0000u64;
        assert_eq!(
            call(F32_TO_I32, Rounding::NearEven, two_and_a_half, 0, 0),
            (2, flag::INEXACT)
        );
        assert_eq!(
            call(F32_TO_I32, Rounding::NearMaxMag, two_and_a_half, 0, 0),
            (3, flag::INEXACT)
        );
        // A negative result of 32 bits is not sign-extended into the 64.
        assert_eq!(
            call(F32_TO_I32, Rounding::MinMag, 0xc020_0000, 0, 0),
            (0xffff_fffe, flag::INEXACT)
        );
        assert_eq!(operation("f64_div"), Some(F64_DIV));
        assert_eq!(operation("f64_nothing"), None);
        // A division by zero is the flag that SoftFloat calls infinite.
        assert_eq!(
            call(F64_DIV, Rounding::NearEven, one, 0, 0),
            (0x7ff0_0000_0000_0000, flag::INFINITE)
        );
    }
}
