//! The few A64 instructions that the harness writes itself: the preludes,
//! the vector tables and the fixed parts of a system stream. The encodings
//! are the fixed bits of the records in `generated/decode.rs` (S1).

/// `MOVZ Xd, #imm16, LSL #(16 * part)`.
pub fn movz(rd: u32, imm16: u16, part: u32) -> u32 {
    0xd280_0000 | (part & 3) << 21 | u32::from(imm16) << 5 | (rd & 31)
}

/// `MOVK Xd, #imm16, LSL #(16 * part)`.
pub fn movk(rd: u32, imm16: u16, part: u32) -> u32 {
    0xf280_0000 | (part & 3) << 21 | u32::from(imm16) << 5 | (rd & 31)
}

/// Four instructions that put `value` into Xd.
pub fn load(rd: u32, value: u64) -> [u32; 4] {
    [
        movz(rd, value as u16, 0),
        movk(rd, (value >> 16) as u16, 1),
        movk(rd, (value >> 32) as u16, 2),
        movk(rd, (value >> 48) as u16, 3),
    ]
}

/// `MRS Xt, <register>`, the register by its 16-bit encoding.
pub fn mrs(rt: u32, encoding: u16) -> u32 {
    0xd520_0000 | u32::from(encoding) << 5 | (rt & 31)
}

/// `MSR <register>, Xt`.
pub fn msr(encoding: u16, rt: u32) -> u32 {
    0xd500_0000 | u32::from(encoding) << 5 | (rt & 31)
}

/// `ADD Xd, Xn, #imm12`.
pub fn add(rd: u32, rn: u32, imm12: u32) -> u32 {
    0x9100_0000 | (imm12 & 0xfff) << 10 | (rn & 31) << 5 | (rd & 31)
}

/// `B` to an offset in instructions.
pub fn b(offset: i32) -> u32 {
    0x1400_0000 | (offset as u32 & 0x03ff_ffff)
}

/// `CBNZ Xt` to an offset in instructions.
pub fn cbnz(rt: u32, offset: i32) -> u32 {
    0xb500_0000 | (offset as u32 & 0x7_ffff) << 5 | (rt & 31)
}

/// `EOR Xd, Xn, Xm`.
pub fn eor(rd: u32, rn: u32, rm: u32) -> u32 {
    0xca00_0000 | (rm & 31) << 16 | (rn & 31) << 5 | (rd & 31)
}

pub fn svc(imm: u16) -> u32 {
    0xd400_0001 | u32::from(imm) << 5
}

pub fn hvc(imm: u16) -> u32 {
    0xd400_0002 | u32::from(imm) << 5
}

pub const ERET: u32 = 0xd69f_03e0;
pub const NOP: u32 = 0xd503_201f;

/// The 16-bit encodings of the registers that the harness names itself.
pub mod reg {
    pub const SPSR_EL1: u16 = 0xc200;
    pub const ELR_EL1: u16 = 0xc201;
    pub const ESR_EL1: u16 = 0xc290;
    pub const FAR_EL1: u16 = 0xc300;
    pub const VBAR_EL1: u16 = 0xc600;
    pub const CPACR_EL1: u16 = 0xc082;
    pub const SCTLR_EL1: u16 = 0xc080;
}

/// The bytes of `words`, as the memory holds them.
pub fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::decode::{Record, decode};
    use crate::generated::sysreg::SYSREGS;

    #[test]
    fn the_words_decode_to_the_records_that_they_are_meant_to_be() {
        let name = |word| decode(word).map(|decoded| decoded.record);
        assert_eq!(name(movz(3, 0x1234, 1)), Some(Record::MOVZXi));
        assert_eq!(name(movk(3, 0x1234, 3)), Some(Record::MOVKXi));
        assert_eq!(name(mrs(0, reg::ESR_EL1)), Some(Record::MRS));
        assert_eq!(name(msr(reg::VBAR_EL1, 16)), Some(Record::MSR));
        assert_eq!(name(add(17, 17, 4)), Some(Record::ADDXri));
        assert_eq!(name(b(-1)), Some(Record::B));
        assert_eq!(name(cbnz(1, 2)), Some(Record::CBNZX));
        assert_eq!(name(eor(1, 2, 3)), Some(Record::EORXrs));
        assert_eq!(name(svc(7)), Some(Record::SVC));
        assert_eq!(name(hvc(0)), Some(Record::HVC));
        assert_eq!(name(ERET), Some(Record::ERET));
        assert_eq!(name(NOP), Some(Record::NOP));
        assert_eq!(load(5, 0x1122_3344_5566_7788)[3], movk(5, 0x1122, 3));
        assert_eq!(bytes(&[0x0403_0201]), vec![1, 2, 3, 4]);
    }

    #[test]
    fn the_named_registers_have_the_encodings_of_the_llvm_tables() {
        for (name, encoding) in [
            ("SPSR_EL1", reg::SPSR_EL1),
            ("ELR_EL1", reg::ELR_EL1),
            ("ESR_EL1", reg::ESR_EL1),
            ("FAR_EL1", reg::FAR_EL1),
            ("VBAR_EL1", reg::VBAR_EL1),
            ("CPACR_EL1", reg::CPACR_EL1),
            ("SCTLR_EL1", reg::SCTLR_EL1),
        ] {
            let found = SYSREGS.iter().find(|register| register.name == name);
            assert_eq!(found.map(|register| register.encoding), Some(encoding));
        }
    }
}
