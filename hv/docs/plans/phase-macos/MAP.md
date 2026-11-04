---
slug: phase-macos
status: specified
created: 2026-10-31
---

# vmlab-hv macOS phase: Intel macOS guests

## Destination
The owner's macos-tahoe template boots unchanged on vmlab-hv with OpenCore; display, USB input, the macOS agent, networking and snapshots work; the user supplies the Apple SMC key through vmlab's host config (vmlab no longer embeds it, D1) and vmlab-hv never embeds it; macOS e2e passes on `--hypervisor vmlab`.

## Grounding
Landed: notes/01-macos-machine.md. The gap is small: q35 with AHCI, transitional virtio, phase 2f's xHCI plus a USB HID keyboard, a hypervisor CPUID leaf 0x40000010 with a pinned invariant TSC, unknown-MSR tolerance, an isa-applesmc at 0x300, an SMBIOS type 2 table, and VMware SVGA II (registers, framebuffer, minimal FIFO; macOS runs it unaccelerated). Correction to the destination's premise: vmlab embeds the OSK today (src/qemu/cmdline.rs APPLE_SMC_OSK); the user does not supply it. No macOS hwref capture exists.
Binding: HV-D30 (QEMU-identical hardware), phase-2 D2 (macOS is its own phase after phase 2). Local template: macos-tahoe (vmlab's macOS agent verified live on Tahoe 26.7.2).

## Decisions
| ID | Decision | Struck by | Recorded in |
|----|----------|-----------|-------------|
| D1 | The OSK becomes a user-supplied host-config value. vmlab deletes its embedded constant, refuses `apple_smc` machines until the key is set, and passes it to whichever hypervisor runs the machine (QEMU's `isa-applesmc,osk=`, vmlab-hv's machine description). Git history is not rewritten. | Q2 (owner, 2026-10-31) | MAP |
| D2 | vmlab-hv tolerates unknown MSRs itself, on macOS machines only, through KVM's userspace MSR exits: reads return 0, writes are dropped, each MSR is logged once. A boot trace decides catch-all versus an enumerated list. The host no longer needs `kvm.ignore_msrs=Y` for vmlab-hv. | Q3 (owner) | MAP |
| D4 | macOS runs on vmlab-hv only once this phase lands. vmlab drops macOS on QEMU: the `macos` profile defaults to `hypervisor = "vmlab"`, a macOS machine on QEMU is refused, and vmlab's `ignore_msrs` warning and its QEMU applesmc/argv path for macOS go away. The host needs no `kvm.ignore_msrs`. | owner, 2026-11-01 | MAP |
| D3 | VMware SVGA II is registers, linear framebuffer, minimal FIFO and SYNC/BUSY, with no 2D blit engine. A macOS hwref capture with FIFO tracing comes first; a blit engine is built only if the capture shows macOS issuing blit commands. | Q4 (owner) | MAP |

## Frontier
- [x] Q1 (research) — The macOS machine. notes/01-macos-machine.md
- [x] Q2-Q4 (decision) — answered, D1-D3.
- [x] Q5 (research) [status: answered] — macOS capture: docs/hwref/q35-macos-tahoe/. No VMware SVGA register or FIFO activity at all; macOS ran on the firmware's VBE framebuffer (640x480 then 1280x800, 32-bit). The MSR list is deferred to vmlab-hv's own D2 logging.
- [x] Q6 (build) — vmlab PR for D1 merged as #195; the owner's key is set locally.

## Fog

## Accepted unknowns

## Out of scope
