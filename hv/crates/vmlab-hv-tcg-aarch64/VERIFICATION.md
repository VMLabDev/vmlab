# Verification of `vmlab-hv-tcg-aarch64`

What each card of phase 3d proved for this crate, with the commands, the
counts and the seeds. The method is in
`hv/docs/plans/phase-3d-aarch64/DESIGN.md` (G1 for the decoder, O3 for the
instruction lockstep). Every number here is from helios (Ryzen 9 9950X) with
QEMU 11.1.3 and LLVM 23.1.4. The recipes run in `hv/`.

## Card C1 (2026-11-26)

Scope: the decoder for the 3d set, the eight format classes of data processing
with an immediate (32 records), and the six branch classes `BranchImm`,
`CallImm`, `BranchCond`, `BaseCmpBranch`, `BaseTestBranch` and `BranchReg`
(14 records).

### The generated files are what the pinned tables give

`just tcg-a64-generate-check` passes with the container-built LLVM tools and
with the host tools (`llvm=host`). The JSON dump has the pinned sha256
`22741631ef3adfdb8248891180a2b83176d816eb4ef3a746a9f19365755d5631`: 2,332
records in 168 format classes, with 15 constraints.

The design counts 2,331 records in 167 format classes. The difference is
`UDF` (format class `UDFType`), which is in no group of its table.

### The decoder names the record that LLVM names

`just tcg-a64-decode-check` (10,000,000 words, seed 1): 9,230,767 words decode
to the same record as `llvm-mc --show-inst`, 769,233 are undefined on both
sides, and 0 differ. No word is refused by a constraint that LLVM does not
have. Nine words in ten are the fixed bits of a random selected record with
random free bits, and one in ten is a random word.

The run found what the mask and value of a record do not say, and
`tables/constraints.toml` now holds it: the conditions of the LLVM decoder
methods `DecodeThreeAddrSRegInstruction`, `DecodeAddSubERegInstruction`,
`DecodeMoveImmInstruction`, `DecodeAddSubImmShift`,
`DecodeLogicalImmInstruction`, `DecodePRFMRegInstruction`,
`DecodeSystemPStateImm0_15Instruction`, `DecodeSystemPStateImm0_1Instruction`,
`DecodeSVCROp` and `DecodeGPRSeqPairsClassRegisterClass`. It also found that
LLVM's `SoftFail` bits are free bits (30 records: the exclusive loads and
stores, `SMULH` and `UMULH`).

`just ci::check` replays this without LLVM: `tests/vectors.rs` decodes the
13,660 committed vectors of `tests/vectors.txt` and compares the decision tree
with a linear scan of the record table on 400,000 more words.

### Instruction lockstep against QEMU

`just tcg-a64-lockstep c1 100000 1 "<the four traces>"`: 1,933,143
instructions verified in 371,934 rounds, no difference, 75 s. An instruction
is verified when both sides ran it from the same registers and had the same
registers after it (X0 to X30, SP, PC and CPSR).

| Format class | Instructions verified |
|---|---|
| `ADRI` | 183,398 |
| `AddSubImmShift` | 165,317 |
| `BaseLogicalImm` | 115,062 |
| `BaseMoveImmediate` | 124,868 |
| `BaseInsertImmediate` | 123,200 |
| `BaseBitfieldImm` | 112,831 |
| `BaseBitfieldImmWith2RegArgs` | 100,861 |
| `BaseExtractImm` | 101,569 |
| `BranchImm` | 109,584 |
| `CallImm` | 312,691 |
| `BranchCond` | 113,893 |
| `BaseCmpBranch` | 131,316 |
| `BaseTestBranch` | 133,477 |
| `BranchReg` | 105,076 |

- Random phase: 71,359 rounds of 1,024 random instructions each, seed 1, until
  every class had 100,000. Every one of the 46 records was verified, the
  least often `EORXri` with 12,617. 346,966 words of the images were made
  without the constraints (one in 200).
- Executed phase: the four traces of the template boots hold 734,599 distinct
  encodings. 493,591 of them are of this group (193,205 that do not branch and
  300,386 branches), and all 493,591 were verified. The counts of the table
  include them.
- "Undefined on one side only": 0. In 479 rounds both sides took the Undefined
  Instruction exception at the same word.
- How the rounds ended: 293,048 at their budget, 21,463 at a branch into
  QEMU's flash or devices, 17,384 at a fetch abort on both sides, 15,873 at
  the exception of a `UDF` word, 14,617 at a loop of the stream, 9,070 at a
  PC that is not a multiple of four on both sides, 479 undefined on both sides.

The traces are the output of `measure/a64trace.c` of the design card for the
four template boots on `-cpu cortex-a53`. They are not in the repository. On
helios they are under `/home/vmlab/src/stc/card-1032/runs/`.

`just ci::check` replays `tests/corpus/c1.txt` without QEMU: three verified
instructions of each record, sampled from this run, with the registers that
QEMU had before and after.

What this run does not compare: which exception QEMU takes (the class and the
syndrome), the vector registers, and memory writes. No instruction of this
group writes memory or a vector register, and the follower fails a round in
which QEMU writes memory. The exception card (C6) compares the syndromes.

What the run showed about the oracle, for the later cards:

- QEMU does not keep the PC register current inside a translation block. The
  plugin takes the PC of an instruction from the address of its block.
- The `virt` machine puts its device tree at the start of the RAM. The
  follower starts from a copy of QEMU's RAM.
- For a PC that is not a multiple of four, QEMU writes an instruction record
  before the exception in some cases and none in others. The follower takes
  both.

### The speed of the loop

`taskset -c 30 just tcg-a64-mips` (100,000,000 iterations of
`add x0, x0, #1 ; subs x1, x1, #1 ; b.ne`, 300,000,000 instructions, 3
repeats): 97.24, 97.34 and 97.35 MIPS, median 97.34. The loop is one block of
three instructions, translated once and run 100,000,000 times.

This is the block executor as it is: a lookup in a direct-mapped cache, the
register slots copied in, the packed operations run on `u128` slots, and the
slots copied out, for every block. It has no chaining of blocks and no memory
access. Card C5 measures a flat CoreMark image against the gate of the design
(M6: 100 MIPS).

### Sources read for this card

- S1: the JSON dump of the LLVM AArch64 TableGen records, and two LLVM
  source files: `MCTargetDesc/AArch64AddressingModes.h` (the logical
  immediate, ported with its notice) and
  `Disassembler/AArch64Disassembler.cpp` (the conditions of the decoder
  methods, restated in `tables/constraints.toml`).
- S6: the behaviour of a running QEMU through its gdbstub and its plugin API
  header `qemu-plugin.h`.
- S2, S3, S4, S5, S7 and S8 were not opened.
- X1 to X3 hold: no QEMU source, no Arm ASL, XML or pseudocode, no Unicorn.

## Card C2 (2026-11-26)

Scope: data processing with registers, 101 records in the nineteen format
classes of `lift/register.rs`: add and subtract with a shifted register, with
an extended register and with the carry, the logical operations, conditional
compare and select, the shifts by a register, divide, multiply and
multiply-add, the bit operations, and CRC32. The lifters are pure IR. CRC32
is one multiply by a constant and one exclusive-or per data bit, because the
helper call of the design (R10) comes with card C4.

### Instruction lockstep against QEMU

`just tcg-a64-lockstep c2 100000 1 "<the four traces>"` on the commit
`8a12cd2e`: 1,958,284 instructions verified in 4,656 rounds, no difference,
18 s. The registers compared are those of card C1 (X0 to X30, SP, PC and
CPSR with N, Z, C and V).

| Format class | Instructions verified |
|---|---|
| `BaseAddSubSReg` | 120,421 |
| `BaseAddSubEReg` | 104,216 |
| `BaseAddSubEReg64` | 100,198 |
| `BaseAddSubCarry` | 100,162 |
| `BaseAddSubCarrySetFlags` | 100,188 |
| `BaseLogicalSReg` | 111,537 |
| `BaseCondComparisonImm` | 101,549 |
| `BaseCondComparisonReg` | 101,835 |
| `BaseCondSelect` | 106,123 |
| `BaseCondSelectOp` | 101,427 |
| `BaseShift` | 103,340 |
| `BaseDiv` | 100,887 |
| `BaseMulAccum` | 102,680 |
| `WideMulAccum` | 102,179 |
| `MulHi` | 100,521 |
| `BaseOneOperandData` | 100,467 |
| `OneWRegData` | 100,199 |
| `OneXRegData` | 100,203 |
| `BaseCRC32` | 100,152 |

- Random phase: 4,600 rounds of 1,024 random instructions each, seed 1, until
  every class had 100,000. Every one of the 101 records was verified, the
  least often `EONWrs` with 6,158. 23,534 words of the images were made
  without the constraints (one in 200).
- Executed phase: of the 734,599 distinct encodings of the four template
  boots, 56,881 are of this group, and all 56,881 were verified. The counts
  of the table include them.
- "Undefined on one side only": 0 in the final run. In 1,247 rounds both
  sides took the Undefined Instruction exception at the same word.
- How the rounds ended: 56 at their budget, 3,353 at the exception of a `UDF`
  word after the image, 1,247 undefined on both sides. No instruction of the
  group branches, so a round runs from its start to the end of its image.
- A second seed, `just tcg-a64-lockstep c2 100000 2` without the traces:
  1,901,401 instructions in 4,481 rounds, no difference.
- The groups of both cards mixed in one stream,
  `just tcg-a64-lockstep all 20000 3`: 670,122 instructions in 14,669 rounds,
  no difference. Here the flags that a compare of this card sets feed the
  conditional branches of card C1, and blocks hold instructions of both.

The first run found one difference, and it is a decoder difference: QEMU takes
the Undefined Instruction exception for `SMULH` and `UMULH` when the bits 14 to
10 are not all ones (first seen at the word `9bc16413`). LLVM marks those bits
`SoftFail` and decodes any value. `tables/constraints.toml` now refuses such a
word (16 constraints), the decoder and the vectors are regenerated
(`just tcg-a64-generate`), and the run above is the one after that change. No
lifter needed a change after its first lockstep run.

`just ci::check` replays `tests/corpus/c2.txt` without QEMU: three verified
instructions of each of the 101 records, sampled from the run with the seed 1.
`tests/register.rs` runs 400 random instructions of each record against a
second model in plain Rust (40,400 instructions), and the corner cases that
random registers reach seldom: a division by zero, the most negative number
divided by minus one, a shift amount of the register width or more, the carry
in and out of `ADCS` and `SBCS`, and a block of 64 `CRC32CX`.

What this run does not prove: the same limits as card C1. In addition, the
speed of CRC32 as pure IR is not measured (about 3 operations per data bit,
193 for `CRC32X`). Card C5 measures the interpreter. A helper call (R10) can
replace the lifter body when card C4 has added it.

### Sources read for this card

- S1: the generated tables of the LLVM AArch64 records (the fields and
  operand classes of the nineteen format classes). No LLVM source file was
  opened for this card.
- S6: the behaviour of a running QEMU through the lockstep harness of card C1.
- S2, S3, S4, S5, S7 and S8 were not opened. The prose of the Arm manual that
  the operator allowed (Q1) was not opened either.

## Card C4 (2026-11-26)

Scope: the second SoftFloat build (R4), helper calls in the IR (R10), FPCR
and FPSR, and the 16 format classes of scalar FP that are not loads and
stores (140 records): `BaseSingleOperandFPData`, `BaseTwoOperandFPData`,
`BaseThreeOperandFPData`, `BaseTwoOperandFPComparison`,
`BaseOneOperandFPComparison`, `BaseFPCondComparison`, `BaseFPCondSelect`,
`BaseFPConversion`, `BaseFPMoveImmediate`, `BaseFPToIntegerUnscaled`,
`BaseFPToInteger`, `BaseIntegerToFPUnscaled`, `BaseIntegerToFP`,
`BaseUnscaledConversion`, `BaseUnscaledConversionFromHigh` and
`BaseUnscaledConversionToHigh`.

### How it is built

- **SoftFloat.** `build.rs` compiles the generic sources of the vendored
  SoftFloat 3e with the `ARM-VFPv2` specialisation of the same upstream
  commit, beside the `RISCV` copy of `vmlab-hv-tcg`. Every symbol of this
  copy has the prefix `a64sf_`. `build.rs` reads the symbol index of the
  archive at every build and fails on a symbol without the prefix. The
  archive refers to nothing outside itself but `__stack_chk_fail`,
  `__tls_get_addr` and `_GLOBAL_OFFSET_TABLE_` (`nm -u`).
- **R10.** A helper is a static with a function pointer. The pure form is the
  scalar `Operation::Call`: a function of its arguments, one scalar for equal
  arguments, dropped when nothing reads it. The ordered form is the event
  `Event::Call`, which runs once at its place and gets the state of the
  frontend. The interpreter loop runs both. Every FP helper is pure: FPCR is
  an argument, and the bits for FPSR come back above the value. No lifter of
  this card needs the ordered form, and a unit test of the loop runs it.
- **What SoftFloat does not decide.** `helpers/fp.rs` picks the NaN of a
  result by the A64 rules before SoftFloat sees an operand, and adds the
  default NaN mode, flush-to-zero on operands (FPSR.IDC) and on results
  (FPSR.UFC alone), the four minimum and maximum forms, the conversions to a
  fixed-point integer, and the alternative half-precision format.

### TestFloat on the helpers

`just tcg-a64-testfloat` builds `testfloat_gen` of Berkeley TestFloat
(`a9c849f1`) against upstream SoftFloat (`0a5ad290`, `ARM-VFPv2`), from the
archives that `toolchain/testfloat/pins.env` pins by sha256. That SoftFloat
is not our build: it comes from upstream's makefile. Each test case goes
through `helpers/fp.rs`, and the result and the exception bits must be the
same.

Level 1, tininess before rounding: 51,025,344 test cases the same in 174
runs, 162,456 not compared, no difference, 8 s. A run is one function in one
rounding mode:

- `add`, `sub`, `mul`, `div`, `sqrt` and `mulAdd` of 32 and 64 bits, in the
  four modes of FPCR.
- `roundToInt` in the four modes with inexact raised (`FRINTX`), and in five
  modes without (`FRINTN`, `FRINTZ`, `FRINTM`, `FRINTP`, `FRINTA`, each run
  with another mode in FPCR).
- The six compares, as the flags of `FCMP` and `FCMPE`.
- The conversions to `i32`, `ui32`, `i64` and `ui64` in five modes, and from
  them in four.
- The six conversions between 16, 32 and 64 bits in four modes.

The cases that are not compared are those of `mulAdd` with a NaN addend and a
NaN factor. SoftFloat takes the NaN from the factors first and A64 from the
addend first, so the two differ by design. The lockstep covers them.

`just ci::check` replays `tests/testfloat.txt` without the generator: 16
cases of each run, 2,784 cases.

The unit tests of `helpers/fp.rs` also compare our own code with SoftFloat
where both can do the same: 300,000 conversions to an integer in every mode
and width, and 300,000 roundings to half precision in the IEEE format (the
same routine that rounds to the alternative format).

### Instruction lockstep against QEMU

`just tcg-a64-lockstep fp 100000 1 "<the four traces>"`: 1,601,649
instructions verified in 133,476 rounds, no difference, 38 s. In this group
an instruction is verified when both sides ran it from the same X0 to X30,
SP, PC, CPSR, V0 to V31, FPSR and FPCR and had the same of all of them after
it. This is the comparison that review note N3 of card C1 asked for, without
the memory writes, which no instruction of this group makes.

| Format class | Instructions verified |
|---|---|
| `BaseSingleOperandFPData` | 100,045 |
| `BaseTwoOperandFPData` | 100,233 |
| `BaseThreeOperandFPData` | 100,041 |
| `BaseTwoOperandFPComparison` | 100,189 |
| `BaseOneOperandFPComparison` | 100,111 |
| `BaseFPCondComparison` | 100,032 |
| `BaseFPCondSelect` | 100,075 |
| `BaseFPConversion` | 100,088 |
| `BaseFPMoveImmediate` | 100,080 |
| `BaseFPToIntegerUnscaled` | 100,039 |
| `BaseFPToInteger` | 100,075 |
| `BaseIntegerToFPUnscaled` | 100,197 |
| `BaseIntegerToFP` | 100,078 |
| `BaseUnscaledConversion` | 100,306 |
| `BaseUnscaledConversionFromHigh` | 100,024 |
| `BaseUnscaledConversionToHigh` | 100,036 |

- Random phase: 133,413 rounds of 12 instructions, seed 1. Every one of the
  140 records was verified, the least often `FCVTZUUWDr` with 2,409. 7,938
  words were made without the constraints. Each of them decodes to a record
  of the group, which has no constraint.
- Executed phase: the four traces hold 734,599 distinct encodings, 693 of
  them of this group, and all 693 were verified.
- The same run with the seeds 2 and 3: 1,601,553 and 1,601,361 instructions
  verified, no difference.
- "Undefined on one side only": 0. No round met an undefined word: every
  round ended at its budget.
- The start of a round: FPCR takes each of the four rounding modes, and
  flush-to-zero, default NaN and alternative half precision each one time in
  four. FPSR is clear in half of the rounds. A vector register holds zeros,
  denormals, infinities, quiet and signalling NaNs, the powers of two where a
  conversion saturates, ties and random bits, in the layouts of all three
  formats, with zeros or other bits above them.
- A round is short because FPSR only collects. After a few instructions most
  of its bits are set, and an instruction that sets one more or one less no
  longer shows.
- QEMU runs FP only with CPACR_EL1.FPEN set, and its gdbstub writes no system
  register. The leader lets QEMU run a prelude of three instructions that sets
  it, once.

That the comparison finds an FP difference was checked with two bugs put in
by hand, each run with 3,000 instructions per class. Without FPSR.IDC on a
flushed operand the run stopped at `FCVTSDr` (FPSR `0x8` against `0x88`).
With the NaN of a fused multiply-add taken from the factors first it stopped
at `FMSUBSrrr` (V22 `0x7fffffff` against `0xffffffff`).

`just ci::check` replays `tests/corpus/c4.txt` without QEMU: three verified
instructions of each record, with the vector and FP registers.

### The speed of the loop

`taskset -c 30 just tcg-a64-mips` with the helper calls in the loop, two
runs of 3 repeats: medians of 96.22 and 95.84 MIPS, against 97.34 in card C1.
That is about 1 % lower. A block without a helper call runs the same loop of
packed instructions as before, after one test for calls. The loop has no FP
benchmark yet: card C5 measures CoreMark.

### What this card does not do

- The check that FP is enabled (CPACR_EL1.FPEN) and its trap. The FP lifters
  run as if it were. It belongs to the exception card (C6), which must add it
  to every lifter of `lift/fp.rs`.
- `MRS` and `MSR` of FPCR and FPSR (C6). Until then nothing in a guest
  changes FPCR. The lockstep sets it through the gdbstub.
- The other bits of FPCR. The rounds set RMode, FZ, DN and AHP only. The trap
  enables, FZ16 and the AArch32 fields are not written, and FPSR.QC is only
  carried.
- Half-precision arithmetic (`FullFP16`) is not in the 3d set. Half precision
  is here as a format of `FCVT` only.
- The FP and vector forms of the loads and stores are card C3, and AdvSIMD is
  card C7.

### Sources read for this card

- S1: the generated decode tables, for the records, their fields and their
  fixed bits.
- S3: dynarmic `src/dynarmic/common/fp` (`process_nan.cpp`, `unpacked.cpp`,
  `op/FPMulAdd.cpp`, `op/FPToFixed.cpp`, `op/FPRoundInt.cpp`,
  `op/FPConvert.cpp`, `op/FPCompare.cpp`), 0BSD, checked in its
  `LICENSE.txt`. No code of it is copied. Where it and the lockstep
  disagreed the lockstep decided: dynarmic calls every negative operand of a
  conversion to an unsigned integer invalid, and QEMU rounds first.
- S5: SoftFloat 3e (the `ARM-VFPv2` specialisation is now vendored, with its
  original headers) and Berkeley TestFloat, both BSD-3-Clause.
- S6: the behaviour of a running QEMU through its gdbstub and the plugin.
- S2, S4, S7 and S8 were not opened. The Arm manual was not opened (Q1).
## Card C3 (2026-11-27)

Scope: loads and stores on a flat address space, the 49 format classes of the
group (578 records): every addressing mode with its write back, the pairs, the
literal, the unscaled and unprivileged forms, the exclusives, `LDAR` and
`STLR`, the FP and vector register forms, `LD1` to `LD4` and `ST1` to `ST4` of
whole registers, of one lane and replicated, and the prefetches.

### Instruction lockstep against QEMU

`just tcg-a64-lockstep loadstore 100000 1 "<the four traces>" <corpus> 1`, at
the commit `0c534006`: 5,408,033 instructions verified in 3,181,492 rounds, no
difference, 1,256 s. An instruction is verified when both sides ran it from
the same registers, wrote the same bytes to memory, and had the same registers
after it: X0 to X30, SP, PC, CPSR, V0 to V31, FPSR and FPCR.

- Every one of the 49 format classes has at least 100,000 verified
  instructions, the fewest `LoadStore128RO` with 100,182. Every one of the 578
  records was verified, the least often `ST3Threev2s_POST` with 878.
- Memory: 2,051,632 of the verified instructions wrote memory, 20,720,045
  bytes, and every byte was compared.
- Random phase: 2,980,860 rounds, seed 1. 696,460 words were made without the
  constraints.
- Executed phase: the four traces hold 734,599 distinct encodings, 180,209 of
  them of this group. All 180,209 were verified. Each ran alone, with its base
  register pointing into the data. Where its address was not aligned that
  way, it ran again at another place.
- Exclusive stores: 108,857 wrote and 206,357 did not, with the same result
  and the same bytes on both sides.
- "Undefined on one side only": 0. In 1,572 rounds both sides took the
  Undefined Instruction exception at the same word.
- How the rounds ended: 1,512,017 at a load or store that reaches QEMU's flash
  or devices, 862,759 at a load or store outside the memory on both sides,
  624,931 at an access that is not aligned to its size on both sides, 180,209
  at the zero word behind the image, 1,572 undefined on both sides, and 4 at a
  store into the code of the round.

How a round of this group differs from a round of C1:

- QEMU first runs a prelude that sets `CPACR_EL1.FPEN` and clears
  `SCTLR_EL1.SA` and `SA0`. The gdbstub cannot write a system register. Our
  side has neither register yet and behaves as with those values.
- The plugin runs with `vectors=on`. The vector registers are set once, at
  random, and then carry over from round to round on both sides.
- A round is a straight run of 48 instructions on registers that mostly point
  into 64 KiB of data, which is loaded once. It ends at the first exception.
  A zero word behind the image ends a round that got that far.
- An exclusive load is followed by an exclusive store at the same base three
  times out of four, and one such store in four is of another size or kind.

What the run does not compare:

- A round that ends at a device of QEMU. Our memory is the RAM only. Where a
  register is a small number and is used as a base, QEMU reads its flash and
  runs on, and our side stops with a data abort. The round ends there, and the
  instructions before it count.
- Which exception QEMU takes at a failed access, and its syndrome. Both sides
  must stop at the same instruction with the registers unchanged. Card C6
  compares the class, `ESR_EL1` and `FAR_EL1`.
- A store into the code of the round (4 rounds). QEMU goes on with the block
  that it has translated and runs the old word. Our side runs the new word.
  A CPU may do either until the guest synchronizes its caches (DESIGN.md U3).
- Encodings that the architecture leaves open (DESIGN.md U2):
  `stream::unpredictable` lists them, and the generator leaves them out.
- More than one CPU (DESIGN.md U5). `tests/loadstore.rs` has the stress test
  of the exclusives on two threads: each adds one to a shared counter 20,000
  times through `LDXR` and `STXR`, and the counter ends at 40,000.

`just ci::check` replays `tests/corpus/c3.txt` without QEMU: one verified
instruction of each of the 578 records from this run, with the registers, the
vector registers and the memory that QEMU had before and after. An exclusive
store that wrote is not in the corpus, because alone it does not write.

### What the run showed about QEMU, and what the core does about it

Each line is a probe on QEMU 11.1.3 (`a64_lockstep --probe`), kept as a test
in `tests/loadstore.rs`.

- **Alignment with the MMU off.** QEMU takes an exception for every load and
  store whose address is not a multiple of its size, with `SCTLR_EL1.A` set or
  clear. The size is that of one register (16 for a Q register), or of one
  element for `LD1` to `LD4` and `ST1` to `ST4`. The core does the same on the
  flat address space (`Stop::Alignment`). Card C8 makes it depend on the
  memory that an address maps to. Card C5 must know it: a flat image that
  makes an access which is not aligned stops on both sides.
- **`LDAR` and `STLR`.** QEMU takes the Undefined Instruction exception unless
  the bits 20 to 16 and 14 to 10 are ones. `tables/constraints.toml` has the
  two conditions. For `LDXR` and `STXR` QEMU ignores the same bits.
- **Exclusives on one CPU.** An exclusive store writes when its address is
  that of the last exclusive load and the memory still holds the value of
  that load, read at the size of the store. The sizes need not be the same. A
  store that is smaller than the value of its load writes where the low bytes
  agree and reports failure unless the whole value agrees. A pair of 64-bit
  registers compares its upper half with the upper half of the last 16-byte
  exclusive load. A store at another address reads no memory and checks no
  alignment. A store at the marked address is an access and checks its
  alignment. "No mark" is the address 2^64 - 1, so a store there is an access
  too. An exception keeps the mark. The core follows all of it (DESIGN.md
  U1). With more CPUs it also fails a store when another CPU wrote to the
  page of the load, which QEMU does not when the value is the same again.
- **The plugin reports an exclusive store that fails its compare as a write**
  of the bytes that were there. The follower compares what the memory holds
  after an instruction, not the two lists of writes.
- **QEMU runs ahead** of the records that the follower has read. Where a
  round ends without an exception of QEMU, the follower takes the memory
  writes that QEMU made since, reads its vector registers, and runs
  `LDXP` and `STXP` on both sides to make the exclusive monitors the same.

### The decoder

This card added two constraints to `tables/constraints.toml`, for
`LoadAcquire` and `StoreRelease`. `just tcg-a64-generate-check host` passes on
the regenerated `decode.rs` and `tests/vectors.txt`. `tests/vectors.rs` passes
on the 13,660 vectors.

### The speed of the loop

`taskset -c 30 just tcg-a64-mips`: 95.40, 95.47 and 95.65 MIPS, median 95.47.
Card C1 measured 97.34 on the same image. A block now carries its memory
events and its early exits, and the loop looks at them for every block. The
image has no load and no store, so this is the cost of the change for code
that does not use it.

### Sources read for this card

- S1: the generated tables of this crate (records, fields, fixed bits,
  operand classes). No LLVM source file was opened in this card. The cases of
  `stream::unpredictable` are the soft failures of LLVM's decoder for pairs,
  writing-back loads and stores and exclusives, written from memory of that
  decoder and not read again. They shape the test stream only.
- S6: the behaviour of a running QEMU 11.1.3 through its gdbstub and the
  lockstep plugin.
- S2, S3, S4, S5, S7 and S8 were not opened. No prose chapter of the Arm
  Architecture Reference Manual was opened (the operator's answer Q1 allows
  it). The fields of `CPACR_EL1` and `SCTLR_EL1` in the prelude of the
  lockstep are written from general knowledge of the two registers, and the
  lockstep shows that they have the effect that the prelude wants.
- X1 to X3 hold: no QEMU source, no Arm ASL, XML or pseudocode, no Unicorn.

## Card C5 (2026-11-27)

Scope: the flat aarch64 CoreMark image and the scalar loop in `bench/a64`,
their runner on the interpreter and on QEMU (`bench/micro_a64.py`), the
aarch64 lines of the bench history, and the gate of the design (M6): the
interpreter runs the CoreMark mix at 100 MIPS or more, or it gets a
pre-decoded block form in this card. It was below, and it got the form
(`src/block.rs`).

### The gate

All numbers are from helios, 3 repeats, pinned with `taskset`, QEMU 11.1.3.

| | CoreMark mix, MIPS (median) | Scalar loop, MIPS (median) |
|---|---|---|
| The interpreter of `next` (the slot executor on `u128` slots), commit `820007cd`, CPU 30 | 88.50, 88.51, 88.48 (**88.50**) | 96.10, 95.23, 95.46 (**95.46**) |
| The pre-decoded block form, commit `0053c015`, CPUs 16-19, in the history | 160.76, 167.16, 167.73 (**167.16**) | 168.16, 165.74, 168.81 (**168.16**) |
| QEMU TCG, `-machine virt -cpu cortex-a53`, CPUs 16-19, in the history | 2716.48, 2702.74, 2647.84 (**2702.74**) | 3441.74, 3197.08, 3458.65 (**3441.74**) |

- The first row is the measurement that decided the gate: 88.50 MIPS is
  below 100. It is not in the history, because that commit is not the code
  of this card.
- The CoreMark mix is 245,425,680 instructions in 336 blocks, and the loop is
  300,000,014 instructions (3 N + 14, which the run asserts) in 3 blocks.
- CoreMark's own checksums: the image leaves `0xe714` (list), `0x1fd7`
  (matrix) and `0x8e3a` (state) in X2 to X4 and an error count of 0 in X5,
  and its exit code is 0. The runner compares the four values on the
  interpreter. QEMU ends with the same exit code of 0, which the image
  computes from the same comparison.
- The interpreter's time holds the translation of the blocks. A QEMU sample
  is the wall time of the process less the median startup (0.0303 s, 15 runs
  of the loop image with one iteration). The net time of a QEMU sample is
  0.09 s, so its spread is a few percent.
- QEMU is 16 times faster on the mix and 20 times on the loop. The design
  expects that for 3d (M6) and leaves the bar of D1 to the JIT of 3e.
  `just bench-report` prints the ratio as "BELOW D1".
- The commands: `just bench-run qemu aarch64 1 3 flat-a64`, then
  `just bench-run interpreter aarch64 1 3`.

### What the block form is

A block of the typed IR is lowered once into a stream of 16-byte operations
on a frame of 64-bit words. A scalar of at most 64 bits is one word and a
wider one is two. An operation with a result of one word that needs one word
of each operand has a code of its own, and every other operation runs through
the `u128` evaluators of `vmlab-hv-tcg-ir`, as before. Loads and stores of 1
to 8 bytes, guards and helper calls are operations of the same stream. The
semantics are still only in the lifters: `block.rs` knows the operations of
the graph and nothing of aarch64. The slot executor of `vmlab-hv-tcg-ir`
(`packed`) is unchanged and the riscv path still uses it.

What it does not do, and what is left for speed: the registers are still
copied into the frame before a block and out after it, a block is still looked
up by its PC after every block (no chaining), the four flags are computed at
the end of every block that sets them, and a vector or FP operation runs on
the generic path at about the old speed. The mix of this card has no FP and
no vector instruction, and no MMU: a boot has both, and a softmmu lookup per
access (card C8). So 167 MIPS is the measure of the integer path on a flat
memory, not of a boot.

### The block form computes what the slot executor computed

- `src/block.rs` has a test that builds a graph of one operation for every
  binary and unary operation, slice, concat and select over twelve types, and
  compares the lowered form with `linear::lower` and `Packed::evaluate` on 16
  random values each: 30,481 binary graphs (11,152 of them with a code of
  their own), 14,325 unary graphs (7,345), and the binary operations again
  with a literal operand.
- The tests of the crate pass on it unchanged, among them the corpus of
  cards C1 to C4 with the registers that QEMU had: `cargo test --release -p
  vmlab-hv-tcg-aarch64`, 96 tests.
- Instruction lockstep against QEMU on the block form, commit `c4e7fcde`, no
  difference in any run:

| Run | Instructions verified | Rounds |
|---|---|---|
| `just tcg-a64-lockstep all 100000 3` | 3,347,191 | 73,159 |
| `just tcg-a64-lockstep fp 100000 5` | 1,600,560 | 133,380 |
| `just tcg-a64-lockstep loadstore 100000 5` | 5,226,214 | 2,977,243 |
| `all 1000 3` with the four traces | 584,441, with all 550,472 executed encodings of the group | 301,376 |
| `fp 1000 3` with the four traces | 16,773, with all 693 executed encodings | 1,403 |
| `loadstore 1000 3` with the four traces | 232,673, with all 180,209 executed encodings | 230,587 |

A finding about the harness, not about this card: `just tcg-a64-lockstep all
100000 5` stops after 50,000 rounds with "QEMU wrote no record before the
timeout". It does the same with the executor of `next` (the sources of
`820007cd`), at the same round.

### What the image needed

- No FP and no vector register (`-mgeneral-regs-only`): the vector lifters
  and `CPACR_EL1` come with later cards.
- Aligned accesses only (`-mstrict-align`), as card C3 found for a flat
  memory.
- No linker relaxation (`-Wl,--no-relax`): lld turns an `adrp` and `add`
  pair into a `nop` and an `adr`, and the hint space has no lifter before
  card C6. The interpreter stopped at that `nop` in the first run.
- An end that both sides honour without a system register: the semihosting
  call `SYS_EXIT`. The interpreter stops in front of `hlt #0xf000` because
  `HLT` has no lifter. When card C6 gives `HLT` an exception, `a64_bench`
  must stop at that exception instead.

### Shared code that changed

`FlatMemory::read`, `write` and `range` of `vmlab-hv-tcg` are `#[inline]`
now, so that a load of a known size in another crate is a bounds check and a
move. Nothing else outside this crate and `bench/` changed.

### Sources read for this card

- None of S1 to S8 for semantics: this card wrote no instruction semantics.
  `block.rs` and `exec.rs` hold none.
- S6: the behaviour of a running QEMU 11.1.3, as the lockstep oracle and as
  the yardstick of the benchmark. The semihosting call is from the public
  semihosting specification of Arm (a prose document, S7 in kind).
- CoreMark (Apache-2.0, EEMBC) is fetched at its pinned revision by the bench
  and not committed, as for riscv64.
- X1 to X3 hold: no QEMU source, no Arm ASL, XML or pseudocode, no Unicorn.
