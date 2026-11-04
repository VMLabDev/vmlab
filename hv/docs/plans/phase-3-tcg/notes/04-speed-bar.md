---
id: "04"
decision: D1
---

# Speed bar: at least QEMU TCG's speed

## Question
What performance must phase 3's emulation reach before a default flips? HV-D28 set TCG at 50% of QEMU's TCG.

## Answer
At least 100% of QEMU TCG's measured speed on the same host and guest (boot-to-login time and the in-guest CPU benchmark from notes/03), for each of riscv64 and aarch64, with 1 and with 4 vCPUs. This supersedes HV-D28's TCG figure for phase 3; HV-D28's KVM bars are unchanged.

## Why
Owner's call (2026-10-31): the design (Sail-derived semantics plus a Cranelift JIT with cross-block optimisation) should be able to match or beat QEMU's TCG rather than trail it.

## Consequences
- The bar applies to the JIT; the Sail-derived interpreter is a correctness stage, not a shippable speed.
- The JIT design must be chosen for code quality as well as compile latency (decision pending on architecture).
- Benchmarks and the measuring harness are part of the phase-3 gate, run against QEMU TCG on the same host.
