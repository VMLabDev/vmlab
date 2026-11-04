---
id: "10"
decision: D9
---

# Audio backends

## Question
Should guest audio work in default builds?

## Answer
Enable vmlab-hv's PipeWire and PulseAudio backends in default and release builds.

## Why
The installed build currently has only the null sink, so guest audio is silent.

## Consequences
Build hosts need libpipewire and libpulse development packages; record in the README.
