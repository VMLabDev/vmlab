//! The lifters: one function per format class of the LLVM tables
//! (DESIGN.md G3a). A lifter takes the raw fields of a decoded instruction
//! and appends its semantics to the block of a `Builder`.

pub mod branch;
pub mod builder;
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
