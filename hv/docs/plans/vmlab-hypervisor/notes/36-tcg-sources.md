---
id: "36"
decision: D34
---

# TCG semantics sources and generator

## Question
Which spec feeds each TCG frontend, and how is Rust generated from it?

## Answer
riscv64 from sail-riscv (BSD-2); aarch64 from the sail-arm models (BSD-3-Clause-Clear). An own Rust emitter consumes Sail's IR at build time; generated files keep upstream notices, collected in THIRD_PARTY_NOTICES. No tooling extracts ASL from Arm's XML releases. Fallback if Arm's position is unfavourable: LLVM AArch64 TableGen for decode and gem5 (BSD-3) as semantics reference.

## Why
Research (`notes/32-sail-licences.md`): both models are permissively licensed and Pydrofoil already ships a Sail-derived emulator under MIT; Sail has no Rust backend, so the emitter is ours. Recommended option, accepted.

## Consequences
- Generated files are BSD-2 or BSD-3-Clear inside an MIT repo; the licence policy for lifted code (accepted unknown under D7) covers them.
- Arm's position on MIT redistribution of sail-arm-derived emulators must be confirmed before aarch64 guest support is released publicly.
