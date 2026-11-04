---
slug: phase-3-tcg
kind: spec
created: 2026-10-31
---

# Phase 3 of vmlab-hv: aarch64 and riscv64 guests through our own TCG — specification

Traces: `Dn` is a row in this plan's `MAP.md`. `HV-Dn` is in
`../vmlab-hypervisor/MAP.md`. Research is in `notes/01-03`.

## Problem

On x86_64 hosts, vmlab runs aarch64 and riscv64 VMs only through QEMU's TCG.
`vmlab-hv` has no CPU emulation, so those profiles and their templates (local:
aarch64 Debian 13, Fedora 44, Ubuntu 24.04, Alpine 3.23; riscv64 Debian 13,
Fedora 42) cannot move off QEMU.

## Solution

A CPU emulator generated from the formal Sail models, faster than QEMU's TCG
(D1). The pipeline:

1. A pinned Sail plus isla-sail toolchain in a container emits textual Jib
   (D3).
2. Our Rust emitter specialises that Jib into a typed IR (D5) and commits the
   generated Rust (D4).
3. The IR drives a reference interpreter and a Cranelift trace JIT with an
   inlined softmmu TLB (D5).

Correctness is held by lockstep comparison against the Sail C++ model per
instruction and against QEMU per boot (D6). Performance is held by a
continuous benchmark suite against QEMU TCG on this host (D2). Each `virt`
machine reproduces what QEMU presents, apart from approved differences (D7,
D8, D9).

The work is done in sub-phases (D12):

| Sub-phase | Delivers |
|---|---|
| 3a | Toolchain, emitter, riscv64 interpreter, lockstep and benchmarks |
| 3b | riscv64 `virt` machine |
| 3c | JIT |
| 3d | aarch64 interpreter and `virt` machine, after the owner's licence check |
| 3e | aarch64 JIT |

## User stories

1. As a vmlab user, my existing riscv64 Debian and Fedora templates boot on `vmlab-hv` and reach agent ready on an x86_64 host (3b).
2. As a vmlab user, a riscv64 or aarch64 VM on `vmlab-hv` is at least as fast as under QEMU TCG: boot time and CPU work, with 1 and with 4 vCPUs (3c, 3e).
3. As a vmlab user, online snapshots, display, input and networking work on TCG guests as on KVM guests (D13).
4. As a developer, I can regenerate the emulator from a newer Sail model with one recipe and review the diff (D3, D4).
5. As a developer, every change shows its effect on the benchmark history and fails the check if it regresses a workload (D2).
6. As a developer, a semantic mismatch between our emulator and the Sail model is caught at the first diverging instruction (D6).

## Implementation decisions

### Crates

| Crate | Contents |
|---|---|
| `vmlab-hv-sail-ir` | Jib text parser into a Rust AST. Pure, fuzzed. |
| `vmlab-hv-tcg-ir` | The typed emulation IR: ops over u8–u128, explicit flags, memory ops through a softmmu interface, exceptions as IR terminators. Plus the specialisation passes: bitvector-to-integer lowering, constant folding, dead-path removal, register-file layout. |
| `vmlab-hv-tcg-emit` | Jib AST to IR, run by `just tcg-generate` against the pinned toolchain. Its output is committed generated Rust in `vmlab-hv-tcg-riscv64` (later `-aarch64`): decoder tables and per-instruction IR builders, with Sail notices. |
| `vmlab-hv-tcg` | The runtime: CPU state, the softmmu (TLB, page walks driven by the generated semantics, MMIO dispatch to the VMM's buses), exceptions and interrupts, the reference interpreter, the block/trace cache with self-modifying-code invalidation, and later the Cranelift JIT. It is driven per vCPU thread by `vmlab-hv-vmm` in place of `KVM_RUN` when the machine's accel is `tcg`. |
| `vmlab-hv-lockstep` | The per-instruction oracle: the Sail-generated C++ model built in the toolchain container and linked through FFI, exposing step, state get/set and memory hooks. Plus a harness that runs our interpreter or JIT and the oracle side by side, reporting the first differing architectural state. |
| `bench/` | The suite, its recipes and the result history (D2). |

### Toolchain (D3, D4)

`toolchain/sail/` holds the container that builds these, with versions and
sha256 sums recorded:
- Sail 0.20.3
- isla-sail at a pinned isla commit
- pinned sail-riscv (and later sail-arm) commits

Two recipes:
- `just tcg-generate` emits the `.ir`, runs the emitter and writes the
  generated crates.
- `just tcg-generate-check` regenerates and diffs, for CI.

The generated crate's size and compile time are measured on the first
generation. If compile time exceeds a few minutes, the crate is split by
instruction group.

### Execution (D5, D10, D11)

- **Interpreter.** It executes the IR directly with a decoded-block cache
  keyed by physical PC and an exception check at block ends. It is the
  correctness reference and the cold path.
- **JIT (3c).** Cranelift compiles hot traces formed from block-execution
  counts. Trace exits are chained. The TLB lookup is inlined, MMIO exits to the
  VMM, and guest registers live in host registers within a trace.
  Self-modifying code is detected through page-protection or generation
  counters on code pages. Interrupts are checked at trace exits and on
  backward branches.
- **Multi-vCPU (D10).** One host thread per vCPU, with per-vCPU exclusive
  monitors and LR/SC reservations. Memory ordering is mapped explicitly for
  aarch64 and riscv atomics on the x86 TSO host.
- **Floating point (D11).** Berkeley SoftFloat, vendored, for correctness.
  Host-FPU fast paths are allowed only when lockstep proves them equal.
- **Hand-written overrides (D5)** are registered by instruction. The lockstep
  harness tests each one against its generated twin.

### Machines

**riscv64 `virt` (3b)** matches notes/02's capture:
- PLIC and CLINT, a 16550, a PCIe ECAM host
- virtio PCI devices, plus virtio-gpu as vmlab uses it
- xHCI with the tablet
- SBI through OpenSBI, as QEMU's firmware path uses
- the edk2 RISC-V pflash pair vmlab ships
- the ISA string per D7, with the exact list recorded when 3a maps the Sail
  model's coverage
- `acpi=off`, device tree generated

**aarch64 `virt` (3d)** matches notes/02's capture:
- GICv2 + GICv2m (D8), PL011, the architectural timer
- PSCI over SMC and HVC
- AAVMF pflash, ACPI tables matched through the normalised diff (D9)
- PCIe ECAM and the same virtio devices

Both machines reuse `vmlab-hv`'s devices, display protocol, snapshot format
and control protocol. Online snapshots work (D13): the emulator's CPU state is
saved, and the translation cache is rebuilt after restore.

### Benchmarks (D1, D2)

`bench/` runs each workload under `vmlab-hv` and QEMU TCG on the same host,
pinned, with the governor fixed and N repeats:

- boot to agent ready
- sha256 over 256 MiB and `xz -9` over 32 MiB inside the guest
- 1 and 4 vCPUs
- per guest arch

Each run appends one JSON line to `bench/history/<arch>.jsonl`: commit, host,
guest, workload, vCPUs, median and spread, and the QEMU baseline.
`just bench-report` shows the trend. `just bench-check` fails on a regression
beyond the measured noise, and runs on this host before a merge that touches
`vmlab-hv-tcg*`.

Profiling hooks:
- per-block counts and hot-trace dumps
- perf map files for JIT code
- an interpreter-only switch

## Testing decisions

- **Unit:** the Jib parser (fuzzed, and round-tripping the whole sail-riscv `.ir`), IR passes, the emitter, softmmu and TLB.
- **Lockstep (D6):**
  - A per-instruction corpus: riscv-tests and riscv-arch-test, plus random instruction streams, run against the in-process Sail model with the first divergence reported.
  - Boot-level comparison against QEMU on the templates under deterministic replay (from 3b).
- **S1:** real templates boot to agent ready under the memory cap (3b, 3d).
- **e2e:** `arch.emulated` and the arch-relevant VM features on `--hypervisor vmlab` (S4).
- **Benchmarks:** the D2 suite. D1's bar gates flipping a TCG default.
- **Merge bar:** `just ci::check`, plus `just tcg-generate-check`.
- **Safety:** every VM-booting test runs under a memory cap with at most two VMs at once.

## Out of scope

- x86 guest emulation (HV-D3).
- Cross-arch containers.
- QEMU `-cpu max` extensions beyond those we implement (D7).

## Open questions

- Whether Sail's unbounded integers survive specialisation anywhere hot enough to need a bignum fast path (notes/01 Q4); 3a answers it.
- The exact riscv64 extension list (D7), fixed in 3a.
- Arm's licence position on sail-arm-derived code: an owner task before 3d.
