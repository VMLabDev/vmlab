//! The lifters: one function per format class of the LLVM tables
//! (DESIGN.md G3a). A lifter takes the raw fields of a decoded instruction
//! and appends its semantics to the block of a `Builder`.

pub mod branch;
pub mod builder;
pub mod fp;
pub mod integer;

pub use builder::{A64Exit, A64Next, Builder, ExceptionClass, Flags};

use crate::generated::decode::{Decoded, Fields};

/// What a lifter did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifted {
    /// The instruction is in the block, and the next one follows it.
    Next,
    /// The instruction is in the block and set the next PC: the block ends.
    Branch,
    /// No lifter for this record yet. Nothing was added to the block.
    Unsupported,
}

/// Lift one decoded instruction at the PC that `builder.begin` set.
pub fn lift(builder: &mut Builder, decoded: &Decoded) -> Lifted {
    let record = decoded.record;
    match &decoded.fields {
        Fields::ADRI(fields) => integer::adr(builder, record, fields),
        Fields::AddSubImmShift(fields) => integer::add_sub_immediate(builder, record, fields),
        Fields::BaseLogicalImm(fields) => integer::logical_immediate(builder, record, fields),
        Fields::BaseMoveImmediate(fields) => integer::move_wide(builder, record, fields),
        Fields::BaseInsertImmediate(fields) => integer::move_keep(builder, record, fields),
        Fields::BaseBitfieldImm(fields) => integer::bitfield(builder, record, fields),
        Fields::BaseBitfieldImmWith2RegArgs(fields) => {
            integer::bitfield_insert(builder, record, fields)
        }
        Fields::BaseExtractImm(fields) => integer::extract(builder, record, fields),
        Fields::BranchImm(fields) => branch::branch(builder, record, fields),
        Fields::CallImm(fields) => branch::call(builder, record, fields),
        Fields::BranchCond(fields) => branch::conditional(builder, record, fields),
        Fields::BaseCmpBranch(fields) => branch::compare(builder, record, fields),
        Fields::BaseTestBranch(fields) => branch::test(builder, record, fields),
        Fields::BranchReg(fields) => branch::register(builder, record, fields),
        Fields::BaseSingleOperandFPData(fields) => fp::single_operand(builder, record, fields),
        Fields::BaseTwoOperandFPData(fields) => fp::two_operand(builder, record, fields),
        Fields::BaseThreeOperandFPData(fields) => fp::three_operand(builder, record, fields),
        Fields::BaseTwoOperandFPComparison(fields) => {
            fp::compare_registers(builder, record, fields)
        }
        Fields::BaseOneOperandFPComparison(fields) => fp::compare_zero(builder, record, fields),
        Fields::BaseFPCondComparison(fields) => fp::conditional_compare(builder, record, fields),
        Fields::BaseFPCondSelect(fields) => fp::conditional_select(builder, record, fields),
        Fields::BaseFPConversion(fields) => fp::convert(builder, record, fields),
        Fields::BaseFPMoveImmediate(fields) => fp::move_immediate(builder, record, fields),
        Fields::BaseFPToIntegerUnscaled(fields) => fp::to_integer_unscaled(builder, record, fields),
        Fields::BaseFPToInteger(fields) => fp::to_integer_scaled(builder, record, fields),
        Fields::BaseIntegerToFPUnscaled(fields) => {
            fp::from_integer_unscaled(builder, record, fields)
        }
        Fields::BaseIntegerToFP(fields) => fp::from_integer_scaled(builder, record, fields),
        Fields::BaseUnscaledConversion(fields) => fp::move_general(builder, record, fields),
        Fields::BaseUnscaledConversionFromHigh(fields) => {
            fp::move_from_high(builder, record, fields)
        }
        Fields::BaseUnscaledConversionToHigh(fields) => fp::move_to_high(builder, record, fields),
        _ => Lifted::Unsupported,
    }
}

/// The format classes that have a lifter, for the tests and the lockstep.
pub const LIFTED_FORMATS: [crate::FormatClass; 14] = {
    use crate::FormatClass::*;
    [
        ADRI,
        AddSubImmShift,
        BaseLogicalImm,
        BaseMoveImmediate,
        BaseInsertImmediate,
        BaseBitfieldImm,
        BaseBitfieldImmWith2RegArgs,
        BaseExtractImm,
        BranchImm,
        CallImm,
        BranchCond,
        BaseCmpBranch,
        BaseTestBranch,
        BranchReg,
    ]
};

/// The format classes of scalar floating point (card C4), for the tests and
/// the lockstep.
pub const FP_FORMATS: [crate::FormatClass; 16] = {
    use crate::FormatClass::*;
    [
        BaseSingleOperandFPData,
        BaseTwoOperandFPData,
        BaseThreeOperandFPData,
        BaseTwoOperandFPComparison,
        BaseOneOperandFPComparison,
        BaseFPCondComparison,
        BaseFPCondSelect,
        BaseFPConversion,
        BaseFPMoveImmediate,
        BaseFPToIntegerUnscaled,
        BaseFPToInteger,
        BaseIntegerToFPUnscaled,
        BaseIntegerToFP,
        BaseUnscaledConversion,
        BaseUnscaledConversionFromHigh,
        BaseUnscaledConversionToHigh,
    ]
};
