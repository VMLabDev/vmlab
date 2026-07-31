# ADR-0002: The Machine interface is the only route to a machine

- **Status**: Accepted
- **Date**: 2026-07-31
- **Related**: [ADR-0001](0001-hypervisor-seam-substitutes-the-host.md)

## Context

Commits `7c9da0d` and `3117cff` collapsed the VM and container command pairs
into one machine surface and gave the machine an interface. The interface's own
documentation records the remaining gap: `start` and `restore` are deliberately
absent, and the lab daemon "owns those two, and they are the only places left
that know a machine's kind."

That claim has decayed. Kind-branching now appears in the lab runtime in around
seven further places — the web-forward installer, the declared-forward
installer, both host preflights, the shared-folder assembly, teardown, and the
snapshot and restore paths.

Worse, a second route exists that never crosses the interface at all: the
wscript host binds `VmHandle` and `ContainerHandle` directly to the concrete
machine types, re-implementing the kind split for the scripting surface.

The cost shows up as duplication with no single home. The two machine kinds
mirror roughly eighteen concepts between them, including one twenty-line body
that appears twice inside a single file. Readiness is the sharpest instance:
there are four implementations of "wait until ready" with three different
timeout policies, and **which one runs depends on whether the caller holds a
concrete machine or the interface** — inherent methods silently shadow the
interface's defaults. The interface has a `ready_timeout` accessor whose entire
purpose is to prevent that fork; it is honoured at one of the four call sites.

## Decision

**Every consumer of a machine goes through the `Machine` interface. There is no
second route.**

Concretely:

- `start` and `restore` move behind the interface. The kind-specific work
  becomes implementation, not a branch in the caller.
- Public inherent methods that duplicate interface methods are removed. If a
  concrete machine type needs an operation, the interface carries it; if the
  interface should not carry it, it is not a machine operation.
- The wscript host binds to the interface. `VmHandle` and `ContainerHandle`
  become one handle over `Arc<dyn Machine>`; wscript's existing per-kind
  vocabulary is preserved at the script level by capability checks, not by
  holding a different Rust type.
- Readiness policy lives in exactly one place: the interface's `ready_timeout`.
  Callers pass it; they do not pass literals.
- The capability surfaces — display, console log, and the kind-specific status
  fields — stay expressed as capabilities. Per **CONTEXT.md**, a capability is
  probed and reported, never inferred from whether the machine is a VM or a
  container. The display capability in particular must stop being structurally
  tied to one concrete machine type.

## Consequences

**Gained**

- Adding a machine kind means writing one implementation, not finding every
  branch. That is the leverage the interface was introduced for.
- Readiness has one home. The current fork — where a VM waits 600 seconds
  through one path and the same VM waits a different budget through another — is
  not expressible.
- Roughly 200 lines of forwarding between the concrete types and the interface
  delete, along with the duplicated body inside the container module.
- Tests can drive orchestration against machine doubles, because the interface
  is the only thing orchestration knows about.

**Given up**

- The wscript surface loses the ability to expose an operation on one machine
  kind purely by virtue of holding that kind's Rust type. Anything kind-specific
  must be modelled as a capability, which is more work up front.
- Dynamic dispatch on the hot path for a handful of operations. Immaterial next
  to QMP round-trip latency.

**Watch for**

- The interface growing to absorb every kind-specific operation. If a method is
  meaningful for one kind and returns "unsupported" for the other, it is a
  capability, not a machine operation. The escape hatch is a capability probe,
  not an untyped map — see [ADR-0004](0004-lab-status-is-a-typed-projection.md).
