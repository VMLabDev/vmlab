---
slug: phase-2-modern-vms
status: specified
created: 2026-10-30
---

# Phase 2 of vmlab-hv: modern x86_64 VMs

## Destination
Phase 2 is finished when every modern x86_64 profile vmlab ships (windows-10, windows-11, windows-server, linux-modern) boots its existing QEMU-built templates on vmlab-hv under KVM, on SeaBIOS or OVMF including secure boot, with display, screenshots and OCR, input, recording, TPM, GPU (virgl, Venus, passthrough), USB, shares, online and offline snapshots, S3, Hyper-V and audio; every phase-2 e2e feature passes on vmlab-hv with the differential suite and performance bars (HV-D13, HV-D26, HV-D28); and the owner can flip the host default for VMs. The native frame protocol (HV-D21) exists and serves vmlab. The viewer app is mapped separately from `../hv-viewer/INTAKE.md`.

## Grounding
Research notes: `notes/01-machine-gap.md`, `notes/02-vmlab-vm-needs.md`, `notes/03-crosvm-lift.md`.
- QEMU's q35 (hwref, QEMU 11.1.2) presents a flat bus 0: host bridge 8086:29c0, transitional virtio (1af4:1000-1003 with legacy BAR0 + modern BAR4), VGA 1234:1111 or virtio-vga 1af4:1050, ICH9 UHCI x3 + EHCI at 1d.x, LPC 1f.0, AHCI 1f.2, SMBus 1f.3; no root ports (notes/01).
- vmlab's default x86 argv uses `-usb`, so the tablet sits on ICH9 UHCI/EHCI; qemu-xhci appears only with `usb = true` or `input = "usb"` (`/home/wil/dev/vmlab/src/qemu/cmdline.rs:839-865`).
- Firmware installs ACPI through fw_cfg's `etc/table-loader` script, reads `etc/e820`, `bootorder`, SMBIOS and the SMI feature negotiation; OVMF needs cfi.pflash01 semantics and SMM for secure boot (notes/01 F3).
- Phase 1 has the in-kernel irqchip, a PCI root on bus 0 function 0 only, a direct-install ACPI builder and modern-only virtio; the gap list totals roughly 60-130 kLOC of estimates (notes/01 F2).
- vmlab's frame consumers read a full RGB frame (vision), an ordered damage stream with cursor shape and DesktopSize (recorder), and inject qcode keys and absolute pointer events (notes/02 F10-F22).
- Shipped modern x86 profiles: windows-10/11/server, linux-modern, macos (OpenCore, VMware SVGA, Apple SMC); windows-legacy is i440fx (notes/02 F3-F5).
- crosvm lift: depend on `rutabaga_gfx` (BSD-3, virglrenderer >= 1.3), lift virtio-gpu protocol; the headless display stub delivers no frames; xHCI core liftable but its identity differs from qemu-xhci; vfio via rust-vmm crates (notes/03).
- Local templates exist for real-guest testing: windows-10, windows-server-2019/2022 (+core), windows-8, debian-13, ubuntu-24.04/26.04, fedora-44 and more (`~/.local/share/vmlab/templates/x86_64/`).
Binding decisions from `../vmlab-hypervisor/MAP.md` (cited as HV-Dn): D17 swtpm, D21 frame protocol plus RFB, D23 SeaBIOS and OVMF unchanged, D24 GPU via crosvm virtio-gpu and rutabaga, D25 USB and vfio at the end of phase 2, D30 QEMU-identical hardware, D33 in-process virtio-fs. Audio: `../hv-audio/SPEC.md`. Viewer intake: `../hv-viewer/INTAKE.md`. Hardware references: `docs/hwref/`.

## Decisions
| ID | Decision | Struck by | Recorded in |
|----|----------|-----------|-------------|
| D1 | Destination confirmed; viewer mapped separately | | `notes/04-destination.md` |
| D2 | macos is its own later phase; windows-legacy joins phase 4 | | `notes/05-profile-scope.md` |
| D3 | Sub-phases 2a platform+SeaBIOS, 2b frames, 2c OVMF/SMM/TPM/Windows, 2d GPU, 2e virtio-fs+audio, 2f USB/vfio | | `notes/06-sub-phases.md` |
| D4 | Hardware identity is pc-q35-11.1 as captured | | `notes/07-identity-baseline.md` |
| D5 | VMs use QEMU-identical transitional virtio; containers stay modern | | `notes/08-transitional-virtio.md` |
| D6 | Approved HV-D30 break: no UHCI/EHCI, tablet on xHCI | | `notes/09-usb-break.md` |
| D7 | Own frame protocol: unix socket, memfd frames, dma-buf with fences | | `notes/10-frame-protocol.md` |
| D8 | vmlab consumes native frames; RFB only for human viewers | | `notes/11-frame-consumers.md` |
| D9 | Online snapshots refused with paravirtual GPU or vfio | | `notes/12-snapshot-gpu-vfio.md` |
| D10 | qemu_* GPU keys ignored with a warning; vmlab-hv probes TSC itself | | `notes/13-host-gpu-settings.md` |
| D11 | HDA at 00:1b.0 | | `notes/14-hda-slot.md` |
| D12 | hwref diff is the platform gate; real templates boot in S1-style tests | | `notes/15-platform-testing.md` |
| D13 | S4 e2e hypervisor parameter now, as a parallel vmlab PR | | `notes/16-s4-timing.md` |
| D14 | Approved HV-D30 difference: no QEMU-built option ROMs (kvmvapic, linuxboot) | | `notes/17-qemu-roms.md` |
| D15 | Approved HV-D30 difference: no VMware backdoor (vmport/vmmouse) | | `notes/18-vmport.md` |
| D16 | Ship a reproducible upstream iPXE build as the virtio-net option ROM | | `notes/19-ipxe.md` |

## Frontier
Owner chose single-session mapping for the previous maps; no split offered.
- [x] Q1 (research) [status: answered] — Machine-model gap between phase 1 and QEMU's q35, and what the firmware needs.
- [x] Q2 (research) [status: answered] — What vmlab's VM path needs from phase 2, including the frame consumers.
- [x] Q3 (research) [status: answered] — Lift cost of crosvm's GPU, xHCI, USB and vfio code.
- [x] ~~Q4 (decision) — Destination, and whether the viewer app is part of phase 2.~~ → D1
- [x] ~~Q5 (decision) — Profile scope: macos and windows-legacy.~~ → D2
- [x] ~~Q6 (decision) — Sub-phase order within phase 2.~~ → D3
- [x] ~~Q7 (decision) — Identity baseline: which QEMU machine version the hardware matches.~~ → D4
- [x] ~~Q8 (decision) — Transitional virtio for VM machines.~~ → D5
- [x] ~~Q9 (decision) — Default USB: ICH9 UHCI/EHCI as QEMU's `-usb`, or a D30 break onto xHCI.~~ → D6
- [x] ~~Q10 (decision) — Frame protocol shape.~~ → D7
- [x] ~~Q11 (decision) — vmlab's frame consumers on vmlab-hv machines.~~ → D8
- [x] ~~Q12 (decision) — Online snapshots with a paravirtual GPU or vfio device.~~ → D9
- [x] ~~Q13 (decision) — Host-GPU settings and TSC probing without QEMU.~~ → D10
- [x] ~~Q14 (decision) — HDA placement on q35.~~ → D11
- [x] ~~Q15 (decision) — Differential and real-template testing for the platform.~~ → D12
- [x] ~~Q16 (decision) — When the vmlab e2e suite gets its hypervisor parameter (S4).~~ → D13

## Fog

## Accepted unknowns
- Frame protocol wire layout, NVIDIA external-only readback (D7): spec work, fixed before 2b
- Per-sub-phase acceptance lists and refusal-list shrinkage (D3): spec work
- vmlab-side follow-ons: refusals for macos/windows-legacy (D2) and GPU/vfio online snapshots (D9), the native frame client (D8), the GPU-key warnings (D10): vmlab PRs, specified in the spec
- Legacy virtio register interface and per-machine transport shape (D5): implementation under a decided contract
- One-time USB re-enumeration and the differential suite's approved-difference list (D6): consequences of the approved break, recorded in the spec
- Minimal xHCI with an HID tablet landing in 2b ahead of 2f (D6): sequencing recorded in the spec
- Probe mode for host facts (D10), the capture script's vmlab-hv mode and the S1 VM mode (D12): implementation
- Phase list update in the vmlab-hypervisor map for the macOS phase (D2): bookkeeping

## Out of scope
- A clean-room TPR-patching ROM, only if phase 4's legacy Windows guests need one (D14)
- Legacy PC guests (phase 4), TCG guests (phase 3)
