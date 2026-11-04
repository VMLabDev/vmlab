---
id: "15"
decision: D14
---

# Snapshot format

## Question
Where do RAM and device state live, and do qcow2 internal snapshots stay?

## Answer
Snapshots are an operation of the disk-format seam (D8). The qcow2 implementation keeps qcow2 internal snapshots for disk state and stores RAM and device state in the snapshot's vmstate area, in the hypervisor's own versioned device-state format.

## Why
Keeps freeze, locked snapshots and ADR-0019 working while the device-state format is ours. Recommended option, accepted.

## Consequences
- A snapshot carrying vmstate is restorable only by the hypervisor that took it; vmlab records the hypervisor per snapshot and refuses a mismatch.
- Disk-only (offline) snapshots are plain qcow2 internal snapshots and so are portable between hypervisors; what happens to existing online snapshots when a machine switches is Q33.
- The device-state format needs a versioning rule so a newer hypervisor restores an older snapshot.
