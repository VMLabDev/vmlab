---
id: "02"
decision: D2
---

# QEMU stays until the new hypervisor is good enough, then goes

## Question
Does QEMU remain a permanent fallback, or is it removed once the new hypervisor is ready?

## Answer
vmlab keeps QEMU as its default backend while vmlab-hypervisor is built, and removes it once the owner is satisfied with the new hypervisor. Machines opt into the new backend in the meantime.

## Why
Owner's call (2026-10-30). Keeping QEMU lets vmlab ship unchanged while the new backend matures; removing it eventually is the point of the project.

## Consequences
- vmlab needs a backend selector and must run both backends side by side, possibly in one lab.
- Every vmlab feature needs a parity bar before QEMU can go; what "satisfied" means is not yet defined.
- The legacy PC platform and TCG are mandatory scope, not optional, because removal leaves no fallback for them.
