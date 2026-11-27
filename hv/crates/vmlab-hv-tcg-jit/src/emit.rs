//! The emitter: one pass over the operations of a block, values in host
//! registers (DESIGN.md H3), guards and exits with their stubs (H9), and
//! every effect through the thunk.
//!
//! The frame of the block is the home of every word. A word lives in a
//! host register from its definition, or from its first use when it is
//! used again. A register goes back to the pool at the last use of its
//! word. When the pool is empty, the word whose next use is furthest goes
//! back to its home in the frame: its word is its spill slot. A guest
//! register is read from the state at its first use, and written to the
//! state at the exit of the block.
//!
//! The thunk runs what the emitter has no inline form for: the accesses of
//! 16 bytes and the exclusive ones, the generic operations, the helper
//! calls, a guard that fails, and a load or a store of 1 to 8 bytes that
//! misses the fast TLB (H7) or that its block may not inline.
//!
//! A load or a store of 1 to 8 bytes looks up the fast TLB inline (H7, the
//! layout in `tlb`): the index from the page bits of the address, the tag
//! compared with the address masked to its page and to the bits below its
//! size, the addend, and the access of the host. A store then reads the
//! watch word of its page and asks the thunk to tell the memory when it is
//! odd (card S3). A miss is a stub after the block (H7c) that puts the words
//! of the access and of its leave into the frame and runs the access in the
//! thunk, which can leave the block through the exit of its instruction. Before it runs,
//! the words that it reads are in the frame. After it, the words that it
//! wrote are read from the frame. The words that a leave of the block names
//! are in the frame before every effect that can leave through it, so the
//! frame is what the interpreter would have at that point.
//!
//! Registers: R13 holds the context of the entry, R14 the frame, R15 the
//! register slots of the state. RAX, RCX and RDX are scratch. The others,
//! but RSP, are the pool.
//!
//! Sources read: the Intel 64 and IA-32 Architectures Software Developer's
//! Manual, volume 2, for what each instruction computes, and the System V
//! AMD64 ABI for the call of the thunk. The operations are those of
//! `vmlab_hv_tcg_ir::block`.

use crate::asm::{
    Alu, Asm, Cond, R8, R9, R10, R11, R12, R13, R14, R15, RAX, RBP, RBX, RCX, RDI, RDX, RSI, RSP,
    Reg, Rm, Shift,
};
use crate::tlb::{ADDEND, ENTRIES, ENTRY_BITS, TAG, WATCH};
use crate::{
    BUDGET, CONTEXT_BUDGET, CONTEXT_ENV, CONTEXT_READS, CONTEXT_THUNK, CONTEXT_WRITES, Features,
    LOGGED, MISS, WATCHED,
};
use vmlab_hv_tcg_ir::block::{Code, FIRST_EFFECT, Generic, Op, Place, Slot};

/// What an effect that the caller describes reads and writes: an access of
/// `Code::Access` or a call of `Code::Call`, by the index in its `imm`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effect {
    pub reads: Vec<Slot>,
    pub writes: Vec<Slot>,
    /// The leave of the block that the effect can leave through.
    pub leave: Option<u32>,
}

/// How the loads and stores of 1 to 8 bytes of a block reach memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Memory {
    /// Every one through the thunk.
    #[default]
    Thunk,
    /// Inline over the fast TLB, but those whose `aux` has `THUNK_ACCESS`.
    Inline,
    /// As `Inline`, and every hit asks the thunk to keep the access
    /// (`LOGGED`): for an owner that keeps the accesses of its CPU, such as
    /// a lockstep against another emulator. The lookup is the same code.
    Logged,
}

/// The bit of `Op::aux` of a load or a store that goes through the thunk
/// whatever the block's `Memory` says. The owner sets it where its fast
/// TLB does not answer for the access (H7a: an unprivileged access of
/// aarch64).
pub const THUNK_ACCESS: u8 = 0x80;

/// A block as the emitter sees it.
pub struct Shape<'a> {
    pub ops: &'a [Op],
    pub generics: &'a [Generic],
    /// The frame with the constants in place.
    pub frame: &'a [u64],
    /// What each access of `Code::Access` touches, by its index.
    pub accesses: &'a [Effect],
    /// What each helper call of `Code::Call` touches, by its index.
    pub calls: &'a [Effect],
    /// The words that each leave of the block reads.
    pub leaves: &'a [Vec<Slot>],
    /// Words of the frame, register slot: read from the state.
    pub inputs: &'a [(Place, u32)],
    /// Register slot, words of the frame: written to the state at the end.
    pub outputs: &'a [(u32, Place)],
    pub instructions: u32,
    pub memory: Memory,
}

/// Why the emitter made no code for a block. The block runs in the
/// interpreter (H2b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// An operation that the emitter has no form for on this host.
    Form(Code),
    /// A block that does not fit the region, or a frame or a register slot
    /// past the reach of a displacement.
    Size,
    /// A word or an index that the block does not hold.
    Malformed,
}

const POOL: [Reg; 9] = [RBX, RBP, R12, RSI, RDI, R8, R9, R10, R11];
/// The registers of the pool that a call does not keep.
const CALLER_SAVED: [Reg; 6] = [RSI, RDI, R8, R9, R10, R11];

/// Where the value of a word is when it is in no register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Home {
    /// Its word of the frame.
    Frame,
    /// A register slot of the state, at this offset in bytes: an input
    /// that is not read yet.
    Slot(i32),
    /// A constant of the frame. Its word holds it, and nothing writes it.
    Const(u64),
}

#[derive(Clone, Copy, Debug)]
struct Word {
    home: Home,
    reg: Option<Reg>,
    /// The register holds a value that the frame does not.
    dirty: bool,
}

/// An operand as an instruction reads it.
#[derive(Clone, Copy, Debug)]
enum Val {
    Reg(Reg),
    Mem(Rm),
    Imm(u64),
}

/// The position of the exit of the block in the order of uses.
fn uses_of(op: &Op, generics: &[Generic], shape: &Shape) -> Option<(Vec<Slot>, Vec<Slot>)> {
    let place = |place: Place| -> Vec<Slot> {
        if place.wide {
            vec![place.slot, place.slot + 1]
        } else {
            vec![place.slot]
        }
    };
    let leave = |index: u64| -> Option<Vec<Slot>> { shape.leaves.get(index as usize).cloned() };
    Some(match op.code {
        Code::Add
        | Code::Sub
        | Code::Mul
        | Code::And
        | Code::Or
        | Code::Xor
        | Code::ShiftLeft
        | Code::ShiftRight
        | Code::ArithmeticShiftRight
        | Code::Equal
        | Code::NotEqual
        | Code::Less
        | Code::LessEqual
        | Code::Concat => (vec![op.a, op.b], vec![op.dst]),
        Code::IsZero
        | Code::BitNot
        | Code::Negate
        | Code::SignExtend
        | Code::LeadingZeros
        | Code::TrailingZeros
        | Code::ReverseBits
        | Code::ReverseBytes
        | Code::PopCount
        | Code::Slice => (vec![op.a], vec![op.dst]),
        Code::Select => (vec![op.a, op.b, Slot::try_from(op.imm).ok()?], vec![op.dst]),
        Code::Widen => (vec![op.a], vec![op.dst, op.dst.checked_add(1)?]),
        Code::Generic => {
            let generic = generics.get(op.imm as usize)?;
            let reads = generic.args().into_iter().flat_map(place).collect();
            (reads, place(generic.dst()))
        }
        Code::Guard => {
            let mut reads = vec![op.a];
            reads.extend(leave(op.imm)?);
            (reads, Vec::new())
        }
        Code::Load1 | Code::Load2 | Code::Load4 | Code::Load8 => {
            let mut reads = vec![op.a];
            reads.extend(leave(op.imm)?);
            (reads, vec![op.dst])
        }
        Code::Store1 | Code::Store2 | Code::Store4 | Code::Store8 => {
            let mut reads = vec![op.a, op.b];
            reads.extend(leave(op.imm)?);
            (reads, Vec::new())
        }
        Code::Access | Code::Call => {
            let effect = if op.code == Code::Access {
                shape.accesses.get(op.imm as usize)?
            } else {
                shape.calls.get(op.imm as usize)?
            };
            let mut reads = effect.reads.clone();
            if let Some(index) = effect.leave {
                reads.extend(leave(index.into())?);
            }
            (reads, effect.writes.clone())
        }
    })
}

struct Emitter<'a> {
    asm: Asm,
    words: Vec<Word>,
    owner: [Option<Slot>; 16],
    /// The positions at which each word is read, in order.
    uses: Vec<Vec<u32>>,
    cursor: Vec<usize>,
    pinned: u16,
    exit: u64,
    features: Features,
    shape: &'a Shape<'a>,
}

impl Emitter<'_> {
    fn frame(slot: Slot) -> Rm {
        Rm::Mem(R14, i32::from(slot) * 8)
    }

    /// The next position after `after` at which `word` is read.
    fn next_use(&mut self, word: Slot, after: u32) -> Option<u32> {
        let w = usize::from(word);
        let uses = &self.uses[w];
        let cursor = &mut self.cursor[w];
        while *cursor < uses.len() && uses[*cursor] <= after {
            *cursor += 1;
        }
        uses.get(*cursor).copied()
    }

    fn unbind(&mut self, word: Slot) {
        let w = &mut self.words[usize::from(word)];
        if let Some(reg) = w.reg.take() {
            self.owner[usize::from(reg.0)] = None;
            self.pinned &= !(1 << reg.0);
        }
        w.dirty = false;
    }

    /// A register of the pool for a value used after `at`: a free one, or
    /// the one whose word is used last, whose value goes home first.
    fn allocate(&mut self, at: u32) -> Reg {
        if let Some(reg) = POOL
            .iter()
            .find(|reg| self.owner[usize::from(reg.0)].is_none())
        {
            return *reg;
        }
        let mut best: Option<(Reg, u32)> = None;
        for reg in POOL {
            if self.pinned & (1 << reg.0) != 0 {
                continue;
            }
            let Some(word) = self.owner[usize::from(reg.0)] else {
                continue;
            };
            let next = self.next_use(word, at).unwrap_or(u32::MAX);
            if best.is_none_or(|(_, far)| next > far) {
                best = Some((reg, next));
            }
        }
        let (reg, _) = best.expect("at most three registers of the pool are pinned");
        let word = self.owner[usize::from(reg.0)].expect("an owned register");
        self.spill(word);
        reg
    }

    /// Put the value of `word` back to its home and free its register.
    fn spill(&mut self, word: Slot) {
        let w = self.words[usize::from(word)];
        if let Some(reg) = w.reg
            && w.dirty
        {
            self.asm.store(Self::frame(word), reg);
            self.words[usize::from(word)].home = Home::Frame;
        }
        self.unbind(word);
    }

    fn bind(&mut self, word: Slot, reg: Reg, dirty: bool) {
        self.owner[usize::from(reg.0)] = Some(word);
        let w = &mut self.words[usize::from(word)];
        w.reg = Some(reg);
        w.dirty = dirty;
        if dirty {
            w.home = Home::Frame;
        }
    }

    /// The memory that holds the value of a word that is in no register.
    fn memory(&self, word: Slot) -> Rm {
        match self.words[usize::from(word)].home {
            Home::Slot(offset) => Rm::Mem(R15, offset),
            Home::Frame | Home::Const(_) => Self::frame(word),
        }
    }

    /// The operand `word` of the operation at `at`, pinned to its register
    /// until the operation ends. A word that is read again later goes into
    /// a register.
    fn value(&mut self, word: Slot, at: u32) -> Val {
        let w = self.words[usize::from(word)];
        if let Home::Const(value) = w.home {
            return Val::Imm(value);
        }
        if let Some(reg) = w.reg {
            self.pinned |= 1 << reg.0;
            return Val::Reg(reg);
        }
        let memory = self.memory(word);
        if self.next_use(word, at).is_none() {
            return Val::Mem(memory);
        }
        let reg = self.allocate(at);
        self.asm.mov(reg, memory);
        self.bind(word, reg, false);
        self.pinned |= 1 << reg.0;
        Val::Reg(reg)
    }

    /// The value as a register or memory: a constant goes into `scratch`.
    fn rm(&mut self, value: Val, scratch: Reg) -> Rm {
        match value {
            Val::Reg(reg) => Rm::Reg(reg),
            Val::Mem(memory) => memory,
            Val::Imm(value) => {
                self.asm.mov_imm(scratch, value);
                Rm::Reg(scratch)
            }
        }
    }

    /// The value in a register: `scratch` unless it is in one.
    fn reg(&mut self, value: Val, scratch: Reg) -> Reg {
        match value {
            Val::Reg(reg) => reg,
            other => {
                let rm = self.rm(other, scratch);
                self.asm.mov(scratch, rm);
                scratch
            }
        }
    }

    fn load(&mut self, dst: Reg, value: Val) {
        match value {
            Val::Reg(reg) => self.asm.mov(dst, Rm::Reg(reg)),
            Val::Mem(memory) => self.asm.mov(dst, memory),
            Val::Imm(value) => self.asm.mov_imm(dst, value),
        }
    }

    /// `<alu> rax, value`.
    fn alu(&mut self, alu: Alu, value: Val) {
        match value {
            Val::Imm(value) if i32::try_from(value as i64).is_ok() => {
                self.asm.alu_imm(alu, Rm::Reg(RAX), value as i32);
            }
            other => {
                let rm = self.rm(other, RDX);
                self.asm.alu(alu, RAX, rm);
            }
        }
    }

    /// `rax &= mask`, in its shortest form.
    fn mask(&mut self, mask: u64) {
        if mask == u64::MAX {
        } else if mask == u64::from(u32::MAX) {
            self.asm.mov32(RAX, RAX);
        } else if i32::try_from(mask as i64).is_ok() {
            self.asm.alu_imm(Alu::And, Rm::Reg(RAX), mask as i32);
        } else {
            self.asm.mov_imm(RDX, mask);
            self.asm.alu(Alu::And, RAX, Rm::Reg(RDX));
        }
    }

    /// The result of the operation at `at` is in RAX: free the registers of
    /// the operands that are not read again, and give `dst` a register when
    /// it is read later.
    fn define(&mut self, dst: Slot, reads: &[Slot], at: u32) {
        self.release(reads, at);
        self.unbind(dst);
        self.words[usize::from(dst)].home = Home::Frame;
        if self.next_use(dst, at).is_none() {
            return;
        }
        let reg = self.allocate(at);
        self.asm.mov(reg, Rm::Reg(RAX));
        self.bind(dst, reg, true);
    }

    fn release(&mut self, reads: &[Slot], at: u32) {
        self.pinned = 0;
        for word in reads {
            if self.next_use(*word, at).is_none() {
                self.unbind(*word);
            }
        }
    }

    /// The instructions that make the frame hold the value of `word`, from
    /// where `w` says that it is. They change no register but RDX.
    fn home_code(asm: &mut Asm, word: Slot, w: Word) {
        match (w.reg, w.home) {
            (_, Home::Const(_)) => {}
            (Some(reg), Home::Frame) if w.dirty => asm.store(Self::frame(word), reg),
            (Some(reg), Home::Slot(_)) => asm.store(Self::frame(word), reg),
            (Some(_), Home::Frame) | (None, Home::Frame) => {}
            (None, Home::Slot(offset)) => {
                asm.mov(RDX, Rm::Mem(R15, offset));
                asm.store(Self::frame(word), RDX);
            }
        }
    }

    /// Make the frame hold the value of `word` from now on.
    fn materialize(&mut self, word: Slot) {
        let w = self.words[usize::from(word)];
        Self::home_code(&mut self.asm, word, w);
        let w = &mut self.words[usize::from(word)];
        if !matches!(w.home, Home::Const(_)) {
            w.home = Home::Frame;
            w.dirty = false;
        }
    }

    /// The registers of the pool that hold a word and that a call does not
    /// keep.
    fn live_caller_saved(&self) -> Vec<Reg> {
        CALLER_SAVED
            .iter()
            .copied()
            .filter(|reg| self.owner[usize::from(reg.0)].is_some())
            .collect()
    }

    /// Call the thunk for the operation `index`, keeping `saved`. RAX holds
    /// what it returns.
    fn call_thunk(asm: &mut Asm, index: u64, saved: &[Reg]) {
        for reg in saved {
            asm.push(*reg);
        }
        // The body runs with RSP at a multiple of 16.
        let pad = saved.len() % 2 == 1;
        if pad {
            asm.alu_imm(Alu::Sub, Rm::Reg(RSP), 8);
        }
        asm.mov(RDI, Rm::Mem(R13, CONTEXT_ENV));
        asm.mov_imm(RSI, index);
        asm.call_indirect(Rm::Mem(R13, CONTEXT_THUNK));
        if pad {
            asm.alu_imm(Alu::Add, Rm::Reg(RSP), 8);
        }
        for reg in saved.iter().rev() {
            asm.pop(*reg);
        }
    }

    /// An operation that runs through the thunk.
    fn effect(&mut self, index: usize, reads: &[Slot], writes: &[Slot]) {
        let at = index as u32;
        for word in reads {
            self.materialize(*word);
        }
        self.release(reads, at);
        for word in writes {
            self.unbind(*word);
            self.words[usize::from(*word)].home = Home::Frame;
        }
        let saved = self.live_caller_saved();
        Self::call_thunk(&mut self.asm, index as u64, &saved);
        self.asm.test(RAX, RAX);
        self.asm.jcc_absolute(Cond::Ne, self.exit);
    }

    /// A guard: on the inline path nothing happens. When its condition does
    /// not hold, a stub puts the words of its leave into the frame and asks
    /// the thunk, which leaves the block or lets it go on.
    fn guard(&mut self, index: usize, op: &Op, reads: &[Slot]) {
        let at = index as u32;
        let condition = self.value(op.a, at);
        let stub = self.asm.label();
        let back = self.asm.label();
        // The guard fails where the condition is zero and it expects a
        // value that is not, or the other way.
        let fail = if op.aux != 0 { Cond::E } else { Cond::Ne };
        match condition {
            Val::Imm(value) => {
                if (value != 0) != (op.aux != 0) {
                    self.asm.jmp(stub);
                }
            }
            Val::Reg(reg) => {
                self.asm.test(reg, reg);
                self.asm.jcc(fail, stub);
            }
            Val::Mem(memory) => {
                self.asm.alu_imm(Alu::Cmp, memory, 0);
                self.asm.jcc(fail, stub);
            }
        }
        self.asm.bind(back);
        // The stub, with the places of the words as they are here.
        self.asm.cold(true);
        self.asm.bind(stub);
        for word in reads {
            let w = self.words[usize::from(*word)];
            Self::home_code(&mut self.asm, *word, w);
        }
        let saved = self.live_caller_saved();
        Self::call_thunk(&mut self.asm, index as u64, &saved);
        self.asm.test(RAX, RAX);
        self.asm.jcc_absolute(Cond::Ne, self.exit);
        self.asm.jmp(back);
        self.asm.cold(false);
        self.release(reads, at);
    }

    /// The words that a stub puts into the frame: those of `reads`, from
    /// where they are now. The code changes no register but RDX.
    fn home_all(&mut self, reads: &[Slot]) {
        for word in reads {
            let w = self.words[usize::from(*word)];
            Self::home_code(&mut self.asm, *word, w);
        }
    }

    /// The thunk for `request` of the operation `index`, keeping the
    /// registers of the pool. With `leaves`, a value other than 0 leaves
    /// the block.
    fn thunk(&mut self, index: usize, request: u64, leaves: bool) {
        let saved = self.live_caller_saved();
        Self::call_thunk(&mut self.asm, index as u64 | request, &saved);
        if leaves {
            self.asm.test(RAX, RAX);
            self.asm.jcc_absolute(Cond::Ne, self.exit);
        }
    }

    /// A load or a store of `bytes` bytes over the fast TLB (H7). The
    /// address is the word `a`, the value of a store the word `b`, the
    /// result of a load the word `dst`, and the leave `imm`.
    fn access(&mut self, index: usize, op: &Op, reads: &[Slot], bytes: u8, store: bool) {
        let at = index as u32;
        let logged = self.shape.memory == Memory::Logged;
        let address = self.value(op.a, at);
        let value = store.then(|| self.value(op.b, at));
        let miss = self.asm.label();
        let join = self.asm.label();
        // RCX: the entry of the page in the table of the block.
        self.load(RAX, address);
        self.asm.mov(RCX, Rm::Reg(RAX));
        self.asm.shift_imm(Shift::Shr, RCX, (12 - ENTRY_BITS) as u8);
        self.asm
            .alu_imm(Alu::And, Rm::Reg(RCX), ((ENTRIES - 1) << ENTRY_BITS) as i32);
        let table = if store { CONTEXT_WRITES } else { CONTEXT_READS };
        self.asm.alu(Alu::Add, RCX, Rm::Mem(R13, table));
        // The address in its page and below its size, against the tag.
        self.asm.mov(RDX, Rm::Reg(RAX));
        self.asm
            .alu_imm(Alu::And, Rm::Reg(RDX), !0xfff | (i32::from(bytes) - 1));
        self.asm.alu(Alu::Cmp, RDX, Rm::Mem(RCX, TAG));
        self.asm.jcc(Cond::Ne, miss);
        self.asm.alu(Alu::Add, RAX, Rm::Mem(RCX, ADDEND));
        if let Some(value) = value {
            let source = match value {
                Val::Reg(reg) => reg,
                Val::Mem(memory) => {
                    self.asm.mov(RDX, memory);
                    RDX
                }
                Val::Imm(value) => {
                    self.asm.mov_imm(RDX, value);
                    RDX
                }
            };
            self.asm.store_sized(Rm::Mem(RAX, 0), source, bytes);
            // The watch word of the page: odd while a CPU holds code of it.
            let watched = self.asm.label();
            let back = self.asm.label();
            self.asm.mov(RCX, Rm::Mem(RCX, WATCH));
            self.asm.test_byte_imm(Rm::Mem(RCX, 0), 1);
            self.asm.jcc(Cond::Ne, watched);
            self.asm.bind(back);
            if logged {
                self.home_all(&[op.a, op.b]);
                self.thunk(index, LOGGED, false);
            }
            self.asm.cold(true);
            self.asm.bind(watched);
            self.home_all(&[op.a]);
            self.thunk(index, WATCHED, false);
            self.asm.jmp(back);
            self.asm.cold(false);
        } else {
            self.asm.load_sized(RAX, Rm::Mem(RAX, 0), bytes);
            if logged {
                self.asm.store(Self::frame(op.dst), RAX);
                self.home_all(&[op.a]);
                self.thunk(index, LOGGED, false);
                self.asm.mov(RAX, Self::frame(op.dst));
            }
        }
        self.asm.bind(join);
        // The miss, with the places of the words as they are here. The
        // thunk writes the result of a load into its word of the frame.
        self.asm.cold(true);
        self.asm.bind(miss);
        self.home_all(reads);
        self.thunk(index, MISS, true);
        if !store {
            self.asm.mov(RAX, Self::frame(op.dst));
        }
        self.asm.jmp(join);
        self.asm.cold(false);
        if store {
            self.release(reads, at);
        } else {
            self.define(op.dst, reads, at);
        }
    }

    /// One pure operation of one word, inline.
    fn pure(&mut self, index: usize, op: &Op, reads: &[Slot]) -> Result<(), Refused> {
        let at = index as u32;
        let width = op.aux;
        let low = |width: u8| u64::MAX >> (64 - u32::from(width));
        if self.next_use(op.dst, at).is_none() && op.code != Code::Widen {
            // Nothing reads the result.
            self.release(reads, at);
            self.unbind(op.dst);
            self.words[usize::from(op.dst)].home = Home::Frame;
            return Ok(());
        }
        match op.code {
            Code::Add | Code::Sub | Code::And | Code::Or | Code::Xor => {
                let a = self.value(op.a, at);
                let b = self.value(op.b, at);
                self.load(RAX, a);
                let alu = match op.code {
                    Code::Add => Alu::Add,
                    Code::Sub => Alu::Sub,
                    Code::And => Alu::And,
                    Code::Or => Alu::Or,
                    _ => Alu::Xor,
                };
                self.alu(alu, b);
                self.mask(op.imm);
            }
            Code::Mul => {
                let a = self.value(op.a, at);
                let b = self.value(op.b, at);
                self.load(RAX, a);
                let rm = self.rm(b, RDX);
                self.asm.imul(RAX, rm);
                self.mask(op.imm);
            }
            Code::ShiftLeft | Code::ShiftRight => {
                let shift = if op.code == Code::ShiftLeft {
                    Shift::Shl
                } else {
                    Shift::Shr
                };
                let a = self.value(op.a, at);
                let b = self.value(op.b, at);
                match b {
                    Val::Imm(count) if count < 64 => {
                        self.load(RAX, a);
                        self.asm.shift_imm(shift, RAX, count as u8);
                    }
                    Val::Imm(_) => self.asm.xor32(RAX, RAX),
                    _ => {
                        self.load(RAX, a);
                        self.load(RCX, b);
                        self.asm.shift_cl(shift, RAX);
                        // A count of 64 or more gives zero.
                        self.asm.xor32(RDX, RDX);
                        self.asm.alu_imm(Alu::Cmp, Rm::Reg(RCX), 64);
                        self.asm.cmov(Cond::Ae, RAX, Rm::Reg(RDX));
                    }
                }
                self.mask(op.imm);
            }
            Code::ArithmeticShiftRight => {
                if width == 0 || width > 64 {
                    return Err(Refused::Malformed);
                }
                let a = self.value(op.a, at);
                let b = self.value(op.b, at);
                self.load(RAX, a);
                self.asm.shift_imm(Shift::Shl, RAX, 64 - width);
                self.asm.shift_imm(Shift::Sar, RAX, 64 - width);
                match b {
                    Val::Imm(count) => {
                        let count = count.min(u64::from(width) - 1) as u8;
                        self.asm.shift_imm(Shift::Sar, RAX, count);
                    }
                    _ => {
                        // The count is at most the width less one.
                        self.load(RCX, b);
                        self.asm.mov_imm(RDX, u64::from(width) - 1);
                        self.asm.alu(Alu::Cmp, RCX, Rm::Reg(RDX));
                        self.asm.cmov(Cond::A, RCX, Rm::Reg(RDX));
                        self.asm.shift_cl(Shift::Sar, RAX);
                    }
                }
                self.mask(op.imm);
            }
            Code::Equal | Code::NotEqual | Code::Less | Code::LessEqual | Code::IsZero => {
                let a = self.value(op.a, at);
                let b = if op.code == Code::IsZero {
                    Val::Imm(0)
                } else {
                    self.value(op.b, at)
                };
                let left = self.reg(a, RCX);
                self.asm.xor32(RAX, RAX);
                match b {
                    Val::Imm(0) => self.asm.test(left, left),
                    Val::Imm(value) if i32::try_from(value as i64).is_ok() => {
                        self.asm.alu_imm(Alu::Cmp, Rm::Reg(left), value as i32);
                    }
                    other => {
                        let rm = self.rm(other, RDX);
                        self.asm.alu(Alu::Cmp, left, rm);
                    }
                }
                let cond = match op.code {
                    Code::Equal | Code::IsZero => Cond::E,
                    Code::NotEqual => Cond::Ne,
                    Code::Less => Cond::B,
                    _ => Cond::Be,
                };
                self.asm.setcc_al(cond);
            }
            Code::BitNot | Code::Negate => {
                let a = self.value(op.a, at);
                self.load(RAX, a);
                if op.code == Code::BitNot {
                    self.asm.not(RAX);
                } else {
                    self.asm.neg(RAX);
                }
                self.mask(op.imm);
            }
            Code::SignExtend => {
                if width == 0 || width > 64 {
                    return Err(Refused::Malformed);
                }
                let a = self.value(op.a, at);
                self.load(RAX, a);
                self.asm.shift_imm(Shift::Shl, RAX, 64 - width);
                self.asm.shift_imm(Shift::Sar, RAX, 64 - width);
                self.mask(op.imm);
            }
            Code::LeadingZeros | Code::TrailingZeros | Code::PopCount | Code::ReverseBytes => {
                if width == 0 || width > 64 {
                    return Err(Refused::Malformed);
                }
                let available = match op.code {
                    Code::LeadingZeros => self.features.lzcnt,
                    Code::TrailingZeros => self.features.bmi1,
                    Code::PopCount => self.features.popcnt,
                    _ => true,
                };
                if !available {
                    return Err(Refused::Form(op.code));
                }
                let a = self.value(op.a, at);
                self.load(RAX, a);
                self.mask(low(width));
                match op.code {
                    Code::LeadingZeros => {
                        self.asm.lzcnt(RAX, RAX);
                        if width < 64 {
                            self.asm
                                .alu_imm(Alu::Sub, Rm::Reg(RAX), i32::from(64 - width));
                        }
                    }
                    Code::TrailingZeros => {
                        // TZCNT gives 64 for zero: at most the width.
                        self.asm.tzcnt(RAX, RAX);
                        self.asm.mov_imm(RDX, u64::from(width));
                        self.asm.alu(Alu::Cmp, RAX, Rm::Reg(RDX));
                        self.asm.cmov(Cond::A, RAX, Rm::Reg(RDX));
                    }
                    Code::PopCount => self.asm.popcnt(RAX, RAX),
                    _ => {
                        self.asm.bswap(RAX);
                        self.asm.shift_imm(Shift::Shr, RAX, 64 - width);
                    }
                }
                self.mask(op.imm);
            }
            Code::Slice => {
                if width >= 64 {
                    return Err(Refused::Malformed);
                }
                let a = self.value(op.a, at);
                self.load(RAX, a);
                self.asm.shift_imm(Shift::Shr, RAX, width);
                self.mask(op.imm);
            }
            Code::Concat => {
                if width >= 64 {
                    return Err(Refused::Malformed);
                }
                let a = self.value(op.a, at);
                let b = self.value(op.b, at);
                self.load(RAX, a);
                self.asm.shift_imm(Shift::Shl, RAX, width);
                self.alu(Alu::Or, b);
                self.mask(op.imm);
            }
            Code::Select => {
                let condition = Slot::try_from(op.imm).map_err(|_| Refused::Malformed)?;
                let a = self.value(op.a, at);
                let b = self.value(op.b, at);
                let c = self.value(condition, at);
                match c {
                    Val::Imm(value) => self.load(RAX, if value != 0 { a } else { b }),
                    _ => {
                        // Every move comes before the test: a move of zero
                        // is an `xor`, which writes the flags.
                        let c = self.reg(c, RCX);
                        let rm = self.rm(a, RDX);
                        self.load(RAX, b);
                        self.asm.test(c, c);
                        self.asm.cmov(Cond::Ne, RAX, rm);
                    }
                }
            }
            Code::Widen => {
                let high = op.dst + 1;
                if self.next_use(op.dst, at).is_some() {
                    let a = self.value(op.a, at);
                    self.load(RAX, a);
                    self.define(op.dst, reads, at);
                } else {
                    self.release(reads, at);
                    self.unbind(op.dst);
                    self.words[usize::from(op.dst)].home = Home::Frame;
                }
                self.unbind(high);
                self.words[usize::from(high)].home = Home::Frame;
                if self.next_use(high, at).is_some() {
                    self.asm.store_imm(Self::frame(high), 0);
                }
                return Ok(());
            }
            Code::ReverseBits => return Err(Refused::Form(op.code)),
            _ => return Err(Refused::Malformed),
        }
        self.define(op.dst, reads, at);
        Ok(())
    }

    /// Write `value` to `target`, through RAX where it must.
    fn put(&mut self, target: Rm, value: Val) {
        match value {
            Val::Reg(reg) => self.asm.store(target, reg),
            Val::Imm(value) if i32::try_from(value as i64).is_ok() => {
                self.asm.store_imm(target, value as i32);
            }
            other => {
                self.load(RAX, other);
                self.asm.store(target, RAX);
            }
        }
    }

    /// The value of `word` as it is now, without a new register.
    fn current(&self, word: Slot) -> Val {
        let w = self.words[usize::from(word)];
        match (w.reg, w.home) {
            (_, Home::Const(value)) => Val::Imm(value),
            (Some(reg), _) => Val::Reg(reg),
            (None, _) => Val::Mem(self.memory(word)),
        }
    }

    /// The end of the block: the outputs to the state, then the exit.
    fn outputs(&mut self) {
        let outputs = self.shape.outputs;
        let targets: Vec<(i32, i32)> = outputs
            .iter()
            .map(|(register, _)| {
                let start = *register as i32 * 16;
                (start, start + 16)
            })
            .collect();
        // An input that is still in its slot is read before an output
        // writes that slot, unless the output writes it to itself.
        for (register, place) in outputs {
            for (half, word) in [(0, place.slot), (8, place.slot + 1)] {
                if half == 8 && !place.wide {
                    continue;
                }
                let w = self.words[usize::from(word)];
                if let (None, Home::Slot(offset)) = (w.reg, w.home)
                    && offset != *register as i32 * 16 + half
                    && targets
                        .iter()
                        .any(|(start, end)| (*start..*end).contains(&offset))
                {
                    self.materialize(word);
                }
            }
        }
        for (register, place) in outputs {
            let base = *register as i32 * 16;
            for (half, word) in [(0, place.slot), (8, place.slot + 1)] {
                let target = Rm::Mem(R15, base + half);
                if half == 8 && !place.wide {
                    self.asm.store_imm(target, 0);
                    continue;
                }
                let w = self.words[usize::from(word)];
                if w.reg.is_none() && w.home == Home::Slot(base + half) {
                    continue;
                }
                let value = self.current(word);
                self.put(target, value);
            }
        }
        self.asm.xor32(RAX, RAX);
        self.asm.jmp_absolute(self.exit);
    }
}

/// The size of a load or a store that the block makes inline, and whether
/// it is a store.
fn inline_access(op: &Op, memory: Memory) -> Option<(u8, bool)> {
    if memory == Memory::Thunk || op.aux & THUNK_ACCESS != 0 {
        return None;
    }
    let (bytes, store) = match op.code {
        Code::Load1 => (1, false),
        Code::Load2 => (2, false),
        Code::Load4 => (4, false),
        Code::Load8 => (8, false),
        Code::Store1 => (1, true),
        Code::Store2 => (2, true),
        Code::Store4 => (4, true),
        Code::Store8 => (8, true),
        _ => return None,
    };
    // The size is in `aux` too, below the bit of the thunk.
    (op.aux == bytes).then_some((bytes, store))
}

/// Emit the code of `shape`. `exit` is the address of the common exit of
/// the trampoline.
pub fn emit(shape: &Shape, exit: u64, features: Features) -> Result<Asm, Refused> {
    let words = shape.frame.len();
    if words > i32::MAX as usize / 8 {
        return Err(Refused::Size);
    }
    let in_frame = |slot: Slot| usize::from(slot) < words;
    let end = u32::try_from(shape.ops.len()).map_err(|_| Refused::Size)?;
    // The uses of every word, and the words that something writes.
    let mut uses: Vec<Vec<u32>> = vec![Vec::new(); words];
    let mut written = vec![false; words];
    let mut effects = Vec::with_capacity(shape.ops.len());
    for (index, op) in shape.ops.iter().enumerate() {
        let (reads, writes) = uses_of(op, shape.generics, shape).ok_or(Refused::Malformed)?;
        if !reads.iter().chain(&writes).all(|slot| in_frame(*slot)) {
            return Err(Refused::Malformed);
        }
        for slot in &reads {
            let list = &mut uses[usize::from(*slot)];
            if list.last() != Some(&(index as u32)) {
                list.push(index as u32);
            }
        }
        for slot in &writes {
            written[usize::from(*slot)] = true;
        }
        effects.push((reads, writes));
    }
    let mut homes = vec![Home::Frame; words];
    for (place, register) in shape.inputs {
        let offset = i32::try_from(*register as usize * 16).map_err(|_| Refused::Size)?;
        let mut parts = vec![(place.slot, offset)];
        if place.wide {
            parts.push((place.slot + 1, offset + 8));
        }
        for (slot, offset) in parts {
            if !in_frame(slot) {
                return Err(Refused::Malformed);
            }
            written[usize::from(slot)] = true;
            homes[usize::from(slot)] = Home::Slot(offset);
        }
    }
    for (register, place) in shape.outputs {
        if *register as usize * 16 + 16 > i32::MAX as usize {
            return Err(Refused::Size);
        }
        let mut slots = vec![place.slot];
        if place.wide {
            slots.push(place.slot + 1);
        }
        for slot in slots {
            if !in_frame(slot) {
                return Err(Refused::Malformed);
            }
            uses[usize::from(slot)].push(end);
        }
    }
    for (slot, home) in homes.iter_mut().enumerate() {
        if !written[slot] {
            *home = Home::Const(shape.frame[slot]);
        }
    }
    let mut emitter = Emitter {
        asm: Asm::new(),
        words: homes
            .into_iter()
            .map(|home| Word {
                home,
                reg: None,
                dirty: false,
            })
            .collect(),
        owner: [None; 16],
        uses,
        cursor: vec![0; words],
        pinned: 0,
        exit,
        features,
        shape,
    };
    // H10: the head of the block takes its instructions from the budget,
    // and leaves when the budget is spent.
    let spent = emitter.asm.label();
    let count = i32::try_from(shape.instructions).map_err(|_| Refused::Size)?;
    emitter
        .asm
        .alu_imm(Alu::Sub, Rm::Mem(R13, CONTEXT_BUDGET), count);
    emitter.asm.jcc(Cond::S, spent);
    emitter.asm.cold(true);
    emitter.asm.bind(spent);
    emitter
        .asm
        .alu_imm(Alu::Add, Rm::Mem(R13, CONTEXT_BUDGET), count);
    emitter.asm.mov_imm(RAX, BUDGET);
    emitter.asm.jmp_absolute(exit);
    emitter.asm.cold(false);
    for (index, op) in shape.ops.iter().enumerate() {
        let (reads, writes) = &effects[index];
        if let Some((bytes, store)) = inline_access(op, shape.memory) {
            emitter.access(index, op, reads, bytes, store);
        } else if op.code == Code::Generic
            || (op.code as u8 >= FIRST_EFFECT && op.code != Code::Guard)
        {
            emitter.effect(index, reads, writes);
        } else if op.code == Code::Guard {
            emitter.guard(index, op, reads);
        } else {
            emitter.pure(index, op, reads)?;
        }
    }
    emitter.outputs();
    Ok(emitter.asm)
}
