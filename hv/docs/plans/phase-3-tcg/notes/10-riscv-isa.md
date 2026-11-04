---
id: "10"
decision: D7
---

# riscv64 ISA string (approved HV-D30 difference)

## Question
Match QEMU's -cpu max or the implemented subset?

## Answer
Present exactly the extensions vmlab-hv implements (RV64GC plus what the Sail model covers: V, Zb*, Sv39/48/57 and so on); the gap to QEMU's -cpu max (H, vector crypto and others) is an approved HV-D30 difference, listed in the differential allow-list.

## Why
Debian and Fedora riscv64 userlands target rv64gc and the kernel probes the ISA at boot. Owner approved 2026-10-31.

## Consequences
- The exact extension list is fixed once the Sail coverage is mapped in 3a, and recorded.
