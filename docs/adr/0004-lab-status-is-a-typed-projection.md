# ADR-0004: Lab status is a typed projection, not an untyped map

- **Status**: Accepted
- **Date**: 2026-07-31
- **Related**: [ADR-0002](0002-machine-is-the-only-route-to-a-machine.md),
  [ADR-0007](0007-the-wire-protocol-carries-a-typed-vocabulary.md)

## Context

Lab status is the most-read thing vmlab produces. It reaches three surfaces —
the CLI status table, the REST endpoint, and the web console — and it crosses
the seam between the lab daemon and all three as a hand-built JSON object.

The machine entries in it are typed for the seven fields both machine kinds
share, and then carry everything kind-specific through a flattened string-keyed
map. VMs insert their template, architecture, CPU count, memory and agent
version into it. Containers insert their image, digest, health, restart count
and exit code. The console declares all of those as typed fields on its own
mirrored interfaces, and casts through an index-signature escape hatch to get
there.

The result is that the one place the Rust and TypeScript type systems meet is
explicitly opted out of type checking, and a producer-side rename is invisible
to `cargo check`, to `tsc`, and to CI.

This has already cost real defects. Commit `3117cff` renamed the top-level
collection from two per-kind arrays to one. Commit `7a0da2f` fixed two
consumers that were still reading the old keys — the CLI's status table, which
rendered as nothing, and a console route path, which returned the SPA's HTML
shell with a 200 and surfaced as a parse error rather than a 404. Its message
records that both were "found by driving a real lab rather than by any test."

A third consumer was missed and is still live: the web layer's guard against
reloading a lab with running machines reads the old keys, gets null, and so
never fires — meaning the daemon restarts under running VMs, which the comment
directly above it says it cannot survive.

Separately, the derivation from raw state to a user-facing label is written
twice with two different vocabularies. The CLI reports the raw state plus a
readiness column; the console maps the same inputs to "running", "booting",
"starting", "unhealthy" and an exit-code form. A segment counter the CLI calls
out as "the thing that makes guest transfers mysteriously slow" reaches the
browser in the payload and is silently dropped, because the console's mirrored
interface does not declare it.

## Decision

**Lab status is a typed projection produced once by the lab daemon and consumed
unchanged by every surface. Kind-specific fields are modelled, not mapped.**

Concretely:

- Machine status becomes a tagged union over machine kind. There is no
  string-keyed overflow map. A field that exists for one kind and not the other
  is a variant field.
- The derivation from raw state to a user-facing label lives in the projection
  module, next to the type, and is the same derivation for all three surfaces.
  One vocabulary, defined once.
- Surfaces render the projection. They do not re-derive it, and they do not
  re-filter machines by kind — the projection presents whatever grouping the
  surfaces need.
- Prior art and the shape to copy: the event formatter in the logging module is
  already shared by the CLI and the web layer, and is the only cross-surface
  renderer in the codebase that has never drifted.
- The console's mirrored types are generated from the projection rather than
  hand-written. See [ADR-0005](0005-the-schema-is-reflected-not-restated.md) for
  the same principle applied to configuration.

## Consequences

**Gained**

- Locality: a producer-side rename becomes a compile error at every consumer
  rather than an empty table.
- The three failure signatures this class produces — silent empty render,
  parse failure that reads as a data bug, and quietly dropped fields — all
  become unrepresentable.
- Status becomes testable without a lab: build a projection value, assert the
  rendered output.
- The CLI and the console converge on one vocabulary for machine state, which
  is currently the most visible inconsistency between the two surfaces.

**Given up**

- Adding a kind-specific status field is no longer a one-line insert into a map.
  It touches the type. That is the point, but it is friction.
- The projection type is a wire contract, so changing it is a breaking change
  for any out-of-tree consumer of the daemon protocol.

**Watch for**

- The tagged union growing a third variant for every capability. Capabilities
  are probed and reported per **CONTEXT.md**; they are not machine kinds.
