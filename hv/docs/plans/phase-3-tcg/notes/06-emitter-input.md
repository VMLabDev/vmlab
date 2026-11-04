---
id: "06"
decision: D3
---

# Emitter input

## Question
Textual Jib or libsail?

## Answer
Textual Jib (`.ir`) from a pinned Sail 0.20.3 + isla-sail toolchain run in a container, parsed by our own Rust parser.

## Why
Diffable, versioned, keeps OCaml out of our build; Sail has no stable API so the toolchain is pinned (notes/01). Recommended, accepted.

## Consequences
- The toolchain container and its pins are part of the repo; upgrading Sail is a deliberate change with a regenerated diff.
