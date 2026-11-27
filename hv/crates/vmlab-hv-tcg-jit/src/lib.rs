//! The x86-64 JIT of vmlab-hv's emulators (phase 3e, DESIGN.md H15).
//!
//! It compiles a block of the pre-decoded form of `vmlab_hv_tcg_ir::block`
//! to host code and knows no guest architecture. The pieces:
//!
//! - `asm`: the assembler.
//! - `region`: the code memory, one `memfd` mapped twice (H13d, Q3).
//! - `emit`: the one-pass allocator with its spill (H3), the inline forms of
//!   the operations of one word, the guards and their stubs (H9), and the
//!   thunk through which every effect runs.
//! - The trampoline here (H4): `enter` saves the registers that the System V
//!   ABI keeps, loads the context, and jumps to a block. A block leaves by a
//!   jump to the common exit, which restores them and returns the exit code.
//!
//! What a block returns: `DONE` after its last operation, with its outputs
//! in the state; `BUDGET` when the budget of the context did not hold its
//! instructions (H10), with nothing done; `CHAIN` and `LOOKUP` after its
//! last operation, when it found no code to go on to (H5, H6); and any other
//! value that the thunk returned, with the words of the leave in the frame.
//! `Context::block` names the block that left.
//!
//! Chaining (H5): the exit of a block to a constant target in its page is a
//! jump that starts at a stub, which leaves with `CHAIN` and the index of
//! the exit. The owner finds the target and links the jump to its code
//! (`link`). Each block keeps the jumps that were linked to it, and
//! `unlink_to` and `unlink_all` set them back to their stubs. Every other
//! exit looks its target up in the jump cache of the CPU (H6): an entry is
//! the virtual PC, the key of the context and the code, direct-mapped by the
//! PC. A miss leaves with `LOOKUP`, and the owner fills the entry (`fill`).
//! When the region is full, its code is dropped whole (H13c), and with it
//! every link and every entry of the jump cache.
//!
//! Sources read: the Intel 64 and IA-32 Architectures Software Developer's
//! Manual, volume 2, the System V AMD64 ABI, and the Linux man pages of the
//! calls in `region`. No code of another JIT was read.

pub mod asm;
pub mod emit;
pub mod region;

use asm::{Asm, R12, R13, R14, R15, RBP, RBX, RCX, RDI, RDX, RSI, RSP, Rm};
pub use emit::{Chain, Effect, Next, NextPc, Refused, Shape};
use region::Region;
use std::collections::HashMap;
use std::ffi::c_void;

/// The block ran to its end.
pub const DONE: u64 = 0;
/// The budget did not hold the instructions of the block.
pub const BUDGET: u64 = 1;
/// The block ran to its end, at its exit `code - CHAIN`, whose jump is not
/// linked yet.
pub const CHAIN: u64 = 0x10;
/// The block ran to its end, and the jump cache had no entry for the PC.
pub const LOOKUP: u64 = 0x20;
/// The most exits of a block that can be linked.
pub const SITES: usize = 2;

/// The function that runs an operation of a block for its code: `env` is
/// what the context holds, `index` the operation, `block` what the head of
/// the block that runs stored in `Context::block`. 0: go on. Anything
/// else: the block leaves with that value.
pub type Thunk = unsafe extern "sysv64" fn(env: *mut c_void, index: u64, block: *mut c_void) -> u64;

/// What compiled code reads through R13.
#[repr(C)]
pub struct Context {
    /// Instructions left. A block takes its count at its head.
    pub budget: i64,
    pub thunk: Thunk,
    pub env: *mut c_void,
    /// The block that runs: the head of a block that chains stores its
    /// `Chain::tag` here.
    pub block: *mut c_void,
    /// The jump cache of the JIT that runs the code.
    pub jump: *const JumpEntry,
    /// Exits through a jump of `CHAIN`, linked or not, and hits of the jump
    /// cache. Compiled code counts them.
    pub chained: u64,
    pub hits: u64,
}

pub(crate) const CONTEXT_BUDGET: i32 = 0;
pub(crate) const CONTEXT_THUNK: i32 = 8;
pub(crate) const CONTEXT_ENV: i32 = 16;
pub(crate) const CONTEXT_BLOCK: i32 = 24;
pub(crate) const CONTEXT_JUMP: i32 = 32;
pub(crate) const CONTEXT_CHAINED: i32 = 40;
pub(crate) const CONTEXT_HITS: i32 = 48;

/// One entry of the jump cache (H6). An empty entry has the key 0, which no
/// context has.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JumpEntry {
    pub pc: u64,
    pub key: u64,
    pub code: u64,
    pad: u64,
}

impl JumpEntry {
    const EMPTY: Self = Self {
        pc: u64::MAX,
        key: 0,
        code: 0,
        pad: 0,
    };
}

/// The entries of the jump cache, a power of two.
pub const JUMP_ENTRIES: usize = 4096;
/// log2 of the size of a `JumpEntry`.
pub(crate) const JUMP_SHIFT: u8 = 5;

/// The entry of the jump cache for `pc`.
pub fn jump_index(pc: u64) -> usize {
    (pc >> 2) as usize & (JUMP_ENTRIES - 1)
}

/// A jump of a block that `link` points at the code of another block: the
/// address of its `jmp rel32`, and that of the stub it starts at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Site {
    jump: u64,
    stub: u64,
}

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

/// The code of one block: where it runs, the generation of the region
/// that it belongs to, and its exits that can be linked: the offset of the
/// jump and of its stub from `address`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub address: u64,
    pub generation: u64,
    pub bytes: u32,
    pub sites: [(u32, u32); SITES],
    pub site_count: u8,
}

/// Counts of the JIT, for the bench rows and the debug log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub compiled: u64,
    pub refused: u64,
    pub bytes: u64,
    pub resets: u64,
    pub compile_ns: u64,
    /// Jumps linked, jumps set back to their stubs, and entries of the
    /// jump cache filled.
    pub links: u64,
    pub unlinks: u64,
    pub fills: u64,
}

/// The default size of the code region of one CPU (H13b).
pub const REGION_BYTES: usize = 64 << 20;

type Enter = unsafe extern "sysv64" fn(*mut Context, u64, *mut u128, *mut u64) -> u64;

/// The JIT of one CPU: its code region, the trampoline at the front of it,
/// the jumps linked to each block, the jump cache, and its counts.
pub struct Jit {
    region: Region,
    enter: Enter,
    exit: u64,
    features: Features,
    /// The linked jumps to each block, by the address of its code.
    incoming: HashMap<u64, Vec<Site>>,
    jump: Box<[JumpEntry]>,
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
            incoming: HashMap::new(),
            jump: vec![JumpEntry::EMPTY; JUMP_ENTRIES].into_boxed_slice(),
            stats: Stats::default(),
        })
    }

    /// A context for code of this JIT, with nothing counted yet.
    pub fn context(&self, budget: i64, thunk: Thunk, env: *mut c_void) -> Context {
        Context {
            budget,
            thunk,
            env,
            block: std::ptr::null_mut(),
            jump: self.jump.as_ptr(),
            chained: 0,
            hits: 0,
        }
    }

    /// Link the exit `site` of the code `from` to the code `to` (H5). The
    /// caller checked that `to` holds the block that the exit reaches, in
    /// the same context and the same page. `false`: either is no longer
    /// current, or `from` has no such exit.
    pub fn link(&mut self, from: &Entry, site: usize, to: &Entry) -> bool {
        if !self.current(from) || !self.current(to) || site >= usize::from(from.site_count) {
            return false;
        }
        let (jump, stub) = from.sites[site];
        let site = Site {
            jump: from.address + u64::from(jump),
            stub: from.address + u64::from(stub),
        };
        let list = self.incoming.entry(to.address).or_default();
        if list.contains(&site) {
            return true;
        }
        list.push(site);
        Self::point(&mut self.region, site.jump, to.address);
        self.stats.links += 1;
        true
    }

    /// Write the displacement of the `jmp rel32` at `jump` so that it goes
    /// to `target`.
    fn point(region: &mut Region, jump: u64, target: u64) {
        let displacement = target.wrapping_sub(jump + 5) as i64;
        let displacement = i32::try_from(displacement).expect("a jump inside the region");
        region.patch(jump + 1, &displacement.to_le_bytes());
    }

    /// Set every jump that was linked to the code at `address` back to its
    /// stub, and drop the entries of the jump cache that name it: its
    /// block is gone or holds other code now.
    pub fn unlink_to(&mut self, address: u64) {
        if let Some(sites) = self.incoming.remove(&address) {
            for site in sites {
                Self::point(&mut self.region, site.jump, site.stub);
                self.stats.unlinks += 1;
            }
        }
        for entry in self.jump.iter_mut() {
            if entry.code == address {
                *entry = JumpEntry::EMPTY;
            }
        }
    }

    /// Set every linked jump back to its stub and empty the jump cache: the
    /// code of the memory changed, or blocks were dropped.
    pub fn unlink_all(&mut self) {
        for (_, sites) in self.incoming.drain() {
            for site in sites {
                Self::point(&mut self.region, site.jump, site.stub);
                self.stats.unlinks += 1;
            }
        }
        self.clear_jumps();
    }

    /// The count of jumps that are linked now.
    pub fn linked(&self) -> usize {
        self.incoming.values().map(Vec::len).sum()
    }

    /// Empty the jump cache: the translation of virtual addresses changed.
    pub fn clear_jumps(&mut self) {
        self.jump.fill(JumpEntry::EMPTY);
    }

    /// Put the code `to` into the jump cache for the virtual PC `pc` in the
    /// context of `key` (H6). The caller checked that `to` holds the block
    /// of `pc` in that context now.
    pub fn fill(&mut self, pc: u64, key: u64, to: &Entry) {
        if key == 0 || !self.current(to) {
            return;
        }
        self.jump[jump_index(pc)] = JumpEntry {
            pc,
            key,
            code: to.address,
            pad: 0,
        };
        self.stats.fills += 1;
    }

    /// The entry of the jump cache for `pc`, for the tests.
    pub fn jump_entry(&self, pc: u64) -> JumpEntry {
        self.jump[jump_index(pc)]
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
        let (asm, labels) = emit::emit(shape, self.exit, self.features)?;
        let mut sites = [(0, 0); SITES];
        for (n, (jump, stub)) in labels.iter().enumerate() {
            let position = |label| {
                asm.position(label)
                    .and_then(|at| u32::try_from(at).ok())
                    .ok_or(Refused::Size)
            };
            sites[n] = (position(*jump)?, position(*stub)?);
        }
        let len = asm.len();
        let base = match self.region.next(len) {
            Some(base) => base,
            None => {
                // H13c: the whole code of the region goes, and with it every
                // linked jump and every entry of the jump cache. The jumps
                // are not set back: their bytes are no code any more.
                self.region.reset();
                self.incoming.clear();
                self.clear_jumps();
                self.stats.resets += 1;
                self.region.next(len).ok_or(Refused::Size)?
            }
        };
        let code = asm.finish(base).ok_or(Refused::Size)?;
        self.region.put(base, &code);
        Ok(Entry {
            address: base,
            generation: self.region.generation(),
            bytes: u32::try_from(code.len()).map_err(|_| Refused::Size)?,
            sites,
            site_count: labels.len() as u8,
        })
    }

    /// Run the block of `entry`, and the blocks that it chains to.
    ///
    /// # Safety
    /// `entry` is current (`current`). `slots` and `frame` are the register
    /// slots and the frame of the block that `entry` was compiled from, as
    /// many as its shape names, and nothing else reads or writes them while
    /// it runs but the thunk of `context`, which `context.env` serves. The
    /// same holds for every block that a linked jump or the jump cache
    /// reaches from it: the frame and the tag of its `Chain` are alive.
    pub unsafe fn enter(
        &self,
        entry: &Entry,
        context: *mut Context,
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
