---
id: "29"
decision: D28
---

# Performance bar

## Question
What bar does each phase meet before its default flips?

## Answer
No-regression against QEMU on the same host: boot to agent-ready no slower than QEMU; virtio-blk and virtio-net at least 80% of QEMU's throughput; TCG at least 50% of QEMU's TCG.

## Why
The PRD sets no performance goal, but users would notice a slower default. Recommended option, accepted.

## Consequences
- The benchmark set and host need defining; an implementation detail of the parity gate.
