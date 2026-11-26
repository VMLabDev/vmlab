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
- X1 to X3 hold: no QEMU source, no Arm ASL, XML or pseudocode, no Unicorn.
