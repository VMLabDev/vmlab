# ADR-0011: A lab name is its host-global runtime identity

- **Status**: Accepted
- **Date**: 2026-08-04
- **Related**: [ADR-0003](0003-decisions-are-values-computed-before-execution.md),
  [ADR-0007](0007-the-wire-protocol-carries-a-typed-vocabulary.md)

## Context

The supervisor registry, lab runtime directory, control socket and process
markers all address a lab by its declared name. Two roots declaring the same
name therefore do not merely overwrite registry metadata: a command from the
second root can reach the first root's lab daemon, and orphan reaping for one
can kill processes belonging to the other.

The `vmlab dev` SSH facade in #56 exposes the same identity requirement at a
user-facing surface. Its host-global `vmlab-<lab>-<machine>` aliases are
well-defined only if one lab name identifies at most one running lab per host.

## Decision

**A lab's declared name is its host-global runtime identity. Its root is not
the registry key.**

Before `lab.ensure` returns an existing socket or starts a daemon, the
supervisor compares the requested canonical root with the root registered for
that name. A different root is a `conflict` in every registry state. The same
decision is made before `lab.restart` releases anything. The decision itself
is a pure registry operation; filesystem canonicalisation happens before it.

Re-keying the registry, runtime paths, sockets and process markers by root was
rejected. A path-derived SSH alias would also make aliases unstable when a
collision appeared.

## Consequences

**Gained**

- Name-keyed sockets, runtime paths, process markers and SSH aliases describe
  one lab without another disambiguator.
- A colliding directory cannot operate on, replace or reap the registered lab.
- The caller receives a structured conflict naming the other root and both
  available remedies: stop that lab, or rename this one.

**Given up**

- Two clones or worktrees that declare the same lab name cannot run
  concurrently on one host without changing one declaration.

**Watch for**

- Any future host-global surface that treats a lab name as a display label
  rather than the identity established here.
