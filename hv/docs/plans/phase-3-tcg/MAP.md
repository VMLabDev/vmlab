---
slug: phase-3-tcg
status: specified
created: 2026-10-31
---

# Phase 3 of vmlab-hv: aarch64 and riscv64 guests through our own TCG

## Destination
Phase 3 is finished when vmlab's riscv64 and aarch64 profiles boot their existing QEMU-built templates (the local Debian, Fedora, Ubuntu and Alpine VM templates; cross-arch containers are out of scope in vmlab's PRD §18) on vmlab-hv on an x86_64 host, through vmlab-hv's own CPU emulation (Sail-derived for riscv64; for aarch64 decoded from the LLVM tables with our own semantics, D15), with QEMU-identical `virt` hardware; every arch-relevant e2e feature passes on `--hypervisor vmlab`; the lockstep harness agrees with its oracle on its corpus (the Sail model for riscv64, QEMU for aarch64, D16); and the JIT reaches at least QEMU TCG's measured speed (D1), tracked by continuous benchmarks (D2). The order of riscv64 and aarch64 is free: 3d needs nothing from 3b (D12, `../phase-3d-aarch64/DESIGN.md`). The design of the aarch64 JIT (3e) is `../phase-3e-aarch64-jit/DESIGN.md`.

## Grounding
Research: `notes/01-sail-toolchain.md`, `notes/02-virt-machines.md`, `notes/03-architecture-baseline.md`.
- Sail's usable IR is textual Jib (`.ir`) via the isla-sail plugin; sail-riscv emits 14.1 MB / 357k lines (Sail 0.20.3 + isla master); Sail has no stable external API (notes/01).
- sail-arm v9.4 is 466k lines: A64 + AArch32 + SVE + SME, EL0-EL3 in one model (notes/01).
- Pydrofoil (Sail-derived, RPython JIT) is 26.7x slower than QEMU TCG geomean; its interpreter mode another 8.6-12.3x slower; specialisation bought 42-63x before the JIT (notes/03).
- QEMU TCG on this host, boot to agent ready: aarch64 28.2 s (smp1) / 17.9 s (smp4); riscv64 52.3 s / 28.3 s. xz -9: aarch64 2.6 s, riscv64 34 s; sha256 256 MiB ~1.9 s both (notes/03).
- aarch64 virt as vmlab runs it: ACPI boot via AAVMF, GICv2 + GICv2m, PL011, PSCI (SMC from ACPI), architectural timer; riscv64 virt: PLIC + CLINT, no AIA, 16550, `-cpu max` ISA string includes H and vector crypto (notes/02).
Binding decisions from `../vmlab-hypervisor/MAP.md` (HV-Dn): D3 x86_64 hosts only, no x86 TCG; D13 parity gate; D22 Sail-derived interpreter, lockstep harness, then Cranelift JIT; D26 lockstep differential tier for TCG; D28 TCG at least 50% of QEMU TCG; D34 sail-riscv and sail-arm through our own Rust emitter, no ASL extraction (for aarch64 replaced by D15: no sail-arm). Licences: `../vmlab-hypervisor/notes/32-sail-licences.md`.
Local templates exist: aarch64 (alpine-3.23, debian-13, fedora-44, ubuntu-24.04, home-assistant) and riscv64 (debian-13, fedora-42) under `~/.local/share/vmlab/templates/`.

## Decisions
| ID | Decision | Struck by | Recorded in |
|----|----------|-----------|-------------|
| D2 | Continuous benchmarking from milestone one: suite vs QEMU, result history, regression check, profiling hooks | | `notes/05-benchmarking.md` |
| D3 | Emitter input: textual Jib from a pinned Sail + isla-sail toolchain | | `notes/06-emitter-input.md` |
| D4 | Generated code committed in its own crate, reproducibility-checked | | `notes/07-generated-code.md` |
| D5 | One pipeline: specialised IR drives reference interpreter and Cranelift trace JIT; overrides behind lockstep. For the JIT, D19 replaces the Cranelift back end and the traces | | `notes/08-execution-architecture.md` |
| D6 | Lockstep: in-process Sail C++ model per instruction, QEMU black box per boot. For aarch64 the per-instruction oracle is QEMU too (D16) | | `notes/09-lockstep.md` |
| D7 | Approved HV-D30 difference: riscv64 ISA string is what we implement | | `notes/10-riscv-isa.md` |
| D8 | aarch64 GICv2 + v2m | | `notes/11-aarch64-gic.md` |
| D9 | aarch64 ACPI via the normalised hwref diff | | `notes/12-aarch64-acpi.md` |
| D10 | MTTCG designed in from day one | | `notes/13-mttcg.md` |
| D11 | SoftFloat plus lockstep-gated host-FPU fast paths | | `notes/14-floating-point.md` |
| D12 | Sub-phases 3a-3e. 3d (aarch64) has no licence check before it and does not wait for 3b or 3c (D15) | | `notes/15-sub-phases.md` |
| D13 | Online snapshots for TCG guests, same contract as KVM | | `notes/16-tcg-snapshots.md` |
| D1 | Speed bar: at least 100% of QEMU TCG (supersedes HV-D28's 50% for phase 3) | | `notes/04-speed-bar.md` |
| ~~D14~~ | Replaced by D15 (operator, 2026-11-26). ~~aarch64 proceeds on sail-arm's published BSD-3-Clause-Clear terms (granted by agreement with Arm): vmlab-hv generates its aarch64 emulator from sail-arm and commits the derived crate with the notice preserved in THIRD_PARTY_NOTICES. vmlab-hv never extracts ASL from Arm's XML releases itself. No patent grant is assumed.~~ | owner, 2026-11-01 | MAP; notes/32 (vmlab-hypervisor plan) |
| D15 | aarch64 uses no sail-arm and nothing derived from Arm's ASL or XML. Decode is generated from the LLVM AArch64 TableGen tables (Apache-2.0 WITH LLVM-exception, notice in THIRD_PARTY_NOTICES) by our own build-time tool at a pinned LLVM. The semantics are our own code in the typed IR of `vmlab-hv-tcg-ir`. Replaces HV-D34 for aarch64, and D14. | operator, 2026-11-26 | `../phase-3d-aarch64/DESIGN.md` (sources, 1, 2) |
| D16 | The aarch64 oracle is a `qemu-system-aarch64` process, per instruction (TCG plugin stream, gdbstub to set state) and per boot (our machine follows QEMU's stream). A test tool only: no product path starts QEMU. Agents do not read QEMU's `target/arm`, `fpu`, `tcg` or `accel/tcg`. | operator, 2026-11-26 | `../phase-3d-aarch64/DESIGN.md` (sources, 3) |
| D17 | The aarch64 CPU core is apart from the `virt` machine: it sees memory, an IRQ and a FIQ line, timer outputs, a hypercall exit and a hook for machine system registers. Exception levels, the granule, the instruction sets and the CPU identity are data, so that an Apple M1 machine is possible later. 3d implements EL0 and EL1, and PSCI is served by the `virt` machine on HVC and SMC. | operator, 2026-11-26 | `../phase-3d-aarch64/DESIGN.md` (G4, M1 section) |
| D18 | 3d is accepted when the aarch64 templates (Debian 13, Fedora 44, Ubuntu 24.04, Alpine 3.23) boot to agent ready on vmlab-hv, snapshots work and e2e `arch.emulated` runs on vmlab-hv. Then vmlab selects vmlab-hv for aarch64. D1's speed bar stays with 3e. | operator, 2026-11-26 | `../phase-3d-aarch64/DESIGN.md` (5, cards) |
| D19 | The aarch64 JIT compiles every pre-decoded block when it is translated, with an x86-64 emitter of our own in a crate shared with riscv64 (`vmlab-hv-tcg-jit`), not with Cranelift. Measured on the same blocks: Cranelift 24 to 160 µs per block, our emitter 0.2 to 1.1 µs, Cranelift's code 8 to 53 % faster. The interpreter stays as the reference and the per-block fallback. Blocks are chained, the softmmu lookup is inlined, the code cache is per vCPU. Compiled code is in a double mapping, never writable and executable in one view. The first bar is T1 (the four templates boot at 1 and 2 vCPUs with margin, 150 MIPS on a boot), and at T1 the JIT is on by default. D1 stays the tracked bar. Cranelift stays possible later as a tier for hot regions. Replaces "Cranelift trace JIT" in D5, for riscv64 (3c) too. | operator, 2026-11-27 | `../phase-3e-aarch64-jit/DESIGN.md` (3, 4, 8) |

## Frontier
- [x] Q1 (research) [status: answered] — Sail toolchain: what IR or output a Rust emitter can consume, model sizes, prior emitters (Pydrofoil).
- [x] Q2 (research) [status: answered] — vmlab's foreign-arch usage and what QEMU's aarch64 and riscv64 `virt` machines present to guests.
- [x] Q3 (research) [status: answered] — Emulator architecture options and QEMU TCG's measured baseline on this host.

- [x] ~~Q4 (decision) — Emitter input: textual Jib via a pinned Sail + isla-sail toolchain, or link OCaml libsail?~~ → D3
- [x] ~~Q5 (decision) — Generated code: committed to the repo, or generated at build time?~~ → D4
- [x] ~~Q6 (decision) — Execution architecture that can reach D1's 100% bar.~~ → D5
- [x] ~~Q7 (decision) — Lockstep oracles: in-process Sail model, external simulator, QEMU.~~ → D6
- [x] ~~Q8 (decision) — riscv64 ISA string: match QEMU's `-cpu max` or ship the subset the Sail model implements (an HV-D30 question).~~ → D7
- [x] ~~Q9 (decision) — aarch64 interrupt controller: GICv2 + v2m as QEMU presents, or GICv3.~~ → D8
- [x] ~~Q10 (decision) — aarch64 ACPI tables: byte-identical or normalised diff.~~ → D9
- [x] ~~Q11 (decision) — Multi-vCPU: MTTCG from the start or later.~~ → D10
- [x] ~~Q12 (decision) — Floating point: soft-float everywhere, or host-FPU fast paths.~~ → D11
- [x] ~~Q13 (decision) — Sub-phase order.~~ → D12
- [x] ~~Q14 (decision) — Online snapshots for TCG guests.~~ → D13

- [ ] Q16 (decision) [status: open] — aarch64 CPU presented to the guest: Armv8.0-A with CRC32, AES, SHA1 and SHA2, and no PMU, in place of QEMU's `-cpu max` (an HV-D30 question, as D7 was). `../phase-3d-aarch64/DESIGN.md` Q2.
- [ ] Q17 (decision) [status: open] — The QEMU adapter also runs riscv64 until 3b: remove it after 3d, or after 3b. `../phase-3d-aarch64/DESIGN.md` Q3.
- [ ] Q18 (decision) [status: open] — Select vmlab-hv for aarch64 at the end of 3d at interpreter speed, or after 3e. `../phase-3d-aarch64/DESIGN.md` Q4.
- [ ] Q15 (decision) [status: open] — How D1 (at least 100% of QEMU TCG) is treated: the first like-for-like measurement is QEMU TCG about 3,900 MIPS on CoreMark against the interpreter's 51 MIPS (about 76x). Decide once the 3c JIT has a measured number. For aarch64 (2026-11-27): QEMU 2,311 to 2,703 MIPS on CoreMark against the interpreter's 120 to 132, and 467 to 690 MIPS average on a boot against 54. Card J9 of `../phase-3e-aarch64-jit/DESIGN.md` brings the measured JIT numbers.
- [x] ~~Q19 (decision) — The JIT back end: our own x86-64 emitter, or Cranelift as D5 says.~~ → D19: our own emitter (operator, 2026-11-27). `../phase-3e-aarch64-jit/DESIGN.md` Q1.
- [x] ~~Q20 (decision) — The JIT on by default at the bar T1, or only when D1 is met.~~ → D19: at T1 (operator, 2026-11-27). `../phase-3e-aarch64-jit/DESIGN.md` Q2.
- [x] ~~Q21 (decision) — Compiled code in a double mapping, never writable and executable in one view, or one mapping with all three rights.~~ → D19: the double mapping (operator, 2026-11-27). `../phase-3e-aarch64-jit/DESIGN.md` Q3.

## Fog

## Accepted unknowns
- Toolchain container and pin upgrades (D3), generated-crate splitting by measured compile time (D4): implementation
- The typed IR design and the override registry (D5), deterministic replay for system lockstep (D6): spec work for 3a
- The exact riscv64 extension list (D7): fixed when 3a maps Sail coverage, recorded in the allow-list
- hwref captures for aarch64 and riscv64 virt (D9), SoftFloat notices (D11), per-sub-phase acceptance lists (D12): spec work
- JIT cache rebuilt after restore, not snapshotted (D13): design consequence, no question
- Bignum fallback for Sail's unbounded integers if specialisation leaves any (notes/01 Q4): found during 3a

## Out of scope
- Cross-arch containers (vmlab PRD §18)
- The Apple M1 machine (D17 keeps it possible; it is a later phase)
- SVE, SME, AArch32 and EL3 for aarch64
- The QEMU `-cpu max` extensions vmlab-hv does not implement (D7)
- x86 guest emulation and ARM hosts (HV-D3)
