---
id: "24"
decision: D23
---

# Firmware

## Question
Reuse SeaBIOS and OVMF, write our own, or UEFI only?

## Answer
Reuse SeaBIOS and OVMF binaries unchanged and implement the platform interfaces they expect (fw_cfg, cfi pflash, ACPI tables) from public specs and observed behaviour.

## Why
Firmware runs inside the guest, so its licence does not reach the hypervisor; vmlab already ships pinned edk2, and legacy guests need SeaBIOS. Recommended option, accepted.

## Consequences
- Secure boot needs SMM emulation compatible with OVMF's secure-boot build.
- Existing per-VM OVMF_VARS files stay valid only if the pflash layout matches (ties to D30).
