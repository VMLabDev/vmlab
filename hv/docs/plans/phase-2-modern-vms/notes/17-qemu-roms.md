---
id: "17"
decision: D14
---

# QEMU-built option ROMs (approved HV-D30 difference)

## Question
QEMU's q35 serves genroms/kvmvapic.bin (TPR patching for 32-bit Windows without x2APIC) and genroms/linuxboot_dma.bin (direct -kernel boot only). Ship them, write our own, or leave them out?

## Answer
Leave both out. This is an approved HV-D30 difference. If phase 4's legacy Windows guests need TPR patching, the project writes its own clean-room ROM then.

## Why
Both are QEMU's own compiled code; shipping them breaks the clean-room rule (HV-D1). No phase-2 guest uses either: linuxboot is for -kernel only and kvmvapic benefits only XP-era Windows. Owner approved 2026-10-30.

## Consequences
- The hwref allow-list records both ROMs as approved, not pending.
- Phase 4 evaluates whether legacy Windows needs a TPR-patching ROM.
