# ADR-0013: The host opens channels, the guest answers

- **Status**: Accepted
- **Date**: 2026-08-05
- **Related**: [ADR-0002](0002-machine-is-the-only-route-to-a-machine.md),
  [ADR-0014](0014-the-workspace-is-a-guest-local-copy-of-a-canonical-host-tree.md)

## Context

The agent protocol has always been one-directional in its stream topology: every
`Open*` is a host message, and the guest's replies carry `Opened`, `Exited`,
`WindowAdjust`, `Error` and the like. Nothing in the vocabulary says *"I have a
new stream for you"*.

That was an unremarked property until PRD §19 asked the protocol to carry a
bidirectional workspace syncer (ADR-0014). Guest changes have to reach the host,
and a sync conflict has to be resolved somewhere. The obvious constructions need
the missing direction: a guest-side watcher pushing changes as they happen, or a
guest-side `vmlab` shim that reports a conflict and asks how to resolve it.

Either could be added. Each would also bring a guest-side listener with its own
lifetime rules, bind policy and reconnection semantics — against a multiplexer
that outlives its client.

## Decision

**The host opens channels; the guest only ever answers.** The agent protocol has
no guest-initiated channel open, and vmlab does not add one.

What would need one is shaped around it instead:

- Guest changes reach the host because the host drains them. The `watch`
  vocabulary accumulates a dirty set in the guest, and the host asks for it.
- Sync conflict resolution is host-side, necessarily. The guest-side signal is a
  marker file the guest writes into its own workspace, not a call back.

Guest→host **messages** are unaffected and may be added. The distinction the
invariant draws is between a message on an existing channel and the creation of
a new one.

## Consequences

**Gained**

- Channel lifetime has one owner. There is no case where the guest holds a
  resource the host has forgotten, or vice versa, and a snapshot-restore
  re-handshake that discards channel state stays cheap and correct.
- A request that needs the guest to open a channel has one reason for being
  refused, so §19 states a rule rather than a table a future reader must keep
  extending.
- No guest-side listener exists to have a bind policy, a lifetime, or a
  reconnection race.

**Given up**

- Any guest-initiated notification must be modelled as something the host
  drains, as the workspace watch is. It arrives when the host asks, not when it
  happens.
- A guest-side process cannot resolve a sync conflict. Resolution runs from the
  host, through `vmlab dev sync`, even when the person resolving it is working
  inside the guest.

**Watch for**

- A feature that "just needs a small callback". Adding the direction is a
  visible amendment to this record, which is the point of writing it down —
  incremental drift into a bidirectional protocol is what it prevents.
