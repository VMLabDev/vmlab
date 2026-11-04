---
slug: phase-3-tcg
status: specified
created: 2026-10-31
---

# Phase 3 of vmlab-hv: aarch64 and riscv64 guests through our own TCG

## Destination
Phase 3 is finished when vmlab's riscv64 and aarch64 profiles boot their existing QEMU-built templates (the local Debian, Fedora, Ubuntu and Alpine VM templates; cross-arch containers are out of scope in vmlab's PRD §18) on vmlab-hv on an x86_64 host, through vmlab-hv's own Sail-derived CPU emulation, with QEMU-identical `virt` hardware; every arch-relevant e2e feature passes on `--hypervisor vmlab`; the lockstep harness agrees with the Sail models and QEMU on its corpus; and the JIT reaches at least QEMU TCG's measured speed (D1), tracked by continuous benchmarks (D2). riscv64 lands first; aarch64 follows the owner's confirmation of Arm's licence position on sail-arm.

## Grounding
Research: `notes/01-sail-toolchain.md`, `notes/02-virt-machines.md`, `notes/03-architecture-baseline.md`.
- Sail's usable IR is textual Jib (`.ir`) via the isla-sail plugin; sail-riscv emits 14.1 MB / 357k lines (Sail 0.20.3 + isla master); Sail has no stable external API (notes/01).
- sail-arm v9.4 is 466k lines: A64 + AArch32 + SVE + SME, EL0-EL3 in one model (notes/01).
- Pydrofoil (Sail-derived, RPython JIT) is 26.7x slower than QEMU TCG geomean; its interpreter mode another 8.6-12.3x slower; specialisation bought 42-63x before the JIT (notes/03).
- QEMU TCG on this host, boot to agent ready: aarch64 28.2 s (smp1) / 17.9 s (smp4); riscv64 52.3 s / 28.3 s. xz -9: aarch64 2.6 s, riscv64 34 s; sha256 256 MiB ~1.9 s both (notes/03).
- aarch64 virt as vmlab runs it: ACPI boot via AAVMF, GICv2 + GICv2m, PL011, PSCI (SMC from ACPI), architectural timer; riscv64 virt: PLIC + CLINT, no AIA, 16550, `-cpu max` ISA string includes H and vector crypto (notes/02).
Binding decisions from `../vmlab-hypervisor/MAP.md` (HV-Dn): D3 x86_64 hosts only, no x86 TCG; D13 parity gate; D22 Sail-derived interpreter, lockstep harness, then Cranelift JIT; D26 lockstep differential tier for TCG; D28 TCG at least 50% of QEMU TCG; D34 sail-riscv and sail-arm through our own Rust emitter, no ASL extraction. Licences: `../vmlab-hypervisor/notes/32-sail-licences.md`.
Local templates exist: aarch64 (alpine-3.23, debian-13, fedora-44, ubuntu-24.04, home-assistant) and riscv64 (debian-13, fedora-42) under `~/.local/share/vmlab/templates/`.

## Decisions
| ID | Decision | Struck by | Recorded in |
|----|----------|-----------|-------------|
| D2 | Continuous benchmarking from milestone one: suite vs QEMU, result history, regression check, profiling hooks | | `notes/05-benchmarking.md` |
| D3 | Emitter input: textual Jib from a pinned Sail + isla-sail toolchain | | `notes/06-emitter-input.md` |
| D4 | Generated code committed in its own crate, reproducibility-checked | | `notes/07-generated-code.md` |
| D5 | One pipeline: specialised IR drives reference interpreter and Cranelift trace JIT; overrides behind lockstep | | `notes/08-execution-architecture.md` |
| D6 | Lockstep: in-process Sail C++ model per instruction, QEMU black box per boot | | `notes/09-lockstep.md` |
| D7 | Approved HV-D30 difference: riscv64 ISA string is what we implement | | `notes/10-riscv-isa.md` |
| D8 | aarch64 GICv2 + v2m | | `notes/11-aarch64-gic.md` |
| D9 | aarch64 ACPI via the normalised hwref diff | | `notes/12-aarch64-acpi.md` |
| D10 | MTTCG designed in from day one | | `notes/13-mttcg.md` |
| D11 | SoftFloat plus lockstep-gated host-FPU fast paths | | `notes/14-floating-point.md` |
| D12 | Sub-phases 3a-3e: riscv64 first, aarch64 after the licence check | | `notes/15-sub-phases.md` |
| D13 | Online snapshots for TCG guests, same contract as KVM | | `notes/16-tcg-snapshots.md` |
| D1 | Speed bar: at least 100% of QEMU TCG (supersedes HV-D28's 50% for phase 3) | | `notes/04-speed-bar.md` |
| D14 | aarch64 proceeds on sail-arm's published BSD-3-Clause-Clear terms (granted by agreement with Arm): vmlab-hv generates its aarch64 emulator from sail-arm and commits the derived crate with the notice preserved in THIRD_PARTY_NOTICES. vmlab-hv never extracts ASL from Arm's XML releases itself. No patent grant is assumed. | owner, 2026-11-01 | MAP; notes/32 (vmlab-hypervisor plan) |

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

- [ ] Q15 (decision) [status: open] — How D1 (at least 100% of QEMU TCG) is treated: the first like-for-like measurement is QEMU TCG about 3,900 MIPS on CoreMark against the interpreter's 51 MIPS (about 76x). Decide once the 3c JIT has a measured number.

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
- The QEMU `-cpu max` extensions vmlab-hv does not implement (D7)
- x86 guest emulation and ARM hosts (HV-D3)
