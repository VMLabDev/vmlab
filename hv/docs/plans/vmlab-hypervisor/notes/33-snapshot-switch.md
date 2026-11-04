---
id: "33"
decision: D31
---

# Snapshots when a machine switches hypervisor

## Question
When a machine switches hypervisor, what happens to its online snapshots taken under the other one?

## Answer
Offline (disk-only) snapshots carry over, because they are plain qcow2 internal snapshots. Online snapshots stay on disk but refuse to restore under the other hypervisor, naming the hypervisor that took them. Switching back makes them restorable again.

## Why
Nothing is lost and the refusal is explicit; a converter would mean reverse-engineering QEMU's vmstate stream for a one-off migration. Recommended option, accepted.

## Consequences
- vmlab records the hypervisor per snapshot (also required by D14).
