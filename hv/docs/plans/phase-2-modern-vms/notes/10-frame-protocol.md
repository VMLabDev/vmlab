---
id: "10"
decision: D7
---

# Frame protocol shape

## Question
Shape of the native frame protocol.

## Answer
An own protocol on a unix socket: frames in shared memory (memfd) with damage rectangles, cursor shape and hotspot, size changes and timestamps; GPU scanouts as dma-buf fds with sync_file fences; multiple independent readers; readers reconnect. Wire layout is spec work.

## Why
Works headless and on WSL2, unlike a PipeWire video node. Recommended option, accepted.

## Consequences
- The wire layout (stride, formats, buffer ownership and release, fence semantics, backpressure) must be fixed in the spec before 2b starts.
- NVIDIA external-only dma-bufs need a CPU readback path for consumers that want pixels.
