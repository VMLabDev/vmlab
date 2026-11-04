---
id: "22"
decision: D21
---

# Display

## Question
RFB-subset server, native frame protocol, or both?

## Answer
A native shared-memory frame protocol is the primary path (framebuffer, damage rectangles, cursor shape, dma-buf for GL scanouts), consumed by screenshots, the recorder and input injection through the frame capability (D19). A small RFB server stays for external viewers.

## Why
Removes the RFB hop and the GL-screenshot fallback while keeping `gui = true` viewers working. Recommended option, accepted.

## Consequences
- vmlab's recorder and display code gain a frame-protocol client beside the RFB one.
- GL scanout export must handle NVIDIA external-only dma-bufs, the case vmlab currently patches QEMU for.
- The protocol's wire layout is spec work.
