---
id: "07"
decision: D4
---

# Identity baseline

## Question
Which QEMU machine version does the hardware match?

## Answer
One identity: pc-q35-11.1 as captured in docs/hwref. Templates built on older QEMU versions are checked by the differential suite and any difference is raised to the owner (HV-D30).

## Why
Recommended option, accepted.

## Consequences
None beyond those already on the map.
