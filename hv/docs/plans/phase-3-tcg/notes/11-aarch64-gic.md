---
id: "11"
decision: D8
---

# aarch64 interrupt controller

## Question
GICv2+v2m or GICv3?

## Answer
GICv2 + GICv2m, as QEMU presents to vmlab today.

## Why
QEMU-identical (HV-D30) and smaller. Recommended, accepted.

## Consequences
None beyond those already on the map.
