---
slug: net-gso-mq
status: mapping
created: 2026-11-02
---

# virtio-net GSO/TSO passthrough (N6) and multi-queue (N7) for vmlab-hv

## Destination
vmlab-hv guests hand large TSO (then USO) segments with their virtio-net header to vmlab's switch, which carries them intact between offload-capable ports and segments (with checksums) only toward ports that cannot take them; guests get checksum offload both ways; multi-queue virtio-net gives one queue pair per vCPU with flow-hash steering. Gains are measured on default-MTU and multi-flow workloads as well as the D28 bars.

## Grounding
notes/01-design-research.md (research). The D28 bars force host_mtu=65520, so they barely measure N6; N6's large win is at MTU 1500; N7's payoff has no bar today. Recommended order N6 then N7; N6's framing revision carries N7's queue id.

## Decisions
| ID | Decision | Struck by | Recorded in |
|----|----------|-----------|-------------|
| D1 | Offload framing is negotiated out of band: vmlab's daemon knows each port's hypervisor and sets the port's framing mode at attach (no in-band hello). | | MAP (owner, Q106) |
| D2 | The bench gains a default-MTU (1500) network workload and a multi-flow guest-to-guest workload. | | MAP (owner, Q107) |
| D3 | N6's first cut is TCP (TSO); UDP (USO) follows as a later step in the same build. | | MAP (owner, Q108) |
| D4 | Guest receive from vmlab's virtual TCP stack stays MSS-clamped in the first cut. | | MAP (owner, Q109) |
| D5 | The AF_XDP fast-path tier is made to interoperate with the new vnet framing now (tap MTU and punt buffers sized for GSO), not left QEMU-only. | | MAP (owner, Q110) |
| D6 | N6's first step (CSUM and GUEST_CSUM offload, checksums computed inside existing copies, DATA_VALID on receive; device-only, no switch change) is built in perf round 6. | | MAP (owner, Q111) |

## Frontier
- [x] Q106-Q111 (decision) — answered, D1-D6.

## Fog
- Build order and split between vmlab (switch, daemon, AF_XDP tier) and vmlab-hv (device, queues) once perf6 merges.

## Accepted unknowns
- netkvm MergeableBuffers (research question 4): measured enabled on Windows 10 standard 19045.2006.9 with netkvm 100.101.104.28500. No registry override/default was present; saved virtio negotiation sets MRG_RXBUF. See notes/03-windows-mergeable-buffers.md.

## Out of scope
- vhost-net/tap (N8) and vhost-user (N9).
