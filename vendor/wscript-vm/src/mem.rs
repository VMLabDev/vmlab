//! The memory limit of a `Vm` (vmlab delta, card #1086,
//! `docs/reviews/wscript-2.md` F13).
//!
//! Fuel counts instructions, and one instruction can allocate any number of
//! bytes (`s = s + s`), so fuel does not limit memory. This limit counts what
//! the script *holds*, not what it has allocated in total: a script that
//! makes and drops values for hours stays inside it.
//!
//! Values have no drop hook to count frees with (`Rc<str>`, `Vec<Value>`),
//! so the count is not kept up to date. It is taken, by walking the values
//! the VM can reach, each time enough new values were made since the last
//! walk:
//!
//! - Every site that makes a value adds an estimate of its size to a
//!   **debt** ([`Vm::mem_note`], [`Vm::mem_reserve`]).
//! - When the debt passes a threshold, [`Vm::mem_check`] walks the register
//!   stack and sums the size of every value reachable from it, each one
//!   counted once. Over the limit is a fault. Otherwise the debt goes back
//!   to zero.
//! - The threshold is half of what the last walk found, at least a sixteenth
//!   of the limit and at most half of it. So the walks cost a constant share
//!   of the allocation they follow, and a script is stopped at about one and
//!   a half times the limit at the latest.
//!
//! A string concatenation is the one instruction whose size is known before
//! it allocates and is not bounded by its operands' count, so it reserves
//! first and faults *before* the allocation. Every other operation makes a
//! value no larger than a small multiple of values the script already holds
//! (a clone, a join, a split, a render), and is counted after.
//!
//! What a native function holds while it runs is not on the register stack.
//! The builtins that collect the results of a callback (`map` and its
//! relatives) say what they hold with [`Vm::mem_hold`]; the count is given
//! back when the native call returns and its result is in a register.

use std::collections::HashSet;
use std::rc::Rc;

use wscript_core::value::{Key, Value};

use crate::{RuntimeError, Vm};

/// The state of the limit. Absent on a `Vm` without one.
pub(crate) struct Memory {
    /// The limit, in bytes.
    limit: u64,
    /// Bytes of new values since the last walk.
    debt: u64,
    /// The debt at which the next walk happens.
    threshold: u64,
    /// Bytes that running native functions hold and the stack does not show.
    held: u64,
}

impl Memory {
    pub(crate) fn new(limit: u64) -> Memory {
        Memory {
            limit,
            debt: 0,
            threshold: Self::threshold_after(limit, 0),
            held: 0,
        }
    }

    fn threshold_after(limit: u64, live: u64) -> u64 {
        (live / 2).clamp(limit / 16, limit / 2).max(1)
    }
}

/// What one heap node costs beside its payload: the `Rc` counts, the
/// `RefCell` flag, the `Vec` or map header, the allocator's own header.
const NODE: u64 = 48;
/// One element of a list, a field, a register.
const SLOT: u64 = size_of::<Value>() as u64;
/// One entry of a map: the key, the value and the tree's share.
const ENTRY: u64 = (size_of::<Key>() + size_of::<Value>() + 16) as u64;
/// A host value: its size is not known here.
const OPAQUE: u64 = 64;

/// The estimate for a node an instruction just made, with `slots` elements.
pub(crate) fn node(slots: u64) -> u64 {
    NODE + slots * SLOT
}

/// Which heap nodes a walk counts.
enum Walk {
    /// The nodes nothing else refers to: what a fresh value brought into
    /// being. A node that is shared was counted when it was made.
    Fresh,
    /// Every node, once.
    Reachable(HashSet<usize>),
}

impl Walk {
    /// Whether to count the node `ptr`, of which the walk itself holds one
    /// more reference than `strong` counted before it.
    fn takes(&mut self, ptr: usize, strong: usize) -> bool {
        match self {
            // One reference is the owner's, one is the walk's.
            Walk::Fresh => strong == 2,
            Walk::Reachable(seen) => seen.insert(ptr),
        }
    }
}

/// The size of the values in `todo` and of everything under them, as `walk`
/// counts. Not recursive: a value can be nested to any depth.
fn measure(mut todo: Vec<Value>, walk: &mut Walk) -> u64 {
    let mut bytes = 0u64;
    let mut add = |n: u64| bytes = bytes.saturating_add(n);
    while let Some(value) = todo.pop() {
        match &value {
            Value::Unit
            | Value::Int(_)
            | Value::Float(_)
            | Value::Bool(_)
            | Value::Char(_)
            | Value::WeakRef(_) => {}
            Value::Str(s) => {
                if walk.takes(Rc::as_ptr(s) as *const u8 as usize, Rc::strong_count(s)) {
                    add(16 + s.len() as u64);
                }
            }
            Value::List(items) => {
                if walk.takes(Rc::as_ptr(items) as usize, Rc::strong_count(items)) {
                    add(NODE);
                    // A list that a native function is changing right now
                    // is counted without its elements.
                    if let Ok(items) = items.try_borrow() {
                        add(items.capacity() as u64 * SLOT);
                        todo.extend(items.iter().filter(|v| is_heap(v)).cloned());
                    }
                }
            }
            Value::Map(entries) => {
                if walk.takes(Rc::as_ptr(entries) as usize, Rc::strong_count(entries)) {
                    add(NODE);
                    if let Ok(entries) = entries.try_borrow() {
                        add(entries.len() as u64 * ENTRY);
                        for (key, v) in entries.iter() {
                            if let Key::Str(s) = key {
                                todo.push(Value::Str(s.clone()));
                            }
                            if is_heap(v) {
                                todo.push(v.clone());
                            }
                        }
                    }
                }
            }
            Value::Struct(s) => {
                if walk.takes(Rc::as_ptr(s) as usize, Rc::strong_count(s)) {
                    add(NODE);
                    if let Ok(fields) = s.fields.try_borrow() {
                        add(fields.len() as u64 * SLOT);
                        todo.extend(fields.iter().filter(|v| is_heap(v)).cloned());
                    }
                }
            }
            Value::Enum(e) => {
                if walk.takes(Rc::as_ptr(e) as usize, Rc::strong_count(e)) {
                    add(NODE);
                    if let Ok(fields) = e.fields.try_borrow() {
                        add(fields.len() as u64 * SLOT);
                        todo.extend(fields.iter().filter(|v| is_heap(v)).cloned());
                    }
                }
            }
            Value::Closure(c) => {
                if walk.takes(Rc::as_ptr(c) as usize, Rc::strong_count(c)) {
                    add(NODE + c.captures.len() as u64 * 8);
                    todo.extend(c.captures.iter().cloned().map(Value::Cell));
                }
            }
            Value::Cell(cell) => {
                if walk.takes(Rc::as_ptr(cell) as usize, Rc::strong_count(cell)) {
                    add(NODE);
                    if let Ok(inner) = cell.try_borrow()
                        && is_heap(&inner)
                    {
                        todo.push(inner.clone());
                    }
                }
            }
            Value::Dyn(d) => {
                if walk.takes(Rc::as_ptr(d) as usize, Rc::strong_count(d)) {
                    add(NODE);
                    if is_heap(&d.inner) {
                        todo.push(d.inner.clone());
                    }
                }
            }
            Value::Opaque(o) => {
                if walk.takes(Rc::as_ptr(o) as usize, Rc::strong_count(o)) {
                    add(OPAQUE);
                }
            }
        }
        // `value` is a second reference to a node its owner still holds, so
        // this drop frees nothing and does not recurse.
    }
    bytes
}

/// Whether `v` is a heap node a walk has to look at.
fn is_heap(v: &Value) -> bool {
    !matches!(
        v,
        Value::Unit
            | Value::Int(_)
            | Value::Float(_)
            | Value::Bool(_)
            | Value::Char(_)
            | Value::WeakRef(_)
    )
}

impl Vm {
    /// The bytes `v` brought into being: the nodes under it that nothing
    /// else refers to. For a value that was just made and is owned by the
    /// caller alone. Zero on a `Vm` without a limit.
    pub(crate) fn mem_fresh(&self, v: &Value) -> u64 {
        if self.mem.is_none() || !is_heap(v) {
            return 0;
        }
        measure(vec![v.clone()], &mut Walk::Fresh)
    }

    /// Count `bytes` of values that exist already and are reachable from the
    /// register stack (or are too small to matter until the next walk).
    #[inline]
    pub(crate) fn mem_note(&mut self, bytes: u64) -> Result<(), RuntimeError> {
        match &mut self.mem {
            None => Ok(()),
            Some(mem) => {
                mem.debt = mem.debt.saturating_add(bytes);
                if mem.debt < mem.threshold {
                    return Ok(());
                }
                self.mem_check(0)
            }
        }
    }

    /// Count `bytes` that the caller is about to allocate in one piece, and
    /// fault instead when they do not fit.
    #[inline]
    pub(crate) fn mem_reserve(&mut self, bytes: u64) -> Result<(), RuntimeError> {
        match &mut self.mem {
            None => Ok(()),
            Some(mem) => {
                mem.debt = mem.debt.saturating_add(bytes);
                if mem.debt < mem.threshold {
                    return Ok(());
                }
                self.mem_check(bytes)
            }
        }
    }

    /// Count a value that a native function got from a callback and keeps
    /// where the register stack does not show it.
    pub(crate) fn mem_hold(&mut self, v: &Value) -> Result<(), RuntimeError> {
        if self.mem.is_none() {
            return Ok(());
        }
        let bytes = SLOT + self.mem_fresh(v);
        if let Some(mem) = &mut self.mem {
            mem.held = mem.held.saturating_add(bytes);
        }
        self.mem_note(bytes)
    }

    /// What running native functions hold; see [`Vm::mem_hold`].
    pub(crate) fn mem_held(&self) -> u64 {
        self.mem.as_ref().map_or(0, |mem| mem.held)
    }

    /// Give back what a native call held, when it returns.
    pub(crate) fn set_mem_held(&mut self, held: u64) {
        if let Some(mem) = &mut self.mem {
            mem.held = held;
        }
    }

    /// Take the count: everything reachable from the register stack, what
    /// native functions hold, and `incoming` bytes not allocated yet.
    #[cold]
    fn mem_check(&mut self, incoming: u64) -> Result<(), RuntimeError> {
        let Some(mem) = &self.mem else {
            return Ok(());
        };
        let (limit, held) = (mem.limit, mem.held);
        let mut roots: Vec<Value> = self.stack.iter().filter(|v| is_heap(v)).cloned().collect();
        roots.extend(
            self.frames
                .iter()
                .filter_map(|f| f.closure.clone())
                .map(Value::Closure),
        );
        let stack = self.stack.capacity() as u64 * SLOT;
        let live = measure(roots, &mut Walk::Reachable(HashSet::new()))
            .saturating_add(stack)
            .saturating_add(held);
        if live.saturating_add(incoming) > limit {
            return Err(self.fault(format!("memory limit of {limit} bytes exceeded")));
        }
        if let Some(mem) = &mut self.mem {
            mem.debt = 0;
            mem.threshold = Memory::threshold_after(limit, live);
        }
        Ok(())
    }
}
