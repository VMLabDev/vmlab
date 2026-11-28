//! The emitter against the interpreter of the block form: every inline
//! form on random operands, and random blocks long enough to spill, with
//! thunk calls, guards, and loads and stores over the fast TLB between the
//! operations.

use super::*;
use crate::asm::Cond;
use emit::THUNK_ACCESS;
use std::sync::atomic::AtomicU32;
use tlb::FastTlb;
use vmlab_hv_tcg_ir::block::{
    Code, Generic, Kind as GenericKind, Op, Place, Slot, op_in_frame, step_unchecked,
};
use vmlab_hv_tcg_ir::native::{Binary, ScalarType, Unary};

/// The guest memory of the tests: `PAGES` pages at the virtual address
/// `RAM`. The page `WATCHED_PAGE` has an odd watch word.
const RAM: u64 = 0x7000_0000;
const PAGES: usize = 4;
const WATCHED_PAGE: usize = 2;
/// The slots of the random blocks that hold one word (`Shape::narrow`):
/// every other one. Their high halves start at 0.
const NARROW: u128 = 0x5555;

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
        Code::LoadPair => (16, false),
        Code::StorePair => (16, true),
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
    if bytes == 16 {
        // A pair: the result in `dst` and the word after it, the value in
        // `b` (low) and `dst` (high).
        if store {
            part[..8].copy_from_slice(&frame[usize::from(op.b)].to_le_bytes());
            part[8..].copy_from_slice(&frame[usize::from(op.dst)].to_le_bytes());
        } else {
            let (low, high) = part.split_at(8);
            frame[usize::from(op.dst)] = u64::from_le_bytes(low.try_into().expect("8 bytes"));
            frame[usize::from(op.dst) + 1] = u64::from_le_bytes(high.try_into().expect("8 bytes"));
        }
        return true;
    }
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

const PURE: [Code; 25] = [
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
    Code::ReverseBits,
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
        | Code::ReverseBits
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
    generics: Vec<Generic>,
    frame: *mut u64,
    words: usize,
    calls: Vec<Effect>,
    thunk_calls: u64,
    /// The helper calls that compiled code made itself (`Effect::direct`).
    direct_calls: u64,
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
    logged: Vec<(u64, u8, u128, bool)>,
    /// What a miss of the jump cache (`JUMP`) gets: the code to go on to,
    /// or 0. And the count of those calls.
    serve: u64,
    jumps: u64,
}

impl Env {
    /// The fields beside the block: no memory, an empty TLB.
    fn base() -> Self {
        Self {
            ops: Vec::new(),
            generics: Vec::new(),
            frame: std::ptr::null_mut(),
            words: 0,
            calls: Vec::new(),
            thunk_calls: 0,
            direct_calls: 0,
            left_at: None,
            ram: std::ptr::null_mut(),
            tlb: FastTlb::new(),
            watch: Vec::new(),
            fill: false,
            misses: 0,
            watched: 0,
            logged: Vec::new(),
            serve: 0,
            jumps: 0,
        }
    }
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
unsafe extern "sysv64" fn thunk(env: *mut c_void, index: u64, _block: *mut c_void) -> u64 {
    // SAFETY: the test passes its `Env`.
    let env = unsafe { &mut *env.cast::<Env>() };
    env.thunk_calls += 1;
    if index == JUMP {
        env.jumps += 1;
        return env.serve;
    }
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
                let value = match (bytes, store) {
                    (16, true) => {
                        u128::from(frame[usize::from(op.b)])
                            | u128::from(frame[usize::from(op.dst)]) << 64
                    }
                    (16, false) => {
                        u128::from(frame[usize::from(op.dst)])
                            | u128::from(frame[usize::from(op.dst) + 1]) << 64
                    }
                    (_, true) => u128::from(frame[usize::from(op.b)]),
                    (_, false) => u128::from(frame[usize::from(op.dst)]),
                };
                let mask = u128::MAX >> (128 - 8 * u32::from(bytes));
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
                .fill(1, store, va, host, watch, page as u64 * 4096, 12, false);
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
            assert!(op_in_frame(&op, frame.len(), env.generics.len()));
            // SAFETY: checked just above.
            assert!(unsafe { step_unchecked(&op, frame, &env.generics) });
            0
        }
    }
}

/// The function that compiled code calls for a helper call with
/// `Effect::direct`: the call of the index `argument`, as the thunk runs it.
///
/// # Safety
/// As for `thunk`.
unsafe extern "sysv64" fn direct(env: *mut c_void, argument: u64, _block: *mut c_void) -> u64 {
    // SAFETY: the test passes its `Env`.
    let env = unsafe { &mut *env.cast::<Env>() };
    env.direct_calls += 1;
    // SAFETY: the frame of the test, as long as its `words`.
    let frame = unsafe { std::slice::from_raw_parts_mut(env.frame, env.words) };
    call(&env.calls[argument as usize], frame);
    0
}

/// The function of a helper call with `Effect::registers` that reads `N`
/// words: what `call` computes, from the argument registers.
extern "sysv64" fn fold<const N: usize>(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> Words {
    let mut value = 0x9e37_79b9_7f4a_7c15u64;
    for word in [a, b, c, d, e, f].into_iter().take(N) {
        value = value.rotate_left(7) ^ word;
    }
    Words {
        low: value.wrapping_mul(3),
        high: value.wrapping_mul(4),
    }
}

/// The address of `fold` for `count` words.
fn fold_of(count: usize) -> u64 {
    let functions: [RegisterFn; 7] = [
        fold::<0>, fold::<1>, fold::<2>, fold::<3>, fold::<4>, fold::<5>, fold::<6>,
    ];
    functions[count] as usize as u64
}

/// A block of the test.
struct Block {
    ops: Vec<Op>,
    generics: Vec<Generic>,
    frame: Vec<u64>,
    calls: Vec<Effect>,
    leaves: Vec<Vec<Slot>>,
    inputs: Vec<(Place, u32)>,
    outputs: Vec<(u32, Place)>,
    memory: Memory,
    /// `Shape::narrow`.
    narrow: u128,
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
                assert!(op_in_frame(op, frame.len(), block.generics.len()));
                // SAFETY: checked just above.
                assert!(unsafe { step_unchecked(op, &mut frame, &block.generics) });
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
    logged: Vec<(u64, u8, u128, bool)>,
    direct_calls: u64,
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
        generics: &block.generics,
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
        sides: &[],
        narrow: block.narrow,
    };
    let entry = jit.compile(&shape).expect("a block that the emitter takes");
    let mut frame = block.frame.clone();
    let fill = tlb.is_some();
    let mut env = Env {
        ops: block.ops.clone(),
        generics: block.generics.clone(),
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
        direct_calls: env.direct_calls,
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
                generics: Vec::new(),
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
                narrow: 0,
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
    // A pair hits inline where its 16 bytes are in its page, aligned or
    // not (N10), and misses where it crosses into the next.
    if bytes == 16 && random.below(3) == 0 {
        return page + random.below(512) * 8 + random.below(2) * random.below(8);
    }
    match random.below(16) {
        0 => page + 4096 - u64::from(bytes) / 2 - 1 + u64::from(bytes == 1),
        1 => page + random.below(4096 - u64::from(bytes)),
        2 => RAM - 4096 + random.below(4096),
        3 => RAM + (PAGES * 4096) as u64 - u64::from(bytes) / 2,
        _ => page + random.below(4096 / u64::from(bytes)) * u64::from(bytes),
    }
}

/// The mask of the low `width` bits of a value of two words.
fn mask128(width: u16) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

/// A random generic operation into the words from `dst` on, over the words
/// of `defined` and the values of two words from the words of `wides`, as
/// the lowering makes one: the kinds that the emitter has a form for, and
/// some that it has none for, which run through the thunk.
fn random_generic(random: &mut Random, defined: &[Slot], wides: &[Slot], dst: Slot) -> Generic {
    let pick = |random: &mut Random, list: &[Slot]| list[random.below(list.len() as u64) as usize];
    let place = |random: &mut Random, wide: bool| {
        if wide && !wides.is_empty() {
            Place {
                slot: pick(random, wides),
                wide: true,
            }
        } else {
            Place {
                slot: pick(random, defined),
                wide: false,
            }
        }
    };
    // A place of two words where `random.below(n) == 0` is `when`.
    let place_by = |random: &mut Random, n: u64, when: bool| {
        let wide = (random.below(n) == 0) == when;
        place(random, wide)
    };
    let width = 1 + random.below(128) as u16;
    let wide = width > 64;
    let ty = ScalarType::Bits(width);
    let out = Place { slot: dst, wide };
    let none = Place::default();
    match random.below(12) {
        0..=3 => {
            let operation = [
                Binary::And,
                Binary::Or,
                Binary::Xor,
                Binary::Add,
                Binary::Sub,
                Binary::Mul,
                Binary::ShiftLeft,
            ][random.below(7) as usize];
            let (a, b) = (place(random, wide), place(random, wide));
            Generic::new(GenericKind::Binary(operation), ty, 0, [a, b, none], out)
        }
        4 => {
            let operation = [Binary::Equal, Binary::NotEqual][random.below(2) as usize];
            let (a, b) = (place_by(random, 2, true), place_by(random, 2, true));
            let out = Place {
                slot: dst,
                wide: false,
            };
            Generic::new(
                GenericKind::Binary(operation),
                ScalarType::Bool,
                0,
                [a, b, none],
                out,
            )
        }
        5 | 6 => {
            let offset = random.below(140) as u16;
            let a = place_by(random, 4, false);
            Generic::new(
                GenericKind::Slice { offset },
                ty,
                mask128(width),
                [a, none, none],
                out,
            )
        }
        7 | 8 => {
            let low_width = random.below(130) as u16;
            let (a, b) = (place_by(random, 2, true), place_by(random, 2, true));
            Generic::new(
                GenericKind::Concat { low_width },
                ty,
                mask128(width),
                [a, b, none],
                out,
            )
        }
        9 => {
            let operation = [
                Unary::BitNot,
                Unary::Truncate(1 + random.below(128) as u16),
                Unary::PopCount(1 + random.below(128) as u16),
            ][random.below(3) as usize];
            let a = place_by(random, 2, true);
            Generic::new(
                GenericKind::Unary(operation),
                ty,
                mask128(width),
                [a, none, none],
                out,
            )
        }
        _ => {
            let condition = place(random, false);
            let (b, c) = (place(random, wide), place(random, wide));
            Generic::new(GenericKind::Select, ty, 0, [condition, b, c], out)
        }
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
    let mut generics = Vec::new();
    let mut calls = Vec::new();
    let mut leaves = Vec::new();
    let mut defined: Vec<Slot> = (1..1 + inputs + constants).collect();
    // Two words that follow each other are a value of two words.
    let mut wides: Vec<Slot> = (1..inputs + constants).collect();
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
        // The comparisons that a select or a guard reads (12 and 13) are in
        // the blocks of the thunk, the loads and stores (12 to 16) in the
        // others, and the generic operations (14 and 15, 17 and 18) in both.
        match random.below(if memory == Memory::Thunk { 16 } else { 21 }) {
            12..=16 if memory != Memory::Thunk => {
                // A load or a store at an address of a constant word, with
                // a leave like a guard's.
                let bytes = [1u8, 2, 4, 8, 16][random.below(5) as usize];
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
                    (16, false) => Code::LoadPair,
                    (1, true) => Code::Store1,
                    (2, true) => Code::Store2,
                    (4, true) => Code::Store4,
                    (8, true) => Code::Store8,
                    _ => Code::StorePair,
                };
                let (dst, b) = match (bytes, store) {
                    // A pair stores `b` (low) and `dst` (high).
                    (16, true) => (any(random, &defined), any(random, &defined)),
                    (_, true) => (0, any(random, &defined)),
                    (16, false) => {
                        frame.extend([0, 0]);
                        (at + 1, 0)
                    }
                    (_, false) => {
                        frame.push(0);
                        (at + 1, 0)
                    }
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
                    if bytes == 16 {
                        defined.push(dst + 1);
                        wides.push(dst);
                    }
                }
            }
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
                    [
                        Code::Equal,
                        Code::NotEqual,
                        Code::Less,
                        Code::LessEqual,
                        Code::IsZero,
                    ][random.below(5) as usize]
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
            14..=18 => {
                let generic = random_generic(random, &defined, &wides, dst);
                let out = generic.dst();
                frame.push(0);
                defined.push(dst);
                if out.wide {
                    frame.push(0);
                    defined.push(dst + 1);
                    wides.push(dst);
                }
                ops.push(Op {
                    code: Code::Generic,
                    aux: 0,
                    dst: 0,
                    a: 0,
                    b: 0,
                    imm: generics.len() as u64,
                });
                generics.push(generic);
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
                // Half of the calls go to their own function (H12), and a
                // third of those with their words in registers (J13).
                let how = random.below(6);
                let direct =
                    (how < 3).then_some((direct as Thunk as usize as u64, calls.len() as u64));
                let registers = (how == 0).then(|| Registers {
                    function: fold_of(reads.len()),
                    args: reads.clone(),
                    results: writes.clone(),
                });
                calls.push(Effect {
                    reads,
                    writes,
                    leave: None,
                    direct,
                    condition: None,
                    registers,
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
        // Some outputs are values of two words.
        let place = if random.below(4) == 0 {
            Place {
                slot: wides[random.below(wides.len() as u64) as usize],
                wide: true,
            }
        } else {
            Place {
                slot: defined[random.below(defined.len() as u64) as usize],
                wide: false,
            }
        };
        outputs.push((register, place));
    }
    Block {
        ops,
        generics,
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
        narrow: NARROW,
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
                                imm: if flip == 2 {
                                    u64::MAX
                                } else {
                                    u64::from(u32::MAX)
                                },
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
                            generics: Vec::new(),
                            frame,
                            calls: Vec::new(),
                            leaves,
                            inputs: vec![(word(1), 0), (word(2), 1), (word(9), 2), (word(10), 3)],
                            outputs: vec![(0, word(11)), (1, word(1))],
                            memory: Memory::Thunk,
                            narrow: 0,
                        };
                        let mut slots = [
                            u128::from(x),
                            u128::from(y),
                            u128::from(random.value()),
                            u128::from(random.value()),
                        ];
                        let mut expected = slots;
                        let (code_expected, words) = interpret(&block, &mut expected, &mut []);
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
    assert!(
        jit.stats.fused >= rounds,
        "{} of {rounds} fused",
        jit.stats.fused
    );
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
                    cases: vec![(9, test), (3, FlagTest::Table(0x0ff0))].into(),
                };
                let calls = vec![Effect {
                    reads: vec![kind_word, 2, 3, 4],
                    writes: vec![5],
                    leave: None,
                    direct: None,
                    condition: Some(lazy),
                    registers: None,
                }];
                let block = Block {
                    generics: Vec::new(),
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
                    memory: Memory::Thunk,
                    narrow: 0,
                };
                let mut slots = [u128::from(kind), u128::from(a), u128::from(b)];
                let mut expected = slots;
                interpret(&block, &mut expected, &mut []);
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
        cases: vec![(1, FlagTest::Always(true))].into(),
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
        generics: Vec::new(),
        frame: vec![0, 0, 0, 0, 11, 0],
        calls: vec![Effect {
            reads: vec![1, 2, 3, 4],
            writes: vec![5],
            leave: None,
            direct: None,
            condition: Some(lazy),
            registers: None,
        }],
        leaves: Vec::new(),
        inputs: vec![(word(1), 0), (word(2), 1), (word(3), 2)],
        outputs: vec![(0, word(5))],
        memory: Memory::Thunk,
        narrow: 0,
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
        generics: Vec::new(),
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
        narrow: 0,
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
        sides: &[],
        narrow: 0,
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
    // RBIT needs no extension of the host (3e J6).
    assert!(jit.compile(&shape).is_ok());
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
            generics: &block.generics,
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
            sides: &[],
            narrow: 0,
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
        generics: &block.generics,
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
        sides: &[],
        narrow: 0,
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
            generics: &block.generics,
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
            sides: &[],
            narrow: 0,
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
        sides: &[],
        narrow: 0,
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
    /// A miss of the jump cache asks the thunk (`Chain::serve`).
    serve: bool,
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
            serve: false,
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
                serve: self.serve,
                terminal: false,
            }),
            next: Some(Next { pc: 1, kind }),
            memory: Memory::Thunk,
            sides: &[],
            narrow: 0,
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
        let (code, context, _) = self.run_served(jit, entry, slots, budget, 0);
        (code, context)
    }

    /// `run`, with a thunk that answers a miss of the jump cache with
    /// `serve`: the code, the context after, and the count of those misses.
    fn run_served(
        &mut self,
        jit: &Jit,
        entry: &Entry,
        slots: &mut [u128; 3],
        budget: i64,
        serve: u64,
    ) -> (u64, Context, u64) {
        let mut env = Env {
            ops: self.ops.clone(),
            frame: self.frame.as_mut_ptr(),
            words: self.frame.len(),
            calls: Vec::new(),
            serve,
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
        (code, context, env.jumps)
    }
}

#[test]
fn a_miss_of_the_jump_cache_asks_the_thunk_and_goes_on_to_the_code_it_gives() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let mut block = Looping::new();
    block.serve = true;
    let entry = block.compile(&mut jit, NextPc::Absolute(word(5)), 9);
    let pc = 0x4000_0100u64;
    let mut slots = [0u128, u128::from(pc), u128::from(pc)];
    // The thunk has no code: the block leaves with `LOOKUP`, after one ask.
    let (code, context, jumps) = block.run_served(&jit, &entry, &mut slots, 100, 0);
    assert_eq!((code, context.hits, jumps), (LOOKUP, 0, 1));
    assert_eq!(slots[0], 1);
    // The thunk gives the code: the run goes on until its budget, with no
    // return to the owner, every turn through the thunk.
    let (code, context, jumps) = block.run_served(&jit, &entry, &mut slots, 20, entry.address);
    assert_eq!(
        (code, context.budget, context.hits, jumps),
        (BUDGET, 0, 0, 20)
    );
    assert_eq!(slots[0], 21);
    // A filled entry hits inline and asks nothing.
    jit.fill(pc, 9, &entry);
    let (code, context, jumps) = block.run_served(&jit, &entry, &mut slots, 20, 0);
    assert_eq!((code, context.hits, jumps), (BUDGET, 20, 0));
    // A block that does not ask leaves at a miss without a call.
    let mut plain = Looping::new();
    let entry = plain.compile(&mut jit, NextPc::Absolute(word(5)), 9);
    slots[2] = u128::from(pc + 4);
    let (code, _, jumps) = plain.run_served(&jit, &entry, &mut slots, 20, entry.address);
    assert_eq!((code, jumps), (LOOKUP, 0));
}

#[test]
fn an_entry_of_the_jump_cache_hits_only_in_its_address_space() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let mut block = Looping::new();
    let entry = block.compile(&mut jit, NextPc::Absolute(word(5)), 9);
    let pc = 0x4000_0100u64;
    let mut slots = [0u128, u128::from(pc), u128::from(pc)];
    jit.set_jump_space(1);
    jit.fill(pc, 9, &entry);
    let (code, context) = block.run(&jit, &entry, &mut slots, 10);
    assert_eq!((code, context.hits), (BUDGET, 10));
    // Another space: a miss, inline and in `jump_target`.
    jit.set_jump_space(2);
    assert_eq!(jit.jump_target(pc, 9), None);
    let (code, context) = block.run(&jit, &entry, &mut slots, 10);
    assert_eq!((code, context.hits), (LOOKUP, 0));
    // The first space again: the entry was kept.
    jit.set_jump_space(1);
    assert_eq!(jit.jump_target(pc, 9), Some(entry.address));
    let (code, context) = block.run(&jit, &entry, &mut slots, 10);
    assert_eq!((code, context.hits), (BUDGET, 10));
    assert_eq!(jit.stats.spaces, 3);
    // An emptied cache misses in every space.
    jit.clear_jumps();
    assert_eq!(jit.jump_target(pc, 9), None);
    // The largest space keeps the bits of the key and of the generation.
    jit.set_jump_space(u16::MAX);
    jit.fill(pc, 9, &entry);
    assert_eq!(jit.jump_target(pc, 9), Some(entry.address));
    assert_eq!(jit.jump_target(pc, 8), None);
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
fn a_full_region_drops_the_part_written_longest_ago_and_the_links_into_it() {
    // X goes into the first part, A and B into the second. A is linked to
    // X and B to A. The drop of the first part sets the jump of A back to
    // its stub and keeps B linked to A (H13c, card J11).
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let mut x = Looping::new();
    let mut a = Looping::new();
    let mut b = Looping::new();
    let mut filler = Looping::new();
    let relative = NextPc::Relative(word(4));
    let to_x = x.compile(&mut jit, relative, 9);
    let mut probe = to_x;
    while probe.generation == to_x.generation {
        probe = filler.compile(&mut jit, NextPc::Absolute(word(5)), 9);
    }
    let to_a = a.compile(&mut jit, relative, 9);
    let to_b = b.compile(&mut jit, relative, 9);
    assert_eq!(to_a.generation, to_b.generation);
    assert!(jit.link(&to_a, 0, &to_x));
    assert!(jit.link(&to_b, 0, &to_a));
    let mut slots = [0u128, 0x4000_0100, 0];
    // A runs and jumps to X, whose head finds the budget spent.
    let (code, context) = a.run(&jit, &to_a, &mut slots, 1);
    assert_eq!((code, context.chained), (BUDGET, 1));
    assert_eq!(jit.stats.resets, 0);
    while jit.stats.resets == 0 {
        filler.compile(&mut jit, NextPc::Absolute(word(5)), 9);
    }
    assert!(!jit.current(&to_x));
    assert!(jit.current(&to_a) && jit.current(&to_b));
    assert_eq!(jit.linked(), 1);
    // A leaves at its stub, and B still goes on to A.
    let (code, context) = a.run(&jit, &to_a, &mut slots, 10);
    assert_eq!((code, context.budget), (CHAIN, 9));
    let (code, context) = b.run(&jit, &to_b, &mut slots, 10);
    assert_eq!((code, context.budget, context.chained), (CHAIN, 8, 2));
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
    access_rounds(0x3e_0003, false);
}

/// Card #1213: loads and stores whose address an `Add` before them
/// computes, which the emitter takes into the access, leave what the
/// interpreter leaves: on a hit, on a miss, logged, into the watched page,
/// and through their leave.
#[test]
fn loads_and_stores_that_fold_their_address_leave_what_the_interpreter_leaves() {
    access_rounds(0x3e_1213, true);
}

/// Random blocks with loads and stores over the fast TLB against the
/// interpreter, with the addresses of `with_sums` when `fold`.
fn access_rounds(seed: u64, fold: bool) {
    let mut jit = Jit::with_capacity(16 << 20).expect("a code region");
    let mut random = Random(seed);
    let (mut hits, mut misses, mut watched, mut left) = (0u64, 0u64, 0u64, 0);
    let mut unaligned = 0u64;
    for round in 0..3000 {
        let count = 1 + random.below(if round % 10 == 0 { 300 } else { 60 }) as usize;
        let memory = if round % 4 == 0 {
            Memory::Logged
        } else {
            Memory::Inline
        };
        let mut block = random_block_with(&mut random, count, memory);
        if fold {
            block = with_sums(&mut random, block);
        }
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
                tlb.fill(1, false, va, host, 0, page as u64 * 4096, 12, false);
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
        // Hits at an address that is not a multiple of the size (N10).
        unaligned += ran
            .logged
            .iter()
            .filter(|&&(va, bytes, ..)| va % u64::from(bytes.min(8)) != 0)
            .count() as u64;
        watched += ran.watched;
    }
    assert!(left > 100, "{left} blocks left");
    assert!(unaligned > 20, "{unaligned} logged hits not aligned");
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
        generics: Vec::new(),
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
        narrow: 0,
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
        sides: &[],
        narrow: 0,
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
    let code = unsafe {
        jit.enter(
            &entry,
            &raw mut context,
            slots.as_mut_ptr(),
            frame.as_mut_ptr(),
        )
    };
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
    tlb.fill(1, false, RAM, host, 0, 0, 12, false);
    let watch_at = &watch as *const AtomicU32 as usize;
    tlb.fill(
        1,
        true,
        RAM + 0x1000,
        host + 0x1000,
        watch_at,
        0x1000,
        12,
        false,
    );
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
    // A load that is not aligned hits in a page that the TLB holds (N10),
    // one that crosses into a page that it does not hold misses, and one
    // outside the memory leaves the block.
    for (load_at, calls, code) in [
        (RAM + 9, 0, DONE),
        (RAM + 0xff8, 0, DONE),
        (RAM + 0xffc, 1, DONE),
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
    tlb.fill(1, false, RAM, ram.as_ptr() as usize, 0, 0, 12, false);
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
    // The owner's bits of `aux` (an alignment of 16 here) do not change
    // what hits: every address whose bytes are in the page (N10).
    let aligned = Op { aux: 0x0f, ..load };
    for (address, misses) in [
        (RAM + 16, 0),
        (RAM + 24, 0),
        (RAM + 0xffc, 0),
        (RAM + 0xffd, 1),
    ] {
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
        tlb.fill(1, false, RAM, host, 0, 0, 12, false);
        let watch_at = &watch as *const AtomicU32 as usize;
        tlb.fill(
            1,
            true,
            page,
            host + offset,
            watch_at,
            offset as u64,
            12,
            false,
        );
        let (reads, writes) = (tlb.table(1, false), tlb.table(1, true));
        ram[offset + 4..offset + 8].fill(0);
        let ran = run_on_tables(&mut jit, &block, &ops, &mut ram, reads, writes);
        // The store hit and wrote, and an odd word asked the thunk once.
        let calls = u64::from(odd);
        assert_eq!(ran, (DONE, calls, 0, calls), "odd: {odd}");
        assert_eq!(&ram[offset + 4..offset + 8], &0xeeff_1122u32.to_le_bytes());
    }
}

/// The generic operations of a block of one operation, on random values of
/// one and two words: the forms of two words (3e J6) leave what the
/// interpreter leaves, and a kind without a form runs through the thunk.
#[test]
fn every_generic_form_computes_what_the_interpreter_computes() {
    let mut jit = Jit::with_capacity(4 << 20).expect("a code region");
    let mut random = Random(0x3e_0006);
    let (mut inline, mut through) = (0u64, 0u64);
    for round in 0..20_000 {
        // Words 1 to 6 are inputs, 7 to 12 constants, 13 and 14 the
        // result.
        let mut frame = vec![0u64; 15];
        for word in &mut frame[7..13] {
            *word = random.value();
        }
        let defined: Vec<Slot> = if round % 3 == 0 {
            (1..13).collect()
        } else {
            (1..7).collect()
        };
        let wides: Vec<Slot> = defined[..defined.len() - 1].to_vec();
        let generic = random_generic(&mut random, &defined, &wides, 13);
        let block = Block {
            ops: vec![Op {
                code: Code::Generic,
                aux: 0,
                dst: 0,
                a: 0,
                b: 0,
                imm: 0,
            }],
            generics: vec![generic],
            frame,
            calls: Vec::new(),
            leaves: Vec::new(),
            inputs: (1..7).map(|n| (word(n), u32::from(n))).collect(),
            outputs: vec![(0, generic.dst())],
            memory: Memory::Thunk,
            narrow: 0,
        };
        let mut slots = [0u128; 7];
        for slot in &mut slots[1..] {
            *slot = u128::from(random.value());
        }
        let mut expected = slots;
        interpret(&block, &mut expected, &mut []);
        let mut got = slots;
        let (_, _, thunk_calls) = compiled(&mut jit, &block, &mut got, 100);
        assert_eq!(got, expected, "round {round}: {generic:?} on {slots:x?}");
        if thunk_calls == 0 {
            inline += 1;
        } else {
            through += 1;
        }
    }
    assert!(
        inline > 10_000 && through > 1000,
        "{inline} inline, {through} through the thunk"
    );
}

/// A helper call with `Effect::direct` is a call of its own function, not
/// of the thunk (H12), and computes what the thunk computes.
#[test]
fn a_direct_call_runs_its_function_and_not_the_thunk() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    let effect = Effect {
        reads: vec![1, 2],
        writes: vec![3, 4],
        leave: None,
        direct: Some((direct as Thunk as usize as u64, 0)),
        condition: None,
        registers: None,
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
        generics: Vec::new(),
        frame: vec![0; 5],
        calls: vec![effect],
        leaves: Vec::new(),
        inputs: vec![(word(1), 1), (word(2), 2)],
        outputs: vec![(0, word(3)), (3, word(4))],
        memory: Memory::Thunk,
        narrow: 0,
    };
    let slots = [0, 0x1234, 0x5678_9abc_def0, 7];
    let mut expected = slots;
    interpret(&block, &mut expected, &mut []);
    let mut got = slots;
    let mut ram = vec![0u8; PAGES * 4096];
    let ran = compiled_on(&mut jit, &block, &mut got, 10, &mut ram, None);
    assert_eq!((ran.code, got), (DONE, expected));
    assert_eq!((ran.thunk_calls, ran.direct_calls), (0, 1));
}

/// A helper call with `Effect::registers` takes six words in the argument
/// registers and gives two in RAX and RDX, with no thunk and no direct
/// call (3e DESIGN.md J13), while other words stay live in the registers
/// that a call does not keep. A call whose words do not fit the effect
/// goes through `direct` as before.
#[test]
fn a_register_call_takes_its_words_in_registers() {
    for fits in [true, false] {
        let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
        let reads: Vec<Slot> = vec![1, 2, 3, 4, 5, 6];
        let effect = Effect {
            reads: reads.clone(),
            writes: vec![7, 8],
            leave: None,
            direct: Some((direct as Thunk as usize as u64, 0)),
            condition: None,
            registers: Some(Registers {
                function: fold_of(6),
                args: reads,
                // A word that the effect does not write: the call does not fit.
                results: if fits { vec![7, 8] } else { vec![7, 9] },
            }),
        };
        // Words 1 to 6 are read again after the call, from the registers
        // that the allocator gave them.
        let mut ops = vec![Op {
            code: Code::Call,
            aux: 0,
            dst: 0,
            a: 0,
            b: 0,
            imm: 0,
        }];
        for n in 0..6u16 {
            ops.push(Op {
                code: Code::Add,
                aux: 0,
                dst: 10 + n,
                a: 1 + n,
                b: 7 + n % 2,
                imm: 0,
            });
        }
        let block = Block {
            ops,
            generics: Vec::new(),
            frame: vec![0; 16],
            calls: vec![effect],
            leaves: Vec::new(),
            inputs: (1..=6).map(|n| (word(n), u32::from(n))).collect(),
            outputs: (0..6).map(|n| (n, word(10 + n as Slot))).collect(),
            memory: Memory::Thunk,
            narrow: 0,
        };
        let slots: Vec<u128> = (0..8).map(|n| 0x1111_2222_3333_4444 * (n + 1)).collect();
        let mut expected = slots.clone();
        interpret(&block, &mut expected, &mut []);
        let mut got = slots;
        let mut ram = vec![0u8; PAGES * 4096];
        let ran = compiled_on(&mut jit, &block, &mut got, 10, &mut ram, None);
        assert_eq!((ran.code, &got), (DONE, &expected), "fits: {fits}");
        let calls = if fits { (0, 0) } else { (0, 1) };
        assert_eq!((ran.thunk_calls, ran.direct_calls), calls, "fits: {fits}");
    }
}

/// `block` with the constant address of most of its loads and stores
/// computed before the access by an `Add`, as the lifter computes a base
/// register plus an offset (card #1213): a base and a displacement in either
/// order, small, large or past 32 bits, or a base and an index. The base and
/// the index are results of an operation, so that they are in registers.
/// Some leaves read the sum, which keeps it from the access, or the base,
/// which keeps the base alive past it.
fn with_sums(random: &mut Random, mut block: Block) -> Block {
    fn push(frame: &mut Vec<u64>, value: u64) -> Slot {
        frame.push(value);
        (frame.len() - 1) as Slot
    }
    // A word that no constant holds: the XOR of two constants.
    fn computed(frame: &mut Vec<u64>, out: &mut Vec<Op>, value: u64, random: &mut Random) -> Slot {
        let key = random.value();
        let (a, b) = (push(frame, value ^ key), push(frame, key));
        let dst = push(frame, 0);
        out.push(Op {
            code: Code::Xor,
            aux: 0,
            dst,
            a,
            b,
            imm: u64::MAX,
        });
        dst
    }
    let ops = std::mem::take(&mut block.ops);
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        if access_size(op.code).is_none() || random.below(5) == 0 {
            out.push(op);
            continue;
        }
        let address = block.frame_value(op.a);
        let displacement = match random.below(4) {
            0 => 0,
            1 => random.below(256),
            2 => random.below(1 << 20).wrapping_sub(1 << 19),
            _ => random.value(),
        };
        let frame = &mut block.frame;
        let base = computed(frame, &mut out, address.wrapping_sub(displacement), random);
        let other = if random.below(3) == 0 {
            computed(frame, &mut out, displacement, random)
        } else {
            push(frame, displacement)
        };
        let sum = push(frame, 0);
        let (a, b) = if random.below(2) == 0 {
            (base, other)
        } else {
            (other, base)
        };
        out.push(Op {
            code: Code::Add,
            aux: 0,
            dst: sum,
            a,
            b,
            imm: u64::MAX,
        });
        match random.below(6) {
            0 => block.leaves[op.imm as usize].push(sum),
            1 => block.leaves[op.imm as usize].push(base),
            _ => {}
        }
        out.push(Op { a: sum, ..op });
    }
    block.ops = out;
    block
}

impl Block {
    /// The constant of the word `slot` of the frame.
    fn frame_value(&self, slot: Slot) -> u64 {
        self.frame[usize::from(slot)]
    }
}

/// Card #1213: an output of one word to a narrow slot writes its low half
/// and leaves the high half, which the owner keeps at 0, as it is; to any
/// other slot it writes the high half with 0. A wide output writes both.
#[test]
fn an_output_to_a_narrow_slot_makes_no_store_to_its_high_half() {
    let mut jit = Jit::with_capacity(1 << 20).expect("a code region");
    for narrow in [0u128, 0b11] {
        let block = Block {
            ops: Vec::new(),
            generics: Vec::new(),
            frame: vec![0, 5, 6, 7],
            calls: Vec::new(),
            leaves: Vec::new(),
            inputs: Vec::new(),
            outputs: vec![
                (0, word(1)),
                (
                    1,
                    Place {
                        slot: 2,
                        wide: true,
                    },
                ),
                (2, word(1)),
            ],
            memory: Memory::Thunk,
            narrow,
        };
        let high = 0xdead_u128 << 64;
        let mut slots = [high; 3];
        let (code, _, _) = compiled(&mut jit, &block, &mut slots, 10);
        assert_eq!(code, DONE);
        let kept = if narrow & 1 != 0 { high } else { 0 };
        assert_eq!(slots, [kept | 5, 7 << 64 | 6, 5], "narrow {narrow:#b}");
    }
}
