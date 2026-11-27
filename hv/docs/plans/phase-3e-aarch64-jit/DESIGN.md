---
slug: phase-3e-aarch64-jit
kind: design
created: 2026-11-27
card: 1145
---

# Phase 3e of vmlab-hv: the aarch64 JIT

This document is the design of sub-phase 3e. `Dn` without a prefix is a row of
`../phase-3-tcg/MAP.md`. `HV-Dn` is a row of `../vmlab-hypervisor/MAP.md`. A
code with the prefix `3d:` is a code of `../phase-3d-aarch64/DESIGN.md`. The
codes of this document (`E`, `T`, `W`, `B`, `H`, `K`, `Y`, `A`, `R`, `G`, `Q`,
`J`) are stable.

Every number marked *measured* comes from helios (Ryzen 9 9950X, QEMU 11.1.3)
on 2026-11-27. Other cards ran boots and builds on the host at the same time
(load average 9 to 17 of 32 threads), so each run was pinned to four cores and
repeated. The tools are in `measure/`. "How to measure again" is at the end. A
number marked *est.* is an estimate, with its basis.

The sources rule of 3d holds for every 3e card: X1 to X3. No agent reads
QEMU's `tcg`, `accel/tcg` or `target/arm`. What this design says about QEMU is
its measured speed. The techniques named here (a block cache, direct block
chaining, an inlined TLB lookup, lazy flags) are described in the published
literature on dynamic binary translation and are written from that
description.

## Summary

- **The result.** Compile every pre-decoded block of the interpreter to
  x86-64 with a small emitter of our own, at the moment the block is
  translated. No tiering and no traces in the first cards. The interpreter
  stays as the reference, as the cold path and as the per-block fallback.
- **Why not Cranelift (D5).** Measured on the same blocks: Cranelift needs 24
  to 160 µs to compile a block, our emitter 0.2 to 1.1 µs. Cranelift's code is
  8 to 53 % faster on a block alone. Chaining and lazy flags are worth more
  than that (1.25 to 2.1 times and 1.2 to 1.7 times) and do not depend on the
  back end. This changes D5. The operator decided it on 2026-11-27 (Q1): our
  own emitter.
- **What the JIT can reach.** *Est.*: 250 to 450 MIPS on a boot and 1,500 to
  3,000 MIPS on CoreMark after cards J1 to J7. The first useful bar (T1, about
  150 MIPS on a boot) is inside that. D1 on a boot (the boot time of QEMU) is
  not certain: after the JIT, most of the time is in what the JIT does not
  remove (R1).
- **The first card** (J1) compiles the integer operations, guards and exits,
  calls the existing Rust paths for memory and helpers, and records CoreMark
  and boot MIPS.

## 1. Target and bar

### E1. Today's numbers, flat images (measured)

`bench/a64` images, one vCPU, no MMU, pinned to cores 28 to 31, three repeats,
median with the range. MIPS is instructions of the image over the run time.

| Runner | Commit | CoreMark mix, MIPS | Scalar loop, MIPS |
|---|---|---|---|
| QEMU 11.1.3 TCG, `-cpu cortex-a53` | | 2,311 (2,248 to 2,365) | 2,830 (2,705 to 2,966) |
| QEMU, the history line of 03:36 the same day, a quieter host | | 2,703 | 3,442 |
| Interpreter, `next` | `5bf56574` | 132 (132.0 to 132.5) | 127.5 (127.5 to 133.1) |
| Interpreter, the tip of card #1048 | `f7346539` | 120 (117.0 to 120.6) | 116 (115.2 to 117.0) |
| Interpreter, the history line of card C5, before the MMU and the system cards | `0053c015` | 167 | 168 |

QEMU is 17.5 times faster than `next` on CoreMark, and 19 times faster than
the tip of card #1048. The changes of #1048 made a boot faster and a flat
image 9 % slower. The basis of an explanation, not measured: the loop now
reads the interrupt lines and the timer deadline before every block. Card #1142 (S1, lazy flags, not merged) reports 138 to 156
MIPS on CoreMark and 136 to 195 on the loop on its branch.

### E2. Today's numbers, a boot (measured)

Debian 13, one vCPU, 2 GiB, the binary of `f7346539` (the fixes F4 to F6 of
card #1048: RAM through its mapping, the hasher, the block store), built with
frame pointers, pinned to cores 12 to 15, with the sampler of W1 attached.
The instruction counts are the counter of the vCPU.

| Phase of the boot | Time (s) | Instructions | MIPS |
|---|---|---|---|
| AAVMF and GRUB | 0 to 95 | 7.33e9 | 87, then 60 |
| Kernel | 95 to 186 | 5.46e9 | 69, then 51 |
| User space (systemd) | 186 to 322 | 4.85e9 | 49, 25, 33 |
| Whole busy part | 0 to 330 | 17.9e9 | 54 |

This run did not reach agent ready inside 600 s under the sampler. A second
run of the same commit without the sampler and without frame pointers, on
cores 8 to 11 with the host at a load average of 15, fell into emergency
mode: a device unit timed out. Card #1048 measured the same commit at 278 s,
285 s and 302 s to agent ready, with emergency mode on Fedora at 1 and 2
vCPUs and on Ubuntu at 1 vCPU. So Debian at one vCPU is at the edge today. A
boot of `next` was not measured again: `next` does not have the fixes of
#1048, and F3 of that card measured it at 45 MIPS with Debian in emergency
mode.

QEMU booted the same template on the same cores a minute later in 33.1 s,
30.0 s and 30.8 s (median 30.8 s, 585 MIPS average).

The QEMU side for all four templates, from `bench/history/aarch64.jsonl`
(2026-11-26, a quieter host, three repeats, median) and the instruction
counts of 3d:M2:

| Template | QEMU, 1 vCPU (s) | 2 vCPUs (s) | 4 vCPUs (s) | Instructions, 1 vCPU | QEMU average MIPS, 1 vCPU |
|---|---|---|---|---|---|
| alpine-3.23 | 48.8 | 46.1 | 42.5 | 29.33e9 | 601 |
| debian-13 | 26.1 | 20.2 | 17.4 | 18.01e9 | 690 |
| fedora-44 | 61.8 | 46.4 | 40.2 | 30.70e9 | 497 |
| ubuntu-24.04 | 44.2 | 28.3 | 24.2 | 20.64e9 | 467 |

The average includes idle time, so QEMU is faster than this while it executes.

### T1. The first useful bar

All four templates reach agent ready at 1 and 2 vCPUs in three runs of
three, with no unit in emergency mode, and the boot MIPS row of Debian at one
vCPU is at least 150.

The basis: the failures of card #1048 are device units that miss a 90 s
timeout at 45 to 60 MIPS, and its worker wrote that a boot "needs about twice
that". 150 MIPS is 2.8 times the measured 54, which gives a margin over the
factor of two. At 150 MIPS the largest boot (Fedora, 30.7e9 instructions) is
205 s of execution.

At T1 the JIT is on by default for `accel: tcg` on aarch64 (decided by the
operator, Q2), and card C13 of 3d can be accepted.

### T2. The bar of D1

For each of the four templates, at 1 and at 4 vCPUs: boot to agent ready in at
most the time of QEMU in the table above. For CPU work: the CoreMark row at
least the QEMU row, and the `sha256` and `xz` workloads of the D2 suite at
most QEMU's time.

In MIPS that is 470 to 690 on a boot average and 2,300 to 2,700 on CoreMark:
9 to 13 times today's boot and 17 to 20 times today's CoreMark. Section 7 says
which part of that is measured and which is estimated. Open question Q15 of
the MAP (how D1 is treated) stays open until card J9 has measured numbers.

## 2. Where the time goes today

### W1. The profile (measured)

A sampler on the instruction pointer of the vCPU thread (`perf_event_open`,
user space only, about 1 kHz). Each sample is resolved to its chain of inlined
functions, and the innermost function decides the class
(`measure/ipclass.py`). The binary is `f7346539` with line tables and frame
pointers. Boot: the run of E2. CoreMark: three runs of the flat image.

| Class | AAVMF and GRUB | Kernel and user space | CoreMark, flat |
|---|---|---|---|
| Samples | 49,202 | 116,855 | 4,102 |
| W1a. Operation loop: fetch, dispatch and the pure operations | 35.8 % | 39.4 % | 53.6 % |
| W1b. Block lookup and the checks of the loop (`run_inner`, `usable`, the block store) | 17.0 % | 21.7 % | 27.5 % |
| W1c. RAM behind a TLB hit: the bounds, the region and the copy of `GuestBus` | 20.9 % | 16.2 % | 0 % |
| W1d. Other frames of the VMM (most are in `vcpu/guest_bus.rs`) | 14.9 % | 6.3 % | 0 % |
| W1e. Softmmu: the TLB lookup, the load and store paths | 2.9 % | 5.4 % | 1.9 % |
| W1f. Register copies at the entry and the exit of a block | 3.5 % | 3.6 % | 6.2 % |
| W1g. Page walk and TLB fill | 1.1 % | 1.5 % | 1.3 % |
| W1h. Translation: lift, graph, lowering | 1.1 % | 1.0 % | 0 % |
| W1i. Helpers: FP, vector, crypto | 0 % | 1.0 % | 0 % |
| W1j. System: exceptions, system registers, timer, interrupts | 0.2 % | 0.5 % | 0 % |
| W1k. Effect dispatch | 0 % | 0.4 % | 0 % |
| W1l. Other in the binary, and libraries | 2.5 % | 3.0 % | 9.5 % |

Read with these facts:

- Translation was 28 % before the block store of card #1048 (F5) and is 1 %
  now, at 8 to 17 µs per translation (measured by the counter of that card).
- Flags are inside W1a. F7 of #1048 counted 23 operations per block and 4.2
  per guest instruction, most of them the NZCV of flag-setting instructions.
- Interrupt and exception entry is small as leaf time (W1j). A boot takes
  460,000 to 1.5 million exceptions and 8,000 to 114,000 interrupts (3d).
- User space runs at half the MIPS of the firmware (E2) with the same class
  shares. The basis of an explanation, not measured: more distinct blocks, so
  more misses of the 4,096-entry block cache and of the host caches on the
  per-block vectors.

### W2. What a JIT removes, and what it does not

| Class | A JIT removes | A JIT does not remove |
|---|---|---|
| W1a, W1k | The fetch and dispatch of every operation, and the frame: an operation is one to three host instructions on host registers. Lazy flags (card #1142) remove flag operations that nothing reads, and compiled code turns a compare and a branch into `cmp` and `jcc` (H8). | The operations themselves. |
| W1b | Chaining (H5) and the jump cache (H6) take the lookup away for most block exits. The validity check (`usable`) goes, because invalidation becomes eager (H11). | One check of the budget per block (H10). A lookup for a jump cache miss. |
| W1c, W1d, W1e | The inlined TLB lookup (H7) reaches host memory in about 10 host instructions. The path through `GuestBus` runs only on a miss, on MMIO and on a page that holds code. | TLB misses and MMIO. |
| W1f | Registers are read from the state at first use and written at exits. | |
| W1g, W1h, W1i, W1j, W1l | Nothing. Translation gets 0.2 to 1.1 µs longer per block (B1). | Page walks, translation, FP and vector helpers through SoftFloat, exception entry and return, device models. |

The classes that a JIT can attack (W1a to W1f and W1k) are 93.0 % of the
kernel and user space samples. The rest (W1g to W1j and W1l) is 7.0 %, and it is the limit: if the 93 % cost nothing, a
boot would run 14 times faster than today (about 770 MIPS). R1 follows from
this.

## 3. The compiler back end

### B1. The spike (measured)

`measure/jitspike` holds a copy of the pre-decoded block form (`block::Op`: a
code, two operand words, a result word, on a frame of 64-bit words) and four
executors for it. The blocks are written as the lifters write them, with the
full NZCV of `add_with_carry`. A load and a store go through a direct-mapped
TLB of 256 entries, inlined, with the miss as a call. Every executor is run
through the same dispatcher (a table by PC) and must leave the same registers
and memory as the interpreter.

- **Interpreter**: the loop of `exec` over the operations, without its checks.
- **Cranelift** 0.138.0, the newest, one function per block, at `opt_level`
  `none` and `speed`. Version 0.123.23 gave the same numbers within 10 %.
- **Template**: one stencil of machine code per operation, with holes for the
  words of the frame, copied and patched. Every value lives in its word of the
  frame. This is the simplest copy-and-patch form. A stencil library built by
  a C compiler could pass a few values in registers between stencils. That
  form was not built.
- **Direct**: x86-64 from an assembler of 250 lines, values in host registers,
  one pass over the block: a register from a pool at the definition, given
  back at the last use, constants as immediates.

Three blocks: six instructions of the integer immediate group with `SUBS` and
`B.NE`. A load, a store, three immediate instructions, `SUBS` and `B.NE`. A
block of 33 instructions that mixes both. Median of three runs, pinned.

| Block | Executor | MIPS | Compile time per block | Code bytes |
|---|---|---|---|---|
| Integer, 6 instructions, 14 operations | Interpreter | 410 | | |
| | Cranelift `none` | 3,033 | 23.7 µs | 208 |
| | Cranelift `speed` | 3,218 | 42.3 µs | 176 |
| | Template | 1,538 | 0.18 µs | 533 |
| | Direct | 2,107 | 0.35 µs | 241 |
| Load and store, 7 instructions, 16 operations | Interpreter | 414 | | |
| | Cranelift `none` | 2,451 | 42.0 µs | 384 |
| | Cranelift `speed` | 2,635 | 77.7 µs | 392 |
| | Template | 1,486 | 0.18 µs | 720 |
| | Direct | 2,430 | 0.47 µs | 460 |
| Mixed, 33 instructions, 44 operations | Interpreter | 842 | | |
| | Cranelift `none` | 7,341 | 85.0 µs | 792 |
| | Cranelift `speed` | 7,089 | 160.1 µs | 808 |
| | Template | 3,097 | 0.48 µs | 1,710 |
| | Direct | 5,562 | 1.09 µs | 1,137 |

Two steps that do not depend on the back end, measured on the direct emitter:

| Block | Direct | With the exit to itself chained | With lazy flags too |
|---|---|---|---|
| Integer | 2,107 | 4,401 (2.1 times) | 7,361 (1.7 times more) |
| Load and store | 2,430 | 3,602 (1.5 times) | 5,500 (1.5 times more) |
| Mixed | 5,562 | 6,955 (1.25 times) | 8,164 (1.2 times more) |

"Chained": the block jumps to its own head while a budget in the state lasts,
without the dispatcher. "Lazy flags": the block keeps the two operands of the
`SUBS` and branches on its result, and writes no N, Z, C and V.

What the spike does not show:

- **B1a.** It is three blocks. A boot has many forms of blocks.
- **B1b.** The spike interpreter runs at 410 MIPS and the real one at 120 to
  132 on CoreMark: the real loop has a block lookup, a validity check and the
  interrupt check (W1b). The MIPS of the table are speeds of a block, not of
  a workload.
- **B1c.** The direct emitter has no spill. The mixed block fits 12 registers
  only because the last value of a guest register is written to the state
  where it is computed (H3).
- **B1d.** Cranelift compiles one function per block here. A trace of several
  blocks would be one function with more for its optimiser to do, at a longer
  compile time.

### B2. Licences and what is pulled in

| Option | Pulled in | Licences |
|---|---|---|
| Cranelift | 34 crates in the build of the spike for this host (38 with version 0.123.23), 19 of them from the Cranelift and Wasmtime project (`cranelift-*`, `regalloc2`, `target-lexicon`, `wasmtime-internal-*`). The spike binary is 7.0 MB. | Apache-2.0 WITH LLVM-exception for those 19. The others MIT or Apache-2.0, one Zlib, one with Unicode-3.0. All usable as dependencies under `hv/AGENTS.md`. |
| Copy-and-patch from stencils of a C compiler | No run-time crate. At build time: clang (already used by `build.rs` for C helpers) and a reader of relocations. | Our own code. |
| Template, stencils from our assembler | Nothing. | Our own code. |
| Direct emission | Nothing. | Our own code. Encodings from the Intel manual, which `hv/AGENTS.md` names as a source. |

### B3. The comparison

| Code | Option | For | Against |
|---|---|---|---|
| O1 | Cranelift (the choice of D5) | A tested register allocator and optimiser. The fastest code on a block alone: 8 to 53 % over the direct emitter in B1. A second host architecture for free. | 24 to 160 µs per block. A boot makes 1.1 to 2.3 million translations (F6 of #1048, with and without the 1.2 million that card #1143 removes), so compiling each is 30 to 360 s of host time. The boot of QEMU is 26 to 62 s. So Cranelift needs tiering: a count per block, a threshold, the interpreter for everything below it. A boot is mostly warm code, not hot code, and the interpreter stays in most of it. The documentation read for the spike shows no direct jump that is patched later (chaining) in its JIT module, so a block would chain through an indirect jump or a tail call. This was not checked further and not measured. The host is x86-64 only (HV-D3), so a second architecture has no use. |
| O2 | Template JIT | The simplest emitter. The same compile time as O3. | About half the speed of O3 (1,486 to 3,097 against 2,107 to 5,562), because every value passes through memory. Three times the code bytes. |
| O3 | Direct emission | 0.2 to 1.1 µs per block: under 10 % of the 8 to 17 µs that a translation costs today. So every block is compiled when it is translated, with no tiering, no counters and no second code path for warm code. Code layout is ours: a patchable jump, a cold stub and a precise exit are plain bytes. No dependency. | We own an assembler and a register allocator with a spill. The code is slower than Cranelift's on a block alone. No optimiser across blocks. |

**Recommendation: O3.** The measured difference in code speed (8 to 53 %) is
smaller than each of the two steps that are free of the back end (chaining,
lazy flags), and the difference in compile time (70 to 160 times) decides
whether the design needs tiering at all.

**What the plan's choice of Cranelift costs or saves.** It saves writing an
allocator and gives better code inside a block or a trace. It costs a tiering
design before the first measured gain, 30 to 360 s of compile time per boot if
tiering is left out, 34 crates, and an unmeasured way to chain. D5 chose it
for "cross-block optimisation" in hot traces. That stays possible later as a
second tier over O3 for the hottest regions, if the numbers of card J9 ask for
it. It is not needed for T1.

This replaces "Cranelift trace JIT" in D5 for aarch64, and by Y1 for riscv64.
The operator decided it on 2026-11-27 (Q1): O3.

## 4. The shape

### H1. What is compiled

The unit is the existing pre-decoded block (`exec::Compiled`): the stream of
`block::Op` with its guards, loads, stores, helper calls and exits. The
emitter consumes that stream and knows nothing of aarch64. It is at most 64
guest instructions (`BLOCK_INSTRUCTIONS`), keyed by the physical address of
its code and its `Context`, and independent of its virtual address after card
#1143.

No traces in the cards J1 to J8. A trace needs counters, a recorder and exits
into the middle of other blocks. Chaining gives most of the gain of a trace
for a loop (B1). Card J9 measures whether a superblock (several blocks
compiled as one unit, with registers kept across the joins) is the next step.

### H2. When

Every block, when it is translated. The basis is B1: 0.2 to 1.1 µs against 8
to 17 µs for the lift and the lowering.

The interpreter stays, for five things:

- **H2a.** The reference of the differential tests (K2, K3).
- **H2b.** A block with an operation that the emitter has no form for yet.
  Such a block runs in the interpreter whole. The count of these blocks and
  of the instructions they run is in every bench row. Card J6 brings it to
  zero on a boot.
- **H2c.** One instruction at a time: the debug port, the end of a budget,
  and the instruction that writes into its own block.
- **H2d.** A run with the access log on (lockstep).
- **H2e.** `VMLAB_HV_JIT=0`, the switch that D2 asks for.

### H3. Registers

- R15 holds the address of the CPU context for the whole time in compiled
  code: the register slots of `State` (3d:R11), then the JIT fields (the TLB
  tables, the budget, the jump cache). RSP is the host stack with the spill
  area. Every other register is in the pool or scratch.
- Inside a block a value lives in a host register. A guest register is read
  from the state at its first use.
- The allocator is one pass over the straight-line block, with the last use of
  every word known from a pass before it. When the pool is empty, the value
  whose next use is furthest goes to a spill slot.
- Values of two words (`Bits(65..=128)`, `Integer`) take two registers or two
  spill slots. Vector registers stay in the state and are passed to helpers by
  address (H12).
- Write-back: a block writes the registers that it changed at each exit, as
  `Compiled::outputs` and `Leave::outputs` say today. An optimisation that the
  spike used and card J6 may take: write the last value of a guest register at
  the end of the instruction that computes it, so it holds no host register to
  the end of the block. The state is exact for every later instruction of the
  block, because that value is the one they see.

### H4. Entry and exit

Compiled blocks are not functions. One trampoline `enter(context, code)` saves
the callee-saved registers, loads R15 and jumps to the block. A block leaves
by a jump to a common exit that restores them and returns an exit record to
the Rust loop: the block, the exit index, and why (end of the budget, a
lookup miss, a fault, a helper that asks to leave). The spike paid twelve
pushes and pops per block for a prologue and an epilogue. This shape pays them
once per entry from Rust.

### H5. Chaining

An exit whose target is a constant (`B`, `BL`, a conditional branch, the fall
through) starts as a jump to a stub that leaves to the Rust loop with the exit
index. The loop finds or translates the target. It then patches the jump to
go to the target's code, when all of these hold:

- **H5a.** The target has the same `Context` as the block.
- **H5b.** The target is in the same guest page as the instruction that jumps
  (4 KiB, the unit of the softmmu, for every granule). A block was entered
  through a valid translation of its page, so a target in that page has the
  same physical page. A target in another page goes through the jump cache
  (H6), which is flushed with the TLB.
- **H5c.** The exit is not after an instruction that can change the context
  or the translation: `MSR`, `ERET`, an exception, a TLBI, a hypercall.

Each block keeps the list of the jumps that were patched to reach it. When the
block is invalidated, they are set back to their stubs.

### H6. Indirect branches

`BR`, `BLR`, `RET`, and a direct branch into another page, look up the target
in a jump cache of the vCPU: 4,096 entries of (virtual PC, context, host
address), direct-mapped by PC, inlined: an index, two compares and an indirect
jump. A miss leaves to the Rust loop, which fills the entry. The cache is
cleared when the TLB is flushed for more than one page, when the `Context`
key cannot tell two address spaces apart (a change of ASID), and when a block
is invalidated. A return stack that predicts `RET` is not in the first cards.

### H7. The softmmu fast path

Today a load is: `Mmu::hit` (the architectural TLB with ASID and permissions),
then `Memory::read` through `GuestBus` (16 % to 21 % of the time, W1c).

The JIT adds a **fast TLB** per vCPU beside the architectural one: two tables
(reads and writes) for each of EL0 and EL1, 256 or more entries, direct-mapped
by the 4 KiB page of the virtual address. An entry is (tag, addend): the tag
is the page address, and host address is virtual address plus addend. The
inlined lookup masks the address so that an access that is not aligned to its
size, or crosses its page, misses.

- **H7a.** An entry exists only for a RAM page with the permission, with no
  translated code in it (for the write table), no watch, and no log. MMIO,
  ROM writes, a page of translated code, an unprivileged access (`LDTR`,
  `STTR`), an exclusive access and 16-byte accesses call the existing Rust
  path (`Side::load`, `Side::store`), which also fills the fast TLB.
- **H7b.** Every flush of the architectural TLB flushes the fast TLB the same
  way: TLBI, a write of `TTBR`, `TCR` or `SCTLR`, a change of ASID.
- **H7c.** A miss is a call that keeps the host registers, in a cold stub
  after the block. It can come back with a fault. Then the block leaves
  through the exit of that instruction (H9).
- **H7d.** 16K and 64K guest pages need no change: the table is in 4 KiB host
  pages (3d:A4).
- **H7e.** The dirty marking of `GuestBus` (`mark_dirty` shows in the
  profile) must still happen for a store through the fast TLB. Card J3 reads
  what it is for today (it is `WriteDiscard` on this path) and either proves
  it is nothing or keeps the write table empty for pages that need it.

### H8. Flags

Card #1142 (S1) makes the flags lazy in the interpreter: a flag-setting
instruction records its operands and its kind, and a reader computes what it
needs. In compiled code:

- **H8a.** A reader in the same block as the writer becomes host flags: `SUBS`
  or `CMP` then `B.cond` is `cmp` and `jcc`, `CSEL` is `cmov`, `CCMP` is a
  compare under a branch. The condition maps directly for a subtraction, with
  the carry inverted (Arm sets C where x86 clears it): HS is `ae`, LO is `b`,
  HI is `a`, LS is `be`, and the signed conditions are the same. For an
  addition and for a logical operation the map is a table of its own.
- **H8b.** At an exit the block writes the lazy record (the kind and two
  operands: two or three stores), not four flags.
- **H8c.** A reader in another block computes the flag from the record, with
  an inlined sequence for the common kinds and a call for the rest.
- **H8d.** Until #1142 is merged, the compiled code computes NZCV as the
  lifters write them today. B1 measured both forms.

### H9. Exceptions and precise state

An instruction that can fault has a `Leave` today: the registers as they were
before it, and the count of completed instructions. The emitter writes one
cold stub per `Leave`. The allocator records where each of those registers is
(a host register, a spill slot, a constant) at that point. The stub writes
them to the state, sets the exit record and leaves. So a fault in the middle
of a block gives the same state as the interpreter gives, and the Rust loop
raises the exception as it does today.

A guard that fails (an alignment check, a range check) is the same stub. An
ordered helper that changes state outside the register slots sees the slots
as they were at the entry of the block, as today (`HelperFn::Ordered`), or the
lifter ends the block before it.

### H10. Interrupts and the budget

At its head a block subtracts its instruction count from a budget in the
context and leaves when the budget is spent. This one check serves three
things:

- **H10a.** The instruction count of the vCPU and the slice.
- **H10b.** The timer deadline: the Rust loop sets the budget so that the
  vCPU leaves in time, as card #1048 made it do within 64 blocks.
- **H10c.** A kick from another thread: an interrupt line that rises, a
  pause, a TLBI from another vCPU, a snapshot. The other thread stores a spent
  budget, and the vCPU leaves at the next block head, at most 64 instructions
  later. A chained loop passes a block head on every turn.

The interrupt is then taken by the Rust loop, at a block boundary, with exact
state.

### H11. Self-modifying code and invalidation

Today a block is checked when it is looked up (`usable`: an epoch, then a
code stamp). Chained code skips the lookup, so invalidation is eager:

- **H11a.** A page that holds translated code has no entry in any write
  table of the fast TLB (H7a). Every store into it takes the Rust path, which
  invalidates the blocks of the page: their entries, their chains (H5), the
  jump cache.
- **H11b.** A store into the block that is running leaves through the exit of
  its instruction, runs that instruction alone in the interpreter, and then
  drops the block, as `Halt::Retry` does today.
- **H11c.** When a page first gets translated code, its write entries must go
  from the fast TLB of every vCPU **before** the code bytes are read. The
  translating vCPU marks the page in a table of the machine, kicks the others
  (H10c) and waits until each has left compiled code once or is outside it.
  The wait is at most one block of the other vCPU. A machine with one vCPU has
  no wait.
- **H11d.** `IC IVAU` and the other cache operations keep what they do today.

### H12. Helpers, FP and vectors

FP, AdvSIMD, crypto and system instructions stay helper calls (3d:G3d), now
called from compiled code with the System V convention: the arguments from
host registers, vector operands by the address of their slots. They are 1 %
of a boot (W1i). Host-FPU fast paths (D11) and inlined AdvSIMD are not in the
cards J1 to J8. Card J9 measures a workload that needs them (`xz`, `sha256`,
a `memcpy` loop of glibc) before a card is written.

### H13. The code cache, eviction, W^X and memory

- **H13a. One cache per vCPU**, as the block cache is today. No lock is on
  the hot path, and a jump is patched only by the thread that runs it. The
  cost is that code used by several vCPUs is compiled once for each, at 0.2
  to 1.1 µs a block.
- **H13b. Size.** B1 measured 34 to 66 bytes of code per guest instruction.
  The default is one region of 64 MiB per vCPU, reserved at once and touched
  as it fills. It is inside the memory cap of the VM process, beside the 256
  MiB young generation of the block store.
- **H13c. Eviction.** When the region is full, the whole cache of that vCPU
  is dropped: the bump pointer is reset, every block loses its code address,
  the chains and the jump cache are cleared. The pre-decoded blocks stay in
  the block store, so a block is compiled again in about a microsecond when
  it runs next. A partial eviction is not built.
- **H13d. W^X.** The region is a `memfd` mapped twice: one view readable and
  writable, where the emitter writes, and one view readable and executable,
  where the vCPU runs. No page is writable and executable in one mapping.
  `mprotect` around each compile is rejected: two system calls cost more than
  the compile. A single mapping with all three rights is simpler and weaker.
  The operator decided on 2026-11-27 (Q3): the double mapping.
- **H13e.** A perf map file (`/tmp/perf-<pid>.map`) names each compiled block
  by its guest PC, behind a switch, as D2 asks.

### H14. More than one vCPU

- One host thread, one block store, one code cache, one fast TLB and one jump
  cache per vCPU.
- Plain loads and stores are plain host `mov`. The host's ordering (TSO) is
  stronger than what the guest architecture asks, so no barrier is added.
  `DMB`, `DSB` and `ISB` keep their helper.
- Exclusives (`LDXR`, `STXR`) stay calls to `load_reserved` and
  `store_conditional` (3d:R3). A v8.0 kernel uses them for every atomic, so
  card J7 measures their share at 2 and 4 vCPUs. The LSE set (3d:G2, a later
  set) maps to `lock cmpxchg`, `xchg` and `lock xadd` after a fast TLB hit.
- A TLBI that reaches every vCPU, and a page that becomes code (H11c), use
  the kick of H10c.

### H15. Where the code lives

- A new crate `vmlab-hv-tcg-jit`: the assembler, the allocator and the
  emitter over the block form, the code region, the trampoline, chaining, the
  jump cache, the layout of the fast TLB and its inlined sequences, the perf
  map. It knows no guest architecture.
- The block form (`block.rs`: `Op`, `Code`, `Lowering`, `Generic`) moves from
  `vmlab-hv-tcg-aarch64` into `vmlab-hv-tcg-ir`, unchanged. Its header says
  that it knows nothing of aarch64.
- `vmlab-hv-tcg-aarch64` keeps the Rust loop, the exits, the flag maps of H8,
  the fill and the flush of the fast TLB, and the helpers.

## 5. Correctness

The interpreter is proven against QEMU by O3 and O4 of 3d. The JIT is proven
against the interpreter, and against QEMU through the same two harnesses.

- **K1. O3 with the JIT on.** The lockstep compares before every instruction,
  so it runs blocks of one instruction. With the JIT on, each of those is
  compiled. Every operation form, every guard, every fault exit and every
  helper call of the emitter is then compared with QEMU, with the same
  streams, counts and seeds as the 3d cards. Each J card that adds an emitter
  form runs the groups it touches.
- **K2. The block differential in `ci::check`.** Random blocks of 1 to 64
  instructions from the stream generator of O3, on two copies of a
  `FlatMemory`: the interpreter on one, the compiled block on the other. The
  registers, the memory, the exit and the count of instructions must be the
  same. This needs no QEMU, so a bounded run with a fixed seed is a test of
  the merge bar, and a long run is a recipe. It covers what K1 cannot: the
  allocator over many instructions, spills, write-back, flag fusion across
  instructions.
- **K3. The replay check on a real boot.** A switch (`VMLAB_HV_JIT_CHECK=1`)
  runs every block twice. First the interpreter runs it with the access log
  on. Then the compiled block runs from the saved registers with an empty fast
  TLB, so every access calls a helper that serves the load from the log and
  compares the store with the log without making it. The registers and the
  exit are compared. A difference stops the machine with the block, both
  states and the log. This proves the emitter on the blocks of a real boot,
  with MMIO and faults. It does not prove the inlined TLB sequence, which K2
  and unit tests cover. Speed: below the interpreter.
- **K4. O4 with the JIT on.** The boot lockstep runs as in 3d, with compiled
  one-instruction blocks. It is the nightly and by-hand run that it is today.
- **K5. kvm-unit-tests and S1** with the JIT on, at 1, 2 and 4 vCPUs: the
  timer, the GIC, PSCI, SMP, and the templates. This is the test of
  interrupts at block boundaries, of chaining with a budget, and of H11c.
- **K6. Invalidation tests**: the existing tests of a store into translated
  code, at one address and at two (card #1143), run with the JIT on, plus a
  chained pair and a store from a second vCPU.
- **K7. Snapshots and the debug port.** Compiled code is never saved (D13). A
  restore, a reset and a state replace drop the code cache, the fast TLB and
  the jump cache. A pause stops a vCPU at a block head (H10c), where the
  state is exact, so the debug port reads the same registers as today. A step
  uses the interpreter (H2c). A breakpoint invalidates the block that holds
  its address. The S1 snapshot tests of card C14 run with the JIT on.

What stays unprovable:

- **U1.** Interleavings of several vCPUs in chained code, and the window of
  H11c. Tests find a defect and do not show its absence (3d:U5).
- **U2.** The point where an interrupt is taken differs between the
  interpreter and the JIT. Both are block boundaries and both are valid. No
  test compares them.
- **U3.** That the inlined fast path equals the Rust path for every address
  and every state of the TLB. K2 and the unit tests sample it.
- **U4.** That host flags equal the lazy record for every operand pair (H8a).
  K2 samples it with boundary values.
- **U5.** Everything that 3d already lists (3d:U1 to U7).

## 6. What is shared, and the M1

### Y. riscv64 (3b and 3c)

- **Y1.** The crate of H15 is built once. The riscv64 runtime of 3a records
  Sail frames into the same graph (`vmlab-hv-tcg-ir::native`) and runs it
  with the slot executor `packed`. When 3c lowers its blocks to the block
  form of H15, it gets the emitter, the code cache, chaining and the jump
  cache without new code. So Q1 decides the back end of 3c too.
- **Y2.** The fast TLB of H7 is the softmmu that 3d:R12 said 3b can use. Its
  layout and its inlined sequences are in the shared crate. The fill and the
  flush are per architecture.
- **Y3.** K2 and K3 take the interpreter and the block form as parameters.
- **Y4.** Not shared: the flag maps (riscv64 has no flags), the context key,
  the exits, and the Sail continuation of a failed guard, which 3c must turn
  into an exit of H9.

### A. An Apple M1-class machine

| Code | Need | What this design does |
|---|---|---|
| A1 | 16K pages | H7d: the fast TLB and the chaining rule H5b work in 4 KiB units for every granule. |
| A2 | Pointer authentication | The PAC instructions are helper calls with the keys read from the system register table (3d:A2). `RETAA` and `BRAA` authenticate in a helper and then use the jump cache. A failed authentication is a fault exit (H9). Nothing in the emitter is specific to it. |
| A3 | The extra instruction sets (LSE, RDM, FP16, DotProd, JSCVT, FCMA, LRCPC, SHA3, SHA512, FlagM, FRINT) | Lifters and helpers, by the cards of 3d:A1. LSE gets inlined host atomics (H14). FlagM adds kinds to the lazy record (H8). The rest reaches the emitter as helper calls. |
| A4 | EL2 with VHE | The fast TLB has one pair of tables per translation regime. EL2 adds a pair. The `Context` key already holds the level. |
| A5 | Speed of FP and vector code, which an M1-class guest (macOS, a desktop) uses far more than a server boot | Not in J1 to J8. H12 names the later cards: host-FPU fast paths under D11 and inlined AdvSIMD. |

## 7. Risks and gains

### The three largest risks

- **R1. The JIT alone does not reach D1 on a boot.** The classes that the JIT
  does not touch are 7 % of today's time (W2), which is 1.3 ns per guest
  instruction at 54 MIPS. That alone caps a boot at about 770 MIPS, and QEMU
  averages 470 to 690 with its idle time inside. Each exception, interrupt,
  page walk and device access must also get cheaper. The answer in the plan:
  card J9 profiles a boot on the JIT and writes the next cards from what it
  finds. T1 does not depend on this.
- **R2. A wrong block that no test sees.** A compiled block that differs from
  the interpreter in a rare case (a spill, a flag fusion, a fault exit, a
  write-back) corrupts a guest silently. The answer: K1 to K3 are built
  before the emitter grows (J1 has K1 and K2, J2 has K3), and every later
  card must pass them. The per-block fallback (H2b) lets a card ship an
  emitter form only when it is proven.
- **R3. Invalidation and chaining with several vCPUs.** H11c and the unlinking
  of H5 are the parts with the least test coverage and the worst failure (a
  vCPU that runs stale code). The answer: one cache per vCPU (H13a) so that
  no patch crosses a thread, the stop of H11c before the code bytes are read,
  a stress test in J7, and a switch that turns chaining off for a bisect.

Smaller risks: the allocator's spill is new code (K2 covers it). A code cache
flush in the middle of a boot costs a recompile of the working set (measured
in J4 as a count). The eager write-back of H3 is only an option.

### G. The gain of each step

| Code | Step | Gain | Kind |
|---|---|---|---|
| G1 | Compiled pure operations, without the dispatch (J1) | 5.1 to 6.6 times on the block itself (interpreter 410 to 842 MIPS, direct 2,107 to 5,562) | measured on the spike |
| | | A boot: 1.4 to 1.6 times. CoreMark: 1.7 to 2 times. Basis: W1a is 39 % and 54 % of the time and becomes a fifth of itself. Memory and block lookup are unchanged in J1. | est. |
| G2 | The inlined softmmu (J3) | A load and store block runs at 2,430 MIPS with the TLB inlined | measured on the spike |
| | | A boot: 1.3 to 1.5 times more. Basis: W1c, W1d and W1e are 22 % to 28 % of today's time. | est. |
| G3 | Chaining, the jump cache, no register copies (J4) | 1.25 to 2.1 times on a block that loops | measured on the spike |
| | | A boot: 1.5 to 2 times more. Basis: W1b and W1f are 25 % of today's time, which is most of what is left after G1 and G2. | est. |
| G4 | Flags as host flags (J5) | 1.2 to 1.7 times on a block that ends in a compare and a branch | measured on the spike |
| | | In the interpreter alone, card #1142 reports 13 % on CoreMark. | reported by that card |
| G5 | All of J1 to J7 | A boot: 250 to 450 MIPS. CoreMark: 1,500 to 3,000 MIPS. Basis: the spike's chained blocks run at 3,600 to 7,000 MIPS. A real workload has guards, helper calls, jump cache misses and TLB misses, and the 7 % of R1 stays. | est. |
| G6 | Translation cost added by the emitter | 0.2 to 1.1 µs per block on top of 8 to 17 µs | measured |

Each J card replaces the estimate of its row by a measured bench row.

### G1, measured by card J1

Helios, 2026-11-27, one vCPU, *measured*. Other cards ran boots and builds
at the same time (load average 5 to 19 of 32 threads), so the interpreter
side moved by a factor of 1.6 between two runs of the same commit. The rows
are in `bench/history/aarch64.jsonl` (backends `interpreter` and `jit` for
the flat images, `vmlab-hv` with `env.jit` for the boot).

| Workload | Interpreter | JIT (J1) | Gain |
|---|---|---|---|
| CoreMark mix, flat, MIPS (median of 3, cores 28 to 31) | 93.4 | 147.7 | 1.58 times |
| Scalar loop, flat, MIPS | 95.1 | 167.2 | 1.76 times |
| Debian 13 boot to agent ready, s (cores 12 to 15) | 244.5, and 390.5 in an earlier run | 193.1, and 186.9 in an earlier run | 1.27 to 2.1 times |
| The same, average MIPS over 18.01e9 instructions (3d:M2) | 74, and 46 | 93, and 96 | |

- **J1a.** CoreMark's checksums hold on both sides. No instruction of the
  flat images runs in the interpreter (H2b). In the boot, 52.3 million of
  19.0e9 instructions (0.28 %) ran in the interpreter, in 6.7 million runs
  of 361 blocks that the emitter refused: `RBIT` has no inline form yet, and
  the string functions of the kernel and of glibc use it.
- **J1b.** Compile time is larger than B1 said. The boot compiled 1.13
  million blocks in 12.4 s (11 µs a block, 6 % of the boot) to 448 MB of
  code (395 bytes a block), and the 64 MiB region was full and dropped 6
  times (H13c). CoreMark: 337 blocks in 1.5 ms (4.4 µs a block). The basis
  of an explanation, not measured: the emitter allocates its use lists and
  effect lists on the heap per block, and every drop of the region compiles
  the working set again.
- **J1c.** The gain is below the estimate of G1 (1.7 to 2 times on CoreMark).
  The basis of an explanation, not measured: every load and store goes
  through the thunk, which puts the words that it reads into the frame and
  saves the caller-saved registers that hold values. J3 inlines them.

### G2, measured by card J3

Helios, 2026-11-27, one vCPU, *measured*, the JIT on for both columns.
"Before" is J1 with card S3 (#1147, RAM straight from the TLB entry)
merged in, commit `55866877`, because J3 builds on the host address of the
S3 entry (H7 below). "After" is J3 on top of it, commit `4e079406`. Other
cards ran boots and builds at the same time (load average 4 to 9 of 32
threads). The rows are in `bench/history/aarch64.jsonl`.

| Workload | Before (J1 and S3) | After (J3) | Gain |
|---|---|---|---|
| CoreMark mix, flat, MIPS (median of 3, cores 28 to 31) | 196.1 | 228.9 | 1.17 times |
| Scalar loop, flat, MIPS (no load or store in its loop) | 175.2 | 167.8 | none: noise |
| Debian 13 boot to agent ready, s (median of 3, cores 28 to 31) | 180.3 | 152.3 | 1.18 times |
| The same, boot MIPS of the bench | 99.6 | 118.5 | 1.19 times |

- **J3a. The hit rate.** CoreMark: 67,145,095 inlined loads and stores in
  the blocks that ran to their end, 6 misses. The boot, at 17.0e9
  instructions: 3,448,872,419 inlined accesses and 70,434,658 misses, a
  hit rate of 98.0 %. In both, no block and no instruction of the flat
  images ran in the interpreter (H2b). The boot ran 44.8 million of 17.0e9
  instructions (0.26 %) in the interpreter, in 358 refused blocks, as in
  J1a.
- **J3b. One fast path.** The fast TLB is not a second softmmu: its entries
  are copies of the entries of the architectural TLB with the host address
  that card S3 put there, filled where the Rust paths of S3 reach RAM
  through that address and under the same rules (H7a, H11a), and flushed
  with them (H7b). The Rust paths are those of S3, unchanged.
- **J3c. H7e settled.** `mark_dirty` of `WriteDiscard` does two things.
  It moves the write generations (the stamps of code and of reservations),
  which a store needs only for a page with a watch word that is odd or
  with code of this CPU, and such a page has no write entry or tells the
  memory after its write (`WATCHED`), as the straight store of S3 does. And
  it drops the private pages of a region that `arm` armed (the PAM banks of
  q35), and a memory with such a region hands out no host pages at all
  (`GuestBus::direct`), so it has no fast entries. Nothing else reads it.
- **J3d. Exact for one vCPU and the same as S3 for more.** A store through
  the fast TLB reads the watch word after its write, as the straight store
  of S3 does, so the argument of S3 for a page that another vCPU starts to
  translate (`membarrier`) holds for it. H11c, which drops the write
  entries of every vCPU before the code is read, is J7.
- **J3e. The lockstep.** The lockstep keeps every access of the CPU, so its
  blocks are compiled with every hit kept (`jit::Memory::Logged`), with the
  same lookup as in a run without the log. K2 keeps the accesses of every
  other round and compares the two lists.

## 8. The cards

The order is the number. "With" names the cards that can run at the same
time. Every card:

- ends with `just ci::check` at exit 0, keeps X1 to X3 and lists the sources
  that it read,
- adds rows to `hv/bench/history/aarch64.jsonl`, before and after, three
  repeats, pinned: the flat CoreMark mix, the scalar loop, and the boot MIPS
  row of card #1142 (Debian, one vCPU), each with the count of blocks and
  instructions that ran in the interpreter (H2b),
- runs K1 for the instruction groups whose emitter forms it touches and K2,
  and from J2 on K3 for a Debian boot to the login prompt.

| Card | Title | What to do | Proof | After | With |
|---|---|---|---|---|---|
| J1 | JIT path end to end: an x86-64 emitter for the pure operations, behind a switch | The crate `vmlab-hv-tcg-jit` (H15): the assembler with its encodings tested against `llvm-mc`, the code region with the mapping of Q3, the trampoline (H4), the one-pass allocator with a spill (H3), emitter forms for the one-word operations of `block::Code`, guards and exits with the stubs of H9. Loads, stores, 16-byte accesses, generic operations and helper calls call the existing Rust functions of `exec` through one thunk. The block form moves to `vmlab-hv-tcg-ir`. `Compiled` gets its code address, the Rust loop enters it, a block with an unknown form runs in the interpreter (H2b). `VMLAB_HV_JIT` selects it, off by default. The budget check of H10 at the block head. K2 as a test and a recipe. The bench backend and its rows. | K1: the integer and branch groups of 3d cards C1 and C2, 100,000 instructions per format class, no difference. K2: one million random blocks, no difference, the seed in the report. CoreMark's checksums. Measured MIPS for CoreMark, the loop and a Debian boot, and the share of instructions in the interpreter. | #1048 merged | none |
| J2 | The replay check and the proof tools | K3 behind `VMLAB_HV_JIT_CHECK`. The perf map (H13e). A counter set in the debug log: blocks compiled, bytes, compile time, fallbacks, exits by kind. K6's tests moved under both runners. | A Debian boot to the login prompt under K3 without a difference. A seeded defect in the emitter (a test-only switch that flips one form) is caught by K2 and by K3. | J1 | J3, J4 |
| J3 | The inlined softmmu | The fast TLB (H7): its tables in the CPU context, the fill in the Rust load and store paths, the flushes of H7b, the write table rule of H11a for one vCPU, the inlined sequences for loads and stores of 1, 2, 4 and 8 bytes, the miss stub. H7e settled. | Unit tests of each rule H7a and H7b. K1 for the load and store group with the MMU on (the streams of 3d card C8). K6 with the JIT on. The rows, with the TLB hit rate. | J1 | J2, J4 |
| J4 | Chaining and the jump cache | H5, H6, H13c: patchable exits, the incoming list, unlinking, the jump cache with its clears, the full flush when the region is full. A switch that turns chaining off. | Tests: a chained pair is unlinked by a store into the target, a TLBI sends a cross-page branch through the lookup, and a context change is never chained. kvm-unit-tests `timer`, `gic` and `selftest` with the JIT on give the results of the interpreter. The rows, with the share of exits that are chained, hit the jump cache, or reach the Rust loop. | J1 | J2, J3 |
| J5 | Flags as host flags | H8a to H8c on the lazy record of card #1142. | K1 for every flag-setting and flag-reading format class. K2 with boundary operands. The rows. | J1, #1142 merged | J3, J4, J6 |
| J6 | The remaining forms | Emitter forms for two-word values, 16-byte and pair accesses, and a direct System V call for each helper kind (H12), until no block of a boot runs in the interpreter for a missing form. Optional and measured: the eager write-back of H3. | The fallback count of the Debian and Fedora boots is zero. K1 for the FP, AdvSIMD and system groups with the JIT on. K3 on a Debian boot. The rows. | J3 | J4, J5 |
| J7 | More than one vCPU | H11c, the kick through the budget from another thread (H10c), the TLBI broadcast with the fast TLB and the jump cache. The share of exclusives in the profile at 2 and 4 vCPUs. | A stress test: two threads, one stores into code that the other runs in a chained loop, 10 million rounds, no stale execution. The exclusives stress test of 3d card C3 with the JIT on. kvm-unit-tests at 4 vCPUs. The boot rows at 2 and 4 vCPUs. | J3, J4 | J5, J6 |
| J8 | Bar T1, and the JIT on by default | K7: the snapshot, reset and debug port tests of card C14 with the JIT on. K4 for Debian from reset to agent ready, started as the nightly run. The S1 matrix. `VMLAB_HV_JIT` on by default for aarch64. `bench-check` gates the JIT rows. VERIFICATION.md. | T1: the four templates at 1 and 2 vCPUs, three runs each, agent ready, no emergency mode, boot MIPS of Debian at least 150. The S1 snapshot tests. K4 without a CPU difference, or its first difference fixed. | J5, J6, J7 | none |
| J9 | Measure against D1 and plan the rest | The profile of W1 again on the JIT (boot and CoreMark, with the perf map), the D2 suite (`sha256`, `xz`) at 1 and 4 vCPUs against QEMU, and a design update with measured rows in place of G5: which of superblocks, a return stack, host-FPU fast paths, inlined AdvSIMD, LSE, cheaper exception entry, and a Cranelift tier for hot regions come next, as cards. | The numbers in the history and in this document. The answer to Q15 of the MAP proposed to the operator. | J8 | none |

J1 is small enough to prove the path: one crate, the emitter forms for the
operations that are 80 % of what a block holds, no softmmu, no chaining, and
the two proofs that every later card uses. It gives a measured MIPS number on
CoreMark and on a boot, which replaces the estimate of G1.

Side by side: after J1, three agents can work at once (J2, J3, J4). J5 waits
for card #1142. J6 and J7 follow J3 and J4.

## Questions for the operator

Operator answers, 2026-11-27, on card #1145 (questions 6, 7 and 8). Each
takes the recommendation, so the design above stands as written. The question
texts on the card quote the first spike run (Cranelift 0.123.23: 25 to 160 µs,
6 to 54 %, 39 crates). This document holds the run on Cranelift 0.138.0.

- **Q1. The back end: OUR OWN EMITTER.** The JIT of aarch64 and, through the
  shared crate, of riscv64 (Y1) is our own x86-64 emitter (O3). Cranelift
  stays possible later as a tier for hot regions if card J9 asks for it. The
  options not taken: Cranelift as D5 said, with a tiering card before J1, and
  both from the start.
- **Q2. When the JIT is on by default: AT T1.** D1 stays the tracked bar, and
  card J9 measures it and plans the rest. The option not taken: only when D1
  is met. Card C13 of 3d and the switch of vmlab to vmlab-hv for aarch64
  (3d:P4) wait for T1.
- **Q3. Memory rights of compiled code: THE DOUBLE MAPPING.** A `memfd`
  mapped twice, one view readable and writable for the emitter and one view
  readable and executable for the vCPU (H13d). The option not taken: one
  mapping with read, write and execute.

## How to measure again

On helios, in a card folder, as the account `vmlab`:

```sh
# E1: the flat images (appends to bench/history/aarch64.jsonl of the checkout)
python3 bench/run.py --backend qemu --arch aarch64 --vcpus 1 --repeats 3 --guest flat-a64 --cpus 28-31
python3 bench/run.py --backend interpreter --arch aarch64 --vcpus 1 --repeats 3 --cpus 28-31

# B1: the spike
cd hv/docs/plans/phase-3e-aarch64-jit/measure/jitspike
cargo build --release && taskset -c 28-31 target/release/jitspike 30000000

# W1: a binary with line tables and frame pointers
CARGO_PROFILE_RELEASE_DEBUG=line-tables-only RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --release -p vmlab-hv
cargo build --release -p vmlab-hv-tcg-aarch64 --example a64_bench   # the same flags
# the sampler: bench/ipsample.c with one more line in resolve(), which writes
# "<dso index> <offset>" of every sample to the file named by $IPRAW
python3 bench/run.py --backend vmlab-hv --arch aarch64 --vcpus 1 --repeats 1 \
  --guest debian-13 --suite boot --keep --cpus 12-15 --hv-bin <binary> &
IPRAW=raw-<t>.txt ipraw <pid of vmlab-hv> 10        # every 15 s of the boot
python3 measure/ipclass.py <binary> raw-*.txt --top 30
```

The boot MIPS of E2 are the differences of the instruction counter that the
vCPU writes to the debug log (`set_log_filter` with
`vmlab_hv_vmm::vcpu::emulated=debug`), as card #1048 read them. Every VM ran
under a memory cap, and the bench refuses a third VM on the host.
