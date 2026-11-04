---
id: "18"
decision: D17
---

# swtpm

## Question
Keep swtpm as an external process?

## Answer
Yes, from phase 2, with the same contract as today (per-VM `tpm-state/`, ctrl socket readiness).

## Why
No Rust TPM 2.0 emulator exists, and vmlab already owns swtpm's lifecycle. TPM is needed by the shipped windows-11 and windows-server profiles. Recommended option, accepted.

## Consequences
- The hypervisor implements tpm-tis (x86) talking swtpm's PTM socket protocol.
