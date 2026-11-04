---
id: "04"
decision: D1
---

# Destination

## Question
Destination, and whether the viewer app is part of phase 2.

## Answer
Phase 2 is finished when every modern x86_64 profile vmlab ships (windows-10, windows-11, windows-server, linux-modern) boots its existing QEMU-built templates on vmlab-hv under KVM, on SeaBIOS or OVMF including secure boot, with display, screenshots and OCR, input, recording, TPM, GPU (virgl, Venus, passthrough), USB, shares, online and offline snapshots, S3, Hyper-V and audio; every phase-2 e2e feature passes on vmlab-hv with the differential suite and performance bars (HV-D13, HV-D26, HV-D28); and the owner can flip the host default for VMs. The native frame protocol (HV-D21) exists and serves vmlab. The viewer app is mapped separately from `../hv-viewer/INTAKE.md`.

## Why
Recommended option, accepted.

## Consequences
- The viewer map depends on the frame protocol this phase defines.
