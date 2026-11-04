---
id: "04"
decision: D4
---

# Phase order

## Question
Which slice reaches parity first?

## Answer
1. Container micro-VMs (x86_64 KVM, direct kernel boot, virtio-blk/serial/fs, no display, no firmware).
2. Modern x86_64 VMs (q35-class, UEFI, virtio, display; Linux and Windows guests).
3. TCG guests (aarch64, riscv64 on x86_64 hosts).
4. Legacy PC guests (DOS, 9x, XP, TempleOS).

## Why
Containers have the smallest guest-visible surface and proven Rust prior art (cloud-hypervisor, libkrun), yet exercise the process model, control protocol, network, agent channel, disks and snapshots. Legacy is last because it is the most new device code with no prior art. Recommended option, accepted.

## Consequences
- Phase 1 needs a direct-kernel-boot loader, virtio-blk, virtio-serial multiport, vhost-user-fs and the stream-socket NIC, and nothing else guest-visible.
- Display, firmware, TCG, legacy devices, GPU and USB are not needed until later phases, but their decisions can be made now.
- Container online snapshots must work in phase 1, so the snapshot format (Q8) is a phase-1 decision.
