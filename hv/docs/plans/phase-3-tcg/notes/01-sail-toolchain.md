---
id: "01"
kind: research
---

# Sail toolchain and emitter input

All measurements taken 2026-10-31 on the workstation (Ryzen 9 9950X, CachyOS).
Scratch checkout: `/home/wil/dev/vmlab-hypervisor/target/p3-research/g1/` (gitignored;
the commands below reproduce it). Repos cloned 2026-10-31:
sail-riscv @ fc074f9 (2026-10-29), sail-arm @ 7d05816 (2026-10-13), pydrofoil @ 9c1692c
(2026-07-15), isla master, Sail release 0.20.3 (2026-09-23).

## 1. Sail compiler: what a Rust generator can consume

Backends in Sail 0.20.3 (`sail --help`): C/C++ (`-c`/`--cpp`), OCaml, Lean, Rocq/Coq,
Lem/Isabelle, SMT, LaTeX, HTML/doc, SystemVerilog, plus `--output-sail` (pretty-printed
post-typecheck Sail). No Rust backend (confirms notes/32 §4), and **no JSON semantic dump**.

The IR that matters is **Jib**: Sail typechecks to an AST, lowers to Jib, and every
native backend compiles from it (`sail0203/src/lib/jib_compile.ml`, `jib_optimize.ml`,
`jib_ssa.ml` in the release sources). Jib is first-order, explicit-SSA-ish, gotos-only
control flow — the paper calls it "easy for the AOT compiler to work with"
(arXiv 2503.04389 §3). But mainline `sail` **cannot dump Jib**; the dump lives in a
plugin: **isla-sail** (from rems-project/isla, `isla-sail/` dir) wraps
`sail -plugin plugin.cmxs -isla ... -o OUT` and writes `OUT.ir`, the textual Jib.

Measured, current sail-riscv master, RV64GC+V+H (`rv64d_v128_e64.json` config):

```
isla-sail -dno_cast -O -Oconstant_fold -memo_z3 -c_no_main \
  --isla-preserve encdec_forwards \
  --project model/riscv.sail_project --all-modules \
  --config build/config/rv64d_v128_e64.json -o riscv_master_RV64
# 3m58s wall; riscv_master_RV64.ir = 14,135,859 bytes, 357,664 lines,
# 2,228 top-level defs, 185 enums, 214 registers, 56,452 gotos
```

Other measured outputs, same model, release binary `sail` 0.20.3
(`cmake --build build --target generated_sail_riscv_model`):
` sail_riscv_model.cpp` **19.06 MB / 591,422 lines** in ~6 min single-threaded,
header 0.5 MB, config schema 170 KB. Exact C++ invocation:
`sail-riscv/model/CMakeLists.txt:77-145` (`--cpp --cpp-namespace hart
--cpp-derive-from "protected PlatformInterface" --c-no-main --c-preserve <list>`).

**Stability: poor, and version-coupled.** Facts:
- isla-sail README: "Sail doesn't have any kind of stable external API, so this tool
  is quite liable to breakage" (rems-project/isla, `isla-sail/README.md`).
- Demonstrated: pydrofoil's pinned plugin (sail 0.18 era) fails to load into sail
  0.20.3: `undefined symbol: camlLibsail__Jib_compile.add_special_functions_7117`.
  Current isla master's plugin builds and works against 0.20.3 (both built here).
- sail-riscv master pins the compiler: `cmake/sail_required_version.txt` = `0.20.3`.
- Pydrofoil's documented stack (`opam-state.export`: OCaml 5.2.1 + sail **0.18**,
  submodules sail @ 5d35b1a 2024-08-05) **cannot parse current sail-riscv master**
  (syntax error on `config extensions.Ssnpm.supported_pmlen_7` in
  `model/extensions/pointer_masking/pm_types.sail:32`). Their committed
  `riscv/riscv_model_RV64.ir` is from model `0.5-273-g87f8bb34d6` (2024-10-08) —
  two years old, while the same model today emits ~2× the IR (7.49 MB → 14.1 MB).
- Reproduction at their pin: cloned 87f8bb34d629163563515a82102c48b722ab9058, ran
  `make RISCVMODELCHECKOUT=<pin> regen-sail-ir-files` (pydrofoil Makefile:24-115) with
  the 0.18 switch + z3 on PATH: RV64.ir 7,492,244 B, RV32.ir 7,185,585 B in <1 min
  total; diff vs their committed files = 182/200 lines of 357k (SSA renumbering),
  version string bit-identical. The recipe works; it just lags upstream.

Getting the toolchain (all verified here, ~40 min total): release binary
`sail-Linux-x86_64.tar.gz` (github.com/rems-project/sail/releases/tag/0.20.3-binary;
39 MB, bundles z3) needs nothing else; for the Jib plugin, opam:
`opam init; opam switch create sc ocaml.5.2.1; opam install sail.0.20.3` (~2.5 min),
then `git clone https://github.com/rems-project/isla; make -C isla/isla-sail` (dune).
Pydrofoil's exact stack: `opam switch import opam-state.export --switch sail/` +
`make -C isla/isla-sail` (their Makefile:378-382).

Licence: compiler + runtime BSD-2; generated output is not licence-free (notes/32 §3).

## 2. sail-riscv (riscv/sail-riscv)

- 37,538 lines of Sail in `model/`, restructured (2025-26) from a flat file list to
  a **project file** (`model/riscv.sail_project`, 633 lines) with per-extension
  modules: I, A(Zaamo/Zalrsc), M, B(Zba-Zbs), C(Zca/Zcb), **H**, FD(+Zfh/Zfa),
  **V**, K(+vector crypto Zvk*), Zicsr, Zifencei, Svinval, Zicond, Zicbom/z/p,
  Zicfilp/z (CFI), Zawrs, Zihpm/Zicntr/Sscofpmf/Sstc/Smcntrpmf/Ssqosid, Sdtrig,
  Stateen, pointer masking, bfloat16, Zilx, rmem (RMEM variable, off by default).
  Per-dir lines: extensions 23,383 (V alone 8,490), core 6,294, sys 4,290,
  postlude 2,458, pmp 469.
- MMU: Sv32/39/48/57 VS-stage + Sv32x4–Sv57x4 G-stage
  (`model/sys/vmem.sail:12-13`); H gives vsatp/hgatp two-stage lookup
  (`vmem.sail:270-282`). So RV64GC + Sv48 + H + V is all in one model — vmlab-hv's
  riscv64 target needs no second source.
- Configuration is **JSON in, JSON-schema out**: `type xlen : Int = config
  base.xlen` (`model/core/xlen.sail:8`); knobs in `config/config.json.in` (1,100+
  lines). CMake renders `build/config/rv{32,64}d_v*.json`; `--output-schema` emits
  the validator schema. A Rust emitter can consume both unchanged.
- Platform seam (what a host must implement), in three layers:
  a) `c_emulator/riscv_platform_if.h`: `PlatformInterface` virtuals — mem/fetch/PC/
     reg/CSR callbacks, PTW/TLB callbacks, events, LR/SC reservations, entropy,
     `plat_term_write/read`, initramfs bounds (`--cpp-derive-from`; declared in
     `model/core/callbacks.sail:12-60`).
  b) `model/sys/platform.sail`: an in-model minimal platform — CLINT (msip,
     mtimecmp, mtime for hart<4095), optional SIG, HTIF tohost (`enable_htif`),
     driven by `plat_*` config values.
  c) RAM access goes through instantiations of the generic Sail memory interface
     (`model/core/phys_mem_interface.sail:122-205`: `sail_mem_read/write` with
     access-kind predicates; `write_ram` returns bool, metadata writes separate).
     The C emulator binds a flat RAM; vmlab-hv would bind its bus + MMIO decode.
- Boots Linux, FreeBSD, seL4 from the C++ emulator (README); `os-boot/` carries
  OpenSBI/Linux recipes. CI runs riscv-tests, riscv-vector-tests, riscv-arch-test,
  H-extension tests damo-rv-priv-ats (`test/README.md`) — reusable D26 inputs.
- `handwritten_support/` now holds Lem/Lean/Coq extras for the prover backends (no
  C); FP goes through Berkeley SoftFloat (`model/core/softfloat_interface.sail`,
  `--c-include riscv_softfloat.h`); the C runtime drags GMP (arbitrary-precision
  `%i`) and zlib. `sail_runtime/` is a CMake stub (35 lines).

## 3. sail-arm (rems-project/sail-arm)

Three model generations in-repo, all BSD-3-Clause-Clear (notes/32 §2):

| Dir | Layout | Sail lines | Notes |
|---|---|---|---|
| arm-v8.5-a | old `model/` (13 files) | 124,375 | pre-restructure; snapshots for lem/isabelle |
| arm-v9.3-a | `src/` | 385,976 | + coq/ |
| arm-v9.4-a | `src/` (37 files) | 466,202 | + ir/, isabelle/, lean/; pydrofoil's source |

- v9.4 contents by file: `sysregs_autogen.sail` 160,302 (generated sysreg glue),
  `instrs64_sve.sail` 95,548, `v8_base.sail` 57,139 (ASL shared pseudocode: MMU/TLB
  walks, exceptions, EL0–EL3 — 1,003 EL3 references), `instrs64.sail` 53,106,
  `instrs32.sail` 48,015 (**AArch32 included**, T32/Thumb referenced 533×),
  `instrs64_sme.sail` 42,546 (**SME/SME2**, gated on `HaveSME2()`).
  So: Armv9.4-A A64 + AArch32 + SVE + SME, EL0–EL3, in one model. A64-only use is a
  configuration matter (don't execute at AArch32 state), not a model fork.
- Provenance: auto-translated from Arm's 2023-03 ASL snapshot by asl_to_sail; "last
  tested using Sail version 0.17.1" (v9.4 README). **Not checked here**: v9.4
  `gen_ir` under 0.20.3 (time-boxed out; only RISC-V IR was dumped).
- Non-ASL support files make it an emulator: `fetch.sail` (fetch-decode-execute),
  `mem.sail`/`interface.sail` (ASL memory accessors → Sail memory interface:
  `_Mem_read`/`_Mem_set` at `src/mem.sail:82-116`), `devices.sail` (minimal GIC +
  generic timer at 0x1C04 + PL011-ish UART for Linux; `__ReadGIC`/`__ReadUART`),
  `impdefs.sail`/`reset.sail` (implementation-defined choices, reset values).
  Replacing `devices.sail` with vmlab-hv's own DT + bus is the intended shape.
- Build: `make gen_c` → `c/armv9.c` + gcc -lgmp -lz; `boot/boot.sh` downloads
  kernel + DTB and boots Linux on the C emulator. IR: `make gen_ir` →
  `isla-sail -v 1 -mono_rewrites $(SAIL_SRCS) src/isla_main.sail -splice
  src/isla_splice.sail -o ir/armv9` (Makefile:108-110), then optional
  `isla-preprocess -A ir/armv9.ir -C ../../isla/configs/armv9p4.toml` → `.irx`.

## 4. Pydrofoil (pydrofoil/pydrofoil), MIT

- Pipeline: `isla-sail` textual `.ir` → RPython AOT compiler (`pydrofoil/parse.py`
  parser; SSA IR, bitvector/int ops, softfloat port) → meta-tracing JIT; ARMv9.4
  and CHERIoT frontends beside RISC-V.
- Measured performance (paper, ECOOP 2025, arXiv 2503.04389; SPEC CPU 2017 +
  Linux boot on a TaiShan server): **>230× faster than the Sail C ISS** (which
  runs <1 MIPS); full Pydrofoil ≈ 22.3 MIPS (mcf) / 15.2 MIPS (perlbench);
  **QEMU is 2.2×–97.9× faster than Pydrofoil, geometric mean 26.7×**.
  Ablations: no-JIT 12.3×/8.6× slower (~1.8 MIPS); no-static-opt 129×/89× slower;
  no-bv-int-opts 63×/42×; AOT+JIT beats either alone; the JIT recovers much of
  missing AOT opts (4.5×/5.3× gap) but not vice versa.
- ARM status: boots Linux to the start of `init`, raw binaries only (no ELF
  loader), ~2 h build (`docs/arm.md`). RISC-V boots an old Linux to login prompt.
- Lessons that transfer to a Rust emitter + interpreter + Cranelift JIT:
  1. Static bitvector/int specialization is the biggest AOT win — Jib's
     `%bv_generic`/`%i_generic` and GMP-backed `%i` must be monomorphised away
     (paper §3.2). Budget a specialization pass in our emitter, not the JIT.
  2. `--isla-preserve encdec_forwards` keeps the decode functions in the IR;
     Pydrofoil AOT-compiles per-instruction execute clauses and dispatches on
     decode — the same shape as a Cranelift basic-block compiler.
  3. Clock: the model ticks time every 100 instructions (artefact, not ISA).
     QEMU checks the clock only at basic-block entry and deopts on timer events
     (paper §5) — copy that.
  4. The model has no TLB — every access walks page tables. Either JIT-specialise
     walks or keep a host-side TLB driven by the model's `tlb_add_callback`/
     `tlb_flush_*` — we own the platform seam, so the latter is cheap for us.
  5. Pin sail + isla + model together and regenerate; pins rot in months (§1).

## 5. Other Sail→native / →Rust efforts

- **Lean emulator** (in-tree, experimental): sail-riscv `lean_emulator/`, CMake
  target `generated_lean_executable_rv64d`, built in CI (`compile-lean.yml`).
- **C → WebAssembly**: emscripten over the C backend output (+gmp/zlib/softfloat
  ports) shown working (sail-riscv discussion #674). Reuse route, not a backend.
- **Isla family**: isla (SMT symbolic execution), isla-testgen (instruction tests
  from Sail — candidate D26 input), isla-axiomatic (litmus; now being rewritten
  in **Rust** — Cargo.toml in rems-project/isla master).
- **Capstone** generates a RISC-V disassembler from sail-riscv decode
  (sail-riscv #658) — decode-table reuse precedent.
- **sail-riscv-oracle** (github.com/yufengzjj/sail-riscv-oracle, 2026-10): Rust
  crate embedding the sail-riscv C++ model via cc/bindgen as an in-process
  **differential-testing oracle** (per-step architectural change lists, RV32+RV64).
  Exactly the D22/D26 lockstep pattern on the C-FFI route — but **no licence
  file** (NOASSERTION), 0 stars: copy the pattern, not the code.
- Sail issue #1407 "Add native C++ code generation" (open). CREATOR-Sail (2026,
  web simulator) — not checked.

## Constraints for the plan

- The only full-semantics machine-readable Sail output is the textual Jib `.ir`
  from isla-sail, a plugin against unstable compiler internals: pin (sail, isla,
  model) triples, regenerate in CI, never hand-edit or blindly vendor `.ir`.
  No JSON Jib exists; Pydrofoil's MIT `parse.py` is the reference text parser.
- Both models and both toolchain routes are permissively licensed (notes/32);
  the Arm patent/click-through caveats from notes/32 still stand.
- Model churn is real: sail-riscv doubled its emitted IR in two years, and
  pydrofoil's pinned compiler cannot parse current master.

## Open questions this raises

- Q1: Consume `.ir` text (own Rust parser, Pydrofoil-style) or link Sail's OCaml
  `libsail` and emit from Jib in-process (isla-style plugin)? Text is versioned and
  diffable; OCaml linking gives type info but drags an OCaml build into our toolchain.
- Q2: Baseline sail 0.20.3 + current isla master (parses both models today),
  accepting plugin churn? And who verifies sail-arm v9.4 `gen_ir` under 0.20.3?
- Q3: Emission cost at Rust scale: 14 MB IR → how much Rust, what compile time?
  C++ weighs 591k lines / 19 MB for the same model — plan crates or generated
  `.rs` artefacts, and measure early.
- Q4: Does the `%i`/GMP surface vanish under `-Oconstant-fold` + specialization,
  or does our interpreter need a bignum fallback (RMEM, vector-length math)?
- Q5: D26 lockstep oracle: embed the C++ model in-process (sail-riscv-oracle
  pattern, ours under BSD-2 notices) or drive `sail_riscv_sim` externally?
  In-process gives per-step change lists; external avoids build coupling.

DONE /home/wil/dev/vmlab-hypervisor/.scratch/plans/phase-3-tcg/notes/01-sail-toolchain.md
