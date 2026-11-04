---
id: "32"
kind: research
---

# Sail and Arm spec licences

## Question

Can vmlab-hypervisor generate its TCG decoders and instruction semantics from formal
specs (sail-riscv, Arm's ASL/XML releases, sail-arm) and ship the generated Rust inside
an MIT project? Licence terms, attribution obligations, restrictions, existing precedent,
and permissive fallbacks for aarch64 if Arm's terms are restrictive.

## Findings

Facts are sourced from licence files and repo documents fetched 2026-10-30; interpretation
is labelled and is not legal advice.

### 1. sail-riscv — github.com/riscv/sail-riscv (default branch `master`, file `LICENCE`)

Fact: BSD 2-Clause. The LICENCE file states it covers "all files and directories except
for the third party dependencies in the 'dependencies' directory (which include copies of
their licences)". Copyright (c) 2017-2025, REMS/Cambridge/Edinburgh and contributors;
conditions are the standard two: "Redistributions of source code must retain the above
copyright notice, this list of conditions and the following disclaimer" and the same for
binary distributions. https://github.com/riscv/sail-riscv/blob/master/LICENCE

Fact: the model generates a C/C++ emulator (`sail_riscv_model.{h,cpp}` + `sail_riscv_sim`)
and an experimental Lean emulator; it is "adopted by RISC-V International".
https://github.com/riscv/sail-riscv/blob/master/README.md

Interpretation: code generated or translated from the model is a derivative work; it may
ship inside an MIT project provided the BSD-2 copyright notice and disclaimer are
preserved for those files (vendored-notice pattern, as Pydrofoil does — see §4). The
project cannot relicense those files as pure MIT text.

### 2. Arm's ASL/XML spec and sail-arm

Facts, in-release notice: every XML file in Arm's machine-readable releases (verified in
`SysReg_xml_v86A-2019-12.tar.gz`, `notice.xml`, doc id LES-PRE-20349) carries: "No part
of this document may be reproduced in any form by any means without the express prior
written permission of Arm. No license, express or implied, by estoppel or otherwise to
any intellectual property rights is granted by this document unless specifically stated",
plus "the practice or implementation of the information contained in this document may be
protected by one or more patents", and access is conditional on not using it "for the
purposes of determining whether implementations infringe any third party patents".
Download page: https://developer.arm.com/downloads/-/arm-a-profile-architecture-specification
(the 2019-era tarball URLs remain directly fetchable by curl, without click-through).

Not checked: the verbatim text of the current download click-through licence (DocuSign
clickwrap on the SPA; not machine-retrievable). Treat its terms as unknown.

Interpretation (secondary source, flagged): Alastair Reid, who released the spec while at
Arm, on the 2017 HN thread: "making it available so that researchers, companies and
individuals can use it in software projects" is the intent, and "my reading of the license
is that [generating competing hardware] is the one thing you cannot do with the spec. But
I am not a lawyer." https://news.ycombinator.com/item?id=14174533

Facts, sail-arm — github.com/rems-project/sail-arm (models of Armv8.5/9.3/9.4-A, auto-
translated from Arm's ASL): README — "All models are licensed under the BSD 3-Clause Clear
license in the LICENSE file in the corresponding directory. Artefacts such as the
snapshots in this repository that are produced by Sail are also subject to the same
license." The per-model LICENSE text: "BSD 3-clause Clear License, Copyright (c) 2023 Arm
Limited (or its affiliates), Thomas Bauereiss, Brian Campbell, Alasdair Armstrong,
Alastair Reid, Peter Sewell", ending "NO EXPRESS OR IMPLIED LICENSES TO ANY PARTY'S PATENT
RIGHTS ARE GRANTED BY THIS LICENSE." (BSD-3-Clause-Clear = BSD-3 minus any patent grant.)

Fact, provenance caveat: the Sail README (sail2 branch) says the sail-arm models are
"automatically translated from the Arm-internal ASL reference ... They are provided under
a BSD 3-Clause Clear license, **by agreement with Arm**" — i.e. the permissive terms for
the translation rest on a specific Arm–REMS agreement, not on the public download licence.
https://github.com/rems-project/sail/blob/sail2/README.md

Interpretation: generating an emulator from sail-arm and shipping it inside the MIT repo
(notices preserved) matches how the models are already redistributed and used. Freshly
extracting ASL from new Arm XML releases and redistributing the result is NOT clearly
licensed by any quotable text — avoid that route without Arm's written position.

### 3. Sail toolchain — github.com/rems-project/sail (default branch `sail2`, file `LICENSE`)

Fact: "Sail and the Sail architecture models here, comprising all files and directories
except the ASL-derived Sail code in the aarch64 directory, the logo ... (CC0), and
various exceptions detailed in THIRD_PARTY_FILES.md are subject to the BSD two-clause
licence". So the compiler, prelude, and runtime libraries are BSD-2; the old bundled
`aarch64/` model is excluded ("The ASL derived parts of the ARMv8.3 specification in
aarch64/no_vector and aarch64/full are copyright ARM Ltd"). THIRD_PARTY_FILES.md adds
BSD-3 files from ASLi (visitor.ml) and MIT cJSON in lib/.

Fact: generated emulators are not licence-free output — the C backend emits code that
compiles against Sail's runtime headers/library (sail.h, rts, under lib/ — BSD-2), and the
OCaml backend links Sail's OCaml library (BSD-2). Whatever language we generate, the
runtime support we reuse from Sail carries BSD-2. Neither BSD-2 nor BSD-3-Clear grants
patent rights.

### 4. Projects that already generate emulators/decoders from these specs

| Project | Licence (source) | Output |
|---|---|---|
| Sail backends (rems-project/sail) | BSD-2 | C and OCaml emulators; Coq/Isabelle/HOL4/Rocq/Lean; SystemVerilog (in progress) |
| sail-riscv C/C++ + Lean emulators | BSD-2 | `sail_riscv_sim`; experimental Lean emulator |
| Pydrofoil (github.com/pydrofoil/pydrofoil) | MIT (GitHub licence) | "A fast RISC-V emulator based on the RISC-V Sail model, and an experimental ARM one" — RPython JIT over the Sail-generated C |
| Isla (rems-project/isla) | BSD-2 (LICENSE) | SMT-based symbolic evaluator over Sail models, not an emulator |
| ASLi (rems-project/asl-interpreter, from alastairreid/isa-tools) | BSD-3, "Copyright 2017-2019 Arm Limited, SPDX-Licence-Identifier: BSD-3-Clause" (quoted in Sail's THIRD_PARTY_FILES.md) | OCaml interpreter for Arm's ASL consuming the MRA spec |
| asl-translator (GaloisInc/asl-translator) | BSD-3-Clause (GitHub licence) | ASL → What4/Crucible symbolic semantics |
| aslp (UQ-PAC/aslp, fork of ASLi) | not checked (GitHub: NOASSERTION; parent is BSD-3) | ASL partial evaluator |
| mra_tools (alastairreid/mra_tools) | not checked (no licence file found) | extracts ASL + decode trees from Arm XML; "All generated files include ARM's license notice" |
| sail-arm (rems-project/sail-arm) | BSD-3-Clause-Clear | Sail Arm models; buildable C emulators that boot Linux |
| ASLref (herd/herdtools7 `asllib`, opam `aslref`) | not checked (herdtools7: NOASSERTION) | Arm's official ASL reference interpreter; Arm also publishes the ASL language definition on Arm Developer |

Fact: none of these produce Rust. Sail has no Rust backend (README enumerates the
backends above). Not checked: exhaustive fork/PR search for a third-party Rust generator;
none surfaced in searches. Pydrofoil is the closest precedent for a MIT-licensed emulator
derived from the Sail models.

### 5. Fallback aarch64 decode/semantic sources with permissive licences

- LLVM AArch64 TableGen (`llvm/lib/Target/AArch64/*.td`) — Apache-2.0 WITH LLVM-exception
  (LICENSE.TXT): complete A64 encoding/decode tables, disassembler, MIR/ISel semantics;
  can seed generated Rust decoder tables. Attribution/NOTICE conditions apply; not a full
  executable architectural semantics.
- gem5 ARM ISA (`src/arch/arm/`) — BSD-3-Clause (gem5 LICENSE): full-system Arm ISA
  semantics in gem5's DSL compiled to C++.
- sail-arm (BSD-3-Clause-Clear) — first choice rather than fallback, per §2.
- yaxpeax-arm 0.5.0 — 0BSD (crates.io): pure-Rust aarch64 disassembler (decode only).
- bad64 0.13.0 — Apache-2.0 (crates.io): Rust aarch64 disassembler (decode only).
- For riscv64 decode tables: riscv-opcodes — BSD-3-Clause, "Copyright (c) 2022 RISC-V
  International" (LICENSE).
- Excluded for an MIT clean-room: Linux arch/arm64 (GPL-2.0), binutils opcodes (GPL-3),
  QEMU target/arm (GPL-2.0).

## Conclusion

RISC-V path is clear: sail-riscv (BSD-2) + Sail toolchain/runtime (BSD-2) → generated
riscv64 decoder/semantics may ship inside the MIT project with the BSD-2 notices
preserved for the derived files; Pydrofoil already ships a Sail-model-derived emulator
under MIT.

Arm path is workable but narrower: use the sail-arm models (BSD-3-Clause-Clear,
redistributed "by agreement with Arm") as the semantic source rather than extracting ASL
ourselves from Arm XML. No quotable licence grants redistribution of derivatives of the
raw XML/ASL releases, and no layer in this stack (BSD-2, BSD-3-Clear) provides a patent
grant while Arm's notice reserves patent rights. The patent posture and the click-through
terms are the two items that need a lawyer before first public release of Arm-guest
support.

## Consequences

- Record in the map: riscv64 semantics source = sail-riscv (BSD-2); aarch64 semantics
  source = sail-arm v9.x models (BSD-3-Clause-Clear); do not build tooling that extracts
  ASL from Arm's XML releases for redistribution.
- Ship a THIRD_PARTY_NOTICES carrying the BSD-2 (Sail, sail-riscv) and BSD-3-Clause-Clear
  (sail-arm) texts; generated files keep upstream headers.
- Expected consequence of the "no Rust backend" fact: our generator consumes Sail's
  JSON/IR or the .sail sources via our own Rust emitter — a build-time tool we own, not a
  Sail fork; budget for that work.
- Fallback if legal review kills the Arm path: LLVM .td for aarch64 decode + gem5
  BSD-3 for semantics reference; hand-written semantics reading the ARM is not cleaner
  (the manual carries the same LES-PRE-20349 notice).
- Open item for a human: confirm Arm's position (contact atg-formal@arm.com, per the
  ASLref README) on MIT redistribution of emulators generated from sail-arm.

DONE /home/wil/dev/vmlab-hypervisor/.scratch/plans/vmlab-hypervisor/notes/32-sail-licences.md
