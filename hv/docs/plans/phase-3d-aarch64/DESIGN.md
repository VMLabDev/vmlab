---
slug: phase-3d-aarch64
kind: design
created: 2026-11-26
card: 1032
---

# Phase 3d of vmlab-hv: the aarch64 emulator

This document is the design of sub-phase 3d. It replaces the Sail route for
aarch64. `Dn` without a prefix is a row of `../phase-3-tcg/MAP.md`. `HV-Dn` is
a row of `../vmlab-hypervisor/MAP.md`. The codes of this document (`P`, `S`,
`M`, `G`, `R`, `O`, `U`, `V`, `A`, `F`, `X`, `Q`, `C`) are stable.

Every number marked *measured* comes from helios (Ryzen 9 9950X, QEMU 11.1.3,
LLVM 23.1.4) on 2026-11-26. The tools are in `measure/`. "How to measure
again" is at the end. A number marked *est.* is an estimate, with its basis.

## The decisions of the operator (2026-11-26)

- **P1.** No sail-arm, and nothing derived from Arm's ASL or XML. Decode comes
  from the LLVM AArch64 TableGen tables. The semantics are our own code in the
  typed IR of `vmlab-hv-tcg-ir`.
- **P2.** QEMU is the reference for behaviour. A `qemu-system-aarch64` process
  is the lockstep oracle. It is a test tool only. No product path starts it.
- **P3.** An Apple M1 machine comes after 3d. The CPU core stays apart from
  the `virt` machine, and nothing in 3d may block the M1.
- **P4.** 3d is accepted when the four aarch64 templates boot to agent ready
  on vmlab-hv, snapshots work, and e2e `arch.emulated` runs on vmlab-hv. Then
  vmlab selects vmlab-hv for aarch64 and the QEMU code is removed.

P1 replaces HV-D34 for aarch64 and D14. P2 replaces the per-instruction half
of D6 for aarch64. The "owner licence check before 3d" of D12 is gone.

One assumption of P4 is wrong as written. The QEMU adapter, the QMP client and
the QEMU process code also run every riscv64 VM, and riscv64 on vmlab-hv is
sub-phase 3b, which has not started. After 3d, vmlab can select vmlab-hv for
aarch64, but the QEMU code can be removed only when 3b is also done, or when
the operator drops riscv64 guests. This is question Q3.

## Sources: what an agent may read to write the semantics

QEMU's `target/arm` is GPL-2.0 and vmlab is MIT. The rule for every 3d card:

| Code | Text | Licence | Use |
|---|---|---|---|
| S1 | LLVM `llvm/lib/Target/AArch64/*.td`, and the files beside them (`MCTargetDesc/AArch64AddressingModes.h`, `Disassembler/`) | Apache-2.0 WITH LLVM-exception | Encodings, fields, operand classes, the names, encodings and access of system registers, feature predicates. Code that we port by hand carries the notice. |
| S2 | gem5 `src/arch/arm` (the `.isa` files, `miscregs`, `faults`, `table_walker`, `pauth_helpers`) | BSD-3-Clause, copyright of Arm Limited and others | The readable reference for instruction and system semantics: exception levels, system registers, translation, faults. The gem5 licence has no patent grant (checked in its `LICENSE`). |
| S3 | dynarmic `src/dynarmic/frontend/A64` | 0BSD | A second readable reference for user-mode A64, FP and AdvSIMD. It emulates no privilege level above user mode. |
| S4 | VIXL's AArch64 simulator | BSD-3-Clause (the card that first reads it checks the `LICENCE` file) | A third reference for user-mode instructions. Its README says that exclusive accesses and some FP rounding modes are not exact. |
| S5 | Berkeley SoftFloat 3e and TestFloat | BSD-3-Clause | Floating point. SoftFloat is already vendored. |
| S6 | The behaviour of a running QEMU: its registers, memory, exceptions, device answers and logs, through its gdbstub, its plugin API (`qemu-plugin.h`, a header for third-party plugins) and its documentation | not a source text | The oracle (P2). |
| S7 | Published prose specifications of devices and firmware interfaces: PSCI, SMCCC, ACPI, the GICv2 architecture specification, the PL011 manual, VIRTIO | documents | Device models, as `hv/AGENTS.md` already says for the Intel manuals. |
| S8 | The Asahi Linux hardware documentation, m1n1 up to 1.9.9 (MIT) | documentation, MIT | Apple registers and instructions, for the later M1 phase only. m1n1 from 2.0.0 is GPL-2.0 (its README says so). It is run as a guest, not read. |

Not allowed for the CPU core:

- **X1.** QEMU source under `target/arm`, `fpu`, `tcg` and `accel/tcg`. Not
  read, not copied, not translated. This is stricter than the device rule of
  `hv/AGENTS.md`, which stays as it is for device models.
- **X2.** sail-arm, Arm's machine-readable specification (ASL, XML), and the
  pseudocode of the Arm Architecture Reference Manual, which is the same ASL.
- **X3.** Unicorn (GPL, derived from QEMU).

Each semantics file names in its header the sources that its author read. Each
card report lists them. A behaviour that S2 to S4 do not settle is settled by
S6: write the probe, run it on QEMU, and keep the probe as a test.

**Can the semantics be written without the Arm manual and without QEMU's
source? Yes.** gem5 runs full-system Arm Linux with exception levels, system
registers, table walks and fault syndromes under BSD-3, so every area has a
permissive readable text. dynarmic and VIXL give two more texts for the
instruction groups where most of the code is (FP and AdvSIMD). The lockstep
run finds what a text left wrong, at the first instruction that differs. The
cost of the rule is time, not feasibility: a rare system behaviour that gem5
does not model must be found by a probe on QEMU. No question to the operator
blocks the work. Q1 asks only whether the prose chapters of the Arm manual
(not its pseudocode) may be opened later if a card shows that it needs them.

## 1. Decode

### M1. The tables are usable as they are (measured)

`llvm-tblgen --dump-json` on `AArch64.td` of the tag `llvmorg-23.1.4` (commit
`7a941dfb2877a0b8cf30323fefa85ad019e0b0f5`, 56 `.td` files, 113,067 lines)
runs in 3.0 s and writes 100 MB of JSON (sha256 `22741631...d5631` with the
`llvm-tblgen` 23.1.4 of the host).

- 7,484 instruction records have an encoding (`Inst`, 32 entries, each a fixed
  bit or a bit of a named field). 1,336 more are pseudos without one.
- 93 distinct field names. 426 operand classes.
- 1,657 `SysReg` records: name, the 16-bit encoding, readable, writeable, and
  the feature that it needs. Also `TLBI` (85), `DC` (40), `AT` (17), `IC` (3).
- Feature predicates are on the records. 331 records need no feature (the A64
  base), 2,017 need `NEON`, 228 `FPARMv8`. SVE and SME are 3,901 records that
  3d does not generate.
- A decoder built from mask and value alone, "the most fixed bits win", decodes
  every one of the 1,119,541 distinct encodings that the four template boots
  translated (235,746 to 318,190 per boot). None was undecodable.
- Overlaps in the default namespace: 117 pairs where one record refines the
  other, and 4 pairs that cross. 3 or 4 executed encodings per boot hit two
  records of equal specificity.
- 466 records name a C++ `DecoderMethod` (25 distinct), and 129 operand
  classes name one (56 distinct). The JSON has only their names.
- `llvm-mc --disassemble --show-inst` of the same LLVM prints the record name
  of an encoding (`<MCInst #1796 ADDXrs ...>`), so LLVM itself is an exact
  oracle for the decoder.

### G1. The generator

A new build-time tool `vmlab-hv-tcg-a64gen` (a crate of the hv workspace, as
`vmlab-hv-tcg-emit` is for riscv) reads the JSON and writes two committed
files of the new crate `vmlab-hv-tcg-aarch64`:

- `src/generated/decode.rs`: one enum variant per format class (the last
  superclass of a record, for example `BaseAddSubSReg`), a struct of raw
  fields per variant, the record id, the feature that the record needs, and a
  decision tree over the fixed bits.
- `src/generated/sysreg.rs`: the `SysReg`, `TLBI`, `DC`, `AT` and `IC` tables
  as constants: name, encoding, access, feature.

Rules of the generator:

- **G1a.** Records are grouped by format class. The tool emits raw field
  extraction only. What a field means (a logical immediate, a shifted
  register, a scaled offset) is hand-written in `fields.rs`, once per operand
  class. LLVM's C++ decoder methods are not run. Where we port one by hand
  (the logical immediate of `AArch64AddressingModes.h`), the file keeps the
  LLVM notice.
- **G1b.** An overlap is resolved by specificity. A crossing pair of equal
  specificity fails the generation unless `overrides.toml` names the winner
  (4 pairs today). The 4 records of the `Fallback` namespace decode last.
- **G1c.** An instruction set is selected by feature predicate in
  `sets.toml`. The tool emits only the selected records. Another encoding
  decodes to `Undefined`.
- **G1d.** The LLVM tables describe a disassembler. They accept some encodings
  that the architecture leaves undefined or unpredictable. `constraints.toml`
  holds the extra conditions per format class (for example a pair load with
  writeback and equal registers). The random lockstep streams find the
  missing ones: QEMU takes an Undefined Instruction exception and we do not.

Pin and recipes, following D3 and D4:

- `toolchain/llvm/pins.env` pins the tag, the commit, and the sha256 of each
  `.td` file read. A container builds `llvm-tblgen` at that tag. The dump is
  not committed (100 MB).
- `just tcg-a64-generate` dumps and regenerates. `just tcg-a64-generate-check`
  regenerates in memory and compares, as `tcg-generate-check` does.
- A unit test inside `just ci::check` needs no LLVM: it decodes a committed
  vector file (encoding, record name) that the generate recipe made with
  `llvm-mc --show-inst`, for every selected record and for random words.
- `THIRD_PARTY_NOTICES` gains an entry for the two generated files and the
  ported helpers, with the Apache-2.0 text and the LLVM exception.

### M2. Which instruction sets (measured)

All four templates boot to agent ready on `-cpu cortex-a53`, the smallest
64-bit model that QEMU 11.1.3 has: Armv8.0-A with CRC32, AES, PMULL, SHA1 and
SHA2. FP and AdvSIMD cannot be taken away, the arm64 Linux ABI needs them.

| Template | Instructions to agent ready (smp 1) | LLVM records executed | System registers |
|---|---|---|---|
| alpine-3.23 | 29.33e9 | 428 | 93 |
| debian-13 | 18.01e9 | 481 | 99 |
| fedora-44 | 30.70e9 | 545 | 101 |
| ubuntu-24.04 | 20.64e9 | 454 | 96 |

The union is 615 records in 142 format classes. AAVMF is inside each boot.

| Group | Records executed | Format classes executed | Records in the tables for the 3d set | Format classes |
|---|---|---|---|---|
| Data processing, immediate | 32 | 8 | 32 | 8 |
| Data processing, register (with CRC32) | 88 | 19 | 101 | 19 |
| Branches, exceptions, system | 31 | 15 | 54 | 21 |
| Loads and stores (integer, FP, AdvSIMD) | 210 | 40 | 578 | 49 |
| FP and AdvSIMD (with AES, SHA2) | 254 | 60 | 1,566 | 70 |
| Total | 615 | 142 | 2,331 | 167 |

- FP and AdvSIMD only mnemonics are 7.5 % to 13.8 % of executed instructions.
  System-class instructions are 1.1 % to 5.2 %.
- Features executed beyond the base: `FPARMv8`, `NEON`, `AES` (AESE, AESMC,
  PMULL), `SHA2` (the four SHA-256 instructions), `CRC` (all eight).
- In the hint space the guests execute `PACIASP` and `AUTIASP` (575 million
  each over the four boots), `BTI` (233 million), `XPACLRI`, `AUTIA1716`,
  `CSDB` and `CHKFEAT`. On this CPU they are NOPs. The distributions build
  with pointer authentication and BTI in the hint space.
- System instructions executed: `MRS`, `MSR`, `MSR DAIFSet/DAIFClr`, `SVC`,
  `HVC`, `ERET`, `WFI`, `YIELD`, `ISB`, `DSB`, `DMB`, `DC ZVA`, `DC CVAU`,
  `DC CVAC`, `DC CIVAC`, `IC IVAU`, `IC IALLU`, `IC IALLUIS`, and the TLBI
  operations `VMALLE1`, `VMALLE1IS`, `VAE1IS`, `VALE1IS`, `VAAE1`, `VAALE1`,
  `VAALE1IS`, `ASIDE1IS`. No `SMC`.
- System registers touched: 101 in the union, all EL0 or EL1. The list is in
  "System registers the templates touch" below.

### G2. The 3d instruction set

`sets.toml` of 3d selects: the base (no predicate), `FPARMv8`, `NEON`, `AES`,
`SHA2` (LLVM puts SHA1 and SHA-256 under it) and `CRC`. That is 2,331 records
in 167 format classes. Cards implement every record of a format class that
they touch, not only the executed ones, because user programs go past what a
boot executes.

The CPU that the guest sees says exactly this in its ID registers: Armv8.0-A,
4K granule, EL0 and EL1 in AArch64 only, no AArch32, no SVE, no PMU (V5). QEMU
presents `-cpu max` today, so this is a guest-visible difference under HV-D30,
as D7 is for riscv64. It needs the approval of the operator (Q2).

A later set, not part of the 3d acceptance: `LSE` (168 records, the atomics of
v8.1, which help multi-vCPU speed), then the M1 list of the section "M1".

## 2. Semantics

### What exists today, and what of it is reused

`vmlab-hv-tcg` is an interpreter of the Sail model. It is not a general
emulator runtime. The page walk, the TLB, the traps and the CSRs are Sail
code that it interprets, and its block cache records traces of Sail frames.

| Code | Part | For aarch64 |
|---|---|---|
| R1 | `vmlab-hv-tcg-ir::native`: `Graph` (guarded scalar SSA over `Bool`, `Bits(1..128)`, `Integer`, with constant folding, ranges, slices and concat), `linear`, `packed` (the slot executor), `dce`, `schedule` | Reused as it is. It is the typed IR of P1. |
| R2 | `native::block::Block`: ordered events `Guard`, `ReadMemory`, `WriteMemory`, `Exit` | Reused in shape. `Exit` and `Checkpoint` hold Sail frames (`SourceFrame`, a `Function`, named locals). They become generic over what an exit resumes (R8). |
| R3 | `vmlab-hv-tcg::memory`: the `Memory` trait (physical read and write, write generations, `CodeStamp` for code pages, `load_reserved` and `store_conditional` on a granule), `FlatMemory`, `SharedMemory` | Reused as it is. `LDXR` and `STXR` map onto the reservation pair. The code stamps give the invalidation of cached blocks. |
| R4 | Berkeley SoftFloat 3e in `vendor/softfloat` | The generic sources are reused. Only the `RISCV` specialisation is vendored and built. The Arm rules for NaN propagation and tininess need the `ARM-VFPv2` specialisation of the same upstream commit. Both cannot link under the same symbol names, so the build compiles the second copy with a symbol prefix. |
| R5 | `vmlab-hv-lockstep`: the step-and-compare runner, the corpus and the random driver | The runner idea is reused. `State` is riscv-shaped (32 GPRs, `fpr`, `csrs`, a privilege of U, S or M). It becomes a trait with an aarch64 state of its own. The Sail oracle (`sail.rs`) is not used. |
| R6 | `bench/`: the history, `report.py`, `check.py`, the flat-image method | Reused. `bench/run.py` has a defect on `next` today: its TCG path stops with `NameError: BOOT_DEADLINE` (line 130). Card C12 repairs it. |
| R7 | `Interpreter`, `cpu.rs` (`Riscv64`), `decode.rs`, `record.rs`, `trace.rs`, `continuation.rs`, `globals.rs`, `memo.rs`, `intrinsics.rs`, `vmlab-hv-sail-ir`, `vmlab-hv-tcg-emit`, `vmlab-hv-tcg-riscv64` | Sail-shaped. Not used by aarch64 and not changed by 3d. |

What must be made general, in `vmlab-hv-tcg-ir` and a new runtime module:

- **R8.** `Block<X>` with the exit payload as a type parameter. The Sail
  recorder keeps its frames. The aarch64 lifter uses `A64Exit`: the next PC as
  a scalar, or an exception class with its syndrome scalars.
- **R9.** `Operation::Select` (condition, a, b). The graph has no conditional
  value today, and `CSEL`, `CCMP` and every flag-setting shift need one.
  Also `Unary::ReverseBits`, `ReverseBytes(width)` and `PopCount`.
- **R10.** A helper call: an event `Call { helper, args, results, effects }`
  and a pure form as a scalar operation. `effects` says which state a helper
  reads and writes (none, FPCR, FPSR, a system register, memory). The
  interpreter calls a Rust function. The 3e JIT calls the same function.
- **R11.** The register file as numbered slots of fixed type (X0 to X30, SP of
  each level, PC, N, Z, C, V, V0 to V31, FPCR, FPSR), given to the graph as
  inputs and written back at an exit. The Sail path keeps its globals by name.
- **R12.** A softmmu TLB in the runtime, in Rust: virtual page to host
  pointer or MMIO, per translation regime and ASID. The riscv path has none
  (Sail owns it), so this is new code, written so that riscv 3b can use it.

### G3. How the semantics are written

The crate `vmlab-hv-tcg-aarch64` holds them:

```
generated/decode.rs, generated/sysreg.rs    from LLVM (G1)
fields.rs        operand classes: immediates, shifts, extends, offsets
state.rs         the register file (R11), the banked system state
lift/builder.rs  the builder over native::Graph
lift/integer.rs  lift/branch.rs  lift/loadstore.rs  lift/system.rs
lift/fp.rs       lift/simd.rs    lift/crypto.rs
helpers/         flags, FP through SoftFloat, lane loops, AES, SHA, CRC, PMULL
sysreg.rs        behaviour of each system register, keyed by the LLVM encoding
exception.rs     levels, entry, return, syndromes, interrupt masking
mmu.rs           stage-1 walk, parameters as data (granule, regime, sizes)
timer.rs         the generic timer as a value with output lines
```

- **G3a. One lifter per format class.** A lifter is a Rust function that takes
  the decoded fields and a `Builder`, and appends scalars and events to the
  block. 167 format classes cover the 2,331 records, because the tables
  already factor the variants (width, sign, opcode) into fields.
- **G3b. Shared helpers keep it small.** `b.x(n)`, `b.sp()`, `b.add_with_carry`
  (the one source of N, Z, C, V for ADD, SUB, CMP, CCMP and the rest),
  `b.shift`, `b.extend`, `b.bitmask_immediate`, `b.address(mode)` for every
  addressing mode with writeback, `b.load` and `b.store`, `b.cond(cc)`.
- **G3c. Flags are four one-bit slots.** A block that sets flags twice keeps
  only the last write after `dce`, which is what a lazy-flags scheme buys.
- **G3d. Two tiers.** Integer data processing, branches, loads and stores are
  pure IR, so that the 3e JIT can optimise the more than 80 % of executed
  instructions that they are. FP, AdvSIMD, crypto and system instructions are
  helper calls (R10) with declared effects. An AdvSIMD lifter passes the lane
  size, the lane count and an operation to one lane-loop helper over
  `[u8; 16]` values. It does not unroll 16 lanes into the graph.
- **G3e. System registers are one table.** `sysreg.rs` gives each register
  its storage, its reset value, the access per exception level, a write mask,
  and an optional hook (a TLB flush on `TTBR0_EL1`, a timer update on
  `CNTV_CTL_EL0`). The key is the LLVM encoding, so a register that we have
  no row for is known by name in the log. A machine can add rows for the
  IMPLEMENTATION DEFINED space (`S3_x_C15_x`), which the M1 needs.
- **G3f. Unknown is loud.** An access to a register without a row, and an
  encoding outside the selected sets, take the Undefined Instruction
  exception as the architecture says and are logged once with the name. A
  boot that depends on a wrong guess is then found in the log, not in a hang.

### G4. Exception levels: EL0 and EL1 in 3d, EL2 kept open, no EL3

Measured: QEMU's `virt` without `virtualization=on` starts AAVMF at EL1h (PC
0, CPSR `0x3c5` at reset, read over the gdbstub). The four boots touch no
EL2 or EL3 register. PSCI arrives as `HVC`, 8 or 9 calls per boot, and no
`SMC` is executed. So:

- 3d implements EL0 and EL1. `ID_AA64PFR0_EL1` says EL2 and EL3 are absent.
- `HVC` and `SMC` at EL1 leave the core as a `Hypercall` exit with the
  conduit named. The `virt` machine serves PSCI 1.1 on both, as QEMU does:
  `CPU_ON`, `CPU_OFF`, `CPU_SUSPEND`, `SYSTEM_OFF`, `SYSTEM_RESET`,
  `PSCI_VERSION`, `FEATURES`, `MIGRATE_INFO_TYPE`, and the SMCCC version.
  PSCI is not in the core, because the M1 has none.
- The level is a value in `state.rs` with room for EL2: banked `SP_ELx`,
  `ELR_ELx`, `SPSR_ELx`, and a function `target_el(exception, state)` that
  every exception entry goes through. Translation takes a `Regime` value
  (EL1&0 now, EL2 and EL2&0 later). Nothing assumes that EL1 is the highest
  level. EL3 is never built.

### M3. Size (est.)

Hand-written lines of Rust without tests. The basis is the count of format
classes and system registers above, and the size of the same groups in the
reference texts.

| Part | Lines |
|---|---|
| Generator tool `a64gen` | 1,500 |
| Generated `decode.rs` and `sysreg.rs` (not hand-written) | 10,000 |
| IR changes R8 to R11 | 1,200 |
| Builder, state, fields, flag and address helpers | 1,800 |
| Integer data processing (27 format classes) | 1,200 |
| Branches, exception-generating and system instructions (21) | 900 |
| Loads and stores with exclusives (49) | 2,000 |
| Scalar FP, and the SoftFloat Arm build and glue | 2,200 |
| AdvSIMD (about 50 format classes, 1,300 records) | 4,500 |
| AES, SHA1, SHA2, PMULL, CRC32 | 600 |
| System register table (about 130 rows) | 1,500 |
| Exceptions, levels, interrupts | 1,000 |
| Stage-1 walk, softmmu TLB, TLBI, AT, DC ZVA | 2,000 |
| Interpreter loop, block cache, invalidation, exclusives, timer | 2,200 |
| Lockstep: the plugin (C), the follower, the RSP client, the stream generator | 4,000 |
| VMM: vCPU backend seam, `virt`, GICv2 and v2m, PL011, PL031, GED, fw_cfg MMIO, ACPI, DT, SMBIOS, PSCI | 8,000 |
| Total hand-written | about 34,600 |

## 3. The oracle

Two things were measured first, because they decide the method.

| Code | Method | Speed (measured) | What it can do |
|---|---|---|---|
| O1 | gdbstub: `s`, then `g` (all 34 core registers) | 55,700 steps/s (85,200 with `s` alone) | Read 239 registers (34 core, 34 FP and vector, 170 system). Write core and vector registers and memory. A write to a system register answers `OK` and changes nothing. Single step masks interrupts and stops timers by default (`qqemu.sstep` = `0x7`: `ENABLE`, `NOIRQ`, `NOTIMER`). |
| O2 | TCG plugin: a callback before each instruction that reads X0 to X30, SP, PC and CPSR | 3.0 million instructions/s. 1.4 million/s when each 268-byte record is also written to a file. 1.6 million/s with V0 to V31, FPSR and FPCR (788 bytes). | The plugin API offers the same 239 registers. It also reports memory accesses with their values, and each interrupt, exception and host call with the PC before and after. It cannot write state. |

The plugin is 54 times faster. The gdbstub can set state. The design uses
each for what it can do.

### O3. Instruction lockstep (per group, in every card)

1. The stream generator takes format classes from our own decode tables and
   makes random instructions: random fields, then the constraints of G1d, then
   boundary values (register 31, zero and maximum immediates, both widths).
   A second source is the list of distinct encodings that the template boots
   executed (M1), so each card tests what the templates run.
2. It builds a flat image: a prelude that sets the system state with `MSR`
   instructions (the gdbstub cannot, O1), the vector table, data pages with
   random contents, and the stream in blocks of N instructions. Each block
   ends in an `HVC` that ends the block. An exception inside a block lands in
   a vector that records and continues.
3. QEMU runs `-machine virt -cpu cortex-a53 -accel tcg -smp 1 -S -gdb unix:...`
   with the plugin. The harness loads the image and the random initial
   registers through the gdbstub (`M`, `G`), then continues. The plugin
   streams one record per instruction through a pipe: PC, the encoding, the
   registers, and after each record the memory writes and the discontinuity
   of that instruction.
4. Our interpreter runs the same image from the same state. The harness
   compares before each instruction: X0 to X30, SP, PC, CPSR (N, Z, C, V, D,
   A, I, F, the level, SPSel), V0 to V31, FPSR, FPCR, and the memory writes of
   the last instruction. After an exception it also compares `ESR_EL1`,
   `FAR_EL1`, `ELR_EL1` and `SPSR_EL1`, which the plugin reads as registers.
5. The first difference stops the run and prints the encoding, the record
   name from the tables, the state before, and both states after. The failing
   instruction and state are added to a committed regression corpus.

Speed: about 1.5 million compared instructions per second with vector
registers. One million random instructions per format class, for 167 format
classes, is 167 million instructions, or two minutes of QEMU time.

A card proves an instruction group with: every record of the group in the
stream, at least 100,000 instructions per format class without a difference,
the executed encodings of the four boots for that group without a difference,
and zero "Undefined on one side only" results. The card report gives the
counts and the seed. The run needs QEMU, so it is a recipe
(`just tcg-a64-lockstep group=...`) and not part of `just ci::check`. The
regression corpus is replayed in `ci::check` against expected states that are
stored with it, without QEMU.

### O4. Boot lockstep (per boot, from card C10 on)

QEMU boots the real template at `-smp 1` with the plugin, and our whole
machine follows the stream from reset. Both start from the same firmware,
the same disk and the same memory map. The follower compares the state before
every instruction, as in O3.

QEMU is the leader for everything that is not a function of the CPU state:

- **An interrupt** is taken where the stream says (the discontinuity record).
  The follower first checks that our CPU could take it there (not masked, and
  our GIC model has it pending or raises it within a tolerance), then takes it.
- **A device read** (MMIO) uses the value that QEMU read, from the memory
  record. Our device model's answer is compared with it and a difference is
  reported, as a device finding and not a CPU finding.
- **A counter read** (`CNTVCT_EL0`, `CNTPCT_EL0`) adopts the value that
  appears in the destination register of the next record.
- **An ID register** that differs by decision (G2) is in a short allow-list.

Speed: the leader gives at most 3.0 million instructions per second, so a
full boot (18 to 31 billion instructions) takes 1.7 to 2.9 hours per
template when our interpreter keeps up. The stream is compared online through
a pipe and never stored (it would be 5 to 8 TB). On a difference the follower
keeps the last records. This is a run by hand and for the nightly, not for
the merge bar.

### M4. Determinism (measured)

Two QEMU runs of the same Debian boot were compared by a hash of (X0 to X30,
SP, PC, CPSR) over every instruction, at checkpoints of 10 or 20 million
instructions:

| QEMU arguments | Result |
|---|---|
| vmlab's argv | differs at the first checkpoint (20 million) |
| with `-icount shift=0,sleep=off -rtc clock=vm` | differs at the first checkpoint |
| with `-machine virt,dtb-randomness=off -rtc base=2020-01-01T00:00:00` | differs at the first checkpoint (10 million) |
| with `dtb-randomness=off`, the fixed RTC base with `clock=vm`, and `-icount shift=0,sleep=off` | the same at all 24 checkpoints, to 240 million instructions |

So the leader is reproducible only with the random seed of the device tree
off, a fixed clock and `-icount`. That was measured for the first 240 million
instructions (firmware and the start of the kernel), not for a whole boot
with disk I/O. `-icount` runs one thread and did not slow the plugin (2.9
million instructions per second in both).

The design does not need a reproducible leader. O3 has no device, no timer
and no interrupt: single step and the test image take them away. O4 follows
one run while it happens. O4 starts the leader with the arguments of the last
row anyway, so that a failure can be looked at a second time. The follower
also writes the last 100,000 records and
its own state to a file, which is enough to replay the failing window in our
interpreter alone when the leader does not repeat.

### O5. The corpus

- **Random streams from our decode tables** (O3). This is the main corpus.
- **The executed encodings of the four boots**: 1.1 million distinct ones.
- **LLVM's MC tests and `llvm-mc --show-inst`** (Apache-2.0 WITH
  LLVM-exception): for the decoder only (G1).
- **Berkeley TestFloat** (BSD-3-Clause): the FP helpers against SoftFloat's
  own vectors, before they meet QEMU.
- **kvm-unit-tests for arm64** (GPL-2.0, built at test time, run as a guest
  image and never committed): `selftest`, `gic`, `timer`, `psci`, the `sieve`
  and `micro-bench`. Run on QEMU and on vmlab-hv with the same pass list.
  This is the test of what O3 and O4 cannot see: the GIC, the timer and SMP.
- **The four templates and the e2e suite** (P4).

There is no public instruction test suite for A64 with a licence that we can
use that plays the part of riscv-tests. Arm's own validation suites are not
public. RISU compares user-mode signal frames on a Linux host of the same
architecture and was not looked at further. So the random streams against
QEMU carry the proof.

### What this method cannot prove

- **U1.** Where QEMU is wrong, or makes an IMPLEMENTATION DEFINED choice, we
  follow it. P2 makes that the definition of correct.
- **U2.** An UNPREDICTABLE encoding. The streams leave them out (G1d). When a
  real guest executes one, QEMU's answer is recorded as a test.
- **U3.** What QEMU's TCG does not model: caches, exact failure of exclusive
  accesses, PMU counts, the timing of `WFI`.
- **U4.** Our own choice of the moment of an interrupt. O4 adopts QEMU's
  moments. Masking and priority are tested by kvm-unit-tests and by a boot
  without the leader.
- **U5.** More than one vCPU: interleavings and memory ordering. O3 and O4
  run one vCPU. SMP is tested by kvm-unit-tests, by the templates at 2 and 4
  vCPUs, and by a stress test of the exclusives.
- **U6.** EL2, and every Apple register. QEMU's `virt,virtualization=on` can
  lead for EL2 later. Nothing leads for the Apple registers.
- **U7.** A behaviour that no test reaches. The lockstep proves the corpus,
  not the architecture.

## 4. The `virt` machine

### What of notes/02, 11 and 12 still holds

All of it, with three measured additions:

- GICv2 at `0x08000000` with the CPU interface at `0x08010000`, and the GICv2m
  frame at `0x08020000` with SPIs 80 to 143 (D8). PL011 at `0x09000000`,
  PL031 at `0x09010000` (present, no driver binds under ACPI), fw_cfg MMIO at
  `0x09020000`, two 64 MiB pflash, RAM at `0x40000000`, the ECAM at
  `0x4010000000`, the PCI windows, virtio and xHCI as PCI functions with
  MSI-X through GICv2m, the GED for power-down, the architectural timer on
  PPI 27. ACPI through the normalised hwref diff (D9). AAVMF unchanged.
- **V1.** The PSCI conduit that the four templates use is `HVC`. notes/02 read
  "SMC" from a kernel line that names the calling convention, not the conduit.
  Both are served (G4).
- **V2.** AAVMF and the kernel run at EL1 from the first instruction. The
  machine sets the reset state. The firmware finds its devices in the device
  tree that the machine puts at the base of RAM, and gets ACPI and SMBIOS
  through fw_cfg. vmlab-hv generates all three.
- **V3.** The kernels write the debug registers (`DBGBCR`, `DBGBVR`, `DBGWCR`,
  `DBGWVR`, `MDSCR_EL1`, `OSLAR_EL1`, `OSDLR_EL1`) at boot. 3d stores them and
  implements no breakpoint or watchpoint for the guest's own use.
- **V4.** The counter runs at the frequency that the machine gives
  (`CNTFRQ_EL0`). It is derived from host time with an offset that a pause
  and a snapshot keep. Under lockstep it comes from the leader.
- **V5.** The kernels probe the PMU (`PMCR_EL0`, `PMCEID0_EL0`,
  `PMCEID1_EL0`, `PMCNTENCLR_EL0`, `PMINTENCLR_EL1`, `PMOVSCLR_EL0`,
  `PMUSERENR_EL0`). QEMU presents PMUv3 on PPI 23. 3d presents no PMU:
  `ID_AA64DFR0_EL1.PMUVer` is 0 and the MADT has no PMU interrupt. This is a
  second guest-visible difference for Q2.

### What the VMM lacks today

`vmlab-hv-vmm` has no TCG path. `Accel::Tcg` exists in `vmlab-hv-proto`
only. There is no riscv64 machine either: 3b has not started. So 3d needs
nothing from 3b and can go first. It builds these parts so that 3b can use
them:

- **F1. A vCPU backend seam.** `vcpu.rs` is the `KVM_RUN` loop with x86 exits.
  It becomes one loop over a trait: run until an exit (MMIO read or write,
  hypercall, halt, shutdown, reset), kick, read and write the state for a
  snapshot and for the debug port. KVM and the emulator implement it.
- **F2. Guest memory for the emulator.** An implementation of the `Memory`
  trait (R3) over the VMM's guest memory, with MMIO sent to the bus that the
  devices are already on.
- **F3. An interrupt controller in the process.** x86 uses the in-kernel
  irqchip. The GICv2 is the first controller that is a device model. It
  implements `IrqLine` for its inputs and `MsiSink` for GICv2m, and drives
  two lines into each vCPU (IRQ and FIQ).
- **F4. A second machine model.** `q35.rs` and `pc.rs` are x86. `virt` gets
  its own module: the memory map, the device tree, ACPI (MADT with GICC, GICD
  and the v2m frame, GTDT, MCFG, SPCR, DBG2, IORT, PPTT, FADT with the PSCI
  flags, DSDT with the GED), SMBIOS 3.0, and fw_cfg over MMIO.
- **F5. New devices**: PL011, PL031, the GED, GICv2, GICv2m. pflash and every
  virtio and PCI device exist.
- **F6. CPU state in snapshots** (D13), reset, pause, and the debug port
  (`gdb.rs` is typed for x86).

The core crate knows none of this. It sees a `Memory`, two interrupt lines,
the timer's output lines, a `Hypercall` exit and a hook for machine system
registers. That is the boundary that P3 asks for.

## 5. Speed without the JIT

### M5. The QEMU baseline (measured)

Boot to agent ready, QEMU 11.1.3 TCG, `-cpu max`, vmlab's argv for a
`linux-modern` VM, AAVMF 2026.08 as vmlab ships it, a fresh overlay per run,
pinned to four cores:

| Template | 1 vCPU: samples, median (s) | 2 vCPUs: samples, median (s) | Instructions (1 vCPU, cortex-a53) | QEMU average MIPS, 1 vCPU |
|---|---|---|---|---|
| alpine-3.23 | 46.60, 45.59, 45.74: **45.7** | 42.96, 43.11, 41.84: **43.0** | 29.33e9 | 641 |
| debian-13 | 28.67, 26.64, 25.94: **26.6** | 20.12, 20.22, 20.15: **20.2** | 18.01e9 | 676 |
| fedora-44 | 60.82, 68.20, 61.34: **61.3** | 42.38, 44.31, 41.46: **42.4** | 30.70e9 | 501 |
| ubuntu-24.04 | 44.18, 45.70, 42.35: **44.2** | 28.97, 28.72, 28.40: **28.7** | 20.64e9 | 467 |

The last column divides the instruction count by the boot time. Idle time is
inside it, so QEMU's speed while it executes is higher. The determinism test
of M4 ran on other cores of the host during some of these boots.

Alpine and Fedora take longer than Debian for reasons inside the guest: the
harness gives no cloud-init seed, and Alpine's cloud-init waits for one. The
numbers are the QEMU side of D1 for 3e, and the yardstick for 3d.

### M6. What the interpreter must reach

- vmlab waits 600 s for a VM to become ready (`VM_READY_TIMEOUT`,
  `src/labd/vm.rs`). The largest boot is 30.7 billion instructions (Fedora).
  So the floor is 51 million instructions per second (MIPS) on the boot mix,
  with one vCPU and no idle time. With a margin of two it is 100 MIPS.
- QEMU's average over a boot, idle time included, is 467 to 676 MIPS (M5).
- The existing interpreter path gives 51 MIPS on the CoreMark mix and 1,594
  MIPS on a three-instruction loop (`crates/vmlab-hv-tcg/PERF.md`), through
  the same `packed` executor (R1), with the whole Sail model inside each
  block. A hand-lifted aarch64 block has no model inside it: one instruction
  is 3 to 10 graph operations.
- *Est.*: 100 to 300 MIPS on the boot mix. The basis is the two measured
  numbers above and the cost of a softmmu lookup per load and store. This is
  not measured. Card C1 measures it on the first instruction groups, and card
  C5 on a flat CoreMark image.

At 100 to 300 MIPS the four templates reach agent ready in 1 to 5 minutes at
one vCPU: 2 to 7 times slower than QEMU, and inside the timeout. That is
acceptable for P4, which asks for a boot and not for a speed. It is not
acceptable as the default for users, which is what 3e is for.

Three rules follow:

- **The gate of C5**: below 100 MIPS on the flat CoreMark image, the
  interpreter gets a pre-decoded block form before more instruction groups
  are written.
- **vCPUs**: the 3d interpreter runs the vCPUs of one machine on one host
  thread each, as D10 says, from the start. The exclusives and the TLB are
  per vCPU, and memory goes through `SharedMemory`.
- **The switch of P4**: vmlab selects vmlab-hv for aarch64 when the
  acceptance list passes. If the operator wants the switch only at QEMU's
  speed, the switch waits for 3e. This is Q4.

## The M1 machine: what 3d keeps possible

The M1 machine is not part of this card. The public facts used here: a Linux
on an M1 reports `fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp
asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 asimddp sha512 asimdfhm
dit uscat ilrcpc flagm ssbs sb paca pacg dcpodp flagm2 frint`. The Asahi
documentation lists the Apple registers and instructions.

| Code | An M1-class guest needs | Beyond `virt` | What 3d does so that it is not blocked |
|---|---|---|---|
| A1 | Armv8.1 to v8.5 instructions: LSE atomics (168 records), RDM (24), FP16 (248) and FHM (16), DotProd (8), JSCVT (1), FCMA (13), LRCPC (4) and LRCPC2 (13), SHA3 and SHA512 (8), FlagM and FlagM2 (4 and more), FRINT (20), SB (1), DIT, SSBS | All are extra | Instruction sets are data (`sets.toml`, G1c). Each is a set of format classes for a later card. No decoder change. |
| A2 | Pointer authentication: the PAC instructions (35 records), the five keys, a PAC function | Extra. On `virt` the PAC hints are NOPs. | The hint space decodes to named records already (`PACIASP` and the rest). The keys are rows of the system register table. The PAC function is one function that the machine picks: the Apple cores use an IMPLEMENTATION DEFINED algorithm, so no fixed algorithm is built in. |
| A3 | EL2 with VHE always on, no EL3 | `virt` has EL1 only | G4: the level is data, the banks have room for EL2, every exception goes through `target_el`, and translation takes a `Regime`. |
| A4 | 16K pages | `virt` guests use 4K | The stage-1 walk takes the granule from `TCR` as data (4K, 16K, 64K). The softmmu TLB works in 4K host pages, which every granule is a multiple of. `ID_AA64MMFR0_EL1` is filled by the CPU model. |
| A5 | The Apple system registers (`S3_x_C15_x`: HID, PMU, IPI, APCTL, the timer FIQ enables) and the Apple instructions at `0x0020xxxx` | None on `virt` | G3e: a machine adds rows to the system register table. The decoder leaves `Undefined` encodings to a machine hook before the exception is taken. |
| A6 | AIC in place of the GIC. Timers arrive as FIQ. | `virt` uses the GICv2 and IRQ | F3: the core has an IRQ line and a FIQ line and knows no controller. The timer is a value with output lines that the machine wires. |
| A7 | No PSCI. Secondary CPUs start through machine registers. | `virt` has PSCI | G4: PSCI is in the `virt` machine. The core has a reset entry that takes the level, the PC and the registers from the machine. |
| A8 | A CPU identity (MIDR, the feature registers, the cache registers) | Another one | The ID registers come from a CPU model value, not from constants in the core. |

What stays open for the M1 phase and is not designed here: SPRR and GXF
(needed by macOS, not by Linux), AMX, the oracle for Apple behaviour (U6).

## System registers the templates touch (measured)

The union over the four boots, 101 registers:

`AIDR_EL1 CLIDR_EL1 CNTFRQ_EL0 CNTKCTL_EL1 CNTVCT_EL0 CNTV_CTL_EL0
CNTV_CVAL_EL0 CONTEXTIDR_EL1 CPACR_EL1 CTR_EL0 CurrentEL DAIF
DBGBCR0..5_EL1 DBGBVR0..5_EL1 DBGWCR0..3_EL1 DBGWVR0..3_EL1 DCZID_EL0
ELR_EL1 ESR_EL1 FAR_EL1 FPCR FPSR ID_AA64DFR0_EL1 ID_AA64DFR1_EL1
ID_AA64FPFR0_EL1 ID_AA64ISAR0..3_EL1 ID_AA64MMFR0..4_EL1 ID_AA64PFR0..2_EL1
ID_AA64SMFR0_EL1 ID_AA64ZFR0_EL1 ID_DFR0_EL1 ID_DFR1_EL1 ID_ISAR0..6_EL1
ID_MMFR0..5_EL1 ID_PFR0..2_EL1 MAIR_EL1 MDSCR_EL1 MIDR_EL1 MPIDR_EL1
MVFR0..2_EL1 NZCV OSDLR_EL1 OSLAR_EL1 PMCEID0_EL0 PMCEID1_EL0
PMCNTENCLR_EL0 PMCR_EL0 PMINTENCLR_EL1 PMOVSCLR_EL0 PMUSERENR_EL0
REVIDR_EL1 SCTLR_EL1 SPSR_EL1 SP_EL0 TCR_EL1 TPIDRRO_EL0 TPIDR_EL0
TPIDR_EL1 TTBR0_EL1 TTBR1_EL1 VBAR_EL1`

Per boot: 8,398 to 114,479 interrupts, 459,367 to 1,546,396 exceptions, 8 or
9 host calls.

## 6. The cards

The order is the number. "With" names the cards that can run at the same
time. Every card ends with `just ci::check` at exit 0, lists the sources that
it read (S1 to S8), and keeps X1 to X3.

| Card | Title | What to do | Proof | After | With |
|---|---|---|---|---|---|
| C1 | aarch64 path end to end: decoder from the LLVM tables, integer immediate group and branches, in lockstep against QEMU | `toolchain/llvm` with the pins. `vmlab-hv-tcg-a64gen` and the two generated files for the 3d set (G1, G2), the two recipes, the vector test. IR changes R8, R9 and R11. The builder and the state. Lifters for data processing immediate (8 format classes) and for `B`, `BL`, `Bcc`, `CBZ`, `TBZ`, `BR`, `BLR`, `RET`. A minimal interpreter loop on `FlatMemory`. The O3 harness: plugin (from `measure/regtrace.c`), the RSP client, the stream generator, the follower. The notices. A line in `hv/AGENTS.md` for X1 to X3, and a change of its sentence "Apache-2.0 crates are used as dependencies only", which P1 ends for the LLVM tables. | `tcg-a64-generate-check` passes. The decoder names the same record as `llvm-mc --show-inst` on 10 million random words of the selected sets. O3: 100,000 instructions per format class and the executed encodings of the group, no difference, counts and seed in the report. The measured MIPS of the loop on a branch-and-add image. | none | none |
| C2 | Integer data processing with registers, and the flags | Lifters for the 19 format classes: add and subtract with carry, shifted and extended, logical, conditional select and compare, multiply and divide, bit operations, CRC32. | O3 for the group, as C1. | C1 | C3, C4, C12 |
| C3 | Loads and stores on a flat address space | The addressing helper and the 49 format classes: all modes with writeback, pairs, literal, unscaled, the exclusives on `load_reserved` and `store_conditional`, the FP and vector register forms, `LD1` to `LD4` and `ST1` to `ST4`. Alignment and translation faults come with C6. | O3 for the group, with memory writes compared. A stress test of the exclusives on two threads. | C1 | C2, C4, C12 |
| C4 | Scalar FP and the SoftFloat build for Arm | The second SoftFloat specialisation under a symbol prefix (R4). R10. FPCR and FPSR. The scalar FP format classes: arithmetic, compare, convert, round, fused multiply, move, select. | TestFloat on the helpers. O3 for the group with FPSR and FPCR compared, in all four rounding modes. | C1 | C2, C3, C12 |
| C5 | Flat CoreMark image and the interpreter speed gate | An aarch64 flat CoreMark and a scalar loop in `bench/`, run on our interpreter and on QEMU. The bench history for aarch64. The gate of M6: below 100 MIPS, add the pre-decoded block form in this card. | The measured MIPS in the bench history, 3 repeats, pinned. CoreMark's own checksums. | C2, C3 | C6, C7 |
| C6 | Exceptions, levels and the system registers | `exception.rs` and `sysreg.rs`: EL0 and EL1, entry and return, syndromes, `SVC`, `HVC` and `SMC` as exits, `MRS` and `MSR`, the PSTATE forms, the hint space, `WFI`, barriers, the about 130 register rows with the ID registers of G2, the interrupt lines and masking, the generic timer. | O3 with the exception registers compared: a stream with faults, system calls and returns. A probe on QEMU for each ID register row that differs, in the allow-list. Each of the 101 measured registers has a row and a test. | C2 | C5, C7 |
| C7 | AdvSIMD and the crypto instructions | The lane helpers and the about 50 format classes of AdvSIMD. AES, SHA1, SHA2, PMULL. Two agents can split it: integer lanes, and FP lanes with crypto. | O3 for the group, with V0 to V31 compared. | C3, C4 | C5, C6, C8 |
| C8 | Translation and the softmmu | `mmu.rs` and R12: the stage-1 walk for 4K with the granule as data (A4), permissions, access and dirty handling as QEMU's CPU does it, faults with their syndromes, the TLB per regime and ASID, TLBI, `AT`, `DC ZVA`, cache operations, self-modifying code through the code stamps. | O3 with the MMU on: random page tables, permission and translation faults compared. A walk test for 16K and 64K against QEMU's `-cpu max`. | C3, C6 | C7, C9 |
| C9 | VMM: the vCPU backend seam and the devices of `virt` | F1, F2, F3 and F5: the trait and the emulator backend behind `accel: tcg`, the memory adapter, GICv2 and GICv2m, PL011, PL031, the GED. No machine yet. | The KVM tests of the hv workspace pass unchanged. Device unit tests against register probes on QEMU (S6, S7). A test VM of one emulated vCPU that writes to the PL011. | C1 (the seam), C6 (the test VM) | C7, C8 |
| C10 | The `virt` machine: AAVMF to the UEFI shell, and boot lockstep | F4: the map, the device tree, fw_cfg over MMIO, pflash, the ECAM and the virtio devices, PSCI on `HVC` and `SMC`, reset and power-off. The O4 follower. | S1: AAVMF reaches its UEFI shell on the serial log. O4 from reset to the shell without a CPU difference. kvm-unit-tests `selftest`, `gic`, `timer` and `psci` pass as on QEMU. | C8, C9 | C11 |
| C11 | ACPI, SMBIOS and the hwref diff for aarch64 | The tables of F4 through fw_cfg. The hwref capture configuration for `virt` (D9) and its allow-list, with the differences of G2 and V5. | The normalised hwref diff against QEMU is empty outside the allow-list. A Debian kernel boots with ACPI to its first user process. | C9 | C10 |
| C12 | Repair `bench/run.py` for the TCG arches, and the four-template baseline | `BOOT_DEADLINE` is not defined on `next`, so no TCG suite runs. Repair it. Make the shutdown path work on Alpine. Record the QEMU baseline of the four templates at 1, 2 and 4 vCPUs in the history. | `bench-test` passes. The history lines. | none | all |
| C13 | The four templates to agent ready | What the boots still miss, found by O4 and the unknown-is-loud log (G3f). Multi-vCPU runs. | S1: each template reaches agent ready at 1 and 2 vCPUs inside 600 s under the memory cap. O4 for Debian from reset to agent ready without a CPU difference. kvm-unit-tests with 4 vCPUs. | C7, C10, C11 | C14 |
| C14 | Snapshots, reset and the debug port for an emulated machine | F6 and D13: CPU, GIC and timer state in the snapshot format, the caches rebuilt after a restore, a guest reset in place. | The S1 snapshot tests on an aarch64 machine: online and offline, restore, and the agent answers after it. | C10 | C13 |
| C15 | vmlab side: describe and select vmlab-hv for aarch64 | `describe.rs` for an aarch64 launch, the refusal list, the selection of §22 for aarch64 with the migration of QEMU identities, firmware lookup, the display and input path, the manual and the PRD. | e2e `arch.emulated` on `--hypervisor vmlab`. The arch-relevant e2e features. `just ci::check`. | C13, C14 | C16 |
| C16 | Remove the QEMU hypervisor code | Only after Q3: the QEMU adapter, the QMP client, the process code, `plan_accel`, and `docs/agents/qemu-removal-remaining.md`. | The whole e2e suite on vmlab-hv. No `qemu-system-*` is started by any product path. | C15, and 3b or the answer to Q3 | none |

C1 is small enough to prove the path: one generator, two instruction groups
(29 format classes), a loop without an MMU, and the harness that every later
card uses.

## Questions for the operator

None blocks C1 to C14.

- **Q1.** May a card open the prose chapters of the Arm Architecture
  Reference Manual (register descriptions, the exception model), never its
  pseudocode, when S2 to S6 do not settle a system behaviour? Recommended: no
  for now. Decide only if a card reports such a case.
- **Q2.** Approve two guest-visible differences from QEMU's `-cpu max`
  (HV-D30): the CPU presents Armv8.0-A with CRC32, AES, SHA1 and SHA2 (G2),
  and no PMU (V5). Recommended: approve, as D7 was for riscv64.
- **Q3.** riscv64 VMs run only on QEMU until 3b. Remove the QEMU code after
  3d and drop riscv64 guests until 3b, or keep it until 3b? Recommended: keep
  it, and do C16 after 3b.
- **Q4.** Does vmlab select vmlab-hv for aarch64 at the end of 3d, at 2 to 7
  times QEMU's boot time (M6), or only after the JIT of 3e? Recommended: at
  the end of 3d as P4 says, with QEMU still selectable by `hypervisor =
  "qemu"` until 3e meets D1.

## How to measure again

On a host with QEMU 11, LLVM 23, gcc and glib:

```sh
gcc -O2 -shared -fPIC $(pkg-config --cflags glib-2.0) -o a64trace.so measure/a64trace.c
gcc -O2 -shared -fPIC $(pkg-config --cflags glib-2.0) -o regtrace.so measure/regtrace.c

# M1: the tables
git clone --depth 1 --branch llvmorg-23.1.4 --filter=blob:none --sparse https://github.com/llvm/llvm-project.git
git -C llvm-project sparse-checkout set llvm/include/llvm llvm/lib/Target/AArch64
llvm-tblgen --dump-json -I llvm-project/llvm/include -I llvm-project/llvm/lib/Target/AArch64 \
  llvm-project/llvm/lib/Target/AArch64/AArch64.td -o aarch64.json

# M2, M5: one boot to agent ready (bench dir, template, -cpu, vCPUs, output dir, plugin)
python3 measure/probe.py hv/bench debian-13 cortex-a53 1 runs a64trace.so
python3 measure/probe.py hv/bench debian-13 max 2 runs
python3 measure/tables.py aarch64.json runs/debian-13-cortex-a53-smp1-plug/trace.txt
python3 measure/union.py aarch64.json runs/*-plug/trace.txt

# O1: QEMU started with -S -gdb unix:gdb.sock,server=on,wait=off on AAVMF
python3 measure/gdbstep.py gdb.sock 20000

# O2, M4: add to a QEMU argv; mode is pchash, gpr or all
#   -plugin regtrace.so,mode=gpr,limit=400000000,ck=20000000
# M4 runs it twice and compares the "ck" lines, also with
#   -machine virt,dtb-randomness=off -rtc base=2020-01-01T00:00:00,clock=vm -icount shift=0,sleep=off
```

`probe.py` needs the templates under `~/.local/share/vmlab/templates/aarch64`
and AAVMF under `~/.local/share/vmlab/guest/firmware/aarch64`
(`guest/fetch-firmware.sh aarch64`). Every QEMU ran under a memory cap, one or
two at a time, pinned.
