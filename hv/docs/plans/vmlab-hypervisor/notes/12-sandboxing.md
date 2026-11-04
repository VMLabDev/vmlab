---
id: "12"
decision: D12
---

# Sandboxing

## Question
Keep "not a security boundary", or confine each machine?

## Answer
No sandboxing now, but no design choice may block adding seccomp and Landlock per machine process later.

## Why
The PRD's single-trusted-user stance holds (`docs/vmlab-prd.md:29`), and process-per-machine (D5) makes confinement cheap to add later. Recommended option, accepted.

## Consequences
- Device emulation must not need ambient authority it could not be confined from (open files and sockets up front, pass fds).
