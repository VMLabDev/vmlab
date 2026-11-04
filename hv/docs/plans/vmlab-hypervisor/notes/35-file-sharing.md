---
id: "35"
decision: D33
---

# File sharing on vmlab-hv

## Question
What file-sharing transport does the new hypervisor offer, Windows especially, and does virtiofsd stay?

## Answer
- Phase 1: external virtiofsd attached over vhost-user (rust-vmm `vhost` front-end), as QEMU does today.
- Phase 2: an in-process virtio-fs device speaking the standard virtio-fs protocol (built on crosvm's BSD `fuse` crate) replaces virtiofsd for `vmlab-hv` machines; guests keep stock drivers (Linux virtiofs, VioFS). Its state serialises into the hypervisor's own snapshot format (D14).
- Windows profiles regain virtiofs once a numbered stable virtio-win build at or after mm325 passes a byte-verified concurrent-read soak.
- SMB at the segment gateway stays the default for large-file Windows transfers and the universal fallback; the workspace syncer stays the answer for watched trees.

## Why
Research (`notes/14-windows-file-sharing.md`): the Windows bottleneck is WinFsp's user-mode stack, so a new guest protocol buys control, not throughput; an own kernel driver is ruled out by signing and HLK. Owning the device side retires a helper process, keeps snapshot state ours, and lets sync queues and multi-queue ship before upstream. Recommended option, accepted.

## Consequences
- Phase 1 must implement vhost-user front-end device-state transfer into the snapshot format, or refuse online snapshots of machines with virtiofs shares until phase 2 (as vmlab does for old virtiofsd).
- The in-process device must reproduce virtiofsd's snapshot semantics (find-paths or file-handles handle migration).
- The soak test for VioFS re-enablement belongs to the differential suite (D26).
