---
id: "23"
decision: D22
---

# TCG approach

## Question
Interpreter first, JIT substrate, harness?

## Answer
A Sail-derived interpreter first, riscv64 then aarch64, with a lockstep differential harness against the Sail model (TestRIG-style) and against QEMU from day one; then a Cranelift JIT behind a benchmark gate (D28).

## Why
The verifiable path agents can grind on; Unicorn is excluded because it is GPLv2. Recommended option, accepted.

## Consequences
- The licence of the Arm Sail/ASL model must allow generating code into an MIT project (research Q32).
- The harness needs deterministic replay of timers and interrupts on both sides.
