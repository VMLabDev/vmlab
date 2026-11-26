//! Loads and stores: the 49 format classes of the group (578 records).
//!
//! - A register at a base with an offset: `BaseLoadStoreUI` (scaled
//!   unsigned), `BaseLoadStoreUnscale`, `BaseLoadStoreUnprivileged`,
//!   `LoadPreIdx`, `StorePreIdx`, `LoadPostIdx`, `StorePostIdx`, the five
//!   `LoadStore*RO` classes (a register offset) and `LoadLiteral`.
//! - A pair: `BaseLoadStorePairOffset`, `BaseLoadStorePairNoAlloc`,
//!   `LoadPairPreIdx`, `StorePairPreIdx`, `LoadPairPostIdx`,
//!   `StorePairPostIdx`.
//! - Exclusive and ordered: `LoadExclusive`, `StoreExclusive`,
//!   `LoadExclusivePair`, `StoreExclusivePair`, `LoadAcquire`,
//!   `StoreRelease`.
//! - AdvSIMD structures: `BaseSIMDLdSt` and `BaseSIMDLdStPost` (`LD1` to
//!   `LD4`, `ST1` to `ST4` of whole registers), `BaseSIMDLdR` and
//!   `BaseSIMDLdRPost` (`LD1R` to `LD4R`), and the sixteen `SIMDLdStSingle*`
//!   classes (one lane).
//! - Prefetches, which do nothing: `PrefetchUI`, `PrefetchLiteral`,
//!   `BasePrefetchRO`, `RPRFM`, and `PRFUMi` of `BaseLoadStoreUnscale`.
//!
//! Sources read (DESIGN.md "Sources"): S1, the LLVM AArch64 tables for the
//! records, their fields, their fixed bits and their operand classes (which
//! register 31 is the stack pointer and which the zero register). S6, the
//! lockstep against QEMU, for the behaviour, with the memory writes and the
//! vector registers compared. No text of S2 to S4 was opened for this file.
//! Not read: QEMU's source, Arm's ASL or pseudocode (X1, X2).
//!
//! What a record does with its data comes from its fixed bits, not from a
//! case per record: the size, the vector bit and the operation. The tests
//! of this file check that reading against the name of every record.
//!
//! Every address here is a physical address. Alignment checks of the stack
//! pointer, translation and permissions come with the cards C6 and C8. An
//! unprivileged load or store (`LDTR`, `STTR`) is an ordinary one until
//! then. An access that the memory does not hold leaves the block before
//! its instruction: no register of the instruction has changed.

use super::{Builder, Lifted};
use crate::fields::sign_extend;
use crate::generated::decode::Record;
use vmlab_hv_tcg_ir::native::{Binary, ScalarId};

/// What a record moves between memory and its data register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Data {
    /// A general register. A load extends the `bytes` bytes of memory to
    /// `register` bits (32 or 64), with the sign when `signed`. A store
    /// writes the low `bytes` bytes.
    Integer {
        bytes: u8,
        signed: bool,
        register: u16,
    },
    /// The low `bytes` bytes of a vector register. A load clears the rest.
    Vector { bytes: u8 },
    /// A prefetch: no access and no register.
    Prefetch,
}

/// One load or store of a single register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub data: Data,
    pub load: bool,
}

impl Access {
    /// The bytes in memory. 1 for a prefetch, which scales nothing.
    pub fn bytes(self) -> u8 {
        match self.data {
            Data::Integer { bytes, .. } | Data::Vector { bytes } => bytes,
            Data::Prefetch => 1,
        }
    }
}

fn bit(value: u32, index: u32) -> bool {
    (value >> index) & 1 != 0
}

/// The access of a record with the layout `size:111:V:..:opc` (the bits 31
/// to 30, 26 and 23 to 22): every class with one data register and a base.
pub fn register_access(record: Record) -> Option<Access> {
    let value = record.info().value;
    let size = value >> 30;
    let opc = (value >> 22) & 3;
    let load = opc & 1 != 0;
    if bit(value, 26) {
        let bytes = match (size, opc >> 1) {
            (size, 0) => 1 << size,
            (0, _) => 16,
            _ => return None,
        };
        return Some(Access {
            data: Data::Vector { bytes },
            load,
        });
    }
    let bytes = 1u8 << size;
    let data = match opc {
        0 | 1 => Data::Integer {
            bytes,
            signed: false,
            register: if size == 3 { 64 } else { 32 },
        },
        2 if size == 3 => {
            return Some(Access {
                data: Data::Prefetch,
                load: false,
            });
        }
        2 => Data::Integer {
            bytes,
            signed: true,
            register: 64,
        },
        _ if size < 2 => Data::Integer {
            bytes,
            signed: true,
            register: 32,
        },
        _ => return None,
    };
    Some(Access {
        data,
        load: opc != 0,
    })
}

/// The access of a `LoadLiteral` record: `opc:011:V` (the bits 31 to 30 and
/// 26).
pub fn literal_access(record: Record) -> Option<Access> {
    let value = record.info().value;
    let data = match (bit(value, 26), value >> 30) {
        (false, 0) => Data::Integer {
            bytes: 4,
            signed: false,
            register: 32,
        },
        (false, 1) => Data::Integer {
            bytes: 8,
            signed: false,
            register: 64,
        },
        (false, 2) => Data::Integer {
            bytes: 4,
            signed: true,
            register: 64,
        },
        (true, opc @ 0..=2) => Data::Vector { bytes: 4 << opc },
        _ => return None,
    };
    Some(Access { data, load: true })
}

/// The access of each register of a pair record: `opc:101:V:..:L` (the bits
/// 31 to 30, 26 and 22).
pub fn pair_access(record: Record) -> Option<Access> {
    let value = record.info().value;
    let load = bit(value, 22);
    let data = match (bit(value, 26), value >> 30) {
        (false, 0) => Data::Integer {
            bytes: 4,
            signed: false,
            register: 32,
        },
        (false, 1) if load => Data::Integer {
            bytes: 4,
            signed: true,
            register: 64,
        },
        (false, 2) => Data::Integer {
            bytes: 8,
            signed: false,
            register: 64,
        },
        (true, opc @ 0..=2) => Data::Vector { bytes: 4 << opc },
        _ => return None,
    };
    Some(Access { data, load })
}

/// `base + offset`.
fn displace(b: &mut Builder, base: ScalarId, offset: i64) -> ScalarId {
    if offset == 0 {
        return base;
    }
    let offset = b.imm(64, offset as u64);
    b.binary(64, Binary::Add, base, offset)
}

/// Make the access at `address` with the data register `rt`.
fn transfer(b: &mut Builder, access: Access, rt: u32, address: ScalarId) {
    match (access.data, access.load) {
        (Data::Prefetch, _) => {}
        (
            Data::Integer {
                bytes,
                signed,
                register,
            },
            true,
        ) => {
            let value = b.load(address, bytes);
            let width = u16::from(bytes) * 8;
            let value = if signed {
                b.sign_extend(value, width, register)
            } else {
                b.zero_extend(value, register)
            };
            b.set_x(rt, register, value);
        }
        (Data::Integer { bytes, .. }, false) => {
            let value = b.x(rt, u16::from(bytes) * 8);
            b.store(address, bytes, value);
        }
        (Data::Vector { bytes }, true) => {
            let value = b.load(address, bytes);
            b.set_v(rt, value);
        }
        (Data::Vector { bytes }, false) => {
            let value = b.v(rt, u16::from(bytes) * 8);
            b.store(address, bytes, value);
        }
    }
}

/// How the offset of an instruction meets its base register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Index {
    /// The access is at `base + offset`. The base stays.
    Offset,
    /// The access is at `base + offset`, and the base takes that address.
    Pre,
    /// The access is at `base`, and the base takes `base + offset`.
    Post,
}

/// The address of an access and the write back of its base, for every mode
/// with an immediate offset. The write back is made after `access` ran, so
/// a store of the base register stores its old value.
fn with_base(
    b: &mut Builder,
    rn: u32,
    offset: i64,
    index: Index,
    access: impl FnOnce(&mut Builder, ScalarId),
) {
    let base = b.x_or_sp(rn, 64);
    let moved = displace(b, base, offset);
    let address = if index == Index::Post { base } else { moved };
    access(b, address);
    if index != Index::Offset {
        b.set_x_or_sp(rn, 64, moved);
    }
}

/// `BaseLoadStoreUI`: a 12-bit unsigned offset in units of the access.
pub fn unsigned_offset(b: &mut Builder, record: Record, offset: u32, rn: u32, rt: u32) -> Lifted {
    let Some(access) = register_access(record) else {
        return Lifted::Unsupported;
    };
    let offset = i64::from(offset & 0xfff) * i64::from(access.bytes());
    with_base(b, rn, offset, Index::Offset, |b, address| {
        transfer(b, access, rt, address)
    });
    Lifted::Next
}

/// `BaseLoadStoreUnscale`, `BaseLoadStoreUnprivileged` and the four classes
/// with a write back: a 9-bit signed offset in bytes.
pub fn signed_offset(
    b: &mut Builder,
    record: Record,
    index: Index,
    offset: u32,
    rn: u32,
    rt: u32,
) -> Lifted {
    let Some(access) = register_access(record) else {
        return Lifted::Unsupported;
    };
    if access.data == Data::Prefetch {
        return Lifted::Next;
    }
    let offset = sign_extend(u64::from(offset & 0x1ff), 9);
    with_base(b, rn, offset, index, |b, address| {
        transfer(b, access, rt, address)
    });
    Lifted::Next
}

/// The five `LoadStore*RO` classes: the offset is a register. `extend` is
/// two bits of the instruction: bit 0 shifts the offset left by the size of
/// the access, and bit 1 gives a 32-bit offset its sign. The W records take
/// a 32-bit offset and the X records a 64-bit one.
pub fn register_offset(
    b: &mut Builder,
    record: Record,
    extend: u32,
    rm: u32,
    rn: u32,
    rt: u32,
) -> Lifted {
    let Some(access) = register_access(record) else {
        return Lifted::Unsupported;
    };
    if access.data == Data::Prefetch {
        return Lifted::Next;
    }
    let base = b.x_or_sp(rn, 64);
    // Bit 13 of the option field: a 64-bit offset register.
    let offset = if bit(record.info().value, 13) {
        b.x(rm, 64)
    } else {
        let low = b.x(rm, 32);
        if extend & 2 != 0 {
            b.sign_extend(low, 32, 64)
        } else {
            b.zero_extend(low, 64)
        }
    };
    let shift = access.bytes().trailing_zeros();
    let offset = if extend & 1 != 0 && shift != 0 {
        b.shl(64, offset, shift)
    } else {
        offset
    };
    let address = b.binary(64, Binary::Add, base, offset);
    transfer(b, access, rt, address);
    Lifted::Next
}

/// `LoadLiteral`: the address is the PC plus a 19-bit count of words.
pub fn literal(b: &mut Builder, record: Record, label: u32, rt: u32) -> Lifted {
    let Some(access) = literal_access(record) else {
        return Lifted::Unsupported;
    };
    let offset = sign_extend(u64::from(label & 0x7_ffff), 19) << 2;
    let address = b.imm(64, b.pc().wrapping_add(offset as u64));
    transfer(b, access, rt, address);
    Lifted::Next
}

/// The six pair classes: two registers side by side in memory, at a 7-bit
/// signed offset in units of one of them. The first register is at the
/// lower address.
pub fn pair(
    b: &mut Builder,
    record: Record,
    index: Index,
    offset: u32,
    rn: u32,
    rt: u32,
    rt2: u32,
) -> Lifted {
    let Some(access) = pair_access(record) else {
        return Lifted::Unsupported;
    };
    let bytes = access.bytes();
    let offset = sign_extend(u64::from(offset & 0x7f), 7) * i64::from(bytes);
    with_base(b, rn, offset, index, |b, address| {
        let second = displace(b, address, i64::from(bytes));
        transfer(b, access, rt, address);
        transfer(b, access, rt2, second);
    });
    Lifted::Next
}

/// The size in bytes of an exclusive or ordered access: the bits 31 to 30.
fn ordered_bytes(record: Record) -> u8 {
    1 << (record.info().value >> 30)
}

/// `LoadExclusive`: `LDXR` and `LDAXR` of 1, 2, 4 or 8 bytes. The address
/// must be a multiple of the size.
pub fn load_exclusive(b: &mut Builder, record: Record, rn: u32, rt: u32) -> Lifted {
    let bytes = ordered_bytes(record);
    let address = b.x_or_sp(rn, 64);
    b.require_aligned(address, bytes);
    let value = b.load_exclusive(address, bytes);
    let value = b.zero_extend(value, 64);
    b.set_x(rt, 64, value);
    Lifted::Next
}

/// The size of one register of an exclusive pair: bit 30.
fn exclusive_pair_bytes(record: Record) -> u8 {
    if bit(record.info().value, 30) { 8 } else { 4 }
}

/// `LoadExclusivePair`: `LDXP` and `LDAXP`. One access of both registers,
/// the first at the lower address.
pub fn load_exclusive_pair(b: &mut Builder, record: Record, rn: u32, rt: u32, rt2: u32) -> Lifted {
    let bytes = exclusive_pair_bytes(record);
    let width = u16::from(bytes) * 8;
    let address = b.x_or_sp(rn, 64);
    b.require_aligned(address, bytes * 2);
    let value = b.load_exclusive(address, bytes * 2);
    let low = b.slice(value, 0, width);
    let high = b.slice(value, width, width);
    b.set_x(rt, width, low);
    b.set_x(rt2, width, high);
    Lifted::Next
}

/// The status of an exclusive store into `ws`: 0 where it wrote.
fn set_status(b: &mut Builder, ws: u32, status: ScalarId) {
    let status = b.zero_extend(status, 32);
    b.set_x(ws, 32, status);
}

/// `StoreExclusive`: `STXR` and `STLXR`.
pub fn store_exclusive(b: &mut Builder, record: Record, rn: u32, rt: u32, ws: u32) -> Lifted {
    let bytes = ordered_bytes(record);
    let address = b.x_or_sp(rn, 64);
    b.require_aligned(address, bytes);
    let value = b.x(rt, u16::from(bytes) * 8);
    let status = b.store_exclusive(address, bytes, value);
    set_status(b, ws, status);
    Lifted::Next
}

/// `StoreExclusivePair`: `STXP` and `STLXP`.
pub fn store_exclusive_pair(
    b: &mut Builder,
    record: Record,
    rn: u32,
    rt: u32,
    rt2: u32,
    ws: u32,
) -> Lifted {
    let bytes = exclusive_pair_bytes(record);
    let width = u16::from(bytes) * 8;
    let address = b.x_or_sp(rn, 64);
    b.require_aligned(address, bytes * 2);
    let low = b.x(rt, width);
    let high = b.x(rt2, width);
    let value = b.concat(high, low, width, width * 2);
    let status = b.store_exclusive(address, bytes * 2, value);
    set_status(b, ws, status);
    Lifted::Next
}

/// `LoadAcquire` and `StoreRelease`: `LDAR` and `STLR`. On one flat memory
/// with every access in order, the ordering asks for nothing more. The
/// address must be a multiple of the size.
pub fn ordered(b: &mut Builder, record: Record, rn: u32, rt: u32) -> Lifted {
    let bytes = ordered_bytes(record);
    let access = Access {
        data: Data::Integer {
            bytes,
            signed: false,
            register: if bytes == 8 { 64 } else { 32 },
        },
        load: bit(record.info().value, 22),
    };
    let address = b.x_or_sp(rn, 64);
    b.require_aligned(address, bytes);
    transfer(b, access, rt, address);
    Lifted::Next
}

/// The write back of an AdvSIMD structure access: the base moves by the
/// bytes of the access when `xm` is 31, else by the register `xm`.
fn structure_write_back(b: &mut Builder, rn: u32, base: ScalarId, xm: Option<u32>, bytes: u32) {
    let Some(xm) = xm else {
        return;
    };
    let moved = if xm == 31 {
        displace(b, base, i64::from(bytes))
    } else {
        let offset = b.x(xm, 64);
        b.binary(64, Binary::Add, base, offset)
    };
    b.set_x_or_sp(rn, 64, moved);
}

/// The shape of a `BaseSIMDLdSt` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Structures {
    /// The vector registers, from `vt` on, wrapping from 31 to 0.
    pub registers: u8,
    /// The elements of the registers alternate in memory (`LD2` to `LD4`).
    /// Without it one register follows the other (`LD1`).
    pub interleaved: bool,
    /// 8 or 16.
    pub vector_bytes: u8,
    pub element_bytes: u8,
    pub load: bool,
}

/// The shape of a record of whole registers: the bit 30 (128 bits), the bit
/// 22 (a load), the bits 15 to 12 (how many registers and how they lie in
/// memory) and the bits 11 to 10 (the size of an element).
pub fn structures(record: Record) -> Option<Structures> {
    let value = record.info().value;
    let (registers, interleaved) = match (value >> 12) & 0xf {
        0b0000 => (4, true),
        0b0010 => (4, false),
        0b0100 => (3, true),
        0b0110 => (3, false),
        0b0111 => (1, false),
        0b1000 => (2, true),
        0b1010 => (2, false),
        _ => return None,
    };
    Some(Structures {
        registers,
        interleaved,
        vector_bytes: if bit(value, 30) { 16 } else { 8 },
        element_bytes: 1 << ((value >> 10) & 3),
        load: bit(value, 22),
    })
}

/// `BaseSIMDLdSt` and `BaseSIMDLdStPost`: `LD1` to `LD4` and `ST1` to `ST4`
/// of whole registers. `xm`: the write back register of the Post class.
pub fn simd_multiple(b: &mut Builder, record: Record, rn: u32, vt: u32, xm: Option<u32>) -> Lifted {
    let Some(shape) = structures(record) else {
        return Lifted::Unsupported;
    };
    let registers = u32::from(shape.registers);
    let vector_bytes = shape.vector_bytes;
    let base = b.x_or_sp(rn, 64);
    if !shape.interleaved {
        // One register after the other. The elements of a register lie in
        // memory in their order, lowest first, so a register is one
        // little-endian access.
        for register in 0..registers {
            let address = displace(b, base, i64::from(register * u32::from(vector_bytes)));
            let access = Access {
                data: Data::Vector {
                    bytes: vector_bytes,
                },
                load: shape.load,
            };
            transfer(b, access, vt + register, address);
        }
    } else {
        let bytes = shape.element_bytes;
        let width = u16::from(bytes) * 8;
        let elements = u32::from(vector_bytes / bytes);
        // Element `e` of register `r` is at `(e * registers + r) * bytes`.
        let at = |b: &mut Builder, element: u32, register: u32| {
            displace(
                b,
                base,
                i64::from((element * registers + register) * u32::from(bytes)),
            )
        };
        if shape.load {
            let mut values: Vec<Option<ScalarId>> = vec![None; registers as usize];
            for element in 0..elements {
                for register in 0..registers {
                    let address = at(b, element, register);
                    let loaded = b.load(address, bytes);
                    let value = &mut values[register as usize];
                    *value = Some(match *value {
                        None => loaded,
                        Some(low) => {
                            let low_width = element as u16 * width;
                            b.concat(loaded, low, low_width, low_width + width)
                        }
                    });
                }
            }
            for (register, value) in values.into_iter().enumerate() {
                if let Some(value) = value {
                    b.set_v(vt + register as u32, value);
                }
            }
        } else {
            let sources: Vec<ScalarId> = (0..registers)
                .map(|register| b.v(vt + register, 128))
                .collect();
            for element in 0..elements {
                for register in 0..registers {
                    let address = at(b, element, register);
                    let value = b.slice(sources[register as usize], element as u16 * width, width);
                    b.store(address, bytes, value);
                }
            }
        }
    }
    structure_write_back(b, rn, base, xm, registers * u32::from(vector_bytes));
    Lifted::Next
}

/// How many registers a single-lane or replicating record names: the bits
/// 13 and 21 as a count from 1 to 4.
pub fn lane_registers(record: Record) -> u32 {
    let value = record.info().value;
    (u32::from(bit(value, 13)) << 1 | u32::from(bit(value, 21))) + 1
}

/// `vector`, of 128 bits, with the `width` bits at `position` replaced by
/// `element`.
fn insert(
    b: &mut Builder,
    vector: ScalarId,
    position: u16,
    width: u16,
    element: ScalarId,
) -> ScalarId {
    let mut result = element;
    let mut have = width;
    if position > 0 {
        let low = b.slice(vector, 0, position);
        have = position + width;
        result = b.concat(result, low, position, have);
    }
    if have < 128 {
        let high = b.slice(vector, have, 128 - have);
        result = b.concat(high, result, have, 128);
    }
    result
}

/// The sixteen `SIMDLdStSingle*` classes: one lane of one to four
/// registers. `idx` is the number of the lane. A load keeps the other lanes
/// of each register.
pub fn simd_single(
    b: &mut Builder,
    record: Record,
    element_bytes: u8,
    idx: u32,
    rn: u32,
    vt: u32,
    xm: Option<u32>,
) -> Lifted {
    let registers = lane_registers(record);
    let load = bit(record.info().value, 22);
    let width = u16::from(element_bytes) * 8;
    let position = idx as u16 * width;
    if position + width > 128 {
        return Lifted::Unsupported;
    }
    let base = b.x_or_sp(rn, 64);
    for register in 0..registers {
        let address = displace(b, base, i64::from(register * u32::from(element_bytes)));
        let vector = b.v(vt + register, 128);
        if load {
            let element = b.load(address, element_bytes);
            let value = insert(b, vector, position, width, element);
            b.set_v(vt + register, value);
        } else {
            let element = b.slice(vector, position, width);
            b.store(address, element_bytes, element);
        }
    }
    structure_write_back(b, rn, base, xm, registers * u32::from(element_bytes));
    Lifted::Next
}

/// `BaseSIMDLdR` and `BaseSIMDLdRPost`: `LD1R` to `LD4R`. One element for
/// each register, copied into every lane of 64 or 128 bits.
pub fn simd_replicate(
    b: &mut Builder,
    record: Record,
    rn: u32,
    vt: u32,
    xm: Option<u32>,
) -> Lifted {
    let value = record.info().value;
    let registers = lane_registers(record);
    let element_bytes = 1u8 << ((value >> 10) & 3);
    let vector_width: u16 = if bit(value, 30) { 128 } else { 64 };
    let base = b.x_or_sp(rn, 64);
    for register in 0..registers {
        let address = displace(b, base, i64::from(register * u32::from(element_bytes)));
        let mut vector = b.load(address, element_bytes);
        let mut width = u16::from(element_bytes) * 8;
        while width < vector_width {
            vector = b.concat(vector, vector, width, width * 2);
            width *= 2;
        }
        b.set_v(vt + register, vector);
    }
    structure_write_back(b, rn, base, xm, registers * u32::from(element_bytes));
    Lifted::Next
}

/// The four prefetch classes: no access, no register, no fault.
pub fn prefetch() -> Lifted {
    Lifted::Next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FormatClass;
    use crate::oracle::stream::records_of;

    fn integer(bytes: u8, signed: bool, register: u16) -> Data {
        Data::Integer {
            bytes,
            signed,
            register,
        }
    }

    /// What the letters between the mnemonic and the addressing mode of a
    /// record name say. `plain_is_integer`: `B` and `H` name a general
    /// register (the unprivileged records), not a vector register.
    fn data_of(letters: &str, plain_is_integer: bool) -> Data {
        match letters {
            "BB" => integer(1, false, 32),
            "HH" => integer(2, false, 32),
            "B" if plain_is_integer => integer(1, false, 32),
            "H" if plain_is_integer => integer(2, false, 32),
            "W" => integer(4, false, 32),
            "X" => integer(8, false, 64),
            "SBW" => integer(1, true, 32),
            "SBX" => integer(1, true, 64),
            "SHW" => integer(2, true, 32),
            "SHX" => integer(2, true, 64),
            "SW" => integer(4, true, 64),
            "B" => Data::Vector { bytes: 1 },
            "H" => Data::Vector { bytes: 2 },
            "S" => Data::Vector { bytes: 4 },
            "D" => Data::Vector { bytes: 8 },
            "Q" => Data::Vector { bytes: 16 },
            other => panic!("the letters {other} of a record name"),
        }
    }

    fn strip<'a>(name: &'a str, prefixes: &[&str], suffixes: &[&str]) -> (&'a str, &'a str) {
        let prefix = prefixes
            .iter()
            .find(|prefix| name.starts_with(**prefix))
            .unwrap_or_else(|| panic!("{name} has no known mnemonic"));
        let rest = &name[prefix.len()..];
        let suffix = suffixes
            .iter()
            .find(|suffix| rest.ends_with(**suffix))
            .unwrap_or_else(|| panic!("{name} has no known addressing mode"));
        (&name[..prefix.len()], &rest[..rest.len() - suffix.len()])
    }

    #[test]
    fn the_fixed_bits_of_a_single_register_record_say_what_its_name_says() {
        use FormatClass::*;
        let classes = [
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
        ];
        let mut checked = 0;
        for class in classes {
            for record in records_of(class) {
                let info = record.info();
                // The bits that `register_access` reads are fixed.
                assert_eq!(info.mask & 0xc4c0_0000, 0xc4c0_0000, "{}", info.name);
                let access = register_access(record)
                    .unwrap_or_else(|| panic!("{} has no access", info.name));
                if info.name == "PRFUMi" {
                    assert_eq!(access.data, Data::Prefetch);
                    continue;
                }
                let (mnemonic, letters) = strip(
                    info.name,
                    &["LDUR", "STUR", "LDTR", "STTR", "LDR", "STR"],
                    &["post", "pre", "roW", "roX", "ui", "i"],
                );
                let expected = data_of(letters, class == BaseLoadStoreUnprivileged);
                assert_eq!(access.data, expected, "{}", info.name);
                assert_eq!(access.load, mnemonic.starts_with("LD"), "{}", info.name);
                if matches!(
                    class,
                    LoadStore8RO | LoadStore16RO | LoadStore32RO | LoadStore64RO | LoadStore128RO
                ) {
                    // Bit 13 tells the X records from the W records.
                    assert_eq!(info.mask & 0x2000, 0x2000, "{}", info.name);
                    assert_eq!(
                        bit(info.value, 13),
                        info.name.ends_with("roX"),
                        "{}",
                        info.name
                    );
                }
                checked += 1;
            }
        }
        assert_eq!(
            checked,
            23 + 23 + 13 + 14 + 9 + 14 + 9 + 12 + 12 + 10 + 8 + 4
        );
    }

    #[test]
    fn the_fixed_bits_of_a_literal_and_of_a_pair_say_what_the_name_says() {
        for record in records_of(FormatClass::LoadLiteral) {
            let info = record.info();
            assert_eq!(info.mask & 0xc400_0000, 0xc400_0000, "{}", info.name);
            let (_, letters) = strip(info.name, &["LDR"], &["l"]);
            assert_eq!(
                literal_access(record),
                Some(Access {
                    data: data_of(letters, false),
                    load: true
                }),
                "{}",
                info.name
            );
        }
        use FormatClass::*;
        let mut checked = 0;
        for class in [
            BaseLoadStorePairOffset,
            BaseLoadStorePairNoAlloc,
            LoadPairPreIdx,
            StorePairPreIdx,
            LoadPairPostIdx,
            StorePairPostIdx,
        ] {
            for record in records_of(class) {
                let info = record.info();
                assert_eq!(info.mask & 0xc440_0000, 0xc440_0000, "{}", info.name);
                let (mnemonic, letters) = strip(
                    info.name,
                    &["LDNP", "STNP", "LDP", "STP"],
                    &["post", "pre", "i"],
                );
                assert_eq!(
                    pair_access(record),
                    Some(Access {
                        data: data_of(letters, false),
                        load: mnemonic.starts_with("LD")
                    }),
                    "{}",
                    info.name
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 11 + 10 + 6 + 5 + 6 + 5);
    }

    #[test]
    fn the_fixed_bits_of_an_exclusive_record_say_what_its_name_says() {
        use FormatClass::*;
        for class in [LoadExclusive, StoreExclusive, LoadAcquire, StoreRelease] {
            for record in records_of(class) {
                let info = record.info();
                assert_eq!(info.mask & 0xc040_0000, 0xc040_0000, "{}", info.name);
                let bytes = match info.name.chars().last() {
                    Some('B') => 1,
                    Some('H') => 2,
                    Some('W') => 4,
                    Some('X') => 8,
                    _ => panic!("{}", info.name),
                };
                assert_eq!(ordered_bytes(record), bytes, "{}", info.name);
                assert_eq!(
                    bit(info.value, 22),
                    info.name.starts_with("LD"),
                    "{}",
                    info.name
                );
            }
        }
        for class in [LoadExclusivePair, StoreExclusivePair] {
            for record in records_of(class) {
                let info = record.info();
                assert_eq!(info.mask & 0x4000_0000, 0x4000_0000, "{}", info.name);
                let bytes = if info.name.ends_with('X') { 8 } else { 4 };
                assert_eq!(exclusive_pair_bytes(record), bytes, "{}", info.name);
            }
        }
    }

    /// The count and the size of the lanes that the end of a record name
    /// gives: `16b`, `1d`, `2d`, `2s`, `4h`, `4s`, `8b`, `8h`.
    fn lanes(text: &str) -> (u8, u8) {
        let split = text.len() - 1;
        let count: u8 = text[..split].parse().expect("a lane count");
        let bytes = match &text[split..] {
            "b" => 1,
            "h" => 2,
            "s" => 4,
            "d" => 8,
            other => panic!("the lane size {other}"),
        };
        (count, bytes)
    }

    #[test]
    fn the_fixed_bits_of_a_structure_record_say_what_its_name_says() {
        use FormatClass::*;
        let mut checked = 0;
        for class in [BaseSIMDLdSt, BaseSIMDLdStPost] {
            for record in records_of(class) {
                let info = record.info();
                assert_eq!(info.mask & 0x4040_fc00, 0x4040_fc00, "{}", info.name);
                let name = info.name.trim_end_matches("_POST");
                // LD1Twov16b: the mnemonic, the count of registers, the lanes.
                let (count, word) = [("One", 1), ("Two", 2), ("Three", 3), ("Four", 4)]
                    .iter()
                    .find_map(|(word, count)| {
                        name[3..].starts_with(word).then_some((*count, *word))
                    })
                    .unwrap_or_else(|| panic!("{name}"));
                let (lane_count, lane_bytes) = lanes(&name[3 + word.len() + 1..]);
                assert_eq!(
                    structures(record),
                    Some(Structures {
                        registers: count,
                        interleaved: &name[2..3] != "1",
                        vector_bytes: lane_count * lane_bytes,
                        element_bytes: lane_bytes,
                        load: name.starts_with("LD"),
                    }),
                    "{}",
                    info.name
                );
                if &name[2..3] != "1" {
                    assert_eq!(name[2..3].parse::<u8>().ok(), Some(count), "{name}");
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 106 + 106);
        for class in [BaseSIMDLdR, BaseSIMDLdRPost] {
            for record in records_of(class) {
                let info = record.info();
                assert_eq!(info.mask & 0x4020_2c00, 0x4020_2c00, "{}", info.name);
                let name = info.name.trim_end_matches("_POST");
                // LD3Rv4h.
                assert_eq!(name[2..3].parse::<u32>().ok(), Some(lane_registers(record)));
                let (lane_count, lane_bytes) = lanes(&name[5..]);
                assert_eq!(1 << ((info.value >> 10) & 3), lane_bytes, "{name}");
                assert_eq!(
                    if bit(info.value, 30) { 16 } else { 8 },
                    lane_count * lane_bytes,
                    "{name}"
                );
            }
        }
        for class in [
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
        ] {
            for record in records_of(class) {
                let info = record.info();
                assert_eq!(info.mask & 0x0060_2000, 0x0060_2000, "{}", info.name);
                // ST2i16_POST.
                assert_eq!(
                    info.name[2..3].parse::<u32>().ok(),
                    Some(lane_registers(record)),
                    "{}",
                    info.name
                );
                assert_eq!(
                    bit(info.value, 22),
                    info.name.starts_with("LD"),
                    "{}",
                    info.name
                );
                // A load is a Tied class: it keeps the other lanes.
                assert_eq!(
                    class.name().contains("Tied"),
                    info.name.starts_with("LD"),
                    "{}",
                    info.name
                );
            }
        }
    }
}
