---
id: "05"
decision: D2
---

# Profile scope

## Question
Where do macos and windows-legacy go?

## Answer
macos becomes its own phase after phase 2; windows-legacy joins phase 4 with the i440fx work.

## Why
Neither shares much with the q35 Windows 10+/Linux path (notes/02 F4-F5). Recommended option, accepted.

## Consequences
- The vmlab-hypervisor map's phase list gains a macOS phase; vmlab's refusal list keeps refusing these profiles on vmlab-hv until their phase.
