//! The seat of a CPU (3e DESIGN.md H10c): the `Context` of its compiled code
//! at an address that does not move while the CPU lives, so that another
//! thread can kick it. A kick takes `KICK` from the budget of the context
//! with one atomic subtraction, and the head of the next block that runs
//! finds the budget spent and leaves, at most one block later.
//!
//! The head of a block takes its count from the budget with a plain
//! read-modify-write, not a locked one: a kick that lands between its read
//! and its write is lost. A thread that waits for the CPU kicks again while
//! it waits, so a lost kick costs time and not an answer. The owner reads
//! the budget back as the remainder by `KICK`, so the count of the
//! instructions that ran is exact after any number of kicks: a run holds
//! at most `MOST` instructions, which is far below `KICK`.
//!
//! The owner writes a fresh context into the seat before each run
//! (`prepare`), and only the budget is shared: it is written and read with
//! atomic operations on both sides of Rust, and compiled code subtracts
//! from it in place.
//!
//! Sources: none beyond the design. No code of another JIT was read.

use crate::Context;
use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, Ordering};

/// What a kick takes from the budget.
pub const KICK: i64 = 1 << 40;

/// The most instructions of one run of compiled code: a budget of a run
/// above this is cut to it.
pub const MOST: u64 = 1 << 38;

/// The budget of `context`, as atomic.
///
/// # Safety
/// `context` points at a live `Context`.
unsafe fn budget<'a>(context: *mut Context) -> &'a AtomicI64 {
    // SAFETY: the caller's promise. The budget is an aligned `i64`, which
    // every side reaches with atomic operations or from compiled code.
    unsafe { AtomicI64::from_ptr(std::ptr::addr_of_mut!((*context).budget)) }
}

/// Kick the run of `context`: its next block head leaves. A context whose
/// budget is spent already keeps its kick.
///
/// # Safety
/// `context` points at a live `Context`.
pub unsafe fn kick(context: *mut Context) {
    // SAFETY: the caller's promise.
    let budget = unsafe { budget(context) };
    if budget.load(Ordering::Relaxed) >= 0 {
        budget.fetch_sub(KICK, Ordering::SeqCst);
    }
}

/// The instructions that are left in the budget of `context`, after any
/// number of kicks.
///
/// # Safety
/// `context` points at a live `Context`.
pub unsafe fn left(context: *mut Context) -> i64 {
    // SAFETY: the caller's promise.
    let raw = unsafe { budget(context) }.load(Ordering::SeqCst);
    raw.rem_euclid(KICK)
}

/// Whether the budget of `context` was kicked since it was set.
///
/// # Safety
/// `context` points at a live `Context`.
pub unsafe fn kicked(context: *mut Context) -> bool {
    // SAFETY: the caller's promise.
    unsafe { budget(context) }.load(Ordering::SeqCst) < 0
}

/// A context that no code runs on.
unsafe extern "sysv64" fn idle(_: *mut c_void, _: u64, _: *mut c_void) -> u64 {
    crate::BUDGET
}

/// The context of one CPU at a fixed address.
pub struct Seat {
    context: UnsafeCell<Context>,
}

// SAFETY: another thread reaches only the budget, with atomic operations.
// Every other field is the owner's.
unsafe impl Send for Seat {}
// SAFETY: as above.
unsafe impl Sync for Seat {}

impl Default for Seat {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Seat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seat").finish_non_exhaustive()
    }
}

impl Seat {
    pub fn new() -> Self {
        Self {
            context: UnsafeCell::new(Context {
                budget: 0,
                thunk: idle,
                env: std::ptr::null_mut(),
                block: std::ptr::null_mut(),
                jump: std::ptr::null(),
                chained: 0,
                hits: 0,
                jump_generation: 0,
                reads: crate::tlb::EMPTY_TABLE.as_ptr(),
                writes: crate::tlb::EMPTY_TABLE.as_ptr(),
                accesses: 0,
            }),
        }
    }

    /// The context, for the run of the owner.
    pub fn context(&self) -> *mut Context {
        self.context.get()
    }

    /// Put `fresh` into the seat for a run. Only the owner calls it, on
    /// its own thread, while no code runs on the seat. A kick that lands
    /// before the budget is written is lost.
    pub fn prepare(&self, fresh: Context) {
        let context = self.context.get();
        // SAFETY: the owner's fields are written by the owner alone, and
        // the budget with an atomic store.
        unsafe {
            use std::ptr::addr_of_mut;
            let Context {
                budget: fresh_budget,
                thunk,
                env,
                block,
                jump,
                chained,
                hits,
                jump_generation,
                reads,
                writes,
                accesses,
            } = fresh;
            addr_of_mut!((*context).thunk).write(thunk);
            addr_of_mut!((*context).env).write(env);
            addr_of_mut!((*context).block).write(block);
            addr_of_mut!((*context).jump).write(jump);
            addr_of_mut!((*context).chained).write(chained);
            addr_of_mut!((*context).hits).write(hits);
            addr_of_mut!((*context).jump_generation).write(jump_generation);
            addr_of_mut!((*context).reads).write(reads);
            addr_of_mut!((*context).writes).write(writes);
            addr_of_mut!((*context).accesses).write(accesses);
            budget(context).store(fresh_budget, Ordering::SeqCst);
        }
    }

    /// Kick the run on the seat, from any thread. The owner leaves its
    /// compiled code at the next block head. Outside a run the kick does
    /// nothing that lasts: the next `prepare` sets the budget again.
    pub fn kick(&self) {
        // SAFETY: the context lives as long as the seat.
        unsafe { kick(self.context.get()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_survives_any_number_of_kicks() {
        let seat = Seat::new();
        let context = seat.context();
        // SAFETY: the seat lives.
        unsafe {
            budget(context).store(1234, Ordering::SeqCst);
            assert!(!kicked(context));
            seat.kick();
            assert!(kicked(context));
            // A spent budget keeps its one kick.
            seat.kick();
            assert_eq!(left(context), 1234);
            // A head that took its count after the kick.
            (*context).budget -= 30;
            assert_eq!(left(context), 1204);
            // A kick lost to the plain write of a head, and a kick again.
            budget(context).store(1000, Ordering::SeqCst);
            seat.kick();
            assert_eq!(left(context), 1000);
            budget(context).store(MOST as i64, Ordering::SeqCst);
            seat.kick();
            assert_eq!(left(context), MOST as i64);
        }
    }
}
