//! What a raw field means (DESIGN.md G1a): immediates and condition codes,
//! written by hand once per operand class.
//!
//! Sources read (DESIGN.md "Sources"): S1, the LLVM AArch64 tables for the
//! field layouts, and `MCTargetDesc/AArch64AddressingModes.h` for the logical
//! immediate. The two logical immediate functions below are ported by hand
//! from that file and carry its notice:
//!
//! Part of the LLVM Project, under the Apache License v2.0 with LLVM
//! Exceptions. See https://llvm.org/LICENSE.txt for license information.
//! SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

/// Sign-extend the low `bits` bits of `value`.
pub fn sign_extend(value: u64, bits: u32) -> i64 {
    debug_assert!((1..=64).contains(&bits));
    ((value << (64 - bits)) as i64) >> (64 - bits)
}

/// A mask of the low `bits` bits, for 0 to 64 bits.
pub fn ones(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

fn logical_immediate_length(value: u32) -> Option<u32> {
    let n = (value >> 12) & 1;
    let imms = value & 0x3f;
    // The index of the highest set bit of N:NOT(imms).
    let pattern = (n << 6) | (!imms & 0x3f);
    (pattern != 0).then(|| 31 - pattern.leading_zeros())
}

/// Whether `N:immr:imms` names a logical immediate. A 32-bit record has a
/// 12-bit field, so its N is 0 here.
///
/// Ported from `AArch64_AM::isValidDecodeLogicalImmediate`.
pub fn logical_immediate_is_valid(value: u32) -> bool {
    let Some(len) = logical_immediate_length(value) else {
        return false;
    };
    let size = 1u32 << len;
    let s = value & 0x3f & (size - 1);
    s != size - 1
}

/// The value of the logical immediate `N:immr:imms` for a register of
/// `width` bits (32 or 64), or `None` for an encoding that names none.
///
/// Ported from `AArch64_AM::decodeLogicalImmediate`.
pub fn logical_immediate(value: u32, width: u32) -> Option<u64> {
    let n = (value >> 12) & 1;
    if !logical_immediate_is_valid(value) || (width == 32 && n != 0) {
        return None;
    }
    let immr = (value >> 6) & 0x3f;
    let imms = value & 0x3f;
    let len = logical_immediate_length(value)?;
    let mut size = 1u32 << len;
    let r = immr & (size - 1);
    let s = imms & (size - 1);
    let element = ones(size);
    let mut pattern = ones(s + 1);
    // Rotate right by R inside the element.
    if r != 0 {
        pattern = ((pattern >> r) | (pattern << (size - r))) & element;
    }
    // Replicate the element to fill the register.
    while size < width {
        pattern |= pattern << size;
        size *= 2;
    }
    Some(pattern & ones(width))
}

/// Whether `table` has an entry with this encoding whose features are all
/// in the selected sets. LLVM's decoder asks the same of its subtarget.
fn is_known(table: &[crate::generated::sysreg::SysOp], encoding: u32) -> bool {
    table.iter().any(|operation| {
        u32::from(operation.encoding) == encoding
            && operation.requires.iter().all(|feature| {
                feature
                    .strip_prefix("Feature")
                    .is_some_and(|name| crate::generated::decode::FEATURE_NAMES.contains(&name))
            })
    })
}

/// Whether the `pstatefield` of `MSRpstateImm4` names a PSTATE field that
/// takes a 4-bit immediate (`SPSel`, `DAIFSet`, `DAIFClr` in the 3d set).
pub fn pstate_imm0_15_is_known(pstatefield: u32) -> bool {
    is_known(&crate::generated::sysreg::PSTATE_IMM0_15_OPS, pstatefield)
}

/// Whether the `pstatefield` of `MSRpstateImm1` names a PSTATE field that
/// takes a 1-bit immediate.
pub fn pstate_imm0_1_is_known(pstatefield: u32) -> bool {
    is_known(&crate::generated::sysreg::PSTATE_IMM0_1_OPS, pstatefield)
}

/// Whether the operand of `MSRpstatesvcrImm1` names an `SVCR` field.
pub fn svcr_is_known(pstatefield: u32) -> bool {
    is_known(&crate::generated::sysreg::SVCR_OPS, pstatefield)
}

/// The sixteen condition codes of `Bcc`, `CSEL` and the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    Eq,
    Ne,
    Cs,
    Cc,
    Mi,
    Pl,
    Vs,
    Vc,
    Hi,
    Ls,
    Ge,
    Lt,
    Gt,
    Le,
    Al,
    Nv,
}

impl Condition {
    pub fn from_bits(bits: u32) -> Self {
        match bits & 15 {
            0 => Self::Eq,
            1 => Self::Ne,
            2 => Self::Cs,
            3 => Self::Cc,
            4 => Self::Mi,
            5 => Self::Pl,
            6 => Self::Vs,
            7 => Self::Vc,
            8 => Self::Hi,
            9 => Self::Ls,
            10 => Self::Ge,
            11 => Self::Lt,
            12 => Self::Gt,
            13 => Self::Le,
            14 => Self::Al,
            _ => Self::Nv,
        }
    }

    /// Whether the condition holds for the flags. `Nv` holds always, as `Al`.
    pub fn holds(self, n: bool, z: bool, c: bool, v: bool) -> bool {
        match self {
            Self::Eq => z,
            Self::Ne => !z,
            Self::Cs => c,
            Self::Cc => !c,
            Self::Mi => n,
            Self::Pl => !n,
            Self::Vs => v,
            Self::Vc => !v,
            Self::Hi => c && !z,
            Self::Ls => !c || z,
            Self::Ge => n == v,
            Self::Lt => n != v,
            Self::Gt => !z && n == v,
            Self::Le => z || n != v,
            Self::Al | Self::Nv => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition of a logical immediate, written without the encoding:
    /// an element of 2, 4, 8, 16, 32 or 64 bits that holds a rotated run of
    /// ones, neither empty nor full, repeated over the register.
    fn by_definition(width: u32) -> std::collections::BTreeSet<u64> {
        let mut values = std::collections::BTreeSet::new();
        let mut size = 2;
        while size <= width {
            for run in 1..size {
                for rotation in 0..size {
                    let element = ones(run);
                    let rotated = if rotation == 0 {
                        element
                    } else {
                        ((element >> rotation) | (element << (size - rotation))) & ones(size)
                    };
                    let mut value = 0u64;
                    let mut at = 0;
                    while at < width {
                        value |= rotated << at;
                        at += size;
                    }
                    values.insert(value & ones(width));
                }
            }
            size *= 2;
        }
        values
    }

    #[test]
    fn every_encoding_decodes_to_a_value_of_the_definition_and_covers_it() {
        for width in [32, 64] {
            let expected = by_definition(width);
            let mut found = std::collections::BTreeSet::new();
            let limit = if width == 64 { 1 << 13 } else { 1 << 12 };
            for encoding in 0..limit {
                match logical_immediate(encoding, width) {
                    Some(value) => {
                        assert!(logical_immediate_is_valid(encoding));
                        assert!(expected.contains(&value), "{encoding:#x} {value:#x}");
                        found.insert(value);
                    }
                    None => assert!(
                        !logical_immediate_is_valid(encoding) || width == 32,
                        "{encoding:#x}"
                    ),
                }
            }
            assert_eq!(found, expected, "width {width}");
        }
        assert_eq!(by_definition(64).len(), 5334);
        assert_eq!(by_definition(32).len(), 1302);
    }

    #[test]
    fn known_logical_immediates() {
        // N=1 immr=0 imms=0: one bit.
        assert_eq!(logical_immediate(0x1000, 64), Some(1));
        // N=0 immr=0 imms=0b111100: element of 2 bits, one bit set.
        assert_eq!(logical_immediate(0x03c, 64), Some(0x5555_5555_5555_5555));
        assert_eq!(logical_immediate(0x03c, 32), Some(0x5555_5555));
        // N=1 imms=62: 63 ones. imms=63 is not an immediate.
        assert_eq!(logical_immediate(0x103e, 64), Some(u64::MAX >> 1));
        assert_eq!(logical_immediate(0x103f, 64), None);
        // N=0 imms=0b111111 names no element size.
        assert_eq!(logical_immediate(0x003f, 64), None);
        // N=1 on a 32-bit register.
        assert_eq!(logical_immediate(0x1000, 32), None);
        // 0xff00 in each 16 bits: element 16, run 8, rotation 8.
        assert_eq!(
            logical_immediate((8 << 6) | 0b100111, 64),
            Some(0xff00_ff00_ff00_ff00)
        );
    }

    #[test]
    fn pstate_fields_of_the_selected_sets() {
        // op1:op2 of SPSel, DAIFSet and DAIFClr.
        let known: Vec<u32> = (0..64)
            .filter(|field| pstate_imm0_15_is_known(*field))
            .collect();
        assert_eq!(known, vec![0b000_101, 0b011_110, 0b011_111]);
        // PAN, UAO, DIT, SSBS and TCO need features outside the 3d set.
        assert!(!pstate_imm0_15_is_known(0b000_100));
        // Every SVCR field needs SME.
        assert!((0..8).all(|field| !svcr_is_known(field)));
    }

    #[test]
    fn sign_extension_and_masks() {
        assert_eq!(sign_extend(0x1f_ffff, 21), -1);
        assert_eq!(sign_extend(0x10_0000, 21), -(1 << 20));
        assert_eq!(sign_extend(0x0f_ffff, 21), 0xf_ffff);
        assert_eq!(sign_extend(u64::MAX, 64), -1);
        assert_eq!(ones(0), 0);
        assert_eq!(ones(64), u64::MAX);
    }

    #[test]
    fn conditions_come_in_inverse_pairs() {
        for bits in (0..14).step_by(2) {
            for flags in 0..16 {
                let (n, z, c, v) = (
                    flags & 8 != 0,
                    flags & 4 != 0,
                    flags & 2 != 0,
                    flags & 1 != 0,
                );
                assert_ne!(
                    Condition::from_bits(bits).holds(n, z, c, v),
                    Condition::from_bits(bits + 1).holds(n, z, c, v)
                );
            }
        }
        assert!(Condition::Al.holds(false, false, false, false));
        assert!(Condition::Nv.holds(false, false, false, false));
        assert!(Condition::Hi.holds(false, false, true, false));
        assert!(!Condition::Hi.holds(false, true, true, false));
        assert!(Condition::Gt.holds(true, false, false, true));
    }
}
