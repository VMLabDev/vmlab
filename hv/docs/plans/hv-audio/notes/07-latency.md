---
id: "07"
decision: D6
---

# Latency target

## Question
What latency target, and the buffer sizes that meet it?

## Answer
Guest-to-speaker at most 20 ms on a tuned host (PipeWire quantum 256 at 48 kHz, guest period at most 10 ms), and at most one quantum added over PipeWire's own graph latency at any quantum. Measured by a loopback harness the project builds: the guest plays a click, the host captures and timestamps it.

## Why
No published baseline exists anywhere (`notes/01`), so the project builds the measurement. Recommended option, accepted.

## Consequences
- The harness needs a guest-side player and a host-side capture with timestamps comparable to the guest's play time.
