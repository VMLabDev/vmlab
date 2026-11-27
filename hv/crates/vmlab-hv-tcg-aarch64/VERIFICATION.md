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

## Card C6 (2026-11-26)

Scope: exceptions, exception levels and the system registers. New files:
`sysreg.rs` (the register table and the CPU model), `exception.rs` (entry,
return, syndromes, interrupt masks), `timer.rs` (the generic timer and the
clock) and `lift/system.rs` (the twelve format classes of the system
instructions but `SYS` and `SYSL`, which are card C8). `exec.rs` now takes
the exceptions of the guest itself.

### How it is built

- **A terminal instruction.** An instruction that can take an exception,
  change the level or write a system register is the last of its block. The
  block ends in `A64Next::Trap`, and the loop runs the instruction in Rust
  after the register slots are written back. The hints, the barriers, `MRS`
  and `MSR` of NZCV, FPCR and FPSR, and `MRS` of a stored register (through
  the ordered helper `sysreg::READ_STORED`, the first use of R10's ordered
  form) stay inside the block.
- **Exceptions are delivered.** `Stop::Undefined`, `Stop::FetchAbort` and
  `Stop::PcAlignment` are gone. The guest takes the exception to VBAR_EL1,
  and `Cpu::taken` names it. The loop still stops for a hypercall
  (`Stop::Hypercall` with `Conduit::Hvc` or `Conduit::Smc`), for `WFI` with
  no interrupt line high (`Stop::Halt`), and for a record without a lifter.
- **The table.** 153 rows, keyed by the LLVM encoding: 94 of the 101
  measured registers answer, and the 7 PMU registers of that list have rows
  that say why they are absent (V5). The unallocated ID space reads as zero.
  A machine adds registers and instructions through `Cpu::hooks` (G3e, A5).
  An access without a row, and a word that no record matches, are in
  `Cpu::unknown` by name, each once (G3f).
- **The CPU model.** `sysreg::V8_0` is the CPU of G2. `QEMU_CORTEX_A53` is
  the same table with QEMU's ID values, and the lockstep runs our core as it.
- **Interrupts and the timer.** `Cpu::irq` and `Cpu::fiq` are levels that a
  machine drives. The timer is a value in the state, `Cpu::timer_lines`
  gives its two output lines, and `Cpu::timer_deadline` the count at which
  one rises. The counter comes from `timer::Clock`: host time with a base
  that a machine keeps over a pause, or a count that the owner sets.

### The probe of QEMU's system registers

`just tcg-a64-sysprobe` (`examples/a64_sysprobe.rs`) runs `MRS` and `MSR` of
each of the 32,768 encodings with op0 of 2 or 3 on QEMU's `cortex-a53`, at
EL1 and at EL0, from a reset each time. 175 encodings are not undefined. For
each it notes the value, whether a write runs, the syndrome of a trap, and
for 61 named registers the value read back after a write of all ones and of
zero. It also notes which bits of PSTATE an exception return takes from
SPSR_EL1, and CCSIDR_EL1 of each cache. The output is the committed file
`tests/qemu-cortex-a53-sysregs.txt`. A second run of the probe gave the same
file, byte for byte.

`just ci::check` compares the table with that file, without QEMU
(`src/sysreg/tests.rs`): every encoding is run through `MRS` and `MSR` on our
interpreter at EL1 and at EL0, as the 3d CPU and as QEMU's CPU.

- As the 3d CPU, 58 names differ, and they are exactly the allow-list
  `sysreg::QEMU_DIFFERENCES`: MIDR_EL1 and REVIDR_EL1 (the 3d CPU is no
  Cortex-A53), the 16 AArch32 ID registers that read as zero (G2),
  ID_AA64PFR0_EL1 (`0x11` against `0x22`), ID_AA64DFR0_EL1 (no PMU, V5),
  ID_AA64MMFR0_EL1 (the 4K granule only, little-endian only), RMR_EL1, the 28
  PMU registers, and 8 IMPLEMENTATION DEFINED registers of the Cortex-A53.
  The test fails on a difference that is not in the list, and on an entry
  that is no difference. The value that the list gives for QEMU is the value
  of the probe.
- As QEMU's CPU, only the 37 registers that we leave out differ.

### Instruction lockstep against QEMU

The follower now takes the exceptions of a stream on both sides and goes on
at the vector. A round of the group `system` starts from a reset of both
sides (QEMU's monitor command `system_reset`). Before every instruction it
compares X0 to X30, SP, PC, CPSR, V0 to V31, FPSR, FPCR and 72 system
registers (`oracle::compared`), among them ESR_EL1, FAR_EL1, ELR_EL1 and
SPSR_EL1, every stored register that a write changes, the timers and the
stack pointer that is not in use. The plugin sends them with each record
(`sysregs=`).

A stream (`oracle/system.rs`) has a vector table, a prelude that sets
VBAR_EL1, and about 400 instructions: `MRS` and `MSR` of the rows and of
random encodings, the PSTATE forms, the hint space, the barriers, `SVC`,
`HVC`, `SMC`, `BRK`, `HLT`, `DCPS`, `UDF`, `DRPS`, `ERET` with a saved state
that the stream made (EL0, EL1 with either stack pointer, illegal states,
PSTATE.IL), branches to a PC that is not a multiple of four, and integer and
FP instructions of the earlier cards. The handlers return to the instruction
after the fault. A call `SVC #0xe1` from EL0 returns to EL1.

`just tcg-a64-lockstep system 100000 1 "<the four traces>"` on the commit
`0d98e906`: 14,146,918 instructions verified in 15,868 rounds, no difference,
47 s.

| Format class | Instructions verified |
|---|---|
| `ExceptionGeneration` | 140,363 |
| `UDFType` | 192,590 |
| `SpecialReturn` | 937,266 |
| `HintI` | 100,048 |
| `SystemNoOperands` | 195,483 |
| `CRmSystemI` | 116,454 |
| `MRSI` | 2,929,713 |
| `MSRI` | 2,618,170 |
| `MSRpstateImm0_15` | 116,125 |

- Random phase: 15,387 rounds, seed 1. `MRS` ran with 24,624 distinct
  encodings and `MSR` with 25,249. The other 6.8 million verified
  instructions are those of the earlier groups in the streams.
- Exceptions that both sides took, with the same registers after them:
  1,120,189. Undefined instruction 589,314, instruction abort 292,285, `SVC`
  82,733, FP access trap 50,816, trapped system instruction 38,536, PC
  alignment 34,345, `BRK` 18,532, illegal execution state 13,626, trapped
  `WFI` and `WFE` 2. `ERET` ran 927,724 times.
- Executed phase: the four traces hold 495 distinct encodings of the group.
  Each ran alone after a reset. 480 were verified. 1 is `WFI`, and 14 name a
  PMU register that the 3d CPU does not have (V5).
- A directed round runs `WFI` and `WFE` at EL0 with SCTLR_EL1.nTWI and nTWE
  clear: both trap, with the syndromes `0x07e00000` and `0x07e00001`.
- "Undefined on one side only": 0.
- QEMU leads in 9,617 verified instructions: the value of a counter read
  (CNTVCT_EL0, CNTPCT_EL0 and the two timer values) and the answer of a host
  call come from its next record (O4).
- The seeds 2 and 3, without the traces: 14,141,960 and 14,143,307
  instructions verified, no difference.
- With the follower of this card the earlier groups still agree:
  `fp 5000 1` 80,184 instructions, `all 5000 3` 167,998, no difference.
- 71 rounds ended before a write that the lockstep does not follow
  (`oracle::system::unfollowed`): SCTLR_EL1.M and TCR_EL1.TBI (card C8),
  MDSCR_EL1.SS, KDE and MDE (V3), and a timer compare value near the count
  of the host. 72 ended at a load, a store or an AdvSIMD record that has no
  lifter yet.

What the lockstep found, each now in the code with the reason:

- QEMU clears N, Z, C and V at an exception entry. We follow it (P2, U1).
- QEMU reports op1 in the place of op2 and op2 in the place of op1 in the
  syndrome of a trapped `MSR DAIFSet` or `MSR DAIFClr` at EL0. We follow it.
- After an exception return to a state that does not exist, QEMU keeps bit
  13 of SPSR_EL1 in PSTATE. `State::pstate13` holds it. It does nothing.
- A write of DBGBCR<n>_EL1 stores bit 5 also in bit 6 and bit 7 also in bit 8.
- DBGCLAIMSET_EL1 and DBGCLAIMCLR_EL1 are one set of eight claim tags.
- A PC above 40 bits takes an address size fault of level 0 while the MMU is
  off, not an external abort.
- `WFE` at EL1 stops QEMU as `WFI` does. Our core runs it as a NOP, which
  the architecture allows. The streams hold neither.
- QEMU names the instruction after an `SVC` as the source of its exception.
- CCSIDR_EL1 of the instruction cache and of the level 2 cache are now the
  values of the probe.

`just ci::check` replays `tests/corpus/c6.txt` without QEMU: 363 verified
instructions with the system registers before and after them, one for each
row that `MRS` and `MSR` reached and three for each other record. 92 unit
tests cover the rest, among them every row of the table against the probe, the
interrupt lines and their masks, `WFI`, the timer, the hooks of a machine,
and 40 streams run on our interpreter alone.

### What this card does not do

- Interrupts and the timer are not in the lockstep: O3 has no device and no
  interrupt. Unit tests cover the masks, the vectors, `WFI` and the timer
  lines. kvm-unit-tests come with card C10. When both lines are high and not
  masked, our core takes FIQ first.
- `SYS` and `SYSL` (cache operations, TLBI, AT, DC ZVA) stop the loop with
  `Stop::Unimplemented`: card C8.
- Translation: the PC is a physical address. A fetch outside the memory is an
  external abort (fault status `0x10`), or an address size fault above 40
  bits. The counts above do not separate the two.
- The debug registers are stored and nothing acts on them (V3). MDSCR_EL1.SS
  does not step.
- A return to AArch32 at EL0 is illegal on our CPU (G2) and legal on QEMU's
  `cortex-a53`. The streams do not make one.
- RMR_EL1 does not exist: a reset is a request of the machine.
- `SMC` is undefined on QEMU's `virt` without EL3. Our core leaves it to the
  machine, as `HVC` (G4). In the lockstep the harness refuses the call where
  QEMU did, and takes QEMU's answer where it served one.
- The speed of the loop was not measured again: card C5 measures it.

### After the merge of card C3 (2026-11-27)

Card C3 (loads and stores) reached `next` first, and this branch took it in.
How the two fit:

- The loop is that of card C3 (blocks with several exits, the exclusive
  monitor, the code pages) with the exceptions, the interrupts and the
  terminal instruction of this card. `ERET` and `CLREX` end the exclusive
  mark, as the record of card C3 expected of this card.
- A load or a store that fails still stops the loop (`Stop::DataAbort`,
  `Stop::Alignment`), as card C3 left it. It is not an exception of the
  guest yet: the syndrome of a data abort needs the lockstep of the
  translation card (C8).
- The follower has two kinds of round. A round that ends at the first
  exception and compares the memory writes is the one of card C3
  (`run_plain`). Our CPU now takes its exceptions itself, so that round puts
  the registers back as they were before the exception. A round of system
  instructions (`run_system`) is the one of this card. It ends before a load
  or a store.
- The programs of `tests/loadstore.rs` ended in a zero word, which stopped
  the loop as a record without a lifter. `UDF` is now an exception of the
  guest, so they end in `IC IALLU`, which has no lifter before card C8.

On the merged commit `98d30c68`:
`just tcg-a64-lockstep system 100000 1 "<the four traces>"` verified
14,144,660 instructions in 15,867 rounds with no difference, and 68 rounds
ended before a load or a store. `c1 100000 1` without the traces verified
1,439,552 instructions, `loadstore 3000 1` 156,639, `fp 3000 1` 48,156,
`c2 3000 1` 57,991 and `all 3000 3` 101,049, each with no difference.

### Sources read for this card

- S1: the generated tables (`generated/decode.rs`, `generated/sysreg.rs`)
  for the records, the fields, and the names and encodings of the registers.
  No LLVM source file was opened.
- S6: the behaviour of a running QEMU through its gdbstub (with the monitor
  command `system_reset`), the plugin, and the plugin API header
  `qemu-plugin.h` (the discontinuity callback and the register calls).
- S2, S3, S4, S5, S7 and S8 were not opened. The Arm manual was not opened
  (Q1). The first version of each file came from the author's general
  knowledge of the A64 exception model, and the probe and the lockstep
  settled every value that is in the code.
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
  `HLT` has no lifter. Since card C6 is merged, `HLT` takes the Undefined
  Instruction exception, and `a64_bench` stops at that exception: it puts an
  `hvc` at the vector, which stops the loop, and takes the two instructions
  out of its count.

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

## Card C7 (2026-11-26)

Scope: AdvSIMD and the crypto instructions, the 54 format classes of the 3d
set that had no lifter and are not system instructions (1,426 records). 47
classes are AdvSIMD (1,412 records, with the four `PMULL` records of the `AES`
set): three registers of one arrangement, two-register miscellaneous, across
lanes, scalar pairwise, three registers of two arrangements, indexed element,
shift by immediate, copy, permute, extract, table lookup and modified
immediate, each for vectors and for scalars. 7 classes are `AESE`, `AESD`,
`AESMC`, `AESIMC` and the ten instructions of SHA-1 and SHA-256 (14 records).

### How it is built

- **One lane helper** (`helpers/simd.rs`, DESIGN.md G3d). `eval` runs one of
  136 operations over the lanes of up to three 128-bit values. A lifter
  passes the operation, the lane size and the lane count as one constant. It
  is pure: FPCR is an argument and the bits for FPSR come back beside the
  value, with QC among them. A result of 64 bits or fewer is one call
  (`SIMD_LOW`, 72 bits). A 128-bit result that can raise a bit is two calls
  with the same arguments (`SIMD` and `SIMD_FLAGS`), because a scalar of the
  graph has at most 128 bits. A result that cannot raise a bit is one call.
- **The lifters read the instruction word** (`lift/simd.rs`). One encoding
  group of AdvSIMD is many format classes of the LLVM tables, so a `plan`
  function turns `U`, `size` and the opcode of a word into an operation.
  `lift/mod.rs` puts the word back from the fixed bits of the record and the
  fields of its class, and `lift` keeps its signature. A unit test holds the
  plan of each of the 1,323 records of the 27 planned classes against the
  mnemonic in its LLVM name.
- **Pure IR** for what needs no lane loop: `AND`, `BIC`, `ORR`, `ORN`, `EOR`,
  `BSL`, `BIT`, `BIF`, `NOT`, `DUP`, `INS`, `SMOV`, `UMOV`, the scalar `MOV`
  of a lane and the modified immediates.
- **Two calls for an accumulation**: `MLA`, `SABA`, `SSRA`, `SMLAL`,
  `SQDMLAL`, `SADALP` and their kin are the inner operation and then an add,
  a subtract or a saturating add on `Rd`. `FMLA` and `FMLS` are one fused
  operation.
- **The FP operations that only AdvSIMD has** (`helpers/fp/vector.rs`):
  `FMULX`, `FABD`, `FRECPS`, `FRSQRTS`, `FRECPE`, `FRSQRTE`, `FRECPX`, the
  compares that give a mask, and `FCVTXN`. `FRSQRTS` is `1.5 + (a / 2) * b`
  as one SoftFloat fused multiply-add, where half of one operand is exact.
  `FCVTXN` is SoftFloat's conversion toward zero with the last bit set where
  it is inexact.
- **Crypto** (`helpers/crypto.rs`): the S-box is computed at compile time
  from its definition. The unit tests compute the first round of the example
  of FIPS 197 and the SHA-1 and SHA-256 digests of "abc" with the functions
  of the instructions, in the order in which a guest uses them.

### Instruction lockstep against QEMU

`just tcg-a64-lockstep c7 100000 1 "<the four traces>" <corpus> 1`, at the
commit `bcc6090f`: 5,405,721 instructions verified in 450,498 rounds, no
difference, 137 s. An instruction is verified when both sides ran it from the
same X0 to X30, SP, PC, CPSR, V0 to V31, FPSR and FPCR and had the same of
all of them after it.

- Every one of the 54 format classes has at least 100,000 verified
  instructions, the fewest `SIMDSMov` with 100,001. Every one of the 1,426
  records was verified, the least often `FABDv4f32` with 249:
  `BaseSIMDThreeSameVector` has 335 records.
- Random phase: 450,252 rounds of 12 instructions, seed 1. 27,363 words were
  made without the constraints. No class of the group has a constraint.
- Executed phase: the four traces hold 734,599 distinct encodings, 2,697 of
  them of this group, and all 2,697 were verified.
- "Undefined on one side only": 0. No round met an undefined word: every
  round ended at its budget.
- The same run with the seeds 2 and 3, without the traces: 5,402,940 and
  5,403,108 instructions verified, no difference.
- Longer runs of the four classes with the most records, seed 4, by the
  name of the class as the group: `BaseSIMDThreeSameVector` 3,000,000,
  `BaseSIMDTwoSameVector` 1,500,000, `BaseSIMDVectorShift` 1,000,008 and
  `BaseSIMDDifferentThreeVector` 900,000 instructions verified, no
  difference.
- The start of a round: FPCR and FPSR as in card C4. Each vector register has
  lanes of one size, 8 to 64 bits, and a lane holds zero, all ones, the ends
  of the signed range, a small signed number or one near the lane size (the
  amount of a shift by a register), an FP value where the rules change, or
  random bits.
- QEMU runs these instructions only with CPACR_EL1.FPEN set. The leader sets
  it with its prelude, as for card C4.

No lifter and no helper needed a change after its first lockstep run. That
the comparison finds a difference was checked with two bugs put in by hand,
each run with 2,000 instructions per class. With `TRN2` reading the lanes of
`TRN1` the run stopped at `TRN2v2i32` (V0 differed). Without FPSR.QC it
stopped at `UQADDv1i32` (FPSR `0x10` against `0x8000010`).

`just ci::check` replays `tests/corpus/c7.txt` without QEMU: one verified
instruction of each of the 1,426 records from the run with the seed 1, with
the vector registers that the instruction names.

### What this card does not do

- The check that FP and AdvSIMD are enabled (CPACR_EL1.FPEN) and its trap
  came with card C6. After the merge of `next`, `lift::needs_fp` names the
  format classes of this card too, so they trap as the scalar FP classes do.
- The speed of AdvSIMD. Every lane operation is a helper call, and a 128-bit
  result that can raise a bit of FPSR is computed twice. The loop of
  `just tcg-a64-mips` has no vector instruction and was not measured again:
  this card changes no line of `exec.rs`.
- Encodings that the LLVM tables do not decode. The stream is made from our
  decode tables, so a word that QEMU runs and LLVM refuses is not in it. The
  decoder check of card C1 covers that side.
- Half-precision lanes (`FullFP16`), `RDM`, `DotProd`, `SHA3`, `SHA512` and
  the other later sets are not in the 3d set.

### Sources read for this card

- S1: the generated tables of this crate (records, fields, fixed bits,
  names). No LLVM source file was opened. The opcode tables of the `plan`
  functions are written from the author's knowledge of the published A64
  encoding and checked against the record names.
- S3: dynarmic at the commit `65a221dd` of the fork
  `github.com/PabloMK7/dynarmic`, 0BSD, checked in its `LICENSE.txt`:
  `src/dynarmic/common/fp/op/FPRecipEstimate.cpp`, `FPRSqrtEstimate.cpp`,
  `FPRecipExponent.cpp`, `FPRecipStepFused.cpp`, `FPRSqrtStepFused.cpp` and
  `src/dynarmic/common/math_util.cpp`, for the two estimates, `FRECPX` and
  the two steps. No code of it is copied. No other file of it was opened: the
  translator files `simd_*.cpp` were listed and not read.
- S5: SoftFloat 3e, as vendored by card C4.
- S6: the behaviour of a running QEMU 11.1.3 through the lockstep harness.
- FIPS 197 and FIPS 180-4, and the published definitions of the integer
  AdvSIMD, AES and SHA instructions, from the author's memory. These are not
  among S1 to S8. The known answers of the standards and the lockstep check
  what was written from them.
- S2, S4, S7 and S8 were not opened. No chapter of the Arm Architecture
  Reference Manual was opened (the operator's answer Q1 allows its prose).
- X1 to X3 hold: no QEMU source, no Arm ASL, XML or pseudocode, no Unicorn.

## Card C8 (2026-11-27)

Scope: translation and the softmmu. New files: `mmu.rs` (the stage-1 walk
with its parameters as data, the permissions, the faults, and the TLB of one
CPU), `vmlab-hv-tcg/src/tlb.rs` (the generic softmmu TLB, R12),
`oracle/translate.rs` (the streams with the MMU on) and `tests/translate.rs`.
`exec.rs` now translates every fetch, load and store, takes the data and
instruction aborts of the guest itself, and runs `SYS`.

### How it is built

- **The walk is a pure function** (`mmu::walk`): the control registers as a
  value (`Controls`), a virtual address, and a function that reads a
  descriptor. The granule (4K, 16K, 64K), the size of each half and the
  output size are numbers, and `geometry` gives the first level and the bits
  of each level from them (A4). The regime is a value (`Regime`, G4): EL1&0
  is the one that exists.
- **The TLB is generic** (`vmlab_hv_tcg::tlb`): direct-mapped, one 4K page
  per entry, tagged with an ASID unless the mapping is global, with the size
  of the guest mapping that an invalidation by address takes whole. It knows
  no architecture, so riscv 3b can use it. A CPU has one for each regime. A
  fault is never kept. A new table base or ASID drops nothing. A write of
  SCTLR_EL1, TCR_EL1 or MAIR_EL1 drops everything.
- **Every access goes through it, also with the MMU off**: the identity
  mapping is an entry like another, of Device memory. The fast path of a
  load or a store is one lookup, a permission bit and the read or write.
- **Aborts are exceptions of the guest.** `Stop::DataAbort` and
  `Stop::Alignment` are gone. `Exception::DataAbort` carries the fault status
  code, the direction, the cache bit and the address for FAR_EL1.
- **Alignment is decided where the guard fails**, because it depends on the
  memory that the address maps to.
- **`LDTR` and `STTR`** are made as EL0. A tagged address translates with
  TCR.TBI, and a branch to a register and `ERET` drop the tag.
- **`SYS`**: `DC ZVA`, the cache operations (which do nothing: the blocks
  follow the memory by their stamps), `AT S1E1R/W` and `S1E0R/W` into
  PAR_EL1, and the TLBI operations of Armv8.0 for the regime, in the forms
  for this CPU and for all of them. Another operation is undefined and is in
  `Cpu::unknown` by name (G3f).
- **Self-modifying code through the code stamps.** A block is kept under its
  virtual PC, its context and the physical address of its code, with the
  code stamp of the memory for its bytes. A store of this CPU into a page of
  translated code, or a write of anything else, starts a new epoch, and a
  block of an older epoch is used again only when its stamp still holds. A
  write into a data page no longer drops the blocks. A write of another CPU
  is seen at the start of `run` and `step` and at every exception and
  exception return.
- **No hardware update of the access flag, and no dirty state.** The CPU is
  Armv8.0, as QEMU's `cortex-a53`: a descriptor with the access flag clear
  faults, and the guest sets the flag.

### Instruction lockstep with the MMU on

A round of the group `mmu` starts from a reset of both sides. The stream
(`oracle/translate.rs`) builds translation tables for the 4K or the 64K
granule in each half, with any size of a half, any output size, TBI, both
sizes of an ASID, EPD and random MAIR, turns the MMU on, and runs about 260
items: loads and stores of every record of card C3 at addresses that the
tables map, do not map or map without the permission (at the start of a
page, at its end where the access crosses into the next, with and without a
tag), calls through a mapped page of `RET`, `AT` with a read of PAR_EL1,
`DC ZVA` and the other cache operations, TLBI, any `SYS` and `SYSL`, a new
descriptor for a mapped address with its TLBI, a new ASID, MAIR or SCTLR_EL1
(with WXN, A, UCI, DZE, and the MMU off and on again), returns to EL0 and to
EL1, and integer instructions between them. The mapped addresses are pages
next to each other and blocks, with any descriptor: valid or not, the access
flag, AP, UXN, PXN, nG, any attribute index, the ignored bits, an output
address in the data, beyond the RAM or beyond the output size, and on the
way to it a missing table, a table beyond the output size or beyond the RAM,
a block where none exists, and APTable, UXNTable and PXNTable.

The follower compares, before every instruction, X0 to X30, SP, PC, CPSR, V0
to V31, FPSR, FPCR and the 72 system registers of card C6, among them
ESR_EL1, FAR_EL1, ELR_EL1, SPSR_EL1 and PAR_EL1, and the memory writes of
each instruction.

`just tcg-a64-lockstep mmu 100000 <seed>` on helios, QEMU 11.1.3
`-cpu cortex-a53`:

| Seed | Instructions verified | Rounds | Halves 4K / 64K | `SYS` verified | Loads and stores | Bytes written and compared | Result |
|---|---|---|---|---|---|---|---|
| 1 | 4,403,165 | 1,155 | 1,169 / 1,141 | 100,008 (4,129 distinct) | 117,604 | 156,024 | no difference, 28 s |
| 2 | 4,457,567 | 1,164 | 1,143 / 1,185 | 100,010 (4,161 distinct) | 117,311 | 153,890 | no difference, 27 s |
| 3 | 4,454,408 | 1,168 | 1,123 / 1,213 | 100,060 (4,148 distinct) | 117,600 | 154,598 | no difference, 28 s |

Exceptions that both sides took in the run of seed 1, with the same
registers after them: data aborts 18,754 translation, 19,054 permission,
9,223 access flag, 16,165 address size, 12,506 alignment, 9,359 external and
7,167 external on a table walk. Instruction aborts 7,737 translation, 3,012
permission, 1,946 access flag, 4,102 address size, 2,251 external and 1,582
external on a table walk. 2,624 trapped system instructions, 13,871 `SVC` and
63,912 undefined instructions (among them the 4,166 `SYSL` and the `SYS`
operations that the CPU does not have). Every level from 0 to 3 is among the
syndromes.

### The walk test for 16K and 64K

`just tcg-a64-lockstep walk 100000 <seed>` runs the same streams for the 16K
and the 64K granule against QEMU's `-cpu max,sve=off,sme=off`. Our side runs
as the model `sysreg::QEMU_MAX_WALK`: 16K, 64K, 48 bits of physical address.
The streams stay inside what an Armv8.0 walk and the walk of that CPU agree
on (`Profile::strict`): sizes of a half from 16 to 39, an output size of at
most 48 bits, no bit of TCR above TBI1, no reserved descriptor bit, no block
where a later extension allows one, the MMU on all the time, no exclusive
access, no `DC ZVA`, and no other `SYS`. The comparison leaves out the
control registers and the bits of PSTATE that an Armv8.0 CPU does not have.

| Seed | Instructions verified | Rounds | Halves 16K / 64K | Loads and stores | Result |
|---|---|---|---|---|---|
| 1 | 4,454,798 | 1,099 | 1,096 / 1,102 | 115,891 | no difference, 22 s |
| 2 | 4,419,636 | 1,092 | 1,101 / 1,083 | 115,029 | no difference, 23 s |

The 3d CPU itself has the 4K granule only (G2). The 16K and 64K walks are
the same code with other numbers, and this test is their proof.

### What the lockstep found on QEMU, and what the core does about it

Each line was a difference of a run, and is now in the code with the reason.

- PAR_EL1 after a translation has bit 9 set (not secure) and bit 11. Device
  memory and the attribute bytes 0x44 and 0x40 read as outer shareable.
- `AT` takes a data abort when its walk meets a descriptor that the memory
  does not hold, with the cache bit and "write" in the syndrome. Every
  other fault of its walk is in PAR_EL1.
- `DC ZVA`: a translation, access flag or permission fault names the address
  that the instruction gave, and an external abort the first byte of the
  block. The syndrome says "write" and has no cache bit.
- A size of a half outside 16 to 39 translates nothing: a translation fault
  of level 0.
- The address bits of TTBR reach below bit 12 for a first table that is
  smaller than 4K.
- TCR.TBID0 and TBID1 (the bits 51 and 52) and TCR.HPD0 and HPD1 (41 and 42)
  belong to later extensions, and QEMU's `cortex-a53` acts on them: a branch
  keeps its tag, and a table descriptor takes no permission away. We follow
  QEMU (P2, U1). A guest for an Armv8.0 CPU leaves them clear.
- Alignment. With the MMU off, or with SCTLR_EL1.A, the fault comes before
  every other one. In Device memory it comes after the faults of the walk
  and before the permission fault, the page of the first byte decides, and
  it asks for the size of the register, also for one of 16 bytes. A load or
  a store of whole vector registers as a structure (`LD1` to `LD4`, `ST1` to
  `ST4`) takes none in Device memory. The forms of one lane and the
  replicating ones do. The alignment fault of an exclusive store says
  "read".
- A pair of general registers is one access: where it crosses into a page
  that faults, that fault comes before the memory of the first page is
  looked at, and no register is written. A pair of vector registers is two
  accesses, and a load has written the first register when the second
  faults.
- `LD1` and `ST1` of whole registers are accesses of 8 bytes, and a load has
  written what it loaded when a later access faults. The upper half of a
  register of 8 bytes is cleared after the last access.
- QEMU keeps a translated block under its physical address and runs it at
  every virtual address that maps there, and the plugin then names the
  address of the first one. The streams run code at one address only.
  `tests/translate.rs` has the two cases with code at two addresses.

### What the lockstep does not compare

- **What a TLB keeps** (DESIGN.md U3). After a write to a descriptor or to a
  control register a stream runs a TLBI that names every ASID. QEMU drops
  its whole TLB at a write of TTBR, and our TLB keeps the entries of another
  ASID. Whether a TLBI with an ASID leaves the entries of another ASID is in
  the unit tests (`tests/translate.rs`, `mmu.rs`, `tlb.rs`).
- **The registers of a load of several vector registers that faulted**: 4,
  5 and 11 such loads in the three runs, and 11 in each walk run. QEMU
  writes them element by element, and our side register by register (`LD1`,
  the forms of one lane and the replicating ones) or not at all (`LD2` to
  `LD4`). The architecture leaves those registers open. Our registers take
  QEMU's there, and the round goes on.
- **A store of several parts that faulted after another count of parts**: 0
  in every run.
- **A round that reaches QEMU's flash or devices** (3 rounds of seed 1): our
  memory is the RAM.
- **The stack pointer alignment check** (SCTLR_EL1.SA and SA0) is not built.
  The streams clear both bits and use no stack pointer as a base.
- **Big-endian data** (SCTLR_EL1.EE and E0E): the 3d CPU is little-endian
  only (G2).
- **More than one CPU** (U5): a TLBI for all CPUs acts on this one. A machine
  with more CPUs hands it on (card C9).

### The other groups still agree

With the softmmu under every access, on the last commit of this card:
`loadstore 20000 1` verified 1,045,883 instructions (395,881 of them wrote
4,035,686 bytes, every byte compared), `system 100000 1` 14,143,805, `c1 20000
1` 288,490, `c2 5000 1` 95,746, `fp 5000 1` 80,184, `c7 3000 1` 162,456 and
`all 3000 3` 101,049, each with no difference.

### Without QEMU

`just ci::check` replays `tests/corpus/c8.txt`: 244 verified instructions of
the run of seed 1, two for each outcome (no exception, or the class and the
low byte of ESR_EL1) of a load or a store, of a system operation and of the
other instructions. An entry names its memory by physical address (`p=`):
the word of the instruction, the descriptors of its walks and its data. A
replay puts them into an empty memory, runs the instruction with the MMU
on, and compares the registers, ESR_EL1, FAR_EL1, ELR_EL1, SPSR_EL1,
PAR_EL1 and the bytes written.

The unit tests: the shape of a walk for each granule, a page and a block of
each granule in both halves, each fault with its level, the permissions with
the table bits and WXN, the controls (ASID, tags, output size), PAR_EL1, the
TLB (`mmu.rs`, 10 tests, and `tlb.rs`, 3 tests). 14 programs with the MMU on
(`tests/translate.rs`): each fault of an access with ESR_EL1 and FAR_EL1, an
access that crosses pages, `LDTR` and `STTR`, alignment, execution, tagged
addresses, `DC ZVA`, `AT`, every TLBI with and without the ASID of the
entry, the cache operations, code written through a second address, an
address that is mapped to other code, and a guest that takes an abort,
repairs the descriptor and returns to the access. A test of
`oracle/translate.rs` runs 24 streams on our interpreter alone, through
every kind of fault.

### Speed

`taskset -c 16-19 just bench-run interpreter aarch64 1 3` on helios, the flat
images of card C5, which run with the MMU off:

| | CoreMark mix, MIPS | Scalar loop, MIPS |
|---|---|---|
| Card C5, commit `0053c015` | 160.76, 167.16, 167.73 (**167.16**) | 168.16, 165.74, 168.81 (**168.16**) |
| This card | 142.84, 138.69, 137.21 (**138.69**) | 142.32, 141.59, 142.43 (**142.32**) |

The softmmu costs 17 % on the mix and 15 % on the loop. The loop has no load
and no store: its cost is the lookup of the code page and the check of the
block (physical address and epoch) before every block. Both stay above the
gate of the design (M6: 100 MIPS). The two lines are not in the bench
history. What is left for speed: the code page of the last block could be
kept beside the TLB, and a pair of general registers is one access of 16
bytes on the path of the wide accesses, not two inlined ones.

### What this card does not do

- EL2 and stage 2: the regime is a value, and only EL1&0 exists (G4).
- 52-bit addresses, the contiguous bit as a hint, and the hardware update
  of the access flag (Armv8.1): not in Armv8.0.
- The `Memory` trait still has no host pointer: a TLB entry holds a physical
  page, and an access is one call of the memory.
- A machine with more CPUs must hand a TLBI for all CPUs to the others, and
  a `DC ZVA` of another CPU into our code is seen at our next exception.

### Sources read for this card

- S1: the generated tables of this crate, for the `SYS` records and the
  names and encodings of the `DC`, `IC`, `AT` and `TLBI` operations.
- S6: the behaviour of a running QEMU 11.1.3 through the lockstep, as
  `cortex-a53` and as `max`. Every rule of "What the lockstep found" was a
  difference of a run first.
- The layout of a translation table descriptor, of TCR_EL1, TTBR, MAIR_EL1,
  PAR_EL1 and of the data and instruction abort syndromes, and the rules of
  AP, UXN, PXN, WXN and the table bits, are written from the author's
  knowledge of the published architecture. They are not among S1 to S8, and
  the lockstep checks what was written from them.
- S2, S3, S4, S5, S7 and S8 were not opened. No chapter of the Arm
  Architecture Reference Manual was opened (the operator's answer Q1 allows
  its prose).
- X1 to X3 hold: no QEMU source was opened, no Arm ASL, XML or pseudocode,
  no Unicorn. Some guesses about the order of two faults came from the
  author's general knowledge of how an emulator of this kind is built. A
  guess stayed in the code only where a run of the lockstep confirmed it,
  and the runs refuted several.

## Card C10 (2026-11-27)

Card C10 is the `virt` machine: `vmlab-hv-vmm/src/virt.rs` with the device
tree (`virt/tree.rs`, `virt/fdt.rs`) and PSCI (`virt/psci.rs`), the flash
bank of `vmlab-hv-devices/src/pflash/cfi.rs`, fw_cfg over MMIO in
`vmlab-hv-firmware`, and the boot lockstep (DESIGN.md O4):
`src/oracle/boot.rs` here and `vmlab-hv-vmm/examples/a64_boot_lockstep.rs`.
A machine with `arch: aarch64`, `accel: tcg`, `machine: virt` needs no KVM:
`VmShared` holds no KVM VM for it.

### AAVMF to its UEFI shell (S1)

`cargo test --release -p vmlab-hv --bin vmlab-hv virt_acceptance` starts the
`vmlab-hv` binary on a machine description, as vmlab does, and reads the
serial log. On helios, with AAVMF 2026.08+ds-2 of `guest/fetch-firmware.sh
aarch64`, unchanged:

| vCPUs | Seconds to `Shell>` | Guest resets | Bytes on the serial log |
|---|---|---|---|
| 1 | 17.0 | 0 | 3,247 |
| 2 | 16.4 | 0 | 3,247 |

This AAVMF starts no shell by itself. Without a disk it prints `BdsDxe: No
bootable option or device was found.` and waits for a key, on QEMU 11.1.3
too. The key opens its boot menu, whose second entry is `EFI Internal
Shell`. The test sends four keys through a socket on the PL011 (a `com1`
port of the machine description).

### Boot lockstep against QEMU (O4)

`just tcg-a64-boot-lockstep` runs QEMU 11.1.3 as `-machine
virt,dtb-randomness=off -cpu cortex-a53 -smp 1 -m 1024 -icount
shift=0,sleep=off -rtc base=2020-01-01T00:00:00,clock=vm -nodefaults` with
the two flash banks and the plugin, and our machine beside it as the same
CPU (`QEMU_CORTEX_A53`). Before every instruction it compares X0 to X30,
SP, PC, PSTATE, V0 to V31, FPSR, FPCR and 20 system registers (those of an
exception, of the translation and of the timers, and both stack pointers),
and after it the memory writes that the leader reports. The leader gets the
same four keys on its serial port.

The run of this card on helios, from the reset to `Shell>` on our serial
log (`just tcg-a64-boot-lockstep`, 2,401 s, 0.59 million instructions a
second):

| | |
|---|---|
| Instructions compared | 1,404,633,088 |
| CPU differences | 0 |
| Interrupts taken where the leader took them | 873, each pending in our GIC |
| Host calls (PSCI) | 2, the same answers |
| Counter reads taken from the leader | 91,007,818 |
| Timer and interrupt status reads that differed | 0 |
| ID registers that differed | none |
| Device reads taken from the leader | 9,265 |
| Device reads where our device answered differently | 411, all of the PL011 flag register |
| Guest resets | 0 |

The 411 reads are `UARTFR` at `0x09000018`: the leader has a key in its
receive FIFO (`0x80`) and our PL011, which gets no key, has none (`0x90`).
The shell takes the last key after one second of the leader's instruction
clock, which is a thousand million instructions: most of the run is that
wait, and most of the counter reads are its loop.

What the leader leads, as the design says: the moment of an interrupt, the
value of a device read, the counter and what follows from it, and the answer
to a host call, which is compared. The follower also takes the device tree
(`dumpdtb`) and the fw_cfg files of the leader (read from a second QEMU over
qtest), because the firmware gets both without a device read: the tree is in
RAM, and the files come through DMA.

### What the lockstep found

- **The flags at reset.** QEMU's CPU starts with Z set (PSTATE `0x400003c5`
  in the first record of the plugin; the gdbstub of the design measurement
  showed `0x3c5`). With NZCV at 0 the standalone boot of AAVMF ended in
  `Synchronous Exception at 0x46AF2284`. The `virt` machine now starts each
  CPU with Z set.
- **`SMCCC_VERSION`.** The leader answers `0x80000000` with NOT_SUPPORTED.
  DESIGN.md G4 lists the SMCCC version among what the machine serves. PSCI
  of `virt` now answers as the leader does.
- **The host bridge.** QEMU has a function at 00:00.0 (1b36:0008, class
  06 00) that our PCI root did not have: 152 ECAM reads differed. It is
  added, with the writable bits that qtest shows.
- **The DSDT names the PCI functions of the machine.** The generated tables
  of card C11 start from the capture of a machine with five PCI functions.
  For a leader without them the DSDT is 68 bytes shorter (5,320 against
  5,388), and the firmware read different bytes at instruction 327,549,115.
  The follower now gets the files of its leader. The product still presents
  the DSDT of the capture, whatever functions the machine has: that is a
  finding for card C11, not changed here.

The second matrix, at commit `f7346539` (the head of this card), measures
the speed of each boot. The same `bench/run.py` command in two lanes of
four cores (16-19 and 20-23); every 20 s the sampler turns on the debug log
of `vmlab_hv_vmm::vcpu::emulated` for 1 s and reads the instruction count of
each vCPU from its exception lines. MIPS is the sum over the vCPUs of the
last count, divided by the time from the process start to that sample, so
idle time is inside it, and a boot in emergency mode idles after it fails.
Other cards ran one or two emulated aarch64 VMs on other cores of helios
during the first runs (load average 11 to 15); Debian ran again at load 5.

| Template | vCPUs | Result | MIPS | Instructions, over |
|---|---|---|---|---|
| alpine-3.23 | 1 | agent ready in 296 s | 50.3 | 14.86e9, 295 s |
| alpine-3.23 | 2 | agent ready in 298 s | 51.1 | 15.08e9, 295 s |
| debian-13 | 1 | emergency mode under load; agent ready in 274 s at load 5 | 25.8; 64.2 | 18.23e9, 708 s; 18.94e9, 295 s |
| debian-13 | 2 | emergency mode under load; agent ready in 204 s at load 5 | 27.2; 89.1 | 19.45e9, 715 s; 18.79e9, 211 s |
| fedora-44 | 1 | emergency mode | 43.8 | 31.33e9, 715 s |
| fedora-44 | 2 | emergency mode | 52.2 | 37.35e9, 716 s |
| ubuntu-24.04 | 1 | emergency mode; the agent answered from it at 522 s | 44.6 | 24.42e9, 547 s |
| ubuntu-24.04 | 2 | agent ready in 341 s | 68.5 | 24.50e9, 358 s |

An agent that answers from emergency mode does not count as a pass of S1.
Debian's two results show how near its timeouts it runs: the load of other
VMs on the host is enough to push it over them.

### kvm-unit-tests

`just tcg-a64-kvm-unit-tests` builds kvm-unit-tests (commit `32f8ad45`, with
clang and lld under the names of the GNU cross tools) and runs each selected
test of `arm/unittests.cfg` on QEMU 11.1.3 (`-cpu cortex-a53`, TCG) and on
`vmlab-hv`, booted as a kernel with our own device tree, and compares the
result lines:

| Test | vCPUs | Result |
|---|---|---|
| selftest-setup | 2 | the same: 2 pass |
| selftest-vectors-kernel | 1 | the same: 3 pass |
| selftest-vectors-user | 1 | the same: 2 pass |
| selftest-smp | 4 | the same: 1 pass |
| gicv2-ipi | 4 | the same: 3 pass |
| gicv2-mmio | 4 | 17 pass on vmlab-hv; on QEMU 15 pass and 2 fail |
| gicv2-mmio-up | 1 | the same: 15 pass, 2 fail |
| gicv2-mmio-3p | 4 | 17 pass on vmlab-hv; on QEMU 15 pass and 2 fail |
| gicv2-active | 4 | the same: 1 pass |
| psci (with `cpu-on`) | 4 | the same: 5 pass |
| timer | 1 | the same: 18 pass |

The two subtests that fail on QEMU with four CPUs and pass here are
`ITARGETSR: bits for non-existent CPUs masked` and `ITARGETSR: byte writes
successful`. No subtest passes on QEMU and fails on vmlab-hv.

### Without QEMU

`just ci::check` runs, without KVM and without host assets: the `virt`
machine on guests of a few words (`vmlab-hv-vmm/src/virt/tests.rs`: a kernel
boot with the device tree in X0, PSCI `CPU_ON` and `CPU_OFF` on two CPUs, a
reset in place and with `exit_on_reset`, a firmware that runs from flash and
reads fw_cfg, the tree and its variables), the same through the binary and
its control socket (`vmlab-hv/src/virt_acceptance.rs`), the device tree, the
FDT writer, PSCI as a decision, the flash bank, and the records and the
adoption rules of the boot lockstep.

### What this card does not do

- Snapshots, the debug port and CPU state in a snapshot: card C14. The
  control server of an emulated machine offers neither capability.
- A display, a TPM, USB, shares and vhost NICs on `virt`: `validate` names
  each as not a part of the machine yet.
- The keys of the lockstep are sent to the leader alone. Our PL011 reads
  what the leader's did, so its own receive path is proven by the S1 test
  and not by the lockstep.
- The follower keeps the last 4,096 instructions before a difference, not
  the 100,000 records of DESIGN.md M4.
- The lockstep runs at 0.6 million instructions a second, not the 1.5
  million of the design estimate.

### Sources read for this card

- S6: QEMU 11.1.3 as a running program: the plugin stream, `dumpdtb`, and
  qtest for fw_cfg, the function 00:00.0 and an empty virtio-mmio
  transport.
- S7: the Devicetree Specification and the Linux binding documents of the
  nodes of `tree.rs`; the PSCI specification (DEN 0022) and the SMC Calling
  Convention (DEN 0028); QEMU's `docs/specs/fw_cfg.rst`; the CFI
  specification and the Intel command set for the flash bank; VIRTIO 1.2
  for the empty transport; the arm64 boot protocol of the Linux kernel.
- The generated tables of this crate (S1) for the encodings of `MRS`, `HVC`
  and `SMC` in the adoption rules.
- S2, S3, S4, S5 and S8 were not opened. No chapter of the Arm Architecture
  Reference Manual was opened.
- X1 to X3 hold: no QEMU source was opened, no Arm ASL, XML or pseudocode,
  no Unicorn. kvm-unit-tests (GPL-2.0) was built and run, and its sources
  were not read to write this code. Its `unittests.cfg` is parsed by the
  script that runs it.

## Card S3 (2026-11-27)

Guest RAM straight from the TLB entry, and stores without stamps on pages
that hold no translated code (the five model views of card #1145, F2 and F3
of each).

### What changed

- **The TLB entry keeps the host address of its page** (`tlb::Entry::host`)
  and the address of the page's watch word (`Entry::watch`), from
  `Memory::host_page` when the entry is filled. A load whose page the TLB
  holds that way is a probe, an add and one host access; a store is the same
  plus two relaxed loads of the watch word. Neither calls the memory. The
  CPU drops its entries when the memory names another mapping
  (`Memory::host_mapping`, checked at every `run` and `step`). `FlatMemory`
  and `GuestBus` hand out host pages; every other memory keeps the call.
- **The TLB has 2,048 entries and a folded index** (the page number XOR the
  page number shifted by the index width), so pages 4 MiB apart no longer
  take one place. An invalidation by address looks at one place when no
  entry of a mapping larger than a page covers the address. It looks at
  every entry when one does, and while more than 64 such mappings are kept.
- **A store into a page without translated code moves no stamp.** The
  page's watch word is odd while a CPU holds code it translated there.
  `GuestBus` stores straight when it is even: no stamp, no `SeqCst`
  operation, no lock. The argument for the race between a translator and a
  straight store is the module comment of `vmlab-hv-vmm/src/vcpu/guest_bus.rs`:
  the translator makes the word odd under a mutex and then runs
  `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED)`, takes the stamp before it
  reads the code (the old code read first and stamped after, which let a
  write between the two go unseen), and checks the word after. A stamped
  write makes the word even before it moves the stamps. A device write is
  always stamped. A host without the barrier or without `cmpxchg16b` runs
  more than one CPU with every write stamped, as before.
- **The exclusive store is one host compare and exchange** against the value
  of its load (`Memory::compare_and_store`), under the transaction lock and
  after the generation check. A store of another CPU that changed the value
  fails it; one that wrote the same value back is taken as one before the
  load, which is what QEMU does. The 16-byte form is `lock cmpxchg16b`.
- **The code-page filter bits are cleared again**: each bit has a count of
  the pages of `code_set` that have it, and the bit goes when the count
  reaches zero.
- A memory that counts the stamps of its own writes (`own_generation`)
  tells `settle` how far they moved the generation, so a straight store does
  not start a new epoch.

### Speed

On helios, pinned to cores 28-31, 3 repeats each, with `bench/run.py`
(`just bench-run interpreter aarch64 1 3` for the flat images, and
`--backend vmlab-hv --arch aarch64 --guest debian-13 --suite boot` at 1 and 2
vCPUs for the boot MIPS row). Before is `next` at `8f98e8f4`, which holds
the lazy flags of card S1 and its boot MIPS row. After is `57ad9140`, a
merge of that `next` and this branch on helios: the branch merges into it
without a conflict. The two ran one after the other, before first, while
other cards booted VMs on other cores. The rows are in
`hv/bench/history/aarch64.jsonl` with a `note`.

| | Before (median) | After (median) | Change |
|---|---|---|---|
| CoreMark mix, MIPS | 132.85, 138.38, 141.39 (**138.38**) | 151.74, 150.99, 150.47 (**150.99**) | +9 % |
| Scalar loop, MIPS | 154.54, 148.81, 157.83 (**154.54**) | 158.70, 158.97, 155.81 (**158.70**) | +3 % |
| Debian boot, 1 vCPU, MIPS | 82.55, 84.74, 81.33 (**82.55**) | 91.26, 96.64, 96.86 (**96.64**) | +17 % |
| Debian to agent ready, 1 vCPU, s | 220.46, 212.27, 220.67 (**220.46**) | 196.47, 184.97, 184.38 (**184.97**) | -16 % |
| Debian boot, 2 vCPUs, MIPS | 104.65, 101.45, 107.58 (**104.65**) | 127.41, 124.60, 110.32 (**124.60**) | +19 % |
| Debian to agent ready, 2 vCPUs, s | 173.53, 180.35, 169.32 (**173.53**) | 142.14, 145.14, 167.13 (**145.14**) | -16 % |

The scalar loop has no load and no store, so its change is within the
noise. The five views estimated 25 to 40 % at 1 vCPU and 50 to 70 % at 2
for the whole of their W1; what is left of the gap at 2 vCPUs is the block
loop and the operations, which card S0 profiled.

### Equal behaviour

- O3 lockstep on `ef5a139c` (the code of this card): `just tcg-a64-lockstep
  loadstore` verified 5,227,768 instructions in 2,980,769 rounds, `mmu`
  4,403,165 in 1,155 rounds and `walk` 4,454,798 in 1,099 rounds (seed 1,
  100,000 per class), no difference in any. Those groups run on
  `FlatMemory`, which hands out host pages, so they run the new paths.
- kvm-unit-tests (`just tcg-a64-kvm-unit-tests`) on `57ad9140`: 11 tests,
  9 with the same results as QEMU 11.1.3, 2 (`gicv2-mmio`, `gicv2-mmio-3p`)
  pass the two `ITARGETSR` subtests that fail on QEMU, as in card C13, and
  none differs otherwise.

### Tests

- `vmlab-hv-vmm/src/vcpu/emulated_tests.rs`: a store moves the stamps only
  where a CPU holds code, a translator sees a write that raced it
  (`watch_holds`), a device write is always stamped; the exclusive store is
  a compare and exchange at 1, 2, 4, 8 and 16 bytes; and two CPUs on two
  threads, 200,000 rounds each, take a spinlock with `LDAXR`/`STXR`, count
  under it with plain loads and stores, give it back with a straight `STLR`,
  count a second word with `LDXR`/`STXR` alone and store beside both in the
  same page: both counts end at 400,000.
- `vmlab-hv-tcg/src/tlb.rs`: the folded index, an invalidation by address
  with and without a larger mapping, and past 64 of them.
- `exec.rs`: a new memory under the CPU drops the host pages of the old one.

### What this card does not do

- The JIT of phase 3e can take the entry as it is (host address and watch
  word); no code of it is here.
- An invalidation by ASID still looks at every entry.
- Code on pages that a CPU also writes as data (mixed pages) is stamped and
  translated again as before: each stamped write makes the watch even, and
  the next translation makes it odd, with a barrier at 2 or more CPUs.

### Sources read for this card

- The state of this crate, of `vmlab-hv-tcg` and of
  `vmlab-hv-vmm/src/vcpu/guest_bus.rs`; the Linux man page of `membarrier`
  for its two commands, and the x86 `cmpxchg16b` instruction from general
  knowledge of the instruction set.
- S1 to S8 for semantics: none opened. No chapter of the Arm Architecture
  Reference Manual was opened.
- X1 to X3 hold: no QEMU source was opened, no Arm ASL, XML or pseudocode,
  no Unicorn.

## Card C14 (2026-11-27)

Snapshots, reset and the debug port of an emulated machine (DESIGN.md F6,
D13).

### How it is built

- `Cpu::replace_state` puts a new state into the core from outside the
  guest and drops what the core derived from the old one: the TLB, the
  translated blocks, the code pages and the exclusive reservation. The TLB
  is tagged by ASID and keeps its entries over a new table base, so a
  restore that brings other tables under the same `TTBR` would otherwise run
  on stale translations. A reset, PSCI `CPU_ON`, a snapshot restore and a
  register write of the debug port all go through it.
- `virt/snapshot.rs` (vmlab-hv-vmm) writes the sections of the machine: each
  CPU (its slots, PSTATE, every system register row by name and both generic
  timers, under the name of the CPU model), which CPUs are on, the system
  counter, the GICv2, the PL011, the PL031, the GED, fw_cfg, the flash bank
  of the variables (its blocks that are not erased) and the host bridge. RAM,
  the virtio functions and the BARs go through the common path of
  `snapshot.rs`, which skips the KVM state and the memory slots on an
  emulated machine. A restore sets the counter first, then the devices, then
  the CPUs.
- The debug port of an emulated machine (`debug/emulated.rs`): pause,
  continue, detach, the registers (`aarch64_registers` in the reply),
  `set_registers` by name, a step that runs one instruction on the thread of
  the request while the machine stays paused, and memory by physical or
  virtual address through the CPU's own walk.
- A boot without firmware unpacks an EFI zboot kernel (gzip), which the
  aarch64 guest asset ships.

### Tests

- S1 (`vmlab-hv`, `virt_acceptance.rs`):
  `snapshots_of_an_emulated_aarch64_machine_online_and_offline` takes an
  online snapshot of a counting guest over the control socket, loads it in
  the same process and in a new one on the same disk (the count and X1 come
  back, and the guest counts on), and refuses the load of an offline
  snapshot of the disk with `snapshot_incompatible`.
- vmlab-hv-vmm: the CPU, GIC, timer and RAM come back equal to the save
  (`a_snapshot_of_an_emulated_machine_brings_its_cpu_gic_timer_and_ram_back`),
  refusals change nothing, the debug port reads, steps and writes a CPU, a
  zboot kernel boots, and the CPU state refuses another model or layout.
- vmlab-hv-tcg-aarch64: a replaced state runs on caches that are built
  again. vmlab-hv-devices: the flash bank's restore brings the array and the
  file back.
- `vmlab-hv-tests/tests/s1_aarch64.rs` boots the aarch64 guest asset to
  agent ready, loads an online snapshot and resets the guest in place, with
  the agent answering after each. It skips without the asset.

### What this card does not do

- The agent proof did not run to its end on helios (2026-11-27): Linux
  6.12 of the guest asset boots on the emulator, then reports a soft lockup
  in the timer softirq and reaches no user process in 540 s. Booting the
  guests is card C13.
- Breakpoints and `write_registers` (x86 registers) are refused on an
  emulated vCPU. A step of a vCPU that PSCI has not turned on is refused.
- The exclusive reservation is not saved: a restore ends it, which an
  exception return also does.

### Sources read for this card

- The state of this crate. For the EFI zboot header: the first bytes of
  the guest asset's kernel (`od`) and what `file` says of it (`MZ`, `zimg`,
  the payload offset and size, `gzip`). No kernel source was read.
- S1 to S8 for semantics: none opened. No chapter of the Arm Architecture
  Reference Manual was opened.
- X1 to X3 hold: no QEMU source was opened, no Arm ASL, XML or pseudocode,
  no Unicorn.

## Card C13 (2026-11-27)

The four templates to agent ready (DESIGN.md P4, M6). The card lands what
it found and fixed. Five of the eight boots reach agent ready inside 600 s
on a quiet host, Debian among them, and the boot lockstep (O4) of Debian did
not run. What stops the rest is the speed of the interpreter, not an
instruction or a device that a boot misses. The boots that fail on speed and
the O4 lockstep of Debian move to card C13b (decision of the overseer,
2026-11-27): guest time stays real time and no guest timeout is lengthened.

### What the boots missed, and what changed

- The virtio functions of `virt` are transitional (`1af4:1001` and so on),
  as QEMU presents them: AAVMF bound no driver to a modern-only function.
  An I/O BAR at port 0 decodes on `virt`, where AAVMF places the first one.
- An Inner Shareable TLBI reaches every CPU of the machine (`shootdown.rs`).
- The vmlab-hv binary ran every emulated vCPU in slices of 64 instructions,
  because the flight recorder makes `log_enabled!(Trace)` always true. The
  slices follow the stderr filter now. With long slices two faults showed:
  the core read its interrupt lines once a slice, so AAVMF took an
  acknowledged interrupt again and again until its stack overflowed, and a
  timer that came due inside a slice was raised at its end, which failed the
  busy-loop subtests of kvm-unit-tests `timer`. A machine CPU reads its lines
  before every block, and leaves its loop within 64 blocks of its timer's
  deadline.
- The boots spent their time in three places (stack samples of the vCPU
  thread of a Debian boot, `eu-stack` through a `PR_SET_PTRACER` wrapper):
  - about 40 % in the RAM path of `GuestBus`: vm-memory's slice lookup for
    each load and store. RAM is read and written through its mapping now,
    with one host access for an aligned access of 2, 4 or 8 bytes, and the
    bitmap of the region still told of each write;
  - about 15 % in SipHash, in the interning of the graph of a translation:
    the tables of a translation hash with a multiply-and-rotate hasher;
  - translations of code translated before: between 120 s and 240 s of a
    Debian boot, 4.8 million translations, 3.6 million of them at a PC whose
    block the cache had dropped (it emptied its store when full), and 1.1
    million of the same code at another virtual address (a library in two
    programs). The store keeps two generations bounded by bytes, a block
    computes its PC-relative values from the PC at its entry, and the cache
    is indexed by the physical address of the code. To the same point of the
    boot the translation time fell from 44.8 s to 7 s.
- The loop reads the words of a block without bounds checks, which every
  operation passes once when its block is made (`block::op_in_frame`).

### The boots (S1)

`bench/run.py --backend vmlab-hv --arch aarch64 --suite boot` on helios,
the templates of `~/.local/share/vmlab/templates/aarch64`, AAVMF of
`guest/fetch-firmware.sh`, 2 GiB, pinned to four cores, under the memory cap
(`systemd-run --scope -p MemoryMax=24G`). One run each at commit `29189e7f`
(the code of this card less its tests and lints), Debian at 1 vCPU three
times.

| Template | 1 vCPU | 2 vCPUs |
|---|---|---|
| alpine-3.23 | agent ready in 358 s | agent ready in 356 s |
| debian-13 | agent ready in 278 s, 285 s and 302 s; one run of an earlier commit of this card in emergency mode | agent ready in 247 s |
| fedora-44 | emergency mode | emergency mode |
| ubuntu-24.04 | emergency mode | agent ready in 317 s |

Each boot that fails falls into systemd's emergency mode: a device unit
(`/boot/efi`, `/dev/ttyAMA0`, the root of the Fedora initrd) does not appear
inside its 90 s job timeout, because udev's coldplug runs too slowly while
the rest of early userspace runs beside it. The guests run nothing that the
CPU does not have: the unknown-is-loud log names nothing, and no boot ends in
an unimplemented instruction.

The speed it takes: Fedora executes 30.7e9 instructions to agent ready on
QEMU (M2). The interpreter runs a boot at about 45 to 60 million
instructions a second, so Fedora needs more than 500 s with no idle time,
and its device timeouts come first. Dynamic counts on the Debian boot: about
23 operations a block and 4.2 a guest instruction, 11 register copies a
block; `Add` is 27 % of the operations, `Slice` 13 %, `And` 10 %, `IsZero`
9 %, `Xor` 6 %, most of it the NZCV of the flag-setting instructions, which
every block that ends in a compare writes out. Lazy flags, block chaining or
the JIT of 3e are what is left for a gain of the size that Fedora needs.

### kvm-unit-tests

`just tcg-a64-kvm-unit-tests` at commit `3353f252`: 11 tests, 9 give the
same result lines as QEMU and 2 (`gicv2-mmio` and `gicv2-mmio-3p`, 4 vCPUs)
pass the two `ITARGETSR` subtests that fail on QEMU, as in card C10. The 4
vCPU tests `selftest-smp`, `gicv2-ipi`, `gicv2-active` and `psci` give the
same results. `timer` gives the same results after the deadline exit above.

### Tests

- vmlab-hv-tcg-aarch64: `one_translation_serves_code_at_two_addresses`
  (`tests/translate.rs`) maps one page of code at two addresses, calls an
  `ADR` at each and counts one translation fewer than for a copy;
  `a_machine_cpu_leaves_its_loop_when_its_timer_is_due` (`exec.rs`).
- vmlab-hv-tcg-ir: the hasher spreads near keys over the buckets.

### What this card does not do

- Fedora at 1 and 2 vCPUs and Ubuntu at 1 vCPU do not reach agent ready,
  and Debian at 1 and 2 vCPUs is near its device timeouts: the speed above.
  These move to card C13b.
- O4 for Debian did not run: at the 0.59 million instructions a second of
  card C10 the 18e9 instructions of the boot take about 8.5 hours. It moves
  to card C13b.
- The S1 test of the aarch64 guest asset (`s1_aarch64.rs`, card C14) did
  not run: helios has no aarch64 guest asset.

### Sources read for this card

- The state of this crate and of vmlab-hv-vmm, and vm-memory's
  `GuestRegionMmap` interface (`as_ptr`, the bitmap) for the RAM path.
- The multiply-and-rotate hash from its description (the hasher of Firefox
  and rustc); no code of it was read or copied.
- The serial logs of the guests, and kvm-unit-tests as a running program.
- S1 to S8 for semantics: none opened. No chapter of the Arm Architecture
  Reference Manual was opened.
- X1 to X3 hold: no QEMU source was opened, no Arm ASL, XML or pseudocode,
  no Unicorn.

## S0: Debian boot profile (#1146)

S1 lazy flags landed in `ab1dc54d` via PR #1031 and is included in this
branch. The S0 function samples, block counts and ceilings below describe
the **pre-S1 interpreter**. The S0 measurement binary was `2251fc8b`.

The `boot_mips` row belongs to the aarch64 Debian 13 `vmlab-hv` boot
suite. A run starts a fresh overlay, waits for the guest agent handshake,
then issues `quit`. The VM emits the instruction count of every vCPU
after joining its workers. The bench divides their sum by the monotonic
wall time at the handshake. A small amount of guest execution can occur
between the handshake and the quit request. A failed handshake or absent
counter supplies no MIPS sample.

```sh
python3 hv/bench/run.py --backend vmlab-hv --arch aarch64 --guest debian-13 \
  --suite boot --vcpus 2 --repeats 3 --cpus 24-27 --hv-bin /path/to/vmlab-hv
```

### Repeat measurements

[Aarch64 history](../../bench/history/aarch64.jsonl) keeps the pre-S1 S0
rows before the S1 comparison. Use the **87.92 MIPS** one-vCPU row
(`ca5825bb`) as the post-S1 baseline. S1 measured **76.68 to 87.92 MIPS**
(+14.7%) at one vCPU, using pin 16–19 and the debug-port count
(`54317232` before S1). There is no post-S1 two-vCPU row. Measure that
configuration on the relevant base commit before comparing future changes.

S0's 62.87 MIPS and S1's 76.68 MIPS before row both predate lazy flags.
They used different pins (16–17 and 16–19 respectively), and S0 measured
under higher host load. S0's successful one-vCPU repeats started at
one-minute load averages of 12.50–13.24, while the S1 before batch
started at 10.19 and ended at 4.39. S0 counted at VM exit just after
the handshake, while S1 read the paused vCPUs through the debug port.
These differences confound the comparison, and the samples cannot
isolate how much each contributed.

At two vCPUs on the pre-S1 S0 build, the median was
80.12 MIPS with block counting disabled and 74.89 MIPS with it enabled,
with three boots per setting. One instrumented run took 350.74 s
compared with 224.99 s for another, so the median difference does not
isolate counter overhead.

At one vCPU, three complete baseline boots pinned to CPUs 16–17 took
285.53–296.45 s, with a median of 62.87 MIPS. One complete instrumented
boot on that pin took 308.34 s (60.19 MIPS) and supplied the block counts.
Other one-vCPU attempts hit the shared VM limit before launch or entered
systemd emergency mode on the device timeout. Only complete boots enter
the history. Guest time ran in real time with the standard timeouts.

Flat CoreMark checked the same 245,425,680 instructions and guest
checksums in each repeat. Three interleaved pairs pinned to CPUs 0–1
gave median 111.12 MIPS on the initial boot-row build (`591d6274`) and
120.07 MIPS on the build with the block counter code (`2251fc8b`).
The initial build's 14.38 MIPS spread is larger than the median
difference. Both builds used release optimization with line tables.
Cargo updated the local lockfile to include the existing `flate2`
dependency of `vmlab-hv-vmm` during the builds.

### Function samples

The one-vCPU column reuses the two windows of the
[JIT design's W1 profile](../../docs/plans/phase-3e-aarch64-jit/DESIGN.md#w1-the-profile-measured)
weighted by their sample counts. Those windows cover the busy part of a
boot that did not reach agent ready. The two-vCPU run used Debian
`13.20261001.0`, 2 GiB of RAM, two vCPUs pinned to logical CPUs 24–27 on
helios, and the release binary of `2251fc8b` with line tables. It reached
agent ready in 241.13 s at 77.17 MIPS (13.35 billion and 5.26 billion
instructions on vCPU 0 and 1 respectively).

The two-vCPU run used `bench/ipsample.c` with raw instruction pointer
output and a `vcpu-*` thread filter. It requested user-space samples at
1 kHz through `perf_event_open`, starting when the control socket
appeared. `addr2line -f -i -C` resolved ELF offsets. The classifier used
function names and inline callers where they identify a subsystem.
Each sample is counted once. This measures on-CPU vCPU work. Idle time,
kernel execution and time descheduled on the host are outside the profile.

| Work | 1 vCPU, W1 (166,057 samples) | 2 vCPUs (342,753 samples) |
|---|---:|---:|
| Operation execution and dispatch, `step_unchecked`, `Compiled::run` | 38.33% | 32.25% |
| Block loop, IRQ/FIQ, shootdown mail and PC translation, `run_inner`, `code_address` | 20.31% including cache | 15.58% |
| Cache probe and code stamps, `usable`, `code_stamp` | included above | 7.35% |
| State copies at block entry and exit, `Place::write` and `Place::read` | 3.57% | 9.73% |
| RAM load path, `Side::load`, `GuestBus::read`, `copy_in` | 26.44% for RAM and other VMM frames combined | 5.37% |
| RAM store path, `GuestBus::write`, ROM check, `copy_out_to` | included above | 3.67% |
| Dirty callback and generation atomics, `WriteDiscard::mark_dirty`, `Stamps::wrote` | included above | 12.77% |
| Global RAM transaction lock in store and exclusive accesses | one-vCPU branch bypasses lock | 3.44% |
| TLB and page walks, `Tlb::lookup`, `Mmu::walk` | 6.04% including load/store dispatch | 1.92% |
| Wide and exclusive access dispatch, `Compiled::access` | 0.28% for all effect dispatch | 0.65% |
| Translation, decoder, lifter, IR and lowering | 1.03% | 0.41% |
| Helpers, system registers and exceptions | 1.11% combined | 1.52% |
| Other or unresolved | 2.85% | 5.36% |

At two vCPUs the helpers account for 0.76% in vector/crypto functions,
0.38% in common helper dispatch, 0.36% in system register/timer functions,
and 0.02% in exception handling. The sampler attributed zero samples
directly to a scalar FP helper. Other samples include 1.38% in outlined
mutex routines whose callers were unavailable. The lock share attributable
to guest RAM is therefore 3.44%, with a ceiling of 4.82% if all of those
outlined mutex samples belong to it. The dirty callback with its SeqCst
generation updates and the identified lock together account for 16.21%.
The rest of the RAM store path accounts for 3.67%.

### Counts per translated block

Set `VMLAB_HV_BOOT_BLOCK_COUNTS=1` for the boot command to collect these
counters. The VM prints totals per vCPU at exit. The first complete
instrumented boot at each vCPU count supplied the totals below. The
two-vCPU column sums the vCPUs before dividing by the number of blocks.

| Counter | 1 vCPU, 492,167 blocks | 2 vCPUs, 711,911 blocks |
|---|---:|---:|
| Lowered operations per block | 13.961 | 14.224 |
| Alignment guard operations per block | 3.326 | 3.398 |
| Exit-only writeback operations per block | 0.938 | 0.966 |
| LDP X instructions per block | 0.179 | 0.184 |
| STP X instructions per block | 0.191 | 0.195 |

The alignment count includes each alignment guard and its lowered scalar
dependencies used exclusively by such guards. The exit writeback count
includes lowered operations needed only to rebuild registers on a side
exit. Values also used by a normal operation, the terminal writeback,
the exception syndrome or an alignment guard are excluded. The LDP/STP
counts include the offset, pre-indexed and post-indexed X register forms.
These counts weight each translation once, including retranslations and
single-instruction blocks. About 31% of the static operations are alignment
or exit-writeback work. The profile gives no execution frequency for those
operations.

### Pre-S1 ceilings for remaining cards

The ceilings below come from the pre-S1 profile. Lazy flags changed
interpreter work, so current shares and relative gains require a new profile.
The ceilings include all of the indicated measured functions, even
where the planned change can remove only part of the work. They express
shares of sampled boot CPU work under the measured guest workload. The
two-vCPU wall-clock effect also depends on which vCPU delays agent readiness,
how much work overlaps and whether faster execution changes the boot path.
The ceilings overlap and should be recalculated after each change.

For a generous wall-time budget at two vCPUs, the sampler's requested
frequency gives about 343 vCPU-seconds over 241 s of elapsed time. The
last column assumes that every saved vCPU-second shortens wall time by
a full second at fixed guest work. It is a sizing estimate.

| Card | Work included in the ceiling | 1 vCPU CPU share | 2 vCPU CPU share | 2 vCPU wall budget |
|---|---|---:|---:|---:|
| S3 TLB RAM access and unwatched stores | RAM load, RAM store, dirty stamps, TLB/page walks and the entire possible lock share. The W1 ceiling includes other VMM work. | 32.48% | 28.55% | 41% |
| S4 alignment, exit writeback and pair accesses | All operations, wide access dispatch, RAM load/store paths and TLB work. The W1 ceiling uses its combined RAM/VMM class. Most of these paths survive S4. | 71.09% | 43.86% | 62% |
| S5 cheaper block entry and chaining | The block loop, cache, register copies and all helper/system work. The W1 helper subtotal includes exceptions. | 24.99% | 34.16% | 49% |

For S4, the static 31% operation share times the two-vCPU operation-loop
share gives about 10% of sampled CPU work for guard and writeback
operations if operation costs and execution frequency are uniform. The
function profile does not distinguish those operations from other
uses of the same opcodes. The effect of the pair fast path also depends
on the proportion of X pairs that hit one page.

The pre-S1 samples suggest investigating S3 first because the dirty callback
and direct lock activity accounted for 16.21% of two-vCPU work. S4 can
proceed alongside it, with the in-page pair path checked against the S3 TLB
changes when they meet. Keep S5 on hold while the JIT's block chaining
card (J4) is on schedule.

### Behaviour checks

On helios, `just tcg-a64-lockstep loadstore 1000 1` verified 52,390
instructions, and `just tcg-a64-lockstep mmu 100000 1` verified 4,403,165.
Both exited 0 with no differences. The counter unit test compares the
same whole block with counting enabled and disabled, including an
overwritten register and both X pair instructions.

The kvm-unit-tests runner exited 0 at suite revision `32f8ad45`. Nine of
eleven cases matched QEMU. As in C13, vmlab-hv passed the two ITARGETSR
subtests that QEMU failed in both `gicv2-mmio` and `gicv2-mmio-3p`.

No instruction semantics sources were consulted for S0. QEMU was used as
a running oracle for lockstep and kvm-unit-tests.

## Card S2, #1143 (2026-11-27)

Translated blocks that do not depend on their virtual address (DESIGN.md
M6). The PC-relative blocks and the cache indexed by physical address were
made on card C13 (`73333030`, `b5117069`, after its F6); this card takes
that branch, measures it, finds what it costs and takes part of the cost
back.

### What a block is keyed by

A block is kept and found under the physical address of its first
instruction and its `Context`: the exception level, `PSTATE.SP`, the FP
enable of `CPACR_EL1.FPEN`, and `TCR.TBI0`/`TBI1` of the regime for an
instruction address. The level, the stack pointer and the FP enable change
what the lifter makes, and TBI changes the graph of a branch to a register.
Nothing else is needed in the key:

- A block never crosses a 4 KB page (`translate` ends it at a page
  boundary), so its virtual and physical addresses have the same low 12
  bits for every granule, and `ADRP` takes the page of the entry PC.
- The execute permission, the level's view of it, the ASID and the
  translation regime are checked by the TLB lookup of the PC before every
  block (`code_address`), not by the key: a mapping without execute
  permission takes its abort before any block is looked up.
- Every value that depends on the PC is computed from the PC at the entry:
  `ADR`, `ADRP`, `LDR` (literal), the link of `BL`/`BLR`, the PC of an
  exception that leaves the block, the PC of the last instruction that the
  loop runs (`Trap`), and the next PC.
- The code bytes are checked as before, by the code stamp of the memory for
  the physical pages of the block and the epoch. A store through any virtual
  address reaches the same physical page, so invalidation stays exact: the
  block is made again only when its own bytes may have changed.

### What it cost, and what changed here

On the flat images, whose translations do not matter, the PC-relative
blocks ran slower: the next PC of every direct branch and the PC of every
exception exit had become an `Add` of the entry PC in the operations of the
block, and the exit PC of a load or a store is computed before the access.
Now:

- A direct branch and the fall through carry their next PC as a distance
  from the entry (`A64Next::Relative`). A conditional branch is a `Select`
  of two constants, as before the change. The loop adds the entry PC.
- An exception exit carries the distance of its instruction (`at` of
  `A64Next::Exception`) and no PC write: the loop sets the PC when the block
  leaves there.
- The next PC is not one of the register outputs any more: the block sets
  it from its own words after them, and reads the entry PC after its
  operations, not across them.

What is still slower than the blocks at fixed addresses: `BL`, `ADR`,
`ADRP` and `LDR` (literal) read the PC and add, and the index of the cache
needs the physical address, which waits for the TLB lookup, where the
virtual index did not.

### Measurements

helios, pinned to four cores (`--cpus 28-31`), 1 vCPU, Debian 13 template,
2 GiB, `bench/run.py --backend vmlab-hv --arch aarch64 --suite boot`. The
host ran other cards' boots and builds throughout (load average 4 to 28),
so the wall times are noisy; the translation counts are not. Before is
`f91d8899`, this branch at `567c0829` with `b5117069` and `73333030`
reverted (C13's two-generation store and everything else kept); after is
`567c0829`; final is `b94d2d67`, the code of this card (a commit of a
scratch copy on helios with the tree of `40c0ffa4` of this branch;
`f91d8899` exists only there too). The instruction and
translation counts come from the line each emulated vCPU logs when the
bench pauses the machine at agent ready (`boot_counters`).

| Build | Translations to agent ready | Translation time | Instructions | Agent ready | Boot MIPS |
|---|---|---|---|---|---|
| before | 1,605,469; 1,591,822; 1,595,640; 1,586,935 | 12.8 s; 13.5 s; 12.6 s; 14.9 s | 18.02e9 to 18.31e9 | 228 s, 249 s, 250 s, 289 s; one run in emergency mode | 79.2, 72.6, 72.1, 63.4 |
| after | 490,571; 488,811; 491,088; 482,317; 491,783; 492,190; 492,717 | 3.7 s to 4.4 s | 17.92e9 to 18.22e9 | 249 s, 264 s, 269 s, 254 s, 226 s, 222 s, 221 s | 72.2, 68.5, 67.0, 71.7, 79.4, 80.9, 81.1 |
| final | 490,335; 491,570; 495,805; 495,209; 492,465 | 3.6 s to 4.0 s | 17.98e9 to 18.27e9 | 269 s, 243 s, 243 s, 220 s, 271 s; one run in emergency mode | 67.9, 74.6, 75.3, 81.9, 66.4 |

The same physical code is translated once: 3.3 times fewer translations,
and the translation time of a boot falls from about 13 s to about 4 s. The
boot time does not show it beyond the noise of the host.

The flat aarch64 images (`--backend interpreter`), 12 samples of each
build, interleaved: before, final and before again, three repeats a run:

| Build | CoreMark mix | Scalar loop |
|---|---|---|
| before | 133.1 MIPS (131.0 to 137.5) | 127.7 MIPS (111.0 to 130.1) |
| after (an earlier interleaving) | 112.9 MIPS against 131.5 before | 112.9 MIPS against 125.5 before |
| final | 123.3 MIPS (119.2 to 126.3) | 121.1 MIPS (119.6 to 123.6) |

The flat mix runs 7 % slower than with blocks at fixed addresses, and the
scalar loop 5 %, from 14 % and 10 % after the change of C13.

`hv/bench/history/aarch64.jsonl` holds the rows of the three builds:
`coremark_algorithms_1000` and `scalar_loop_300000000` of the interpreter,
and `boot_to_agent_ready` and `boot_mips` of vmlab-hv, whose `env` keeps
the counts of each repeat (`boot_counters`). These rows were taken with this
card's pause counters; since the merge with card #1142 the bench takes
`boot_mips` from #1142's instruction counts, and the pause line of a vCPU
(`parking`) remains for the translation counts.

### Equal behaviour

- O3 lockstep (`just tcg-a64-lockstep <group>`, seed 1, 100,000 a class) at
  `b94d2d67`, the tree of `40c0ffa4`: `c1` 1,439,552 instructions, `loadstore` 5,227,727, `system`
  14,143,370 (with the exceptions), `mmu` 4,403,165; no difference in any.
- kvm-unit-tests (`just tcg-a64-kvm-unit-tests`) at `b94d2d67`: 11 tests, 9
  give the same results as QEMU, and `gicv2-mmio` and `gicv2-mmio-3p` pass
  the two `ITARGETSR` subtests that fail on QEMU, as in cards C10 and C13.

### Tests

- `a_shared_block_computes_its_page_and_its_link_from_the_pc_it_runs_at`
  (`tests/translate.rs`): one page of code with `ADRP`, `BL` and a return
  at two addresses in two pages, run as blocks and stepped.
- `a_write_to_shared_code_reaches_it_at_every_address`: the block of the
  first address serves the second until a store through a third address
  changes its code, and the second address runs the new code.
- `one_translation_serves_code_at_two_addresses` (card C13).
- bench: `BootCountersTests` reads the pause lines of the vCPUs.

### Sources read for this card

- The state of this crate, of vmlab-hv-vmm's emulated vCPU and of the bench.
- S1 to S8 for semantics: none opened. No chapter of the Arm Architecture
  Reference Manual was opened.
- X1 to X3 hold: no QEMU source was opened, no Arm ASL, XML or pseudocode,
  no Unicorn.

## Card J3, #1154 (2026-11-27)

The inlined softmmu of the JIT (3e DESIGN.md H7): the fast TLB of
`vmlab-hv-tcg-jit::tlb` in the `Mmu`, its fill in the Rust load and store
paths of card S3 (#1147, merged into the branch first, because the fast
TLB copies the host address of its TLB entries), the flushes of H7b, the
write table rule of H11a, and the inlined loads and stores of 1, 2, 4 and
8 bytes with their miss stub. The measured rows and H7e are in G2 of the
design.

### Equal behaviour

- K1, the O3 lockstep with the JIT on (`VMLAB_HV_JIT=1 just
  tcg-a64-lockstep <group> 100000 1`, seed 1, 100,000 a class): at
  `4e079406`, `mmu` 4,403,165 instructions in 1,155 rounds and `walk`
  4,454,798 in 1,099 rounds, no difference. `loadstore` stopped once, after
  896 s, on a difference of the low byte of V7 at the start of a round
  (ours 0x00, QEMU 0xff). It did not repeat: at `073314f6` (the same code
  with a switch for the fills, which was then removed) two full runs gave
  5,228,140 and 5,227,849 instructions and runs with only read fills and
  only write fills 5,227,893 and 5,227,817, all with no difference. The
  lockstep is not deterministic (the round counts of those runs differ).
  At `55866877`, before J3, `loadstore` with the JIT on gave 5,227,868,
  no difference, in one run.
- K2 (`just tcg-a64-jit-diff 300000 3e0001`) at `41f4fc16`, with the
  accesses of every other round kept and compared: 300,000 rounds,
  2,792,656 instructions, no difference.
- K6 with the JIT on: `VMLAB_HV_JIT=1 cargo test -p vmlab-hv-tcg-aarch64`
  (every test of the crate, among them the stores into translated code of
  `tests/loadstore.rs` and `tests/translate.rs`) and `VMLAB_HV_JIT=1 cargo
  test -p vmlab-hv-vmm --lib vcpu::`: all pass.

### Tests

- `tests/fasttlb.rs`: the fill of each table and level (H7a), nothing for
  an unprivileged access, a read-only page, or a memory without host
  pages, the write entries of a page that gets translated code (H11a), the
  exclusive mark, every flush of H7b (TLBI by address, of every entry and by
  ASID, a new TTBR0, SCTLR and TCR, a new state), the watch word after an
  inlined store, and K6 with the JIT on.
- `vmlab-hv-tcg-jit`: `tlb::tests` (the lookup, the flushes), the inlined
  forms against the interpreter of the block form on random blocks with
  misses, hits, faults and a watched page, `Memory::Logged`, and the new
  encodings against `llvm-mc`.
- `tests/jit.rs`: K2 asks for hits and misses of the fast TLB.

### Sources read for this card

- The state of `vmlab-hv-tcg-jit`, of this crate, of vmlab-hv-vmm's guest bus
  and emulated vCPU, and of the bench. The Intel 64 and IA-32 Architectures
  Software Developer's Manual, volume 2, for `MOVZX`, `TEST` and the sized
  forms of `MOV`.
- S1 to S8 for semantics: none opened. No chapter of the Arm Architecture
  Reference Manual was opened.
- X1 to X3 hold: no QEMU source was opened, no Arm ASL, XML or pseudocode,
  no Unicorn.
