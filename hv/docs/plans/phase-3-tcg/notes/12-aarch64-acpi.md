---
id: "12"
decision: D9
---

# aarch64 ACPI tables

## Question
Byte-identical or normalised?

## Answer
Matched through the hwref normalised diff, extended to MADT, GTDT, SPCR, IORT, PPTT, DBG2 and the rest.

## Why
Same approach as x86. Recommended, accepted.

## Consequences
- hwref gains aarch64 and riscv64 capture configurations.
