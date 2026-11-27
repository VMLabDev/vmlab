//! The x86-64 JIT of vmlab-hv's emulators (phase 3e, DESIGN.md H15).
//!
//! It compiles a block of the pre-decoded form of `vmlab_hv_tcg_ir::block`
//! to host code and knows no guest architecture. The pieces:
//!
//! - `asm`: the assembler.
//! - `region`: the code memory, one `memfd` mapped twice (H13d, Q3).
//! - `emit`: the one-pass allocator with its spill (H3), the inline forms of
//!   the operations of one word and of the common ones of two words, the
//!   guards and their stubs (H9), the inlined loads and stores of 1 to 16
//!   bytes over the fast TLB (H7), the flags of H8 (a comparison as host
//!   flags, and the inline forms of a lazy condition), the direct calls of
//!   helpers (H12), and the thunk through which every other effect runs.
//! - `tlb`: the layout of the fast TLB and its fill and flush (H7).
//! - `perf`: the perf map (H13e), which names each block for `perf`.
//! - `seat`: the context of a CPU at a fixed address, which another thread
//!   kicks through its budget (H10c).
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
pub mod perf;
pub mod region;
pub mod seat;
pub mod tlb;

use asm::{Asm, R12, R13, R14, R15, RBP, RBX, RCX, RDI, RDX, RSI, RSP, Rm};
pub use emit::{
    Chain, Defect, Effect, FlagOperation, FlagTest, LazyCondition, Memory, Next, NextPc, Refused,
    Shape, add_flags, sub_flags,
};
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
/// The block ran to its end and the jump cache had an entry for the PC,
/// which it did not take (`Chain::verify`).
pub const VERIFY: u64 = 0x30;
/// The most exits of a block that can be linked.
pub const SITES: usize = 2;

/// The function that runs an operation of a block for its code: `env` is
/// what the context holds, `index` the operation in the low 32 bits and
/// what is asked of it in the high ones (`MISS`, `WATCHED`, `LOGGED`; none:
/// run the operation), `block` what the head of the block that runs stored
/// in `Context::block`. 0: go on. Anything else: the block leaves with that
/// value.
pub type Thunk = unsafe extern "sysv64" fn(env: *mut c_void, index: u64, block: *mut c_void) -> u64;

/// An inlined load or store missed the fast TLB: run it as the operation,
/// which fills the TLB where H7a lets it.
pub const MISS: u64 = 1 << 32;
/// An inlined store wrote its bytes and found the watch word of its page
/// odd (card S3): the owner tells its memory. The address is in the frame.
/// Nothing else happens, and the block goes on.
pub const WATCHED: u64 = 2 << 32;
/// An inlined load or store hit, in a block compiled for a CPU that keeps
/// its accesses (`Memory::Logged`): the address, and the value of a store,
/// are in the frame, and the value of a load is in its result word. The
/// owner keeps the access. The block goes on.
pub const LOGGED: u64 = 4 << 32;

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
    /// The generation of the jump cache, which every key of an entry holds
    /// added to it: `clear_jumps` moves it, and every entry is then a miss.
    pub jump_generation: u64,
    /// The read and the write table of the fast TLB for the level of the
    /// blocks that run (`tlb::FastTlb::table`), or `tlb::EMPTY_TABLE`, which
    /// `Jit::context` puts here. The thunk may change them while the code
    /// runs. A chained block has the context of the block before it, and so
    /// its level.
    pub reads: *const tlb::Entry,
    pub writes: *const tlb::Entry,
    /// The loads and stores that the code makes inline (H7): the head of a
    /// block adds those of its block, so a block that leaves before its end
    /// counts all of them.
    pub accesses: u64,
}

pub(crate) const CONTEXT_BUDGET: i32 = 0;
pub(crate) const CONTEXT_THUNK: i32 = 8;
pub(crate) const CONTEXT_ENV: i32 = 16;
pub(crate) const CONTEXT_BLOCK: i32 = 24;
pub(crate) const CONTEXT_JUMP: i32 = 32;
pub(crate) const CONTEXT_CHAINED: i32 = 40;
pub(crate) const CONTEXT_HITS: i32 = 48;
pub(crate) const CONTEXT_JUMP_GENERATION: i32 = 56;
pub(crate) const CONTEXT_READS: i32 = 64;
pub(crate) const CONTEXT_WRITES: i32 = 72;
pub(crate) const CONTEXT_ACCESSES: i32 = 80;

const _: () = {
    assert!(std::mem::offset_of!(Context, jump_generation) == CONTEXT_JUMP_GENERATION as usize);
    assert!(std::mem::offset_of!(Context, reads) == CONTEXT_READS as usize);
    assert!(std::mem::offset_of!(Context, writes) == CONTEXT_WRITES as usize);
    assert!(std::mem::offset_of!(Context, accesses) == CONTEXT_ACCESSES as usize);
};

/// One entry of the jump cache (H6): the key is that of the context plus
/// the generation of the cache when it was filled. An empty entry has the
/// key 0, which no context has.
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

/// A step of the generation of the jump cache. A key of a context uses the
/// bits below 32 and the bit 63, so a generation never meets them.
const JUMP_GENERATION_STEP: u64 = 1 << 32;
/// The generation after which the cache is emptied and starts again.
const JUMP_GENERATION_LAST: u64 = (1 << 62) - JUMP_GENERATION_STEP;

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
    /// Times the jump cache was emptied.
    pub clears: u64,
    /// Comparisons compiled as host flags that the next operation reads
    /// (H8a), and lazy conditions compiled inline (H8c).
    pub fused: u64,
    pub lazy: u64,
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
    jump_generation: u64,
    defect: Defect,
    /// The address and the length of the trampoline.
    trampoline: (u64, u64),
    /// Each block is named in the perf map (H13e).
    perf_map: bool,
    /// How the code in the region reaches memory (H7, `set_memory`).
    memory: Memory,
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
            incoming: HashMap::new(),
            jump: vec![JumpEntry::EMPTY; JUMP_ENTRIES].into_boxed_slice(),
            jump_generation: JUMP_GENERATION_STEP,
            defect: Defect::None,
            trampoline: (base, len as u64),
            perf_map: false,
            memory: Memory::Thunk,
            host_flags: true,
            stats: Stats::default(),
        })
    }

    /// How the blocks that are compiled from now on reach memory (H7). A
    /// change drops the code of the region, as a full region does (H13c),
    /// so that no block of the other kind runs or is chained to: a block
    /// that keeps its accesses never reaches one that does not.
    pub fn set_memory(&mut self, memory: Memory) {
        if memory != self.memory {
            self.region.reset();
            self.incoming.clear();
            self.clear_jumps();
            self.memory = memory;
        }
    }

    /// How the code in the region reaches memory.
    pub fn memory(&self) -> Memory {
        self.memory
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
            jump_generation: self.jump_generation,
            reads: tlb::EMPTY_TABLE.as_ptr(),
            writes: tlb::EMPTY_TABLE.as_ptr(),
            accesses: 0,
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
    /// A new generation, so that it costs no write of its entries: it runs
    /// at every write of a table base, twice for each system call of a
    /// kernel without hardware PAN.
    pub fn clear_jumps(&mut self) {
        if self.jump_generation >= JUMP_GENERATION_LAST {
            self.jump.fill(JumpEntry::EMPTY);
            self.jump_generation = JUMP_GENERATION_STEP;
        } else {
            self.jump_generation += JUMP_GENERATION_STEP;
        }
        self.stats.clears += 1;
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
            key: key + self.jump_generation,
            code: to.address,
            pad: 0,
        };
        self.stats.fills += 1;
    }

    /// The code that the jump cache gives for `pc` in the context of `key`.
    pub fn jump_target(&self, pc: u64, key: u64) -> Option<u64> {
        let entry = self.jump[jump_index(pc)];
        (entry.pc == pc && entry.key == key.wrapping_add(self.jump_generation))
            .then_some(entry.code)
    }

    /// Put `defect` into every block compiled from now on. For the tests
    /// of K2 and K3 only (3e DESIGN.md J2): a guest never runs with one.
    pub fn seed_defect(&mut self, defect: Defect) {
        self.defect = defect;
    }

    /// Name every block compiled from now on in the perf map of the
    /// process (H13e), with the trampoline first.
    pub fn set_perf_map(&mut self, on: bool) -> std::io::Result<()> {
        if on && !self.perf_map {
            let (address, bytes) = self.trampoline;
            perf::name(address, bytes, "vmlab-hv-jit enter")?;
        }
        self.perf_map = on;
        Ok(())
    }

    /// Whether blocks are named in the perf map: the caller names each
    /// block that it compiles with `name`.
    pub fn perf_map(&self) -> bool {
        self.perf_map
    }

    /// Name the code of `entry` in the perf map, when it is on. A failed
    /// write turns the map off, and says why.
    pub fn name(&mut self, entry: &Entry, label: &str) -> std::io::Result<()> {
        if !self.perf_map {
            return Ok(());
        }
        let written = perf::name(entry.address, u64::from(entry.bytes), label);
        if written.is_err() {
            self.perf_map = false;
        }
        written
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
        let (asm, labels, fused, lazy) = emit::emit(
            shape,
            self.exit,
            self.features,
            self.defect,
            self.host_flags,
        )?;
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
        self.stats.fused += fused;
        self.stats.lazy += lazy;
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
