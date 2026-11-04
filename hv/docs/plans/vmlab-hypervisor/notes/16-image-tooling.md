---
id: "16"
decision: D15
---

# Image tooling

## Question
What replaces qemu-img for templates and freeze once QEMU is removed?

## Answer
A disk crate in the hypervisor workspace that vmlab links directly, implementing the operations vmlab uses today (create, linked clone, rebase safe and unsafe, info, resize, convert including from a named snapshot, snapshot create/apply/delete, commit) through the disk-format seam, plus a thin CLI for debugging. vmlab keeps calling qemu-img until the crate reaches parity.

## Why
The only option that lets a future format replace qcow2 everywhere at once. Recommended option, accepted.

## Consequences
- vmlab's `src/template/qimg.rs` gains a second implementation behind a trait; its callers (template build, store, freeze, supervisor backing scan) must not shell out to qemu-img directly.
- Parity for the crate is measured by running vmlab's freeze and template tests against both.
