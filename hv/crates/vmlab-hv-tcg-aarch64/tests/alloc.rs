//! The heap calls of a translation and of a compile (3e DESIGN.md J11). A
//! program of many blocks runs twice on one CPU, the second time from a
//! copy at another address, so that every block of the second run is
//! translated, and compiled, with the buffers that the first run grew. The
//! heap calls of the second run are counted on this thread, once with the
//! JIT off and once with it on: their difference is what the compiles
//! make, and it is none. What a translation makes is the lists that the
//! block it makes keeps, and the box of the block: 13.5 a block of this
//! program when card J11 measured it, 71.2 before.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use vmlab_hv_tcg::FlatMemory;
use vmlab_hv_tcg_aarch64::{Cpu, State, Stop};

/// Counts the allocations and reallocations of the thread that turned it
/// on.
struct Counting;

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static CALLS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    if ON.with(Cell::get) {
        CALLS.with(|calls| calls.set(calls.get() + 1));
    }
}

// SAFETY: every call goes to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's promise.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's promise.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: the caller's promise.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's promise.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const BASE: u64 = 0x4000_0000;
/// The second copy of the program.
const COPY: u64 = 0x8000;
/// The data that the blocks load and store, past both copies.
const DATA: u64 = 0x1_0000;
const HVC: u32 = 0xd400_0002;

/// One piece of the program, two blocks or three: arithmetic, a load and a store,
/// a pair of each, a comparison with a select, and a conditional branch
/// over one instruction to a branch to the next piece.
const PIECE: [u32; 12] = [
    0x9100_0442, // add x2, x2, #1
    0xf940_0423, // ldr x3, [x1, #8]
    0x8b02_0063, // add x3, x3, x2
    0xf900_0423, // str x3, [x1, #8]
    0xf100_1464, // subs x4, x3, #5
    0x9a83_b045, // csel x5, x2, x3, lt
    0xa941_1c26, // ldp x6, x7, [x1, #16]
    0xa902_1c26, // stp x6, x7, [x1, #32]
    0xeb06_00bf, // cmp x5, x6
    0x5400_0041, // b.ne +8
    0x9100_0508, // add x8, x8, #1
    0x1400_0001, // b +4
];
const PIECES: usize = 300;

/// The heap calls of a run of the copy at `COPY`, after a run of the copy
/// at 0, and the blocks that it translated.
fn second_run(jit: bool) -> (u64, u64, u64) {
    let mut program = Vec::new();
    for _ in 0..PIECES {
        program.extend_from_slice(&PIECE);
    }
    program.push(HVC);
    let code: Vec<u8> = program.iter().flat_map(|word| word.to_le_bytes()).collect();
    let mut data = vec![0u8; DATA as usize + 0x1000];
    data[..code.len()].copy_from_slice(&code);
    data[COPY as usize..COPY as usize + code.len()].copy_from_slice(&code);
    let mut cpu = Cpu::new(FlatMemory::new(BASE, data), State::reset(BASE));
    cpu.set_jit(jit).expect("a code region");
    cpu.set_chaining(false).expect("a code region");
    cpu.state.set_x(1, BASE + DATA);
    assert!(matches!(cpu.run(1_000_000), Stop::Hypercall { .. }));
    let (translations, compiled) = (cpu.translations, cpu.jit_counts().compiled);
    cpu.state.set_pc(BASE + COPY);
    CALLS.with(|calls| calls.set(0));
    ON.with(|on| on.set(true));
    let stop = cpu.run(1_000_000);
    ON.with(|on| on.set(false));
    assert!(matches!(stop, Stop::Hypercall { .. }), "{stop:?}");
    (
        CALLS.with(Cell::get),
        cpu.translations - translations,
        cpu.jit_counts().compiled - compiled,
    )
}

#[test]
fn a_compile_makes_no_heap_call_once_the_buffers_have_grown() {
    let (interpreted, translations, none) = second_run(false);
    let (compiled, again, blocks) = second_run(true);
    assert_eq!(none, 0);
    assert_eq!(translations, again);
    assert!(translations >= PIECES as u64 * 2, "{translations}");
    assert_eq!(
        blocks, translations,
        "every block of the second run is compiled"
    );
    eprintln!(
        "{translations} blocks: {interpreted} heap calls without the JIT ({:.1} a block), \
         {compiled} with it ({:.1} a block)",
        interpreted as f64 / translations as f64,
        compiled as f64 / translations as f64,
    );
    assert_eq!(
        compiled, interpreted,
        "the compiles of {blocks} blocks made heap calls"
    );
    assert!(
        interpreted < translations * 16,
        "a translation makes heap calls beside the lists of its block"
    );
}
