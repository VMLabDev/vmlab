---
id: "13"
decision: D10
---

# Multi-vCPU

## Question
MTTCG from the start?

## Answer
Designed for multi-threaded TCG from day one (per-vCPU exclusive monitors, LR/SC, memory ordering on an x86 TSO host); the interpreter may run single-threaded; the JIT stage delivers MTTCG.

## Why
D1 is measured at 4 vCPUs too. Recommended, accepted.

## Consequences
None beyond those already on the map.
