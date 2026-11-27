//! A small x86-64 assembler: the instructions that the emitter uses, with
//! 64-bit operands unless a name says otherwise, labels in a hot and a cold
//! section, and jumps to absolute addresses inside the code region.
//!
//! Sources read: the Intel 64 and IA-32 Architectures Software Developer's
//! Manual, volume 2 (the instruction formats, REX, ModRM and SIB, and the
//! opcode tables). The test at the end compares every form with `llvm-mc`
//! when it is installed (DESIGN.md J1).

/// A general-purpose register by its number: 0 is RAX, 15 is R15.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Reg(pub u8);

pub const RAX: Reg = Reg(0);
pub const RCX: Reg = Reg(1);
pub const RDX: Reg = Reg(2);
pub const RBX: Reg = Reg(3);
pub const RSP: Reg = Reg(4);
pub const RBP: Reg = Reg(5);
pub const RSI: Reg = Reg(6);
pub const RDI: Reg = Reg(7);
pub const R8: Reg = Reg(8);
pub const R9: Reg = Reg(9);
pub const R10: Reg = Reg(10);
pub const R11: Reg = Reg(11);
pub const R12: Reg = Reg(12);
pub const R13: Reg = Reg(13);
pub const R14: Reg = Reg(14);
pub const R15: Reg = Reg(15);

/// A register or a quadword in memory at a base register and a
/// displacement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rm {
    Reg(Reg),
    Mem(Reg, i32),
}

/// The arithmetic and logic group: the value is the `/n` of the immediate
/// forms (opcodes 81 and 83).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alu {
    Add = 0,
    Or = 1,
    /// Add with the carry, and subtract with the borrow: the high words of
    /// a value of two words.
    Adc = 2,
    Sbb = 3,
    And = 4,
    Sub = 5,
    Xor = 6,
    Cmp = 7,
}

/// The shifts: the value is the `/n` of opcodes C1 and D3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    Shl = 4,
    Shr = 5,
    Sar = 7,
}

/// A condition code, as the low nibble of `Jcc`, `SETcc` and `CMOVcc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    /// Overflow: OF set.
    O = 0,
    No = 1,
    /// Below: CF set.
    B = 2,
    /// Above or equal: CF clear.
    Ae = 3,
    E = 4,
    Ne = 5,
    /// Below or equal: CF or ZF set.
    Be = 6,
    /// Above: CF and ZF clear.
    A = 7,
    /// Sign: SF set.
    S = 8,
    Ns = 9,
    /// Less, signed: SF and OF differ.
    L = 0xc,
    Ge = 0xd,
    /// Less or equal, signed: ZF set, or SF and OF differ.
    Le = 0xe,
    G = 0xf,
}

impl Cond {
    /// The condition that holds where this one does not.
    pub fn inverse(self) -> Self {
        match self {
            Cond::O => Cond::No,
            Cond::No => Cond::O,
            Cond::B => Cond::Ae,
            Cond::Ae => Cond::B,
            Cond::E => Cond::Ne,
            Cond::Ne => Cond::E,
            Cond::Be => Cond::A,
            Cond::A => Cond::Be,
            Cond::S => Cond::Ns,
            Cond::Ns => Cond::S,
            Cond::L => Cond::Ge,
            Cond::Ge => Cond::L,
            Cond::Le => Cond::G,
            Cond::G => Cond::Le,
        }
    }

    /// The condition of a comparison of `b` with `a` that holds where this
    /// one holds for `a` with `b`. `None`: a condition that is no order.
    pub fn swapped(self) -> Option<Self> {
        Some(match self {
            Cond::E | Cond::Ne => self,
            Cond::B => Cond::A,
            Cond::A => Cond::B,
            Cond::Ae => Cond::Be,
            Cond::Be => Cond::Ae,
            Cond::L => Cond::G,
            Cond::G => Cond::L,
            Cond::Ge => Cond::Le,
            Cond::Le => Cond::Ge,
            Cond::O | Cond::No | Cond::S | Cond::Ns => return None,
        })
    }

    /// Whether the condition holds for the flags CF, ZF, SF and OF, as
    /// volume 1 of the Intel manual defines each (appendix B).
    pub fn holds(self, cf: bool, zf: bool, sf: bool, of: bool) -> bool {
        match self {
            Cond::O => of,
            Cond::No => !of,
            Cond::B => cf,
            Cond::Ae => !cf,
            Cond::E => zf,
            Cond::Ne => !zf,
            Cond::Be => cf || zf,
            Cond::A => !cf && !zf,
            Cond::S => sf,
            Cond::Ns => !sf,
            Cond::L => sf != of,
            Cond::Ge => sf == of,
            Cond::Le => zf || sf != of,
            Cond::G => !zf && sf == of,
        }
    }
}

/// A place in the code that a jump can name before it is bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Label(usize);

#[derive(Clone, Copy, Debug)]
enum Target {
    Label(Label),
    /// An address inside the code region.
    Absolute(u64),
}

#[derive(Clone, Copy, Debug)]
struct Fixup {
    cold: bool,
    /// The offset of the 32-bit displacement in its section.
    at: usize,
    target: Target,
}

/// The code of one unit, in two sections: the hot one runs in order, the
/// cold one holds the stubs that the hot one jumps to. The cold section
/// follows the hot one in the finished code.
#[derive(Default)]
pub struct Asm {
    hot: Vec<u8>,
    cold: Vec<u8>,
    in_cold: bool,
    labels: Vec<Option<(bool, usize)>>,
    fixups: Vec<Fixup>,
}

fn fits_i8(value: i64) -> bool {
    i8::try_from(value).is_ok()
}

fn fits_i32(value: i64) -> bool {
    i32::try_from(value).is_ok()
}

impl Asm {
    pub fn new() -> Self {
        Self::default()
    }

    fn code(&mut self) -> &mut Vec<u8> {
        if self.in_cold {
            &mut self.cold
        } else {
            &mut self.hot
        }
    }

    fn byte(&mut self, byte: u8) {
        self.code().push(byte);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.code().extend_from_slice(bytes);
    }

    /// Write into the cold section from now on, or back into the hot one.
    pub fn cold(&mut self, cold: bool) {
        self.in_cold = cold;
    }

    pub fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    /// Bind `label` to the current place of the current section.
    pub fn bind(&mut self, label: Label) {
        let at = self.code().len();
        self.labels[label.0] = Some((self.in_cold, at));
    }

    /// Where `label` is in the finished code, from its start: the cold
    /// section follows the hot one. `None`: a label that is not bound.
    pub fn position(&self, label: Label) -> Option<usize> {
        let (cold, offset) = self.labels[label.0]?;
        Some(offset + if cold { self.hot.len() } else { 0 })
    }

    /// The bytes of both sections, for the size of the finished code.
    pub fn len(&self) -> usize {
        self.hot.len() + self.cold.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The finished code, placed at `base`: the hot section, then the cold
    /// one, with every jump resolved. `None`: a label that was never bound,
    /// or a target out of the reach of a 32-bit displacement.
    pub fn finish(self, base: u64) -> Option<Vec<u8>> {
        let mut code = Vec::new();
        self.finish_into(base, &mut code)?;
        Some(code)
    }

    /// Start a new unit, keeping the memory of the buffers.
    pub fn clear(&mut self) {
        self.hot.clear();
        self.cold.clear();
        self.in_cold = false;
        self.labels.clear();
        self.fixups.clear();
    }

    /// `finish` into `code`, which is cleared first and keeps its memory.
    pub fn finish_into(&self, base: u64, code: &mut Vec<u8>) -> Option<()> {
        let hot = self.hot.len();
        code.clear();
        code.extend_from_slice(&self.hot);
        code.extend_from_slice(&self.cold);
        for fixup in &self.fixups {
            let at = fixup.at + if fixup.cold { hot } else { 0 };
            let target = match fixup.target {
                Target::Label(label) => {
                    let (cold, offset) = self.labels[label.0]?;
                    base + (offset + if cold { hot } else { 0 }) as u64
                }
                Target::Absolute(address) => address,
            };
            let next = base + at as u64 + 4;
            let displacement = i32::try_from(target.wrapping_sub(next) as i64).ok()?;
            code[at..at + 4].copy_from_slice(&displacement.to_le_bytes());
        }
        Some(())
    }

    /// A REX prefix where one is needed: `w` for a 64-bit operand, or an
    /// extended register in the reg field or in the r/m field. `force`
    /// writes one also when it would be 0x40.
    fn rex(&mut self, w: bool, reg: u8, rm: Rm, force: bool) {
        let base = match rm {
            Rm::Reg(r) | Rm::Mem(r, _) => r.0,
        };
        let value = 0x40 | (u8::from(w) << 3) | ((reg >> 3) << 2) | (base >> 3);
        if value != 0x40 || force {
            self.byte(value);
        }
    }

    /// The ModRM byte, a SIB byte for a base of RSP or R12, and the
    /// displacement, in its shortest form.
    fn modrm(&mut self, reg: u8, rm: Rm) {
        match rm {
            Rm::Reg(r) => self.byte(0xc0 | ((reg & 7) << 3) | (r.0 & 7)),
            Rm::Mem(base, displacement) => {
                let low = base.0 & 7;
                let mode = if displacement == 0 && low != 5 {
                    0
                } else if fits_i8(displacement.into()) {
                    1
                } else {
                    2
                };
                self.byte((mode << 6) | ((reg & 7) << 3) | low);
                if low == 4 {
                    self.byte(0x24);
                }
                match mode {
                    1 => self.byte(displacement as u8),
                    2 => self.bytes(&displacement.to_le_bytes()),
                    _ => {}
                }
            }
        }
    }

    /// One instruction of the form `[prefix] REX opcode ModRM`.
    fn op(&mut self, prefix: Option<u8>, w: bool, opcode: &[u8], reg: u8, rm: Rm) {
        if let Some(prefix) = prefix {
            self.byte(prefix);
        }
        self.rex(w, reg, rm, false);
        self.bytes(opcode);
        self.modrm(reg, rm);
    }

    /// `mov dst, src`.
    pub fn mov(&mut self, dst: Reg, src: Rm) {
        if src == Rm::Reg(dst) {
            return;
        }
        self.op(None, true, &[0x8b], dst.0, src);
    }

    /// `mov dst, src` with a register source: a store when `dst` is memory.
    pub fn store(&mut self, dst: Rm, src: Reg) {
        if dst == Rm::Reg(src) {
            return;
        }
        self.op(None, true, &[0x89], src.0, dst);
    }

    /// `mov dst, value` in its shortest form.
    pub fn mov_imm(&mut self, dst: Reg, value: u64) {
        if value == 0 {
            self.xor32(dst, dst);
        } else if u32::try_from(value).is_ok() {
            self.rex(false, 0, Rm::Reg(dst), false);
            self.byte(0xb8 | (dst.0 & 7));
            self.bytes(&(value as u32).to_le_bytes());
        } else if fits_i32(value as i64) {
            self.op(None, true, &[0xc7], 0, Rm::Reg(dst));
            self.bytes(&(value as i32).to_le_bytes());
        } else {
            self.rex(true, 0, Rm::Reg(dst), false);
            self.byte(0xb8 | (dst.0 & 7));
            self.bytes(&value.to_le_bytes());
        }
    }

    /// A load of `bytes` (1, 2, 4 or 8) from `src`, zero-extended to 64
    /// bits: `movzx eax, byte`, `movzx eax, word`, `mov eax, dword` or
    /// `mov rax, qword`.
    pub fn load_sized(&mut self, dst: Reg, src: Rm, bytes: u8) {
        match bytes {
            1 => self.op(None, false, &[0x0f, 0xb6], dst.0, src),
            2 => self.op(None, false, &[0x0f, 0xb7], dst.0, src),
            4 => self.op(None, false, &[0x8b], dst.0, src),
            _ => self.op(None, true, &[0x8b], dst.0, src),
        }
    }

    /// A store of the low `bytes` (1, 2, 4 or 8) of `src` to `dst`. A byte
    /// of SPL, BPL, SIL or DIL takes a REX prefix.
    pub fn store_sized(&mut self, dst: Rm, src: Reg, bytes: u8) {
        match bytes {
            1 => {
                self.rex(false, src.0, dst, src.0 >= 4);
                self.byte(0x88);
                self.modrm(src.0, dst);
            }
            2 => self.op(Some(0x66), false, &[0x89], src.0, dst),
            4 => self.op(None, false, &[0x89], src.0, dst),
            _ => self.op(None, true, &[0x89], src.0, dst),
        }
    }

    /// `test byte dst, value`.
    pub fn test_byte_imm(&mut self, dst: Rm, value: u8) {
        self.op(None, false, &[0xf6], 0, dst);
        self.byte(value);
    }

    /// `mov qword dst, value`, the value sign-extended from 32 bits.
    pub fn store_imm(&mut self, dst: Rm, value: i32) {
        self.op(None, true, &[0xc7], 0, dst);
        self.bytes(&value.to_le_bytes());
    }

    /// `mov dst32, src32`: the low 32 bits, zero-extended.
    pub fn mov32(&mut self, dst: Reg, src: Reg) {
        self.op(None, false, &[0x8b], dst.0, Rm::Reg(src));
    }

    /// `xor dst32, src32`.
    pub fn xor32(&mut self, dst: Reg, src: Reg) {
        self.op(None, false, &[0x33], dst.0, Rm::Reg(src));
    }

    /// `<alu> dst, src`.
    pub fn alu(&mut self, alu: Alu, dst: Reg, src: Rm) {
        let opcode = match alu {
            Alu::Add => 0x03,
            Alu::Or => 0x0b,
            Alu::Adc => 0x13,
            Alu::Sbb => 0x1b,
            Alu::And => 0x23,
            Alu::Sub => 0x2b,
            Alu::Xor => 0x33,
            Alu::Cmp => 0x3b,
        };
        self.op(None, true, &[opcode], dst.0, src);
    }

    /// `<alu> dst32, src32`.
    pub fn alu32(&mut self, alu: Alu, dst: Reg, src: Rm) {
        let opcode = match alu {
            Alu::Add => 0x03,
            Alu::Or => 0x0b,
            Alu::Adc => 0x13,
            Alu::Sbb => 0x1b,
            Alu::And => 0x23,
            Alu::Sub => 0x2b,
            Alu::Xor => 0x33,
            Alu::Cmp => 0x3b,
        };
        self.op(None, false, &[opcode], dst.0, src);
    }

    /// `<alu> dst32, value`: the 32-bit form, whose immediate is the whole
    /// operand.
    pub fn alu_imm32(&mut self, alu: Alu, dst: Rm, value: u32) {
        if fits_i8(i64::from(value as i32)) {
            self.op(None, false, &[0x83], alu as u8, dst);
            self.byte(value as u8);
        } else {
            self.op(None, false, &[0x81], alu as u8, dst);
            self.bytes(&value.to_le_bytes());
        }
    }

    /// `<alu> dst, value`, the value sign-extended from 32 or 8 bits.
    pub fn alu_imm(&mut self, alu: Alu, dst: Rm, value: i32) {
        if fits_i8(value.into()) {
            self.op(None, true, &[0x83], alu as u8, dst);
            self.byte(value as u8);
        } else {
            self.op(None, true, &[0x81], alu as u8, dst);
            self.bytes(&value.to_le_bytes());
        }
    }

    /// `test a, b`.
    pub fn test(&mut self, a: Reg, b: Reg) {
        self.op(None, true, &[0x85], b.0, Rm::Reg(a));
    }

    /// `test a, b` with `a` a register or memory, of 64 bits or of 32.
    pub fn test_rm(&mut self, a: Rm, b: Reg, wide: bool) {
        self.op(None, wide, &[0x85], b.0, a);
    }

    /// `test a32, b32`.
    pub fn test32(&mut self, a: Reg, b: Reg) {
        self.op(None, false, &[0x85], b.0, Rm::Reg(a));
    }

    /// `imul dst, src`: the low 64 bits of the product.
    pub fn imul(&mut self, dst: Reg, src: Rm) {
        self.op(None, true, &[0x0f, 0xaf], dst.0, src);
    }

    /// `<shift> dst, cl`.
    pub fn shift_cl(&mut self, shift: Shift, dst: Reg) {
        self.op(None, true, &[0xd3], shift as u8, Rm::Reg(dst));
    }

    /// `<shift> dst, count`. A count of zero writes nothing.
    pub fn shift_imm(&mut self, shift: Shift, dst: Reg, count: u8) {
        if count == 0 {
            return;
        }
        self.op(None, true, &[0xc1], shift as u8, Rm::Reg(dst));
        self.byte(count);
    }

    pub fn not(&mut self, dst: Reg) {
        self.op(None, true, &[0xf7], 2, Rm::Reg(dst));
    }

    pub fn neg(&mut self, dst: Reg) {
        self.op(None, true, &[0xf7], 3, Rm::Reg(dst));
    }

    /// `set<cond> al`.
    pub fn setcc_al(&mut self, cond: Cond) {
        self.bytes(&[0x0f, 0x90 | cond as u8, 0xc0]);
    }

    /// `cmov<cond> dst, src`.
    pub fn cmov(&mut self, cond: Cond, dst: Reg, src: Rm) {
        self.op(None, true, &[0x0f, 0x40 | cond as u8], dst.0, src);
    }

    pub fn lzcnt(&mut self, dst: Reg, src: Reg) {
        self.op(Some(0xf3), true, &[0x0f, 0xbd], dst.0, Rm::Reg(src));
    }

    pub fn tzcnt(&mut self, dst: Reg, src: Reg) {
        self.op(Some(0xf3), true, &[0x0f, 0xbc], dst.0, Rm::Reg(src));
    }

    pub fn popcnt(&mut self, dst: Reg, src: Reg) {
        self.op(Some(0xf3), true, &[0x0f, 0xb8], dst.0, Rm::Reg(src));
    }

    pub fn bswap(&mut self, dst: Reg) {
        self.rex(true, 0, Rm::Reg(dst), false);
        self.bytes(&[0x0f, 0xc8 | (dst.0 & 7)]);
    }

    pub fn push(&mut self, reg: Reg) {
        self.rex(false, 0, Rm::Reg(reg), false);
        self.byte(0x50 | (reg.0 & 7));
    }

    pub fn pop(&mut self, reg: Reg) {
        self.rex(false, 0, Rm::Reg(reg), false);
        self.byte(0x58 | (reg.0 & 7));
    }

    /// `call qword [target]`.
    pub fn call_indirect(&mut self, target: Rm) {
        self.op(None, false, &[0xff], 2, target);
    }

    /// `jmp target` with a register.
    pub fn jmp_reg(&mut self, target: Reg) {
        self.op(None, false, &[0xff], 4, Rm::Reg(target));
    }

    /// `jmp qword [target]`, or `jmp target` with a register.
    pub fn jmp_indirect(&mut self, target: Rm) {
        self.op(None, false, &[0xff], 4, target);
    }

    pub fn ret(&mut self) {
        self.byte(0xc3);
    }

    fn fixup(&mut self, target: Target) {
        let at = self.code().len();
        let cold = self.in_cold;
        self.fixups.push(Fixup { cold, at, target });
        self.bytes(&[0; 4]);
    }

    /// `jmp label`, with a 32-bit displacement.
    pub fn jmp(&mut self, label: Label) {
        self.byte(0xe9);
        self.fixup(Target::Label(label));
    }

    /// `jmp address`, an address inside the code region.
    pub fn jmp_absolute(&mut self, address: u64) {
        self.byte(0xe9);
        self.fixup(Target::Absolute(address));
    }

    /// `j<cond> label`, with a 32-bit displacement.
    pub fn jcc(&mut self, cond: Cond, label: Label) {
        self.bytes(&[0x0f, 0x80 | cond as u8]);
        self.fixup(Target::Label(label));
    }

    /// `j<cond> address`, an address inside the code region.
    pub fn jcc_absolute(&mut self, cond: Cond, address: u64) {
        self.bytes(&[0x0f, 0x80 | cond as u8]);
        self.fixup(Target::Absolute(address));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    const NAMES: [&str; 16] = [
        "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12",
        "r13", "r14", "r15",
    ];
    const NAMES32: [&str; 16] = [
        "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d",
        "r12d", "r13d", "r14d", "r15d",
    ];
    const NAMES16: [&str; 16] = [
        "ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w",
        "r13w", "r14w", "r15w",
    ];
    const NAMES8: [&str; 16] = [
        "al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b",
        "r13b", "r14b", "r15b",
    ];

    fn reg(n: usize) -> Reg {
        Reg(n as u8)
    }

    fn rm_text(rm: Rm) -> String {
        match rm {
            Rm::Reg(r) => NAMES[usize::from(r.0)].to_string(),
            Rm::Mem(base, displacement) => {
                let base = NAMES[usize::from(base.0)];
                match displacement {
                    0 => format!("qword ptr [{base}]"),
                    d if d > 0 => format!("qword ptr [{base} + {d}]"),
                    d => format!("qword ptr [{base} - {}]", -i64::from(d)),
                }
            }
        }
    }

    /// Every form of the assembler with operands over the registers and the
    /// displacements whose encodings differ, and the Intel syntax of each.
    fn cases() -> Vec<(Vec<u8>, String)> {
        let mut cases = Vec::new();
        let mut add = |text: String, f: &dyn Fn(&mut Asm)| {
            let mut asm = Asm::new();
            f(&mut asm);
            let code = asm.finish(0x1000).expect("no labels");
            cases.push((code, text));
        };
        let mems: Vec<Rm> = [RAX, RSP, RBP, R12, R13, R14, R15]
            .iter()
            .flat_map(|base| {
                [0, 8, -8, 127, 128, -129, 0x1234_5678]
                    .map(|displacement| Rm::Mem(*base, displacement))
            })
            .collect();
        let alus = [
            (Alu::Add, "add"),
            (Alu::Or, "or"),
            (Alu::Adc, "adc"),
            (Alu::Sbb, "sbb"),
            (Alu::And, "and"),
            (Alu::Sub, "sub"),
            (Alu::Xor, "xor"),
            (Alu::Cmp, "cmp"),
        ];
        for d in 0..16 {
            let dst = reg(d);
            let name = NAMES[d];
            for s in 0..16 {
                let src = reg(s);
                if s != d {
                    add(format!("mov {name}, {}", NAMES[s]), &|a| {
                        a.mov(dst, Rm::Reg(src))
                    });
                }
                for (alu, text) in alus {
                    add(format!("{text} {name}, {}", NAMES[s]), &|a| {
                        a.alu(alu, dst, Rm::Reg(src))
                    });
                }
                add(format!("imul {name}, {}", NAMES[s]), &|a| {
                    a.imul(dst, Rm::Reg(src))
                });
                add(format!("test {}, {name}", NAMES[s]), &|a| a.test(src, dst));
                add(format!("cmove {name}, {}", NAMES[s]), &|a| {
                    a.cmov(Cond::E, dst, Rm::Reg(src))
                });
                add(format!("lzcnt {name}, {}", NAMES[s]), &|a| {
                    a.lzcnt(dst, src)
                });
                add(format!("tzcnt {name}, {}", NAMES[s]), &|a| {
                    a.tzcnt(dst, src)
                });
                add(format!("popcnt {name}, {}", NAMES[s]), &|a| {
                    a.popcnt(dst, src)
                });
                add(format!("mov {}, {}", NAMES32[d], NAMES32[s]), &|a| {
                    a.mov32(dst, src)
                });
                add(format!("xor {}, {}", NAMES32[d], NAMES32[s]), &|a| {
                    a.xor32(dst, src)
                });
                for (alu, text) in alus {
                    add(format!("{text} {}, {}", NAMES32[d], NAMES32[s]), &|a| {
                        a.alu32(alu, dst, Rm::Reg(src))
                    });
                }
                add(format!("test {}, {}", NAMES32[s], NAMES32[d]), &|a| {
                    a.test32(src, dst)
                });
            }
            for mem in &mems {
                let text = rm_text(*mem);
                add(format!("mov {name}, {text}"), &|a| a.mov(dst, *mem));
                add(format!("mov {text}, {name}"), &|a| a.store(*mem, dst));
                add(format!("add {name}, {text}"), &|a| {
                    a.alu(Alu::Add, dst, *mem)
                });
                add(format!("cmp {name}, {text}"), &|a| {
                    a.alu(Alu::Cmp, dst, *mem)
                });
                add(format!("imul {name}, {text}"), &|a| a.imul(dst, *mem));
                add(format!("cmovb {name}, {text}"), &|a| {
                    a.cmov(Cond::B, dst, *mem)
                });
                let text32 = text.replace("qword", "dword");
                add(format!("cmp {}, {text32}", NAMES32[d]), &|a| {
                    a.alu32(Alu::Cmp, dst, *mem)
                });
                add(format!("add {}, {text32}", NAMES32[d]), &|a| {
                    a.alu32(Alu::Add, dst, *mem)
                });
                add(format!("test {text}, {name}"), &|a| {
                    a.test_rm(*mem, dst, true)
                });
                add(format!("test {text32}, {}", NAMES32[d]), &|a| {
                    a.test_rm(*mem, dst, false)
                });
                let sized = |size: &str| text.replacen("qword", size, 1);
                let (byte, word, dword) = (sized("byte"), sized("word"), sized("dword"));
                let name32 = NAMES32[d];
                add(format!("movzx {name32}, {byte}"), &|a| {
                    a.load_sized(dst, *mem, 1)
                });
                add(format!("movzx {name32}, {word}"), &|a| {
                    a.load_sized(dst, *mem, 2)
                });
                add(format!("mov {name32}, {dword}"), &|a| {
                    a.load_sized(dst, *mem, 4)
                });
                add(format!("mov {name}, {text}"), &|a| {
                    a.load_sized(dst, *mem, 8)
                });
                add(format!("mov {byte}, {}", NAMES8[d]), &|a| {
                    a.store_sized(*mem, dst, 1)
                });
                add(format!("mov {word}, {}", NAMES16[d]), &|a| {
                    a.store_sized(*mem, dst, 2)
                });
                add(format!("mov {dword}, {name32}"), &|a| {
                    a.store_sized(*mem, dst, 4)
                });
                add(format!("mov {text}, {name}"), &|a| {
                    a.store_sized(*mem, dst, 8)
                });
            }
            for value in [
                1u64,
                0x7f,
                0xffff_ffff,
                u64::MAX,
                0xffff_ffff_8000_0000,
                1 << 40,
            ] {
                let text = if value <= u64::from(u32::MAX) {
                    format!("mov {}, {value}", NAMES32[d])
                } else if fits_i32(value as i64) {
                    format!("mov {name}, {}", value as i64)
                } else {
                    format!("movabs {name}, {value}")
                };
                add(text, &|a| a.mov_imm(dst, value));
            }
            for (alu, text) in alus {
                for value in [1i32, -1, 127, 128, -129, 0x7fff_ffff, i32::MIN] {
                    add(format!("{text} {name}, {value}"), &|a| {
                        a.alu_imm(alu, Rm::Reg(dst), value)
                    });
                }
            }
            for value in [
                1u32,
                0x7f,
                0x80,
                0xffff_ff80,
                0x7fff_ffff,
                0x8000_0000,
                u32::MAX,
            ] {
                // llvm-mc prints the immediate of eight bits signed and the
                // one of 32 bits unsigned.
                let text = if fits_i8(i64::from(value as i32)) {
                    (value as i32).to_string()
                } else {
                    value.to_string()
                };
                add(format!("cmp {}, {text}", NAMES32[d]), &|a| {
                    a.alu_imm32(Alu::Cmp, Rm::Reg(dst), value)
                });
            }
            for (shift, text) in [
                (Shift::Shl, "shl"),
                (Shift::Shr, "shr"),
                (Shift::Sar, "sar"),
            ] {
                add(format!("{text} {name}, cl"), &|a| a.shift_cl(shift, dst));
                // A count of 1 prints as the form without a count (D1).
                for count in [2u8, 7, 63] {
                    add(format!("{text} {name}, {count}"), &|a| {
                        a.shift_imm(shift, dst, count)
                    });
                }
            }
            add(format!("not {name}"), &|a| a.not(dst));
            add(format!("neg {name}"), &|a| a.neg(dst));
            add(format!("bswap {name}"), &|a| a.bswap(dst));
            add(format!("push {name}"), &|a| a.push(dst));
            add(format!("pop {name}"), &|a| a.pop(dst));
            add(format!("jmp {name}"), &|a| a.jmp_reg(dst));
        }
        for mem in &mems {
            let text = rm_text(*mem);
            add(format!("mov {text}, -5"), &|a| a.store_imm(*mem, -5));
            add(format!("mov {text}, 100000"), &|a| {
                a.store_imm(*mem, 100_000)
            });
            add(format!("sub {text}, 64"), &|a| {
                a.alu_imm(Alu::Sub, *mem, 64)
            });
            add(format!("add {text}, 1000"), &|a| {
                a.alu_imm(Alu::Add, *mem, 1000)
            });
            add(format!("cmp {text}, 0"), &|a| a.alu_imm(Alu::Cmp, *mem, 0));
            let text32 = text.replace("qword", "dword");
            add(format!("cmp {text32}, 3"), &|a| {
                a.alu_imm32(Alu::Cmp, *mem, 3)
            });
            add(format!("cmp {text32}, -1"), &|a| {
                a.alu_imm32(Alu::Cmp, *mem, u32::MAX)
            });
            add(format!("cmp {text32}, 2147483648"), &|a| {
                a.alu_imm32(Alu::Cmp, *mem, 0x8000_0000)
            });
            add(format!("call {text}"), &|a| a.call_indirect(*mem));
            add(
                format!("test {}, 1", text.replacen("qword", "byte", 1)),
                &|a| a.test_byte_imm(*mem, 1),
            );
            add(format!("jmp {text}"), &|a| a.jmp_indirect(*mem));
        }
        for (cond, text) in [
            (Cond::B, "b"),
            (Cond::Ae, "ae"),
            (Cond::E, "e"),
            (Cond::Ne, "ne"),
            (Cond::Be, "be"),
            (Cond::A, "a"),
            (Cond::S, "s"),
            (Cond::O, "o"),
            (Cond::No, "no"),
            (Cond::Ns, "ns"),
            (Cond::L, "l"),
            (Cond::Ge, "ge"),
            (Cond::Le, "le"),
            (Cond::G, "g"),
        ] {
            add(format!("cmov{text} rax, rbx"), &|a| {
                a.cmov(cond, RAX, Rm::Reg(RBX))
            });
            add(format!("set{text} al"), &|a| a.setcc_al(cond));
            add(format!("j{text} 0x2000"), &|a| a.jcc_absolute(cond, 0x2000));
        }
        add("jmp 0x2000".into(), &|a| a.jmp_absolute(0x2000));
        add("ret".into(), &|a| a.ret());
        cases
    }

    fn llvm_mc(args: &[&str], input: &str) -> Option<String> {
        let mut child = Command::new("llvm-mc")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        child
            .stdin
            .take()
            .expect("a pipe")
            .write_all(input.as_bytes())
            .expect("llvm-mc reads its input");
        let output = child.wait_with_output().expect("llvm-mc ends");
        assert!(
            output.status.success(),
            "llvm-mc {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(String::from_utf8(output.stdout).expect("text"))
    }

    /// The instructions of a listing of llvm-mc, one per line, without the
    /// directives and the comments.
    fn instructions(listing: &str) -> Vec<String> {
        listing
            .lines()
            .map(|line| line.split('#').next().unwrap_or("").trim())
            .filter(|line| !line.is_empty() && !line.starts_with('.'))
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    }

    /// Each form of the assembler, disassembled by llvm-mc, is the
    /// instruction that its text names, assembled and printed by llvm-mc.
    /// So the bytes decode to that instruction and to nothing more. The test
    /// says so and passes where llvm-mc is not installed.
    #[test]
    fn every_form_is_the_instruction_that_llvm_mc_reads() {
        let cases = cases();
        let intel = [
            "-triple=x86_64",
            "-x86-asm-syntax=intel",
            "-output-asm-variant=1",
        ];
        // A jump is printed relative to its own address by the
        // disassembler, so each case is placed at 0x1000 and decoded alone.
        // A relative jump is not assembled: Intel syntax reads its number
        // as a memory operand. Its mnemonic and its displacement are
        // compared.
        let is_jump = |text: &str| text.starts_with('j') && text.ends_with("0x2000");
        let mut source = String::from(".intel_syntax noprefix\n");
        let mut bytes = String::new();
        for (code, text) in &cases {
            if !is_jump(text) {
                source.push_str(text);
                source.push('\n');
            }
            for byte in code {
                bytes.push_str(&format!("0x{byte:02x} "));
            }
            bytes.push('\n');
        }
        let Some(assembled) = llvm_mc(&intel, &source) else {
            eprintln!("llvm-mc is not installed: the encodings were not compared");
            return;
        };
        let mut assembled = instructions(&assembled).into_iter();
        let mut disassemble = intel.to_vec();
        disassemble.push("--disassemble");
        let got = instructions(&llvm_mc(&disassemble, &bytes).expect("llvm-mc ran before"));
        let mut wrong = Vec::new();
        for (index, (code, text)) in cases.iter().enumerate() {
            let line = got.get(index).map(String::as_str).unwrap_or("<none>");
            if is_jump(text) {
                // Printed as the displacement from the next instruction.
                let displacement = 0x2000i64 - (0x1000 + code.len() as i64);
                if line.split_whitespace().next() != text.split_whitespace().next()
                    || !line.ends_with(&displacement.to_string())
                {
                    wrong.push(format!("{text}: {code:02x?} decodes as {line:?}"));
                }
            } else {
                let want = assembled.next().expect("llvm-mc read every case");
                if line != want {
                    wrong.push(format!(
                        "{text}: {code:02x?} decodes as {line:?}, not {want:?}"
                    ));
                }
            }
        }
        assert!(assembled.next().is_none(), "llvm-mc read one line per case");
        assert_eq!(
            got.len(),
            cases.len(),
            "one instruction per case: {wrong:#?}"
        );
        assert!(
            wrong.is_empty(),
            "{} of {}: {wrong:#?}",
            wrong.len(),
            cases.len()
        );
    }

    const CONDS: [Cond; 14] = [
        Cond::O,
        Cond::No,
        Cond::B,
        Cond::Ae,
        Cond::E,
        Cond::Ne,
        Cond::Be,
        Cond::A,
        Cond::S,
        Cond::Ns,
        Cond::L,
        Cond::Ge,
        Cond::Le,
        Cond::G,
    ];

    /// The flags of `cmp a, b` in 64 bits, from the definitions of `sub`.
    fn compare(a: u64, b: u64) -> (bool, bool, bool, bool) {
        let result = a.wrapping_sub(b);
        let signed = i128::from(a as i64) - i128::from(b as i64);
        (
            a < b,
            result == 0,
            (result as i64) < 0,
            signed != i128::from(result as i64),
        )
    }

    #[test]
    fn an_inverse_holds_where_its_condition_does_not_and_a_swap_reverses_the_order() {
        for bits in 0..16u8 {
            let flags = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
            for cond in CONDS {
                let (cf, zf, sf, of) = flags;
                assert_eq!(
                    cond.inverse().holds(cf, zf, sf, of),
                    !cond.holds(cf, zf, sf, of)
                );
                assert_eq!(cond.inverse().inverse(), cond);
            }
        }
        let values = [
            0,
            1,
            2,
            0x7fff_ffff_ffff_ffff,
            1 << 63,
            (1 << 63) + 1,
            u64::MAX - 1,
            u64::MAX,
        ];
        for a in values {
            for b in values {
                for cond in CONDS {
                    let Some(swapped) = cond.swapped() else {
                        continue;
                    };
                    let (cf, zf, sf, of) = compare(a, b);
                    let (cf2, zf2, sf2, of2) = compare(b, a);
                    assert_eq!(
                        swapped.holds(cf2, zf2, sf2, of2),
                        cond.holds(cf, zf, sf, of),
                        "{cond:?} {a:#x} {b:#x}"
                    );
                }
            }
        }
        // The signed and unsigned orders, as the conditions after `cmp`.
        for a in values {
            for b in values {
                let (cf, zf, sf, of) = compare(a, b);
                assert_eq!(Cond::B.holds(cf, zf, sf, of), a < b);
                assert_eq!(Cond::L.holds(cf, zf, sf, of), (a as i64) < (b as i64));
                assert_eq!(Cond::Le.holds(cf, zf, sf, of), (a as i64) <= (b as i64));
            }
        }
    }

    #[test]
    fn labels_resolve_in_both_sections() {
        let mut asm = Asm::new();
        let stub = asm.label();
        let back = asm.label();
        asm.jcc(Cond::E, stub);
        asm.bind(back);
        asm.ret();
        asm.cold(true);
        asm.bind(stub);
        asm.jmp(back);
        let code = asm.finish(0x1000).expect("bound labels");
        // je +1 (over the ret), ret, jmp -10 (back to the ret).
        assert_eq!(
            code,
            [
                0x0f, 0x84, 0x01, 0, 0, 0, 0xc3, 0xe9, 0xfa, 0xff, 0xff, 0xff
            ]
        );
    }
}
