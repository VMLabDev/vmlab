//! The emitter against the interpreter of the block form: every inline
//! form on random operands, and random blocks long enough to spill, with
//! thunk calls and guards between the operations.

use super::*;
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
        match random.below(12) {
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

#[test]
fn the_perf_map_names_the_trampoline_and_each_block() {
    assert_eq!(
        crate::perf::line(0x7f00_1000, 0x40, "a\nb"),
        "7f001000 40 a b\n"
    );
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let mut random = Random(5);
    let block = random_block(&mut random, 20);
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
    // Off: nothing is written.
    let entry = jit.compile(&shape).expect("a block");
    jit.name(&entry, "not named").expect("no map, no write");
    jit.set_perf_map(true).expect("the perf map opens");
    let entry = jit.compile(&shape).expect("a block");
    jit.name(&entry, "a64 0x1000 test block")
        .expect("a line of the map");
    let map = std::fs::read_to_string(crate::perf::path()).expect("the perf map");
    let line = format!(
        "{:x} {:x} a64 0x1000 test block",
        entry.address, entry.bytes
    );
    assert!(map.lines().any(|l| l == line), "{map}");
    assert!(
        map.lines().any(|l| l.ends_with(" vmlab-hv-jit enter")),
        "{map}"
    );
    assert!(!map.contains("not named"));
    let _ = std::fs::remove_file(crate::perf::path());
}

#[test]
fn written_words_are_the_results_and_the_inputs_and_the_rest_are_constants() {
    let mut random = Random(9);
    for _ in 0..50 {
        let block = random_block(&mut random, 30);
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
        let written = crate::emit::written(&shape).expect("a block");
        assert_eq!(written.len(), block.frame.len());
        for op in &block.ops {
            if (op.code as u8) < vmlab_hv_tcg_ir::block::FIRST_EFFECT && op.code != Code::Generic {
                assert!(written[usize::from(op.dst)], "{op:?}");
            }
        }
        for (place, _) in &block.inputs {
            assert!(written[usize::from(place.slot)]);
        }
    }
}

#[test]
fn a_seeded_defect_makes_an_add_subtract() {
    // add, of the inputs, to the output: 5 + 3 is 2 with the defect.
    let frame = vec![0u64; 3];
    let ops = [Op {
        code: Code::Add,
        aux: 64,
        dst: 2,
        a: 0,
        b: 1,
        imm: u64::MAX,
    }];
    let inputs = [
        (
            Place {
                slot: 0,
                wide: false,
            },
            0,
        ),
        (
            Place {
                slot: 1,
                wide: false,
            },
            1,
        ),
    ];
    let outputs = [(
        2,
        Place {
            slot: 2,
            wide: false,
        },
    )];
    let shape = Shape {
        ops: &ops,
        generics: &[],
        frame: &frame,
        accesses: &[],
        calls: &[],
        leaves: &[],
        inputs: &inputs,
        outputs: &outputs,
        instructions: 1,
    };
    for (defect, expected) in [(crate::Defect::None, 8), (crate::Defect::AddSubtracts, 2)] {
        let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
        jit.seed_defect(defect);
        let entry = jit.compile(&shape).expect("a block");
        let mut slots = [5u128, 3, 0];
        let mut run_frame = frame.clone();
        let mut env = Env {
            ops: ops.to_vec(),
            frame: run_frame.as_mut_ptr(),
            words: run_frame.len(),
            calls: Vec::new(),
            thunk_calls: 0,
            left_at: None,
        };
        let mut context = Context {
            budget: 10,
            thunk,
            env: (&raw mut env).cast(),
        };
        // SAFETY: the entry was just compiled from this block, and the
        // slots and the frame are as large as it names.
        let code = unsafe {
            jit.enter(
                &entry,
                &mut context,
                slots.as_mut_ptr(),
                run_frame.as_mut_ptr(),
            )
        };
        assert_eq!((code, slots[2]), (DONE, expected), "{defect:?}");
    }
}
