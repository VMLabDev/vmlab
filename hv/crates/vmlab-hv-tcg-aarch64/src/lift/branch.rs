//! Branches: the six format classes `BranchImm` (`B`), `CallImm` (`BL`),
//! `BranchCond` (`Bcc`), `BaseCmpBranch` (`CBZ`, `CBNZ`), `BaseTestBranch`
//! (`TBZ`, `TBNZ`) and `BranchReg` (`BR`, `BLR`, `RET`), 14 records.
//!
//! Sources read (DESIGN.md "Sources"): S1, the LLVM AArch64 tables for the
//! records and their fields. S6, the lockstep against QEMU, for the
//! behaviour. No text of S2 to S4 was opened for this file. Not read: QEMU's
//! source, Arm's ASL or pseudocode (X1, X2).
//!
//! A branch ends its block. A conditional branch is one `Select` between the
//! target and the next instruction, so a block has one exit.

use super::{Builder, Lifted};
use crate::fields::{Condition, sign_extend};
use crate::generated::decode::{
    BaseCmpBranch, BaseTestBranch, BranchCond, BranchImm, BranchReg, CallImm, Record,
};
use crate::state::slot;

/// The address that a PC-relative field of `bits` bits names: the field
/// counts instructions from the branch.
fn target(b: &Builder, field: u32, bits: u32) -> u64 {
    b.pc()
        .wrapping_add((sign_extend(field.into(), bits) << 2) as u64)
}

/// The next PC of a conditional branch: `target` where `taken` holds.
fn conditional_branch(b: &mut Builder, taken: vmlab_hv_tcg_ir::native::ScalarId, target: u64) {
    let target = b.imm(64, target);
    let next = b.imm(64, b.pc().wrapping_add(4));
    let pc = b.select(64, taken, target, next);
    b.branch(pc);
}

fn link(b: &mut Builder) {
    let next = b.imm(64, b.pc().wrapping_add(4));
    b.write(slot::X0 + 30, next);
}

/// `B`.
pub fn branch(b: &mut Builder, record: Record, f: &BranchImm) -> Lifted {
    if record != Record::B {
        return Lifted::Unsupported;
    }
    let target = b.imm(64, target(b, f.addr, 26));
    b.branch(target);
    Lifted::Branch
}

/// `BL`: X30 takes the address of the next instruction.
pub fn call(b: &mut Builder, record: Record, f: &CallImm) -> Lifted {
    if record != Record::BL {
        return Lifted::Unsupported;
    }
    let target = b.imm(64, target(b, f.addr, 26));
    link(b);
    b.branch(target);
    Lifted::Branch
}

/// `B.cond`.
pub fn conditional(b: &mut Builder, record: Record, f: &BranchCond) -> Lifted {
    if record != Record::Bcc {
        return Lifted::Unsupported;
    }
    let taken = b.condition(Condition::from_bits(f.cond));
    let target = target(b, f.target, 19);
    conditional_branch(b, taken, target);
    Lifted::Branch
}

/// `CBZ` and `CBNZ`: branch on a register that is zero, or is not.
pub fn compare(b: &mut Builder, record: Record, f: &BaseCmpBranch) -> Lifted {
    let (width, on_zero) = match record {
        Record::CBZW => (32, true),
        Record::CBZX => (64, true),
        Record::CBNZW => (32, false),
        Record::CBNZX => (64, false),
        _ => return Lifted::Unsupported,
    };
    let value = b.x(f.rt, width);
    let zero = b.is_zero(width, value);
    let taken = if on_zero { zero } else { b.bool_not(zero) };
    let target = target(b, f.target, 19);
    conditional_branch(b, taken, target);
    Lifted::Branch
}

/// `TBZ` and `TBNZ`: branch on one bit of a register. The W records test the
/// bits 0 to 31 and the X records the bits 32 to 63.
pub fn test(b: &mut Builder, record: Record, f: &BaseTestBranch) -> Lifted {
    let (high, on_zero) = match record {
        Record::TBZW => (false, true),
        Record::TBZX => (true, true),
        Record::TBNZW => (false, false),
        Record::TBNZX => (true, false),
        _ => return Lifted::Unsupported,
    };
    let index = (f.bit_off & 31) as u16 + if high { 32 } else { 0 };
    let value = b.x(f.rt, 64);
    let bit = b.bit(value, index);
    let set = b.to_bool(bit);
    let taken = if on_zero { b.bool_not(set) } else { set };
    let target = target(b, f.target, 14);
    conditional_branch(b, taken, target);
    Lifted::Branch
}

/// `BR`, `BLR` and `RET`: branch to a register. Register 31 is the zero
/// register here. `BLR` reads its target before it writes X30.
pub fn register(b: &mut Builder, record: Record, f: &BranchReg) -> Lifted {
    let link_register = match record {
        Record::BR | Record::RET => false,
        Record::BLR => true,
        _ => return Lifted::Unsupported,
    };
    let target = b.x(f.rn, 64);
    if link_register {
        link(b);
    }
    b.branch(target);
    Lifted::Branch
}
