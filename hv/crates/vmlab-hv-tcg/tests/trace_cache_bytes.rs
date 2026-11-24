//! What the trace cache of one hart keeps, in bytes of host memory.
//!
//! This binary holds one test, because it counts every live heap byte of the
//! process through its own global allocator: a second test running beside it
//! would be counted too.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use vmlab_hv_tcg::{FlatMemory, Memory, Riscv64, TRACE_CACHE_BYTES};

static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every call is passed to `System` unchanged, which upholds the
// contract of `GlobalAlloc`. The counter is only a side record.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        let moved = unsafe { System.realloc(pointer, layout, size) };
        if !moved.is_null() {
            LIVE.fetch_add(size, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const BASE: u64 = 0x8000_0000;
const INSTRUCTIONS: u64 = 3200;
const MIB: usize = 1 << 20;

/// `jal x0, offset`.
fn jump(offset: i32) -> u32 {
    let imm = offset as u32;
    ((imm >> 20) & 1) << 31
        | ((imm >> 1) & 0x3ff) << 21
        | ((imm >> 11) & 1) << 20
        | ((imm >> 12) & 0xff) << 12
        | 0x6f
}

/// A straight run of `addi x10, x10, 1` and a jump back to its start: every
/// trace the hart records starts at a PC no earlier trace started at.
fn straight_run() -> Riscv64<FlatMemory> {
    let mut memory = FlatMemory::new(BASE, vec![0; 32 * 1024]);
    for index in 0..INSTRUCTIONS {
        memory
            .write(BASE + index * 4, &0x0015_0513u32.to_le_bytes())
            .unwrap();
    }
    memory
        .write(
            BASE + INSTRUCTIONS * 4,
            &jump(-(INSTRUCTIONS as i32 * 4)).to_le_bytes(),
        )
        .unwrap();
    let mut cpu = Riscv64::new(memory).unwrap();
    cpu.set_pc(BASE);
    cpu.specialize = true;
    cpu
}

#[test]
fn the_trace_cache_stays_within_a_byte_budget() {
    // One and a half times the cache's own limit. The limit bounds what the
    // cache counts, the count is an estimate, and a trace is recorded before
    // an older one is evicted for it. Without a limit in bytes this run holds
    // about 71 MiB, and twice that for twice the steps.
    let budget = TRACE_CACHE_BYTES + TRACE_CACHE_BYTES / 2;
    assert!(
        budget <= 48 * MIB,
        "the budget of one hart is {budget} bytes"
    );

    let mut cpu = straight_run();
    // One step outside the measurement: the first step builds what every
    // hart builds once, whatever the guest does.
    cpu.run(1).unwrap();
    let before = LIVE.load(Ordering::Relaxed);
    let mut largest = 0usize;
    while cpu.steps < INSTRUCTIONS {
        cpu.run((INSTRUCTIONS - cpu.steps).min(64)).unwrap();
        let counted = cpu.trace_cache_bytes();
        assert!(
            counted <= TRACE_CACHE_BYTES,
            "the cache counts {counted} bytes, over its limit of {TRACE_CACHE_BYTES}"
        );
        largest = largest.max(LIVE.load(Ordering::Relaxed).saturating_sub(before));
    }
    let stats = &cpu.specialization_stats;
    assert!(
        stats.recordings >= 90,
        "the run recorded {} traces, too few to fill a cache",
        stats.recordings
    );
    assert_eq!(cpu.gpr(10).unwrap(), cpu.steps);
    let held = LIVE.load(Ordering::Relaxed).saturating_sub(before);
    let counted = cpu.trace_cache_bytes();
    println!(
        "held {held} bytes, largest {largest}, counted {counted}, recordings {}, evictions {}",
        stats.recordings, stats.evictions
    );
    assert!(
        largest <= budget,
        "{} guest steps made the hart hold {largest} bytes, over the budget of {budget} \
         ({} recordings, {} evictions, the cache counts {counted})",
        cpu.steps,
        stats.recordings,
        stats.evictions
    );
    assert!(
        stats.evictions > 0,
        "the run never filled the cache: it counts {counted} bytes"
    );
    // The count must be near what the host pays. A count far under the truth
    // is the fault this test is for.
    assert!(
        counted >= held / 2 && counted <= held.saturating_mul(2),
        "the cache counts {counted} bytes and the hart holds {held}"
    );
}
