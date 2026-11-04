---
id: "08"
decision: D8
---

# Disk format

## Question
Is qcow2 with backing chains the hypervisor's disk format?

## Answer
Yes for now, behind a disk-format seam inside the hypervisor so a purpose-built format can replace qcow2 later. The owner intends to design that format to fit vmlab's requirements more closely.

## Why
Templates, linked clones, freeze chains, OCI chunking, ADR-0017 and ADR-0019 assume qcow2, so it is required for parity. The owner wants a better-fitting format eventually, so qcow2 must not leak past the seam.

## Consequences
- The hypervisor reads and writes qcow2 natively (imago or own implementation) through a format trait.
- The future format is a separate design: what it must do better than qcow2 is not yet statable (Fog).
- Image tooling (Q9) and snapshot format (Q8) must work through the same seam.
