---
id: "17"
decision: D16
---

# qemu_args on the new hypervisor

## Question
What happens to a machine that sets qemu_args and selects the new hypervisor?

## Answer
Validation error naming the argument and suggesting QEMU for that machine.

## Why
Silently ignoring flags is worse; a passthrough for the new binary can be added if a need appears. Recommended option, accepted.

## Consequences
- Template and variant build VMs that carry qemu_args (`template/build.rs:1953-1956`) also stay on QEMU.
