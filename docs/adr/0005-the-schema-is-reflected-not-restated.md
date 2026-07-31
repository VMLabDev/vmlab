# ADR-0005: The schema is reflected, not restated

- **Status**: Accepted
- **Date**: 2026-07-31
- **Related**: [ADR-0006](0006-one-wcl-block-extractor.md),
  [ADR-0004](0004-lab-status-is-a-typed-projection.md)

## Context

**CONTEXT.md** defines the **Schema projection** as "the single reflected
description of the `vmlab.wcl` schema — every block, field, type, default and
doc string — from which the visual designer's forms are driven," and instructs
readers to avoid the term *descriptor table*.

No such projection exists. What actually drives the designer's forms is a
five-hundred-line file of hand-copied field descriptors whose own header opens
with "Descriptor tables for the inspector forms" and closes with "keep in sync
when the schema grows." The glossary entry is an aspiration written as fact.

The schema is in practice declared five times: the WCL schema, which already
carries types, optionality, defaults and doc strings; the extractor, keyed by
string; the typed model; the serialisation types; and the console's mirrored
interfaces and form descriptors. Four of the five are hand-maintained copies
with no drift guard anywhere.

A single VM field — memory — is spelled out twenty-four times across twelve
files and two languages. A container volume takes sixteen hops from schema to
guest, and the "exactly one of host or name" rule is written three times: once
as schema prose, once in the extractor, once in the console's edit operations.
The doc text has already drifted: the console's copy of one field's help string
is a reworded version of the schema's.

Two things make this tractable rather than aspirational:

1. **WCL reflection already works and is already used in this repo** — the
   rendered schema reference in the wskill data reflects the live schema
   precisely so that "the reference can never drift from the code." The
   machinery exists; it is pointed at the book, not the app.
2. **A partial version already exists in the web layer** and demonstrates the
   failure mode. It sources two of nine enum option lists from Rust constants,
   and its comment claims all nine are "sourced from the Rust constants so they
   can never drift." The other seven are re-typed string literals, then re-typed
   again in the console.

## Decision

**`schema.wcl` is the single source of truth for the shape of `vmlab.wcl`.
Everything that needs to know that shape reads a reflected projection of it. No
surface restates it by hand.**

Concretely:

- The Schema projection becomes a real artefact: block, field, type,
  optionality, default and doc string, reflected from the schema via WCL's
  reflection builtins.
- The designer's forms are driven from the projection. The hand-written
  descriptor tables are retired, and with them the duplicated help prose.
- Enum option lists come from the projection. All of them, not two of nine.
- The console's configuration types are generated from the projection rather
  than hand-declared.
- Invariants the schema can express — cardinality, "exactly one of" — are
  expressed there and consumed everywhere, rather than re-implemented per
  surface.
- Where an invariant genuinely cannot be expressed in the schema, it lives in
  validation only, with no second copy in the console.

Sequencing: the enum option lists first, since they are the smallest complete
slice and the one where a false claim of non-drift is already written down.

## Consequences

**Gained**

- Leverage: one schema, N surfaces. A new field lands in one file and appears in
  the extractor, the forms, the console's types and the reference.
- The **CONTEXT.md** definition of Schema projection becomes true. Until this
  lands, the glossary describes something that does not exist, which is worse
  than not defining it.
- Doc prose stops forking. The designer's help text and the rendered reference
  become the same string.
- The two highest-risk untested regions in the configuration pipeline — the
  extractor and the console's edit-operation writer, together roughly 2,200
  lines with no tests and, on the console side, no test runner — shrink to
  generated code plus a much smaller hand-written remainder.

**Given up**

- A build-time reflection step the configuration pipeline does not currently
  have, with the usual costs: build complexity, and a generated artefact that
  must be either committed or reliably reproduced.
- Fields whose designer presentation is genuinely bespoke — a slider with
  non-linear steps, say — need an override mechanism, which is a second place to
  look. The override set should stay small and be reviewed as it grows.

**Watch for**

- The projection becoming a second hand-maintained artefact because reflection
  cannot express something. If overrides outnumber reflected fields, this
  decision has failed and should be revisited rather than worked around.
