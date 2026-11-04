---
id: "30"
decision: D29
---

# Cutover

## Question
Flip per phase or all at once?

## Answer
Per phase: when a phase passes its gate (D13, D28) and the owner agrees, the host default flips for that phase's machines. QEMU is removed after phase 4.

## Why
Real usage starts early. Recommended option, accepted.

## Consequences
- The host-config default from D11 is keyed by machine kind or profile, not a single global value.
