---
id: "20"
decision: D19
---

# vmlab seam refactor

## Question
Widen Control, add capability traits, or degrade?

## Answer
Capability traits: `Control` exposes optional snapshot, input, frame and hot-plug capabilities; `LaunchSpec` becomes a typed machine description and each adapter composes its own launch (the QEMU adapter calls the existing pure argv builder). The refactor lands in vmlab before phase 1.

## Why
Matches vmlab's Capability concept (probed and reported, `CONTEXT.md:149`) and lets phase 1 ship without display. Recommended option, accepted.

## Consequences
- ADR-0001's statement that argv composition stays outside the seam is revised by this; vmlab needs an ADR for it.
- The QEMU adapter must translate each typed capability to QMP, so QEMU behaviour is unchanged.
