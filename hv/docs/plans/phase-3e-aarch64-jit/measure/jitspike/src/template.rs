//! The template JIT: one stencil of machine code per operation, with holes
//! for the words of the frame, copied and patched.
//!
//! Every value lives in its word of the frame (RBX), as in the interpreter.
//! A stencil reads its operands from the frame and writes its result back.
//! A stencil library built by a C compiler (copy-and-patch) could also pass
//! a few values between stencils in registers. This one does not.

use crate::asm::*;
use crate::{Block, Code, OFF_TLB, PC, TLB_ENTRIES, TLB_MASK, slow_load, slow_store};

#[derive(Clone, Copy)]
enum Hole {
    /// The displacement of the word of the operand `a`, `b`, the result.
    A,
    B,
    D,
    /// The displacement of a guest register in the state.
    Guest,
    Aux,
    Frame,
    Taken,
    Fall,
}

#[derive(Default)]
struct Stencil {
    bytes: Vec<u8>,
    holes: Vec<(usize, Hole)>,
}

impl Stencil {
    fn build(make: impl FnOnce(&mut Asm, &mut Vec<(usize, Hole)>)) -> Self {
        let mut bytes = Vec::new();
        let mut holes = Vec::new();
        make(&mut Asm { buf: &mut bytes }, &mut holes);
        Self { bytes, holes }
    }
}

const FRAME: Reg = RBX;
const STATE: Reg = R15;

fn frame_load(asm: &mut Asm, holes: &mut Vec<(usize, Hole)>, reg: Reg, hole: Hole) {
    asm.load(reg, FRAME, 0);
    holes.push((asm.here() - 4, hole));
}

fn frame_store(asm: &mut Asm, holes: &mut Vec<(usize, Hole)>, reg: Reg) {
    asm.store(FRAME, 0, reg);
    holes.push((asm.here() - 4, Hole::D));
}

/// The TLB lookup for the address in RDX. RCX holds the host address.
fn lookup(asm: &mut Asm) -> usize {
    asm.mov_rr(RAX, RDX);
    asm.shr_ri(RAX, 12);
    asm.alu_ri(AND, RAX, TLB_ENTRIES as i32 - 1);
    asm.shl_ri(RAX, 4);
    asm.alu_rm(ADD, RAX, STATE, OFF_TLB);
    asm.mov_rr(RCX, RDX);
    asm.alu_ri(AND, RCX, TLB_MASK);
    asm.alu_rm(CMP, RCX, RAX, 0);
    let miss = asm.jcc(CC_NE);
    asm.load(RCX, RAX, 8);
    asm.alu_rr(ADD, RCX, RDX);
    miss
}

pub struct Stencils {
    prologue: Stencil,
    input: Stencil,
    output: Stencil,
    add: Stencil,
    sub: Stencil,
    and: Stencil,
    or: Stencil,
    xor: Stencil,
    shr: Stencil,
    less_eq: Stencil,
    less: Stencil,
    is_zero: Stencil,
    load: Stencil,
    store: Stencil,
    exit: Stencil,
}

impl Stencils {
    pub fn new() -> Self {
        let binary = |op: u8| {
            Stencil::build(|asm, holes| {
                frame_load(asm, holes, RAX, Hole::A);
                asm.alu_rm(op, RAX, FRAME, 0);
                holes.push((asm.here() - 4, Hole::B));
                frame_store(asm, holes, RAX);
            })
        };
        let compare = |cc: u8| {
            Stencil::build(|asm, holes| {
                frame_load(asm, holes, RAX, Hole::A);
                asm.alu_rm(CMP, RAX, FRAME, 0);
                holes.push((asm.here() - 4, Hole::B));
                asm.setcc(cc, RAX);
                frame_store(asm, holes, RAX);
            })
        };
        Self {
            prologue: Stencil::build(|asm, holes| {
                asm.push(FRAME);
                asm.push(STATE);
                asm.alu_ri(SUB, RSP, 8);
                asm.mov_rr(STATE, RDI);
                asm.mov_ri64(FRAME, 0);
                holes.push((asm.here() - 8, Hole::Frame));
            }),
            input: Stencil::build(|asm, holes| {
                asm.load(RAX, STATE, 0);
                holes.push((asm.here() - 4, Hole::Guest));
                frame_store(asm, holes, RAX);
            }),
            output: Stencil::build(|asm, holes| {
                frame_load(asm, holes, RAX, Hole::A);
                asm.store(STATE, 0, RAX);
                holes.push((asm.here() - 4, Hole::Guest));
            }),
            add: binary(ADD),
            sub: binary(SUB),
            and: binary(AND),
            or: binary(OR),
            xor: binary(XOR),
            shr: Stencil::build(|asm, holes| {
                frame_load(asm, holes, RAX, Hole::A);
                asm.shr_ri(RAX, 0);
                holes.push((asm.here() - 1, Hole::Aux));
                frame_store(asm, holes, RAX);
            }),
            less_eq: compare(CC_BE),
            less: compare(CC_B),
            is_zero: Stencil::build(|asm, holes| {
                frame_load(asm, holes, RAX, Hole::A);
                asm.test_rr(RAX, RAX);
                asm.setcc(CC_E, RAX);
                frame_store(asm, holes, RAX);
            }),
            load: Stencil::build(|asm, holes| {
                frame_load(asm, holes, RDX, Hole::A);
                let miss = lookup(asm);
                asm.load(RAX, RCX, 0);
                let back = asm.here();
                frame_store(asm, holes, RAX);
                let end = asm.jmp();
                let slow = asm.here();
                asm.bind(miss, slow);
                asm.mov_rr(RDI, STATE);
                asm.mov_rr(RSI, RDX);
                asm.mov_ri64(RAX, slow_load as *const () as usize as u64);
                asm.call_r(RAX);
                asm.jmp_to(back);
                let here = asm.here();
                asm.bind(end, here);
            }),
            store: Stencil::build(|asm, holes| {
                frame_load(asm, holes, RDX, Hole::A);
                frame_load(asm, holes, RSI, Hole::B);
                let miss = lookup(asm);
                asm.store(RCX, 0, RSI);
                let end = asm.jmp();
                let slow = asm.here();
                asm.bind(miss, slow);
                asm.mov_rr(RAX, RDX);
                asm.mov_rr(RDX, RSI);
                asm.mov_rr(RSI, RAX);
                asm.mov_rr(RDI, STATE);
                asm.mov_ri64(RAX, slow_store as *const () as usize as u64);
                asm.call_r(RAX);
                let here = asm.here();
                asm.bind(end, here);
            }),
            exit: Stencil::build(|asm, holes| {
                frame_load(asm, holes, RAX, Hole::A);
                asm.mov_ri64(RCX, 0);
                holes.push((asm.here() - 8, Hole::Fall));
                asm.mov_ri64(RDX, 0);
                holes.push((asm.here() - 8, Hole::Taken));
                asm.test_rr(RAX, RAX);
                asm.cmovcc(CC_NE, RCX, RDX);
                asm.store(STATE, PC as i32 * 8, RCX);
                asm.alu_ri(ADD, RSP, 8);
                asm.pop(STATE);
                asm.pop(FRAME);
                asm.ret();
            }),
        }
    }

    /// Compile `block`, whose frame is at `frame`, into `code`.
    pub fn compile(&self, block: &Block, frame: u64, code: &mut Vec<u8>) {
        #[derive(Default, Clone, Copy)]
        struct Fill {
            a: u16,
            b: u16,
            d: u16,
            guest: u32,
            aux: u8,
        }
        let mut put = |stencil: &Stencil, fill: Fill| {
            let at = code.len();
            code.extend_from_slice(&stencil.bytes);
            for (position, hole) in &stencil.holes {
                let position = at + position;
                let word = |slot: u16| (i32::from(slot) * 8).to_le_bytes();
                match hole {
                    Hole::A => code[position..position + 4].copy_from_slice(&word(fill.a)),
                    Hole::B => code[position..position + 4].copy_from_slice(&word(fill.b)),
                    Hole::D => code[position..position + 4].copy_from_slice(&word(fill.d)),
                    Hole::Guest => code[position..position + 4]
                        .copy_from_slice(&(fill.guest as i32 * 8).to_le_bytes()),
                    Hole::Aux => code[position] = fill.aux,
                    Hole::Frame => code[position..position + 8].copy_from_slice(&frame.to_le_bytes()),
                    Hole::Taken => {
                        code[position..position + 8].copy_from_slice(&block.taken.to_le_bytes());
                    }
                    Hole::Fall => {
                        code[position..position + 8].copy_from_slice(&block.fall.to_le_bytes());
                    }
                }
            }
        };
        put(&self.prologue, Fill::default());
        for (slot, guest) in &block.inputs {
            put(&self.input, Fill { d: *slot, guest: *guest, ..Fill::default() });
        }
        for op in &block.ops {
            let stencil = match op.code {
                Code::Add => &self.add,
                Code::Sub => &self.sub,
                Code::And => &self.and,
                Code::Or => &self.or,
                Code::Xor => &self.xor,
                Code::Shr => &self.shr,
                Code::LessEq => &self.less_eq,
                Code::Less => &self.less,
                Code::IsZero => &self.is_zero,
                Code::Load8 => &self.load,
                Code::Store8 => &self.store,
            };
            put(stencil, Fill { a: op.a, b: op.b, d: op.dst, guest: 0, aux: op.aux });
        }
        for (guest, slot) in &block.outputs {
            put(&self.output, Fill { a: *slot, guest: *guest, ..Fill::default() });
        }
        put(&self.exit, Fill { a: block.cond, ..Fill::default() });
    }
}
