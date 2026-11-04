---
id: "08"
decision: D5
---

# Transitional virtio

## Question
How do VM machines present virtio devices?

## Answer
QEMU-identical transitional virtio for VM machines: legacy I/O BAR0, MSI-X BAR1, modern BAR4, IDs 1af4:1000-1003 and subsystems as captured. Containers keep the phase-1 modern-only transport.

## Why
A Windows template's boot driver bound to these IDs; a mismatch risks INACCESSIBLE_BOOT_DEVICE. Recommended option, accepted.

## Consequences
- The virtio transport gains the legacy (0.9.5) register interface and a QEMU-shaped capability/BAR layout, selectable per machine.
