---
id: "34"
decision: D32
---

# Device-state compatibility across hypervisor versions

## Question
Which older `vmlab-hv` versions' online snapshots must a newer one restore?

## Answer
Online snapshots whose device-state format shares the current major version restore; an older major refuses with a clear message. Before 1.0 the guarantee is best effort.

## Why
Templates and freeze only use offline snapshots, so online snapshots are short-lived working state; forever-compatibility is the burden that makes QEMU's migration code dense. Recommended option, accepted.

## Consequences
- The device-state format carries a major version in every snapshot.
