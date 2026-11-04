---
id: "03"
decision: D2
---

# Hypervisor frame export rework

## Question
Rework vmlab-hv's frame export for zero-copy?

## Answer
Yes, in this work: stable surfaces with cached buffer imports on both sides; send the guest scanout dma-buf directly and blit only where an NVIDIA external-only buffer requires it; no CPU readback unless a client asked for shared-memory frames; damage and publish driven by guest resource flushes instead of the fixed 16 ms poll.

## Why
Without it the destination's frame-rate and one-frame-latency goals cannot be met (grounding: per-frame blit, readback, new fd, full damage, 16 ms poll).

## Consequences
vmlab's recorder and screenshot clients also consume native frames: they must keep working (they request shared memory). Placeholder-frame semantics from the earlier recording work must be preserved.
