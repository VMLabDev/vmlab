//! The emitter against the interpreter of the block form: every inline
//! form on random operands, and random blocks long enough to spill, with
//! thunk calls, guards, and loads and stores over the fast TLB between the
//! operations.

use super::*;
use emit::THUNK_ACCESS;
use std::sync::atomic::AtomicU32;
use tlb::FastTlb;
use vmlab_hv_tcg_ir::block::{Code, Op, Place, Slot, op_in_frame, step_unchecked};

/// The guest memory of the tests: `PAGES` pages at the virtual address
/// `RAM`. The page `WATCHED_PAGE` has an odd watch word.
const RAM: u64 = 0x7000_0000;
const PAGES: usize = 4;
const WATCHED_PAGE: usize = 2;

/// Where an access of `bytes` at `va` is in the memory of the tests, when it
/// is there.
fn offset_of(va: u64, bytes: u8) -> Option<usize> {
    let offset = va.checked_sub(RAM)?;
    (offset + u64::from(bytes) <= (PAGES * 4096) as u64).then_some(offset as usize)
}

fn access_size(code: Code) -> Option<(u8, bool)> {
    Some(match code {
        Code::Load1 => (1, false),
        Code::Load2 => (2, false),
        Code::Load4 => (4, false),
        Code::Load8 => (8, false),
        Code::Store1 => (1, true),
        Code::Store2 => (2, true),
        Code::Store4 => (4, true),
        Code::Store8 => (8, true),
        _ => return None,
    })
}

/// A load or a store of the tests on `ram`. `false`: the address is outside
/// it, and the access leaves the block.
fn access(op: &Op, frame: &mut [u64], ram: &mut [u8]) -> bool {
    let (bytes, store) = access_size(op.code).expect("an access");
    let Some(offset) = offset_of(frame[usize::from(op.a)], bytes) else {
        return false;
    };
    let part = &mut ram[offset..offset + usize::from(bytes)];
    if store {
        part.copy_from_slice(&frame[usize::from(op.b)].to_le_bytes()[..usize::from(bytes)]);
    } else {
        let mut value = [0u8; 8];
        value[..usize::from(bytes)].copy_from_slice(part);
        frame[usize::from(op.dst)] = u64::from_le_bytes(value);
    }
    true
}

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

/// The environment of the test thunk: the block and its frame, and the
/// memory with its fast TLB.
struct Env {
    ops: Vec<Op>,
    frame: *mut u64,
    words: usize,
    calls: Vec<Effect>,
    thunk_calls: u64,
    /// The guard or the access that left the block.
    left_at: Option<usize>,
    ram: *mut u8,
    tlb: Box<FastTlb>,
    watch: Vec<AtomicU32>,
    /// A miss fills the TLB.
    fill: bool,
    misses: u64,
    watched: u64,
    /// The accesses that hits asked to keep: address, size, value, store.
    logged: Vec<(u64, u8, u64, bool)>,
}

impl Env {
    /// The fields beside the block: no memory, an empty TLB.
    fn base() -> Self {
        Self {
            ops: Vec::new(),
            frame: std::ptr::null_mut(),
            words: 0,
            calls: Vec::new(),
            thunk_calls: 0,
            left_at: None,
            ram: std::ptr::null_mut(),
            tlb: FastTlb::new(),
            watch: Vec::new(),
            fill: false,
            misses: 0,
            watched: 0,
            logged: Vec::new(),
        }
    }
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
unsafe extern "sysv64" fn thunk(env: *mut c_void, index: u64, _block: *mut c_void) -> u64 {
    // SAFETY: the test passes its `Env`.
    let env = unsafe { &mut *env.cast::<Env>() };
    env.thunk_calls += 1;
    // SAFETY: the frame of the test, as long as its `words`.
    let frame = unsafe { std::slice::from_raw_parts_mut(env.frame, env.words) };
    let request = index & !0xffff_ffff;
    let index = index & 0xffff_ffff;
    let op = env.ops[index as usize];
    // SAFETY: the memory of the test, `PAGES` pages.
    let ram = unsafe { std::slice::from_raw_parts_mut(env.ram, PAGES * 4096) };
    if let Some((bytes, store)) = access_size(op.code) {
        let va = frame[usize::from(op.a)];
        match request {
            WATCHED => {
                assert!(store);
                assert_eq!(offset_of(va, bytes).map(|at| at / 4096), Some(WATCHED_PAGE));
                env.watched += 1;
                return 0;
            }
            LOGGED => {
                let value = if store {
                    frame[usize::from(op.b)]
                } else {
                    frame[usize::from(op.dst)]
                };
                let mask = u64::MAX >> (64 - 8 * u32::from(bytes));
                env.logged.push((va, bytes, value & mask, store));
                return 0;
            }
            MISS => env.misses += 1,
            0 => {}
            _ => panic!("a request {request:#x} of an access"),
        }
        if !access(&op, frame, ram) {
            env.left_at = Some(index as usize);
            return LEFT;
        }
        if env.fill {
            let page = offset_of(va, bytes).expect("the access was made") / 4096;
            let watch = if store {
                &env.watch[page] as *const AtomicU32 as usize
            } else {
                0
            };
            let host = env.ram as usize + page * 4096;
            env.tlb
                .fill(1, store, va, host, watch, page as u64 * 4096, 12);
        }
        return 0;
    }
    assert_eq!(request, 0);
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
    memory: Memory,
}

/// The block in the interpreter, on `ram`: the slots after it, and the
/// leave and the words of the frame that it names when it left through one.
fn interpret(block: &Block, slots: &mut [u128], ram: &mut [u8]) -> (u64, Vec<u64>) {
    let mut frame = block.frame.clone();
    for (place, register) in &block.inputs {
        place.write(&mut frame, slots[*register as usize]);
    }
    for op in &block.ops {
        if access_size(op.code).is_some() {
            if !access(op, &mut frame, ram) {
                let words = block.leaves[op.imm as usize]
                    .iter()
                    .map(|word| frame[usize::from(*word)])
                    .collect();
                return (LEFT, words);
            }
            continue;
        }
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

/// What a compiled run did beside its slots.
struct Ran {
    code: u64,
    words: Vec<u64>,
    thunk_calls: u64,
    misses: u64,
    watched: u64,
    logged: Vec<(u64, u8, u64, bool)>,
}

/// What a compiled run did beside its slots.
struct Ran {
    code: u64,
    words: Vec<u64>,
    thunk_calls: u64,
    misses: u64,
    watched: u64,
    logged: Vec<(u64, u8, u64, bool)>,
}

fn compiled(jit: &mut Jit, block: &Block, slots: &mut [u128], budget: i64) -> (u64, Vec<u64>, u64) {
    let mut ram = vec![0u8; PAGES * 4096];
    let ran = compiled_on(jit, block, slots, budget, &mut ram, None);
    (ran.code, ran.words, ran.thunk_calls)
}

/// Run `block` compiled on `ram`, with the fast TLB `tlb`, which a miss
/// fills, or with an empty one that stays empty.
fn compiled_on(
    jit: &mut Jit,
    block: &Block,
    slots: &mut [u128],
    budget: i64,
    ram: &mut [u8],
    tlb: Option<Box<FastTlb>>,
) -> Ran {
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
        chain: None,
        next: None,
        memory: block.memory,
    };
    let entry = jit.compile(&shape).expect("a block that the emitter takes");
    let mut frame = block.frame.clone();
    let fill = tlb.is_some();
    let mut env = Env {
        ops: block.ops.clone(),
        frame: frame.as_mut_ptr(),
        words: frame.len(),
        calls: block.calls.clone(),
        ram: ram.as_mut_ptr(),
        tlb: tlb.unwrap_or_else(FastTlb::new),
        watch: (0..PAGES)
            .map(|page| AtomicU32::new(u32::from(page == WATCHED_PAGE)))
            .collect(),
        fill,
        ..Env::base()
    };
    let reads = env.tlb.table(1, false);
    let writes = env.tlb.table(1, true);
    let mut context = jit.context(budget, thunk, (&raw mut env).cast());
    context.reads = reads;
    context.writes = writes;
    // SAFETY: the entry was just compiled from this block, and the slots
    // and the frame are as large as it names. The tables live in `env`.
    let code = unsafe {
        jit.enter(
            &entry,
            &raw mut context,
            slots.as_mut_ptr(),
            frame.as_mut_ptr(),
        )
    };
    let mut words = Vec::new();
    if code == LEFT {
        let op = block.ops[env.left_at.expect("the guard that failed")];
        words = block.leaves[op.imm as usize]
            .iter()
            .map(|word| frame[usize::from(*word)])
            .collect();
    }
    Ran {
        code,
        words,
        thunk_calls: env.thunk_calls,
        misses: env.misses,
        watched: env.watched,
        logged: env.logged,
    }
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
                memory: Memory::Thunk,
            };
            let mut slots = [0u128; 4];
            for slot in &mut slots {
                *slot = u128::from(random.value());
            }
            // A slot holds a value of one word.
            let mut expected = slots;
            interpret(&block, &mut expected, &mut []);
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
    random_block_with(random, count, Memory::Thunk)
}

/// An address for an access of `bytes` of the tests: mostly aligned and in
/// the memory, sometimes not aligned, sometimes across a page, sometimes
/// outside.
fn address(random: &mut Random, bytes: u8) -> u64 {
    let page = RAM + 4096 * random.below(PAGES as u64);
    match random.below(16) {
        0 => page + 4096 - u64::from(bytes) / 2 - 1 + u64::from(bytes == 1),
        1 => page + random.below(4096 - u64::from(bytes)),
        2 => RAM - 4096 + random.below(4096),
        3 => RAM + (PAGES * 4096) as u64 - u64::from(bytes) / 2,
        _ => page + random.below(4096 / u64::from(bytes)) * u64::from(bytes),
    }
}

fn random_block_with(random: &mut Random, count: usize, memory: Memory) -> Block {
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
        match random.below(if memory == Memory::Thunk { 12 } else { 16 }) {
            12..=15 => {
                // A load or a store at an address of a constant word, with
                // a leave like a guard's.
                let bytes = [1u8, 2, 4, 8][random.below(4) as usize];
                let store = random.below(2) == 0;
                let at = frame.len() as Slot;
                frame.push(address(random, bytes));
                let leave: Vec<Slot> = (0..random.below(5))
                    .map(|_| any(random, &defined))
                    .collect();
                let code = match (bytes, store) {
                    (1, false) => Code::Load1,
                    (2, false) => Code::Load2,
                    (4, false) => Code::Load4,
                    (8, false) => Code::Load8,
                    (1, true) => Code::Store1,
                    (2, true) => Code::Store2,
                    (4, true) => Code::Store4,
                    _ => Code::Store8,
                };
                let (dst, b) = if store {
                    (0, any(random, &defined))
                } else {
                    frame.push(0);
                    (at + 1, 0)
                };
                ops.push(Op {
                    code,
                    aux: 0,
                    dst,
                    a: at,
                    b,
                    imm: leaves.len() as u64,
                });
                leaves.push(leave);
                if !store {
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
        memory,
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
        let (code, words) = interpret(&block, &mut expected, &mut []);
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
        memory: Memory::Thunk,
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
        chain: None,
        next: None,
        memory: Memory::Thunk,
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
            chain: None,
            next: None,
            memory: Memory::Thunk,
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
        chain: None,
        next: None,
        memory: Memory::Thunk,
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
            chain: None,
            next: None,
            memory: Memory::Thunk,
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
        chain: None,
        next: None,
        memory: Memory::Thunk,
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
            ..Env::base()
        };
        let mut context = jit.context(10, thunk, (&raw mut env).cast());
        // SAFETY: the entry was just compiled from this block, and the
        // slots and the frame are as large as it names.
        let code = unsafe {
            jit.enter(
                &entry,
                &raw mut context,
                slots.as_mut_ptr(),
                run_frame.as_mut_ptr(),
            )
        };
        assert_eq!((code, slots[2]), (DONE, expected), "{defect:?}");
    }
}

/// A block of one instruction that adds 1 to the register 0 and moves the
/// PC (the register 1) by the constant 0 or to the value of the register 2:
/// it chains to itself through a linked jump or through the jump cache.
struct Looping {
    frame: Vec<u64>,
    ops: Vec<Op>,
    inputs: Vec<(Place, u32)>,
    outputs: Vec<(u32, Place)>,
}

fn word(slot: Slot) -> Place {
    Place { slot, wide: false }
}

impl Looping {
    fn new() -> Self {
        // Word 1 is the register 0, 2 the sum, 3 the constant 1, 4 the
        // distance 0, 5 the register 2.
        let mut frame = vec![0u64; 6];
        frame[3] = 1;
        Self {
            frame,
            ops: vec![Op {
                code: Code::Add,
                aux: 0,
                dst: 2,
                a: 1,
                b: 3,
                imm: u64::MAX,
            }],
            inputs: vec![(word(1), 0), (word(5), 2)],
            outputs: vec![(0, word(2))],
        }
    }

    fn compile(&mut self, jit: &mut Jit, kind: NextPc, key: u64) -> Entry {
        let frame = self.frame.as_mut_ptr() as u64;
        let shape = Shape {
            ops: &self.ops,
            generics: &[],
            frame: &self.frame,
            accesses: &[],
            calls: &[],
            leaves: &[],
            inputs: &self.inputs,
            outputs: &self.outputs,
            instructions: 1,
            chain: Some(Chain {
                frame,
                tag: 0x7a6,
                key,
                page_offset: 0x100,
                sites: true,
                jumps: true,
                verify: false,
            }),
            next: Some(Next { pc: 1, kind }),
            memory: Memory::Thunk,
        };
        jit.compile(&shape).expect("a block that the emitter takes")
    }

    /// Run from `entry` with `budget`: the code it left with, and the
    /// context after.
    fn run(
        &mut self,
        jit: &Jit,
        entry: &Entry,
        slots: &mut [u128; 3],
        budget: i64,
    ) -> (u64, Context) {
        let mut env = Env {
            ops: self.ops.clone(),
            frame: self.frame.as_mut_ptr(),
            words: self.frame.len(),
            calls: Vec::new(),
            ..Env::base()
        };
        let mut context = jit.context(budget, thunk, (&raw mut env).cast());
        // SAFETY: the entry was compiled from this block, whose frame is
        // alive, and the slots are as many as it names.
        let code = unsafe {
            jit.enter(
                entry,
                &raw mut context,
                slots.as_mut_ptr(),
                self.frame.as_mut_ptr(),
            )
        };
        (code, context)
    }
}

#[test]
fn a_linked_jump_chains_until_the_budget_and_unlinks_to_its_stub() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let mut block = Looping::new();
    let entry = block.compile(&mut jit, NextPc::Relative(word(4)), 9);
    assert_eq!(entry.site_count, 1);
    let mut slots = [0u128, 0x4000_0100, 0];
    // Not linked: one run, and the exit of the jump.
    let (code, context) = block.run(&jit, &entry, &mut slots, 100);
    assert_eq!((code, context.budget, context.chained), (CHAIN, 99, 1));
    assert_eq!(context.block as u64, 0x7a6);
    assert_eq!(slots[..2], [1, 0x4000_0100]);
    // Linked: the block runs until its head finds the budget spent.
    assert!(jit.link(&entry, 0, &entry));
    assert_eq!(jit.linked(), 1);
    let (code, context) = block.run(&jit, &entry, &mut slots, 10);
    assert_eq!((code, context.budget, context.chained), (BUDGET, 0, 10));
    assert_eq!(slots[0], 11);
    // Unlinked: back to the stub.
    jit.unlink_to(entry.address);
    assert_eq!(jit.linked(), 0);
    let (code, _) = block.run(&jit, &entry, &mut slots, 10);
    assert_eq!(code, CHAIN);
    assert_eq!(slots[0], 12);
    assert_eq!((jit.stats.links, jit.stats.unlinks), (1, 1));
}

#[test]
fn the_jump_cache_serves_its_pc_and_its_key_and_is_dropped_with_the_region() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let mut block = Looping::new();
    let entry = block.compile(&mut jit, NextPc::Absolute(word(5)), 9);
    assert_eq!(entry.site_count, 0);
    let pc = 0x4000_0100u64;
    let mut slots = [0u128, u128::from(pc), u128::from(pc)];
    let (code, _) = block.run(&jit, &entry, &mut slots, 100);
    assert_eq!(code, LOOKUP);
    jit.fill(pc, 9, &entry);
    assert_eq!(jit.jump_target(pc, 9), Some(entry.address));
    let (code, context) = block.run(&jit, &entry, &mut slots, 20);
    assert_eq!((code, context.hits), (BUDGET, 20));
    assert_eq!(slots[0], 21);
    // An entry of another context is a miss.
    jit.fill(pc, 8, &entry);
    let (code, context) = block.run(&jit, &entry, &mut slots, 20);
    assert_eq!((code, context.hits), (LOOKUP, 0));
    // An entry from before the cache was emptied is a miss.
    jit.fill(pc, 9, &entry);
    jit.clear_jumps();
    assert_eq!(jit.jump_target(pc, 9), None);
    let (code, _) = block.run(&jit, &entry, &mut slots, 20);
    assert_eq!(code, LOOKUP);
    // Another PC is a miss.
    jit.fill(pc, 9, &entry);
    slots[2] = u128::from(pc + 4);
    let (code, _) = block.run(&jit, &entry, &mut slots, 20);
    assert_eq!(code, LOOKUP);
    // A full region drops the jump cache with the code.
    let mut other = Looping::new();
    let resets = jit.stats.resets;
    while jit.stats.resets == resets {
        other.compile(&mut jit, NextPc::Absolute(word(5)), 9);
    }
    assert_eq!(jit.jump_target(pc, 9), None);
    assert!(!jit.current(&entry));
}

#[test]
fn a_fixed_next_pc_leaves_with_done_and_a_jump_outside_the_page_has_no_site() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let mut block = Looping::new();
    let entry = block.compile(&mut jit, NextPc::Fixed(8), 9);
    assert_eq!(entry.site_count, 0);
    let mut slots = [0u128, 0x4000_0100, 0];
    let (code, _) = block.run(&jit, &entry, &mut slots, 100);
    assert_eq!(code, DONE);
    assert_eq!(slots[1], 0x4000_0108);
    // A distance of a page from the offset 0x100 leaves it: the jump cache.
    block.frame[4] = 0x1000;
    let entry = block.compile(&mut jit, NextPc::Relative(word(4)), 9);
    assert_eq!(entry.site_count, 0);
    let (code, _) = block.run(&jit, &entry, &mut slots, 100);
    assert_eq!(code, LOOKUP);
    assert_eq!(slots[1], 0x4000_1108);
}

/// The memory of the tests with a pattern in it.
fn pattern(random: &mut Random) -> Vec<u8> {
    (0..PAGES * 4096).map(|_| random.next() as u8).collect()
}

#[test]
fn inlined_loads_and_stores_leave_what_the_interpreter_leaves() {
    let mut jit = Jit::with_capacity(16 << 20).expect("a code region");
    let mut random = Random(0x3e_0003);
    let (mut hits, mut misses, mut watched, mut left) = (0u64, 0u64, 0u64, 0);
    for round in 0..3000 {
        let count = 1 + random.below(if round % 10 == 0 { 300 } else { 60 }) as usize;
        let memory = if round % 4 == 0 {
            Memory::Logged
        } else {
            Memory::Inline
        };
        let block = random_block_with(&mut random, count, memory);
        let accesses = block
            .ops
            .iter()
            .filter(|op| access_size(op.code).is_some())
            .count() as u64;
        let mut slots = [0u128; 16];
        for slot in &mut slots {
            *slot = u128::from(random.value());
        }
        let ram = pattern(&mut random);
        let mut expected = slots;
        let mut expected_ram = ram.clone();
        let (code, words) = interpret(&block, &mut expected, &mut expected_ram);
        let mut got = slots;
        let mut got_ram = ram.clone();
        // Some rounds start with every page in the read table. The write
        // entries need the watch words of the run, so a miss fills them.
        let mut tlb = FastTlb::new();
        if round % 3 != 0 {
            for page in 0..PAGES {
                let host = got_ram.as_ptr() as usize + page * 4096;
                let va = RAM + page as u64 * 4096;
                tlb.fill(1, false, va, host, 0, page as u64 * 4096, 12);
            }
        }
        let ran = compiled_on(&mut jit, &block, &mut got, 100, &mut got_ram, Some(tlb));
        assert_eq!(ran.code, code, "round {round}");
        assert!(
            got_ram == expected_ram,
            "round {round}: the memories differ"
        );
        if code == DONE {
            assert_eq!(got, expected, "round {round}");
        } else {
            left += 1;
            assert_eq!(got, slots, "round {round}");
            assert_eq!(ran.words, words, "round {round}");
        }
        if memory == Memory::Logged {
            // Every hit kept its access, in order, and a miss none: the
            // owner keeps those itself.
            assert!(ran.logged.len() as u64 <= accesses);
        }
        misses += ran.misses;
        hits += ran.logged.len() as u64;
        watched += ran.watched;
    }
    assert!(left > 100, "{left} blocks left");
    assert!(
        misses > 1000 && hits > 1000,
        "{misses} misses, {hits} logged hits"
    );
    assert!(watched > 100, "{watched} stores into the watched page");
}

/// One block of `ops` over the constant words `frame`, with leaves of no
/// words, that writes the word `result` to the slot 0.
fn access_block(ops: Vec<Op>, frame: Vec<u64>, result: Slot, memory: Memory) -> Block {
    let leaves = vec![Vec::new(); ops.len()];
    Block {
        ops,
        frame,
        calls: Vec::new(),
        leaves,
        inputs: Vec::new(),
        outputs: vec![(
            0,
            Place {
                slot: result,
                wide: false,
            },
        )],
        memory,
    }
}

/// Run `block` once on `ram` with the tables `reads` and `writes`, which
/// the thunk does not fill, the thunk running the operations of `thunk_ops`.
/// The exit code, the calls of the thunk, and the misses among them.
fn run_on_tables(
    jit: &mut Jit,
    block: &Block,
    thunk_ops: &[Op],
    ram: &mut [u8],
    reads: *const tlb::Entry,
    writes: *const tlb::Entry,
) -> (u64, u64, u64, u64) {
    let shape = Shape {
        ops: &block.ops,
        generics: &[],
        frame: &block.frame,
        accesses: &[],
        calls: &[],
        leaves: &block.leaves,
        inputs: &[],
        outputs: &block.outputs,
        instructions: 1,
        chain: None,
        next: None,
        memory: block.memory,
    };
    let entry = jit.compile(&shape).expect("a block");
    let mut frame = block.frame.clone();
    let mut env = Env {
        ops: thunk_ops.to_vec(),
        frame: frame.as_mut_ptr(),
        words: frame.len(),
        ram: ram.as_mut_ptr(),
        ..Env::base()
    };
    let mut context = jit.context(10, thunk, (&raw mut env).cast());
    context.reads = reads;
    context.writes = writes;
    let mut slots = [0u128; 1];
    // SAFETY: the entry was just compiled from this block, the slots and
    // the frame are as large as it names, and the caller's tables live
    // through the run.
    let code = unsafe { jit.enter(&entry, &raw mut context, slots.as_mut_ptr(), frame.as_mut_ptr()) };
    (code, env.thunk_calls, env.misses, env.watched)
}

/// A Load8 of the word 2 at the address of the word 1, and a Store4 of the
/// word 3 at the address of the word 4.
fn load_and_store(load_at: u64, store_at: u64) -> (Vec<Op>, Vec<u64>) {
    let ops = vec![
        Op {
            code: Code::Load8,
            aux: 0,
            dst: 2,
            a: 1,
            b: 0,
            imm: 0,
        },
        Op {
            code: Code::Store4,
            aux: 0,
            dst: 0,
            a: 4,
            b: 3,
            imm: 1,
        },
    ];
    (ops, vec![0, load_at, 0, 0xaabb_ccdd_eeff_1122, store_at])
}

#[test]
fn a_hit_asks_nothing_and_a_miss_runs_the_access_in_the_thunk() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let (ops, frame) = load_and_store(RAM + 8, RAM + 0x1004);
    let block = access_block(ops.clone(), frame, 2, Memory::Inline);
    let mut random = Random(9);
    let mut ram = pattern(&mut random);
    let want = u64::from_le_bytes(ram[8..16].try_into().expect("eight bytes"));
    // An empty TLB that the misses fill: both miss.
    let mut slots = [0u128; 1];
    let ran = compiled_on(
        &mut jit,
        &block,
        &mut slots,
        10,
        &mut ram,
        Some(FastTlb::new()),
    );
    assert_eq!((ran.code, ran.misses, ran.thunk_calls), (DONE, 2, 2));
    assert_eq!(slots[0], u128::from(want));
    assert_eq!(&ram[0x1004..0x1008], &0xeeff_1122u32.to_le_bytes());
    // A TLB that holds both pages: no call at all.
    let mut tlb = FastTlb::new();
    let watch = AtomicU32::new(0);
    let host = ram.as_ptr() as usize;
    tlb.fill(1, false, RAM, host, 0, 0, 12);
    let watch_at = &watch as *const AtomicU32 as usize;
    tlb.fill(1, true, RAM + 0x1000, host + 0x1000, watch_at, 0x1000, 12);
    ram[0x1004..0x1008].fill(0);
    let (reads, writes) = (tlb.table(1, false), tlb.table(1, true));
    let run = |jit: &mut Jit, block: &Block, ram: &mut [u8], writes| {
        run_on_tables(jit, block, &ops, ram, reads, writes)
    };
    assert_eq!(run(&mut jit, &block, &mut ram, writes), (DONE, 0, 0, 0));
    assert_eq!(&ram[0x1004..0x1008], &0xeeff_1122u32.to_le_bytes());
    // The write table of nothing: the store misses alone.
    let empty = tlb::EMPTY_TABLE.as_ptr();
    assert_eq!(run(&mut jit, &block, &mut ram, empty), (DONE, 1, 1, 0));
    // A load that is not aligned misses also in a page that the TLB holds,
    // and one outside the memory leaves the block.
    for (load_at, calls, code) in [
        (RAM + 9, 1, DONE),
        (RAM + 0x1ff8, 1, DONE),
        (RAM - 8, 1, LEFT),
    ] {
        let (ops, frame) = load_and_store(load_at, RAM + 0x1004);
        let block = access_block(ops, frame, 2, Memory::Inline);
        // RAM + 0x1ff8 is in the second page, which has no read entry.
        assert_eq!(
            run(&mut jit, &block, &mut ram, writes),
            (code, calls, calls, 0),
            "{load_at:#x}"
        );
    }
}

#[test]
fn an_unprivileged_access_and_a_block_without_inlining_take_the_thunk() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let load = Op {
        code: Code::Load4,
        aux: 0,
        dst: 2,
        a: 1,
        b: 0,
        imm: 0,
    };
    let frame = vec![0, RAM + 16, 0];
    let mut ram = vec![0u8; PAGES * 4096];
    let mut tlb = FastTlb::new();
    tlb.fill(1, false, RAM, ram.as_ptr() as usize, 0, 0, 12);
    let (reads, writes) = (tlb.table(1, false), tlb.table(1, true));
    for (op, memory) in [
        (
            Op {
                aux: THUNK_ACCESS,
                ..load
            },
            Memory::Inline,
        ),
        (load, Memory::Thunk),
    ] {
        let block = access_block(vec![op], frame.clone(), 2, memory);
        // One call of the operation itself, not a miss. The test thunk
        // runs the plain load.
        assert_eq!(
            run_on_tables(&mut jit, &block, &[load], &mut ram, reads, writes),
            (DONE, 1, 0, 0)
        );
    }
    // The same load inline: a hit.
    let block = access_block(vec![load], frame, 2, Memory::Inline);
    assert_eq!(
        run_on_tables(&mut jit, &block, &[load], &mut ram, reads, writes),
        (DONE, 0, 0, 0)
    );
    // An alignment of 16 in `aux`: an address aligned to 16 hits, one
    // aligned to 8 only misses.
    let aligned = Op { aux: 0x0f, ..load };
    for (address, misses) in [(RAM + 16, 0), (RAM + 24, 1)] {
        let block = access_block(vec![aligned], vec![0, address, 0], 2, Memory::Inline);
        assert_eq!(
            run_on_tables(&mut jit, &block, &[load], &mut ram, reads, writes),
            (DONE, misses, misses, 0),
            "{address:#x}"
        );
    }
}

#[test]
fn a_store_into_a_watched_page_tells_the_thunk_after_its_write() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let page = RAM + 4096 * WATCHED_PAGE as u64;
    let (ops, frame) = load_and_store(RAM + 8, page + 4);
    let block = access_block(ops.clone(), frame, 2, Memory::Inline);
    let mut ram = vec![0u8; PAGES * 4096];
    let host = ram.as_ptr() as usize;
    let offset = WATCHED_PAGE * 4096;
    for odd in [false, true] {
        let watch = AtomicU32::new(2 + u32::from(odd));
        let mut tlb = FastTlb::new();
        tlb.fill(1, false, RAM, host, 0, 0, 12);
        let watch_at = &watch as *const AtomicU32 as usize;
        tlb.fill(1, true, page, host + offset, watch_at, offset as u64, 12);
        let (reads, writes) = (tlb.table(1, false), tlb.table(1, true));
        ram[offset + 4..offset + 8].fill(0);
        let ran = run_on_tables(&mut jit, &block, &ops, &mut ram, reads, writes);
        // The store hit and wrote, and an odd word asked the thunk once.
        let calls = u64::from(odd);
        assert_eq!(ran, (DONE, calls, 0, calls), "odd: {odd}");
        assert_eq!(&ram[offset + 4..offset + 8], &0xeeff_1122u32.to_le_bytes());
    }
}
