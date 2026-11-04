---
id: "14"
decision: D11
---

# Floating point

## Question
Soft-float or host fast paths?

## Answer
Berkeley SoftFloat (BSD-3, vendored as the Sail models use it) for correctness, plus host-FPU fast paths for common rounding modes admitted only behind lockstep tests.

## Why
Correctness first, speed where proven. Recommended, accepted.

## Consequences
- SoftFloat joins THIRD_PARTY_NOTICES.
