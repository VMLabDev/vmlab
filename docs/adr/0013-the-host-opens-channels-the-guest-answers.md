# ADR-0013: The host opens channels, the guest answers

- **Status**: Accepted
- **Date**: 2026-08-05
- **Related**: [ADR-0002](0002-machine-is-the-only-route-to-a-machine.md),
  [ADR-0012](0012-vmlab-terminates-ssh-on-the-host.md),
  [ADR-0014](0014-the-workspace-is-a-guest-local-copy-of-a-canonical-host-tree.md)

## Context

The agent protocol has always been one-directional in its stream topology: every
`Open*` is a host message, and the guest's replies carry `Opened`, `Exited`,
`WindowAdjust`, `Error` and the like. Nothing in the vocabulary says *"I have a
new stream for you"*.

That was an unremarked property until PRD §19 asked the protocol to carry an SSH
facade (ADR-0012) and a bidirectional workspace syncer (ADR-0014). Both raise
requests that need the missing direction: `ssh -R` and `streamlocal-forward`
need a listener inside the guest that opens a channel outward when something
connects; agent forwarding needs `$SSH_AUTH_SOCK` served from inside the guest;
X11 the same. On the workspace side, a guest-side `vmlab` shim reporting a sync
conflict would need it too.

Each could be added. Each would also bring a guest-side listener with its own
lifetime rules, bind policy and reconnection semantics — against a multiplexer
that outlives its client.

## Decision

**The host opens channels; the guest only ever answers.** The agent protocol has
no guest-initiated channel open, and vmlab does not add one.

Everything that would need one is refused as a consequence, and the consequences
are named rather than enumerated as a list of requests:

- The SSH facade answers `session` and `direct-tcpip` and refuses
  `tcpip-forward`, `auth-agent-req@openssh.com` and `x11-req`.
- Sync conflict resolution is host-side, necessarily. The guest-side signal is a
  marker file the guest writes into its own workspace, not a call back.

Guest→host **messages** are unaffected and may be added: an `Eof` in that
direction was added for tunnel half-close. The distinction the invariant draws
is between a message on an existing channel and the creation of a new one.

## Consequences

**Gained**

- Channel lifetime has one owner. There is no case where the guest holds a
  resource the host has forgotten, or vice versa, and a snapshot-restore
  re-handshake that discards channel state stays cheap and correct.
- Every refusal in the SSH facade has one reason instead of five, so §19 states
  a rule rather than a table a future reader must keep extending.
- No guest-side listener exists to have a bind policy, a lifetime, or a
  reconnection race.

**Given up**

- `ssh -R` and agent forwarding, and with them one real workflow: an offline
  guest reaching a host-side mirror, proxy or licence server through a reverse
  tunnel. The answer is a NIC on a segment with egress, which the NAT engine
  already proxies over ordinary host sockets.
- Any future guest-initiated notification must be modelled as something the host
  drains, as the workspace watch is.

**Watch for**

- A feature that "just needs a small callback". Adding the direction is a
  visible amendment to this record, which is the point of writing it down —
  incremental drift into a bidirectional protocol is what it prevents.
