//! The emitter against the interpreter of the block form: every inline
//! form on random operands, and random blocks long enough to spill, with
//! thunk calls and guards between the operations.

use super::*;
use crate::asm::Cond;
use vmlab_hv_tcg_ir::block::{Code, Op, Place, Slot, op_in_frame, step_unchecked};

/// xorshift64, so that a failure repeats.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// A value with the edges of the range more often than not.
    fn value(&mut self) -> u64 {
        match self.below(10) {
            0 => 0,
            1 => u64::MAX,
            2 => 1,
            3 => self.below(70),
            4 => 1 << self.below(64),
            5 => !(1 << self.below(64)),
            6 => self.next() & 0xffff_ffff,
            7 => 0x8000_0000_0000_0000,
            _ => self.next(),
        }
    }

    fn mask(&mut self) -> u64 {
        match self.below(5) {
            0 => u64::MAX,
            1 => u64::from(u32::MAX),
            2 => u64::MAX >> self.below(64),
            3 => 1,
            _ => self.next(),
        }
    }
}

const PURE: [Code; 24] = [
    Code::Add,
    Code::Sub,
    Code::Mul,
    Code::And,
    Code::Or,
    Code::Xor,
    Code::ShiftLeft,
    Code::ShiftRight,
    Code::ArithmeticShiftRight,
    Code::Equal,
    Code::NotEqual,
    Code::Less,
    Code::LessEqual,
    Code::IsZero,
    Code::BitNot,
    Code::Negate,
    Code::SignExtend,
    Code::LeadingZeros,
    Code::TrailingZeros,
    Code::ReverseBytes,
    Code::PopCount,
    Code::Slice,
    Code::Concat,
    Code::Select,
];

/// A random operation of `code` with the operands given, as the lowering
/// of the block form makes them.
fn operation(random: &mut Random, code: Code, dst: Slot, a: Slot, b: Slot, c: Slot) -> Op {
    let width = |random: &mut Random| 1 + random.below(64) as u8;
    let (aux, imm) = match code {
        Code::ArithmeticShiftRight
        | Code::SignExtend
        | Code::LeadingZeros
        | Code::TrailingZeros
        | Code::PopCount => {
            let width = width(random);
            (width, random.mask())
        }
        Code::ReverseBytes => (8 * (1 + random.below(8) as u8), random.mask()),
        Code::Slice | Code::Concat => (random.below(64) as u8, random.mask()),
        Code::Equal | Code::NotEqual | Code::Less | Code::LessEqual | Code::IsZero => (0, 0),
        Code::Select => (0, u64::from(c)),
        _ => (0, random.mask()),
    };
    Op {
        code,
        aux,
        dst,
        a,
        b,
        imm,
    }
}

/// The environment of the test thunk: the block and its frame.
struct Env {
    ops: Vec<Op>,
    frame: *mut u64,
    words: usize,
    calls: Vec<Effect>,
    thunk_calls: u64,
    /// The guard that left the block.
    left_at: Option<usize>,
}

/// A helper call of the test: the first word that it writes gets a mix of
/// the words that it reads.
fn call(effect: &Effect, frame: &mut [u64]) {
    // A lazy condition gives what its form gives for a kind with one.
    if let Some(lazy) = &effect.condition {
        let word = |slot: Slot| frame[usize::from(slot)];
        if let Some((_, test)) = lazy.cases.iter().find(|(kind, _)| *kind == word(lazy.kind)) {
            frame[usize::from(lazy.result)] = u64::from(test.evaluate(word(lazy.a), word(lazy.b)));
            return;
        }
    }
    let mut value = 0x9e37_79b9_7f4a_7c15u64;
    for word in &effect.reads {
        value = value.rotate_left(7) ^ frame[usize::from(*word)];
    }
    for (n, word) in effect.writes.iter().enumerate() {
        frame[usize::from(*word)] = value.wrapping_mul(n as u64 + 3);
    }
}

/// What a guard that fails gives back: the block leaves.
const LEFT: u64 = 7;

/// # Safety
/// `env` is an `Env` whose frame is valid.
unsafe extern "sysv64" fn thunk(env: *mut c_void, index: u64) -> u64 {
    // SAFETY: the test passes its `Env`.
    let env = unsafe { &mut *env.cast::<Env>() };
    env.thunk_calls += 1;
    // SAFETY: the frame of the test, as long as its `words`.
    let frame = unsafe { std::slice::from_raw_parts_mut(env.frame, env.words) };
    let op = env.ops[index as usize];
    match op.code {
        Code::Guard => {
            if (frame[usize::from(op.a)] != 0) != (op.aux != 0) {
                env.left_at = Some(index as usize);
                LEFT
            } else {
                0
            }
        }
        Code::Call => {
            call(&env.calls[op.imm as usize], frame);
            0
        }
        _ => {
            assert!(op_in_frame(&op, frame.len(), 0));
            // SAFETY: checked just above.
            unsafe { step_unchecked(&op, frame, &[]) };
            0
        }
    }
}

/// A block of the test.
struct Block {
    ops: Vec<Op>,
    frame: Vec<u64>,
    calls: Vec<Effect>,
    leaves: Vec<Vec<Slot>>,
    inputs: Vec<(Place, u32)>,
    outputs: Vec<(u32, Place)>,
}

/// The block in the interpreter: the slots after it, and the leave and
/// the words of the frame that it names when it left through one.
fn interpret(block: &Block, slots: &mut [u128]) -> (u64, Vec<u64>) {
    let mut frame = block.frame.clone();
    for (place, register) in &block.inputs {
        place.write(&mut frame, slots[*register as usize]);
    }
    for op in &block.ops {
        match op.code {
            Code::Guard => {
                if (frame[usize::from(op.a)] != 0) != (op.aux != 0) {
                    let words = block.leaves[op.imm as usize]
                        .iter()
                        .map(|word| frame[usize::from(*word)])
                        .collect();
                    return (LEFT, words);
                }
            }
            Code::Call => call(&block.calls[op.imm as usize], &mut frame),
            _ => {
                assert!(op_in_frame(op, frame.len(), 0));
                // SAFETY: checked just above.
                unsafe { step_unchecked(op, &mut frame, &[]) };
            }
        }
    }
    for (register, place) in &block.outputs {
        slots[*register as usize] = place.read(&frame);
    }
    (DONE, Vec::new())
}

fn compiled(jit: &mut Jit, block: &Block, slots: &mut [u128], budget: i64) -> (u64, Vec<u64>, u64) {
    let shape = Shape {
        ops: &block.ops,
        generics: &[],
        frame: &block.frame,
        accesses: &[],
        calls: &block.calls,
        leaves: &block.leaves,
        inputs: &block.inputs,
        outputs: &block.outputs,
        instructions: 1,
    };
    let entry = jit.compile(&shape).expect("a block that the emitter takes");
    let mut frame = block.frame.clone();
    let mut env = Env {
        ops: block.ops.clone(),
        frame: frame.as_mut_ptr(),
        words: frame.len(),
        calls: block.calls.clone(),
        thunk_calls: 0,
        left_at: None,
    };
    let mut context = Context {
        budget,
        thunk,
        env: (&raw mut env).cast(),
    };
    // SAFETY: the entry was just compiled from this block, and the slots
    // and the frame are as large as it names.
    let code = unsafe { jit.enter(&entry, &mut context, slots.as_mut_ptr(), frame.as_mut_ptr()) };
    let mut words = Vec::new();
    if code == LEFT {
        let op = block.ops[env.left_at.expect("the guard that failed")];
        words = block.leaves[op.imm as usize]
            .iter()
            .map(|word| frame[usize::from(*word)])
            .collect();
    }
    (code, words, env.thunk_calls)
}

#[test]
fn every_inline_form_computes_what_the_interpreter_computes() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let mut random = Random(0x0123_4567_89ab_cdef);
    for code in PURE {
        for round in 0..400 {
            // Words 1 to 3 are inputs, 4 to 6 constants, 7 the result.
            let mut frame = vec![0u64; 8];
            for word in &mut frame[4..7] {
                *word = random.value();
            }
            // Some rounds take their operands from the constants, so that
            // the immediate forms run too.
            let pick = |random: &mut Random, n: u16| {
                if round % 3 == 0 && random.below(2) == 0 {
                    n + 3
                } else {
                    n
                }
            };
            let (a, b, c) = (
                pick(&mut random, 1),
                pick(&mut random, 2),
                pick(&mut random, 3),
            );
            let op = operation(&mut random, code, 7, a, b, c);
            let block = Block {
                ops: vec![op],
                frame,
                calls: Vec::new(),
                leaves: Vec::new(),
                inputs: (1..4)
                    .map(|n| {
                        (
                            Place {
                                slot: n,
                                wide: false,
                            },
                            u32::from(n),
                        )
                    })
                    .collect(),
                outputs: vec![(
                    0,
                    Place {
                        slot: 7,
                        wide: false,
                    },
                )],
            };
            let mut slots = [0u128; 4];
            for slot in &mut slots {
                *slot = u128::from(random.value());
            }
            // A slot holds a value of one word.
            let mut expected = slots;
            interpret(&block, &mut expected);
            let mut got = slots;
            compiled(&mut jit, &block, &mut got, 100);
            assert_eq!(
                got,
                expected,
                "{op:?} on {slots:x?} and {:x?}",
                &block.frame[4..7]
            );
        }
    }
}

/// A random block: `count` operations over inputs, constants and the
/// results before them, with helper calls and guards, and outputs of the
/// last results and of some inputs.
fn random_block(random: &mut Random, count: usize) -> Block {
    let inputs = 1 + random.below(12) as u16;
    let constants = 1 + random.below(6) as u16;
    let mut frame = vec![0u64; usize::from(1 + inputs + constants)];
    for word in &mut frame[usize::from(1 + inputs)..] {
        *word = random.value();
    }
    let mut ops = Vec::new();
    let mut calls = Vec::new();
    let mut leaves = Vec::new();
    let mut defined: Vec<Slot> = (1..1 + inputs + constants).collect();
    for _ in 0..count {
        let any = |random: &mut Random, defined: &[Slot]| {
            // Mostly a recent value, so that values die and registers come
            // back, sometimes an old one, which keeps it alive.
            if random.below(4) == 0 {
                defined[random.below(defined.len() as u64) as usize]
            } else {
                let recent = defined.len().min(6);
                defined[defined.len() - 1 - random.below(recent as u64) as usize]
            }
        };
        let dst = frame.len() as Slot;
        match random.below(14) {
            12 | 13 => {
                // A comparison, its operands flipped at 32 or 64 bits
                // sometimes, and the select or the guard that reads it.
                let mut x = any(random, &defined);
                let mut y = any(random, &defined);
                let mut conjunction = false;
                if random.below(4) == 0 {
                    let and = frame.len() as Slot;
                    frame.push(0);
                    ops.push(Op {
                        code: Code::And,
                        aux: 0,
                        dst: and,
                        a: x,
                        b: y,
                        imm: if random.below(2) == 0 {
                            u64::MAX
                        } else {
                            u64::from(u32::MAX)
                        },
                    });
                    x = and;
                    conjunction = true;
                } else if random.below(2) == 0 {
                    let (sign, mask) = if random.below(2) == 0 {
                        (1u64 << 63, u64::MAX)
                    } else {
                        (1 << 31, u64::from(u32::MAX))
                    };
                    let sign_word = frame.len() as Slot;
                    frame.push(sign);
                    for operand in [&mut x, &mut y] {
                        if random.below(4) == 0 {
                            continue;
                        }
                        let flipped = frame.len() as Slot;
                        frame.push(0);
                        ops.push(Op {
                            code: Code::Xor,
                            aux: 0,
                            dst: flipped,
                            a: *operand,
                            b: sign_word,
                            imm: mask,
                        });
                        *operand = flipped;
                    }
                }
                let code = if conjunction {
                    Code::IsZero
                } else {
                    [Code::Equal, Code::NotEqual, Code::Less, Code::LessEqual, Code::IsZero]
                        [random.below(5) as usize]
                };
                let condition = frame.len() as Slot;
                frame.push(0);
                ops.push(Op {
                    code,
                    aux: 0,
                    dst: condition,
                    a: x,
                    b: y,
                    imm: 0,
                });
                if random.below(3) == 0 {
                    let leave: Vec<Slot> = (0..random.below(4))
                        .map(|_| any(random, &defined))
                        .collect();
                    ops.push(Op {
                        code: Code::Guard,
                        aux: u8::from(random.below(2) == 0),
                        dst: 0,
                        a: condition,
                        b: 0,
                        imm: leaves.len() as u64,
                    });
                    leaves.push(leave);
                } else {
                    let dst = frame.len() as Slot;
                    frame.push(0);
                    ops.push(Op {
                        code: Code::Select,
                        aux: 0,
                        dst,
                        a: any(random, &defined),
                        b: any(random, &defined),
                        imm: u64::from(condition),
                    });
                    defined.push(dst);
                }
            }
            0 => {
                let reads: Vec<Slot> = (0..random.below(4))
                    .map(|_| any(random, &defined))
                    .collect();
                let writes: Vec<Slot> = (0..1 + random.below(2)).map(|n| dst + n as Slot).collect();
                frame.extend(writes.iter().map(|_| 0));
                defined.extend(&writes);
                ops.push(Op {
                    code: Code::Call,
                    aux: 0,
                    dst: 0,
                    a: 0,
                    b: 0,
                    imm: calls.len() as u64,
                });
                calls.push(Effect {
                    reads,
                    writes,
                    leave: None,
                    condition: None,
                });
            }
            1 => {
                // A guard that holds most of the time.
                let leave: Vec<Slot> = (0..random.below(5))
                    .map(|_| any(random, &defined))
                    .collect();
                let condition = any(random, &defined);
                ops.push(Op {
                    code: Code::Guard,
                    aux: u8::from(random.below(8) != 0),
                    dst: 0,
                    a: condition,
                    b: 0,
                    imm: leaves.len() as u64,
                });
                leaves.push(leave);
            }
            _ => {
                let code = PURE[random.below(PURE.len() as u64) as usize];
                let (a, b, c) = (
                    any(random, &defined),
                    any(random, &defined),
                    any(random, &defined),
                );
                ops.push(operation(random, code, dst, a, b, c));
                frame.push(0);
                defined.push(dst);
            }
        }
    }
    let mut outputs = Vec::new();
    for register in 0..(1 + random.below(16) as u32) {
        let slot = defined[random.below(defined.len() as u64) as usize];
        outputs.push((register, Place { slot, wide: false }));
    }
    Block {
        ops,
        frame,
        calls,
        leaves,
        inputs: (1..1 + inputs)
            .map(|n| {
                (
                    Place {
                        slot: n,
                        wide: false,
                    },
                    // Inputs from slots that outputs also write.
                    u32::from(n - 1) % 16,
                )
            })
            .collect(),
        outputs,
    }
}

#[test]
fn random_blocks_spill_call_and_leave_as_the_interpreter_does() {
    let mut jit = Jit::with_capacity(4 << 20).expect("a code region");
    let mut random = Random(0xfeed_face_dead_beef);
    let mut left = 0;
    for round in 0..3000 {
        let count = 1 + random.below(if round % 10 == 0 { 400 } else { 60 }) as usize;
        let block = random_block(&mut random, count);
        let mut slots = [0u128; 16];
        for slot in &mut slots {
            *slot = u128::from(random.value());
        }
        let mut expected = slots;
        let (code, words) = interpret(&block, &mut expected);
        let mut got = slots;
        let (got_code, got_words, _) = compiled(&mut jit, &block, &mut got, 100);
        assert_eq!(got_code, code, "round {round}");
        if code == DONE {
            assert_eq!(got, expected, "round {round}");
        } else {
            left += 1;
            // A block that leaves writes no slot: the words of the leave are
            // in the frame.
            assert_eq!(got, slots, "round {round}");
            assert_eq!(got_words, words, "round {round}");
        }
    }
    assert!(left > 100, "{left} blocks left through a guard");
}

/// Operands at the edges of the orders of 32 and 64 bits.
const EDGES: [u64; 14] = [
    0,
    1,
    2,
    0x7fff_ffff,
    0x8000_0000,
    0x8000_0001,
    0xffff_ffff,
    0x1_0000_0000,
    0x7fff_ffff_ffff_ffff,
    0x8000_0000_0000_0000,
    0x8000_0000_7fff_ffff,
    0xffff_ffff_8000_0000,
    u64::MAX - 1,
    u64::MAX,
];

fn word(slot: Slot) -> Place {
    Place { slot, wide: false }
}

/// H8a: a comparison that a select or a guard reads next is host flags.
/// Every comparison, unsigned and signed at both widths, with its operands
/// in registers, in the state and constant, against the interpreter on the
/// edges of the orders.
#[test]
fn comparisons_read_as_host_flags_give_what_the_interpreter_gives() {
    let mut jit = Jit::with_capacity(16 << 20).expect("a code region");
    let codes = [
        Code::Equal,
        Code::NotEqual,
        Code::Less,
        Code::LessEqual,
        Code::IsZero,
    ];
    let mut random = Random(0x0e85_0001);
    let mut rounds = 0;
    for code in codes {
        // 0: unsigned, 32 and 64: the operands flipped at that width, 2
        // and 3: the conjunction of the operands at 64 and 32 bits compared
        // with zero.
        for flip in [0u8, 32, 64, 2, 3] {
            if flip < 32 && flip != 0 && matches!(code, Code::Less | Code::LessEqual) {
                continue;
            }
            // 0: a select, 1 and 2: a guard that expects zero or not.
            for consumer in 0..3 {
                // Which operands are constants.
                for constants in 0..4u8 {
                    for _ in 0..40 {
                        let x = EDGES[random.below(EDGES.len() as u64) as usize];
                        let y = EDGES[random.below(EDGES.len() as u64) as usize];
                        // Words 1, 2: inputs x and y. 3, 4: constants x and
                        // y. 5: the sign. 6, 7: flips. 8: the condition. 9,
                        // 10: what the select picks from. 11: its result.
                        let (sign, mask) = match flip {
                            32 => (1u64 << 31, u64::from(u32::MAX)),
                            _ => (1 << 63, u64::MAX),
                        };
                        let frame = vec![0, 0, 0, x, y, sign, 0, 0, 0, 0, 0, 0];
                        let left = if constants & 1 != 0 { 3 } else { 1 };
                        let right = if constants & 2 != 0 { 4 } else { 2 };
                        let mut ops = Vec::new();
                        let (mut a, mut b) = (left, right);
                        if flip == 2 || flip == 3 {
                            ops.push(Op {
                                code: Code::And,
                                aux: 0,
                                dst: 6,
                                a: left,
                                b: right,
                                imm: if flip == 2 { u64::MAX } else { u64::from(u32::MAX) },
                            });
                            // Word 0 is the constant zero.
                            (a, b) = (6, 0);
                        } else if flip != 0 {
                            ops.push(Op {
                                code: Code::Xor,
                                aux: 0,
                                dst: 6,
                                a: left,
                                b: 5,
                                imm: mask,
                            });
                            // A constant operand is flipped already.
                            if constants & 2 == 0 {
                                ops.push(Op {
                                    code: Code::Xor,
                                    aux: 0,
                                    dst: 7,
                                    a: right,
                                    b: 5,
                                    imm: mask,
                                });
                                b = 7;
                            }
                            a = 6;
                        }
                        ops.push(Op {
                            code,
                            aux: 0,
                            dst: 8,
                            a,
                            b,
                            imm: 0,
                        });
                        let mut leaves = Vec::new();
                        if consumer == 0 {
                            ops.push(Op {
                                code: Code::Select,
                                aux: 0,
                                dst: 11,
                                a: 9,
                                b: 10,
                                imm: 8,
                            });
                        } else {
                            ops.push(Op {
                                code: Code::Guard,
                                aux: consumer - 1,
                                dst: 0,
                                a: 8,
                                b: 0,
                                imm: 0,
                            });
                            leaves.push(vec![1, 2, 9]);
                        }
                        let block = Block {
                            ops,
                            frame,
                            calls: Vec::new(),
                            leaves,
                            inputs: vec![(word(1), 0), (word(2), 1), (word(9), 2), (word(10), 3)],
                            outputs: vec![(0, word(11)), (1, word(1))],
                        };
                        let mut slots = [
                            u128::from(x),
                            u128::from(y),
                            u128::from(random.value()),
                            u128::from(random.value()),
                        ];
                        let mut expected = slots;
                        let (code_expected, words) = interpret(&block, &mut expected);
                        let (got_code, got_words, _) = compiled(&mut jit, &block, &mut slots, 100);
                        let what = format!("{:?}", block.ops);
                        assert_eq!(got_code, code_expected, "{what} on {x:#x} {y:#x}");
                        assert_eq!(got_words, words, "{what} on {x:#x} {y:#x}");
                        if code_expected == DONE {
                            assert_eq!(slots, expected, "{what} on {x:#x} {y:#x}");
                        }
                        rounds += 1;
                    }
                }
            }
        }
    }
    assert!(jit.stats.fused >= rounds, "{} of {rounds} fused", jit.stats.fused);
}

/// H8c: a lazy condition runs its form for a kind that has one and calls
/// the thunk for any other, for every host test on the edges.
#[test]
fn lazy_conditions_run_their_forms_inline_and_call_for_other_kinds() {
    let mut jit = Jit::with_capacity(16 << 20).expect("a code region");
    let conds = [
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
    let mut tests = vec![
        FlagTest::Always(false),
        FlagTest::Always(true),
        FlagTest::Table(0xa5c3),
        FlagTest::Table(1),
    ];
    for operation in [FlagOperation::Sub, FlagOperation::Add, FlagOperation::Test] {
        for wide in [false, true] {
            for cond in conds {
                tests.push(FlagTest::Host {
                    operation,
                    wide,
                    cond,
                });
            }
        }
    }
    let mut random = Random(0x0e85_0002);
    let mut stubs = 0;
    for test in tests {
        for constant_kind in [false, true] {
            for _ in 0..60 {
                let a = EDGES[random.below(EDGES.len() as u64) as usize];
                let b = EDGES[random.below(EDGES.len() as u64) as usize];
                // Kind 9 has the form, 3 an explicit one, 5 none.
                let kind = [9u64, 3, 5][random.below(3) as usize];
                // Words 1: the kind, 2, 3: a, b, 4: the condition, 5: the
                // result, 6: the kind as a constant.
                let frame = vec![0, 0, 0, 0, 11, 0, kind];
                let kind_word = if constant_kind { 6 } else { 1 };
                let lazy = LazyCondition {
                    kind: kind_word,
                    a: 2,
                    b: 3,
                    code: 4,
                    code_value: 11,
                    result: 5,
                    cases: vec![(9, test), (3, FlagTest::Table(0x0ff0))],
                };
                let calls = vec![Effect {
                    reads: vec![kind_word, 2, 3, 4],
                    writes: vec![5],
                    leave: None,
                    condition: Some(lazy),
                }];
                let block = Block {
                    ops: vec![Op {
                        code: Code::Call,
                        aux: 0,
                        dst: 0,
                        a: 0,
                        b: 0,
                        imm: 0,
                    }],
                    frame,
                    calls,
                    leaves: Vec::new(),
                    inputs: vec![(word(1), 0), (word(2), 1), (word(3), 2)],
                    outputs: vec![(0, word(5)), (1, word(2))],
                };
                let mut slots = [u128::from(kind), u128::from(a), u128::from(b)];
                let mut expected = slots;
                interpret(&block, &mut expected);
                let (code, _, thunk_calls) = compiled(&mut jit, &block, &mut slots, 100);
                assert_eq!(code, DONE);
                assert_eq!(slots, expected, "{test:?} kind {kind} on {a:#x} {b:#x}");
                assert_eq!(thunk_calls, u64::from(kind == 5), "{test:?} kind {kind}");
                stubs += thunk_calls;
            }
        }
    }
    assert!(stubs > 0);
    assert!(jit.stats.lazy > 0);
}

/// The forms of a lazy condition are those of its one condition: where the
/// word of the condition is not the constant they are for, the call runs.
#[test]
fn a_lazy_condition_for_another_code_is_a_call() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let lazy = LazyCondition {
        kind: 1,
        a: 2,
        b: 3,
        code: 4,
        code_value: 12,
        result: 5,
        cases: vec![(1, FlagTest::Always(true))],
    };
    let block = Block {
        ops: vec![Op {
            code: Code::Call,
            aux: 0,
            dst: 0,
            a: 0,
            b: 0,
            imm: 0,
        }],
        frame: vec![0, 0, 0, 0, 11, 0],
        calls: vec![Effect {
            reads: vec![1, 2, 3, 4],
            writes: vec![5],
            leave: None,
            condition: Some(lazy),
        }],
        leaves: Vec::new(),
        inputs: vec![(word(1), 0), (word(2), 1), (word(3), 2)],
        outputs: vec![(0, word(5))],
    };
    let mut slots = [1u128, 2, 3];
    let (_, _, thunk_calls) = compiled(&mut jit, &block, &mut slots, 100);
    assert_eq!((thunk_calls, jit.stats.lazy), (1, 0));
}

/// The definitions of the host flags that the forms are checked against.
#[test]
fn the_host_flags_are_those_of_the_definitions() {
    for a in EDGES {
        for b in EDGES {
            let (cf, zf, sf, of) = sub_flags(a, b, true);
            assert_eq!(cf, a < b);
            assert_eq!(zf, a == b);
            assert_eq!(sf, (a.wrapping_sub(b) as i64) < 0);
            assert_eq!(of, (a as i64).checked_sub(b as i64).is_none());
            let (cf, zf, sf, of) = sub_flags(a, b, false);
            let (a32, b32) = (a as u32, b as u32);
            assert_eq!(cf, a32 < b32);
            assert_eq!(zf, a32 == b32);
            assert_eq!(sf, (a32.wrapping_sub(b32) as i32) < 0);
            assert_eq!(of, (a32 as i32).checked_sub(b32 as i32).is_none());
            let (cf, zf, sf, of) = add_flags(a, b, true);
            assert_eq!(cf, a.checked_add(b).is_none());
            assert_eq!(zf, a.wrapping_add(b) == 0);
            assert_eq!(sf, (a.wrapping_add(b) as i64) < 0);
            assert_eq!(of, (a as i64).checked_add(b as i64).is_none());
            let (cf, _, _, of) = add_flags(a, b, false);
            assert_eq!(cf, a32.checked_add(b32).is_none());
            assert_eq!(of, (a32 as i32).checked_add(b32 as i32).is_none());
        }
    }
}

#[test]
fn the_head_of_a_block_takes_its_count_from_the_budget() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let block = Block {
        ops: Vec::new(),
        frame: vec![0, 5],
        calls: Vec::new(),
        leaves: Vec::new(),
        inputs: Vec::new(),
        outputs: vec![(
            0,
            Place {
                slot: 1,
                wide: false,
            },
        )],
    };
    let mut slots = [0u128; 1];
    let (code, _, _) = compiled(&mut jit, &block, &mut slots, 0);
    assert_eq!((code, slots[0]), (BUDGET, 0));
    let (code, _, _) = compiled(&mut jit, &block, &mut slots, 1);
    assert_eq!((code, slots[0]), (DONE, 5));
}

#[test]
fn a_form_that_the_host_lacks_is_refused() {
    let mut jit = Jit::with_features(1 << 20, Features::default()).expect("a code region");
    let op = Op {
        code: Code::PopCount,
        aux: 64,
        dst: 2,
        a: 1,
        b: 0,
        imm: u64::MAX,
    };
    let frame = [0u64; 3];
    let outputs = [(
        0,
        Place {
            slot: 2,
            wide: false,
        },
    )];
    let inputs = [(
        Place {
            slot: 1,
            wide: false,
        },
        0,
    )];
    let shape = Shape {
        ops: &[op],
        generics: &[],
        frame: &frame,
        accesses: &[],
        calls: &[],
        leaves: &[],
        inputs: &inputs,
        outputs: &outputs,
        instructions: 1,
    };
    assert_eq!(jit.compile(&shape), Err(Refused::Form(Code::PopCount)));
    let rbit = Op {
        code: Code::ReverseBits,
        ..op
    };
    let shape = Shape {
        ops: &[rbit],
        ..shape
    };
    assert_eq!(jit.compile(&shape), Err(Refused::Form(Code::ReverseBits)));
}

#[test]
fn a_full_region_starts_a_new_generation() {
    let mut jit = Jit::with_capacity(64 << 10).expect("a code region");
    let mut random = Random(3);
    let block = random_block(&mut random, 50);
    let mut first = None;
    for _ in 0..2000 {
        let shape = Shape {
            ops: &block.ops,
            generics: &[],
            frame: &block.frame,
            accesses: &[],
            calls: &block.calls,
            leaves: &block.leaves,
            inputs: &block.inputs,
            outputs: &block.outputs,
            instructions: 1,
        };
        let entry = jit.compile(&shape).expect("a block");
        let first = *first.get_or_insert(entry);
        if !jit.current(&first) {
            assert!(jit.stats.resets > 0);
            return;
        }
    }
    panic!("the region never filled");
}
