# ADR-0006: One WCL block extractor

- **Status**: Accepted
- **Date**: 2026-07-31
- **Related**: [ADR-0005](0005-the-schema-is-reflected-not-restated.md)

## Context

vmlab reads WCL in four unrelated places: lab files, host configuration, guest
OS profiles, and template store metadata. Each has its own independently written
block-to-struct extractor, and they agree on nothing:

| Extractor | Error style | Source spans | Direct tests |
| --- | --- | --- | --- |
| Lab files | accumulated issue list | yes | none |
| Host configuration | fail-fast result | no | a few |
| Profiles | fail-fast result, hand-rolled getters | no | shipped profiles only |
| Template metadata | fail-fast result, plus a hand-written WCL *emitter* | no | a few |

The largest is the sole gate between user text and every typed model in the
product, and it has no direct tests — its coverage is incidental, arriving
through tests of the validator and the loader that happen to exercise some of
its paths. Its error branches are reached only when some other test's fixture
trips them.

All four re-derive things the schema already states. The size getter
re-establishes that a byte size is a non-negative integer — a fact the schema
declares. The profile extractor spends around 150 lines on hand-rolled string
and boolean getters that duplicate, less capably, what the lab extractor already
has: no spans, different error text, a different result shape.

The practical cost is that a user gets a precise, span-anchored diagnostic for a
mistake in a lab file and a bare message for the same class of mistake in a
profile.

## Decision

**There is one WCL block extractor. The four callers keep only their field
mappings.**

Concretely:

- A single extractor module owns typed field access, coercion, span tracking and
  the issue vocabulary.
- All four call sites use it. None keeps a private getter.
- Diagnostics are uniform: the same mistake produces the same shape of message
  with the same span information regardless of which file it was made in.
- Accumulated issues, not fail-fast, is the default — it is the behaviour the
  most-used call site already has, and it is what lets the validator report
  everything wrong with a file in one pass.
- Coercion rules are derived from the schema where the schema states them, per
  [ADR-0005](0005-the-schema-is-reflected-not-restated.md). This ADR covers the
  parsing direction; ADR-0005 covers the projecting direction. They meet at the
  schema.
- The extractor is tested directly, through its own interface, rather than
  incidentally through the validator. Prior art for the seam: the validator's
  injectable context, which is a small trait with a test fake and is the
  best-tested module in the configuration pipeline.

## Consequences

**Gained**

- One error vocabulary across all configuration a user can write.
- Source spans everywhere, not just in lab files — so the designer and the CLI
  can point at the offending text in profiles and host configuration too.
- Locality: type coercion is tested once instead of four times partially.
- The largest untested file in the configuration pipeline gets an interface
  worth testing through.

**Given up**

- The three smaller call sites take on the accumulated-issue shape, which is
  more machinery than a fail-fast result where the caller only ever reports the
  first problem.
- The template metadata path also *writes* WCL. That emitter is out of scope
  here and remains hand-written, which leaves one asymmetry: metadata is read
  through the shared extractor and written through bespoke code.

**Watch for**

- The shared extractor accumulating call-site-specific behaviour behind flags.
  If it grows a parameter per caller it has become four extractors again with
  extra steps.
