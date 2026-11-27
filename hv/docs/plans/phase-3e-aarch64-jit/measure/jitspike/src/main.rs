//! The back-end spike of the phase 3e design (DESIGN.md section 3).
//!
//! The same guest blocks, in a small copy of the pre-decoded block form of
//! `vmlab-hv-tcg-aarch64` (`block::Op`: a code, two operand words, a result
//! word, on a frame of 64-bit words), run through four executors:
//!
//! - `interp`: a loop over the operations, as `exec` runs a block today;
//! - `cranelift`: one Cranelift function per block;
//! - `template`: stencils of machine code with holes, copied and patched,
//!   every value in its word of the frame;
//! - `direct`: x86-64 emitted by a small assembler, values in host
//!   registers, with a one-pass allocation over the straight-line block.
//!
//! The blocks are written as the lifters write them (`add_with_carry` of
//! `lift/builder.rs` for the flags). A load and a store go through a
//! direct-mapped TLB that is inlined, with the miss as a call.
//!
//! Sources read: the Intel manual (instruction encodings), the Cranelift
//! documentation. No emulator source.

mod asm;
mod cl;
mod direct;
mod template;

use std::time::Instant;

pub const SP: u32 = 31;
pub const PC: u32 = 32;
pub const N: u32 = 33;
pub const Z: u32 = 34;
pub const C: u32 = 35;
pub const V: u32 = 36;
/// The operands of the last flag-setting instruction, for the lazy form.
pub const CC_A: u32 = 37;
pub const CC_B: u32 = 38;
pub const REGS: usize = 40;

pub const TLB_ENTRIES: usize = 256;
pub const GUEST_BASE: u64 = 0x4000_0000;
pub const RAM_SIZE: usize = 1 << 20;
pub const EXIT_PC: u64 = 0xdead_0000;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Tlb {
    /// The virtual address of the page, or all ones.
    pub tag: u64,
    /// Host address less virtual address.
    pub addend: u64,
}

#[repr(C)]
pub struct State {
    pub regs: [u64; REGS],
    pub tlb: *mut Tlb,
    pub ram: *mut u8,
    pub slow: u64,
    /// Blocks that a chained block may still enter without the dispatcher.
    pub budget: u64,
}

pub const OFF_TLB: i32 = (REGS * 8) as i32;
pub const OFF_BUDGET: i32 = OFF_TLB + 24;

/// What `(va & TLB_MASK) == tag` asks: the page, and an aligned access.
pub const TLB_MASK: i32 = !0xfff | 7;

fn fill(state: &mut State, va: u64) -> *mut u8 {
    assert!(
        (GUEST_BASE..GUEST_BASE + RAM_SIZE as u64).contains(&va) && va & 7 == 0,
        "the spike has no fault path: {va:#x}"
    );
    state.slow += 1;
    let page = va & !0xfff;
    let addend = (state.ram as u64).wrapping_sub(GUEST_BASE);
    unsafe {
        *state.tlb.add((va >> 12) as usize & (TLB_ENTRIES - 1)) = Tlb { tag: page, addend };
    }
    va.wrapping_add(addend) as *mut u8
}

pub extern "C" fn slow_load(state: *mut State, va: u64) -> u64 {
    unsafe { (fill(&mut *state, va) as *const u64).read_unaligned() }
}

pub extern "C" fn slow_store(state: *mut State, va: u64, value: u64) {
    unsafe { (fill(&mut *state, va) as *mut u64).write_unaligned(value) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    Add,
    Sub,
    And,
    Or,
    Xor,
    /// `a >> aux`.
    Shr,
    /// Unsigned `a <= b` and `a < b`: 0 or 1.
    LessEq,
    Less,
    IsZero,
    /// `dst = [a]`.
    Load8,
    /// `[a] = b`.
    Store8,
}

#[derive(Clone, Copy, Debug)]
pub struct Op {
    pub code: Code,
    pub aux: u8,
    pub dst: u16,
    pub a: u16,
    pub b: u16,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub pc: u64,
    pub instructions: u32,
    pub ops: Vec<Op>,
    /// The words that hold a constant.
    pub consts: Vec<(u16, u64)>,
    pub slots: usize,
    pub inputs: Vec<(u16, u32)>,
    pub outputs: Vec<(u32, u16)>,
    /// The word that decides the exit: not zero is `taken`.
    pub cond: u16,
    pub taken: u64,
    pub fall: u64,
}

impl Block {
    pub fn frame(&self) -> Vec<u64> {
        let mut frame = vec![0; self.slots];
        for (slot, value) in &self.consts {
            frame[*slot as usize] = *value;
        }
        frame
    }

    pub fn constant(&self, slot: u16) -> Option<u64> {
        self.consts.iter().find(|(s, _)| *s == slot).map(|(_, v)| *v)
    }
}

/// Writes a block as the lifters do.
struct Lift {
    block: Block,
    /// The word that holds each register now.
    current: [Option<u16>; REGS],
    written: [bool; REGS],
    lazy: bool,
}

impl Lift {
    fn new(pc: u64, lazy: bool) -> Self {
        Self {
            block: Block {
                pc,
                instructions: 0,
                ops: Vec::new(),
                consts: Vec::new(),
                slots: 0,
                inputs: Vec::new(),
                outputs: Vec::new(),
                cond: 0,
                taken: 0,
                fall: 0,
            },
            current: [None; REGS],
            written: [false; REGS],
            lazy,
        }
    }

    fn slot(&mut self) -> u16 {
        self.block.slots += 1;
        (self.block.slots - 1) as u16
    }

    fn imm(&mut self, value: u64) -> u16 {
        if let Some((slot, _)) = self.block.consts.iter().find(|(_, v)| *v == value) {
            return *slot;
        }
        let slot = self.slot();
        self.block.consts.push((slot, value));
        slot
    }

    fn get(&mut self, reg: u32) -> u16 {
        if let Some(slot) = self.current[reg as usize] {
            return slot;
        }
        let slot = self.slot();
        self.block.inputs.push((slot, reg));
        self.current[reg as usize] = Some(slot);
        slot
    }

    fn set(&mut self, reg: u32, slot: u16) {
        self.current[reg as usize] = Some(slot);
        self.written[reg as usize] = true;
    }

    fn op(&mut self, code: Code, a: u16, b: u16, aux: u8) -> u16 {
        let dst = self.slot();
        self.block.ops.push(Op { code, aux, dst, a, b });
        dst
    }

    fn binary_imm(&mut self, code: Code, rd: u32, rn: u32, imm: u64) {
        let a = self.get(rn);
        let b = self.imm(imm);
        let value = self.op(code, a, b, 0);
        self.set(rd, value);
        self.block.instructions += 1;
    }

    /// `subs rd, rn, #imm`: `add_with_carry(a, !imm, 1)`.
    fn subs_imm(&mut self, rd: u32, rn: u32, imm: u64) {
        let a = self.get(rn);
        let b = self.imm(!imm);
        let one = self.imm(1);
        let partial = self.op(Code::Add, a, b, 0);
        let result = self.op(Code::Add, partial, one, 0);
        self.block.instructions += 1;
        if self.lazy {
            // The operands are kept, and a later reader computes what it
            // needs. The branch of this block reads Z from the result.
            self.set(CC_A, a);
            self.set(CC_B, b);
            self.set(rd, result);
            self.current[Z as usize] = None;
            self.block.cond = result;
            return;
        }
        let carry = self.op(Code::LessEq, result, a, 0);
        let from_a = self.op(Code::Xor, a, result, 0);
        let from_b = self.op(Code::Xor, b, result, 0);
        let both = self.op(Code::And, from_a, from_b, 0);
        let zero = self.op(Code::IsZero, result, 0, 0);
        let n = self.op(Code::Shr, result, 0, 63);
        let v = self.op(Code::Shr, both, 0, 63);
        self.set(N, n);
        self.set(Z, zero);
        self.set(C, carry);
        self.set(V, v);
        self.set(rd, result);
    }

    fn ldr(&mut self, rt: u32, rn: u32, offset: u64) {
        let base = self.get(rn);
        let address = if offset == 0 {
            base
        } else {
            let imm = self.imm(offset);
            self.op(Code::Add, base, imm, 0)
        };
        let value = self.op(Code::Load8, address, 0, 0);
        self.set(rt, value);
        self.block.instructions += 1;
    }

    fn str(&mut self, rt: u32, rn: u32, offset: u64) {
        let base = self.get(rn);
        let address = if offset == 0 {
            base
        } else {
            let imm = self.imm(offset);
            self.op(Code::Add, base, imm, 0)
        };
        let value = self.get(rt);
        self.block.ops.push(Op { code: Code::Store8, aux: 0, dst: 0, a: address, b: value });
        self.block.instructions += 1;
    }

    /// `b.ne taken`, else `fall`.
    fn bne(mut self, taken: u64, fall: u64) -> Block {
        if !self.lazy {
            let z = self.get(Z);
            self.block.cond = self.op(Code::IsZero, z, 0, 0);
        }
        self.block.instructions += 1;
        self.block.taken = taken;
        self.block.fall = fall;
        for reg in 0..REGS {
            if self.written[reg]
                && let Some(slot) = self.current[reg]
            {
                self.block.outputs.push((reg as u32, slot));
            }
        }
        self.block
    }
}

fn integer_body(lift: &mut Lift) {
    lift.binary_imm(Code::Add, 5, 5, 1);
    lift.binary_imm(Code::And, 1, 5, 0xff);
    lift.binary_imm(Code::Xor, 2, 1, 0x55);
    lift.binary_imm(Code::Or, 4, 2, 0x100);
}

fn memory_body(lift: &mut Lift) {
    lift.ldr(1, 0, 0);
    lift.binary_imm(Code::Add, 1, 1, 3);
    lift.str(1, 0, 8);
    lift.binary_imm(Code::Add, 0, 0, 8);
    lift.binary_imm(Code::And, 0, 0, GUEST_BASE | 0xfff8);
}

/// Six instructions of the integer immediate group and a branch.
fn integer_block(lazy: bool) -> Block {
    let mut lift = Lift::new(0x1000, lazy);
    integer_body(&mut lift);
    lift.subs_imm(3, 3, 1);
    lift.bne(0x1000, EXIT_PC)
}

/// A load, a store, three immediate instructions and a branch.
fn memory_block(lazy: bool) -> Block {
    let mut lift = Lift::new(0x2000, lazy);
    memory_body(&mut lift);
    lift.subs_imm(3, 3, 1);
    lift.bne(0x2000, EXIT_PC)
}

/// A long block: 32 instructions.
fn long_block(lazy: bool) -> Block {
    let mut lift = Lift::new(0x3000, lazy);
    for _ in 0..3 {
        integer_body(&mut lift);
        memory_body(&mut lift);
    }
    integer_body(&mut lift);
    lift.subs_imm(3, 3, 1);
    lift.bne(0x3000, EXIT_PC)
}

#[inline(always)]
fn tlb_host(state: &mut State, va: u64) -> Option<*mut u8> {
    let entry = unsafe { *state.tlb.add((va >> 12) as usize & (TLB_ENTRIES - 1)) };
    (va & TLB_MASK as i64 as u64 == entry.tag).then(|| va.wrapping_add(entry.addend) as *mut u8)
}

/// The loop of `exec` over a block, without its checks.
#[inline(never)]
fn interp(block: &Block, frame: &mut [u64], state: &mut State) {
    for (slot, reg) in &block.inputs {
        frame[*slot as usize] = state.regs[*reg as usize];
    }
    for op in &block.ops {
        let a = frame[op.a as usize];
        let b = frame[op.b as usize];
        let value = match op.code {
            Code::Add => a.wrapping_add(b),
            Code::Sub => a.wrapping_sub(b),
            Code::And => a & b,
            Code::Or => a | b,
            Code::Xor => a ^ b,
            Code::Shr => a >> op.aux,
            Code::LessEq => u64::from(a <= b),
            Code::Less => u64::from(a < b),
            Code::IsZero => u64::from(a == 0),
            Code::Load8 => match tlb_host(state, a) {
                Some(host) => unsafe { (host as *const u64).read_unaligned() },
                None => slow_load(state, a),
            },
            Code::Store8 => {
                match tlb_host(state, a) {
                    Some(host) => unsafe { (host as *mut u64).write_unaligned(b) },
                    None => slow_store(state, a, b),
                }
                continue;
            }
        };
        frame[op.dst as usize] = value;
    }
    for (reg, slot) in &block.outputs {
        state.regs[*reg as usize] = frame[*slot as usize];
    }
    state.regs[PC as usize] = if frame[block.cond as usize] != 0 { block.taken } else { block.fall };
}

pub type Entry = unsafe extern "C" fn(*mut State);

/// One megabyte-aligned region that is writable and executable. A product
/// keeps W^X (DESIGN.md section 4); the spike does not need it.
pub struct Region {
    pub base: *mut u8,
    pub size: usize,
    pub used: usize,
}

impl Region {
    fn new(size: usize) -> Self {
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert!(base != libc::MAP_FAILED);
        Self { base: base.cast(), size, used: 0 }
    }

    pub fn put(&mut self, code: &[u8]) -> *const u8 {
        if self.used + code.len() > self.size {
            self.used = 0;
        }
        let at = unsafe { self.base.add(self.used) };
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), at, code.len()) };
        self.used += (code.len() + 15) & !15;
        at
    }
}

struct Machine {
    state: Box<State>,
    _tlb: Vec<Tlb>,
    ram: Vec<u8>,
}

fn machine(iterations: u64) -> Machine {
    let mut tlb = vec![Tlb { tag: u64::MAX, addend: 0 }; TLB_ENTRIES];
    let mut ram = vec![0u8; RAM_SIZE];
    for (index, byte) in ram.iter_mut().enumerate() {
        *byte = (index * 7) as u8;
    }
    let mut state = Box::new(State {
        regs: [0; REGS],
        tlb: tlb.as_mut_ptr(),
        ram: ram.as_mut_ptr(),
        slow: 0,
        budget: u64::MAX,
    });
    state.regs[0] = GUEST_BASE;
    state.regs[3] = iterations;
    Machine { state, _tlb: tlb, ram }
}

/// A hash of the registers and the memory. `flags`: with N, Z, C and V,
/// which the lazy form does not write.
fn digest(machine: &Machine, flags: bool) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |value: u64| hash = (hash ^ value).wrapping_mul(0x0000_0100_0000_01b3);
    // CC_A and CC_B are written by the lazy form only.
    for (index, reg) in machine.state.regs.iter().enumerate() {
        let lazy = index == CC_A as usize || index == CC_B as usize;
        if !lazy && (flags || !(N as usize..=V as usize).contains(&index)) {
            mix(*reg);
        }
    }
    for chunk in machine.ram.chunks_exact(8) {
        mix(u64::from_le_bytes(chunk.try_into().unwrap()));
    }
    hash
}

/// The dispatcher of every compiled form: a tagged table by PC.
fn run_compiled(table: &[(u64, Option<Entry>)], state: &mut State) {
    while state.regs[PC as usize] != EXIT_PC {
        let pc = state.regs[PC as usize];
        let (tag, entry) = table[(pc >> 2) as usize & (table.len() - 1)];
        assert_eq!(tag, pc);
        unsafe { entry.unwrap()(state) };
    }
}

fn table_of(pc: u64, entry: Entry) -> Vec<(u64, Option<Entry>)> {
    let mut table = vec![(u64::MAX, None); 4096];
    table[(pc >> 2) as usize & 4095] = (pc, Some(entry));
    table
}

struct Row {
    name: &'static str,
    mips: f64,
    compile_ns: f64,
    bytes: usize,
    /// With the flags, and without them.
    digest: (u64, u64),
    lazy: bool,
}

fn time_run(block: &Block, iterations: u64, entry: Entry) -> (f64, (u64, u64)) {
    let mut best = 0f64;
    let mut hash = (0, 0);
    for _ in 0..3 {
        let mut machine = machine(iterations);
        machine.state.regs[PC as usize] = block.pc;
        let table = table_of(block.pc, entry);
        let start = Instant::now();
        run_compiled(&table, &mut machine.state);
        let seconds = start.elapsed().as_secs_f64();
        best = best.max(iterations as f64 * f64::from(block.instructions) / seconds / 1e6);
        hash = (digest(&machine, true), digest(&machine, false));
    }
    (best, hash)
}

fn measure(label: &str, block: &Block, lazy_block: &Block, iterations: u64) {
    let mut rows = Vec::new();

    // The interpreter.
    {
        let mut best = 0f64;
        let mut hash = (0, 0);
        for _ in 0..3 {
            let mut machine = machine(iterations);
            machine.state.regs[PC as usize] = block.pc;
            let mut frame = block.frame();
            let start = Instant::now();
            while machine.state.regs[PC as usize] != EXIT_PC {
                assert_eq!(machine.state.regs[PC as usize], block.pc);
                interp(std::hint::black_box(block), &mut frame, &mut machine.state);
            }
            let seconds = start.elapsed().as_secs_f64();
            best = best.max(iterations as f64 * f64::from(block.instructions) / seconds / 1e6);
            hash = (digest(&machine, true), digest(&machine, false));
        }
        rows.push(Row { name: "interp", mips: best, compile_ns: 0.0, bytes: 0, digest: hash, lazy: false });
    }

    let mut region = Region::new(64 << 20);

    // Cranelift, at both optimisation levels.
    for (name, level) in [("cranelift none", "none"), ("cranelift speed", "speed")] {
        let mut jit = cl::Jit::new(level);
        let rounds = 300;
        let start = Instant::now();
        let mut last = (std::ptr::null(), 0);
        for _ in 0..rounds {
            last = jit.compile(block);
        }
        let compile_ns = start.elapsed().as_nanos() as f64 / f64::from(rounds);
        let entry: Entry = unsafe { std::mem::transmute(last.0) };
        let (mips, digest) = time_run(block, iterations, entry);
        rows.push(Row { name, mips, compile_ns, bytes: last.1, digest, lazy: false });
    }

    // The template JIT.
    {
        let stencils = template::Stencils::new();
        let rounds = 100_000;
        let mut frames = Vec::new();
        let mut code = Vec::new();
        let start = Instant::now();
        let mut at = std::ptr::null();
        for _ in 0..rounds {
            frames.clear();
            let frame = block.frame().into_boxed_slice();
            code.clear();
            stencils.compile(block, frame.as_ptr() as u64, &mut code);
            at = region.put(&code);
            frames.push(frame);
        }
        let compile_ns = start.elapsed().as_nanos() as f64 / f64::from(rounds);
        let entry: Entry = unsafe { std::mem::transmute(at) };
        let (mips, digest) = time_run(block, iterations, entry);
        rows.push(Row { name: "template", mips, compile_ns, bytes: code.len(), digest, lazy: false });
    }

    // Direct emission, without and with a chained exit, and the lazy flags.
    for (name, chained, block, lazy) in [
        ("direct", false, block, false),
        ("direct chained", true, block, false),
        ("direct chained lazy flags", true, lazy_block, true),
    ] {
        let rounds = 100_000;
        let mut code = Vec::new();
        let start = Instant::now();
        let mut at = std::ptr::null();
        for _ in 0..rounds {
            code.clear();
            direct::compile(block, chained, &mut code);
            at = region.put(&code);
        }
        let compile_ns = start.elapsed().as_nanos() as f64 / f64::from(rounds);
        let entry: Entry = unsafe { std::mem::transmute(at) };
        let (mips, digest) = time_run(block, iterations, entry);
        rows.push(Row { name, mips, compile_ns, bytes: code.len(), digest, lazy });
    }

    println!(
        "\n{label}: {} instructions, {} operations, {iterations} runs of the block",
        block.instructions,
        block.ops.len()
    );
    println!("{:<28} {:>10} {:>14} {:>8}  state", "executor", "MIPS", "compile ns", "bytes");
    for row in &rows {
        println!(
            "{:<28} {:>10.0} {:>14.0} {:>8}  {}",
            row.name,
            row.mips,
            row.compile_ns,
            row.bytes,
            if same(row, &rows[0]) { "same" } else { "DIFFERS" }
        );
    }
    assert!(rows.iter().all(|row| same(row, &rows[0])), "an executor differs");
}

/// The lazy form is compared without the flags, which it leaves unwritten.
fn same(row: &Row, reference: &Row) -> bool {
    if row.lazy { row.digest.1 == reference.digest.1 } else { row.digest == reference.digest }
}

fn main() {
    let iterations: u64 = std::env::args().nth(1).and_then(|arg| arg.parse().ok()).unwrap_or(30_000_000);
    measure("integer immediate block", &integer_block(false), &integer_block(true), iterations);
    measure("load and store block", &memory_block(false), &memory_block(true), iterations);
    measure("long block", &long_block(false), &long_block(true), iterations / 4);
}
