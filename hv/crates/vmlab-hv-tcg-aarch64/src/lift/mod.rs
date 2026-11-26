//! The lifters: one function per format class of the LLVM tables
//! (DESIGN.md G3a). A lifter takes the raw fields of a decoded instruction
//! and appends its semantics to the block of a `Builder`.

pub mod branch;
pub mod builder;
pub mod integer;
pub mod loadstore;

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

/// The format classes that have a lifter and no memory access, for the
/// tests and the lockstep.
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
