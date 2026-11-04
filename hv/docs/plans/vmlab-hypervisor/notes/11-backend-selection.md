---
id: "11"
decision: D11
---

# Backend selection

## Question
Where does a lab choose the hypervisor?

## Answer
A per-machine field in the lab file (working name `hypervisor = "vmlab"`), with a host-config default that starts as QEMU.

## Why
Finest grain, matches where the seam lives (`labd/vm.rs:404`, `labd/container.rs:405`), lets one machine dogfood the new backend in a QEMU lab, and flipping the host-config default is the eventual switchover. Recommended option, accepted.

## Consequences
- Validation must reject a machine whose profile or features the selected backend cannot yet run, naming what is missing.
- The field name and the term for the backend must respect CONTEXT.md's vocabulary ("Hypervisor", avoid "backend").
- `qemu_args` on a new-backend machine needs a rule (Q13); the PRD's §1.2 non-goal needs amending (Q19).
