---
id: "16"
decision: D13
---

# Online snapshots for TCG guests

## Question
Online or offline only?

## Answer
Same contract as KVM guests: CPU state from the emulator, in the device-state format (HV-D14, HV-D32).

## Why
Users expect snapshots on any machine. Recommended, accepted.

## Consequences
- Translated code is not snapshotted; the JIT cache is rebuilt after restore.
