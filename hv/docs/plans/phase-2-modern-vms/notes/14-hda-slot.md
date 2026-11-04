---
id: "14"
decision: D11
---

# HDA placement

## Question
Where does HDA sit on q35?

## Answer
At 00:1b.0, where real ICH9 places it.

## Why
QEMU's captures carry no audio device, so nothing constrains it. Recommended option, accepted.

## Consequences
None beyond those already on the map.
