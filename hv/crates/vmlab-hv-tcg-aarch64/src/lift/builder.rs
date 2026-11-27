//! The builder that the lifters share (DESIGN.md G3b): the registers of one
//! block as scalars of a `native::Graph`, the flags, and the exit.
//!
//! Sources read: none. The carry and overflow rules of `add_with_carry` are
//! the two's complement identities, and the lockstep against QEMU (S6)
//! checks them.

use crate::exception::Exception;
use crate::fields::Condition;
use crate::flags;
use crate::state::{Context, REGISTER_FILE, slot};
use vmlab_hv_tcg_ir::native::block::{BlockOf, Event, ExitId, ExitScalars};
use vmlab_hv_tcg_ir::native::regfile::Writeback;
use vmlab_hv_tcg_ir::native::{Binary, Helper, HelperRef, ScalarId, ScalarType, Unary};

/// A PSTATE field that `MSR` writes with an immediate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PstateField {
    SpSel,
    DaifSet,
    DaifClr,
}

/// What the last instruction of a block does outside the graph (DESIGN.md
/// G3d): the loop runs it in Rust after the registers of the block are
/// written back, with the PC at the instruction. An instruction that can
/// take an exception, change the level or write a system register ends its
/// block this way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    /// The instruction takes this exception.
    Exception(Exception),
    /// A word that no selected record matches: the Undefined Instruction
    /// exception, and a line in the log of the CPU (G3f). A machine can
    /// claim the word first (A5).
    Unknown(u32),
    Hvc(u16),
    Smc(u16),
    Eret,
    Wfi,
    Wfe,
    /// `CLREX`: the exclusive mark of the CPU ends (card C3).
    ClearExclusive,
    /// `MRS Xt, <register>`, the register by its encoding.
    Mrs {
        encoding: u16,
        rt: u8,
    },
    /// `MSR <register>, Xt`.
    Msr {
        encoding: u16,
        rt: u8,
    },
    MsrPstate {
        field: PstateField,
        immediate: u8,
    },
    /// `SYS` (`read` false) or `SYSL`: a cache, TLB or address translation
    /// operation (card C8). `encoding`: op1:CRn:CRm:op2, as the LLVM tables
    /// key their operations.
    Sys {
        encoding: u16,
        rt: u8,
        read: bool,
    },
}

/// The class of an exception that leaves a block before its end, at a load
/// or a store (card C3). The loop takes the data abort (card C8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExceptionClass {
    Undefined,
    /// A load or a store that cannot be made. `unprivileged`: the accesses
    /// of the instruction are made as EL0 (`LDTR`, `STTR`).
    DataAbort {
        unprivileged: bool,
    },
    /// An access to an address that is not a multiple of its size. The
    /// syndrome is the address. `always`: the instruction asks for the
    /// alignment (an exclusive access). Otherwise the memory that the
    /// address maps to decides, and SCTLR_EL1.A.
    Alignment {
        write: bool,
        always: bool,
        unprivileged: bool,
        /// The alignment in bytes that Device memory asks of this access. 1:
        /// none.
        device: u8,
    },
}

/// Where a block goes when it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum A64Next {
    /// The next PC.
    Pc(ScalarId),
    /// An exception that leaves the block before one of its instructions,
    /// with the scalars of its syndrome.
    Exception {
        class: ExceptionClass,
        syndrome: Vec<ScalarId>,
    },
    /// The instruction at `pc` is the last of the block, and the loop runs
    /// it.
    Trap { pc: u64, trap: Trap },
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
            A64Next::Trap { .. } => {}
        }
    }
}

/// N, Z, C and V, each a scalar of one bit.
#[derive(Clone, Copy, Debug)]
struct FlagBits {
    n: ScalarId,
    z: ScalarId,
    c: ScalarId,
    v: ScalarId,
}

/// `a + b + carry` of `width` bits, or `a + NOT(b) + carry` when
/// `subtract`, and its `result`: what the flags of a sum follow from.
#[derive(Clone, Copy, Debug)]
pub struct Sum {
    width: u16,
    a: ScalarId,
    b: ScalarId,
    carry: ScalarId,
    subtract: bool,
    result: ScalarId,
}

/// The flags that an instruction sets, as what they follow from (card
/// #1142). They are computed only where a condition, `MRS NZCV` or the
/// Rust side reads them.
#[derive(Clone, Copy, Debug)]
pub enum Flags {
    Sum(Sum),
    /// N and Z of `result`, C and V clear.
    Logical {
        width: u16,
        result: ScalarId,
    },
    /// NZCV as four bits, N the highest.
    Explicit(ScalarId),
    /// The flags of `sum` where the Bool `holds`, else `otherwise` (NZCV).
    Chosen {
        holds: ScalarId,
        sum: Sum,
        otherwise: u8,
    },
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
    /// What the last instruction does outside the graph.
    trap: Option<Trap>,
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
    /// The addresses of this instruction that an alignment guard covers,
    /// each with the size that it was checked for.
    aligned: Vec<(ScalarId, u8)>,
    /// The accesses of this instruction are made as EL0.
    unprivileged: bool,
    /// The flags as the block set them last. `None`: the block has not set
    /// them, and they are in the flag slots.
    flags: Option<Flags>,
}

/// The alignment that Device memory asks of a load or a store of a register
/// whose own alignment is `align` bytes. What the lockstep found on QEMU
/// (card C8): the size of the register, also for one of 16 bytes.
fn device_alignment(align: u8) -> u8 {
    align
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
            trap: None,
            completed: 0,
            started: false,
            before: [None; slot::COUNT],
            before_written: [false; slot::COUNT],
            abort: None,
            effects: 0,
            aligned: Vec::new(),
            unprivileged: false,
            flags: None,
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
        self.aligned.clear();
        self.unprivileged = false;
    }

    /// The loads and stores of the instruction that is being lifted are
    /// made as EL0, whatever the level (`LDTR`, `STTR`). Call it before the
    /// first access.
    pub fn unprivileged(&mut self) {
        debug_assert!(self.abort.is_none(), "after an access of the instruction");
        self.unprivileged = true;
    }

    /// Take back the `begin` of an instruction that was not lifted and that
    /// ends the block before it.
    pub fn unbegin(&mut self) {
        debug_assert!(self.completed > 0, "the first instruction of a block");
        self.completed = self.completed.saturating_sub(1);
    }

    /// What the instruction has written to its registers so far stays
    /// written when a later access of it faults. A load of several vector
    /// registers calls this after each: QEMU writes them as it goes, and
    /// the registers of a load that faults are whatever the CPU left in
    /// them (DESIGN.md U1).
    pub fn commit(&mut self) {
        self.before = self.current;
        self.before_written = self.written;
        self.abort = None;
    }

    /// The address of the instruction that is being lifted.
    pub fn pc(&self) -> u64 {
        self.pc
    }

    /// The level, the stack pointer choice and the FP enable that the block
    /// is lifted for.
    pub fn context(&self) -> Context {
        self.context
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

    /// Shift by an amount that is a scalar. The executor gives zero for a
    /// logical shift by `width` or more, and the sign for an arithmetic one.
    pub fn shl_by(&mut self, width: u16, value: ScalarId, amount: ScalarId) -> ScalarId {
        self.binary(width, Binary::ShiftLeft, value, amount)
    }

    pub fn lsr_by(&mut self, width: u16, value: ScalarId, amount: ScalarId) -> ScalarId {
        self.binary(width, Binary::ShiftRight, value, amount)
    }

    pub fn asr_by(&mut self, width: u16, value: ScalarId, amount: ScalarId) -> ScalarId {
        self.binary(width, Binary::ArithmeticShiftRight, value, amount)
    }

    /// Rotate right by a constant below `width`.
    pub fn ror(&mut self, width: u16, value: ScalarId, amount: u32) -> ScalarId {
        if amount == 0 {
            return value;
        }
        let low = self.lsr(width, value, amount);
        let high = self.shl(width, value, u32::from(width) - amount);
        self.binary(width, Binary::Or, low, high)
    }

    /// The low `from` bits of `value` with their sign above them, as `width`
    /// bits. `from` is at most `width`.
    pub fn sign_extend(&mut self, value: ScalarId, from: u16, width: u16) -> ScalarId {
        let low = self.low(value, from);
        if from == width {
            return low;
        }
        total(
            self.block
                .graph
                .unary(ScalarType::Bits(width), Unary::SignExtend(from), low),
        )
    }

    /// A unary operation on `width` bits with a result of `width` bits.
    pub fn unary(&mut self, width: u16, operation: Unary, value: ScalarId) -> ScalarId {
        total(
            self.block
                .graph
                .unary(ScalarType::Bits(width), operation, value),
        )
    }

    /// `0 - value` in `width` bits.
    pub fn negate(&mut self, width: u16, value: ScalarId) -> ScalarId {
        let zero = self.imm(width, 0);
        self.binary(width, Binary::Sub, zero, value)
    }

    /// Bits `offset` and up of `value`, as `width` bits.
    pub fn slice(&mut self, value: ScalarId, offset: u16, width: u16) -> ScalarId {
        self.block.graph.slice(value, offset, width)
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

    // --- FP and vector registers -------------------------------------------

    /// `high` above the `low_width` bits of `low`, as `width` bits.
    pub fn concat(
        &mut self,
        high: ScalarId,
        low: ScalarId,
        low_width: u16,
        width: u16,
    ) -> ScalarId {
        self.block.graph.concat(high, low, low_width, width)
    }

    /// The low `width` bits of the register V`n`: 32 for S`n`, 64 for D`n`,
    /// 128 for the whole register.
    pub fn v(&mut self, n: u32, width: u16) -> ScalarId {
        let value = self.read(slot::V0 + n as usize);
        self.low(value, width)
    }

    /// Write the low `width` bits of V`n`. The bits above them become zero,
    /// as every scalar write of an FP register does.
    pub fn set_v(&mut self, n: u32, width: u16, value: ScalarId) {
        debug_assert_eq!(self.block.graph.scalars[value].ty, ScalarType::Bits(width));
        let value = self.zero_extend(value, 128);
        self.write(slot::V0 + n as usize, value);
    }

    /// The result of the pure helper `helper` on `args`, as `width` bits
    /// (R10). The lifter gives the count of arguments that the helper names.
    pub fn call(&mut self, width: u16, helper: &'static Helper, args: &[ScalarId]) -> ScalarId {
        self.block
            .graph
            .call(ScalarType::Bits(width), helper, args)
            .expect("a pure helper with its count of arguments")
    }

    /// The result of the ordered helper `helper` on `args`, as `width` bits
    /// (R10): the call runs once, at this place among the events of the
    /// block, and sees the state of the CPU that is not in a register slot.
    pub fn call_ordered(
        &mut self,
        width: u16,
        helper: &'static Helper,
        args: &[ScalarId],
    ) -> ScalarId {
        debug_assert_eq!(usize::from(helper.args), args.len());
        let result = self.effect(width);
        self.block.events.push(Event::Call {
            helper: HelperRef(helper),
            args: args.to_vec(),
            results: vec![result],
        });
        result
    }

    /// FPCR now.
    pub fn fpcr(&mut self) -> ScalarId {
        self.read(slot::FPCR)
    }

    /// Set the cumulative exception bits `bits` in FPSR. `bits` has eight
    /// bits, in the positions of FPSR.
    pub fn raise(&mut self, bits: ScalarId) {
        let bits = self.zero_extend(bits, 32);
        let old = self.read(slot::FPSR);
        let new = self.binary(32, Binary::Or, old, bits);
        self.write(slot::FPSR, new);
    }

    // --- flags -----------------------------------------------------------

    /// `a + b + carry` in `width` bits, with the flags of the sum. `carry`
    /// is one bit. This and `sub_with_carry` are the one source of N, Z, C
    /// and V for every add, subtract and compare. The flags are lazy: they
    /// are computed only where something reads them (card #1142).
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
        let sum = Sum {
            width,
            a,
            b,
            carry,
            subtract: false,
            result,
        };
        (result, Flags::Sum(sum))
    }

    /// `a + NOT(b) + carry` in `width` bits, with its flags: `a - b` when
    /// the carry is set.
    pub fn sub_with_carry(
        &mut self,
        width: u16,
        a: ScalarId,
        b: ScalarId,
        carry: ScalarId,
    ) -> (ScalarId, Flags) {
        let inverted = self.not(width, b);
        let (result, _) = self.add_with_carry(width, a, inverted, carry);
        let sum = Sum {
            width,
            a,
            b,
            carry,
            subtract: true,
            result,
        };
        (result, Flags::Sum(sum))
    }

    /// The flags of a logical result: N and Z from it, C and V clear.
    pub fn logical_flags(&mut self, width: u16, result: ScalarId) -> Flags {
        Flags::Logical { width, result }
    }

    /// `compared` where the Bool `holds`, else the flags `otherwise` (NZCV
    /// as the bits 3 to 0).
    pub fn chosen_flags(&mut self, holds: ScalarId, compared: Flags, otherwise: u8) -> Flags {
        match compared {
            Flags::Sum(sum) => Flags::Chosen {
                holds,
                sum,
                otherwise,
            },
            compared => {
                let bits = self.nzcv_of(Some(compared));
                let otherwise = self.imm(4, otherwise.into());
                Flags::Explicit(self.select(4, holds, bits, otherwise))
            }
        }
    }

    /// Set the flags: their kind and operands go into the flag slots, and
    /// the conditions of the rest of the block are lifted from them.
    pub fn set_flags(&mut self, flags: Flags) {
        match flags {
            Flags::Sum(sum) => {
                let (kind, a, b) = self.sum_slots(sum);
                self.write(slot::FLAGS_KIND, kind);
                self.write(slot::FLAGS_A, a);
                self.write(slot::FLAGS_B, b);
            }
            Flags::Logical { width, result } => {
                let kind = self.imm(4, flags::kind(flags::LOGICAL, width).into());
                let a = self.zero_extend(result, 64);
                self.write(slot::FLAGS_KIND, kind);
                self.write(slot::FLAGS_A, a);
            }
            Flags::Explicit(bits) => {
                let kind = self.imm(4, flags::EXPLICIT.into());
                let a = self.zero_extend(bits, 64);
                self.write(slot::FLAGS_KIND, kind);
                self.write(slot::FLAGS_A, a);
            }
            Flags::Chosen {
                holds,
                sum,
                otherwise,
            } => {
                let (kind, a, b) = self.sum_slots(sum);
                let explicit = self.imm(4, flags::EXPLICIT.into());
                let kind = self.select(4, holds, kind, explicit);
                let otherwise = self.imm(64, otherwise.into());
                let a = self.select(64, holds, a, otherwise);
                self.write(slot::FLAGS_KIND, kind);
                self.write(slot::FLAGS_A, a);
                self.write(slot::FLAGS_B, b);
            }
        }
        self.flags = Some(flags);
    }

    /// The flag slots of a sum: its kind and its two operands.
    fn sum_slots(&mut self, sum: Sum) -> (ScalarId, ScalarId, ScalarId) {
        let operation = if sum.subtract { flags::SUB } else { flags::ADD };
        let base = flags::kind(operation, sum.width);
        let kind = match self.block.graph.literal(sum.carry) {
            Some(carry) => self.imm(
                4,
                u64::from(base | if carry != 0 { flags::CARRY } else { 0 }),
            ),
            None => {
                let base = self.imm(4, base.into());
                let carry = self.zero_extend(sum.carry, 4);
                let carry = self.shl(4, carry, 3);
                self.binary(4, Binary::Or, base, carry)
            }
        };
        let a = self.zero_extend(sum.a, 64);
        let b = self.zero_extend(sum.b, 64);
        (kind, a, b)
    }

    /// The flags now as NZCV in four bits.
    pub fn nzcv(&mut self) -> ScalarId {
        self.nzcv_of(self.flags)
    }

    /// The flags as four bits: of `flags`, or of the flag slots when the
    /// block has not set them.
    fn nzcv_of(&mut self, flags: Option<Flags>) -> ScalarId {
        match flags {
            None => {
                let kind = self.read(slot::FLAGS_KIND);
                let a = self.read(slot::FLAGS_A);
                let b = self.read(slot::FLAGS_B);
                self.call(4, &flags::NZCV, &[kind, a, b])
            }
            Some(Flags::Explicit(bits)) => bits,
            Some(Flags::Logical { width, result }) => {
                let n = self.bit(result, width - 1);
                let zero = self.is_zero(width, result);
                let z = self.to_bit(zero);
                let clear = self.imm(2, 0);
                let nz = self.concat(n, z, 1, 2);
                self.concat(nz, clear, 2, 4)
            }
            Some(Flags::Sum(sum)) => {
                let bits = self.sum_bits(sum);
                self.pack(bits)
            }
            Some(Flags::Chosen {
                holds,
                sum,
                otherwise,
            }) => {
                let bits = self.sum_bits(sum);
                let bits = self.pack(bits);
                let otherwise = self.imm(4, otherwise.into());
                self.select(4, holds, bits, otherwise)
            }
        }
    }

    /// N, Z, C and V as four bits, N the highest.
    fn pack(&mut self, bits: FlagBits) -> ScalarId {
        let nz = self.concat(bits.n, bits.z, 1, 2);
        let cv = self.concat(bits.c, bits.v, 1, 2);
        self.concat(nz, cv, 2, 4)
    }

    /// The four flags of a sum, each one bit.
    fn sum_bits(&mut self, sum: Sum) -> FlagBits {
        let Sum {
            width,
            a,
            b,
            carry,
            subtract,
            result,
        } = sum;
        let b = if subtract { self.not(width, b) } else { b };
        let partial = self.binary(width, Binary::Add, a, b);
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
        FlagBits {
            n: self.bit(result, width - 1),
            z: self.to_bit(zero),
            c: self.to_bit(carry_out),
            v: self.bit(both, width - 1),
        }
    }

    /// Whether `condition` holds for the flags now. A Bool.
    pub fn condition(&mut self, condition: Condition) -> ScalarId {
        if matches!(condition, Condition::Al | Condition::Nv) {
            return self.block.graph.constant(ScalarType::Bool, 1);
        }
        match self.flags {
            None => {
                let kind = self.read(slot::FLAGS_KIND);
                let a = self.read(slot::FLAGS_A);
                let b = self.read(slot::FLAGS_B);
                let code = self.imm(4, condition as u64);
                self.block
                    .graph
                    .call(ScalarType::Bool, &flags::CONDITION, &[kind, a, b, code])
                    .expect("a pure helper with its count of arguments")
            }
            Some(Flags::Explicit(bits)) => {
                let flags = FlagBits {
                    n: self.bit(bits, 3),
                    z: self.bit(bits, 2),
                    c: self.bit(bits, 1),
                    v: self.bit(bits, 0),
                };
                self.condition_of_bits(condition, flags)
            }
            Some(Flags::Logical { width, result }) => {
                self.condition_of_logical(condition, width, result)
            }
            Some(Flags::Sum(sum)) => self.condition_of_sum(condition, sum),
            Some(Flags::Chosen {
                holds,
                sum,
                otherwise,
            }) => {
                let compared = self.condition_of_sum(condition, sum);
                if flags::holds(condition as u8, otherwise) {
                    let not_holds = self.bool_not(holds);
                    self.bool_binary(Binary::Or, not_holds, compared)
                } else {
                    self.bool_binary(Binary::And, holds, compared)
                }
            }
        }
    }

    /// `value` with its sign bit flipped: an unsigned order of two flipped
    /// values is the signed order of the values.
    fn flip_sign(&mut self, width: u16, value: ScalarId) -> ScalarId {
        let sign = self.imm(width, 1 << (width - 1));
        self.binary(width, Binary::Xor, value, sign)
    }

    /// `a < b` (or `a <= b`) for signed values of `width` bits. A Bool.
    fn signed_less(&mut self, width: u16, a: ScalarId, b: ScalarId, or_equal: bool) -> ScalarId {
        let a = self.flip_sign(width, a);
        let b = self.flip_sign(width, b);
        let operation = if or_equal {
            Binary::LessEqualUnsigned
        } else {
            Binary::LessUnsigned
        };
        self.compare(operation, a, b)
    }

    /// Whether `value` is negative as `width` bits. A Bool.
    fn negative(&mut self, width: u16, value: ScalarId) -> ScalarId {
        let sign = self.imm(width, 1 << (width - 1));
        self.compare(Binary::LessEqualUnsigned, sign, value)
    }

    fn condition_of_logical(
        &mut self,
        condition: Condition,
        width: u16,
        result: ScalarId,
    ) -> ScalarId {
        let constant = |builder: &mut Self, value: bool| {
            builder
                .block
                .graph
                .constant(ScalarType::Bool, u128::from(value))
        };
        let zero = self.imm(width, 0);
        match condition {
            Condition::Eq => self.compare(Binary::Equal, result, zero),
            Condition::Ne => self.compare(Binary::NotEqual, result, zero),
            // N == V with V clear: not negative.
            Condition::Mi | Condition::Lt => self.negative(width, result),
            Condition::Pl | Condition::Ge => {
                let sign = self.imm(width, 1 << (width - 1));
                self.compare(Binary::LessUnsigned, result, sign)
            }
            Condition::Gt => self.signed_less(width, zero, result, false),
            Condition::Le => self.signed_less(width, result, zero, true),
            // C and V are clear.
            Condition::Cs | Condition::Vs | Condition::Hi => constant(self, false),
            Condition::Cc | Condition::Vc | Condition::Ls | Condition::Al | Condition::Nv => {
                constant(self, true)
            }
        }
    }

    fn condition_of_sum(&mut self, condition: Condition, sum: Sum) -> ScalarId {
        let Sum {
            width,
            a,
            b,
            carry,
            subtract,
            result,
        } = sum;
        let carry = self.block.graph.literal(carry);
        // A compare: `a - b`, where every condition but V is an order of
        // the two operands.
        if subtract && carry == Some(1) {
            let direct = match condition {
                Condition::Eq => Some(self.compare(Binary::Equal, a, b)),
                Condition::Ne => Some(self.compare(Binary::NotEqual, a, b)),
                Condition::Cs => Some(self.compare(Binary::LessEqualUnsigned, b, a)),
                Condition::Cc => Some(self.compare(Binary::LessUnsigned, a, b)),
                Condition::Hi => Some(self.compare(Binary::LessUnsigned, b, a)),
                Condition::Ls => Some(self.compare(Binary::LessEqualUnsigned, a, b)),
                Condition::Ge => Some(self.signed_less(width, b, a, true)),
                Condition::Lt => Some(self.signed_less(width, a, b, false)),
                Condition::Gt => Some(self.signed_less(width, b, a, false)),
                Condition::Le => Some(self.signed_less(width, a, b, true)),
                _ => None,
            };
            if let Some(direct) = direct {
                return direct;
            }
        }
        let zero = self.imm(width, 0);
        match condition {
            Condition::Eq => return self.compare(Binary::Equal, result, zero),
            Condition::Ne => return self.compare(Binary::NotEqual, result, zero),
            Condition::Mi => return self.negative(width, result),
            Condition::Pl => {
                let sign = self.imm(width, 1 << (width - 1));
                return self.compare(Binary::LessUnsigned, result, sign);
            }
            // An add without a carry in carries out where the sum is below
            // an operand.
            Condition::Cs if !subtract && carry == Some(0) => {
                return self.compare(Binary::LessUnsigned, result, a);
            }
            Condition::Cc if !subtract && carry == Some(0) => {
                return self.compare(Binary::LessEqualUnsigned, a, result);
            }
            _ => {}
        }
        let bits = self.sum_bits(sum);
        self.condition_of_bits(condition, bits)
    }

    /// Whether `condition` holds for four flags of one bit each.
    fn condition_of_bits(&mut self, condition: Condition, flags: FlagBits) -> ScalarId {
        let flag = |builder: &mut Self, bit: ScalarId| builder.to_bool(bit);
        // Each pair is a base condition and its inverse.
        let base = match condition {
            Condition::Eq | Condition::Ne => flag(self, flags.z),
            Condition::Cs | Condition::Cc => flag(self, flags.c),
            Condition::Mi | Condition::Pl => flag(self, flags.n),
            Condition::Vs | Condition::Vc => flag(self, flags.v),
            Condition::Hi | Condition::Ls => {
                let c = flag(self, flags.c);
                let z = flag(self, flags.z);
                let not_z = self.bool_not(z);
                self.bool_binary(Binary::And, c, not_z)
            }
            Condition::Ge | Condition::Lt => self.compare(Binary::Equal, flags.n, flags.v),
            Condition::Gt | Condition::Le => {
                let same = self.compare(Binary::Equal, flags.n, flags.v);
                let z = flag(self, flags.z);
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
        let class = ExceptionClass::DataAbort {
            unprivileged: self.unprivileged,
        };
        let exit = self.exit_before(class, Vec::new());
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
    /// `bytes` is 1, 2, 4, 8 or 16. The address must be a multiple of
    /// `bytes` where the memory asks for that.
    pub fn load(&mut self, address: ScalarId, bytes: u8) -> ScalarId {
        self.load_aligned(address, bytes, bytes)
    }

    /// As `load`, for an access of several elements of `align` bytes each:
    /// the address must be a multiple of `align`.
    pub fn load_aligned(&mut self, address: ScalarId, bytes: u8, align: u8) -> ScalarId {
        self.require_aligned(address, align, device_alignment(align), false, false);
        self.read_event(address, bytes, false)
    }

    /// As `load_aligned`, for a load of whole vector registers as a
    /// structure (`LD1` to `LD4`): Device memory asks no alignment of it.
    /// It does of the forms of one lane and of the replicating ones.
    pub fn load_structure(&mut self, address: ScalarId, bytes: u8, align: u8) -> ScalarId {
        self.require_aligned(address, align, 1, false, false);
        self.read_event(address, bytes, false)
    }

    /// Store `value`, of `bytes * 8` bits, at `address`, little-endian.
    pub fn store(&mut self, address: ScalarId, bytes: u8, value: ScalarId) {
        self.store_aligned(address, bytes, value, bytes);
    }

    /// As `store`, for an access of several elements of `align` bytes each.
    pub fn store_aligned(&mut self, address: ScalarId, bytes: u8, value: ScalarId, align: u8) {
        self.require_aligned(address, align, device_alignment(align), true, false);
        self.write_event(address, bytes, value, false);
    }

    /// As `store_aligned`, for a store of whole vector registers as a
    /// structure (`ST1` to `ST4`).
    pub fn store_structure(&mut self, address: ScalarId, bytes: u8, value: ScalarId, align: u8) {
        self.require_aligned(address, align, 1, true, false);
        self.write_event(address, bytes, value, false);
    }

    /// A load that also marks the address for exclusive access. The address
    /// must be a multiple of `bytes` on every memory.
    pub fn load_exclusive(&mut self, address: ScalarId, bytes: u8) -> ScalarId {
        self.require_aligned(address, bytes, bytes, false, true);
        self.read_event(address, bytes, true)
    }

    /// A store that writes only when the exclusive mark of this CPU still
    /// covers the address. The result is one bit: 0 where it wrote, 1 where
    /// it did not. A store that does not write looks at no memory and at no
    /// alignment, as QEMU's does not (S6). One that writes has the address
    /// and the size of its exclusive load, which was aligned.
    pub fn store_exclusive(&mut self, address: ScalarId, bytes: u8, value: ScalarId) -> ScalarId {
        self.write_event(address, bytes, value, true)
            .expect("an exclusive store has a result")
    }

    /// Leave the block with an alignment fault before this instruction,
    /// unless `address` is a multiple of `bytes`, a power of two. `always`:
    /// the instruction asks for the alignment. Otherwise the fault is taken
    /// only where something asks for it: SCTLR_EL1.A or the MMU being off,
    /// which ask for `bytes`, or Device memory at the address, which asks
    /// for `device` bytes. The loop decides that when the guard fails.
    pub fn require_aligned(
        &mut self,
        address: ScalarId,
        bytes: u8,
        device: u8,
        write: bool,
        always: bool,
    ) {
        if bytes == 1
            || self
                .aligned
                .iter()
                .any(|(checked, size)| *checked == address && *size >= bytes)
        {
            return;
        }
        self.aligned.push((address, bytes));
        let mask = self.imm(64, u64::from(bytes) - 1);
        let low = self.binary(64, Binary::And, address, mask);
        let aligned = self.is_zero(64, low);
        if self.block.graph.literal(aligned) == Some(1) {
            return;
        }
        let class = ExceptionClass::Alignment {
            write,
            always,
            unprivileged: self.unprivileged,
            device,
        };
        let exit = self.exit_before(class, vec![address]);
        self.block.events.push(Event::Guard {
            condition: aligned,
            expected: true,
            exit,
        });
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

    /// The instruction that is being lifted is the last of the block, and
    /// the loop runs `trap` for it.
    pub fn trap(&mut self, trap: Trap) {
        self.trap = Some(trap);
    }

    /// End the block. `fallthrough` is the next PC when no instruction set one.
    pub fn finish(mut self, fallthrough: u64) -> BlockOf<A64Exit> {
        let next = match (self.trap, self.branch) {
            (Some(trap), _) => A64Next::Trap { pc: self.pc, trap },
            (None, Some(target)) => A64Next::Pc(target),
            (None, None) => A64Next::Pc(self.imm(64, fallthrough)),
        };
        let writes = self.writes(self.current, self.written);
        let completed = self.completed + u32::from(self.started);
        let exit = self.block.exit(A64Exit {
            writeback: Writeback { writes },
            next,
            completed,
        });
        self.block.events.push(Event::Exit(exit));
        self.block
    }
}
