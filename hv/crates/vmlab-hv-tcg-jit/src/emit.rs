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
//! Flags (DESIGN.md H8): a comparison that the next operation reads, as the
//! condition of a `Select` or of a `Guard`, and nothing else reads, is a
//! `cmp` whose host flags that operation reads: a `cmov` or a `jcc`, with
//! no `setcc` and no `test` between them (H8a). A signed order, which the
//! block form spells as an unsigned order of two values whose sign bits are
//! flipped, is a `cmp` of the values at their width, with a signed
//! condition. A helper call that computes a condition from a lazy flag
//! record of the guest (`LazyCondition`) is inline for the kinds of record
//! that its caller names, and a call for the rest (H8c).
//!
//! Registers: R13 holds the context of the entry, R14 the frame, R15 the
//! register slots of the state. RAX, RCX and RDX are scratch. The others,
//! but RSP, are the pool.
//!
//! Sources read: the Intel 64 and IA-32 Architectures Software Developer's
//! Manual, volume 2, for what each instruction computes, and the System V
//! AMD64 ABI for the call of the thunk. The operations are those of
//! `vmlab_hv_tcg_ir::block`.

use crate::asm::Label;
use crate::asm::{
    Alu, Asm, Cond, R8, R9, R10, R11, R12, R13, R14, R15, RAX, RBP, RBX, RCX, RDI, RDX, RSI, RSP,
    Reg, Rm, Shift,
};
use crate::tlb::{ADDEND, ENTRIES, ENTRY_BITS, TAG, WATCH};
use crate::{
    BUDGET, CHAIN, CONTEXT_ACCESSES, CONTEXT_BLOCK, CONTEXT_BUDGET, CONTEXT_CHAINED, CONTEXT_ENV,
    CONTEXT_HITS, CONTEXT_JUMP, CONTEXT_JUMP_GENERATION, CONTEXT_READS, CONTEXT_THUNK,
    CONTEXT_WRITES, Features, JUMP_ENTRIES, JUMP_SHIFT, LOGGED, LOOKUP, MISS, SITES, VERIFY,
    WATCHED,
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
    /// A helper call that is a condition of a lazy flag record, with the
    /// inline forms of its kinds (H8c).
    pub condition: Option<LazyCondition>,
}

/// A helper call whose result is whether a condition holds for a lazy flag
/// record of the guest: a kind and two operands (DESIGN.md H8c). The caller
/// knows the guest. It names each kind of record that has an inline form,
/// and the form. The call runs for a record of any other kind. The forms
/// are used only where the word `code` is the constant `code_value`: they
/// are the forms of that one condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LazyCondition {
    pub kind: Slot,
    pub a: Slot,
    pub b: Slot,
    pub code: Slot,
    pub code_value: u64,
    /// The word of the result: 0 or 1.
    pub result: Slot,
    /// Each kind of record with its form, tried in order.
    pub cases: Vec<(u64, FlagTest)>,
}

/// The host operation whose flags a `FlagTest::Host` reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagOperation {
    /// `cmp a, b`: the flags of `a - b`.
    Sub,
    /// `add a, b`: the flags of `a + b`.
    Add,
    /// `test a, a`: the flags of `a`, with CF and OF clear.
    Test,
}

/// How a condition follows from the operands of one kind of record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagTest {
    /// It holds, or does not, whatever the operands are.
    Always(bool),
    /// The host condition `cond` after `operation` on the low 32 bits of
    /// the operands, or on all 64 when `wide`.
    Host {
        operation: FlagOperation,
        wide: bool,
        cond: Cond,
    },
    /// The bit `a & 15` of the mask: a record that holds the flags
    /// themselves in the low four bits of `a`.
    Table(u16),
}

/// The flags CF, ZF, SF and OF of `cmp a, b` in 64 bits, or in 32 when not
/// `wide`, from the definition of `sub` in volume 2 of the Intel manual.
pub fn sub_flags(a: u64, b: u64, wide: bool) -> (bool, bool, bool, bool) {
    let (width, mask) = if wide {
        (64, u64::MAX)
    } else {
        (32, u64::from(u32::MAX))
    };
    let (a, b) = (a & mask, b & mask);
    let result = a.wrapping_sub(b) & mask;
    let sign = |value: u64| value >> (width - 1) & 1 == 1;
    (
        a < b,
        result == 0,
        sign(result),
        sign(a) != sign(b) && sign(result) != sign(a),
    )
}

/// The flags of `add a, b`, as `sub_flags` gives those of `cmp`.
pub fn add_flags(a: u64, b: u64, wide: bool) -> (bool, bool, bool, bool) {
    let (width, mask) = if wide {
        (64, u64::MAX)
    } else {
        (32, u64::from(u32::MAX))
    };
    let (a, b) = (a & mask, b & mask);
    let sum = u128::from(a) + u128::from(b);
    let result = sum as u64 & mask;
    let sign = |value: u64| value >> (width - 1) & 1 == 1;
    (
        sum >> width != 0,
        result == 0,
        sign(result),
        sign(a) == sign(b) && sign(result) != sign(a),
    )
}

impl FlagTest {
    /// Whether the test holds for the operands, by the definitions of the
    /// host instructions: what the compiled form must give.
    pub fn evaluate(self, a: u64, b: u64) -> bool {
        match self {
            FlagTest::Always(holds) => holds,
            FlagTest::Table(mask) => mask >> (a & 15) & 1 == 1,
            FlagTest::Host {
                operation,
                wide,
                cond,
            } => {
                let (cf, zf, sf, of) = match operation {
                    FlagOperation::Sub => sub_flags(a, b, wide),
                    FlagOperation::Add => add_flags(a, b, wide),
                    FlagOperation::Test => {
                        let (_, zf, sf, _) = sub_flags(a, 0, wide);
                        (false, zf, sf, false)
                    }
                };
                cond.holds(cf, zf, sf, of)
            }
        }
    }
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
/// The bits of `Op::aux` of a load or a store that are a mask of the low
/// bits of the address that an inline hit asks to be zero, beside those
/// below the size of the access. An address with one of them set misses
/// the fast TLB, and the thunk serves it, which knows why the owner asks
/// for them (the alignment of Device memory on aarch64).
pub const ALIGNMENT: u8 = 0x7f;

/// How a block that can be chained to starts and ends (DESIGN.md H5, H6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chain {
    /// The address of the frame of the block: its head loads it, because
    /// a block reached by a jump finds the frame of another one in R14.
    pub frame: u64,
    /// What its head stores in `Context::block`.
    pub tag: u64,
    /// The key of the context of the block in the jump cache. Not 0.
    pub key: u64,
    /// The offset of its first instruction in its 4 KiB page.
    pub page_offset: u32,
    /// Whether its exits look for the code of the next block: `sites`,
    /// linked jumps to a target in the same page, and `jumps`, the jump
    /// cache for the rest. Without either, the block leaves with `DONE`, and
    /// without one, its exits leave with `DONE` where it would serve them.
    pub sites: bool,
    pub jumps: bool,
    /// A hit of the jump cache leaves with `VERIFY` instead of jumping, so
    /// that the owner can compare the entry with its own lookup (a check
    /// for a bisect).
    pub verify: bool,
}

/// What the block makes of the PC of the state at its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NextPc {
    /// The PC moves by a constant: the block ends before an instruction
    /// that its owner runs. It leaves with `DONE`.
    Fixed(u64),
    /// The PC moves by the value of a word.
    Relative(Place),
    /// The PC is the value of a word.
    Absolute(Place),
}

/// The PC at the end of a block: the register slot that holds it, and how
/// it changes. No operation of a block writes that slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Next {
    pub pc: u32,
    pub kind: NextPc,
}

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
    /// The head and the exits of a block that is chained (H5). `None`: the
    /// frame is the one that `enter` names, and the block leaves with
    /// `DONE`.
    pub chain: Option<Chain>,
    /// The PC at the end of the block. `None`: the block does not change
    /// it.
    pub next: Option<Next>,
    /// How its loads and stores of 1 to 8 bytes reach memory (H7).
    pub memory: Memory,
}

/// A defect that the emitter puts into its code on purpose, for the tests
/// that show that K2 and K3 catch a wrong form (3e DESIGN.md J2). Only a
/// test turns one on: `Jit::seed_defect`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Defect {
    #[default]
    None,
    /// `Code::Add` subtracts.
    AddSubtracts,
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
        // Two words from `dst` on, and from `b` (low) and `dst` (high).
        Code::LoadPair => {
            let mut reads = vec![op.a];
            reads.extend(leave(op.imm)?);
            (reads, vec![op.dst, op.dst.checked_add(1)?])
        }
        Code::StorePair => {
            let mut reads = vec![op.a, op.b, op.dst];
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

/// An operand of a comparison that becomes host flags: a word, or a
/// constant that no word holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operand {
    Word(Slot),
    Imm(u64),
}

/// A comparison that the operation after it reads as host flags (H8a):
/// `cmp left, right` in 64 bits, or in 32 when not `wide`, then `cond`.
/// With `test`, a comparison of `left AND right` with zero: `test left,
/// right`.
#[derive(Clone, Copy, Debug)]
struct Fusion {
    left: Operand,
    right: Operand,
    cond: Cond,
    wide: bool,
    test: bool,
}

impl Fusion {
    fn words(&self) -> impl Iterator<Item = Slot> {
        [self.left, self.right]
            .into_iter()
            .filter_map(|operand| match operand {
                Operand::Word(word) => Some(word),
                Operand::Imm(_) => None,
            })
    }
}

/// The comparisons of the block that become host flags (H8a): for each
/// operation, the comparison that it reads as flags, and whether the
/// emitter skips it because the operation after it reads it so. The uses of
/// the operands move to the operation that reads the flags.
fn fusions(
    ops: &[Op],
    leaves: &[Vec<Slot>],
    homes: &[Home],
    uses: &mut [Vec<u32>],
) -> (Vec<Option<Fusion>>, Vec<bool>) {
    let mut fused = vec![None; ops.len()];
    let mut skip = vec![false; ops.len()];
    for i in 0..ops.len().saturating_sub(1) {
        let op = ops[i];
        let cond = match op.code {
            Code::Equal | Code::IsZero => Cond::E,
            Code::NotEqual => Cond::Ne,
            Code::Less => Cond::B,
            Code::LessEqual => Cond::Be,
            _ => continue,
        };
        let j = i + 1;
        let consumer = ops[j];
        let reads_flags = match consumer.code {
            Code::Select => {
                consumer.imm == u64::from(op.dst) && consumer.a != op.dst && consumer.b != op.dst
            }
            Code::Guard => {
                consumer.a == op.dst
                    && leaves
                        .get(consumer.imm as usize)
                        .is_some_and(|leave| !leave.contains(&op.dst))
            }
            _ => false,
        };
        let right = if op.code == Code::IsZero {
            Operand::Imm(0)
        } else {
            Operand::Word(op.b)
        };
        if !reads_flags
            || uses[usize::from(op.dst)] != [j as u32]
            || op.a == op.dst
            || right == Operand::Word(op.dst)
        {
            continue;
        }
        let mut fusion = Fusion {
            left: Operand::Word(op.a),
            right,
            cond,
            wide: true,
            test: false,
        };
        let mut group = vec![i];
        let with_zero = match op.code {
            Code::IsZero => true,
            Code::Equal | Code::NotEqual => homes[usize::from(op.b)] == Home::Const(0),
            _ => false,
        };
        if with_zero && let Some((x, y, wide, at)) = conjunction(ops, i, uses) {
            fusion = Fusion {
                left: Operand::Word(x),
                right: Operand::Word(y),
                cond,
                wide,
                test: true,
            };
            group.push(at);
        } else if matches!(op.code, Code::Less | Code::LessEqual)
            && let Some((x, y, wide, flips)) = signed(ops, i, homes, uses)
        {
            fusion = Fusion {
                left: x,
                right: y,
                cond: if op.code == Code::Less {
                    Cond::L
                } else {
                    Cond::Le
                },
                wide,
                test: false,
            };
            group.extend(flips);
        }
        for word in fusion.words() {
            let list = &mut uses[usize::from(word)];
            list.retain(|at| !group.contains(&(*at as usize)));
            let at = list.partition_point(|at| *at < j as u32);
            if list.get(at) != Some(&(j as u32)) {
                list.insert(at, j as u32);
            }
        }
        for at in group {
            skip[at] = true;
            uses[usize::from(ops[at].dst)].clear();
        }
        fused[j] = Some(fusion);
    }
    (fused, skip)
}

/// A comparison with zero at `i` of the result of an `And` just before it
/// that nothing else reads, of 64 bits or of 32: the operands of the `And`,
/// whether it is of 64 bits, and its place.
fn conjunction(ops: &[Op], i: usize, uses: &[Vec<u32>]) -> Option<(Slot, Slot, bool, usize)> {
    let at = i.checked_sub(1)?;
    let def = ops[at];
    if def.code != Code::And || def.dst != ops[i].a || uses[usize::from(def.dst)] != [i as u32] {
        return None;
    }
    let wide = match def.imm {
        u64::MAX => true,
        0xffff_ffff => false,
        _ => return None,
    };
    if [def.a, def.b].contains(&ops[i].dst) {
        return None;
    }
    Some((def.a, def.b, wide, at))
}

/// A signed order at `i`: an unsigned order of two values whose sign bits
/// are flipped, each by an operation just before it that nothing else
/// reads, or as a constant. The values, whether their width is 64 bits
/// (else 32), and the operations that flip them.
fn signed(
    ops: &[Op],
    i: usize,
    homes: &[Home],
    uses: &[Vec<u32>],
) -> Option<(Operand, Operand, bool, Vec<usize>)> {
    let op = ops[i];
    let constant = |word: Slot| match homes[usize::from(word)] {
        Home::Const(value) => Some(value),
        _ => None,
    };
    // A flip of the operations before `i`: the value and its width.
    let flip = |word: Slot| -> Option<(Slot, bool, usize)> {
        let at = (i.saturating_sub(2)..i)
            .rev()
            .find(|at| ops[*at].dst == word)?;
        let def = ops[at];
        if def.code != Code::Xor || uses[usize::from(word)] != [i as u32] {
            return None;
        }
        let (value, sign) = match (constant(def.a), constant(def.b)) {
            (_, Some(sign)) => (def.a, sign),
            (Some(sign), None) => (def.b, sign),
            (None, None) => return None,
        };
        let wide = match (sign, def.imm) {
            (0x8000_0000_0000_0000, u64::MAX) => true,
            (0x8000_0000, 0xffff_ffff) => false,
            _ => return None,
        };
        Some((value, wide, at))
    };
    let x = flip(op.a);
    let y = flip(op.b);
    let wide = match (x, y) {
        (Some((_, a, at_a)), Some((_, b, at_b))) if a == b && at_a != at_b => a,
        (Some((_, wide, _)), None) | (None, Some((_, wide, _))) => wide,
        _ => return None,
    };
    let sign = if wide { 1u64 << 63 } else { 1 << 31 };
    // The other operand is a constant with its sign bit flipped already.
    let operand = |word: Slot, flipped: Option<(Slot, bool, usize)>| match flipped {
        Some((value, _, _)) => Some(Operand::Word(value)),
        None => constant(word)
            .filter(|value| wide || *value <= u64::from(u32::MAX))
            .map(|value| Operand::Imm(value ^ sign)),
    };
    let left = operand(op.a, x)?;
    let right = operand(op.b, y)?;
    let flips: Vec<usize> = [x, y].into_iter().flatten().map(|(_, _, at)| at).collect();
    // A value read later than its place must not be written in between.
    let written: Vec<Slot> = (flips.iter().copied().min()?..=i)
        .map(|at| ops[at].dst)
        .collect();
    for operand in [left, right] {
        if let Operand::Word(word) = operand
            && written.contains(&word)
        {
            return None;
        }
    }
    Some((left, right, wide, flips))
}

struct Emitter<'a> {
    asm: Asm,
    /// Where each word is at the head of the block.
    initial: Vec<Home>,
    words: Vec<Word>,
    owner: [Option<Slot>; 16],
    /// The positions at which each word is read, in order.
    uses: Vec<Vec<u32>>,
    cursor: Vec<usize>,
    pinned: u16,
    exit: u64,
    features: Features,
    defect: Defect,
    shape: &'a Shape<'a>,
    /// The lazy conditions emitted inline.
    lazy: u64,
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
        asm.mov(RDX, Rm::Mem(R13, CONTEXT_BLOCK));
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

    /// Whether `lazy` describes the call at `index` as the block holds it:
    /// its words are those that the call reads and writes, and the word of
    /// its condition is the constant that its forms are for.
    fn lazy_fits(&self, lazy: &LazyCondition, reads: &[Slot], writes: &[Slot]) -> bool {
        let words = self.words.len();
        [lazy.kind, lazy.a, lazy.b, lazy.code, lazy.result]
            .iter()
            .all(|word| usize::from(*word) < words)
            && [lazy.kind, lazy.a, lazy.b, lazy.code]
                .iter()
                .all(|word| reads.contains(word))
            && writes == [lazy.result]
            && self.words[usize::from(lazy.code)].home == Home::Const(lazy.code_value)
    }

    /// The form of a test of the record whose operands are `a` and `b`: 0
    /// or 1 in RAX. It uses RCX and RDX.
    fn flag_test(&mut self, test: FlagTest, a: Val, b: Val) {
        let setcc = |asm: &mut Asm, cond: Cond| asm.setcc_al(cond);
        match test {
            FlagTest::Always(holds) => self.asm.mov_imm(RAX, u64::from(holds)),
            FlagTest::Table(mask) => {
                self.load(RCX, a);
                self.asm.alu_imm(Alu::And, Rm::Reg(RCX), 15);
                self.asm.mov_imm(RAX, u64::from(mask));
                self.asm.shift_cl(Shift::Shr, RAX);
                self.asm.alu_imm(Alu::And, Rm::Reg(RAX), 1);
            }
            FlagTest::Host {
                operation,
                wide,
                cond,
            } => {
                // The moves first, then the `xor` that clears RAX, then the
                // operation whose flags the `setcc` reads.
                match operation {
                    FlagOperation::Sub => {
                        let left = self.reg(a, RDX);
                        let right = self.rm(b, RCX);
                        self.asm.xor32(RAX, RAX);
                        if wide {
                            self.asm.alu(Alu::Cmp, left, right);
                        } else {
                            self.asm.alu32(Alu::Cmp, left, right);
                        }
                    }
                    FlagOperation::Add => {
                        self.load(RDX, a);
                        let right = self.rm(b, RCX);
                        self.asm.xor32(RAX, RAX);
                        if wide {
                            self.asm.alu(Alu::Add, RDX, right);
                        } else {
                            self.asm.alu32(Alu::Add, RDX, right);
                        }
                    }
                    FlagOperation::Test => {
                        let left = self.reg(a, RDX);
                        self.asm.xor32(RAX, RAX);
                        if wide {
                            self.asm.test(left, left);
                        } else {
                            self.asm.test32(left, left);
                        }
                    }
                }
                setcc(&mut self.asm, cond);
            }
        }
    }

    /// A helper call that is a condition of a lazy flag record (H8c): the
    /// kind is compared with each kind that has a form, whose form then
    /// runs inline. Any other kind goes to a stub that calls the thunk, as
    /// `effect` does, and reads the result from the frame.
    fn lazy_condition(&mut self, index: usize, lazy: &LazyCondition, reads: &[Slot]) {
        let at = index as u32;
        if self.next_use(lazy.result, at).is_none() {
            // A pure call that nothing reads.
            self.release(reads, at);
            self.unbind(lazy.result);
            self.words[usize::from(lazy.result)].home = Home::Frame;
            return;
        }
        self.lazy += 1;
        let kind = self.value(lazy.kind, at);
        let a = self.value(lazy.a, at);
        let b = self.value(lazy.b, at);
        let done = self.asm.label();
        let stub = self.asm.label();
        match kind {
            Val::Imm(kind) => match lazy.cases.iter().find(|(k, _)| *k == kind) {
                Some((_, test)) => self.flag_test(*test, a, b),
                None => self.asm.jmp(stub),
            },
            kind => {
                let kind = self.rm(kind, RCX);
                for (value, test) in &lazy.cases {
                    let next = self.asm.label();
                    match i32::try_from(*value as i64) {
                        Ok(value) => self.asm.alu_imm(Alu::Cmp, kind, value),
                        Err(_) => {
                            self.asm.mov_imm(RDX, *value);
                            match kind {
                                Rm::Reg(reg) => self.asm.alu(Alu::Cmp, reg, Rm::Reg(RDX)),
                                memory => self.asm.alu(Alu::Cmp, RDX, memory),
                            }
                        }
                    }
                    self.asm.jcc(Cond::Ne, next);
                    self.flag_test(*test, a, b);
                    self.asm.jmp(done);
                    self.asm.bind(next);
                }
                self.asm.jmp(stub);
            }
        }
        self.asm.bind(done);
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
        self.asm.mov(RAX, Self::frame(lazy.result));
        self.asm.jmp(done);
        self.asm.cold(false);
        self.define(lazy.result, reads, at);
    }

    /// A guard: on the inline path nothing happens. When its condition does
    /// not hold, a stub puts the words of its leave into the frame and asks
    /// the thunk, which leaves the block or lets it go on.
    fn guard(&mut self, index: usize, op: &Op, reads: &[Slot], fusion: Option<Fusion>) {
        let at = index as u32;
        let stub = self.asm.label();
        let back = self.asm.label();
        let mut all = reads.to_vec();
        if let Some(fusion) = fusion {
            // The guard reads the flags of the comparison before it (H8a).
            all.extend(fusion.words());
            let left = self.operand(fusion.left, at);
            let right = self.operand(fusion.right, at);
            match self.compare(fusion, left, right) {
                Ok(cond) => {
                    let fail = if op.aux != 0 { cond.inverse() } else { cond };
                    self.asm.jcc(fail, stub);
                }
                Err(holds) => {
                    if holds != (op.aux != 0) {
                        self.asm.jmp(stub);
                    }
                }
            }
        } else {
            let condition = self.value(op.a, at);
            // The guard fails where the condition is zero and it expects a
            // value that is not, or the other way.
            let fail = if op.aux != 0 { Cond::E } else { Cond::Ne };
            self.guard_test(condition, fail, op, stub);
        }
        self.asm.bind(back);
        // The stub, with the places of the words as they are here.
        self.asm.cold(true);
        self.asm.bind(stub);
        for word in reads {
            let w = self.words[usize::from(*word)];
            Self::home_code(&mut self.asm, *word, w);
        }
        if fusion.is_some() {
            // The thunk reads the condition: the value that fails.
            self.asm
                .store_imm(Self::frame(op.a), i32::from(op.aux == 0));
        }
        let saved = self.live_caller_saved();
        Self::call_thunk(&mut self.asm, index as u64, &saved);
        self.asm.test(RAX, RAX);
        self.asm.jcc_absolute(Cond::Ne, self.exit);
        self.asm.jmp(back);
        self.asm.cold(false);
        self.release(&all, at);
    }

    /// The test of a guard on the word of its condition.
    fn guard_test(&mut self, condition: Val, fail: Cond, op: &Op, stub: crate::asm::Label) {
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
    }

    /// The value of an operand of a fused comparison at `at`.
    fn operand(&mut self, operand: Operand, at: u32) -> Val {
        match operand {
            Operand::Word(word) => self.value(word, at),
            Operand::Imm(value) => Val::Imm(value),
        }
    }

    /// Set the host flags of the comparison `fusion` of `left` and `right`,
    /// and give the condition that holds where the comparison does. With
    /// two constants it emits nothing and gives whether it holds. It uses
    /// RCX as its scratch, and no other.
    fn compare(&mut self, fusion: Fusion, left: Val, right: Val) -> Result<Cond, bool> {
        let wide = fusion.wide;
        let cond = fusion.cond;
        if fusion.test {
            return self.test_flags(fusion, left, right);
        }
        let fits = |value: u64| !wide || i32::try_from(value as i64).is_ok();
        let cmp = |asm: &mut Asm, left: Reg, right: Rm| {
            if wide {
                asm.alu(Alu::Cmp, left, right);
            } else {
                asm.alu32(Alu::Cmp, left, right);
            }
        };
        match (left, right) {
            (Val::Imm(a), Val::Imm(b)) => {
                let (cf, zf, sf, of) = sub_flags(a, b, wide);
                Err(cond.holds(cf, zf, sf, of))
            }
            (left, Val::Imm(value)) if fits(value) => {
                let left = self.reg(left, RCX);
                if value == 0 {
                    // The flags of `test` are those of `cmp` with zero.
                    if wide {
                        self.asm.test(left, left);
                    } else {
                        self.asm.test32(left, left);
                    }
                } else if wide {
                    self.asm.alu_imm(Alu::Cmp, Rm::Reg(left), value as i32);
                } else {
                    self.asm.alu_imm32(Alu::Cmp, Rm::Reg(left), value as u32);
                }
                Ok(cond)
            }
            (Val::Imm(value), right) if fits(value) => {
                let swapped = Fusion {
                    cond: cond.swapped().expect("an order"),
                    ..fusion
                };
                self.compare(swapped, right, Val::Imm(value))
            }
            (left, Val::Imm(value)) => {
                // A constant that is no immediate, against a register or
                // memory.
                self.asm.mov_imm(RCX, value);
                match left {
                    Val::Reg(reg) => {
                        cmp(&mut self.asm, reg, Rm::Reg(RCX));
                        Ok(cond)
                    }
                    other => {
                        let memory = self.rm(other, RCX);
                        cmp(&mut self.asm, RCX, memory);
                        Ok(cond.swapped().expect("an order"))
                    }
                }
            }
            (left, right) => {
                let left = self.reg(left, RCX);
                let right = self.rm(right, RCX);
                cmp(&mut self.asm, left, right);
                Ok(cond)
            }
        }
    }

    /// The flags of `test left, right` for a fused comparison of a
    /// conjunction with zero. It uses RCX as its scratch, and no other.
    fn test_flags(&mut self, fusion: Fusion, left: Val, right: Val) -> Result<Cond, bool> {
        let wide = fusion.wide;
        let test = |asm: &mut Asm, a: Rm, b: Reg| asm.test_rm(a, b, wide);
        match (left, right) {
            (Val::Imm(a), Val::Imm(b)) => {
                let (_, zf, sf, _) = sub_flags(a & b, 0, wide);
                Err(fusion.cond.holds(false, zf, sf, false))
            }
            (Val::Imm(value), other) | (other, Val::Imm(value)) => {
                self.asm.mov_imm(RCX, value);
                let other = self.rm(other, RCX);
                test(&mut self.asm, other, RCX);
                Ok(fusion.cond)
            }
            (left, right) => {
                let left = self.reg(left, RCX);
                let right = self.rm(right, RCX);
                test(&mut self.asm, right, left);
                Ok(fusion.cond)
            }
        }
    }

    /// A `Select` that reads the flags of the comparison before it: the
    /// moves, then `cmp` and `cmov` (H8a).
    fn fused_select(&mut self, index: usize, op: &Op, reads: &[Slot], fusion: Fusion) {
        let at = index as u32;
        let mut all = reads.to_vec();
        all.extend(fusion.words());
        if self.next_use(op.dst, at).is_none() {
            self.release(&all, at);
            self.unbind(op.dst);
            self.words[usize::from(op.dst)].home = Home::Frame;
            return;
        }
        let a = self.value(op.a, at);
        let b = self.value(op.b, at);
        let left = self.operand(fusion.left, at);
        let right = self.operand(fusion.right, at);
        if let (Val::Imm(x), Val::Imm(y)) = (left, right) {
            let holds = self.compare(fusion, Val::Imm(x), Val::Imm(y)) == Err(true);
            self.load(RAX, if holds { a } else { b });
        } else {
            // Every move comes before the comparison: a move of zero is an
            // `xor`, which writes the flags.
            let rm = self.rm(a, RDX);
            self.load(RAX, b);
            match self.compare(fusion, left, right) {
                Ok(cond) => self.asm.cmov(cond, RAX, rm),
                Err(_) => unreachable!("two constants are handled above"),
            }
        }
        self.define(op.dst, &all, at);
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
        let low = (i32::from(bytes) - 1) | i32::from(op.aux & ALIGNMENT);
        self.asm.alu_imm(Alu::And, Rm::Reg(RDX), !0xfff | low);
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
                    Code::Add if self.defect == Defect::AddSubtracts => Alu::Sub,
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

    /// The end of the block: the outputs to the state, the PC, then the
    /// exit. The exits that chain are returned: the label of each jump and
    /// of its stub.
    fn outputs(&mut self) -> Vec<(Label, Label)> {
        // The word of the next PC goes into RCX before an output can write
        // over the slot that it is still in.
        let next = self.shape.next;
        if let Some(Next {
            kind: NextPc::Relative(place) | NextPc::Absolute(place),
            ..
        }) = next
        {
            let value = self.current(place.slot);
            self.load(RCX, value);
        }
        self.write_outputs();
        let Some(next) = next else {
            self.asm.xor32(RAX, RAX);
            self.asm.jmp_absolute(self.exit);
            return Vec::new();
        };
        let pc = i32::try_from(u64::from(next.pc) * 16).unwrap_or(i32::MAX);
        let slot = Rm::Mem(R15, pc);
        match next.kind {
            NextPc::Fixed(distance) => {
                self.asm.mov(RAX, slot);
                if i32::try_from(distance as i64).is_ok() {
                    self.asm.alu_imm(Alu::Add, Rm::Reg(RAX), distance as i32);
                } else {
                    self.asm.mov_imm(RDX, distance);
                    self.asm.alu(Alu::Add, RAX, Rm::Reg(RDX));
                }
            }
            NextPc::Relative(_) => {
                self.asm.mov(RAX, slot);
                self.asm.alu(Alu::Add, RAX, Rm::Reg(RCX));
            }
            NextPc::Absolute(_) => self.asm.mov(RAX, Rm::Reg(RCX)),
        }
        self.asm.store(slot, RAX);
        self.asm.store_imm(Rm::Mem(R15, pc + 8), 0);
        let chain = match self.shape.chain {
            Some(chain)
                if (chain.sites || chain.jumps) && !matches!(next.kind, NextPc::Fixed(_)) =>
            {
                chain
            }
            _ => {
                self.asm.xor32(RAX, RAX);
                self.asm.jmp_absolute(self.exit);
                return Vec::new();
            }
        };
        // H5: a constant distance to a target in the page of the block is a
        // jump that its owner links. RCX holds the distance.
        let mut sites = Vec::new();
        if let NextPc::Relative(place) = next.kind
            && chain.sites
        {
            for distance in self.candidates(place.slot) {
                let target = i64::from(chain.page_offset) + distance as i64;
                if !(0..4096).contains(&target) || sites.len() == SITES {
                    continue;
                }
                let Ok(immediate) = i32::try_from(distance as i64) else {
                    continue;
                };
                let other = self.asm.label();
                let jump = self.asm.label();
                let stub = self.asm.label();
                self.asm.alu_imm(Alu::Cmp, Rm::Reg(RCX), immediate);
                self.asm.jcc(Cond::Ne, other);
                self.asm.alu_imm(Alu::Add, Rm::Mem(R13, CONTEXT_CHAINED), 1);
                self.asm.bind(jump);
                self.asm.jmp(stub);
                self.asm.bind(other);
                self.asm.cold(true);
                self.asm.bind(stub);
                self.asm.mov_imm(RAX, CHAIN + sites.len() as u64);
                self.asm.jmp_absolute(self.exit);
                self.asm.cold(false);
                sites.push((jump, stub));
            }
        }
        if !chain.jumps {
            self.asm.xor32(RAX, RAX);
            self.asm.jmp_absolute(self.exit);
            return sites;
        }
        // H6: the jump cache, by the PC in RAX.
        let miss = self.asm.label();
        self.asm.mov(RDX, Rm::Reg(RAX));
        self.asm.shift_imm(Shift::Shr, RDX, 2);
        self.asm
            .alu_imm(Alu::And, Rm::Reg(RDX), (JUMP_ENTRIES - 1) as i32);
        self.asm.shift_imm(Shift::Shl, RDX, JUMP_SHIFT);
        self.asm.alu(Alu::Add, RDX, Rm::Mem(R13, CONTEXT_JUMP));
        self.asm.alu(Alu::Cmp, RAX, Rm::Mem(RDX, 0));
        self.asm.jcc(Cond::Ne, miss);
        self.asm.mov_imm(RCX, chain.key);
        self.asm
            .alu(Alu::Add, RCX, Rm::Mem(R13, CONTEXT_JUMP_GENERATION));
        self.asm.alu(Alu::Cmp, RCX, Rm::Mem(RDX, 8));
        self.asm.jcc(Cond::Ne, miss);
        self.asm.alu_imm(Alu::Add, Rm::Mem(R13, CONTEXT_HITS), 1);
        if chain.verify {
            self.asm.mov_imm(RAX, VERIFY);
            self.asm.jmp_absolute(self.exit);
        } else {
            self.asm.jmp_indirect(Rm::Mem(RDX, 16));
        }
        self.asm.cold(true);
        self.asm.bind(miss);
        self.asm.mov_imm(RAX, LOOKUP);
        self.asm.jmp_absolute(self.exit);
        self.asm.cold(false);
        sites
    }

    /// The constant values that the word `slot` can have at the end of
    /// the block: a constant, or a `Select` of two constants (a
    /// conditional branch).
    fn candidates(&self, slot: Slot) -> Vec<u64> {
        let constant = |word: Slot| match self.initial.get(usize::from(word)) {
            Some(Home::Const(value)) => Some(*value),
            _ => None,
        };
        if let Some(value) = constant(slot) {
            return vec![value];
        }
        let Some(op) = self.shape.ops.iter().rev().find(|op| {
            uses_of(op, self.shape.generics, self.shape)
                .is_some_and(|(_, writes)| writes.contains(&slot))
        }) else {
            return Vec::new();
        };
        if op.code != Code::Select || op.dst != slot {
            return Vec::new();
        }
        let mut values = Vec::new();
        for word in [op.a, op.b] {
            if let Some(value) = constant(word)
                && !values.contains(&value)
            {
                values.push(value);
            }
        }
        values
    }

    /// The outputs to the state.
    fn write_outputs(&mut self) {
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
    Some((bytes, store))
}

/// The words of the frame of `shape` that its code writes: the results of
/// its operations and its inputs. Every other word is a constant, which no
/// run changes. K3 fills the written words with a pattern before a replay,
/// so that a word that the code reads before it writes it shows.
pub fn written(shape: &Shape) -> Result<Vec<bool>, Refused> {
    Ok(analyse(shape)?.written)
}

/// What `emit` knows of a block before its pass.
struct Analysis {
    uses: Vec<Vec<u32>>,
    written: Vec<bool>,
    effects: Vec<(Vec<Slot>, Vec<Slot>)>,
    homes: Vec<Home>,
}

fn analyse(shape: &Shape) -> Result<Analysis, Refused> {
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
    if let Some(next) = shape.next {
        if next.pc as usize * 16 + 16 > i32::MAX as usize {
            return Err(Refused::Size);
        }
        if let NextPc::Relative(place) | NextPc::Absolute(place) = next.kind {
            if place.wide || !in_frame(place.slot) {
                return Err(Refused::Malformed);
            }
            uses[usize::from(place.slot)].push(end);
        }
    }
    for (slot, home) in homes.iter_mut().enumerate() {
        if !written[slot] {
            *home = Home::Const(shape.frame[slot]);
        }
    }
    Ok(Analysis {
        uses,
        written,
        effects,
        homes,
    })
}

/// Emit the code of `shape`. `exit` is the address of the common exit of
/// the trampoline. `host_flags` turns on the forms of H8. With the code, the
/// labels of the jumps that chain and of their stubs, in the order of their
/// exit indices, the count of comparisons that became host flags, and the
/// count of lazy conditions that are inline.
pub fn emit(
    shape: &Shape,
    exit: u64,
    features: Features,
    defect: Defect,
    host_flags: bool,
) -> Result<(Asm, Vec<(Label, Label)>, u64, u64), Refused> {
    let Analysis {
        mut uses,
        effects,
        homes,
        ..
    } = analyse(shape)?;
    let words = shape.frame.len();
    let (fused, skip) = if host_flags {
        fusions(shape.ops, shape.leaves, &homes, &mut uses)
    } else {
        (vec![None; shape.ops.len()], vec![false; shape.ops.len()])
    };
    let mut emitter = Emitter {
        asm: Asm::new(),
        initial: homes.clone(),
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
        defect,
        shape,
        lazy: 0,
    };
    // H5: the head of a block that chains loads its own frame and names
    // itself, because a jump from another block reaches it.
    if let Some(chain) = shape.chain {
        emitter.asm.mov_imm(R14, chain.frame);
        emitter.asm.mov_imm(RAX, chain.tag);
        emitter.asm.store(Rm::Mem(R13, CONTEXT_BLOCK), RAX);
    }
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
    // H7: the block counts its inlined loads and stores, for the hit rate.
    let inlined = shape
        .ops
        .iter()
        .filter(|op| inline_access(op, shape.memory).is_some())
        .count();
    if inlined != 0 {
        let inlined = i32::try_from(inlined).map_err(|_| Refused::Size)?;
        emitter
            .asm
            .alu_imm(Alu::Add, Rm::Mem(R13, CONTEXT_ACCESSES), inlined);
    }
    for (index, op) in shape.ops.iter().enumerate() {
        if skip[index] {
            continue;
        }
        let (reads, writes) = &effects[index];
        let lazy = match op.code {
            Code::Call => shape
                .calls
                .get(op.imm as usize)
                .and_then(|call| call.condition.as_ref())
                .filter(|lazy| host_flags && emitter.lazy_fits(lazy, reads, writes)),
            _ => None,
        };
        if let Some((bytes, store)) = inline_access(op, shape.memory) {
            emitter.access(index, op, reads, bytes, store);
        } else if let Some(lazy) = lazy {
            emitter.lazy_condition(index, lazy, reads);
        } else if op.code == Code::Generic
            || (op.code as u8 >= FIRST_EFFECT && op.code != Code::Guard)
        {
            emitter.effect(index, reads, writes);
        } else if op.code == Code::Guard {
            emitter.guard(index, op, reads, fused[index]);
        } else if let (Code::Select, Some(fusion)) = (op.code, fused[index]) {
            emitter.fused_select(index, op, reads, fusion);
        } else {
            emitter.pure(index, op, reads)?;
        }
    }
    let sites = emitter.outputs();
    let fused = fused.iter().filter(|fusion| fusion.is_some()).count() as u64;
    Ok((emitter.asm, sites, fused, emitter.lazy))
}
