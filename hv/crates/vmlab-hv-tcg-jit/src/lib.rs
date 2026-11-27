//! The x86-64 JIT of vmlab-hv's emulators (phase 3e, DESIGN.md H15).
//!
//! It compiles a block of the pre-decoded form of `vmlab_hv_tcg_ir::block`
//! to host code and knows no guest architecture. The pieces:
//!
//! - `asm`: the assembler.
//! - `region`: the code memory, one `memfd` mapped twice (H13d, Q3).
//! - `emit`: the one-pass allocator with its spill (H3), the inline forms of
//!   the operations of one word, the guards and their stubs (H9), and the
//!   thunk through which every effect runs, and the flags of H8: a
//!   comparison as host flags, and the inline forms of a lazy condition.
//! - The trampoline here (H4): `enter` saves the registers that the System V
//!   ABI keeps, loads the context, and jumps to a block. A block leaves by a
//!   jump to the common exit, which restores them and returns the exit code.
//!
//! What a block returns: `DONE` after its last operation, with its outputs
//! in the state; `BUDGET` when the budget of the context did not hold its
//! instructions (H10), with nothing done; and any other value that the
//! thunk returned, with the words of the leave in the frame.
//!
//! Sources read: the Intel 64 and IA-32 Architectures Software Developer's
//! Manual, volume 2, the System V AMD64 ABI, and the Linux man pages of the
//! calls in `region`. No code of another JIT was read.

pub mod asm;
pub mod emit;
pub mod region;

use asm::{Asm, R12, R13, R14, R15, RBP, RBX, RCX, RDI, RDX, RSI, RSP, Rm};
pub use emit::{
    Effect, FlagOperation, FlagTest, LazyCondition, Refused, Shape, add_flags, sub_flags,
};
use region::Region;
use std::ffi::c_void;

/// The block ran to its end.
pub const DONE: u64 = 0;
/// The budget did not hold the instructions of the block.
pub const BUDGET: u64 = 1;

/// The function that runs an operation of a block for its code: `env` is
/// what the context holds, `index` the operation. 0: go on. Anything else:
/// the block leaves with that value.
pub type Thunk = unsafe extern "sysv64" fn(env: *mut c_void, index: u64) -> u64;

/// What compiled code reads through R13.
#[repr(C)]
pub struct Context {
    /// Instructions left. A block takes its count at its head.
    pub budget: i64,
    pub thunk: Thunk,
    pub env: *mut c_void,
}

pub(crate) const CONTEXT_BUDGET: i32 = 0;
pub(crate) const CONTEXT_THUNK: i32 = 8;
pub(crate) const CONTEXT_ENV: i32 = 16;

/// The instructions of the host that the emitter may use beyond x86-64.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    pub lzcnt: bool,
    pub bmi1: bool,
    pub popcnt: bool,
}

impl Features {
    pub fn host() -> Self {
        Self {
            lzcnt: std::arch::is_x86_feature_detected!("lzcnt"),
            bmi1: std::arch::is_x86_feature_detected!("bmi1"),
            popcnt: std::arch::is_x86_feature_detected!("popcnt"),
        }
    }
}

/// The code of one block: where it runs, and the generation of the region
/// that it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub address: u64,
    pub generation: u64,
    pub bytes: u32,
}

/// Counts of the JIT, for the bench rows and the debug log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub compiled: u64,
    pub refused: u64,
    pub bytes: u64,
    pub resets: u64,
    pub compile_ns: u64,
    /// Comparisons compiled as host flags that the next operation reads
    /// (H8a), and lazy conditions compiled inline (H8c).
    pub fused: u64,
    pub lazy: u64,
}

/// The default size of the code region of one CPU (H13b).
pub const REGION_BYTES: usize = 64 << 20;

type Enter = unsafe extern "sysv64" fn(*mut Context, u64, *mut u128, *mut u64) -> u64;

/// The JIT of one CPU: its code region, the trampoline at the front of it,
/// and its counts.
pub struct Jit {
    region: Region,
    enter: Enter,
    exit: u64,
    features: Features,
    /// Compile comparisons as host flags and lazy conditions inline (H8).
    /// Off: every comparison is a `setcc`, and every lazy condition a call,
    /// as card J1 made them. For measurements before and after.
    pub host_flags: bool,
    pub stats: Stats,
}

impl Jit {
    pub fn new() -> std::io::Result<Self> {
        Self::with_capacity(REGION_BYTES)
    }

    pub fn with_capacity(bytes: usize) -> std::io::Result<Self> {
        Self::with_features(bytes, Features::host())
    }

    /// A JIT that emits only what `features` allows, for the tests of the
    /// forms that a host lacks.
    pub fn with_features(bytes: usize, features: Features) -> std::io::Result<Self> {
        let mut region = Region::new(bytes)?;
        // enter(context, code, slots, frame): RDI, RSI, RDX, RCX.
        let mut asm = Asm::new();
        let saved = [RBX, RBP, R12, R13, R14, R15];
        for reg in saved {
            asm.push(reg);
        }
        // Six pushes over the return address: RSP is 8 past a multiple of
        // 16. The body runs at a multiple of 16.
        asm.alu_imm(asm::Alu::Sub, Rm::Reg(RSP), 8);
        asm.mov(R13, Rm::Reg(RDI));
        asm.mov(R15, Rm::Reg(RDX));
        asm.mov(R14, Rm::Reg(RCX));
        asm.jmp_reg(RSI);
        let exit_offset = asm.len();
        asm.alu_imm(asm::Alu::Add, Rm::Reg(RSP), 8);
        for reg in saved.iter().rev() {
            asm.pop(*reg);
        }
        asm.ret();
        let len = asm.len();
        let base = region
            .next(len)
            .ok_or_else(|| std::io::Error::other("the code region has no room"))?;
        let code = asm
            .finish(base)
            .ok_or_else(|| std::io::Error::other("the trampoline does not assemble"))?;
        region.put(base, &code);
        region.seal_floor();
        // SAFETY: `base` is the start of the code just written to the
        // executable view: a function of the System V ABI with the
        // signature of `Enter`, which returns through the common exit.
        let enter = unsafe { std::mem::transmute::<*const (), Enter>(base as *const ()) };
        Ok(Self {
            region,
            enter,
            exit: base + exit_offset as u64,
            features,
            host_flags: true,
            stats: Stats::default(),
        })
    }

    /// Whether the code of `entry` is still in the region.
    pub fn current(&self, entry: &Entry) -> bool {
        entry.generation == self.region.generation() && self.region.contains(entry.address)
    }

    /// The generation of the code in the region.
    pub fn generation(&self) -> u64 {
        self.region.generation()
    }

    /// Compile `shape`. A full region is dropped and starts again (H13c):
    /// every earlier entry is then of an older generation.
    pub fn compile(&mut self, shape: &Shape) -> Result<Entry, Refused> {
        let started = std::time::Instant::now();
        let result = self.compile_inner(shape);
        self.stats.compile_ns += started.elapsed().as_nanos() as u64;
        match &result {
            Ok(entry) => {
                self.stats.compiled += 1;
                self.stats.bytes += u64::from(entry.bytes);
            }
            Err(_) => self.stats.refused += 1,
        }
        result
    }

    fn compile_inner(&mut self, shape: &Shape) -> Result<Entry, Refused> {
        let (asm, fused, lazy) = emit::emit(shape, self.exit, self.features, self.host_flags)?;
        let len = asm.len();
        let base = match self.region.next(len) {
            Some(base) => base,
            None => {
                self.region.reset();
                self.stats.resets += 1;
                self.region.next(len).ok_or(Refused::Size)?
            }
        };
        let code = asm.finish(base).ok_or(Refused::Size)?;
        self.region.put(base, &code);
        self.stats.fused += fused;
        self.stats.lazy += lazy;
        Ok(Entry {
            address: base,
            generation: self.region.generation(),
            bytes: u32::try_from(code.len()).map_err(|_| Refused::Size)?,
        })
    }

    /// Run the block of `entry`.
    ///
    /// # Safety
    /// `entry` is current (`current`). `slots` and `frame` are the register
    /// slots and the frame of the block that `entry` was compiled from, as
    /// many as its shape names, and nothing else reads or writes them while
    /// it runs but the thunk of `context`, which `context.env` serves.
    pub unsafe fn enter(
        &self,
        entry: &Entry,
        context: &mut Context,
        slots: *mut u128,
        frame: *mut u64,
    ) -> u64 {
        debug_assert!(self.current(entry));
        // SAFETY: the caller's promise, and the trampoline keeps every
        // register that the ABI asks it to keep.
        unsafe { (self.enter)(context, entry.address, slots, frame) }
    }
}

#[cfg(test)]
mod tests;
