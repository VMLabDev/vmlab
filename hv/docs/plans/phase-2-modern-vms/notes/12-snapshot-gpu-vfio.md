---
id: "12"
decision: D9
---

# Online snapshots with GPU or vfio

## Question
Online snapshots with a paravirtual GPU or vfio device?

## Answer
Refused, matching QEMU's migration blockers; offline snapshots work.

## Why
rutabaga cannot restore virgl or Venus state and vfio state is physical (notes/03 F6, F12, F27). Recommended option, accepted.

## Consequences
- vmlab's refusal list gains these online-snapshot refusals for vmlab-hv machines.
