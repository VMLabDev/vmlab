---
id: "15"
decision: D12
---

# Platform testing

## Question
Differential and real-template testing.

## Answer
The hwref capture script also runs against vmlab-hv, and its normalised diff against QEMU (timestamps and firmware allocation addresses masked, D6's approved differences listed) is HV-D26's platform gate. S1-style tests boot real local templates to agent ready: Debian 13 and Ubuntu 24.04 first, then Windows 10 and Windows Server 2022.

## Why
Recommended option, accepted.

## Consequences
- The capture script needs a vmlab-hv mode; the S1 harness needs a VM mode with firmware boot.
