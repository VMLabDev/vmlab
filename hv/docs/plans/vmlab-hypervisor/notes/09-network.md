---
id: "09"
decision: D9
---

# Network attachment

## Question
How does a NIC reach vmlab's switch?

## Answer
vmlab's existing unix stream socket with 4-byte big-endian length-prefixed Ethernet frames (`src/net/framing.rs:1-14`). The tap-fd fast path (`-netdev tap,fd=`) follows later.

## Why
No vmlab changes, rootless and WSL2-safe, a few dozen lines. The PRD sets no throughput target. Recommended option, accepted.

## Consequences
- The hypervisor's virtio-net backend reads and writes that framing; afxdp tier support needs a tap-fd backend in a later phase.
