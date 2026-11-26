//! The pre-decoded block form (DESIGN.md M6, the gate of card C5).
//!
//! A block of the typed IR is lowered once into a dense stream of 16-byte
//! operations on a frame of 64-bit words, and the loop of `exec` runs that
//! stream. The semantics stay where they are, in the lifters: this file
//! knows the operations of the graph and nothing of aarch64.
//!
//! - A scalar of at most 64 bits (`Bool`, `Bits(1..=64)`) is one word of the
//!   frame. A wider one (`Bits(65..=128)`, `Integer`) is two words, the low
//!   one first.
//! - A view of a scalar (`Refine`) shares its words.
//! - An operation whose result is one word and that needs one word of each
//!   operand has a code of its own. Every other operation is a `Generic`:
//!   it runs through the `u128` evaluators of `vmlab-hv-tcg-ir`, as the slot
//!   executor of that crate does.
//! - A constant is a word of the frame, written when the block is lowered.
//!   No operation holds an immediate, so every operand is read the same way.
//!
//! The tests of this file compare every lowered operation with the slot
//! executor of `vmlab-hv-tcg-ir` (`linear::lower` and `Packed::evaluate`) on
//! random values.
//!
//! Sources read: none. This file holds no instruction semantics.

use std::collections::HashMap;
use vmlab_hv_tcg_ir::native::{
    Binary, Graph, Operation, ScalarId, ScalarType, Unary, evaluate_binary, evaluate_unary, mask,
};

/// A word of the frame of a block.
pub(crate) type Slot = u16;

/// What an operation does. The codes from `Guard` on are the effects: the
/// loop of `exec` runs them, `step` runs the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Code {
    /// `a + b`, and so on for the next five: the low words, then `imm` as a
    /// mask.
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    /// `a` shifted by `b`, in `aux` bits. `imm` is the mask of the result.
    ShiftLeft,
    ShiftRight,
    ArithmeticShiftRight,
    /// Comparisons of two words. The result is 0 or 1.
    Equal,
    NotEqual,
    Less,
    LessEqual,
    /// `a == 0`: a comparison with zero, and the negation of a Bool.
    IsZero,
    BitNot,
    Negate,
    /// The unary operations on the low `aux` bits of `a`, then `imm` as a
    /// mask.
    SignExtend,
    LeadingZeros,
    TrailingZeros,
    ReverseBits,
    ReverseBytes,
    PopCount,
    /// `(a >> aux) & imm`: a slice, and a truncation with `aux` zero.
    Slice,
    /// `((a << aux) | b) & imm`.
    Concat,
    /// `a` where the word `imm` is not zero, else `b`.
    Select,
    /// A word as the low word of two.
    Widen,
    /// The generic operation of the index `imm`.
    Generic,
    /// Leave through the exit `imm` unless the word `a` is not zero (`aux`
    /// 1) or is zero (`aux` 0).
    Guard,
    /// Load `aux` bytes at the address `a` into `dst`. The exit is `imm`.
    Load1,
    Load2,
    Load4,
    Load8,
    /// Store the low `aux` bytes of `b` at the address `a`. The exit is
    /// `imm`.
    Store1,
    Store2,
    Store4,
    Store8,
    /// The access of the index `imm`: 16 bytes, or exclusive.
    Access,
    /// The helper call of the index `imm`.
    Call,
}

/// The first code that `step` does not run.
pub(crate) const FIRST_EFFECT: u8 = Code::Guard as u8;

/// One operation of a block.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Op {
    pub code: Code,
    pub aux: u8,
    pub dst: Slot,
    pub a: Slot,
    pub b: Slot,
    pub imm: u64,
}

/// An operand or a result of a generic operation: its first word, and
/// whether it has a second one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Place {
    pub slot: Slot,
    pub wide: bool,
}

impl Place {
    #[inline]
    pub fn read(self, frame: &[u64]) -> u128 {
        let low = u128::from(frame[usize::from(self.slot)]);
        if self.wide {
            low | u128::from(frame[usize::from(self.slot) + 1]) << 64
        } else {
            low
        }
    }

    #[inline]
    pub fn write(self, frame: &mut [u64], value: u128) {
        frame[usize::from(self.slot)] = value as u64;
        if self.wide {
            frame[usize::from(self.slot) + 1] = (value >> 64) as u64;
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Unary(Unary),
    Binary(Binary),
    Slice { offset: u16 },
    Concat { low_width: u16 },
    Select,
}

/// An operation on `u128` values: what has no code of its own.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Generic {
    kind: Kind,
    ty: ScalarType,
    /// The mask of the result. A binary operation masks by its type.
    mask: u128,
    args: [Place; 3],
    dst: Place,
}

impl Generic {
    /// `false`: an arithmetic operation outside its domain.
    #[inline(never)]
    fn run(&self, frame: &mut [u64]) -> bool {
        let a = self.args[0].read(frame);
        let b = self.args[1].read(frame);
        let value = match self.kind {
            Kind::Unary(operation) => evaluate_unary(operation, a).map(|value| value & self.mask),
            Kind::Binary(operation) => evaluate_binary(operation, self.ty, a, b),
            Kind::Slice { offset } => Some(a.checked_shr(offset.into()).unwrap_or(0) & self.mask),
            Kind::Concat { low_width } => {
                Some((a.checked_shl(low_width.into()).unwrap_or(0) | b) & self.mask)
            }
            Kind::Select => Some(if a != 0 { b } else { self.args[2].read(frame) }),
        };
        match value {
            Some(value) => {
                self.dst.write(frame, value);
                true
            }
            None => false,
        }
    }
}

/// The low `width` bits, for a width of 1 to 64.
#[inline(always)]
fn low(width: u8) -> u64 {
    u64::MAX >> (64 - u32::from(width))
}

/// Run one operation that is not an effect. `false`: an arithmetic
/// operation outside its domain, which only a generic operation has.
#[inline(always)]
pub(crate) fn step(op: &Op, frame: &mut [u64], generics: &[Generic]) -> bool {
    let a = frame[usize::from(op.a)];
    let b = frame[usize::from(op.b)];
    let width = u32::from(op.aux);
    let value = match op.code {
        Code::Add => a.wrapping_add(b) & op.imm,
        Code::Sub => a.wrapping_sub(b) & op.imm,
        Code::Mul => a.wrapping_mul(b) & op.imm,
        Code::And => a & b & op.imm,
        Code::Or => (a | b) & op.imm,
        Code::Xor => (a ^ b) & op.imm,
        Code::ShiftLeft => (if b < 64 { a << b } else { 0 }) & op.imm,
        Code::ShiftRight => (if b < 64 { a >> b } else { 0 }) & op.imm,
        Code::ArithmeticShiftRight => {
            let signed = ((a << (64 - width)) as i64) >> (64 - width);
            (signed >> b.min(u64::from(width) - 1)) as u64 & op.imm
        }
        Code::Equal => u64::from(a == b),
        Code::NotEqual => u64::from(a != b),
        Code::Less => u64::from(a < b),
        Code::LessEqual => u64::from(a <= b),
        Code::IsZero => u64::from(a == 0),
        Code::BitNot => !a & op.imm,
        Code::Negate => a.wrapping_neg() & op.imm,
        Code::SignExtend => ((a << (64 - width)) as i64 >> (64 - width)) as u64 & op.imm,
        Code::LeadingZeros => u64::from((a & low(op.aux)).leading_zeros() - (64 - width)) & op.imm,
        Code::TrailingZeros => u64::from((a & low(op.aux)).trailing_zeros().min(width)) & op.imm,
        Code::ReverseBits => ((a & low(op.aux)).reverse_bits() >> (64 - width)) & op.imm,
        Code::ReverseBytes => ((a & low(op.aux)).swap_bytes() >> (64 - width)) & op.imm,
        Code::PopCount => u64::from((a & low(op.aux)).count_ones()) & op.imm,
        Code::Slice => (a >> width) & op.imm,
        Code::Concat => ((a << width) | b) & op.imm,
        Code::Select => {
            if frame[op.imm as usize] != 0 {
                a
            } else {
                b
            }
        }
        Code::Widen => {
            frame[usize::from(op.dst) + 1] = 0;
            a
        }
        Code::Generic => return generics[op.imm as usize].run(frame),
        // The loop of `exec` runs the effects.
        Code::Guard
        | Code::Load1
        | Code::Load2
        | Code::Load4
        | Code::Load8
        | Code::Store1
        | Code::Store2
        | Code::Store4
        | Code::Store8
        | Code::Access
        | Code::Call => return true,
    };
    frame[usize::from(op.dst)] = value;
    true
}

fn is_wide(ty: ScalarType) -> bool {
    match ty {
        ScalarType::Bool => false,
        ScalarType::Bits(width) => width > 64,
        ScalarType::Integer => true,
    }
}

/// The mask of a type of one word, for a result of `step`. A Bool is 0 or 1
/// by what computes it.
fn word_mask(ty: ScalarType) -> u64 {
    match ty {
        ScalarType::Bits(width) => mask(width) as u64,
        ScalarType::Bool | ScalarType::Integer => u64::MAX,
    }
}

fn type_mask(ty: ScalarType) -> u128 {
    match ty {
        ScalarType::Bits(width) => mask(width),
        ScalarType::Bool | ScalarType::Integer => u128::MAX,
    }
}

/// The words of the scalars of one block, and the operations so far.
pub(crate) struct Lowering {
    first: Vec<Slot>,
    wide: Vec<bool>,
    /// The frame of the block. The constants are in place.
    pub frame: Vec<u64>,
    pub ops: Vec<Op>,
    pub generics: Vec<Generic>,
    /// The words that hold a literal for an operand of one word.
    literals: HashMap<u64, Slot>,
}

impl Lowering {
    /// `None`: the block has more words than a `Slot` counts.
    pub fn new(graph: &Graph) -> Option<Self> {
        let mut first: Vec<Slot> = Vec::with_capacity(graph.scalars.len());
        let mut wide = Vec::with_capacity(graph.scalars.len());
        // The word 0 is the operand of an operation that has none.
        let mut words = 1usize;
        for scalar in &graph.scalars {
            let is = is_wide(scalar.ty);
            wide.push(is);
            // A view has the value of its scalar, and that value fits the
            // type of the view. A view of one word of a scalar of two is its
            // low word.
            if let Operation::Refine(value) = scalar.operation
                && (wide[value] || !is)
            {
                first.push(first[value]);
                continue;
            }
            first.push(Slot::try_from(words).ok()?);
            words += if is { 2 } else { 1 };
        }
        Slot::try_from(words).ok()?;
        Some(Self {
            first,
            wide,
            frame: vec![0; words],
            ops: Vec::new(),
            generics: Vec::new(),
            literals: HashMap::new(),
        })
    }

    /// The words of the scalar `id`.
    pub fn place(&self, id: ScalarId) -> Place {
        Place {
            slot: self.first[id],
            wide: self.wide[id],
        }
    }

    /// The low word of the scalar `id`.
    pub fn word(&self, id: ScalarId) -> Slot {
        self.first[id]
    }

    /// Put a constant of the graph into its words.
    pub fn constant(&mut self, id: ScalarId, value: u128) {
        self.place(id).write(&mut self.frame, value);
    }

    /// A word that holds the whole value of `id`: the scalar has one word,
    /// or is a view of a scalar of one word, or is a literal that fits one.
    fn whole(&mut self, graph: &Graph, mut id: ScalarId) -> Option<Slot> {
        loop {
            if !self.wide[id] {
                return Some(self.first[id]);
            }
            let Operation::Refine(value) = graph.scalars[id].operation else {
                break;
            };
            id = value;
        }
        let value = u64::try_from(graph.literal(id)?).ok()?;
        if let Some(slot) = self.literals.get(&value) {
            return Some(*slot);
        }
        let slot = Slot::try_from(self.frame.len()).ok()?;
        self.frame.push(value);
        self.literals.insert(value, slot);
        Some(slot)
    }

    fn push(&mut self, code: Code, aux: u8, dst: Slot, a: Slot, b: Slot, imm: u64) {
        self.ops.push(Op {
            code,
            aux,
            dst,
            a,
            b,
            imm,
        });
    }

    /// An effect: an operation that the loop of `exec` runs.
    pub fn effect(&mut self, code: Code, aux: u8, dst: Slot, a: Slot, b: Slot, imm: u64) {
        debug_assert!(code as u8 >= FIRST_EFFECT);
        self.push(code, aux, dst, a, b, imm);
    }

    fn generic(&mut self, kind: Kind, id: ScalarId, graph: &Graph, args: &[ScalarId], mask: u128) {
        let mut places = [Place::default(); 3];
        for (place, arg) in places.iter_mut().zip(args) {
            *place = self.place(*arg);
        }
        let index = self.generics.len() as u64;
        self.generics.push(Generic {
            kind,
            ty: graph.scalars[id].ty,
            mask,
            args: places,
            dst: self.place(id),
        });
        self.push(Code::Generic, 0, 0, 0, 0, index);
    }

    /// The operation of one word for the unary scalar `id`, where it has one.
    fn unary(&mut self, graph: &Graph, id: ScalarId, operation: Unary, value: ScalarId) -> bool {
        let ty = graph.scalars[id].ty;
        if self.wide[id] {
            return false;
        }
        let imm = word_mask(ty);
        let dst = self.first[id];
        // The low word of the operand is enough for an operation on its low
        // 64 bits or fewer.
        let low_word = self.first[value];
        let (code, aux, a) = match operation {
            Unary::BoolNot if !self.wide[value] => (Code::IsZero, 0, low_word),
            Unary::BitNot => (Code::BitNot, 0, low_word),
            Unary::Negate if !self.wide[value] => (Code::Negate, 0, low_word),
            Unary::Truncate(width) => {
                let imm = (mask(width) as u64) & imm;
                self.push(Code::Slice, 0, dst, low_word, 0, imm);
                return true;
            }
            Unary::SignExtend(width @ 1..=64) => (Code::SignExtend, width as u8, low_word),
            Unary::LeadingZeros(width @ 1..=64) => (Code::LeadingZeros, width as u8, low_word),
            Unary::TrailingZeros(width @ 1..=64) => (Code::TrailingZeros, width as u8, low_word),
            Unary::ReverseBits(width @ 1..=64) => (Code::ReverseBits, width as u8, low_word),
            Unary::ReverseBytes(width @ 8..=64) if width.is_multiple_of(8) => {
                (Code::ReverseBytes, width as u8, low_word)
            }
            Unary::PopCount(width @ 1..=64) => (Code::PopCount, width as u8, low_word),
            _ => return false,
        };
        // A Bool that is negated is 0 or 1 and takes no mask, and a type of
        // no bits has the mask zero, which is right for every code here.
        self.push(code, aux, dst, a, 0, imm);
        true
    }

    /// The operation of one word for the binary scalar `id`, where it has one.
    fn binary(
        &mut self,
        graph: &Graph,
        id: ScalarId,
        operation: Binary,
        left: ScalarId,
        right: ScalarId,
    ) -> bool {
        let ty = graph.scalars[id].ty;
        if self.wide[id] {
            return false;
        }
        let dst = self.first[id];
        let bits = matches!(ty, ScalarType::Bits(_));
        match operation {
            Binary::Add | Binary::Sub | Binary::Mul | Binary::And | Binary::Or | Binary::Xor => {
                // A result of at most 64 bits is a function of the low words
                // of its operands. A Bool has no mask, so its operands are
                // whole words.
                let (a, b) = if bits {
                    (self.first[left], self.first[right])
                } else if matches!(operation, Binary::And | Binary::Or | Binary::Xor)
                    && !self.wide[left]
                    && !self.wide[right]
                {
                    (self.first[left], self.first[right])
                } else {
                    return false;
                };
                let code = match operation {
                    Binary::Add => Code::Add,
                    Binary::Sub => Code::Sub,
                    Binary::Mul => Code::Mul,
                    Binary::And => Code::And,
                    Binary::Or => Code::Or,
                    _ => Code::Xor,
                };
                self.push(code, 0, dst, a, b, word_mask(ty));
                true
            }
            Binary::ShiftLeft | Binary::ShiftRight | Binary::ArithmeticShiftRight => {
                let ScalarType::Bits(width @ 1..=64) = ty else {
                    return false;
                };
                // A shift to the right moves bits of the second word in.
                if operation == Binary::ShiftRight && self.wide[left] {
                    return false;
                }
                let Some(amount) = self.whole(graph, right) else {
                    return false;
                };
                let code = match operation {
                    Binary::ShiftLeft => Code::ShiftLeft,
                    Binary::ShiftRight => Code::ShiftRight,
                    _ => Code::ArithmeticShiftRight,
                };
                self.push(
                    code,
                    width as u8,
                    dst,
                    self.first[left],
                    amount,
                    word_mask(ty),
                );
                true
            }
            Binary::Equal
            | Binary::NotEqual
            | Binary::LessSigned
            | Binary::LessUnsigned
            | Binary::LessEqualSigned
            | Binary::LessEqualUnsigned => {
                // The result is 0 or 1. A type of no bits would mask it away.
                if matches!(ty, ScalarType::Bits(0)) {
                    return false;
                }
                let Some(a) = self.whole(graph, left) else {
                    return false;
                };
                let Some(b) = self.whole(graph, right) else {
                    return false;
                };
                // Two values of one word are not negative, so the signed
                // order is the unsigned one.
                let code = match operation {
                    Binary::Equal => Code::Equal,
                    Binary::NotEqual => Code::NotEqual,
                    Binary::LessSigned | Binary::LessUnsigned => Code::Less,
                    _ => Code::LessEqual,
                };
                // A comparison with the literal zero reads one word.
                if graph.literal(right) == Some(0) {
                    match code {
                        Code::Equal | Code::LessEqual => {
                            self.push(Code::IsZero, 0, dst, a, 0, 0);
                            return true;
                        }
                        _ => {}
                    }
                }
                self.push(code, 0, dst, a, b, 0);
                true
            }
            Binary::DivideSigned
            | Binary::RemainderSigned
            | Binary::DivideUnsigned
            | Binary::RemainderUnsigned => false,
        }
    }

    fn slice(
        &mut self,
        graph: &Graph,
        id: ScalarId,
        value: ScalarId,
        offset: u16,
        width: u16,
    ) -> bool {
        if self.wide[id] {
            return false;
        }
        let imm = (mask(width) as u64) & word_mask(graph.scalars[id].ty);
        let dst = self.first[id];
        // The bits that the slice takes, counted from `offset`.
        let length = u32::from(64 - imm.leading_zeros() as u16);
        let offset = u32::from(offset);
        if !self.wide[value] {
            if offset >= 64 {
                return false;
            }
            self.push(Code::Slice, offset as u8, dst, self.first[value], 0, imm);
            return true;
        }
        if offset + length <= 64 && offset < 64 {
            self.push(Code::Slice, offset as u8, dst, self.first[value], 0, imm);
            true
        } else if (64..128).contains(&offset) {
            let high = self.first[value] + 1;
            self.push(Code::Slice, (offset - 64) as u8, dst, high, 0, imm);
            true
        } else {
            false
        }
    }

    /// Append the operation that computes the scalar `id`: a `Unary`, a
    /// `Binary`, a `Slice`, a `Concat`, a `Select` or a `Refine`. The words
    /// of its operands are computed already.
    pub fn scalar(&mut self, graph: &Graph, id: ScalarId) {
        let scalar = &graph.scalars[id];
        let ty = scalar.ty;
        match scalar.operation {
            Operation::Refine(value) => {
                // A view of two words of a scalar of one. Every other view
                // shares the words of its scalar.
                if self.wide[id] && !self.wide[value] {
                    self.push(Code::Widen, 0, self.first[id], self.first[value], 0, 0);
                }
            }
            Operation::Unary(operation, value) => {
                if !self.unary(graph, id, operation, value) {
                    self.generic(Kind::Unary(operation), id, graph, &[value], type_mask(ty));
                }
            }
            Operation::Binary(operation, left, right) => {
                if !self.binary(graph, id, operation, left, right) {
                    self.generic(Kind::Binary(operation), id, graph, &[left, right], 0);
                }
            }
            Operation::Slice {
                value,
                offset,
                width,
            } => {
                if !self.slice(graph, id, value, offset, width) {
                    let mask = mask(width) & type_mask(ty);
                    self.generic(Kind::Slice { offset }, id, graph, &[value], mask);
                }
            }
            Operation::Concat {
                high,
                low,
                low_width,
            } => {
                if !self.wide[id] && low_width < 64 {
                    let (a, b) = (self.first[high], self.first[low]);
                    self.push(
                        Code::Concat,
                        low_width as u8,
                        self.first[id],
                        a,
                        b,
                        word_mask(ty),
                    );
                } else {
                    let kind = Kind::Concat { low_width };
                    self.generic(kind, id, graph, &[high, low], type_mask(ty));
                }
            }
            Operation::Select {
                condition,
                when_true,
                when_false,
            } => {
                if !self.wide[id] && !self.wide[condition] {
                    let (a, b) = (self.first[when_true], self.first[when_false]);
                    let condition = u64::from(self.first[condition]);
                    self.push(Code::Select, 0, self.first[id], a, b, condition);
                } else {
                    let args = [condition, when_true, when_false];
                    self.generic(Kind::Select, id, graph, &args, 0);
                }
            }
            Operation::Constant(_)
            | Operation::Input(_)
            | Operation::Effect(_)
            | Operation::Call { .. } => {
                debug_assert!(false, "the loop of exec binds this scalar");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmlab_hv_tcg_ir::native::linear;
    use vmlab_hv_tcg_ir::native::packed::Packed;

    #[test]
    fn an_operation_is_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<Op>(), 16);
    }

    /// A small generator, so that a failure repeats.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// A value of `ty`, with the edges of its range more often than not.
        fn value(&mut self, ty: ScalarType) -> u128 {
            let raw = u128::from(self.next()) | u128::from(self.next()) << 64;
            let shaped = match self.next() % 8 {
                0 => 0,
                1 => u128::MAX,
                2 => 1,
                3 => raw & 0x7f,
                4 => 1u128 << (self.next() % 128),
                5 => !(1u128 << (self.next() % 128)),
                _ => raw,
            };
            match ty {
                ScalarType::Bool => shaped & 1,
                ScalarType::Bits(width) => shaped & mask(width),
                // The shift counts and the offsets of a lifter: not negative.
                ScalarType::Integer => shaped & mask(100),
            }
        }
    }

    /// Run the scalar `id` of `graph`, whose inputs are the scalars
    /// `inputs`, on the slot executor of `vmlab-hv-tcg-ir`.
    fn reference(graph: &Graph, inputs: &[(ScalarId, u128)], id: ScalarId) -> Option<u128> {
        let (slots, count) = linear::slots(graph);
        let mut values = vec![0u128; count];
        let mut seen = vec![false; graph.scalars.len()];
        for scalar in vmlab_hv_tcg_ir::native::schedule::scalars(graph, &[id], &mut seen) {
            match graph.scalars[scalar].operation {
                Operation::Constant(value) => values[slots[scalar]] = value,
                Operation::Input(_) => {
                    let (_, value) = inputs.iter().find(|(input, _)| *input == scalar)?;
                    values[slots[scalar]] = *value;
                }
                Operation::Refine(_) => {}
                _ => {
                    let mut instruction = linear::lower(graph, scalar)?;
                    instruction.remap(&slots);
                    let packed = Packed::new(slots[scalar], instruction)?;
                    values[slots[scalar]] = packed.evaluate(&values)?;
                }
            }
        }
        Some(values[slots[id]])
    }

    /// The same on the pre-decoded form. The second value: whether every
    /// operation had a code of its own.
    fn lowered(graph: &Graph, inputs: &[(ScalarId, u128)], id: ScalarId) -> (Option<u128>, bool) {
        let mut lowering = Lowering::new(graph).expect("a small block");
        let mut seen = vec![false; graph.scalars.len()];
        for scalar in vmlab_hv_tcg_ir::native::schedule::scalars(graph, &[id], &mut seen) {
            match graph.scalars[scalar].operation {
                Operation::Constant(value) => lowering.constant(scalar, value),
                Operation::Input(_) => {
                    let (_, value) = inputs
                        .iter()
                        .find(|(input, _)| *input == scalar)
                        .expect("an input with a value");
                    lowering.constant(scalar, *value);
                }
                _ => lowering.scalar(graph, scalar),
            }
        }
        let fast = lowering.generics.is_empty();
        let mut frame = lowering.frame.clone();
        for op in &lowering.ops {
            if !step(op, &mut frame, &lowering.generics) {
                return (None, fast);
            }
        }
        (Some(lowering.place(id).read(&frame)), fast)
    }

    const TYPES: [ScalarType; 12] = [
        ScalarType::Bool,
        ScalarType::Bits(1),
        ScalarType::Bits(7),
        ScalarType::Bits(8),
        ScalarType::Bits(16),
        ScalarType::Bits(32),
        ScalarType::Bits(63),
        ScalarType::Bits(64),
        ScalarType::Bits(65),
        ScalarType::Bits(100),
        ScalarType::Bits(128),
        ScalarType::Integer,
    ];

    const BINARY: [Binary; 19] = [
        Binary::Add,
        Binary::Sub,
        Binary::Mul,
        Binary::And,
        Binary::Or,
        Binary::Xor,
        Binary::ShiftLeft,
        Binary::ShiftRight,
        Binary::ArithmeticShiftRight,
        Binary::Equal,
        Binary::NotEqual,
        Binary::LessSigned,
        Binary::LessUnsigned,
        Binary::LessEqualSigned,
        Binary::LessEqualUnsigned,
        Binary::DivideSigned,
        Binary::RemainderSigned,
        Binary::DivideUnsigned,
        Binary::RemainderUnsigned,
    ];

    /// Compare both executors on `rounds` random values of the inputs.
    fn compare(
        random: &mut Random,
        graph: &Graph,
        inputs: &[ScalarId],
        id: ScalarId,
        rounds: usize,
        what: &str,
    ) -> bool {
        let mut fast = true;
        for _ in 0..rounds {
            let values: Vec<(ScalarId, u128)> = inputs
                .iter()
                .map(|input| (*input, random.value(graph.scalars[*input].ty)))
                .collect();
            let expected = reference(graph, &values, id);
            // A value outside its type is not a value: a graph that a
            // lifter builds has none, and the two executors need not agree.
            let fits = match (expected, graph.scalars[id].ty) {
                (Some(value), ScalarType::Bool) => value <= 1,
                (Some(value), ScalarType::Bits(width)) => value <= mask(width),
                _ => true,
            };
            if !fits {
                continue;
            }
            let (got, was_fast) = lowered(graph, &values, id);
            fast &= was_fast;
            assert_eq!(got, expected, "{what} on {values:x?}");
        }
        fast
    }

    #[test]
    fn binary_operations_match_the_slot_executor() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        let mut fast = 0;
        let mut built = 0;
        for operation in BINARY {
            for result in TYPES {
                for left in TYPES {
                    for right in TYPES {
                        let mut graph = Graph::default();
                        let a = graph.input(0, left, None);
                        let b = graph.input(1, right, None);
                        let Some(id) = graph.binary(result, operation, a, b) else {
                            continue;
                        };
                        if !matches!(graph.scalars[id].operation, Operation::Binary(..)) {
                            continue;
                        }
                        built += 1;
                        let what = format!("{operation:?} {left:?} {right:?} -> {result:?}");
                        fast += usize::from(compare(&mut random, &graph, &[a, b], id, 16, &what));
                    }
                }
            }
        }
        // The count says that the test met the codes and the generic path.
        assert!(fast > 0 && fast < built, "{fast} of {built}");
    }

    #[test]
    fn binary_operations_with_a_literal_match_the_slot_executor() {
        let mut random = Random(0xdead_beef_cafe_f00d);
        for operation in BINARY {
            for result in TYPES {
                for left in TYPES {
                    for (ty, literal) in [
                        (ScalarType::Integer, 0u128),
                        (ScalarType::Integer, 1),
                        (ScalarType::Integer, 31),
                        (ScalarType::Integer, 63),
                        (ScalarType::Integer, 64),
                        (ScalarType::Integer, 200),
                        (ScalarType::Integer, 1 << 70),
                        (ScalarType::Bits(64), 0),
                        (ScalarType::Bits(64), u128::from(u64::MAX)),
                        (ScalarType::Bits(128), u128::MAX),
                    ] {
                        let mut graph = Graph::default();
                        let a = graph.input(0, left, None);
                        let b = graph.constant(ty, literal);
                        let Some(id) = graph.binary(result, operation, a, b) else {
                            continue;
                        };
                        if !matches!(graph.scalars[id].operation, Operation::Binary(..)) {
                            continue;
                        }
                        let what = format!("{operation:?} {left:?} {literal:#x} -> {result:?}");
                        compare(&mut random, &graph, &[a], id, 16, &what);
                    }
                }
            }
        }
    }

    #[test]
    fn unary_operations_match_the_slot_executor() {
        let mut random = Random(0x0123_4567_89ab_cdef);
        let mut fast = 0;
        let mut built = 0;
        for width in [1u16, 7, 8, 16, 24, 32, 63, 64, 65, 100, 128] {
            let operations = [
                Unary::BoolNot,
                Unary::BitNot,
                Unary::Negate,
                Unary::Truncate(width),
                Unary::SignExtend(width),
                Unary::LeadingZeros(width),
                Unary::TrailingZeros(width),
                Unary::ReverseBits(width),
                Unary::ReverseBytes(width),
                Unary::PopCount(width),
            ];
            for operation in operations {
                for result in TYPES {
                    for operand in TYPES {
                        let mut graph = Graph::default();
                        let a = graph.input(0, operand, None);
                        let Some(id) = graph.unary(result, operation, a) else {
                            continue;
                        };
                        if !matches!(graph.scalars[id].operation, Operation::Unary(..)) {
                            continue;
                        }
                        built += 1;
                        let what = format!("{operation:?} {operand:?} -> {result:?}");
                        fast += usize::from(compare(&mut random, &graph, &[a], id, 16, &what));
                    }
                }
            }
        }
        assert!(fast > 0 && fast < built, "{fast} of {built}");
    }

    #[test]
    fn slices_concats_and_selects_match_the_slot_executor() {
        let mut random = Random(0x5151_5151_a0a0_a0a0);
        for operand in TYPES {
            let ScalarType::Bits(from) = operand else {
                continue;
            };
            for offset in [0u16, 1, 8, 31, 32, 33, 63, 64, 65, 96, 127] {
                for width in [1u16, 8, 32, 33, 64, 65, 96, 128] {
                    if offset >= from {
                        continue;
                    }
                    let mut graph = Graph::default();
                    let a = graph.input(0, operand, None);
                    let id = graph.slice(a, offset, width);
                    let what = format!("slice {operand:?} at {offset} of {width}");
                    compare(&mut random, &graph, &[a], id, 16, &what);
                }
            }
        }
        for (high_width, low_width) in [
            (1u16, 1u16),
            (8, 8),
            (32, 32),
            (1, 63),
            (63, 1),
            (64, 64),
            (32, 64),
            (64, 32),
            (96, 32),
            (1, 127),
        ] {
            let mut graph = Graph::default();
            let high = graph.input(0, ScalarType::Bits(high_width), None);
            let low = graph.input(1, ScalarType::Bits(low_width), None);
            let id = graph.concat(high, low, low_width, high_width + low_width);
            let what = format!("concat {high_width} above {low_width}");
            compare(&mut random, &graph, &[high, low], id, 16, &what);
        }
        for ty in TYPES {
            let mut graph = Graph::default();
            let condition = graph.input(0, ScalarType::Bool, None);
            let a = graph.input(1, ty, None);
            let b = graph.input(2, ty, None);
            let id = graph.select(ty, condition, a, b).expect("a select");
            let what = format!("select {ty:?}");
            compare(&mut random, &graph, &[condition, a, b], id, 16, &what);
        }
    }

    #[test]
    fn a_view_shares_the_words_of_its_scalar_or_widens_them() {
        let mut graph = Graph::default();
        let byte = graph.input(0, ScalarType::Bits(8), None);
        let word = graph
            .unary(ScalarType::Bits(64), Unary::Truncate(64), byte)
            .expect("a view");
        let wide = graph
            .unary(ScalarType::Bits(128), Unary::Truncate(128), byte)
            .expect("a view");
        assert!(matches!(
            graph.scalars[word].operation,
            Operation::Refine(_)
        ));
        assert!(matches!(
            graph.scalars[wide].operation,
            Operation::Refine(_)
        ));
        let mut lowering = Lowering::new(&graph).expect("a small block");
        assert_eq!(lowering.place(word), lowering.place(byte));
        assert_ne!(lowering.word(wide), lowering.word(byte));
        lowering.constant(byte, 0xa5);
        lowering.scalar(&graph, word);
        lowering.scalar(&graph, wide);
        assert_eq!(lowering.ops.len(), 1);
        let mut frame = lowering.frame.clone();
        // The second word of the view is zero also after another value.
        frame[usize::from(lowering.word(wide)) + 1] = u64::MAX;
        for op in &lowering.ops {
            assert!(step(op, &mut frame, &lowering.generics));
        }
        assert_eq!(lowering.place(wide).read(&frame), 0xa5);
    }

    #[test]
    fn a_division_by_zero_is_outside_the_domain() {
        let mut graph = Graph::default();
        let a = graph.input(0, ScalarType::Bits(64), None);
        let b = graph.input(1, ScalarType::Bits(64), None);
        let id = graph
            .binary(ScalarType::Bits(64), Binary::DivideUnsigned, a, b)
            .expect("a division");
        assert_eq!(lowered(&graph, &[(a, 7), (b, 0)], id).0, None);
        assert_eq!(lowered(&graph, &[(a, 7), (b, 2)], id).0, Some(3));
    }
}
