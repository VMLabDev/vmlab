---
id: "19"
decision: D16
---

# virtio-net PXE option ROM: ship our own iPXE build

## Question
QEMU attaches an iPXE boot ROM to each virtio NIC. Omit it, or ship one?

## Answer
Ship our own iPXE build: a reproducible build of upstream iPXE (a separate GPLv2 project, not QEMU) from a pinned commit, producing the virtio-net option ROM, served through the NIC's PCI expansion-ROM BAR as QEMU does.

## Why
Owner's choice (2026-10-31): keep QEMU's guest-visible network-boot entry rather than approve another difference.

## Consequences
- iPXE is GPLv2: distributing the binary requires providing its corresponding source (pinned commit, build script, config); the binary sits beside the firmware vmlab already bundles, not inside MIT code.
- The machine description gains an option-ROM path per NIC; vmlab supplies it.
- The hwref diff compares the ROM's presence, BAR size and PCI ROM header identity, not its bytes, since the build differs from QEMU's.
