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
  remove (R1). *Measured* by card J9 after J1 to J8: 279 MIPS on a Debian
  boot (42 % of QEMU's speed) and 2,025 MIPS on CoreMark (79 % of QEMU's).
  Only 19 % of the boot is in compiled code (J9a).
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
Card J9 measured them (J9b) and proposes an answer (Q4).

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
- **H13c. Eviction.** The region is cut into four parts, written one after
  another. When the part that the JIT writes into is full, it goes on to
  the next one, the part written longest ago, and drops only the code in
  it: the blocks of that part lose their code address, the jumps of the
  other parts that are linked into it go back to their stubs, and the jump
  cache is cleared. The code of the other three parts stays. The
  pre-decoded blocks stay in the block store, so a block is compiled again
  in about a microsecond when it runs next. Until card J11 the whole region
  was dropped, and the recompiles after a drop were 56 % of the compiles of
  a Debian boot (J11a).
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
  - **K3a.** `VMLAB_HV_JIT_CHECK_RUNS=<n>` checks the first n runs of each
    compiled block, and every later run is a plain run of its code. Every
    block that runs is still checked, at least once after each compile. A
    check of every run is too slow for the 90 s device timeouts of a Debian
    boot (J2a), so the boot of the proof uses n = 1000.
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
| G5 | All of J1 to J8 | A Debian boot at 1 vCPU: 279.2 MIPS, 64.4 s to agent ready. CoreMark: 2,024.8 MIPS. The scalar loop: 2,040.4 MIPS. The estimate was 250 to 450 and 1,500 to 3,000 MIPS. | measured by card J9 (J9b) |
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
### J2, the proof tools (measured)

Helios, 2026-11-27, *measured*, with the load average of the host at 5 to
14 of 32 threads. The tools: `VMLAB_HV_JIT_CHECK` (K3, K3a),
`VMLAB_HV_JIT_PERF_MAP` (H13e), the counters of the debug log, and
`Cpu::seed_jit_defect` for the tests.

| Proof | Result |
|---|---|
| K3 on a Debian 13 boot, 1 vCPU, `VMLAB_HV_JIT_CHECK=1` and `VMLAB_HV_JIT_CHECK_RUNS=1000` | The login prompt on `ttyAMA0` after 244.1 s, no difference. 855,603 compiles and 134.3 million checked runs (637.7 million instructions) up to the last counter line, at 18.0e9 instructions. The same setting reached agent ready in 266.6 s in a run of the bench. |
| K3 of every run on the same boot | No difference in 2.45 billion checked runs (14.97e9 instructions). At 32 MIPS the boot fell into emergency mode on the 90 s device timeouts after 544 s, before the login prompt (J2a). |
| K3 on the K2 streams, seed `0x3e0001`, 1,000 rounds (`tests/jit.rs`) | 5,185 checked runs, no difference. |
| A seeded defect (`Defect::AddSubtracts`: `Code::Add` subtracts) | K2 finds it in round 1 (the states differ). K3 finds it in round 1: a store at another address than the interpreter's. |

The rows before (`cc6060d7`, `next`) and after (`76a9a1dd`), interleaved on
the same cores, in `bench/history/aarch64.jsonl`. MIPS, median of 3, cores 20
to 23 for the flat images; one boot each, side by side on cores 12 to 15 and
16 to 19. The second pass of the flat images ran while the load rose; its
rows are in the history too.

| Workload | Before | After |
|---|---|---|
| CoreMark mix, flat, JIT | 160.3 | 158.4 |
| Scalar loop, flat, JIT | 176.5 | 175.5 |
| CoreMark mix, flat, interpreter | 133.5 | 113.3 |
| Scalar loop, flat, interpreter | 149.7 | 131.4 |
| Debian 13 boot with the JIT, to agent ready, s | 219.1 | 187.1 |
| The same, boot MIPS | 82.5 | 96.0 |
| Instructions in the interpreter (H2b) in the boot, in blocks | 48.6 million in 6.17 million | 45.2 million in 5.74 million |

No instruction of the flat images ran in the interpreter. The interpreter rows
of the flat images moved by 13 % between the two passes of one commit as well
(`cc6060d7`: 133.5 and 108.9 on CoreMark), so the difference in that pair is
the noise of the host. J2 changes no emitter form and the plain run of a
compiled block only by its counters.

- **J2a.** K3 costs per block a copy of the 72 register slots and one
  compare, the slow paths of the loads and stores in the interpreter (the
  log is on), and the replay through its own thunk. A block with an ordered
  helper call copies and compares the whole state. The first build cloned the
  whole state twice per block and formatted the two ends of every run: 15 to
  20 MIPS. Without both, 32 MIPS. A first build of J2 also slowed the plain
  JIT by a fifth (145 against 181 MIPS on the loop): the replay was a branch
  in the one thunk, and the run of compiled code was no longer inlined.
- **J2b.** The perf map is `/tmp/perf-<pid>.map` behind
  `VMLAB_HV_JIT_PERF_MAP=1`: the trampoline as `vmlab-hv-jit enter`, and each
  block as `a64 <pc> pa <physical address> el<n> <count> insns`. After a reset
  of the region an address can carry two names: the later line names the
  code that is there now.
- **J2c.** The counter line of the debug log (every 1e9 instructions, from
  `vmlab_hv_vmm::vcpu::emulated`) gains the fallbacks (runs of refused blocks,
  and runs that the budget sent to the interpreter), the exits of compiled
  runs by kind (the end of the block, an exception, a store into translated
  code, a stop) and the runs and instructions that K3 checked. The bench reads
  them into `jit_reports`.
- **J2d.** K6: the tests of a store into translated code (into its own
  block, through another address, under an address that maps other code, at
  two addresses, a write from outside, a data store that keeps the blocks)
  run on the interpreter, on the JIT and on the JIT with K3, and a new test
  stores from a second CPU into code that the first ran. A chained pair waits
  for J4.
- **J2e.** No emitter form changed, so K1 had no group to run. K2 ran in
  `ci::check` (`tests/jit.rs`).
### G3, measured by card J4

Helios, 2026-11-27, one vCPU, *measured*. Before: `b640db7c` (`next` with
J1). After: `f286f9b3`. Other cards ran boots and builds at the same time
(load average 7 to 27 of 32 threads). The rows are in
`bench/history/aarch64.jsonl`.

| Workload | JIT, J1 | JIT, J4 | Gain |
|---|---|---|---|
| Scalar loop, flat, MIPS (median of 3, cores 28 to 31) | 163.6 | 1,901.9 | 11.6 times |
| CoreMark mix, flat, MIPS | 143.6 | 315.2 | 2.2 times |
| Debian 13 boot to agent ready, s (median of 3, cores 12 to 15) | 168.4 (166.3 to 170.5) | 136.4 (132.4 to 157.8) | 1.23 times |
| The same, boot MIPS | 106.5 | 132.1 | 1.24 times |

- **J4a. Where the exits go.** An exit of a block is a linked jump, a hit
  of the jump cache, or a return to the Rust loop. CoreMark: 54.37 million
  linked, 1.65 million hits, 463 to the loop. The boot, the last report of
  the first run (18.0e9 instructions): 2,496 million linked (81 %), 339
  million hits (11 %), 247 million to the loop (8 %), of which 106 million
  are misses of the jump cache (a hit rate of 76 %) and 33 million are jumps
  not linked yet. The jump cache was emptied 1.28 million times: every write
  of TTBR0 or TTBR1, every TLBI, and every new epoch. The kernel has no
  hardware PAN (`cortex-a53`), so it writes TTBR0 at every entry from EL0
  and every return to it.
- **J4b. The interpreter share** is that of J1: 47.4 million of 18.0e9
  instructions (0.26 %) in 6.0 million runs of 361 refused blocks. No
  instruction of the flat images runs in the interpreter.
- **J4c. With `VMLAB_HV_JIT_CHAIN=0`** the flat rows are those of J1 (163.1
  and 142.9 MIPS): the switch leaves the code of J1.
- **J4d. Code.** The boot compiled 826,000 blocks to 284 MB of code (344
  bytes a block) and dropped the region 4 times, against 550,000 blocks, 120
  MB and one drop before: the exits are longer, and every drop compiles the
  working set again.
- **J4e. Two boots fell into emergency mode** at the first commits of the
  card (`01d6a923`, `e8e239ff`), both with heavy work of other processes on
  the host: udev did not find the EFI partition within its 90 s, where the
  other boots needed 22 to 35 s. A boot with only the linked jumps, one with
  only the jump cache, and one with both passed at low load. At those
  commits each clear of the jump cache wrote all of its 128 KiB, and a phase
  with many system calls paid two to four of those writes for each call.
  Since `ca9d9be6` a clear moves a generation that every key holds, and the
  three boots of the rows passed at a load of 15 to 27. That the clear was
  the cause, and not a defect of the chaining, is an explanation and not
  proven. Its basis: the failed phase ran the same instructions in more host
  time, the boots of each half and the boots after the change passed, and
  K1 and K2 found no difference.

### J7, more than one vCPU (measured)

What card J7 built:

- **J7a. The seat (H10c).** The context of compiled code is no longer on
  the stack of a run. It is in a seat (`vmlab-hv-tcg-jit::seat`) at an
  address that lives as long as the CPU: the seat of its interrupt lines
  (`exec::Lines`), or a seat of its own. Another thread kicks the run with
  one atomic subtraction of 2^40 from the budget, and the next block head
  leaves. The head subtracts with a plain read-modify-write, so a kick that
  lands inside it is lost. A thread that waits for the CPU kicks again
  every 64 turns of its wait. The owner reads the budget back as its
  remainder by 2^40, so the count of instructions is exact after any
  number of kicks, and a run holds at most 2^38 instructions.
- **J7b. Who kicks.** An interrupt line that rises (`CpuLine::set_level`).
  A kick of the VMM (a pause, a snapshot, a change of the run state), which
  also sets `Lines::leave`, so that `run` ends at the next block and not at
  the end of its slice. Every post to a mailbox. The thunk kicks its own
  run, as before, after a store into translated code, and after a wait of
  J7c in which the CPU took code mail or an invalidation of its TLB: the
  rest of the run would follow links and a jump cache of translations
  that are gone. A line and a kick of the VMM kick once. When the head of a
  block overwrites that kick, the interrupt or the pause waits for the end
  of the run, as before J7 (`CHAIN_SLICE`); only a waiter of a mailbox
  kicks again.
- **J7c. Code mail (H11a with several vCPUs).** A store that ended
  translated code of some CPU made the watch word of its page even. The
  memory counts these writes for each handle (`Memory::ended_code`). The
  CPU that made the store posts code mail to the others, kicks them and
  waits until each one that runs has taken it, before the store
  instruction completes. It waits after it lets go of the lock of an
  exclusive store. A CPU that takes code mail starts a new epoch: its
  compiled code leaves at the next block head, every link is unlinked,
  and each block passes its code stamp again before it runs. Before J7 a
  CPU learned of such a store at the end of its slice of 65,536
  instructions, and ran the old code of a chained loop until then.
- **J7d. H11c.** A CPU that translates code in a page that it holds no
  code of posts the page (`Invalidation::CodePage`) after it makes the
  watch word odd and before it reads the code. It waits until every other
  CPU that runs has dropped its write entries of the page from its fast
  TLB. A translation whose watch word changed while it read the code
  (`watch_holds` is false) ends its epoch after its run: a block without a
  stamp chained and ran until the next epoch before.
- **J7e. The TLBI broadcast** is that of card C13 with the kick. A CPU
  applies the mail to its architectural TLB, which flushes the fast TLB the
  same way (H7b), and the next run empties the jump cache, because the
  changes of the MMU moved. A broadcaster waited up to `CHAIN_SLICE` (4,096
  instructions) of each other CPU before J7 and waits one block now.
- **J7f. Counts.** The JIT line of the debug log ends with what the CPUs
  of a machine told each other: exclusive loads and stores, code mails,
  code pages, and the time spent waiting. `bench/lib.py` reads them.

Proof, helios, 2026-11-27, *measured*:

| Proof | Result |
|---|---|
| The stress test of J7 (`a_cpu_in_chained_code_runs_the_code_that_another_cpu_stored`, two CPUs on two threads on a `GuestBus`, the JIT and chaining on): the writer stores `MOVZ X1, #round` into a function that the runner calls from a chained loop, then publishes the round; the runner checks the value and acknowledges | 10,000,000 rounds in 43.2 s, no stale execution (`VMLAB_HV_J7_ROUNDS=10000000`). The test of `ci::check` runs 20,000 rounds. A build that told the others after the stores of the Rust paths but not after the store that `Memory::write` serves stopped in its first rounds with the runner on old code: the test sees the defect. |
| The exclusives stress test of card C3 (`two_cpus_count_under_a_spinlock_and_with_exclusives_and_lose_nothing`) | Passes on the interpreter and with the JIT and chaining, both with the mailboxes of two CPUs. |
| K3 on a Debian 13 boot at 2 vCPUs, `VMLAB_HV_JIT_CHECK=1` and `VMLAB_HV_JIT_CHECK_RUNS=1000` | Agent ready after 93.7 s, no difference. 150.1 million checked runs (722.7 million instructions) over both vCPUs up to their last counter lines, at 18.0e9 instructions. |
| kvm-unit-tests (`just tcg-a64-kvm-unit-tests`, `VMLAB_HV_JIT=1`; `selftest-smp`, `gicv2-ipi`, `gicv2-mmio`, `gicv2-mmio-3p`, `gicv2-active` and `psci` at 4 vCPUs) | 11 tests. vmlab-hv passes every subtest that it passes on the interpreter. QEMU failed `psci: cpu-on` in one of its two runs. |

The rows, before (`6242195c`, `next`) and after (`3b07327b`), side by
side: cores 20 to 23 and 24 to 27 for the boots, 28 to 31 for the flat
images, the median of 3, at a load of 3 to 11 of 32 threads. The boot rows
of both sides are marked dirty: the flat rows of the same checkout were
written first.

| Workload | Before | After |
|---|---|---|
| Scalar loop, flat, JIT, MIPS | 1,876.0 | 1,959.2 |
| CoreMark mix, flat, JIT, MIPS | 1,479.9 | 1,558.0 |
| Debian 13 boot to agent ready, 1 vCPU, s | 102.9 | 93.0 |
| The same, boot MIPS | 174.3 | 191.7 |
| Debian 13 boot, 2 vCPUs, s | 80.2 | 72.2 |
| The same, boot MIPS (both vCPUs) | 231.0 | 256.2 |
| Debian 13 boot, 4 vCPUs, s | 108.8 | 114.3 |
| The same, boot MIPS (four vCPUs) | 179.0 | 171.8 |
| Instructions in the interpreter (H2b), 1 vCPU, at the last report | 44.0 million of 17.0e9 in 5.60 million runs | 43.5 million of 17.0e9 in 5.56 million runs |

No instruction of the flat images ran in the interpreter. The 4 vCPU
boots of both sides slowed from repeat to repeat (76 to 143 s and 72 to
145 s) while the load of the host rose, so that row holds no difference
of the two.

- **J7g. The share of exclusives** (after, the sum over the vCPUs at their
  last report, about 17e9 instructions). It is a share of the instructions
  that ran, from the counts of J7f, not a sampled profile of the host time.

  | vCPUs | Exclusive loads | Exclusive stores | Share of the instructions | Code mails | Code pages | Waiting |
  |---|---|---|---|---|---|---|
  | 1 | 12.0 million | 11.9 million | 0.14 % | 0 | 0 | 0 |
  | 2 | 27.3 to 30.9 million | 11.3 to 12.9 million | 0.23 to 0.24 % | 2,412 to 2,559 | 11,259 to 11,573 | 114 to 136 ms |
  | 4 | 44.9 to 64.6 million | 10.6 to 11.2 million | 0.33 to 0.42 % | 2,392 to 2,442 | 16,408 to 17,447 | 2.6 to 8.2 s |

  The stores stay at about 11 million and the loads grow with the vCPUs:
  the extra loads are spins of a lock (`LDAXR` until the lock is free). At
  4 vCPUs on 4 host cores the waits of J7c and J7d took 2.6 to 8.2 s of
  CPU time over a boot of 72 to 145 s, 0.2 to 0.5 ms a wait. The basis of
  an explanation, not measured: a waited CPU whose thread is not on a
  core (four vCPU threads and the threads of the devices share four
  cores) takes its mail only when it runs again.

### G4, measured by card J5

Helios, 2026-11-27, one vCPU, cores 28 to 31, three repeats, *measured*.
Both sides are one binary: `VMLAB_HV_JIT_FLAGS=0` turns the forms of H8 off
and leaves the forms of J1. Other cards ran builds and boots at the same time
(load average 5 to 25). The rows are in `bench/history/aarch64.jsonl` with
the note "card J5".

| Workload | J1 forms | Host flags (J5) |
|---|---|---|
| CoreMark mix, flat, MIPS (median) | 164.4 | 169.6 |
| Scalar loop, flat, MIPS (median) | 158.3 | 154.4 |
| Debian 13 boot MIPS to agent ready (median, two sets) | 132.7 and 109.0 | 127.0 |

- **J5a.** What is built. H8a: a comparison (`Equal`, `NotEqual`, `Less`,
  `LessEqual`, `IsZero`) whose one reader is the next operation, a `Select`
  or a `Guard`, is a `cmp` and a `cmov` or a `jcc`. A signed order, which the
  lifter writes as an unsigned order of two values with flipped sign bits,
  is a `cmp` at 32 or 64 bits with a signed condition, and an `And` compared
  with zero is a `test`. H8b needed no change: the lifter of #1142 writes the
  record (the kind and two operands) as outputs of the block, a store each.
  H8c: a call of `a64_condition` is inline for seven kinds of record (a
  compare at 64 and 32 bits, a logical result at both, the explicit flags as
  a mask of 16 bits, a sum without a carry in at both), from the map
  `flags::host_forms`, and a call for the rest. HI and LS after a sum have
  no host condition and call.
- **J5b.** The gain is below G4 (1.2 to 1.7 times): CoreMark gains 3 %, and
  the loop and the boot move by less than the noise. CoreMark has 545 fused
  comparisons in 337 blocks and no lazy condition. The basis of an
  explanation, not measured: a fused comparison saves a `setcc`, a `test`
  and a register, about two host instructions of the 40 to 60 of a block,
  and every block still leaves to the Rust loop. The spike measured G4 on
  chained blocks, which J4 brings.
- **J5c.** Not fused: the `And` and `Or` of comparisons that `CCMP` leaves,
  a lazy condition whose reader is a `Select` (it is a `setcc` and a
  `test`), and a comparison that is not just before its reader. In the
  blocks of `CMP` with `B.cond`, `CSEL` and `CSET`, `TST` with `B.cond` and
  `CMN` with `CSET`, the reader is always next.
- **J5d.** One boot of the twelve went to emergency mode, with the host
  flags on: `systemd-udevd` took more than 90 s to start, `/boot/efi` timed
  out, and the vCPU ran 1e9 instructions in 66 s, so the guest was mostly
  waiting. That set of three has no row. The other five boots with the host
  flags on and all six without them reached agent ready. No serial log of
  the twelve has a segfault, a trap or an oops. K1 and K2 found no
  difference. This card does not show whether the JIT caused it. It is a
  risk for T1 (J8).

### J6, measured by card J6

Helios, 2026-11-27, one vCPU, *measured*, the JIT on for both columns.
Before: `6242195c` (`next` before J5 was merged). After: `078143df`, the
same base with this card. Both sides lack J5 (G4). The two boots of
each pair ran side by side, on cores 12 to 15 and 16 to 19. Other cards ran
builds and boots at the same time, and the merge bar of this card ran
during the third pair of Debian boots (load average 6 to 15 of 32 threads).
The rows are in `bench/history/aarch64.jsonl`.

| Workload | Before | After | Gain |
|---|---|---|---|
| CoreMark mix, flat, MIPS (median of 3, cores 28 to 31) | 1,562.9 | 1,854.2 | 1.19 times |
| Scalar loop, flat, MIPS | 1,967.9 | 1,983.0 | none: noise |
| Debian 13 boot to agent ready, s (median of 3) | 88.3 (82.7 to 112.3) | 84.4 (81.4 to 118.6) | 1.05 times |
| The same, boot MIPS | 203.1 | 212.3 | 1.05 times |
| Blocks refused by the emitter in the Debian boot (H2b), each repeat | 357, 347, 354 | 0, 0, 0 | |
| Instructions in the interpreter in the Debian boot, each repeat | 44.0, 43.7, 46.9 million in 5.60, 5.57, 5.98 million runs of refused blocks | 15, 9, 0, in 2, 1, 0 runs at the budget (H2c) | |
| Fedora 44, the same counts at 29e9 to 31e9 instructions (J6f) | 757 blocks refused, 283.3 million instructions in 21.4 million runs | 0 refused, 30 and 34 instructions in 3 runs at the budget, in two runs | |
| K3 on Debian 13 (`VMLAB_HV_JIT_CHECK_RUNS=1000`), agent ready, s | | 144.2, no difference, 137.3 million runs checked | |

- **J6a. The fallback count is zero.** No block of the Debian or the Fedora
  boot runs in the interpreter for a missing form. What runs there is H2c:
  a block that the budget does not hold whole runs one instruction at a time
  (0 to 15 instructions a boot). No instruction of the flat images runs in
  the interpreter, before or after.
- **J6b. What the boots refused** was `RBIT`, as J1a found: with its form
  no block is refused, and on a host with `lzcnt`, `bmi1` and `popcnt`
  (helios) the emitter has no other refusal of a form.
- **J6c. The other forms** replace calls of the thunk, which were no
  fallback. The gain on CoreMark is theirs: no block of the flat images was
  refused before either. Which of them gives it is not measured. The basis
  of an explanation: its functions save and restore registers in pairs,
  and an `LDP` or an `STP` is now inline over the fast TLB.
- **J6d. Code.** The Debian boot compiled 0.95 million blocks to 455 MB of
  code (479 bytes a block) and dropped the region 6 times, against 0.88
  million, 368 MB (418 bytes a block) and 5 drops before, and every drop
  compiles the working set again. Compile time 6.8 s against 5.7 s. The
  basis of an explanation, not measured: a pair and a generic of two words
  are longer inline than a call of the thunk.
- **J6e. Not done:** the eager write-back of H3, which the card left
  optional: neither built nor measured.
- **J6f. Fedora at one vCPU** reached emergency mode in both runs of this
  card and in the run of `next` beside the first, on a 90 s device timeout
  (`boot-efi.mount`, `boot.mount`), as card #1048 found. Its counts in the
  table are those of the counter line at 29e9 to 31e9 instructions, into
  systemd, of the 30.7e9 of a whole boot (3d:M2). T1 (J8) still asks for
  Fedora at agent ready.

### T1, measured by card J8

Helios, 2026-11-28, *measured*. The JIT is on by default (Q2): every boot
of the after column ran with no `VMLAB_HV_JIT` in its environment, and each
vCPU said in its log that the JIT was on. Other cards ran builds and boots
at the same time (load average 2 to 10 of 32 threads). The rows are in
`bench/history/aarch64.jsonl` with the note "card J8". The boots of the
1 vCPU column ran on cores 20 to 23, those of the 2 vCPU column on cores 24
to 27, the two columns side by side.

| Template | 1 vCPU, s (each repeat) | 2 vCPUs, s (each repeat) |
|---|---|---|
| alpine-3.23 | 124.3, 88.4, 93.9 | 108.6, 120.3, 96.3 |
| debian-13 | 92.5, 78.2, 96.2 | 57.4, 60.2, 53.6 |
| fedora-44 | 168.5, 148.4, 150.4 | 168.5, 159.0, 160.4 |
| ubuntu-24.04 | 108.7, 107.0, 106.8 | 100.3, 96.4, 95.0 |

- **J8a. T1 holds.** All 24 boots reached agent ready. No serial log of
  the 24 has a line of emergency mode, a failed dependency or a timed-out
  job. The boot MIPS row of Debian at one vCPU is 195.5 (189.6 to 230.3),
  above 150. Fedora at one vCPU, which fell into emergency mode in every
  run of J6 (J6f) and of #1048, reached agent ready in 148 to 169 s.
- **J8b. The fallback count is zero** in every boot: no block refused, and
  85 to 7,788 instructions a boot in the interpreter at the budget (H2c).
- **J8c. Before and after.** The JIT code did not change, only its default,
  so the two sides measure the same code. Before: `next` (`a734a48c`) with
  `VMLAB_HV_JIT=1`. After: this card with no switch. Side by side, Debian at
  one vCPU, the median of 3, cores 12 to 15 and 16 to 19: 70.1 s (256.5
  MIPS) before and 74.6 s (241.2 MIPS) after, inside the spread of each
  side (23.9 and 23.0 s). An earlier pair one after the other on cores 20
  to 23: 57.1 s before and 76.2 s after, with the load of the host
  changing between them. The flat images, cores 16 to 19: CoreMark 2,036
  MIPS before and 2,075 after, the scalar loop 2,091 and 2,126. No
  instruction of the flat images ran in the interpreter.
- **J8d. bench-check** compares a JIT row with the JIT row before it and an
  interpreter row with the interpreter row before it, leaves the rows of K3
  out, and fails a JIT row whose boots ran more blocks in the interpreter
  for a missing form than the JIT row before it.
- **J8e. K7.** The snapshot, debug port and reset tests of card C14 run with
  the JIT and on the interpreter, in `vmlab-hv-vmm` and at S1 over the
  control socket (VERIFICATION.md, card J8).
- **J8f. K4, the first difference and its fix.** `just
  tcg-a64-boot-lockstep` (AAVMF 2026.08+ds-2 of `guest/fetch-firmware.sh`,
  QEMU 11.1.3, cores 28 to 31) with the JIT on, as every machine CPU runs
  now: every instruction ran as a compiled block of one (the report counts
  the runs of compiled code). The first run stopped at a CPU difference
  after 318,699,178 instructions: at an `ISB` after `MSR DAIFClr` our CPU
  took an interrupt that the leader did not take there. It was the harness,
  not the JIT: since card C13 a CPU of a machine reads the lines of its
  machine before every block, so the follower took an interrupt that its
  GIC had pending, where the lockstep means the leader to lead every
  interrupt. The interpreter reads them the same way, and no O4 run had
  passed that point since C13. The lockstep now runs its CPU without those
  lines. The second run had no CPU difference in 351,352,701 instructions,
  all of them compiled, with 4 interrupts taken where the leader took them,
  each pending in our GIC, 2 host calls and 1,266 device reads with no
  device finding. There the leader stopped writing its stream, before the
  boot menu: QEMU with the plugin and no follower (`--leader-only 1`)
  stops at the same instruction, 351,352,700, so the end of that run is
  the leader's and not ours. Why QEMU stops there is not known.
- **J8g. Not done: K4 for Debian to agent ready.** The boot lockstep boots
  the firmware with no disk. A Debian boot needs the disk and the virtio
  devices on both sides, with the leader leading their completions, and an
  agent at its end. The leader runs at 0.53 million instructions a second
  with the plugin, so the 18e9 instructions of a Debian boot are about 9.4
  hours. Neither the follower for that boot nor its nightly run was built.

### C13b, measured by card C13b (#1144)

Helios, 2026-11-28, *measured*, `next` at `4af3439a` with the JIT on by
default. Other cards ran builds and boots at the same time, and so did this
card: its merge bar, the boot that wrote the windows of O4w and the window
lockstep (load average 4 to 12 of 32 threads).

**The stack profile.** Debian 13 to agent ready at one vCPU, cores 16 to
19, a W1 build (line tables, frame pointers), `VMLAB_HV_JIT_PERF_MAP=1`,
sampled by `measure/ipstack.c` at 1000 Hz with the user call chain over
frame pointers, every 5 s from the release of the CPUs to agent ready (79.0
s, 16 windows, 73,954 samples, 99.2 % of them on `vcpu-0`), and resolved by
`measure/ipstack.py`. Compiled code keeps no frame pointer, so a chain that
starts in it ends there: the leaf is what the numbers below count.

| Leaf (its caller) | Share of the vCPU |
|---|---|
| Compiled code | 17.3 % |
| `Seat::prepare` (`run_chained`) | 10.7 % |
| libc, a function that it does not export, with no frame below (called from compiled code or a helper) | 8.4 % |
| `run_chained` (`execute`) | 5.4 % |
| `execute` (`run_inner`, and `Cpu::run_block_within`) | 6.0 % |
| `Cpu::run` (`EmulatedVcpu::run`) | 4.5 % |
| `thunk` (compiled code) | 3.3 % |
| `Jit::link` (`make_link`) | 2.5 % |
| The map of blocks by (address, context): `remove` and `insert` | 2.2 % |
| libc again, called from `Cpu` (1.4 %), from `asynchronous` (1.1 %), from the translation (1.0 %) and from `Vec` growth (1.3 %) | 4.8 % |
| `Cache::revive` | 1.2 % |
| `GuestBus::write` and `load_reserved` behind a store pair, a `DC ZVA` and a load | 2.5 % |
| The translation (class of `ipclass.py`) | 6.0 % |
| FP and vector helpers (`simd::same`, `lane`, `eval`, `run`) | 2.7 % |

- **C13b-P1. The three largest costs that remain.**
  1. *Into and out of compiled code*: `Seat::prepare`, `run_chained`,
     `execute`, `Cpu::run`, `run_inner` and the thunk, 31.5 % together. The
     J8 rows count 247 million entries into compiled code in a Debian boot,
     one for every 73 instructions. `Seat::prepare` writes the budget of the
     seat with a sequentially consistent store at each entry, which on x86
     waits for the store buffer of the code before it (basis of the
     explanation, not measured apart): its 10.7 % of 79 s are about 34 ns
     an entry.
  2. *Translation and the code cache*: the lift and the emitter (6.0 %),
     `Jit::link`, the map of blocks, `Cache::revive` and the allocations of
     the translation, about 14 % together. J6d counts 0.95 million blocks
     and 6 drops of the region in a boot, each drop compiling the working
     set again.
  3. *Memory behind the fast TLB*: the libc function with no frame (8.4 %),
     libc under `asynchronous` (1.1 %) and the RAM paths of `GuestBus`
     (2.5 %), about 12 %. Which libc function it is was not resolved: glibc
     does not export its memmove and memset, and no caller is in the chain.
- **C13b-P2.** The guest's own work, compiled code, is 17.3 % of the vCPU.

**S1, the boot matrix.** The four templates at 1 and 2 vCPUs, three repeats
each, to agent ready (`bench/run.py --suite boot`), the 1 vCPU column on
cores 20 to 23 and the 2 vCPU column on cores 24 to 27, side by side. This
card adds no row to `bench/history/aarch64.jsonl`: it changed no speed, and
its rows ran on a host that other cards and this card loaded, so they would
be a wrong base for the next speed card to compare with.

| Template | 1 vCPU, s (each repeat) | 2 vCPUs, s (each repeat) |
|---|---|---|
| alpine-3.23 | 113.6, 124.3, 98.7 | 108.2, 95.0, 88.3 |
| debian-13 | 76.8, 84.6, 80.5 | 54.8, 54.5, 54.1 |
| fedora-44 | 172.5, 160.5; 149.6, 157.1, emergency; emergency; emergency (C13b-S1) | 160.3, 160.4, 182.0 |
| ubuntu-24.04 | 193.7, 144.2, 188.6 | 123.0, 154.7, 242.0 |

- **C13b-S1. S1 does not hold for Fedora at one vCPU.** Of seven boots
  (a run of three cut short by the VM cap of the host after two repeats, a
  run of three, and two more runs that this card stopped after their first
  repeat), four reached agent ready in 149.6 to
  172.5 s and three went to emergency mode on the 90 s device timeout of
  systemd (`dev-disk-by-uuid`, once also `dev-zram0`), the failure of
  #1048 and J6f. In each of the three the rate of the vCPU fell from 170 to
  240 MIPS to 37 to 130 MIPS from about 80 s on, against 115 to 170 MIPS
  in the same phase of a good boot, and the guest then waited in emergency
  mode. The host
  had a load of 7 to 12 then: the boot that wrote the windows (68 copies of
  2 GiB of RAM), the window lockstep, the merge bar of this card, and the
  boots and builds of other cards, once a VM of another card pinned to the
  same cores 20 to 23. J8 had all three Fedora boots at one vCPU at agent
  ready in 148 to 169 s on a host at a load of 2 to 10. So Fedora at one
  vCPU reaches agent ready with no margin on a loaded host: the remaining
  speed is the three costs of C13b-P1.
- **C13b-S1a. Decision of the leader (2026-11-27): the card lands with this
  cell open.** Guest timeouts stay as they are (decision of the operator),
  so the fix is speed. J10 (entry and exit, 31.5 % of C13b-P1), J11
  (translation and the code cache, about 14 %) and J12 (the fast TLB, about
  12 %) work on the three costs, and a follow-up card measures Fedora 44 at
  one vCPU again, three repeats on S1's terms, after all three of them.
- **C13b-S2.** No serial log of the other 21 boots of the matrix has a line
  of emergency mode, a failed dependency, a timed-out job, an oops or a
  segfault, and each of them reached agent ready inside 600 s (the longest
  242.0 s). Repeats that the bench refused for
  want of a VM slot on the host (the cap of four counts the QEMU leaders of
  the lockstep too) were run again and are not in the table.
- **C13b-S3.** The Ubuntu boots of 190 to 242 s ran while the merge bar of
  this card built and tested on the same host.

**O4, the boot lockstep of Debian.** O4w of 3d (`just
tcg-a64-window-lockstep`). Debian 13 at one vCPU wrote 68 windows, one every
250 million instructions from 1e9 to 17.75e9, and reached agent ready in
124.9 s. The window lockstep, cores 16 to 19, the JIT on, a budget of 40
million instructions a window, compared 1,821,246,744 instructions in 3,410
s with no CPU difference: 769 million in 24 windows of the firmware and the
boot loader, 866 million in 33 windows of the kernel with 47,999 exceptions,
and 186 million in 11 windows of user space with 14,682 exceptions. 502 of
the exceptions were taken at a fetch. 39 windows ended at their budget, 28
at an access outside RAM and one at `WFI`. The boot and the run took 59
minutes, inside the two hours of the card.

- **C13b-O1. Two defects of the harness**, both found by the first windows
  and fixed: an instruction abort at the fetch of the next instruction has
  no record of its own in the stream of the leader, so the follower steps
  into it; and the plugin wrote the PC of an instruction as the address
  that QEMU translated its block at, plus the offset. One block of QEMU
  serves two virtual addresses of the same code (a shared library in two
  processes), and the record then named the address of the other process.
  The plugin now takes the PC register at the start of each block. With it
  the AAVMF run of K4 (J8f) went past 351,352,700 instructions, where the
  stream of the leader had stopped, sent the four keys of the boot menu,
  and had no CPU difference in the 469.8 million instructions until this
  card stopped it after 902 s. The O3 group `c1` gave no difference with
  the new plugin (29,023 instructions in 1,400 rounds).
- **C13b-O2. kvm-unit-tests** (`just tcg-a64-kvm-unit-tests`, the JIT on by
  default; `selftest-smp`, `gicv2-ipi`, `gicv2-mmio`, `gicv2-mmio-3p`,
  `gicv2-active` and `psci` at 4 vCPUs, `timer` at 1): 5 tests give the
  results of QEMU, and the two `gicv2-mmio` tests pass subtests that fail on
  QEMU, as in J7.
- **C13b-O3. What O4w does not prove** is in 3d O4w (W1 to W5): interrupts,
  devices and DMA, the instructions between the windows, and a defect of
  ours earlier in the boot that left a state that is consistent.

### G5 and D1, measured by card J9

Helios, 2026-11-28, *measured*, at commit `4af3439a` (`next` with J1 to J8,
the JIT on by default). Other cards ran builds and boots at the same time
(load average 3 to 9 of 32 threads), and the host-wide VM cap of the bench
made some runs wait for a slot. The rows are in
`bench/history/aarch64.jsonl` with the commit `4af3439a`. The rows of the
profiling runs below are not in the history: they ran a binary with frame
pointers and line tables.

The bench runs QEMU with `-cpu max` and vmlab-hv with its own set, the ID
registers of a `cortex-a53` (3d) with AES, SHA-1, SHA-256 and CRC32 and
without LSE. So the guest can choose other code on the two sides (the
atomics of the kernel, the string functions of glibc). The same Debian
image and the same commands run on both.

**J9a. W1 again, on the JIT.** The sampler of W1 with three changes
(`measure/ipraw.c`): it samples only the vCPU threads (`IPCOMM=vcpu-`), it
writes a sample in no mapped object as compiled code, and it writes the
first return address in the binary of each sample's call chain, so that a
sample in libc is classed by its caller where the frame pointers allow.
`measure/ipclass.py` names compiled code by the perf map of the run
(`VMLAB_HV_JIT_PERF_MAP=1`) and has three new classes for the Rust side of
the JIT. Sources read for the sampler: the `perf_event_open(2)` manual
page (the layout of a sample with a call chain). No QEMU source was read
(X1 to X3). Debian 13 at one vCPU through the bench's full suite, cores 16 to
19, one run, samples cut at agent ready and at the ends of the two
workloads by the times of the bench log. CoreMark: three runs of a flat
image of 30,000 iterations (4.30e9 instructions each), cores 16 to 19.

| Class | Boot to agent ready | `sha256` | `xz` | CoreMark, flat |
|---|---|---|---|---|
| Samples | 70,379 | 7,994 | 6,997 | 4,172 |
| Compiled code: the blocks, the trampoline, and code the perf map does not name | 18.9 % | 4.4 % | 25.4 % | 99.9 % |
| Entering and leaving compiled code in the Rust loop (`Seat::prepare`, `execute`, `run_chained`, `Jit::current`, `Jit::link`) | 24.9 % | 4.3 % | 5.6 % | 0 % |
| Block lookup and the checks of the loop, with the libc calls made from it | 12.6 % | 1.8 % | 0.7 % | 0 % |
| Translation and compile (lift, lowering, emit, assembler), with the libc calls made from them | 10.5 % | 3.2 % | 0.2 % | 0 % |
| libc with no caller in the binary (by the offsets: `malloc`, `free`, `realloc`) | 8.7 % | 2.6 % | 0.2 % | 0 % |
| Memory: RAM behind a hit, the softmmu paths, page walks | 7.9 % | 3.9 % | 44.7 % | 0 % |
| The thunk and the pure calls | 3.8 % | 11.9 % | 20.1 % | 0 % |
| Helpers: FP, AdvSIMD, crypto | 3.2 % | 57.6 % | 0 % | 0 % |
| Register copies, the operation loop of the interpreter, system, devices, other | 9.4 % | 10.3 % | 3.1 % | 0.1 % |

- **J9a1. The boot is not in compiled code.** 18.9 % of the vCPU's time is
  in compiled blocks. The round trip to the Rust loop (the two rows after
  them) is 37.5 %. `Seat::prepare`, which stores the context and the budget
  before every entry, is 10.1 % alone. The counters of the boot row (17.0e9
  instructions): 2,878.6 million block runs (5.9 instructions a run), of
  which 2,362.7 million came through a linked jump (82 %), 318.9 million
  through the jump cache (11 %) and 197.0 million from the Rust loop
  (6.8 %). Of these 197.0 million, 90.5 million are misses of the jump
  cache and 22.5 million made a new link. The jump cache was emptied 1.10
  million times and 22.4 million links were unlinked. *Est.*: 37.5 % of the
  64.4 s of the boot row over 197.0 million round trips is about 120 ns a
  round trip. The basis of an explanation for `Seat::prepare`, not
  measured: its store of the budget is sequentially consistent (an
  `xchg`), on a cache line that the threads which raise the interrupt
  lines also write.
- **J9a2. Translation is a tenth of the boot.** 947,061 compiles in the
  boot (453 MB of code, 5.9 s in the compiler by its own counter, 6 drops
  of the 64 MiB region), as in J6d. With the heap calls that the call
  chains give to it, translation and compile are 10.5 %. libc with no
  caller in the binary is another 8.7 %: `malloc` and `free` without frame
  pointers hide their caller, so how much of it is translation is not
  known.
- **J9a3. `sha256` runs in the helpers.** The guest's `sha256sum` uses the
  SHA-256 instructions, which vmlab-hv's ID registers offer.
  `helpers::crypto` and the generic AdvSIMD evaluator (`simd::run`,
  `eval`, `same`, `lane`) are 57.6 %, and the pure calls that reach them
  another 11.9 %.
- **J9a4. `xz` runs in the miss path of the fast TLB.** `-9` searches a
  dictionary larger than the 1 MiB that 256 entries of 4 KiB reach
  (`tlb::ENTRIES`). The miss path (the thunk, `load`, `FastTlb::fill`,
  `fill_fast`, the lookup of the architectural TLB: the softmmu class and
  the thunk) is 62.4 %, page walks 0.3 %: the architectural TLB hits and
  the fast TLB does not. The counters:
  the boot alone missed 3.0 % of 4.18e9 inlined accesses, and a run of
  the full suite (26.0e9 instructions) 10.2 % of 6.83e9. The part after
  agent ready (the preparation, `sha256`, `xz` and the check) is then
  about 21.6 % (571.6 million of 2.64e9), from the difference of the two
  runs.
- **J9a5. CoreMark is all compiled code**: 99.9 % of the samples, none in
  the Rust loop, the thunk or translation. 1.68e9 block runs for 4.30e9
  instructions, 2.56 instructions a run, and its hottest blocks have 2 to
  5 instructions. What is left there is the code of each block: the budget
  check at its head and the registers that it reads and writes at its
  ends (H3, H10).
- **J9a6. Small for every workload:** exceptions, system registers, the
  timer and the interrupts (0.9 % of the boot), page walks (1.2 %), FP
  helpers (none visible: the helper samples are AdvSIMD and crypto), the
  exclusives (`load_reserved` 0.50 % and `compare_and_store` 0.42 % of the
  first boot profile at one vCPU, whole run).

**J9b. The rows against QEMU.** The flat images: cores 28 to 31, the median
of 3 with the range. The D2 suite on Debian 13: vmlab-hv on cores 20 to 23,
QEMU on cores 24 to 27, the median of 3 with the range. QEMU at one vCPU
ran beside vmlab-hv at one vCPU, QEMU at four vCPUs after the vmlab-hv
runs. The boot row of the boot suite ran alone after the full suite. "Of
QEMU" is QEMU's time over ours, or our MIPS over QEMU's.

| Workload | vCPUs | vmlab-hv, JIT | QEMU 11.1.3 TCG | Of QEMU |
|---|---|---|---|---|
| CoreMark mix, flat, MIPS | 1 | 2,024.8 (2,023.3 to 2,038.7) | 2,565.6 (2,522.0 to 2,591.5) | 79 % |
| Scalar loop, flat, MIPS | 1 | 2,040.4 (2,026.9 to 2,046.9) | 3,130.4 (3,118.9 to 3,176.9) | 65 % |
| Boot to agent ready, boot suite, s | 1 | 64.4 (64.2 to 73.0), 279.2 MIPS | 26.9 (26.7 to 28.4), full suite | 42 % |
| Boot to agent ready, full suite, s | 1 | 80.2 (70.9 to 81.6), 225.1 MIPS | 26.9 (26.7 to 28.4) | 34 % |
| `sha256_256M`, s | 1 | 9.76 (9.61 to 9.92) | 2.13 (2.05 to 2.16) | 22 % |
| `xz_9_32M`, s | 1 | 8.86 (8.61 to 9.02) | 2.78 (2.74 to 2.79) | 31 % |
| Boot to agent ready, full suite, s | 4 | 56.3 (55.0 to 61.7), 345.9 MIPS over the four vCPUs | 20.1 (20.1 to 20.7) | 36 % |
| `sha256_256M`, s | 4 | 7.43 (6.88 to 7.68) | 2.02 (2.02 to 2.04) | 27 % |
| `xz_9_32M`, s | 4 | 8.43 (8.39 to 9.16) | 2.72 (2.71 to 2.75) | 32 % |

- **J9b1. The interpreter share (H2b)** is zero for a missing form in every
  row: no block refused. At the budget (H2c): 53 blocks and 257
  instructions in the boot row at one vCPU, 1,398 blocks and 7,891
  instructions in the run at four vCPUs at its last report. No instruction
  of the flat images ran in the interpreter.
- **J9b2. The two boot rows at one vCPU differ by 16 s.** The full suite ran
  beside QEMU's runs and the boot suite alone. Both are in the history.
- **J9b3. Four vCPUs** make our boot 1.42 times faster than one vCPU (the
  full suite rows) and QEMU's 1.34 times. `sha256` and `xz` are one thread
  each, and run 1.3 and 1.05 times faster on our side with four vCPUs. The
  basis of an explanation, not measured: at one vCPU the same vCPU also
  serves the interrupts and the reads of the page cache. The waits of J7c
  and J7d took 1.6 s of CPU time in the four vCPU run (the counters at its
  last report).
- **J9b4. D1 is met by no row.** The nearest are CoreMark (79 %) and the
  boot at one vCPU (42 %).

**J9c. What comes next, from J9a.** The order is the expected gain on the
D2 suite. Each estimate is *est.*, with its basis.

| Code | Step | Measured share it attacks | Gain, est. | Decision |
|---|---|---|---|---|
| N1 | Fewer and cheaper round trips to the Rust loop | 37.5 % of a boot (J9a1) | A boot 1.3 to 1.5 times. Basis: half the round trips (the jump cache kept across a write of TTBR0 and TTBR1 when the ASID tells the spaces apart, and a miss served from the stub without the loop), each at half its cost. | Card J10 |
| N2 | Cheaper translation and compile | 10.5 % of a boot, and up to 8.7 % more in libc (J9a2) | A boot 1.1 to 1.2 times. Basis: no heap call per block in the lifter and the emitter, and fewer recompiles after a drop of the region. | Card J11 |
| N3 | The reach of the fast TLB | 62.4 % of `xz` (J9a4) | `xz` 1.4 to 1.8 times. A boot: little, its miss rate is 3 %. | Card J12 |
| N4 | Host-FPU fast paths and inlined AdvSIMD, with the crypto helpers | 69.5 % of `sha256` (J9a3), 3.2 % of a boot | `sha256` 2 to 3 times. Basis: the SHA-256 operations as direct helpers of their own, without the generic lane evaluator and the marshalling of the pure call. FP: no row of D2 has FP work, so the FP half of D11 waits for a workload that has it. | Card J13 (AdvSIMD and crypto only) |
| N5 | Superblocks: several blocks as one unit, with registers kept across the joins (H1) | 99.9 % of CoreMark in compiled code, 2.56 instructions a block run (J9a5) | CoreMark 1.2 to 1.4 times, which is 2,400 to 2,800 MIPS against QEMU's 2,566. A boot: at most its 18.9 % in compiled code. | Card J14 |
| N6 | A return stack for `RET` | The jump cache serves 11 % of the block runs of a boot, inline | At most 1 to 2 % of a boot. Basis: a hit is an index, two compares and a jump (H6). | Not now. N1 measures it again. |
| N7 | LSE (3d:G2) with host atomics | The exclusives: about 1 % of a boot at one vCPU (J9a6), 0.14 to 0.42 % of the instructions (J7g) | Small at one vCPU. At four vCPUs the spins of J7g. | Not now. A profile at four vCPUs after N1 decides it. |
| N8 | Cheaper exception entry | 0.9 % of a boot (J9a6) | Under 1 %. | Not a card. |
| N9 | A Cranelift tier for hot regions (Q1) | Compiled code: 18.9 % of a boot, 4.4 % of `sha256`, 25.4 % of `xz` | A boot at most 1.1 times even if that code were twice as fast. CoreMark: B1 measured 8 to 53 % on a block alone. | Not now. After J14, if CoreMark is still under QEMU. |

*Est.* after J10 to J14 together: a Debian boot at one vCPU of 36 to 45 s
(60 to 75 % of QEMU), CoreMark near QEMU, `sha256` 44 to 65 % and `xz` 44
to 57 % of QEMU. Basis: the gains of the table multiplied, each on the
measured rows of J9b. So R1 holds: the next cards do not reach D1 on the
boot or on the D2 workloads, and they do not show what would.

### N1, measured by card J10

What card J10 built:

- **J10a. The jump cache keeps its entries across a new address space.**
  The key of an entry holds the ASID beside the key of the context and the
  generation (`Jit::set_jump_space`, bits 32 to 47 of the key). A write of
  TTBR0 or TTBR1 that brings a new ASID and changes nothing else now keeps
  the cache, as the TLB keeps its tagged entries (`Mmu::jump_changes`). A
  new base under the same ASID, a TLBI and a change of a control register
  still empty it. A kernel without hardware PAN writes TTBR0 and the ASID
  of TTBR1 at every entry and every exit, so this is the change that most
  misses came from.
- **J10b. A miss of the jump cache is served without leaving compiled
  code.** The cold stub of a miss calls the thunk with `jit::JUMP`. The
  thunk returns the code of the block at the PC when the loop would run that
  code next without a step of its own: the TLB holds the page of the PC for
  a fetch, the block is in its entry of the cache for the same context, it
  was seen in this epoch, its code is current, K3 has no check of it left,
  and since the run started no store went into translated code and the MMU
  did not change. It fills the entry as the loop does, and the stub jumps
  to the code. Anything else returns 0, and the block leaves with `LOOKUP`
  as before. The interrupt lines, the mail and the timer are not read
  there, as a hit does not read them: a kick ends the run at the next block
  head (H10c). `VMLAB_HV_JIT_SERVE=0` turns it off.
- **J10c. `Seat::prepare` stores the budget with release order**, a `mov`,
  where it stored it sequentially consistent, an `xchg`. J7a allows it: a
  kick that lands before the store is lost in either order, and the owner
  reads its mail and its lines before `prepare` and nothing that a kicker
  writes after it. The seat is aligned to 128 bytes, on cache lines of its
  own.
- **J10d. The small functions of the round trip are inlined across
  crates** (`Jit::current`, `Jit::context`, `Jit::enter`, `Region::contains`
  and the reads of the seat). The release profile has no LTO, so they were
  calls.
- **J10e. Counts.** The JIT line of the debug log ends with the returns to
  the loop by reason (the misses served in code, a block that the loop
  ends itself, a block with nothing to chain to, the budget, a kick) and
  the changes of the address space that kept the cache. `bench/lib.py`
  reads them.

Proof, helios, 2026-11-28, *measured*. Before is `next` (`2f8a7dc4`),
after is `648891e6`. Other cards ran builds and boots at the same time
(load average 8 to 14 of 32 threads). The rows are in
`bench/history/aarch64.jsonl` with these two commits. The boot rows of
both sides are marked dirty: the flat rows of the same checkout were
written first.

**J10f. The returns to the loop by reason**, Debian 13 to agent ready at
one vCPU, the counts of the last JIT line of the boot (18.0e9 instructions
on both sides):

| Reason | Before | After |
|---|---|---|
| Returns to the loop (runs of compiled code entered) | 223.3 million | 131.2 million |
| A miss of the jump cache | 102.2 million | 12.0 million |
| A miss of the jump cache served in code (J10b) | none | 30.9 million |
| A first link of a jump (H5) | 27.7 million | 26.1 million |
| A block that the loop ends itself (H5c: `MSR`, `ERET`, `SVC`, a system instruction) | 93.4 million, with the two rows below (not counted apart before) | 88.3 million |
| A block with nothing to chain to | | 1.9 million |
| The budget at a block head, spent or kicked | | 2.9 million, of which 1,205 kicked |
| An exception or a store into translated code | 83,000 | 83,000 |
| Emptyings of the jump cache | 1.24 million | 0.30 million, and 0.93 million changes of the address space kept it |
| Hits of the jump cache inline | 341.8 million | 399.6 million |

The misses of the jump cache, served or not, fell from 102.2 million to
42.9 million (J10a), and the thunk served 72 % of those that are left
(J10b). The round trips fell by 41 %. At four vCPUs the last line of a
boot is that of one vCPU. Two secondary vCPUs at 3.0e9 instructions each:
62.9 million returns before (21.8 million misses), 51.0 million after
(8.5 million misses and 4.5 million served).

**J10g. The cost of one round trip**, the flat images with chaining off
(`VMLAB_HV_JIT_CHAIN=0`), so that every block returns to the loop: the
time less that of the same image with chaining on, over the runs entered.
Cores 28 to 31, the median of 5, the three builds in turn. "After with an
`xchg`" is the after build with the store of `prepare` sequentially
consistent again, to measure that store alone.

| Image | Before | After | After with an `xchg` |
|---|---|---|---|
| Scalar loop, 100.0 million round trips | 34.8 ns | 26.2 ns | 29.5 ns |
| CoreMark mix, 56.0 million round trips | 35.3 ns | 26.8 ns | 30.4 ns |

A round trip costs a quarter less. The store of `prepare` is 3.3 to 3.6
ns of it on one thread, the inlining and the alignment the rest. A boot
pays more for a round trip than this: J9a1 estimated 120 ns from the
profile. How much of `Seat::prepare` in that profile was the line shared
with other threads is not measured.

**J10h. The rows.** The boots: Debian 13 to agent ready, the boot suite.
The two sides ran one after the other repeat by repeat on cores 20 to 23
(one VM at a time), the median of 3 with the range. A first set ran the
three repeats of each side in a block, before then after, while the load
of the host fell.

| Workload | vCPUs | Before | After |
|---|---|---|---|
| Boot to agent ready, s, in turn | 1 | 102.4 (86.8 to 107.1), 176.2 MIPS | 84.2 (82.4 to 88.3), 216.1 MIPS |
| Boot to agent ready, s, in turn | 4 | 105.4 (83.1 to 118.9), 194.6 MIPS | 78.0 (61.0 to 90.0), 259.5 MIPS |
| Boot to agent ready, s, in a block | 1 | 86.4 (85.0 to 104.2), 209.6 MIPS | 64.2 (60.3 to 64.3), 281.9 MIPS |
| Boot to agent ready, s, in a block | 4 | 58.8 (54.3 to 60.9), 339.4 MIPS | 50.8 (50.4 to 64.6), 377.1 MIPS |
| Scalar loop, flat, MIPS, in turn, median of 5 | 1 | 1,970.5 (1,921.4 to 1,995.9) | 1,992.4 (1,916.0 to 2,002.5) |
| CoreMark mix, flat, MIPS, in turn, median of 5 | 1 | 1,978.7 (1,955.8 to 2,014.9) | 1,937.8 (1,718.2 to 1,981.4) |
| Scalar loop, flat, MIPS, bench rows | 1 | 1,889.7 | 1,834.3 |
| CoreMark mix, flat, MIPS, bench rows | 1 | 1,871.0 | 1,810.7 |

- **J10i. N1 holds in part.** The boot at one vCPU is 1.22 times faster in
  turn and 1.35 times in a block, against the 1.3 to 1.5 of the estimate.
  At four vCPUs 1.35 and 1.16 times. The round trips fell by 41 %, not by
  half, and each costs 25 % less on a flat image, not half. The flat rows
  move by less than the spread of the host: they make almost no round
  trips (the scalar loop enters compiled code 5 times a run).
- **J10j. The interpreter share (H2b)** is zero for a missing form in
  every row: no block refused.
- **J10k. K3** on a Debian 13 boot at two vCPUs with the after build,
  `VMLAB_HV_JIT_CHECK=1` and `VMLAB_HV_JIT_CHECK_RUNS=1000`: agent ready
  after 93.8 s, no difference. 160.8 million checked runs (778.9 million
  instructions) over both vCPUs at their last lines, with 11.7 million
  misses served in code between the checked runs.
- **J10l. K2, K6 and the stress tests of J7** run in `just ci::check`: the
  block differential, the stores into translated code with the JIT on, the
  chained pair and the store from a second CPU, and
  `a_cpu_in_chained_code_runs_the_code_that_another_cpu_stored` at 20,000
  rounds. The stress test also ran alone at 2,000,000 rounds
  (`VMLAB_HV_J7_ROUNDS=2000000`), beside the exclusives stress test of
  card C3: both passed, no stale execution.
- **J10m. What is left.** 67 % of the returns that are left are blocks
  that the loop ends itself (H5c), one every 204 instructions of a boot.
  The `MSR` of a table base and of the ASID at every entry to and exit
  from the kernel is among them. Serving those in code is a card of its
  own: each changes the state that the loop reads before the next block.

### J11, measured by card J11

Helios, 2026-11-28, *measured*. Before: `f0d5a25e`, which is `next`
(`2f8a7dc4`) with only the counters of this card. After: `33c67ec8`. The
merge bar ran on `85f8afcb`, which differs from it only in rustfmt and
clippy changes. Each side was built from a tree exported from its commit.
The boots ran on cores 20 to 23 and the flat images on cores 28 to 31.
Other cards ran builds and boots at the same time: the load average was
4.6 to 15.2 of 32 threads at the starts and the ends of the runs. The merge
bar of this card ran during the first after boots and the second before
boots. The last pair of boots ran side by side, before on cores 20 to 23
and after on cores 24 to 27, at a load average of 4.6 to 12.9.
The rows are in `bench/history/aarch64.jsonl` with the note "card J11".
No QEMU source was read (X1 to X3). No other source was read.

**J11a. The compiles of a boot by cause.** The new counters of the debug
log (`compiles: ... first, ... after a drop of the region, ... after an
invalidation, ... after an eviction`) are read at the last counter line
before agent ready. Each repeat of Debian 13 at one vCPU:

| At the line of | Before | After |
|---|---|---|
| 18.0e9 instructions (before: 6 boots, after: 4) | 1.088 to 1.100 million compiles: 457,165 to 472,073 the first time, 608,000 to 616,680 after a drop of the region, 13,870 to 14,015 after an invalidation or an epoch, none after an eviction. 7 drops of the region of 64 MiB | 0.923 to 0.966 million: 457,193 to 475,070 the first time, 443,863 to 477,752 after a drop of a part, 13,931 to 14,139 after an invalidation, none after an eviction. 23 or 24 drops of a part of 16 MiB |
| The same, compile time | 7.99 to 8.52 s (7.3 to 7.8 µs a compile) | 1.53 to 1.60 s (1.58 to 1.73 µs) |
| The same, translation time | 2.50 to 2.67 s for 471,035 to 486,088 blocks (5.1 to 5.5 µs a block) | 1.86 to 2.09 s for 471,332 to 489,031 blocks (3.8 to 4.4 µs) |
| 17.0e9 instructions, the side-by-side pair (before: 2 boots, after: 2) | 947,480 and 951,378 compiles, 501,467 and 503,317 after a drop; 5.73 and 5.76 s compiling (6.0 µs); 2.04 s translating (4.6 µs a block) | 802,212 and 793,286 compiles, 353,329 and 345,398 after a drop; 1.26 and 1.23 s compiling (1.6 µs); 1.59 s translating (3.5 µs) |

At four vCPUs (three boots each side, the four vCPUs summed at their last
lines): before 9.89 to 14.22 s compiling, 7.7 to 9.5 µs a compile; after
2.08 to 2.23 s, 1.7 to 1.9 µs.

- **J11a1. Most compiles were recompiles.** Before, 56 % of the compiles
  of a boot came after a drop of the region: 7 drops, each followed by
  about 87,000 compiles of blocks that the boot had compiled before.
  Blocks dropped from the block store and compiled again: none.
- **J11a2. The region drops a quarter now (H13c).** The recompiles after
  a drop fall by 21 to 28 % at 18.0e9 instructions, to 48 to 50 % of the
  compiles. The basis of an explanation, not measured: the code that a
  boot runs again is larger than three quarters of the region, so a part
  that is dropped still holds code that runs again.
- **J11a3. The size of the region stays 64 MiB.** After the change, a
  recompile costs 1.6 µs, so the 0.35 to 0.48 million recompiles after a
  drop cost 0.6 to 0.8 s of a boot of 58 to 86 s. Code that the boot
  compiles the first time is 0.22 GB at one vCPU (457,000 to 475,000
  blocks of 479 bytes), so a region that kept it all is 256 MiB, and the
  memfd keeps each page that it wrote: up to 256 MiB of host memory for
  each vCPU. *Est.*: that region would save at most the 0.6 to 0.8 s, 1 to
  1.4 % of a boot.

**J11b. The heap calls of a block** (`tests/alloc.rs` of
`vmlab-hv-tcg-aarch64`: 603 blocks translated, and compiled with the JIT,
on a CPU whose buffers grew on 603 blocks before; the heap calls of the
thread counted):

| | Before | After |
|---|---|---|
| A compile (the shape of the block, the emitter, the assembler, the code) | 117.6 a block | 0 |
| A translation (the lifter, the pruning, the lowering, the block) | 71.2 a block | 13.5 a block |

- **J11b1. What stays** is the lists that the pre-decoded block keeps (its
  operations, its frame, its leaves with their operations and outputs, its
  calls) and its box, each allocated once at its own size. The lifter
  fills the block of the translation before, the exits take their lists
  from those of the block before, the lowering, the pruning and the lists
  of `compile` are kept by the CPU (`Translator`), and the emitter and the
  assembler by the JIT (`emit::Scratch`). The forms of a lazy condition are
  a table made once (`flags::host_forms_of`).
- **J11b2. The test fails** when a compile makes a heap call, or when a
  translation of that program makes 16 or more a block.

**J11c. The rows.**

| Workload | Before | After |
|---|---|---|
| Debian 13 boot to agent ready, 1 vCPU, s, the side-by-side pair (median of 3) | 68.66 (62.53 to 80.27), 261.7 MIPS | 58.38 (57.25 to 64.23), 305.2 MIPS |
| The same, the runs one after the other | 88.37 (72.39 to 96.40), and 94.80 (90.56 to 119.10) beside the merge bar | 85.51 (76.24 to 86.47), beside the merge bar |
| Debian 13 boot to agent ready, 4 vCPUs, s | 60.40 (60.28 to 66.40), and 90.39 (88.55 to 133.10) beside the merge bar | 60.37 (58.83 to 70.17) |
| CoreMark mix, flat, MIPS (median of 3, two runs a side) | 1,839.7, 1,951.4 | 2,023.1, 1,989.0 |
| Scalar loop, flat, MIPS | 1,577.6 (1,362.0 to 1,748.0), 1,954.5 | 2,008.9, 1,982.6 |
| Instructions in the interpreter at the last counter line of each boot (H2b, H2c) | none for a missing form; at the budget 102 to 328 at 1 vCPU, 5,300 to 7,404 at 4 | none for a missing form; at the budget 136 to 225 at 1 vCPU, 5,650 to 6,770 at 4 |
| K3 on Debian 13 at 1 vCPU (`VMLAB_HV_JIT_CHECK=1`, `VMLAB_HV_JIT_CHECK_RUNS=1000`) | | Agent ready after 132.3 s, no difference, 121.2 million checked runs (581.7 million instructions) up to the counter line at 18.0e9 instructions |

- **J11c1. The boot at one vCPU is 1.18 times faster** in the pair that ran
  side by side, inside the estimate of N2 (1.1 to 1.2 times). The runs one
  after the other differ by less than the spread of the before side (24.0
  and 28.5 s), with the load of the host changing between them. The basis of an explanation: the
  pair saves 4.5 s of compiling and 0.5 s of translating of the 17.0e9
  instructions, and the 0.15 million fewer compiles also make fewer links
  and fewer misses of the jump cache.
- **J11c2. The flat images** compile a few blocks, and they run the same
  within their noise, as they should.
- **J11c3. K2** (`tests/jit.rs` of `vmlab-hv-tcg-aarch64`, the random blocks
  and the flag operands against the interpreter) passes on the after side.
  The JIT tests add one for the drop of a part: a jump into the dropped
  part goes back to its stub, and a jump between two parts that stay stays
  linked.
- **J11c4. Not done:** a profile of the boot as in J9a, so the share of
  translation and compile in the samples after the change, and how much of
  the 8.7 % of libc of J9a2 went with it, are not measured.

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
| J10 | Fewer and cheaper round trips to the Rust loop (N1) | Count the returns to the Rust loop by reason (a jump cache miss, a first link, an exit that H5c does not link, the budget, an exception) and measure the cost of one round trip and of `Seat::prepare`. Then: the jump cache kept across a write of TTBR0 and TTBR1 where the ASID tells the address spaces apart (H6 clears it today), a miss of the jump cache served by a call from the stub that returns the host address without leaving compiled code, and `Seat::prepare` without a sequentially consistent store where the kick protocol of J7a allows it. | The returns per boot by reason, before and after. K2, K3 on a Debian boot, K6, the stress tests of J7. The rows. | J9 | J12, J13 |
| J11 | Cheaper translation and compile (N2) | Count the compiles of a boot by cause (the first time, after a drop of the region, after an invalidation or an epoch). No heap allocation per block in the lifter, the lowering and the emitter (buffers kept by the CPU). The size of the region from that count, or a drop of only the older part (H13c). | The compiles and the compile time per boot, before and after. K2, K3 on a Debian boot. The rows. | J9 | J12, J13 |
| J12 | The reach of the fast TLB (N3) | `tlb::ENTRIES` from a measured curve of the miss rate of `xz` and of a boot against the size, with the cost of a flush (H7b) at each size. | The miss rates and the `xz` rows at 1 and 4 vCPUs, before and after. The unit tests of H7a and H7b at the new size. K2. | J9 | J10, J11, J13 |
| J13 | AdvSIMD and crypto without the generic evaluator (N4) | The SHA-256 operations (`SHA256H`, `SHA256H2`, `SHA256SU0`, `SHA256SU1`) and the AdvSIMD forms that the profile of a boot and of `sha256` names, as direct helpers or inline forms, called without the marshalling of the pure call. | K1 for the AdvSIMD and crypto groups with the JIT on. K2, K3 on a Debian boot. The `sha256` rows at 1 and 4 vCPUs. | J9 | J10, J11, J12 |
| J14 | Superblocks (N5) | Several blocks compiled as one unit with registers kept across the joins, for the targets that a block reaches through a linked jump inside its page, with the exits and the precise state of H9 at each join. | CoreMark and the loop against QEMU. K2 with blocks that join. K1 for the branch groups. K3 on a Debian boot. The rows. | J10 | none |

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

Proposed by card J9, open (Q15 of the MAP):

- **Q4. How D1 is treated.** Measured (J9b): CoreMark at 79 % of QEMU's
  speed, the Debian boot at 34 to 42 % at one vCPU and 36 % at four,
  `sha256` at 22 and 27 %, `xz` at 31 and 32 %. *Est.* after J10 to J14
  (J9c): the boot at 60 to 75 %, CoreMark near 100 %, `sha256` and `xz` at
  44 to 65 %. No card in view reaches D1 on the boot or on the D2
  workloads.
  - **O1 (recommended).** D1 stays the tracked target: `bench-report`
    gives each row as a percentage of QEMU, at 1 and 4 vCPUs, and the
    design records it at every card. It is not a gate of 3e. 3e closes at
    a measured bar T3 after J10 to J14: CoreMark at least 100 % of QEMU,
    and every row of the D2 suite (the boot, `sha256`, `xz`) at least 50 %
    of QEMU at 1 and 4 vCPUs. The estimates of J9c put the boot above
    that bar, and CoreMark, `sha256` and `xz` about at it. D1 is decided
    again with the rows of T3.
  - **O2.** D1 as written: phase 3 is not finished until every row is at
    least 100 %. Cards after J14 (N6 to N9 and others) without an
    estimate that they reach it.
  - **O3.** D1 for CPU work only (CoreMark, `sha256`, `xz`), and a boot
    bar of its own (for example at most twice QEMU's time).

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

J9, on a checkout of the commit with the firmware of
`guest/fetch-firmware.sh aarch64` in `VMLAB_HV_FIRMWARE_DIR_AARCH64`:

```sh
# J9b: the D2 suite and the boot row, both sides, and the flat rows
python3 bench/run.py --backend vmlab-hv --arch aarch64 --vcpus 1,4 --repeats 3 --guest debian-13 --suite full --cpus 20-23
python3 bench/run.py --backend qemu --arch aarch64 --vcpus 1,4 --repeats 3 --guest debian-13 --suite full --cpus 24-27
python3 bench/run.py --backend vmlab-hv --arch aarch64 --vcpus 1 --repeats 3 --guest debian-13 --suite boot --cpus 20-23
python3 bench/run.py --backend jit --arch aarch64 --vcpus 1 --repeats 3 --cpus 28-31
python3 bench/run.py --backend qemu --arch aarch64 --vcpus 1 --repeats 3 --guest flat-a64 --cpus 28-31

# J9a: the profiling build of W1, the sampler of this directory, the perf map
cc -O2 -o ipraw measure/ipraw.c
VMLAB_HV_JIT_PERF_MAP=1 python3 bench/run.py --backend vmlab-hv --arch aarch64 --vcpus 1 --repeats 1 \
  --guest debian-13 --suite full --keep --cpus 16-19 --hv-bin <binary> &
IPCOMM=vcpu- IPRAW=raw.txt ipraw <pid of vmlab-hv> 3600     # the whole run, until the VM ends
cp /tmp/perf-<pid>.map perf.map
python3 measure/ipclass.py <binary> raw.txt@perf.map --top 40 --window <from> <to>   # unix seconds of the bench log
# CoreMark: a long image (micro_a64.build_coremark(30000)) under a64_bench with
# VMLAB_HV_JIT=1 VMLAB_HV_JIT_PERF_MAP=1, sampled the same way, one map per run

# J11: the heap calls of a translation and of a compile, then the compiles by
# cause, read from the last JIT line of each kept run (stderr.log of bench/run)
cargo test --release -p vmlab-hv-tcg-aarch64 --test alloc -- --nocapture
python3 bench/run.py --backend vmlab-hv --arch aarch64 --vcpus 1,4 --repeats 3 --guest debian-13 --suite boot --keep --cpus 20-23
grep -o "compiles: .*" bench/run/*/stderr.log
# K3 on a boot
VMLAB_HV_JIT_CHECK=1 VMLAB_HV_JIT_CHECK_RUNS=1000 python3 bench/run.py --backend vmlab-hv --arch aarch64 \
  --vcpus 1 --repeats 1 --guest debian-13 --suite boot --keep --cpus 20-23 --timeout-scale 4
```

J10, the same firmware, one checkout and one release build of each side
(`CARGO_TARGET_DIR` of its own):

```sh
# J10h: the boot rows, the two sides in turn, one VM at a time (each side in its checkout)
python3 bench/run.py --backend vmlab-hv --arch aarch64 --vcpus 1 --repeats 1 --guest debian-13 \
  --suite boot --keep --hv-bin <side>/release/vmlab-hv --cpus 20-23    # before, after, before, ...
# J10f: the returns by reason are in the JIT lines of the kept stderr.log, and in env.jit_reports
# of each row
# J10g: the cost of a round trip, each image with chaining on and off, the builds in turn
VMLAB_HV_JIT=1 VMLAB_HV_JIT_CHAIN=0 taskset -c 28-31 <side>/release/examples/a64_bench <image> 4000000000
# J10k: K3 on a Debian boot at two vCPUs
VMLAB_HV_JIT_CHECK=1 VMLAB_HV_JIT_CHECK_RUNS=1000 python3 bench/run.py --backend vmlab-hv \
  --arch aarch64 --vcpus 2 --repeats 1 --guest debian-13 --suite boot --timeout-scale 3 --cpus 24-27
```

The boot MIPS of E2 are the differences of the instruction counter that the
vCPU writes to the debug log (`set_log_filter` with
`vmlab_hv_vmm::vcpu::emulated=debug`), as card #1048 read them. Every VM ran
under a memory cap, and the bench refuses a third VM on the host.
