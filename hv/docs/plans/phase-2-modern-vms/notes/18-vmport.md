---
id: "18"
decision: D15
---

# VMware backdoor (vmport) — approved HV-D30 difference

## Question
QEMU's q35 enables the VMware I/O-port backdoor and vmmouse by default. Implement or omit?

## Answer
Omit. Approved HV-D30 difference. The xHCI tablet provides the absolute pointer; the macOS phase may add vmport if it needs it.

## Why
Nothing in vmlab relies on it and QEMU itself supports vmport=off. Owner approved 2026-10-31.

## Consequences
- The hwref allow-list moves vmport/vmmouse from pending to approved (D15).
