//! A small x86-64 assembler: the instructions the spike needs, 64-bit
//! operands. Encodings from the Intel manual, volume 2.

pub type Reg = u8;
pub const RAX: Reg = 0;
pub const RCX: Reg = 1;
pub const RDX: Reg = 2;
pub const RBX: Reg = 3;
pub const RSP: Reg = 4;
pub const RBP: Reg = 5;
pub const RSI: Reg = 6;
pub const RDI: Reg = 7;
pub const R8: Reg = 8;
pub const R9: Reg = 9;
pub const R10: Reg = 10;
pub const R11: Reg = 11;
pub const R12: Reg = 12;
pub const R13: Reg = 13;
pub const R14: Reg = 14;
pub const R15: Reg = 15;

/// The `/r` opcodes of `op r/m64, r64`.
pub const ADD: u8 = 0x01;
pub const OR: u8 = 0x09;
pub const AND: u8 = 0x21;
pub const SUB: u8 = 0x29;
pub const XOR: u8 = 0x31;
pub const CMP: u8 = 0x39;

/// Condition codes.
pub const CC_B: u8 = 2;
pub const CC_E: u8 = 4;
pub const CC_NE: u8 = 5;
pub const CC_BE: u8 = 6;

pub struct Asm<'a> {
    pub buf: &'a mut Vec<u8>,
}

impl Asm<'_> {
    pub fn here(&self) -> usize {
        self.buf.len()
    }

    fn rex(&mut self, w: bool, reg: Reg, rm: Reg) {
        self.buf.push(0x40 | u8::from(w) << 3 | (reg >> 3) << 2 | rm >> 3);
    }

    fn modrm_reg(&mut self, reg: Reg, rm: Reg) {
        self.buf.push(0xc0 | (reg & 7) << 3 | rm & 7);
    }

    /// `[base + disp32]`. The displacement is the last four bytes.
    fn modrm_mem(&mut self, reg: Reg, base: Reg, disp: i32) {
        self.buf.push(0x80 | (reg & 7) << 3 | base & 7);
        if base & 7 == 4 {
            self.buf.push(0x24);
        }
        self.buf.extend_from_slice(&disp.to_le_bytes());
    }

    /// `op dst, src` for the `/r` opcodes above.
    pub fn alu_rr(&mut self, op: u8, dst: Reg, src: Reg) {
        self.rex(true, src, dst);
        self.buf.push(op);
        self.modrm_reg(src, dst);
    }

    /// `op dst, imm32` (sign-extended).
    pub fn alu_ri(&mut self, op: u8, dst: Reg, imm: i32) {
        self.rex(true, 0, dst);
        self.buf.push(0x81);
        self.modrm_reg(op >> 3, dst);
        self.buf.extend_from_slice(&imm.to_le_bytes());
    }

    /// `op dst, [base + disp]`.
    pub fn alu_rm(&mut self, op: u8, dst: Reg, base: Reg, disp: i32) {
        self.rex(true, dst, base);
        self.buf.push(op + 2);
        self.modrm_mem(dst, base, disp);
    }

    pub fn mov_rr(&mut self, dst: Reg, src: Reg) {
        if dst != src {
            self.alu_rr(0x89, dst, src);
        }
    }

    /// `mov dst, imm` in its shortest form.
    pub fn mov_ri(&mut self, dst: Reg, imm: u64) {
        if imm <= u64::from(u32::MAX) {
            if dst >= 8 {
                self.buf.push(0x41);
            }
            self.buf.push(0xb8 + (dst & 7));
            self.buf.extend_from_slice(&(imm as u32).to_le_bytes());
        } else if imm as i64 == i64::from(imm as i32) {
            self.rex(true, 0, dst);
            self.buf.push(0xc7);
            self.modrm_reg(0, dst);
            self.buf.extend_from_slice(&(imm as u32).to_le_bytes());
        } else {
            self.mov_ri64(dst, imm);
        }
    }

    /// `mov dst, imm64`. The immediate is the last eight bytes.
    pub fn mov_ri64(&mut self, dst: Reg, imm: u64) {
        self.rex(true, 0, dst);
        self.buf.push(0xb8 + (dst & 7));
        self.buf.extend_from_slice(&imm.to_le_bytes());
    }

    /// `mov dst, [base + disp]`.
    pub fn load(&mut self, dst: Reg, base: Reg, disp: i32) {
        self.rex(true, dst, base);
        self.buf.push(0x8b);
        self.modrm_mem(dst, base, disp);
    }

    /// `mov [base + disp], src`.
    pub fn store(&mut self, base: Reg, disp: i32, src: Reg) {
        self.rex(true, src, base);
        self.buf.push(0x89);
        self.modrm_mem(src, base, disp);
    }

    /// `shr dst, imm8` and `shl dst, imm8`. The count is the last byte.
    pub fn shr_ri(&mut self, dst: Reg, count: u8) {
        self.rex(true, 0, dst);
        self.buf.push(0xc1);
        self.modrm_reg(5, dst);
        self.buf.push(count);
    }

    pub fn shl_ri(&mut self, dst: Reg, count: u8) {
        self.rex(true, 0, dst);
        self.buf.push(0xc1);
        self.modrm_reg(4, dst);
        self.buf.push(count);
    }

    pub fn test_rr(&mut self, a: Reg, b: Reg) {
        self.alu_rr(0x85, a, b);
    }

    /// `dst = cc ? 1 : 0`.
    pub fn setcc(&mut self, cc: u8, dst: Reg) {
        self.rex(false, 0, dst);
        self.buf.extend_from_slice(&[0x0f, 0x90 + cc]);
        self.modrm_reg(0, dst);
        // movzx dst32, dst8
        self.rex(false, dst, dst);
        self.buf.extend_from_slice(&[0x0f, 0xb6]);
        self.modrm_reg(dst, dst);
    }

    pub fn cmovcc(&mut self, cc: u8, dst: Reg, src: Reg) {
        self.rex(true, dst, src);
        self.buf.extend_from_slice(&[0x0f, 0x40 + cc]);
        self.modrm_reg(dst, src);
    }

    /// `dec qword [base + disp]`.
    pub fn dec_m(&mut self, base: Reg, disp: i32) {
        self.rex(true, 0, base);
        self.buf.push(0xff);
        self.modrm_mem(1, base, disp);
    }

    pub fn push(&mut self, reg: Reg) {
        if reg >= 8 {
            self.buf.push(0x41);
        }
        self.buf.push(0x50 + (reg & 7));
    }

    pub fn pop(&mut self, reg: Reg) {
        if reg >= 8 {
            self.buf.push(0x41);
        }
        self.buf.push(0x58 + (reg & 7));
    }

    pub fn call_r(&mut self, reg: Reg) {
        if reg >= 8 {
            self.buf.push(0x41);
        }
        self.buf.push(0xff);
        self.modrm_reg(2, reg);
    }

    pub fn ret(&mut self) {
        self.buf.push(0xc3);
    }

    /// `jcc rel32` to a place not yet known: the position to `bind`.
    pub fn jcc(&mut self, cc: u8) -> usize {
        self.buf.extend_from_slice(&[0x0f, 0x80 + cc, 0, 0, 0, 0]);
        self.here()
    }

    pub fn jmp(&mut self) -> usize {
        self.buf.extend_from_slice(&[0xe9, 0, 0, 0, 0]);
        self.here()
    }

    /// Make the jump that ends at `from` land at `to`.
    pub fn bind(&mut self, from: usize, to: usize) {
        let rel = (to as i64 - from as i64) as i32;
        self.buf[from - 4..from].copy_from_slice(&rel.to_le_bytes());
    }

    pub fn jmp_to(&mut self, to: usize) {
        let from = self.jmp();
        self.bind(from, to);
    }
}
