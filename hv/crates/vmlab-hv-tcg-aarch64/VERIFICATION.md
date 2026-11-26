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

`tables/constraints.toml` has 17 constraints (15 before). `just
tcg-a64-generate-check host` passes on the regenerated `decode.rs` and
`tests/vectors.txt`. `tests/vectors.rs` passes on the 13,660 vectors.

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
