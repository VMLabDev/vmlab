---
id: "08"
decision: D7
---

# vmlab surface

## Question
How does a machine turn audio on, and what is the default?

## Answer
Per machine: `audio = true` (default off) and `microphone = true` (default off, requires `audio`). On a QEMU machine both are a validation error until QEMU support is added. This is a vmlab PRD change, made by the vmlab-side PR.

## Why
Most labs are headless automation; audio in every VM adds noise for nothing. Recommended option, accepted.

## Consequences
- The machine description (`vmlab-hv-proto`) gains audio and microphone fields.
- vmlab's PRD §11 audio non-goal is amended with the selector PR or a follow-up.
