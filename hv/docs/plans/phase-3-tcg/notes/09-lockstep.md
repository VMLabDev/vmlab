---
id: "09"
decision: D6
---

# Lockstep oracles

## Question
Which oracles?

## Answer
Per-instruction: the Sail-generated C++ model linked in-process under its notices; system level: QEMU as a black box through its gdbstub and record/replay.

## Why
Per-step state diffs in-process; whole-boot behaviour from QEMU. Recommended, accepted.

## Consequences
- Deterministic replay on our side (instruction-count timers, recorded device input) is needed for system-level lockstep.
