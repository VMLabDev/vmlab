# Architecture Decision Records

Decisions that shape vmlab's module structure. Read the ones that touch the area
you're working in before you start.

`docs/vmlab-prd.md` outranks every ADR here. `CONTEXT.md` is the glossary these
records speak in; where an ADR introduces a term, the glossary carries it too.

| # | Decision | Status |
| --- | --- | --- |
| [0001](0001-hypervisor-seam-substitutes-the-host.md) | The Hypervisor seam substitutes the host, and Machine is the test surface | Accepted |
| [0002](0002-machine-is-the-only-route-to-a-machine.md) | The Machine interface is the only route to a machine | Accepted |
| [0003](0003-decisions-are-values-computed-before-execution.md) | Decisions are computed as values before execution | Accepted |
| [0004](0004-lab-status-is-a-typed-projection.md) | Lab status is a typed projection, not an untyped map | Accepted |
| [0005](0005-the-schema-is-reflected-not-restated.md) | The schema is reflected, not restated | Accepted |
| [0006](0006-one-wcl-block-extractor.md) | One WCL block extractor | Accepted |
| [0007](0007-the-wire-protocol-carries-a-typed-vocabulary.md) | The wire protocol carries a typed vocabulary and error codes | Accepted |
| [0008](0008-hardware-resolution-happens-once.md) | Hardware resolution happens once, for both machine kinds | Accepted |
| [0009](0009-build-hardware-resolves-block-over-source-template.md) | Build hardware resolves block over source template, and the profile stays live | Accepted |

## Format

Each record carries status, date, related records, the context that forced the
decision, the decision itself, and its consequences — what was gained, what was
given up, and what to watch for. A decision that gives nothing up is usually a
decision that has not been made yet.

## Adding one

Number sequentially. Supersede rather than edit: if a decision changes, write a
new record and mark the old one superseded, so the reasoning that led to the
change stays readable.
