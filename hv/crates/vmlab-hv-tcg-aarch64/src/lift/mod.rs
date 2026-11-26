//! The lifters: one function per format class of the LLVM tables
//! (DESIGN.md G3a). A lifter takes the raw fields of a decoded instruction
//! and appends its semantics to the block of a `Builder`.

pub mod branch;
pub mod builder;
pub mod fp;
pub mod integer;
pub mod loadstore;
pub mod register;

pub use builder::{A64Exit, A64Next, Builder, ExceptionClass, Flags};

use crate::generated::decode::{Decoded, Fields};
use loadstore::Index;

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
        Fields::BaseLoadStoreUI(f) => {
            loadstore::unsigned_offset(builder, record, f.offset, f.rn, f.rt)
        }
        Fields::BaseLoadStoreUnscale(f) => {
            loadstore::signed_offset(builder, record, Index::Offset, f.offset, f.rn, f.rt)
        }
        Fields::BaseLoadStoreUnprivileged(f) => {
            loadstore::signed_offset(builder, record, Index::Offset, f.offset, f.rn, f.rt)
        }
        Fields::LoadPreIdx(f) => {
            loadstore::signed_offset(builder, record, Index::Pre, f.offset, f.rn, f.rt)
        }
        Fields::StorePreIdx(f) => {
            loadstore::signed_offset(builder, record, Index::Pre, f.offset, f.rn, f.rt)
        }
        Fields::LoadPostIdx(f) => {
            loadstore::signed_offset(builder, record, Index::Post, f.offset, f.rn, f.rt)
        }
        Fields::StorePostIdx(f) => {
            loadstore::signed_offset(builder, record, Index::Post, f.offset, f.rn, f.rt)
        }
        Fields::LoadStore8RO(f) => {
            loadstore::register_offset(builder, record, f.extend, f.rm, f.rn, f.rt)
        }
        Fields::LoadStore16RO(f) => {
            loadstore::register_offset(builder, record, f.extend, f.rm, f.rn, f.rt)
        }
        Fields::LoadStore32RO(f) => {
            loadstore::register_offset(builder, record, f.extend, f.rm, f.rn, f.rt)
        }
        Fields::LoadStore64RO(f) => {
            loadstore::register_offset(builder, record, f.extend, f.rm, f.rn, f.rt)
        }
        Fields::LoadStore128RO(f) => {
            loadstore::register_offset(builder, record, f.extend, f.rm, f.rn, f.rt)
        }
        Fields::LoadLiteral(f) => loadstore::literal(builder, record, f.label, f.rt),
        Fields::BaseLoadStorePairOffset(f) => {
            loadstore::pair(builder, record, Index::Offset, f.offset, f.rn, f.rt, f.rt2)
        }
        Fields::BaseLoadStorePairNoAlloc(f) => {
            loadstore::pair(builder, record, Index::Offset, f.offset, f.rn, f.rt, f.rt2)
        }
        Fields::LoadPairPreIdx(f) => {
            loadstore::pair(builder, record, Index::Pre, f.offset, f.rn, f.rt, f.rt2)
        }
        Fields::StorePairPreIdx(f) => {
            loadstore::pair(builder, record, Index::Pre, f.offset, f.rn, f.rt, f.rt2)
        }
        Fields::LoadPairPostIdx(f) => {
            loadstore::pair(builder, record, Index::Post, f.offset, f.rn, f.rt, f.rt2)
        }
        Fields::StorePairPostIdx(f) => {
            loadstore::pair(builder, record, Index::Post, f.offset, f.rn, f.rt, f.rt2)
        }
        Fields::LoadExclusive(f) => loadstore::load_exclusive(builder, record, f.rn, f.rt),
        Fields::LoadExclusivePair(f) => {
            loadstore::load_exclusive_pair(builder, record, f.rn, f.rt, f.rt2)
        }
        Fields::StoreExclusive(f) => loadstore::store_exclusive(builder, record, f.rn, f.rt, f.ws),
        Fields::StoreExclusivePair(f) => {
            loadstore::store_exclusive_pair(builder, record, f.rn, f.rt, f.rt2, f.ws)
        }
        Fields::LoadAcquire(f) => loadstore::ordered(builder, record, f.rn, f.rt),
        Fields::StoreRelease(f) => loadstore::ordered(builder, record, f.rn, f.rt),
        Fields::BaseSIMDLdSt(f) => loadstore::simd_multiple(builder, record, f.rn, f.vt, None),
        Fields::BaseSIMDLdStPost(f) => {
            loadstore::simd_multiple(builder, record, f.rn, f.vt, Some(f.xm))
        }
        Fields::BaseSIMDLdR(f) => loadstore::simd_replicate(builder, record, f.rn, f.vt, None),
        Fields::BaseSIMDLdRPost(f) => {
            loadstore::simd_replicate(builder, record, f.rn, f.vt, Some(f.xm))
        }
        Fields::SIMDLdStSingleB(f) => {
            loadstore::simd_single(builder, record, 1, f.idx, f.rn, f.vt, None)
        }
        Fields::SIMDLdStSingleBPost(f) => {
            loadstore::simd_single(builder, record, 1, f.idx, f.rn, f.vt, Some(f.xm))
        }
        Fields::SIMDLdStSingleBTied(f) => {
            loadstore::simd_single(builder, record, 1, f.idx, f.rn, f.vt, None)
        }
        Fields::SIMDLdStSingleBTiedPost(f) => {
            loadstore::simd_single(builder, record, 1, f.idx, f.rn, f.vt, Some(f.xm))
        }
        Fields::SIMDLdStSingleH(f) => {
            loadstore::simd_single(builder, record, 2, f.idx, f.rn, f.vt, None)
        }
        Fields::SIMDLdStSingleHPost(f) => {
            loadstore::simd_single(builder, record, 2, f.idx, f.rn, f.vt, Some(f.xm))
        }
        Fields::SIMDLdStSingleHTied(f) => {
            loadstore::simd_single(builder, record, 2, f.idx, f.rn, f.vt, None)
        }
        Fields::SIMDLdStSingleHTiedPost(f) => {
            loadstore::simd_single(builder, record, 2, f.idx, f.rn, f.vt, Some(f.xm))
        }
        Fields::SIMDLdStSingleS(f) => {
            loadstore::simd_single(builder, record, 4, f.idx, f.rn, f.vt, None)
        }
        Fields::SIMDLdStSingleSPost(f) => {
            loadstore::simd_single(builder, record, 4, f.idx, f.rn, f.vt, Some(f.xm))
        }
        Fields::SIMDLdStSingleSTied(f) => {
            loadstore::simd_single(builder, record, 4, f.idx, f.rn, f.vt, None)
        }
        Fields::SIMDLdStSingleSTiedPost(f) => {
            loadstore::simd_single(builder, record, 4, f.idx, f.rn, f.vt, Some(f.xm))
        }
        Fields::SIMDLdStSingleD(f) => {
            loadstore::simd_single(builder, record, 8, f.idx, f.rn, f.vt, None)
        }
        Fields::SIMDLdStSingleDPost(f) => {
            loadstore::simd_single(builder, record, 8, f.idx, f.rn, f.vt, Some(f.xm))
        }
        Fields::SIMDLdStSingleDTied(f) => {
            loadstore::simd_single(builder, record, 8, f.idx, f.rn, f.vt, None)
        }
        Fields::SIMDLdStSingleDTiedPost(f) => {
            loadstore::simd_single(builder, record, 8, f.idx, f.rn, f.vt, Some(f.xm))
        }
        Fields::PrefetchUI(_)
        | Fields::PrefetchLiteral(_)
        | Fields::BasePrefetchRO(_)
        | Fields::RPRFM(_) => loadstore::prefetch(),
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
        Fields::BaseAddSubSReg(fields) => register::add_sub_shifted(builder, record, fields),
        Fields::BaseAddSubEReg(fields) => register::add_sub_extended(builder, record, fields),
        Fields::BaseAddSubEReg64(fields) => register::add_sub_extended64(builder, record, fields),
        Fields::BaseAddSubCarry(fields) => register::add_sub_carry(builder, record, fields),
        Fields::BaseAddSubCarrySetFlags(fields) => {
            register::add_sub_carry_flags(builder, record, fields)
        }
        Fields::BaseLogicalSReg(fields) => register::logical_shifted(builder, record, fields),
        Fields::BaseCondComparisonImm(fields) => {
            register::conditional_compare_immediate(builder, record, fields)
        }
        Fields::BaseCondComparisonReg(fields) => {
            register::conditional_compare_register(builder, record, fields)
        }
        Fields::BaseCondSelect(fields) => register::conditional_select(builder, record, fields),
        Fields::BaseCondSelectOp(fields) => {
            register::conditional_select_op(builder, record, fields)
        }
        Fields::BaseShift(fields) => register::shift_variable(builder, record, fields),
        Fields::BaseDiv(fields) => register::divide(builder, record, fields),
        Fields::BaseMulAccum(fields) => register::multiply_add(builder, record, fields),
        Fields::WideMulAccum(fields) => register::multiply_add_long(builder, record, fields),
        Fields::MulHi(fields) => register::multiply_high(builder, record, fields),
        Fields::BaseOneOperandData(fields) => register::one_operand(builder, record, fields),
        Fields::OneWRegData(fields) => register::reverse32(builder, record, fields),
        Fields::OneXRegData(fields) => register::reverse64(builder, record, fields),
        Fields::BaseCRC32(fields) => register::crc32(builder, record, fields),
        _ => Lifted::Unsupported,
    }
}

/// The format classes of the loads and stores (card C3).
pub const LOAD_STORE_FORMATS: [crate::FormatClass; 49] = {
    use crate::FormatClass::*;
    [
        BaseLoadStoreUI,
        BaseLoadStoreUnscale,
        BaseLoadStoreUnprivileged,
        LoadPreIdx,
        StorePreIdx,
        LoadPostIdx,
        StorePostIdx,
        LoadStore8RO,
        LoadStore16RO,
        LoadStore32RO,
        LoadStore64RO,
        LoadStore128RO,
        LoadLiteral,
        BaseLoadStorePairOffset,
        BaseLoadStorePairNoAlloc,
        LoadPairPreIdx,
        StorePairPreIdx,
        LoadPairPostIdx,
        StorePairPostIdx,
        LoadExclusive,
        LoadExclusivePair,
        StoreExclusive,
        StoreExclusivePair,
        LoadAcquire,
        StoreRelease,
        BaseSIMDLdSt,
        BaseSIMDLdStPost,
        BaseSIMDLdR,
        BaseSIMDLdRPost,
        SIMDLdStSingleB,
        SIMDLdStSingleBPost,
        SIMDLdStSingleBTied,
        SIMDLdStSingleBTiedPost,
        SIMDLdStSingleH,
        SIMDLdStSingleHPost,
        SIMDLdStSingleHTied,
        SIMDLdStSingleHTiedPost,
        SIMDLdStSingleS,
        SIMDLdStSingleSPost,
        SIMDLdStSingleSTied,
        SIMDLdStSingleSTiedPost,
        SIMDLdStSingleD,
        SIMDLdStSingleDPost,
        SIMDLdStSingleDTied,
        SIMDLdStSingleDTiedPost,
        PrefetchUI,
        PrefetchLiteral,
        BasePrefetchRO,
        RPRFM,
    ]
};

/// The format classes of data processing with an immediate (card C1).
pub const IMMEDIATE_FORMATS: [crate::FormatClass; 8] = {
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
    ]
};

/// The format classes of the branches (card C1).
pub const BRANCH_FORMATS: [crate::FormatClass; 6] = {
    use crate::FormatClass::*;
    [
        BranchImm,
        CallImm,
        BranchCond,
        BaseCmpBranch,
        BaseTestBranch,
        BranchReg,
    ]
};

/// The format classes of data processing with registers (card C2).
pub const REGISTER_FORMATS: [crate::FormatClass; 19] = {
    use crate::FormatClass::*;
    [
        BaseAddSubSReg,
        BaseAddSubEReg,
        BaseAddSubEReg64,
        BaseAddSubCarry,
        BaseAddSubCarrySetFlags,
        BaseLogicalSReg,
        BaseCondComparisonImm,
        BaseCondComparisonReg,
        BaseCondSelect,
        BaseCondSelectOp,
        BaseShift,
        BaseDiv,
        BaseMulAccum,
        WideMulAccum,
        MulHi,
        BaseOneOperandData,
        OneWRegData,
        OneXRegData,
        BaseCRC32,
    ]
};

/// The format classes that have a lifter, for the tests and the lockstep.
pub const LIFTED_FORMATS: [crate::FormatClass; 33] = {
    let mut all = [crate::FormatClass::ADRI; 33];
    let mut at = 0;
    while at < 8 {
        all[at] = IMMEDIATE_FORMATS[at];
        at += 1;
    }
    while at < 14 {
        all[at] = BRANCH_FORMATS[at - 8];
        at += 1;
    }
    while at < 33 {
        all[at] = REGISTER_FORMATS[at - 14];
        at += 1;
    }
    all
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
