---
id: "02"
decision: D1
---

# Microphone input

## Question
Does host-to-guest audio input come with guest audio?

## Answer
Yes. A capture stream from the host's PipeWire into the guest, opt-in per machine and off by default.

## Why
Both candidate guest devices carry capture streams, so the device cost is small; the risk is a guest listening to the host's microphone, which an explicit per-machine opt-in addresses. Owner chose option 2 with the recommended opt-in.

## Consequences
- vmlab needs a per-machine setting that enables capture; the machine description must carry it.
- The capture stream must not exist in PipeWire at all when the setting is off.
