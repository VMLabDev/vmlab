---
id: "13"
decision: D10
---

# Host-GPU settings and TSC probing

## Question
Host-GPU settings and TSC probing without QEMU.

## Answer
On vmlab-hv, qemu_binary and qemu_env do not apply and warn when set; render-node selection, the Vulkan environment and PCI filter layer, and keep-awake apply unchanged. vmlab-hv reports TSC and invariant-TSC facts through a probe so vmlab stops launching QEMU to ask.

## Why
Recommended option, accepted.

## Consequences
- vmlab-hv gains a probe mode or control command for host facts.
