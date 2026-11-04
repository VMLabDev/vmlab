---
id: "03"
decision: D2
---

# Guest audio device

## Question
Which guest audio device(s) for Linux and Windows guests?

## Answer
Intel HDA first: an ICH9 HDA controller on q35 with a duplex codec (line-out and line-in), implemented from the public Intel HDA 1.0a spec. virtio-snd (VIRTIO 1.3 ch. 5.14) follows as the Linux fast path. Legacy AC97 and SB16 are phase 4.

## Why
Windows has no virtio-snd driver (virtio-win #929 open since 2023), while HDA has inbox drivers on Windows 7-11 and Linux (`notes/01`). vmlab's QEMU templates carry no audio device, so D30 hardware identity does not constrain the controller choice. Recommended option, accepted.

## Consequences
- The HDA model needs controller, CORB/RIRB, stream descriptors with BDL DMA, position reporting (LPIB, DMA position buffer, wall clock) and a codec verb handler.
- Device state for both devices joins the hypervisor's device-state format (vmlab-hypervisor D14).
