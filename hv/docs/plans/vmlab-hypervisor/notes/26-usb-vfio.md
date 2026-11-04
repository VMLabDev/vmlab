---
id: "26"
decision: D25
---

# USB and vfio

## Question
When and on what?

## Answer
At the end of phase 2: xhci and usb-host lifted from crosvm, vfio through rust-vmm's vfio crates. Online snapshots stay refused while USB devices are attached.

## Why
Parity for x86 VMs includes them. Recommended option, accepted.

## Consequences
None beyond those already on the map.
