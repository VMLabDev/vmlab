---
id: "13"
decision: D13
---

# Parity gate

## Question
What proves a phase is done?

## Answer
vmlab's e2e suite (`e2e/features.py`), parameterised by hypervisor, passes for every feature in the phase's scope, plus a differential suite against QEMU for guest-visible behaviour. The owner makes the final call to flip vmlab's default (D2).

## Why
The e2e suite proves vmlab's contract; the differential suite proves guests see what they expect, which matters most for TCG and legacy devices. Recommended option, accepted.

## Consequences
- The e2e harness needs a hypervisor parameter and a per-phase feature scope.
- The differential suite needs a design: what is compared at which granularity per phase (the TCG harness, Q21, is one instance).
