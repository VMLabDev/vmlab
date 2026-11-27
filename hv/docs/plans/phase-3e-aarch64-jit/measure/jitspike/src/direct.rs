//! Direct emission: one pass over the block, values in host registers.
//!
//! - R15 holds the state. RAX and RCX are scratch.
//! - Every other register is in the pool. A value gets a register at its
//!   definition and gives it back at its last use, and an operation that
//!   ends the life of its first operand writes its result over it.
//! - A constant is an immediate, never a register, where the instruction
//!   has the form.
//! - A guest register is read from the state at its first use. Its last
//!   value in the block is written to the state where it is computed, so it
//!   holds a host register only as long as an operation still reads it.
//!   The state is then exact at every later instruction of the block.
//!
//! The spike has no spill: a block that needs more than twelve live values
//! panics. A product needs one (DESIGN.md section 4).

use crate::asm::*;
use crate::{Block, Code, OFF_BUDGET, OFF_TLB, PC, TLB_ENTRIES, TLB_MASK, slow_load, slow_store};

const POOL: [Reg; 12] = [RBX, RBP, R12, R13, R14, RDX, RSI, RDI, R8, R9, R10, R11];
const CALLER_SAVED: [Reg; 7] = [RDX, RSI, RDI, R8, R9, R10, R11];
const CALLEE_SAVED: [Reg; 6] = [RBX, RBP, R12, R13, R14, R15];
const STATE: Reg = R15;

struct Slow {
    /// The end of the `jne` into the stub, and where the stub returns to.
    from: usize,
    back: usize,
    address: Reg,
    /// The value of a store, or the result of a load.
    value: Reg,
    store: bool,
}

struct Emit<'a, 'b> {
    asm: Asm<'a>,
    block: &'b Block,
    place: Vec<Option<Reg>>,
    last: Vec<usize>,
    constant: Vec<Option<u64>>,
    input: Vec<Option<u32>>,
    free: Vec<Reg>,
    slow: Vec<Slow>,
    /// The guest registers that a word is the last value of.
    output: Vec<Vec<u32>>,
    stored: Vec<bool>,
    used: Vec<bool>,
}

impl Emit<'_, '_> {
    fn alloc(&mut self) -> Reg {
        self.free.pop().expect("the spike has no spill")
    }

    /// The register of a word, read from the state or made from its
    /// constant when it has none yet.
    fn reg(&mut self, slot: u16) -> Reg {
        if let Some(reg) = self.place[slot as usize] {
            return reg;
        }
        let reg = self.alloc();
        if let Some(value) = self.constant[slot as usize] {
            self.asm.mov_ri(reg, value);
        } else {
            let guest = self.input[slot as usize].expect("a word that nothing defines");
            self.asm.load(reg, STATE, guest as i32 * 8);
        }
        self.place[slot as usize] = Some(reg);
        reg
    }

    /// Give back the register of `slot` when `index` is its last use.
    fn release(&mut self, slot: u16, index: usize) {
        if self.last[slot as usize] == index
            && let Some(reg) = self.place[slot as usize].take()
        {
            self.free.push(reg);
        }
    }

    /// The register of the result of the operation `index`, which holds the
    /// value of `a` already: the register of `a` when it dies here.
    fn result_over(&mut self, dst: u16, a: u16, index: usize) -> Reg {
        let from = self.reg(a);
        let reg = if self.last[a as usize] == index {
            self.place[a as usize] = None;
            from
        } else {
            let reg = self.alloc();
            self.asm.mov_rr(reg, from);
            reg
        };
        self.place[dst as usize] = Some(reg);
        reg
    }

    /// `op reg, b` with `b` as an immediate where it is a small constant.
    fn alu(&mut self, op: u8, reg: Reg, b: u16) {
        match self.constant[b as usize] {
            Some(value) if value as i64 == i64::from(value as i32) => {
                self.asm.alu_ri(op, reg, value as i32);
            }
            Some(value) => {
                self.asm.mov_ri(RAX, value);
                self.asm.alu_rr(op, reg, RAX);
            }
            None => {
                let b = self.reg(b);
                self.asm.alu_rr(op, reg, b);
            }
        }
    }

    /// The inlined TLB lookup for the address in `address`. RCX holds the
    /// host address after it. The `jne` of a miss is returned.
    fn lookup(&mut self, address: Reg) -> usize {
        self.asm.mov_rr(RAX, address);
        self.asm.shr_ri(RAX, 12);
        self.asm.alu_ri(AND, RAX, TLB_ENTRIES as i32 - 1);
        self.asm.shl_ri(RAX, 4);
        self.asm.alu_rm(ADD, RAX, STATE, OFF_TLB);
        self.asm.mov_rr(RCX, address);
        self.asm.alu_ri(AND, RCX, TLB_MASK);
        self.asm.alu_rm(CMP, RCX, RAX, 0);
        let miss = self.asm.jcc(CC_NE);
        self.asm.load(RCX, RAX, 8);
        self.asm.alu_rr(ADD, RCX, address);
        miss
    }

    fn op(&mut self, index: usize) {
        let op = self.block.ops[index];
        match op.code {
            Code::Add | Code::And | Code::Or | Code::Xor | Code::Sub => {
                let code = match op.code {
                    Code::Add => ADD,
                    Code::And => AND,
                    Code::Or => OR,
                    Code::Xor => XOR,
                    _ => SUB,
                };
                // A constant goes second where the operation commutes.
                let (a, b) = if op.code != Code::Sub
                    && self.constant[op.a as usize].is_some()
                    && self.place[op.a as usize].is_none()
                {
                    (op.b, op.a)
                } else {
                    (op.a, op.b)
                };
                let reg = self.result_over(op.dst, a, index);
                self.alu(code, reg, b);
                self.release(b, index);
            }
            Code::Shr => {
                let reg = self.result_over(op.dst, op.a, index);
                self.asm.shr_ri(reg, op.aux);
            }
            Code::IsZero => {
                let a = self.reg(op.a);
                self.asm.test_rr(a, a);
                self.release(op.a, index);
                let reg = self.alloc();
                self.asm.setcc(CC_E, reg);
                self.place[op.dst as usize] = Some(reg);
            }
            Code::LessEq | Code::Less => {
                let a = self.reg(op.a);
                self.alu(CMP, a, op.b);
                self.release(op.a, index);
                self.release(op.b, index);
                let reg = self.alloc();
                self.asm.setcc(if op.code == Code::Less { CC_B } else { CC_BE }, reg);
                self.place[op.dst as usize] = Some(reg);
            }
            Code::Load8 => {
                let address = self.reg(op.a);
                let from = self.lookup(address);
                // The result may not take the register of the address: the
                // stub reads the address after the fast path is laid out.
                let reg = self.alloc();
                self.asm.load(reg, RCX, 0);
                let back = self.asm.here();
                self.slow.push(Slow { from, back, address, value: reg, store: false });
                self.release(op.a, index);
                self.place[op.dst as usize] = Some(reg);
            }
            Code::Store8 => {
                let address = self.reg(op.a);
                let value = self.reg(op.b);
                let from = self.lookup(address);
                self.asm.store(RCX, 0, value);
                let back = self.asm.here();
                self.slow.push(Slow { from, back, address, value, store: true });
                self.release(op.a, index);
                self.release(op.b, index);
            }
        }
    }

    /// The miss of a load or a store: a call that keeps every register of
    /// the pool.
    fn stub(&mut self, slow: &Slow) {
        let here = self.asm.here();
        self.asm.bind(slow.from, here);
        for reg in CALLER_SAVED {
            self.asm.push(reg);
        }
        // Seven pushes: one more word keeps the stack aligned for the call.
        self.asm.push(RAX);
        if slow.store {
            self.asm.push(slow.value);
            self.asm.push(slow.address);
            self.asm.pop(RSI);
            self.asm.pop(RDX);
            self.asm.mov_ri64(RAX, slow_store as *const () as usize as u64);
        } else {
            self.asm.push(slow.address);
            self.asm.push(slow.address);
            self.asm.pop(RSI);
            self.asm.pop(RSI);
            self.asm.mov_ri64(RAX, slow_load as *const () as usize as u64);
        }
        self.asm.mov_rr(RDI, STATE);
        self.asm.call_r(RAX);
        self.asm.pop(RCX);
        for reg in CALLER_SAVED.iter().rev() {
            self.asm.pop(*reg);
        }
        if !slow.store {
            self.asm.mov_rr(slow.value, RAX);
        }
        self.asm.jmp_to(slow.back);
    }

    fn epilogue(&mut self) {
        self.asm.alu_ri(ADD, RSP, 8);
        for reg in CALLEE_SAVED.iter().rev() {
            self.asm.pop(*reg);
        }
        self.asm.ret();
    }
}

/// Compile `block` into `code`. `chained`: an exit to the block itself
/// jumps to its head while the budget of the state lasts.
pub fn compile(block: &Block, chained: bool, code: &mut Vec<u8>) {
    let never = usize::MAX;
    let mut last = vec![0usize; block.slots];
    for (index, op) in block.ops.iter().enumerate() {
        last[op.a as usize] = index;
        if !matches!(op.code, Code::Shr | Code::IsZero | Code::Load8) {
            last[op.b as usize] = index;
        }
    }
    let mut used = vec![false; block.slots];
    for op in &block.ops {
        used[op.a as usize] = true;
        used[op.b as usize] = true;
    }
    used[block.cond as usize] = true;
    let mut output = vec![Vec::new(); block.slots];
    for (guest, slot) in &block.outputs {
        output[*slot as usize].push(*guest);
    }
    last[block.cond as usize] = never;
    let mut constant = vec![None; block.slots];
    for (slot, value) in &block.consts {
        constant[*slot as usize] = Some(*value);
    }
    let mut input = vec![None; block.slots];
    for (slot, reg) in &block.inputs {
        input[*slot as usize] = Some(*reg);
    }
    let mut emit = Emit {
        asm: Asm { buf: code },
        block,
        place: vec![None; block.slots],
        last,
        constant,
        input,
        free: Vec::new(),
        slow: Vec::new(),
        output,
        stored: vec![false; block.slots],
        used,
    };
    for reg in CALLEE_SAVED {
        emit.asm.push(reg);
    }
    emit.asm.alu_ri(SUB, RSP, 8);
    emit.asm.mov_rr(STATE, RDI);
    let head = emit.asm.here();
    emit.free = POOL.iter().rev().copied().collect();
    for index in 0..block.ops.len() {
        emit.op(index);
        let op = block.ops[index];
        if op.code == Code::Store8 {
            continue;
        }
        let slot = op.dst as usize;
        if let Some(reg) = emit.place[slot]
            && !emit.output[slot].is_empty()
        {
            for guest in emit.output[slot].clone() {
                emit.asm.store(STATE, guest as i32 * 8, reg);
            }
            emit.stored[slot] = true;
            if !emit.used[slot] {
                emit.place[slot] = None;
                emit.free.push(reg);
            }
        }
    }
    // The exit: the registers not yet written, then the next PC.
    for (guest, slot) in &block.outputs {
        if emit.stored[*slot as usize] {
            continue;
        }
        match (emit.place[*slot as usize], emit.constant[*slot as usize]) {
            (Some(reg), _) => emit.asm.store(STATE, *guest as i32 * 8, reg),
            (None, Some(value)) => {
                emit.asm.mov_ri(RAX, value);
                emit.asm.store(STATE, *guest as i32 * 8, RAX);
            }
            (None, None) => {
                let reg = emit.reg(*slot);
                emit.asm.store(STATE, *guest as i32 * 8, reg);
            }
        }
    }
    let cond = emit.reg(block.cond);
    if chained && block.taken == block.pc {
        emit.asm.test_rr(cond, cond);
        let fall = emit.asm.jcc(CC_E);
        emit.asm.dec_m(STATE, OFF_BUDGET);
        let spent = emit.asm.jcc(CC_E);
        emit.asm.jmp_to(head);
        let here = emit.asm.here();
        emit.asm.bind(spent, here);
        emit.asm.mov_ri(RAX, block.taken);
        emit.asm.store(STATE, PC as i32 * 8, RAX);
        emit.epilogue();
        let here = emit.asm.here();
        emit.asm.bind(fall, here);
        emit.asm.mov_ri(RAX, block.fall);
        emit.asm.store(STATE, PC as i32 * 8, RAX);
        emit.epilogue();
    } else {
        emit.asm.mov_ri(RAX, block.fall);
        emit.asm.mov_ri(RCX, block.taken);
        emit.asm.test_rr(cond, cond);
        emit.asm.cmovcc(CC_NE, RAX, RCX);
        emit.asm.store(STATE, PC as i32 * 8, RAX);
        emit.epilogue();
    }
    let slow = std::mem::take(&mut emit.slow);
    for stub in &slow {
        emit.stub(stub);
    }
}
