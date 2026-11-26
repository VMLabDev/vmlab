//! The builder that the lifters share (DESIGN.md G3b): the registers of one
//! block as scalars of a `native::Graph`, the flags, and the exit.
//!
//! Sources read: none. The carry and overflow rules of `add_with_carry` are
//! the two's complement identities, and the lockstep against QEMU (S6)
//! checks them.

use crate::fields::Condition;
use crate::state::{Context, REGISTER_FILE, slot};
use vmlab_hv_tcg_ir::native::block::{BlockOf, Event, ExitId, ExitScalars};
use vmlab_hv_tcg_ir::native::regfile::Writeback;
use vmlab_hv_tcg_ir::native::{Binary, ScalarId, ScalarType, Unary};

/// The class of an exception that ends a block. The exception cards (C6)
/// add the classes and their syndromes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExceptionClass {
    Undefined,
    /// A load or a store that the memory does not hold.
    DataAbort,
    /// An access to an address that is not a multiple of its size, where the
    /// instruction asks for one. The syndrome is the address.
    Alignment,
}

/// Where a block goes when it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum A64Next {
    /// The next PC.
    Pc(ScalarId),
    /// An exception, with the scalars of its syndrome.
    Exception {
        class: ExceptionClass,
        syndrome: Vec<ScalarId>,
    },
}

/// The exit payload of an aarch64 block (R8): the register slots that the
/// block writes back, and where it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct A64Exit {
    pub writeback: Writeback,
    pub next: A64Next,
    /// The instructions of the block that are complete when it leaves here.
    pub completed: u32,
}

impl ExitScalars for A64Exit {
    fn for_each_scalar(&mut self, visit: &mut dyn FnMut(&mut ScalarId)) {
        self.writeback.for_each_scalar(visit);
        match &mut self.next {
            A64Next::Pc(scalar) => visit(scalar),
            A64Next::Exception { syndrome, .. } => syndrome.iter_mut().for_each(visit),
        }
    }
}

/// N, Z, C and V, each a scalar of one bit.
#[derive(Clone, Copy, Debug)]
pub struct Flags {
    pub n: ScalarId,
    pub z: ScalarId,
    pub c: ScalarId,
    pub v: ScalarId,
}

pub struct Builder {
    block: BlockOf<A64Exit>,
    /// The value of each slot now. `None`: not read and not written yet.
    current: [Option<ScalarId>; slot::COUNT],
    written: [bool; slot::COUNT],
    context: Context,
    /// The address of the instruction that is being lifted.
    pc: u64,
    /// The next PC, when an instruction of the block has set one.
    branch: Option<ScalarId>,
    /// The instructions before the one that is being lifted.
    completed: u32,
    started: bool,
    /// The slots as they were before the instruction that is being lifted:
    /// what an exit inside the instruction writes back.
    before: [Option<ScalarId>; slot::COUNT],
    before_written: [bool; slot::COUNT],
    /// The exit of a failed memory access of this instruction, once made.
    abort: Option<ExitId>,
    /// The memory effects of the block so far. Each has its own number.
    effects: usize,
}

fn total(value: Option<ScalarId>) -> ScalarId {
    value.expect("an operation on bitvectors that the graph always builds")
}

impl Builder {
    pub fn new(context: Context, pc: u64) -> Self {
        Self {
            block: BlockOf::default(),
            current: [None; slot::COUNT],
            written: [false; slot::COUNT],
            context,
            pc,
            branch: None,
            completed: 0,
            started: false,
            before: [None; slot::COUNT],
            before_written: [false; slot::COUNT],
            abort: None,
            effects: 0,
        }
    }

    /// Start the instruction at `pc`. The instruction before it is complete.
    pub fn begin(&mut self, pc: u64) {
        if self.started {
            self.completed += 1;
        }
        self.started = true;
        self.pc = pc;
        self.before = self.current;
        self.before_written = self.written;
        self.abort = None;
    }

    /// Take back the `begin` of an instruction that was not lifted and that
    /// ends the block before it.
    pub fn unbegin(&mut self) {
        debug_assert!(self.completed > 0, "the first instruction of a block");
        self.completed = self.completed.saturating_sub(1);
    }

    /// The address of the instruction that is being lifted.
    pub fn pc(&self) -> u64 {
        self.pc
    }

    // --- slots -----------------------------------------------------------

    pub fn read(&mut self, slot: usize) -> ScalarId {
        if let Some(value) = self.current[slot] {
            return value;
        }
        let value = REGISTER_FILE.read(&mut self.block.graph, slot);
        self.current[slot] = Some(value);
        value
    }

    /// `value` must have the type of the slot.
    pub fn write(&mut self, slot: usize, value: ScalarId) {
        debug_assert_eq!(
            self.block.graph.scalars[value].ty, REGISTER_FILE.slots[slot].ty,
            "slot {}",
            REGISTER_FILE.slots[slot].name
        );
        self.current[slot] = Some(value);
        self.written[slot] = true;
    }

    // --- values ----------------------------------------------------------

    /// A constant of `width` bits.
    pub fn imm(&mut self, width: u16, value: u64) -> ScalarId {
        self.block
            .graph
            .constant(ScalarType::Bits(width), value.into())
    }

    /// The low `width` bits of `value`.
    pub fn low(&mut self, value: ScalarId, width: u16) -> ScalarId {
        self.block.graph.slice(value, 0, width)
    }

    /// `value` with zeros above it, as `width` bits.
    pub fn zero_extend(&mut self, value: ScalarId, width: u16) -> ScalarId {
        if self.block.graph.scalars[value].ty == ScalarType::Bits(width) {
            return value;
        }
        total(
            self.block
                .graph
                .unary(ScalarType::Bits(width), Unary::Truncate(width), value),
        )
    }

    /// A binary operation whose operands and result have `width` bits.
    pub fn binary(&mut self, width: u16, operation: Binary, a: ScalarId, b: ScalarId) -> ScalarId {
        total(
            self.block
                .graph
                .binary(ScalarType::Bits(width), operation, a, b),
        )
    }

    pub fn not(&mut self, width: u16, value: ScalarId) -> ScalarId {
        total(
            self.block
                .graph
                .unary(ScalarType::Bits(width), Unary::BitNot, value),
        )
    }

    fn shift(&mut self, width: u16, operation: Binary, value: ScalarId, amount: u32) -> ScalarId {
        debug_assert!(amount < u32::from(width));
        let amount = self
            .block
            .graph
            .constant(ScalarType::Integer, amount.into());
        self.binary(width, operation, value, amount)
    }

    /// Shift left by a constant below `width`.
    pub fn shl(&mut self, width: u16, value: ScalarId, amount: u32) -> ScalarId {
        self.shift(width, Binary::ShiftLeft, value, amount)
    }

    /// Logical shift right by a constant below `width`.
    pub fn lsr(&mut self, width: u16, value: ScalarId, amount: u32) -> ScalarId {
        self.shift(width, Binary::ShiftRight, value, amount)
    }

    /// Arithmetic shift right by a constant below `width`.
    pub fn asr(&mut self, width: u16, value: ScalarId, amount: u32) -> ScalarId {
        self.shift(width, Binary::ArithmeticShiftRight, value, amount)
    }

    /// Bit `index` of `value`, as one bit.
    pub fn bit(&mut self, value: ScalarId, index: u16) -> ScalarId {
        self.block.graph.slice(value, index, 1)
    }

    /// A Bool as one bit.
    pub fn to_bit(&mut self, condition: ScalarId) -> ScalarId {
        total(
            self.block
                .graph
                .unary(ScalarType::Bits(1), Unary::Truncate(1), condition),
        )
    }

    /// One bit as a Bool.
    pub fn to_bool(&mut self, bit: ScalarId) -> ScalarId {
        let zero = self.imm(1, 0);
        total(
            self.block
                .graph
                .binary(ScalarType::Bool, Binary::NotEqual, bit, zero),
        )
    }

    /// A comparison of two values of the same width, as a Bool.
    pub fn compare(&mut self, operation: Binary, a: ScalarId, b: ScalarId) -> ScalarId {
        total(self.block.graph.binary(ScalarType::Bool, operation, a, b))
    }

    /// Whether `value`, of `width` bits, is zero. A Bool.
    pub fn is_zero(&mut self, width: u16, value: ScalarId) -> ScalarId {
        let zero = self.imm(width, 0);
        self.compare(Binary::Equal, value, zero)
    }

    fn bool_binary(&mut self, operation: Binary, a: ScalarId, b: ScalarId) -> ScalarId {
        total(self.block.graph.binary(ScalarType::Bool, operation, a, b))
    }

    pub fn bool_not(&mut self, value: ScalarId) -> ScalarId {
        total(
            self.block
                .graph
                .unary(ScalarType::Bool, Unary::BoolNot, value),
        )
    }

    /// `when_true` where the Bool `condition` holds, else `when_false`.
    pub fn select(
        &mut self,
        width: u16,
        condition: ScalarId,
        when_true: ScalarId,
        when_false: ScalarId,
    ) -> ScalarId {
        total(
            self.block
                .graph
                .select(ScalarType::Bits(width), condition, when_true, when_false),
        )
    }

    // --- general registers -----------------------------------------------

    /// Register `n` as `width` bits (32 or 64), where 31 is the zero register.
    pub fn x(&mut self, n: u32, width: u16) -> ScalarId {
        if n == 31 {
            return self.imm(width, 0);
        }
        let value = self.read(slot::X0 + n as usize);
        self.low(value, width)
    }

    /// Register `n` as `width` bits, where 31 is the stack pointer.
    pub fn x_or_sp(&mut self, n: u32, width: u16) -> ScalarId {
        if n != 31 {
            return self.x(n, width);
        }
        let value = self.read(self.context.sp_slot());
        self.low(value, width)
    }

    /// Write register `n`, where 31 is the zero register and the write is
    /// dropped. A 32-bit value clears the upper half.
    pub fn set_x(&mut self, n: u32, width: u16, value: ScalarId) {
        debug_assert_eq!(self.block.graph.scalars[value].ty, ScalarType::Bits(width));
        if n == 31 {
            return;
        }
        let value = self.zero_extend(value, 64);
        self.write(slot::X0 + n as usize, value);
    }

    /// Write register `n`, where 31 is the stack pointer.
    pub fn set_x_or_sp(&mut self, n: u32, width: u16, value: ScalarId) {
        if n != 31 {
            return self.set_x(n, width, value);
        }
        debug_assert_eq!(self.block.graph.scalars[value].ty, ScalarType::Bits(width));
        let value = self.zero_extend(value, 64);
        self.write(self.context.sp_slot(), value);
    }

    // --- flags -----------------------------------------------------------

    /// `a + b + carry` in `width` bits, with the flags of the sum. `carry`
    /// is one bit. A subtraction is `a + NOT(b) + 1`. This is the one source
    /// of N, Z, C and V for every add, subtract and compare.
    pub fn add_with_carry(
        &mut self,
        width: u16,
        a: ScalarId,
        b: ScalarId,
        carry: ScalarId,
    ) -> (ScalarId, Flags) {
        let partial = self.binary(width, Binary::Add, a, b);
        let carry_wide = self.zero_extend(carry, width);
        let result = self.binary(width, Binary::Add, partial, carry_wide);
        // The carry out of an unsigned sum shows as a sum below an operand.
        let carry_out = match self.block.graph.literal(carry) {
            Some(0) => self.compare(Binary::LessUnsigned, result, a),
            Some(_) => self.compare(Binary::LessEqualUnsigned, result, a),
            None => {
                let first = self.compare(Binary::LessUnsigned, partial, a);
                let second = self.compare(Binary::LessUnsigned, result, partial);
                self.bool_binary(Binary::Or, first, second)
            }
        };
        // Signed overflow: both operands differ in sign from the result.
        let from_a = self.binary(width, Binary::Xor, a, result);
        let from_b = self.binary(width, Binary::Xor, b, result);
        let both = self.binary(width, Binary::And, from_a, from_b);
        let zero = self.is_zero(width, result);
        let flags = Flags {
            n: self.bit(result, width - 1),
            z: self.to_bit(zero),
            c: self.to_bit(carry_out),
            v: self.bit(both, width - 1),
        };
        (result, flags)
    }

    /// The flags of a logical result: N and Z from it, C and V clear.
    pub fn logical_flags(&mut self, width: u16, result: ScalarId) -> Flags {
        let zero = self.is_zero(width, result);
        Flags {
            n: self.bit(result, width - 1),
            z: self.to_bit(zero),
            c: self.imm(1, 0),
            v: self.imm(1, 0),
        }
    }

    pub fn set_flags(&mut self, flags: Flags) {
        self.write(slot::N, flags.n);
        self.write(slot::Z, flags.z);
        self.write(slot::C, flags.c);
        self.write(slot::V, flags.v);
    }

    /// Whether `condition` holds for the flags now. A Bool.
    pub fn condition(&mut self, condition: Condition) -> ScalarId {
        let flag = |builder: &mut Self, slot: usize| {
            let bit = builder.read(slot);
            builder.to_bool(bit)
        };
        // Each pair is a base condition and its inverse.
        let base = match condition {
            Condition::Eq | Condition::Ne => flag(self, slot::Z),
            Condition::Cs | Condition::Cc => flag(self, slot::C),
            Condition::Mi | Condition::Pl => flag(self, slot::N),
            Condition::Vs | Condition::Vc => flag(self, slot::V),
            Condition::Hi | Condition::Ls => {
                let c = flag(self, slot::C);
                let z = flag(self, slot::Z);
                let not_z = self.bool_not(z);
                self.bool_binary(Binary::And, c, not_z)
            }
            Condition::Ge | Condition::Lt => {
                let n = self.read(slot::N);
                let v = self.read(slot::V);
                self.compare(Binary::Equal, n, v)
            }
            Condition::Gt | Condition::Le => {
                let n = self.read(slot::N);
                let v = self.read(slot::V);
                let same = self.compare(Binary::Equal, n, v);
                let z = flag(self, slot::Z);
                let not_z = self.bool_not(z);
                self.bool_binary(Binary::And, not_z, same)
            }
            Condition::Al | Condition::Nv => {
                return self.block.graph.constant(ScalarType::Bool, 1);
            }
        };
        match condition {
            Condition::Ne
            | Condition::Cc
            | Condition::Pl
            | Condition::Vc
            | Condition::Ls
            | Condition::Lt
            | Condition::Le => self.bool_not(base),
            _ => base,
        }
    }

    // --- memory ------------------------------------------------------------

    /// The slots that differ from their value at the entry of the block.
    fn writes(
        &mut self,
        current: [Option<ScalarId>; slot::COUNT],
        written: [bool; slot::COUNT],
    ) -> Vec<(usize, ScalarId)> {
        let mut writes = Vec::new();
        for slot in 0..slot::COUNT {
            let Some(value) = current[slot] else {
                continue;
            };
            if !written[slot] {
                continue;
            }
            // A slot that holds its own entry value again needs no write.
            let entry = REGISTER_FILE.read(&mut self.block.graph, slot);
            if value != entry {
                writes.push((slot, value));
            }
        }
        writes
    }

    /// An exit that leaves the block before the instruction that is being
    /// lifted: the registers as they were before it, and its address as the
    /// PC.
    fn exit_before(&mut self, class: ExceptionClass, syndrome: Vec<ScalarId>) -> ExitId {
        let mut writes = self.writes(self.before, self.before_written);
        let pc = self.imm(64, self.pc);
        writes.push((slot::PC, pc));
        self.block.exit(A64Exit {
            writeback: Writeback { writes },
            next: A64Next::Exception { class, syndrome },
            completed: self.completed,
        })
    }

    fn abort_exit(&mut self) -> ExitId {
        if let Some(exit) = self.abort {
            return exit;
        }
        let exit = self.exit_before(ExceptionClass::DataAbort, Vec::new());
        self.abort = Some(exit);
        exit
    }

    fn effect(&mut self, width: u16) -> ScalarId {
        let effect = self.effects;
        self.effects += 1;
        self.block.graph.effect(effect, ScalarType::Bits(width))
    }

    fn read_event(&mut self, address: ScalarId, bytes: u8, exclusive: bool) -> ScalarId {
        debug_assert!(matches!(bytes, 1 | 2 | 4 | 8 | 16));
        let error = self.abort_exit();
        let result = self.effect(u16::from(bytes) * 8);
        self.block.events.push(Event::ReadMemory {
            address,
            bytes,
            exclusive,
            result,
            error,
        });
        result
    }

    fn write_event(
        &mut self,
        address: ScalarId,
        bytes: u8,
        value: ScalarId,
        exclusive: bool,
    ) -> Option<ScalarId> {
        debug_assert!(matches!(bytes, 1 | 2 | 4 | 8 | 16));
        debug_assert_eq!(
            self.block.graph.scalars[value].ty,
            ScalarType::Bits(u16::from(bytes) * 8)
        );
        let error = self.abort_exit();
        let result = exclusive.then(|| self.effect(1));
        self.block.events.push(Event::WriteMemory {
            address,
            value,
            bytes,
            exclusive,
            result,
            error,
            // The executor leaves through `error` before a store into a page
            // that holds translated code, and runs the instruction alone.
            invalidated: error,
        });
        result
    }

    /// The `bytes` bytes at `address`, little-endian, as `bytes * 8` bits.
    /// `bytes` is 1, 2, 4, 8 or 16.
    pub fn load(&mut self, address: ScalarId, bytes: u8) -> ScalarId {
        self.read_event(address, bytes, false)
    }

    /// Store `value`, of `bytes * 8` bits, at `address`, little-endian.
    pub fn store(&mut self, address: ScalarId, bytes: u8, value: ScalarId) {
        self.write_event(address, bytes, value, false);
    }

    /// A load that also marks the address for exclusive access.
    pub fn load_exclusive(&mut self, address: ScalarId, bytes: u8) -> ScalarId {
        self.read_event(address, bytes, true)
    }

    /// A store that writes only when the exclusive mark of this CPU still
    /// covers the address. The result is one bit: 0 where it wrote, 1 where
    /// it did not.
    pub fn store_exclusive(&mut self, address: ScalarId, bytes: u8, value: ScalarId) -> ScalarId {
        self.write_event(address, bytes, value, true)
            .expect("an exclusive store has a result")
    }

    /// Leave the block with an alignment fault before this instruction,
    /// unless `address` is a multiple of `bytes`, a power of two.
    pub fn require_aligned(&mut self, address: ScalarId, bytes: u8) {
        if bytes == 1 {
            return;
        }
        let mask = self.imm(64, u64::from(bytes) - 1);
        let low = self.binary(64, Binary::And, address, mask);
        let aligned = self.is_zero(64, low);
        if self.block.graph.literal(aligned) == Some(1) {
            return;
        }
        let exit = self.exit_before(ExceptionClass::Alignment, vec![address]);
        self.block.events.push(Event::Guard {
            condition: aligned,
            expected: true,
            exit,
        });
    }

    // --- vector registers ----------------------------------------------------

    /// The low `width` bits of the vector register `n`.
    pub fn v(&mut self, n: u32, width: u16) -> ScalarId {
        let value = self.read(slot::V0 + (n & 31) as usize);
        self.low(value, width)
    }

    /// Write the vector register `n`. A value below 128 bits clears the bits
    /// above it.
    pub fn set_v(&mut self, n: u32, value: ScalarId) {
        let value = self.zero_extend(value, 128);
        self.write(slot::V0 + (n & 31) as usize, value);
    }

    /// `value`, of `from` bits, sign-extended to `width` bits.
    pub fn sign_extend(&mut self, value: ScalarId, from: u16, width: u16) -> ScalarId {
        debug_assert_eq!(self.block.graph.scalars[value].ty, ScalarType::Bits(from));
        if from == width {
            return value;
        }
        total(
            self.block
                .graph
                .unary(ScalarType::Bits(width), Unary::SignExtend(from), value),
        )
    }

    /// The bits `offset` to `offset + width` of `value`.
    pub fn slice(&mut self, value: ScalarId, offset: u16, width: u16) -> ScalarId {
        self.block.graph.slice(value, offset, width)
    }

    /// `high` above `low`. `low` has `low_width` bits and the result `width`.
    pub fn concat(
        &mut self,
        high: ScalarId,
        low: ScalarId,
        low_width: u16,
        width: u16,
    ) -> ScalarId {
        debug_assert_eq!(
            self.block.graph.scalars[low].ty,
            ScalarType::Bits(low_width)
        );
        debug_assert_eq!(
            self.block.graph.scalars[high].ty,
            ScalarType::Bits(width - low_width)
        );
        self.block.graph.concat(high, low, low_width, width)
    }

    // --- the end of the block ----------------------------------------------

    /// Set the next PC. The instruction that calls this ends the block.
    pub fn branch(&mut self, target: ScalarId) {
        debug_assert_eq!(
            self.block.graph.scalars[target].ty,
            ScalarType::Bits(64),
            "a branch target is 64 bits"
        );
        self.branch = Some(target);
    }

    /// End the block. `fallthrough` is the next PC when no instruction set one.
    pub fn finish(mut self, fallthrough: u64) -> BlockOf<A64Exit> {
        let next = match self.branch {
            Some(target) => target,
            None => self.imm(64, fallthrough),
        };
        let writes = self.writes(self.current, self.written);
        let completed = self.completed + u32::from(self.started);
        let exit = self.block.exit(A64Exit {
            writeback: Writeback { writes },
            next: A64Next::Pc(next),
            completed,
        });
        self.block.events.push(Event::Exit(exit));
        self.block
    }
}
