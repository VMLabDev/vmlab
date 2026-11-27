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
//! `prepare` writes the budget with a release store, a plain `mov` on the
//! host, and not a sequentially consistent one, an `xchg` (card J10). The
//! protocol asks nothing more of it. A kick that lands before the store is
//! lost whatever the order of the store, and the owner reads its mail and
//! its lines before `prepare` and nothing that a kicker writes after it, so
//! no later load of the owner has to wait for the store. What a lost kick
//! costs is the wait of a kicker until it kicks again (J7a). The seat has
//! cache lines of its own, so that the stores of `prepare` do not share a
//! line with what other threads write.
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
#[inline(always)]
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
#[inline]
pub unsafe fn left(context: *mut Context) -> i64 {
    // SAFETY: the caller's promise.
    let raw = unsafe { budget(context) }.load(Ordering::SeqCst);
    raw.rem_euclid(KICK)
}

/// Whether the budget of `context` was kicked since it was set.
///
/// # Safety
/// `context` points at a live `Context`.
#[inline]
pub unsafe fn kicked(context: *mut Context) -> bool {
    // SAFETY: the caller's promise.
    unsafe { budget(context) }.load(Ordering::SeqCst) < 0
}

/// A context that no code runs on.
unsafe extern "sysv64" fn idle(_: *mut c_void, _: u64, _: *mut c_void) -> u64 {
    crate::BUDGET
}

/// The context of one CPU at a fixed address, on cache lines of its own.
#[repr(align(128))]
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
    #[inline]
    pub fn context(&self) -> *mut Context {
        self.context.get()
    }

    /// Put `fresh` into the seat for a run. Only the owner calls it, on
    /// its own thread, while no code runs on the seat. A kick that lands
    /// before the budget is written is lost.
    #[inline]
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
            budget(context).store(fresh_budget, Ordering::Release);
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

    #[test]
    fn a_seat_is_on_lines_of_its_own_and_a_kick_after_prepare_holds() {
        assert_eq!(std::mem::align_of::<Seat>(), 128);
        let seat = std::sync::Arc::new(Seat::new());
        assert_eq!(seat.context() as usize % 128, 0);
        let mut fresh = seat_context(500);
        fresh.chained = 7;
        seat.prepare(fresh);
        // A kick from another thread after the store of `prepare` is seen.
        std::thread::spawn({
            let seat = seat.clone();
            move || seat.kick()
        })
        .join()
        .unwrap();
        // SAFETY: the seat lives.
        unsafe {
            assert!(kicked(seat.context()));
            assert_eq!(left(seat.context()), 500);
            assert_eq!((*seat.context()).chained, 7);
        }
        // The next `prepare` sets the budget again.
        seat.prepare(seat_context(40));
        // SAFETY: as above.
        unsafe {
            assert!(!kicked(seat.context()));
            assert_eq!(left(seat.context()), 40);
        }
    }

    fn seat_context(budget: i64) -> Context {
        Context {
            budget,
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
        }
    }
}
