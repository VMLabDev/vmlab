---
id: "06"
decision: D5
---

# Phase

## Question
Which phase delivers audio?

## Answer
Phase 2 of vmlab-hypervisor, alongside display and the viewer: HDA first, virtio-snd later in phase 2. Legacy AC97 and SB16 in phase 4.

## Why
Audio is a desktop-guest feature; phase 1 containers are unaffected. Recommended option, accepted.

## Consequences
None beyond those already on the map.
