---
id: "15"
decision: D12
---

# Sub-phase order

## Question
Order of phase 3.

## Answer
3a toolchain, emitter, riscv64 interpreter, lockstep harness, benchmark suite (D2); 3b riscv64 virt machine booting the Debian and Fedora templates to agent ready; 3c Cranelift JIT iterated to D1; 3d aarch64 interpreter and virt machine after the owner's Arm licence check; 3e aarch64 JIT.

## Why
riscv64 is the smaller, permissively licensed model. Recommended, accepted.

## Consequences
- Each sub-phase needs an acceptance list in the spec.
