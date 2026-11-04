---
id: "08"
decision: D5
---

# Execution architecture

## Question
How to reach D1's 100% bar?

## Answer
One pipeline from Sail: the emitter specialises Sail semantics into our own typed IR (bitvectors lowered to u64, constant specialisation, dead-path removal); that IR drives a reference interpreter (oracle and cold path) and a Cranelift JIT compiling hot traces across blocks with the softmmu TLB fast path inlined. Hand-written overrides for hot instructions are allowed only behind lockstep tests against the generated semantics.

## Why
The only design with a credible path past QEMU while keeping Sail authoritative; Pydrofoil shows specialisation is worth 42-63x (notes/03). Recommended, accepted.

## Consequences
- The IR design is the central spec item for 3a.
- Overrides need a registry so the lockstep harness can test each against its generated counterpart.
