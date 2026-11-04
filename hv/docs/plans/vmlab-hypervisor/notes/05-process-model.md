---
id: "05"
decision: D5
---

# Process model

## Question
Is the hypervisor a process per machine, a library in the lab daemon, or a process per lab?

## Answer
One separate hypervisor process per machine, spawned by vmlab, as QEMU is today.

## Why
Fault isolation: a device bug or guest-triggered panic kills one machine, not the lab daemon. vmlab's `Process` trait, stop ladder and orphan sweep (`src/labd/hypervisor/mod.rs:50-65`, `src/qemu/process.rs:308-346`) keep working, and per-machine sandboxing stays cheap (D12). Recommended option, accepted.

## Consequences
- The hypervisor ships as a binary vmlab finds and execs; vmlab's `required_binaries` and orphan sweep markers must learn it.
- vmlab and the hypervisor talk over a socket (Q5), so a shared protocol crate is needed (Q17).
