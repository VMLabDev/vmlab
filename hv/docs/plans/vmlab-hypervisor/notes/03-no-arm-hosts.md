---
id: "03"
decision: D3
---

# ARM Linux hosts are out of scope for now

## Question
Must the hypervisor run on ARM Linux hosts, which would require emulating x86 guests under TCG?

## Answer
No, not for now. Hosts are x86_64 Linux. TCG covers aarch64 and riscv64 guests only; x86 guests always run under KVM.

## Why
Owner's call (2026-10-30). x86 has no formal machine-readable spec, so an x86 TCG frontend is the least verifiable piece of the project; dropping ARM hosts removes it.

## Consequences
- Legacy x86 guests (DOS, 9x, XP, TempleOS) must run under KVM, including real-mode and 16-bit code paths.
- aarch64 guests run under KVM only on aarch64 hosts, which are out of scope, so on supported hosts aarch64 guests are always TCG.
