---
slug: phase-aarch64-v8x
kind: design
created: 2026-11-28
card: 1235
---

# vmlab-hv aarch64: the Armv8.1 to v8.5 instruction sets, 16K pages and GICv3

This document is the design of three additions to the aarch64 emulator of
phase 3d: the instruction sets of Armv8.1 to v8.5 (F2), the 16K translation
granule (F4), and GICv3 with more than 8 vCPUs (F6, which is also gap G7 of
the `virt` machine). F2, F4, F6, G7, V8 and MQ2 are the codes of the macOS
review (thread 1, post 44) and of the card. No part of this document is a
macOS boot path, and none depends on one. The three help Linux guests.

It is a document of its own and not a section of the 3d or 3e design. The
reason: 3d is accepted and its codes are closed, 3e is the JIT and is 3,196
lines of measurements, and these three additions have their own cards, their
own oracle rule and their own questions.

A code with a prefix (`3d:G1c`, `3e:H7`) is a row of
`../phase-3d-aarch64/DESIGN.md` or `../phase-3e-aarch64-jit/DESIGN.md`. The
codes of this document (`M`, `I`, `E`, `L`, `N`, `T`, `K`, `V`, `B`, `R`, `Y`,
`Q`) are stable. Every number marked *measured* comes from helios (QEMU
11.1.3, LLVM 23.1.4) on 2026-11-28, with the tools in `measure/`. A number
marked *est.* is an estimate, with its basis.

The rules of 3d hold for every card of this document: `3d:P1` (decode from
the LLVM tables, our own semantics), `3d:P2` (QEMU is the oracle as a running
program), the sources `3d:S1` to `3d:S8`, and `3d:X1` to `3d:X3`. `3d:X1` in
particular: no card reads, copies or translates QEMU source under
`target/arm`, `fpu`, `tcg` or `accel/tcg`. Nothing in this document came from
there. What it says about QEMU was read from a running QEMU: its ID registers
over the gdbstub, its device tree with `dumpdtb`, and its error messages.

## Summary

- **F2.** 15 LLVM feature sets add 532 records in 54 format classes to the
  decoder: 23 format classes are new and 31 exist and get more records
  (M1). The decoder becomes one decoder for two CPU models, with the feature
  test at run time (E1 to E4). The lifters are 9 groups (L1 to L9). QEMU has
  no CPU model that is Armv8.5 and nothing more, so the lockstep uses two
  leaders: `neoverse-n1`, which is exact for the v8.2 part, and `max` with
  SVE, SME and pointer authentication off, compared under a mask, for the
  rest (T1 to T7).
- **F4.** The stage-1 walk already takes the granule from `TCR_EL1`, and the
  lockstep group `walk` already compares it with QEMU's `max` for 16K. What
  is left is a CPU model that says so, three checks of the TLBs, and a guest
  with a 16K kernel as the proof (K1 to K6).
- **F6.** GICv3 is a second interrupt controller model beside the GICv2. Its
  CPU interface is system registers, which the machine adds to the core
  through the hook of `3d:G3e`, so the core still knows no controller. The
  first card runs PCI devices on INTx, as QEMU's `its=off` does. The ITS is
  a card of its own (V1 to V9).
- **The CPU model.** The `virt` CPU stays `vmlab-v8.0` (`3d:Q2`). The sets go
  into a second model, `vmlab-v8.5` (MQ2). Which machine gets it is Q1. PAN
  and LSE in the `virt` CPU (V8) stay open (B4).
- 14 cards (Y1 to Y14). After Y1, four agents can work at once.

## 1. F2: the instruction sets

### M1. What each set adds (measured)

`measure/sets.py` reads the JSON dump of the pinned LLVM tables with the
selection rule of the generator (`3d:G1c`) and counts what a feature adds to
the 3d set (2,332 records in 168 format classes).

| Code | LLVM feature | Arm name, version | Records | Format classes | New format classes |
|---|---|---|---|---|---|
| I1 | `LSE` | atomics, v8.1 | 168 | 4 | `BaseCAS` (16), `BaseCASP` (8), `BaseLDOPregister` (128), `BaseSWP` (16) |
| I2 | `RDM` | `SQRDMLAH`, `SQRDMLSH`, v8.1 | 24 | 3 | `BaseSIMDThreeSameVectorTiedR0`, `BaseSIMDThreeScalarTied` |
| I3 | `LOR` | `LDLAR`, `STLLR`, v8.1 | 8 | 2 | none |
| I4 | `FullFP16` | half-precision arithmetic, v8.2 | 242 | 27 | none |
| I5 | `FP16FML` | `FMLAL`, `FMLSL` (FHM), v8.2 | 16 | 2 | `BaseSIMDThreeSameVectorFML`, `BaseSIMDThreeSameVectorIndexH` |
| I6 | `DotProd` | `SDOT`, `UDOT`, v8.2 | 8 | 2 | `BaseSIMDThreeSameVectorDot`, `BaseSIMDThreeSameVectorIndexS` |
| I7 | `SHA3` | SHA3 and SHA512, v8.2 | 8 | 6 | `CryptoRRRR_16B`, `CryptoRRRTied`, `CryptoRRRTied_2D`, `CryptoRRR_2D`, `CryptoRRRi6`, `CryptoRRTied_2D` |
| I8 | `JS` | `FJCVTZS` (JSCVT), v8.3 | 1 | 1 | none |
| I9 | `ComplxNum` | `FCADD`, `FCMLA` (FCMA), v8.3 | 7, and 6 more with `FullFP16` | 3 | `BaseSIMDIndexedTiedComplex`, `BaseSIMDThreeSameVectorComplex`, `BaseSIMDThreeSameVectorTiedComplex` |
| I10 | `RCPC` | `LDAPR` (LRCPC), v8.3 | 4 | 1 | `RCPCLoad` |
| I11 | `RCPC_IMMO` | `LDAPUR`, `STLUR` (LRCPC2), v8.4 | 13 | 1 | none |
| I12 | `FlagM` | `CFINV`, `RMIF`, `SETF8`, `SETF16`, v8.4 | 4 | 3 | `BaseFlagManipulation`, `FlagRotate`, `SimpleSystemI` |
| I13 | `AltFPCmp` | `AXFLAG`, `XAFLAG` (FlagM2), v8.5 | 2 | 1 | none |
| I14 | `FRInt3264` | `FRINT32Z`, `FRINT32X`, `FRINT64Z`, `FRINT64X`, v8.5 | 20 | 2 | none |
| I15 | `SB` | the speculation barrier, v8.5 | 1 | 1 | none (`SimpleSystemI` of I12) |
| | I1 to I15 together | | **532** | **54** | **23** |

LLVM has no feature `SHA512`: its `SHA3` holds the four SHA512 instructions
and `EOR3`, `RAX1`, `XAR`, `BCAX`. The decoder after this work has 2,864
records in 191 format classes.

Three more sets were counted and are not in I1 to I15:

| Code | LLVM feature | Records | New format classes | Why not |
|---|---|---|---|---|
| I16 | `PAuth` (pointer authentication, v8.3) | 35 | 8 | A question of its own (Q2). In the hint space the PAC instructions stay NOPs, as on `vmlab-v8.0`. |
| I17 | `SM4` (SM3 and SM4, v8.2 optional) | 9 | 5 | Optional in the architecture, and no M1-class CPU has it (`3d` "M1": the feature line has no `sm3` or `sm4`). |
| I18 | `MTE` (v8.5 optional) | 25 | 6 | Optional, and it is tag storage for all of RAM, not an instruction set. |

Not instruction records at all: BTI, DIT, SSBS, PAN and UAO are bits of
PSTATE, the `BTI` instructions are in the hint space and decode already
(`3d:M2`). They are in section N below.

### M2. What QEMU can lead (measured)

`measure/idregs.py` starts a stopped QEMU and reads the ID registers of a
CPU model over the gdbstub. `ID_AA64ISAR0_EL1`, `ID_AA64PFR0_EL1` and
`MIDR_EL1` are not in the register list of the gdbstub: the card that writes
a model row reads those with `just tcg-a64-sysprobe`, which runs `MRS` in the
guest.

| QEMU `-cpu` | `ID_AA64ISAR1_EL1` | `ID_AA64MMFR0_EL1` | `ID_AA64MMFR1_EL1` | `ID_AA64MMFR2_EL1` | `ID_AA64ZFR0_EL1` | What it says |
|---|---|---|---|---|---|---|
| `cortex-a53` (the leader of 3d) | 0 | `0x1122` | 0 | 0 | 0 | v8.0, 4K and 64K |
| `cortex-a76`, `cortex-a55` | `0x100001` | `0x101122` | `0x10212122` | `0x1011` | 0 | v8.2: LRCPC 1, DPB 1, PAN 2, LO, HPDS 2, VH, UAO. 16K. 40 bits of physical address. |
| `neoverse-n1` | `0x100001` | `0x101125` | `0x10212122` | `0x1011` | 0 | The same with 48 bits of physical address. |
| `neoverse-v1` | `0x0011000010211302` | `0x101125` | `0x10212122` | `0x0220011102101011` | not 0 | v8.4 with SVE, BF16 and I8MM |
| `cortex-a710` | `0x0010111110211502` | `0x22200101122` | `0x10212122` | `0x1221011110101011` | not 0 | v9.0 with SVE2 |
| `max` | `0x0111211110211502` | `0x2100032310201126` | `0x0110112010312122` | `0x1221011112011011` | not 0 | everything QEMU emulates |
| `max,sve=off,sme=off,pauth=off` | `0x0111211100211002` | the same | the same | the same | 0 | `max` without SVE, SME and PAC |

- **M2a.** Only `max` has FRINT, SB, FlagM2 and (by QEMU's documentation of
  its emulated features) SHA3, SHA512, FlagM and FHM without SVE. No QEMU
  model is "Armv8.5 and nothing more".
- **M2b.** `max` without SVE and SME still has sets that the v8.5 model
  lacks: BF16 2, I8MM, DGH, XS, SPECRES in `ID_AA64ISAR1_EL1`, and WFxT,
  RPRES, MOPS, HBC, CSSC in `ID_AA64ISAR2_EL1` (`0x1120000000110012`). It has
  no property `mte`: `ID_AA64PFR1_EL1` is `0x0010101010000121`, which is BTI,
  SSBS 2 and MTE 1, and MTE 3 with `-machine virt,mte=on`. BTI cannot be
  turned off.
- **M2c.** `pauth=off` exists and clears API and GPI. `pauth-qarma5=on` is
  accepted. The default of `max` is API 5, an IMPLEMENTATION DEFINED
  algorithm.
- **M2d.** `neoverse-n1` is an exact named model for the v8.2 part: LSE, RDM,
  LOR, FP16, DotProd, RCPC, PAN, UAO and the 16K granule. The sysprobe of
  card Y2 confirms `ID_AA64ISAR0_EL1` and `ID_AA64PFR0_EL1` for it before the
  design counts on it (R1).

### The decoder work

- **E1. One decoder, two models.** Today `tables/sets.toml` selects the 3d
  set and the generator emits only its records: another encoding is
  `Undefined` (`3d:G1c`). With two CPU models in one binary the generator
  emits the union (3d and I1 to I15), and a CPU model holds a `Features`
  value. A record whose features the model lacks does not match.
- **E2. A missing feature falls through, it does not end the decode.** A
  word can match a record of a v8.x set and a less specific record of the
  base. The decision tree keeps its candidates in the order of specificity
  (`3d:G1b`), and the generated match takes the first candidate whose
  features the model has. So `vmlab-v8.0` decodes every word exactly as the
  3d decoder does today. The proof is a generator test and a unit test: the
  tree of the union, asked with the features of 3d, names the same record as
  the committed `tests/vectors.txt` and as a tree generated from the 3d set
  alone, on every vector and on 10 million random words.
- **E3. The feature word grows.** `generated/decode.rs` keeps the features
  of a record in `Features(pub u16)`, one bit per selected feature. 3d uses
  5 bits and I1 to I15 need 15 more. It becomes a `u32`. `sets.toml` gets
  one set per row of M1, and `selected` names them all.
- **E4. Overlaps and constraints.** `overrides.toml` and `constraints.toml`
  (`3d:G1b`, `3d:G1d`) are extended as the generation and the lockstep ask.
  The generator fails on a crossing pair of equal specificity, so a new
  overlap cannot pass without a decision. Known before the work: `CASP`
  needs even register numbers, and `RMIF`, `SETF8`, `SETF16` have
  should-be bits.
- **E5. No hot-path cost.** A block is translated for one CPU, and the model
  of a CPU never changes. The feature test is in the decode of a
  translation, not in a block. The jump cache and the block cache need no
  new key.
- **E6. The generated system tables do not change.** `generated/sysreg.rs`
  already holds `SysReg`, `TLBI`, `DC`, `AT`, `IC` and the PSTATE fields of
  every feature, each with what it `requires`. A model turns a row on by
  having a row for it in `sysreg.rs` (`3d:G3e`).

### The lifter work

One lifter per format class (`3d:G3a`). The two tiers of `3d:G3d` hold:
integer work is pure IR, FP, AdvSIMD and crypto are helper calls.

| Code | Group | Sets | Work | Lines (*est.*) |
|---|---|---|---|---|
| L1 | Atomics | I1 | Four lifters. `CAS` and `CASP` map onto `Memory::compare_and_store`, which the exclusives use already (`3d:R3`). `SWP` and the eight `LD<op>` forms need an atomic read-modify-write on the `Memory` trait: one new method with a default made from a `compare_and_store` loop, and a native one in `SharedMemory` and in the host-page fast path (host `lock xadd`, `lock cmpxchg` loop, `xchg`). The alignment and the fault order follow the leader. | 700 |
| L2 | Ordered loads and stores | I3, I10, I11 | `LDLAR` and `STLLR` are the lifters of `LDAR` and `STLR`. `LDAPR` is a plain load in our memory model (every load is already ordered). `LDAPUR` and `STLUR` are the unscaled lifter with nine-bit offsets. | 150 |
| L3 | Flags | I12, I13 | `CFINV`, `RMIF`, `SETF8`, `SETF16`, `AXFLAG`, `XAFLAG` on the lazy flag record (`3d:G3c`): each writes the kind "the four bits themselves". Pure IR. | 200 |
| L4 | Half precision, scalar | I4, I8, I14 | The scalar FP lifters take a third size. The `f16` operations of SoftFloat 3e are vendored (`crates/vmlab-hv-tcg/vendor/softfloat/source/f16_*.c`), and the card adds those that the Arm build of `3d:R4` does not compile yet. `FPCR.FZ16` and the flush rules of half precision. `FJCVTZS` with its Z flag. The four `FRINT32/64` forms. | 900 |
| L5 | Half precision, AdvSIMD | I4, I5 | The lane helper of `3d:G3d` takes 16-bit FP lanes for the 14 AdvSIMD format classes that `FullFP16` adds records to. `FMLAL` and `FMLSL` widen half to single. | 900 |
| L6 | AdvSIMD integer | I2, I6 | `SQRDMLAH`, `SQRDMLSH` with saturation into FPSR.QC. `SDOT` and `UDOT`, vector and indexed. | 350 |
| L7 | Complex | I9 | `FCADD` and `FCMLA` with the rotation field, for half, single and double lanes. | 300 |
| L8 | SHA3 and SHA512 | I7 | Eight helpers, written from FIPS 180-4 and FIPS 202 (`3d:S7`) and `3d:S2` to `3d:S4`. | 250 |
| L9 | `SB` | I15 | A barrier: ends the block as `ISB` does. | 10 |
| | Generator (E1 to E4) | | | 400 |
| | The model, its ID rows, and section N | | | 1,200 |
| | Lockstep changes (T1 to T6) | | | 800 |
| | Total | | | about 6,200 |

The basis of the estimates is the size of the same groups in 3d (`3d:M3`)
by the count of format classes.

**The JIT (3e).** Every new lifter runs under the JIT from its first day,
because a helper call is a form that the emitter has (`3e:H12`). L1 is the
one group where speed is the reason for the work: `3e:J9` names LSE as a
candidate, because a guest with LSE stops running `LDXR` and `STXR` loops,
whose cost at 2 and 4 vCPUs card `3e:J7` measured. Card Y4 ships L1 as
helpers and measures. An inline emitter form for `CAS` and `LDADD` is card
Y5, and it is written only if the measurement of Y4 asks for it.

### N. What a v8.5 CPU has besides instructions

A guest reads the ID registers and then uses what they name. A model that
names a feature must have all of it. The rows of `vmlab-v8.5`, with the
state in today's code (`sysreg.rs`: `ID_AA64MMFR1_EL1` and `ID_AA64MMFR2_EL1`
of `vmlab-v8.0` are 0):

| Code | Feature | Version | In `vmlab-v8.5` | Work |
|---|---|---|---|---|
| N1 | PAN | v8.1 | yes | `PSTATE.PAN`, `MSR PAN`, `SCTLR_EL1.SPAN`, set at exception entry. The permission check of a privileged access to a user page. The softmmu TLB and the fast TLB of `3e:H7` hold the read and write rights of a level, so they hold them per value of PAN, or a change of PAN flushes. Card Y6 measures which (Linux changes PAN at every user access routine). |
| N2 | LOR registers | v8.1 | yes | Five registers, stored. With I3. |
| N3 | HPDS | v8.1 | yes | `TCR_EL1.HPD0` and `HPD1` in the walk. |
| N4 | HAFDBS | v8.1, optional | no | The hardware update of the access and dirty bits. Left for a card that measures the faults it saves. |
| N5 | VHE, and everything of EL2 | v8.1 and later | no | There is no EL2 (`3d:G4`). |
| N6 | UAO | v8.2 | yes | `PSTATE.UAO` and its effect on `LDTR` and `STTR`. |
| N7 | DPB and DPB2 | v8.2, v8.5 | yes | `DC CVAP` and `DC CVADP` as the other cache operations. |
| N8 | RAS | v8.2 | minimal | `ESB` in the hint space is a NOP, `ERRIDR_EL1` reads 0 records, `DISR_EL1` is stored. |
| N9 | DIT | v8.4 | yes | A stored bit of PSTATE. |
| N10 | LSE2 | v8.4 | yes | Which unaligned accesses of the ordered and exclusive loads and stores do not fault. Settled against the leader `max`. |
| N11 | TLBI by range, and the outer-shareable forms | v8.4 | yes | Rows of the `TLBI` table, on the flushes that exist. The range form scales by the granule (K4). |
| N12 | The trap of the ID space at EL0 (IDS) | v8.4 | yes | A syndrome choice at an existing exception. |
| N13 | SSBS | v8.5 (optional from v8.0) | yes | A stored bit of PSTATE and `MSR SSBS`. |
| N14 | SPECRES | v8.5 | yes | Three system instructions that do nothing here. |
| N15 | E0PD | v8.5 | yes | `TCR_EL1.E0PD0` and `E0PD1`: a fault for EL0 in the walk. |
| N16 | BTI | v8.5 | Q3 | `PSTATE.BTYPE`, the GP bit of a page, a check at the first instruction after an indirect branch, a new exception class. It costs work in every indirect branch of the JIT. |
| N17 | RNG (`RNDR`, `RNDRRS`) | v8.5, optional | no | Small and useful to a guest. Not asked for. Named for a later card. |
| N18 | Pointer authentication | v8.3 | Q2 | I16. |
| N19 | The PMU | | no | As `3d:V5`. |

`vmlab-v8.5` is therefore not a CPU that Arm would call v8.5: it has no EL2,
no PMU, and, by Q2 and Q3, perhaps no pointer authentication and no BTI. A
guest does not see an architecture version. It sees the ID fields, and each
field of the model is true. The name of the model says where its sets end.

The source rule for section N is the rule of 3d: gem5 (`3d:S2`) for the
exception and translation behaviour, a probe on QEMU where it is silent, and
the prose of the Arm manual by `3d:Q1`, recorded in the card report.

### The test plan

`3d:O3` holds as it is: random streams from our own decode tables, a flat
image, QEMU with the plugin, a compare before every instruction, and a
regression corpus that `ci::check` replays without QEMU. What changes is the
leader.

- **T1. Two leaders.** `neoverse-n1` leads the sets that it has (I1, I2,
  I3, I4, I6, I10 and N1, N2, N3, N6, N7 by M2d). The lockstep CPU for it is
  a new model `QEMU_NEOVERSE_N1` beside `QEMU_CORTEX_A53`: QEMU's ID values
  row by row, from `tcg-a64-sysprobe`. Against this leader every rule of 3d
  holds unchanged, also "zero Undefined on one side only".
- **T2. `max` leads the rest**, started as
  `-cpu max,sve=off,sme=off,pauth=off` (M2b, M2c): I5, I7, I8, I9, I11 to
  I15, and N9 to N15. `max` is a black box that has more than our model, so
  three rules are added for this leader only.
- **T3. Undefined on our side only is allowed for a named reason.** A word
  that we decode to `Undefined` and the leader executes is not a difference
  when the pinned LLVM tables give the word a record of a feature outside
  the model (the generator writes that list of masks and values beside the
  vectors, from the same dump). It is a difference in every other case. The
  streams are made from our own tables, so they hold such a word only in the
  random-word part.
- **T4. System state is compared under the mask of the model.** A bit of
  PSTATE, of `SPSR_EL1` or of a control register that is RES0 in our model
  and live in `max` (for example `PSTATE.BTYPE`, `PSTATE.TCO`, `SCTLR_EL1`
  bits of MTE) is left out of the comparison. The stream generator also
  keeps such a bit from getting set: before an `MSR` to a register with a
  write mask it puts an `AND` with the mask of the model, so the leader does
  not act on a bit that our CPU does not have.
- **T5. The ID registers** of the model that differ from the leader are in
  the allow-list of `3d:O4`, each with its reason, as the rows of
  `vmlab-v8.0` are today.
- **T6. Corpus beyond random streams.** Half precision gets the vectors of
  Berkeley TestFloat for `f16` (`3d:O5`) before it meets QEMU. SHA3 and
  SHA512 get the published test vectors of FIPS 202 and FIPS 180-4, as whole
  hashes built from the instructions. The atomics get the two-thread stress
  test of `3d:C3`, extended: counters incremented with `LDADD`, a lock made
  from `CAS`, a `SWP` exchange, 10 million rounds, at 2 and 4 threads, under
  the interpreter and under the JIT.
- **T7. Executed encodings.** The four templates never execute these sets on
  `vmlab-v8.0`. Card Y9 boots Debian on the new model with the trace plugin
  on the leader `max` and adds the distinct encodings of the new sets that
  the boot executed to the corpus, as `3d:O5` did for the base.

The bar of a card is the bar of `3d:O3`: every record of the group in the
stream, at least 100,000 instructions per format class without a difference,
the counts and the seed in the report. The JIT proofs of 3e apply to every
card that adds a form: K1 for the group and K2.

What this cannot prove, beyond `3d:U1` to `3d:U7`:

- **T8.** Where only `max` leads, an Undefined agreement is proven against
  LLVM's tables and not against a CPU of the same shape (T3).
- **T9.** The memory order of the atomics on more than one vCPU. The leader
  runs one vCPU. The stress test of T6 carries that proof.

## 2. F4: the 16K granule

### K1. What exists (read from the code)

- `mmu.rs` takes the granule from `TCR_EL1.TG0` and `TG1` as data. Its
  `geometry` gives the start level, the bits of the first level and the
  stride for 4K, 16K and 64K, and its unit tests translate a page and a
  block of each granule (`a_page_and_a_block_of_each_granule_translate`).
- A CPU model names its granules in `ID_AA64MMFR0_EL1`, and a granule that
  the CPU lacks is replaced (`Controls::supported`).
- The lockstep group `walk` (`just tcg-a64-lockstep walk`) runs random
  translation tables of the 16K and 64K granules against QEMU's `-cpu max`,
  with the model `QEMU_MAX_WALK`, which "is no CPU of a machine".
- The softmmu TLB and the fast TLB of the JIT work in 4K host pages
  (`PAGE_BITS`, `va >> 12` in `vmlab-hv-tcg-jit/src/tlb.rs`), and an entry
  knows the size of the guest page it came from. The code pages of the block
  cache are 4K pages of physical memory.

So the walk is done. `vmlab-v8.0` says 4K only (`TGran16` is 0, as QEMU's
`cortex-a53` has it: `ID_AA64MMFR0_EL1` `0x1122`, M2), and the `virt` CPU
stays that way (`3d:Q2`).

### What is left

- **K2. The model says it.** `vmlab-v8.5` has `TGran16` 1 and `TGran64` 0 in
  `ID_AA64MMFR0_EL1`, with 48 bits of input address and no LPA2: at 16K that
  is four levels, the first with two entries, and blocks of 32 MiB at level
  2 only.
- **K3. One guest page is four TLB entries.** A `TLBI VAE1` of one 16K page
  must end all four 4K entries of the softmmu TLB, of the fast TLB and of
  the jump cache rule of `3e:H6`. The entries record the size of their page
  for the 2 MiB blocks of today. Card Y7 adds the unit tests at 14 bits to
  the rules `3e:H7a` and `3e:H7b`, and repairs what they find.
- **K4. The argument of a TLBI.** The address of a `TLBI` by address is in
  4K units for every granule. The range forms of N11 name their granule in
  the argument. Both are lockstep streams of the group `walk`.
- **K5. Faults.** The level in a fault syndrome at 16K (levels 0 to 3, with
  the two-entry level 0) is in the streams of `walk` already. The card
  extends the group with permission faults and with the access flag at 16K.
- **K6. The proof is a guest.** A Linux with a 16K kernel boots to agent
  ready on a machine with `vmlab-v8.5`, and `getconf PAGESIZE` over the
  agent's `exec` answers 16384. The guest is the Debian 13 template with a
  16K kernel package installed in a copy of its disk (*est.*: Debian 13
  ships `linux-image-arm64-16k`. The card checks, and takes Fedora's
  `kernel-16k` if not). AAVMF itself runs with 4K pages and needs nothing.
  The window lockstep of `3d:O4w` runs on that boot with the leader `max`
  under T2 to T5, because `cortex-a53` has no 16K.
- **K7. Speed.** A 16K guest takes fewer TLB misses for the same memory, but
  each guest page fills up to four fast TLB entries. The card records the
  boot row and the `xz` row of the 16K guest beside the 4K rows of the same
  build. No bar.

Not in F4: 64K as a product granule (the walk has it, no model says it),
52-bit addresses (LPA, LVA, LPA2), and the contiguous hint, which a TLB of
4K entries can ignore.

## 3. F6: GICv3 and more than 8 vCPUs

### V1. Today (read from the code, and measured)

- The controller is `vmlab-hv-devices/src/gic.rs`: a GICv2 with the GICv2m
  frame, `MAX_CPUS` 8, one mutex, and the CPU of a bus access taken from
  `set_current_cpu`. `vmlab_hv_firmware::virt::MAX_VCPUS` is 8, and vmlab
  refuses more in `src/config/hypervisor.rs` (`VIRT_MAX_VCPUS`).
- QEMU's `virt` refuses 9 vCPUs without `gic-version=3` ("exceeds max CPUs
  supported by machine 'mach-virt' (8)", *measured*), and vmlab's QEMU argv
  names no GIC version. So no vmlab machine has ever had more than 8 aarch64
  vCPUs or a GICv3. There is no existing identity to keep.
- QEMU's map with `gic-version=3`, from `dumpdtb` (*measured*): the
  distributor at `0x08000000`, 64 KiB. The redistributors from `0x080a0000`,
  `0xf60000` bytes, which is 123 CPUs at two 64 KiB frames each. With 200
  vCPUs a second redistributor region at `0x4000000000`, `0x4000000` bytes.
  The ITS at `0x08080000`, 128 KiB, and `msi-map` points at it. With
  `its=off` there is no MSI controller at all: no ITS, no v2m frame, no
  `msi-map`. QEMU's limit is 512 vCPUs. The timer interrupts keep their
  PPIs.
- The gdbstub of QEMU lists no `ICC_` register (*measured*: 0 of 345). The
  CPU interface can be observed only from inside the guest.

### The design

- **V2. A second device model.** `gicv3.rs` beside `gic.rs`, written from
  the GICv3 and GICv4 architecture specification (Arm IHI 0069, a prose
  specification, `3d:S7`) and from register probes of a running QEMU, as the
  header of `gic.rs` says for the GICv2. It has the distributor
  (`GICD_*`, affinity routing always on, `GICD_IROUTER`), and one
  redistributor per CPU with its two frames (`GICR_*`: the SGIs and PPIs of
  that CPU, `GICR_WAKER`, and the LPI registers for V7). No security
  extensions and one security state, as QEMU's `virt` without `secure=on`.
  The GICv2 model does not change, and a machine has one or the other.
- **V3. The CPU interface is system registers that the machine adds.** The
  core knows no controller (`3d:F3`, `3d:A6`) and keeps it that way. `3d:G3e`
  lets a machine add rows to the system register table: the `virt` machine
  with a GICv3 adds `ICC_PMR_EL1`, `ICC_IAR0_EL1` and `ICC_IAR1_EL1`,
  `ICC_EOIR0_EL1` and `ICC_EOIR1_EL1`, `ICC_HPPIR0_EL1` and `ICC_HPPIR1_EL1`,
  `ICC_BPR0_EL1` and `ICC_BPR1_EL1`, `ICC_CTLR_EL1`, `ICC_SRE_EL1`,
  `ICC_IGRPEN0_EL1` and `ICC_IGRPEN1_EL1`, `ICC_RPR_EL1`, `ICC_DIR_EL1`,
  `ICC_SGI0R_EL1`, `ICC_SGI1R_EL1`, `ICC_ASGI1R_EL1` and the active priority
  registers, each with a hook into the device model. It also sets the GIC
  field of `ID_AA64PFR0_EL1` to 1 on the CPU model of that machine. The
  names and encodings are in `generated/sysreg.rs` already (E6). The M1
  machine uses the same hook for its AIC registers, so this is the first
  user of a seam that `3d:A5` needs.
- **V4. Locks.** One mutex for the whole controller is the GICv2 today. With
  16 or more vCPUs each interrupt would take it three times (acknowledge,
  end, and the line). The state is split by who writes it: the CPU interface
  of a CPU (priority mask, binary points, group enables, running priority,
  active priorities) belongs to the thread of that vCPU and takes no lock.
  A redistributor has its own lock. The SPIs of the distributor share one.
  Each redistributor publishes its best pending candidate (interrupt number,
  priority, group) in one atomic word, which the CPU interface reads to
  drive the IRQ and FIQ lines and to answer `ICC_HPPIR`. An SGI
  (`ICC_SGI1R_EL1`) takes the lock of each target redistributor in turn and
  kicks its vCPU through the kick of `3e:H10c`.
- **V5. Numbers.** CPU `n` has the affinity that QEMU gives it: card Y10
  reads the `reg` of every `cpu@` node of QEMU's device tree at 16 and 32
  vCPUs and the `GICR_TYPER` of each redistributor, and the model follows.
  1,020 interrupt numbers are not needed: the SPI count stays what the
  GICv2 of `virt` has.
- **V6. The firmware tables.** The device tree node `arm,gic-v3` with the
  redistributor region. ACPI: a MADT with a GICC entry per CPU, a GICD of
  version 3 and a GICR region (and the ITS entry in V7), the GTDT and the
  PPTT for the larger topology. The hwref capture of `3d:C11` gets a second
  configuration with `gic-version=3`, and the normalised diff against QEMU
  is empty outside the allow-list. AAVMF picks its GIC driver from what the
  machine presents and is not changed.
- **V7. MSIs: INTx first, then the ITS.** QEMU has two shapes with a GICv3
  (V1): the ITS, or no MSI. The GICv2m frame with a GICv3 is not a shape of
  QEMU's `virt`, so there is no hwref for it, and it is not built.
  - Card Y10 builds the shape of `its=off`: PCI functions offer no usable
    MSI controller and the guests use INTx, which `virt` wires already
    (`map::PCI_INTX_SPI`, the `interrupt-map` of `virt/tree.rs`). Four
    shared level lines carry every virtio device and the xHCI.
  - Card Y11 builds the ITS: the command queue, the device, collection and
    interrupt translation tables in guest memory, LPIs with their
    configuration and pending tables, `GITS_TRANSLATER` as the `MsiSink`,
    the IORT with the ITS group, and `msi-map`. *Est.* 2,500 lines: it is
    the largest single device of the `virt` machine after virtio.
  - A machine with more than 8 vCPUs is offered to vmlab users only after
    Y11 (Q4), because shared INTx lines under multiqueue virtio-net are the
    slow shape.
- **V8. Which controller a machine gets.** 8 vCPUs or fewer: the GICv2, as
  today, so no existing machine changes what its guest sees and no snapshot
  is orphaned (HV-D30). More than 8: the GICv3. The machine description
  gets a field for it, and `describe.rs` of vmlab sets it from the vCPU
  count. No key in the lab file (Q5). A snapshot holds the state of the
  controller it was taken with, and a restore builds that controller.
- **V9. How many vCPUs.** The map has room for 123 without the second
  redistributor region, which is not built. vmlab accepts up to 32 at first
  (Q6): helios has 32 host threads, the emulator runs one host thread per
  vCPU (`3d:M6`), and no number above the host's threads can be measured
  there. `MAX_VCPUS` of the firmware crate and `VIRT_MAX_VCPUS` of vmlab
  become functions of the controller.

### The test plan of F6

- **V10. The MMIO registers** against a running QEMU: `virt-devices-oracle.py`
  records the answers of QEMU's distributor and redistributors to a list of
  reads and writes (through qtest, as it does for the GICv2 today), and the
  unit tests of `gicv3.rs` replay the record.
- **V11. The CPU interface** in instruction lockstep, because nothing else
  can see it (V1): a new group `gicv3` of `tcg-a64-lockstep` with the leader
  started as `-machine virt,gic-version=3`. The image makes interrupts
  pending with writes to `GICD_ISPENDR` and `GICR_ISPENDR0` (no timer and no
  device, so the moment is the same on both sides), then runs streams of
  `MRS` and `MSR` on the `ICC_` registers with random priorities, masks and
  groups, and takes the interrupts. The registers are compared through the
  general registers that the stream reads them into. The device reads of the
  leader are adopted and compared as `3d:O4` does.
- **V12. kvm-unit-tests** (`3d:O5`, `scripts/a64-kvm-unit-tests.py`): the
  `gicv3` variants of `gic` (`ipi`, `active`), `timer`, `psci` and, with
  Y11, the `its` tests, at 4, 16 and 32 vCPUs, with the pass list of QEMU.
- **V13. Guests.** The four templates to agent ready at 16 vCPUs on the
  GICv3, in INTx shape after Y10 and with MSIs after Y11. The boot lockstep
  of `3d:O4` to the UEFI shell with the leader on `gic-version=3`. The
  snapshot tests of `3d:C14` on a GICv3 machine. A stress run: the `sha256`
  and `xz` rows of the bench at 16 vCPUs, under K3 of 3e, for the IPI and
  TLBI broadcast paths at a count that no test reaches today.

## 4. The CPU model

- **B1.** The `virt` CPU stays `vmlab-v8.0` (`3d:Q2`). Nothing in this
  document changes what an existing machine presents: not an ID register,
  not a decoded word (E2), not the controller (V8).
- **B2.** A second model, `vmlab-v8.5` (MQ2): `vmlab-v8.0` and I1 to I15,
  the rows of section N marked yes, and the 16K granule (K2). It is a
  `CpuModel` value in `sysreg.rs` like `V8_0`: a list of ID resets, and the
  `Features` of E1. It grows card by card: a card that adds a set also sets
  its ID field, and no ID field is set before the lockstep of its set
  passes. Until the last card the model is not selectable outside tests.
- **B3. How a machine gets it.** `MachineDescription.cpu_model` exists in
  `vmlab-hv-proto` and the `virt` machine takes a `CpuModel` already
  (`virt.rs`: `model.unwrap_or(&V8_0)`). The name `vmlab-v8.5` in the
  description selects the model. Who writes that name is Q1. A snapshot
  records the model, and a restore under another model is refused, as it is
  for the hypervisor today.
- **B4. Open, by the card: PAN and LSE in the `virt` CPU (V8).** This
  document does not decide it. What it gives the decision: LSE is I1 with L1
  and PAN is N1, each a card that lands in `vmlab-v8.5` first. After Y4 and
  Y6 both exist behind a model, and moving either into `vmlab-v8.0` is a
  change of two ID fields plus a guest-visible difference under HV-D30 for
  every existing machine. The measurements that the decision needs are named
  in Y4 (the boot and the D2 suite at 2 and 4 vCPUs with LSE against
  without) and Y6 (the cost of PAN in the TLBs).
- **B5. The M1.** `3d:A1` lists these sets for an M1-class CPU, `3d:A4` the
  16K granule. An M1 model is a third `CpuModel` over the same decoder and
  the same lifters. Nothing here is specific to it, and nothing here builds
  it.

## Risks

- **R1.** `neoverse-n1` may lack a set that T1 gives it (M2d was read from
  four ID registers, not from `ID_AA64ISAR0_EL1`). Then that set moves to
  the leader `max` under T2. The plan holds, with a weaker Undefined proof
  for that set (T8).
- **R2.** The mask of T4 hides a real difference in a bit that the model
  does have. The mask is generated from the write masks of the model's own
  register rows, not written by hand, and the card report prints it.
- **R3.** PAN in the fast TLB (N1) costs speed on every user access of the
  kernel. Y6 measures before the model turns the field on, and the field
  stays 0 if the cost is not acceptable.
- **R4.** The ITS is large (V7) and Linux falls back to INTx without it, so
  a defect in it can hide behind a guest that still boots. V12 and a check
  that the guest's `/proc/interrupts` shows ITS interrupts carry the proof.
- **R5.** More than 8 vCPUs reaches paths that 4 vCPUs barely load: the TLBI
  broadcast of `3e:J7` is a wait on every other vCPU. V13 measures it. The
  result may be that a 16-vCPU guest is slower than an 8-vCPU guest on some
  workloads. That is reported, not gated.
- **R6.** The 16K Debian kernel package of K6 may not exist or may not boot
  under ACPI on `virt`. Fedora's 16K kernel is the second choice, and a
  kernel built for the test is the third.

## Cards

Every card ends with `just ci::check` at exit 0, keeps `3d:X1` to `3d:X3`,
lists the sources that it read, and adds its rows to
`hv/bench/history/aarch64.jsonl` where it names a measurement. "With" names
the cards that can run at the same time.

| Card | Title | What to do | Proof | After | With |
|---|---|---|---|---|---|
| Y1 | One decoder for two CPU models | E1 to E4 and E6: the union in `sets.toml`, `Features` as `u32`, the feature test with fall-through in the generated match, `Features` in `CpuModel`. The list of T3 from the generator. No lifter: a record of a new set decodes and lifts to Undefined until its card. | `tcg-a64-generate-check`. E2: the union tree with the features of 3d names the record of every committed vector and of 10 million random words as the 3d tree does. `tcg-a64-decode-check` on the union. The flat rows of the bench do not move. | none | Y10 |
| Y2 | The model `vmlab-v8.5`, the two leaders and the masked compare | B2 and B3 without a set turned on. `QEMU_NEOVERSE_N1` from `tcg-a64-sysprobe` (M2d, R1). The leader `max` with T2 to T5 in the lockstep harness, and the `AND` of T4 in the stream generator. | The 3d groups of `tcg-a64-lockstep` pass against `neoverse-n1` and against `max` with the model `vmlab-v8.5` at its first state (the 3d sets), 100,000 instructions per format class. The mask of T4 printed in the report. | Y1 | Y10 |
| Y3 | Ordered loads and stores, flags and `SB` | L2, L3, L9 with N2, N9 and N13. | `3d:O3` for the groups, the leaders of T1 and T2. K1 and K2 of 3e. | Y2 | Y4, Y6, Y7, Y8 |
| Y4 | The LSE atomics | L1 as helpers, with the new method of the `Memory` trait. The stress test of T6. Measure: the Debian boot, `sha256` and `xz` at 2 and 4 vCPUs on `vmlab-v8.5` with the LSE field on and off (the input of B4 and of Y5). | `3d:O3` for the four format classes against `neoverse-n1`, with memory writes compared. The stress test at 2 and 4 threads, interpreter and JIT. The rows. | Y2 | Y3, Y6, Y7, Y8 |
| Y5 | Inline emitter forms for the atomics | `CAS`, `LDADD` and `SWP` as host instructions on the fast TLB path of `3e:H7`. Only if the rows of Y4 show the helper in the profile. | K1 and K2 of 3e, the stress test, the rows before and after. | Y4 | any |
| Y6 | The system features of v8.1 to v8.5 | Section N: N1, N3, N6, N7, N8, N10, N11, N12, N14, N15, each with its ID field. N1 with the measurement of R3. | `3d:O3` group `system` and `mmu` with streams for each feature: PAN and UAO permission faults, HPD and E0PD walks, the TLBI range forms, the unaligned accesses of LSE2. The boot row before and after N1. | Y2 | Y3, Y4, Y7, Y8 |
| Y7 | The 16K granule in a model, and a 16K guest | K2 to K7. | The unit tests of K3. The group `walk` with the streams of K4 and K5. K6: the 16K guest to agent ready at 1 and 2 vCPUs, the page size read through the agent, and the window lockstep on that boot. The rows of K7. | Y2, Y6 for N11 | Y3, Y4, Y8 |
| Y8 | Half precision and the remaining AdvSIMD and crypto sets | L4 to L8. Two agents can split it: L4 and L5, and L6 to L8. | TestFloat for `f16` and the FIPS vectors (T6). `3d:O3` for the groups with V0 to V31, FPSR and FPCR compared, in all rounding modes, the leaders of T1 and T2. K1 and K2 of 3e. | Y2 | Y3, Y4, Y6, Y7 |
| Y9 | `vmlab-v8.5` complete: a Linux guest that uses it | The model selectable by name (B3), by the answer to Q1. The executed encodings of T7. Snapshots record the model. | The four templates to agent ready at 1 and 2 vCPUs on `vmlab-v8.5`, with the `Features` line of `/proc/cpuinfo` naming `atomics`, `asimdrdm`, `fphp`, `asimdhp`, `asimddp`, `jscvt`, `fcma`, `lrcpc`, `ilrcpc`, `sha3`, `sha512`, `asimdfhm`, `flagm`, `flagm2`, `frint`, `sb`, `dit`, `ssbs`. The window lockstep on a Debian boot with the leader `max`. K3 of 3e on that boot. The boot and D2 rows beside those of `vmlab-v8.0`. | Y3, Y4, Y6, Y8 | Y11 |
| Y10 | GICv3: the device model and the CPU interface, with INTx | V2 to V6, the INTx shape of V7, V8 inside vmlab-hv (the field of the machine description), V9 inside vmlab-hv. Not yet selectable from vmlab. | V10, V11 and V12 without the `its` tests. AAVMF to the UEFI shell on a GICv3 machine in boot lockstep. The hwref diff for `gic-version=3,its=off`. Debian to agent ready at 16 vCPUs. The snapshot tests. | none | Y1 to Y9 |
| Y11 | The ITS | V7: the ITS, LPIs, the IORT and `msi-map`. | The `its` tests of kvm-unit-tests. The hwref diff for `gic-version=3`. The four templates at 16 vCPUs with ITS interrupts in `/proc/interrupts` (R4). The fastpath rows of the net bench against the INTx shape. | Y10 | Y3 to Y9 |
| Y12 | vmlab: more than 8 vCPUs for an aarch64 VM | V8 and V9 on the vmlab side: `describe.rs`, the refusal of `src/config/hypervisor.rs` with the new limit, the manual and the PRD. V13. | `just ci::check`. e2e `arch.emulated` with a 16-vCPU machine. The stress rows of V13 and what R5 found. | Y11 | Y9 |
| Y13 | Pointer authentication | Only by the answer to Q2. I16, the five keys, the PAC function as a value that the model picks (`3d:A2`), QARMA5 from its published description for the lockstep with `pauth-qarma5=on` (M2c). | `3d:O3` for the eight format classes and the hint-space forms against `max`. A Debian boot with `paca` and `pacg` in `/proc/cpuinfo`, and its row. | Y9 | any |
| Y14 | BTI | Only by the answer to Q3. N16. | `3d:O3` with guarded pages and every branch type. The JIT rows before and after: the indirect branch path of `3e:H6` must not slow down on `vmlab-v8.0`. | Y9 | any |

Y1 and Y10 have no dependency on each other or on an open card, so two
agents start at once. After Y2, Y3, Y4, Y6, Y7 and Y8 run side by side with
Y10. Nothing in Y1 to Y9 waits for the GIC, and nothing in Y10 to Y12 waits
for an instruction set.

The lead files the cards. This document creates none.

## Questions for the operator

- **Q1. Which machine gets `vmlab-v8.5`?** MQ2 approved the model. The
  `virt` CPU stays v8.0. So a machine needs a way to ask for the model.
  - **O1 (recommended).** A machine setting on the vmlab side that names the
    CPU model, with `vmlab-v8.0` as the default, carried in the existing
    `cpu_model` field of the machine description. Card Y9 adds it and its
    documentation. No existing machine changes.
  - **O2.** No user-facing setting. The model exists for tests and for the
    later M1-class machine only, selected by environment variable in the
    bench and the lockstep.
  - **O3.** `vmlab-v8.5` becomes the default of new machines, and existing
    machines keep `vmlab-v8.0` through their persisted identity. This is a
    guest-visible change under HV-D30 for every new aarch64 VM.
- **Q2. Pointer authentication in `vmlab-v8.5`?** It is mandatory from
  v8.3, the distributions are built for it (`3d:M2`: 575 million `PACIASP`
  per four boots, NOPs today), and a guest that sees it does real work in
  every function prologue. **Recommended: not in the first model.** Y13
  adds it afterwards behind its own ID field, with its measured cost on a
  boot, and the operator decides then if the field is on.
- **Q3. BTI in `vmlab-v8.5`?** Mandatory from v8.5, and the guests are
  built for it (233 million `BTI` per four boots). It puts state on every
  indirect branch of the JIT. **Recommended: not in the first model**, Y14
  afterwards, measured, as Q2.
- **Q4. More than 8 vCPUs only with the ITS?** V7 gives vmlab users a
  16-vCPU machine after Y11 and not after Y10, because Y10 alone runs every
  PCI device on four shared INTx lines. **Recommended: yes, wait for Y11.**
  The option: offer it after Y10 and say in the manual that it is slower on
  I/O until the ITS lands.
- **Q5. The GIC version by vCPU count alone?** V8: GICv2 up to 8 vCPUs,
  GICv3 above, no lab-file key. **Recommended: yes.** The option is a key
  that puts a GICv3 on a small machine, which nobody has asked for.
- **Q6. The vCPU limit.** V9 proposes 32, the host thread count of helios,
  with the map ready for 123. The options: 16 (what V13 tests on all four
  templates), or 123 with a warning when the count is above the host's
  threads.
- **Q7. PAN and LSE in the `virt` CPU (V8).** Not decided here (B4). The
  rows of Y4 and Y6 are its input. This question is listed so that it is
  not lost. No answer is needed before Y4 and Y6 report.

## How to measure again

On a host with QEMU 11, LLVM 23, `dtc` and Python 3:

```sh
# M1: the dump as the 3d design makes it (or `just tcg-a64-dump` in hv/)
python3 measure/sets.py aarch64.json
python3 measure/sets.py aarch64.json LSE RDM LOR FullFP16 FP16FML DotProd SHA3 JS \
  ComplxNum RCPC RCPC_IMMO FlagM AltFPCmp FRInt3264 SB
python3 measure/sets.py aarch64.json PAuth SM4 MTE

# M2: the ID registers of a QEMU CPU model (cpu, machine, vCPUs, name pattern)
python3 measure/idregs.py max
python3 measure/idregs.py max,sve=off,sme=off,pauth=off
python3 measure/idregs.py neoverse-n1 virt 1 'AA64ISAR|AA64PFR|AA64MMFR[012]_'

# V1: the map of QEMU's virt with a GICv3, and its limits
qemu-system-aarch64 -machine virt,gic-version=3,dumpdtb=v3.dtb -cpu max -smp 8 -m 256 -display none
qemu-system-aarch64 -machine virt,gic-version=3,its=off,dumpdtb=v3-noits.dtb -cpu max -smp 8 -m 256 -display none
qemu-system-aarch64 -machine virt,gic-version=3,dumpdtb=v3-200.dtb -cpu max -smp 200 -m 256 -display none
dtc -I dtb -O dts v3.dtb | grep -A14 'intc@'
qemu-system-aarch64 -machine virt -smp 9 -display none -S     # refused: 8 is the limit of the GICv2
```
