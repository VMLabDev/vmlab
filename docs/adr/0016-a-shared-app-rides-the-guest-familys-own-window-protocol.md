# ADR-0016: A shared app rides the guest family's own window protocol

- **Status**: Accepted
- **Date**: 2026-10-15
- **Supersedes**: nothing
- **Related**: [ADR-0013](0013-the-host-opens-channels-the-guest-answers.md),
  [ADR-0015](0015-one-guest-side-spawner-seam-carries-identity.md)

## Context

PRD §20 puts one guest program's windows on the host desktop, for Linux and
Windows guests, on Wayland and X11 hosts. Remoting individual windows is a hard
problem that three protocols already solve. Waypipe forwards Wayland clients.
X11 has been network-transparent since 1987. RDP RemoteApp is the only seamless
per-window option a Windows guest offers.

Those protocols assume a network path, an SSH tunnel or a TCP port. vmlab has
neither by default. The agent channel (§7.4) is the one path every machine has,
and ADR-0013 lets only the host open channels on it. Waypipe and X11 clients
connect out of the guest, once per Wayland or X client.

The SSH editor-attach surface had a general TCP `tunnel` vocabulary, and it was
removed with that surface. Bringing it back as a general port forward would
rebuild the thing that was deliberately deleted.

## Decision

**vmlab writes no window protocol. A shared app carries the guest family's own
protocol over the agent channel, and the host end runs in the CLI.**

- One `open_app` channel per session carries length-prefixed records. Each
  transport connection is its own `open_app_conn` channel with its own credit
  window, so one stalled connection cannot hold up the others.
- A connection the guest makes is accepted and parked by the agent, which
  announces it with a `pending` record on the open app channel. The host adopts
  it with `open_app_conn`. That is a message on an existing channel followed by
  a host-opened channel, so ADR-0013 stands unamended.
- The vocabulary is scoped to a session the host opened for one program. RDP's
  `open_app_conn` dials one fixed destination, the guest's own loopback port
  3389, and nothing else. It cannot be used as a port forward.
- For X11 the host mints the cookie, writes it into the guest, and checks it on
  every connection before swapping in the host display's real cookie, the way
  OpenSSH forwards X11. A guest process that has not been given the cookie
  cannot reach the host display.
- The CLI owns the host end because the compositor and X server only answer
  processes in the user's desktop session. The lab daemon brokers the agent
  channels and stays headless (§11).

## Consequences

- Each guest family gets a mature, maintained protocol: compression and video
  encoding from waypipe, clipboard and seamless windows from RemoteApp. vmlab
  carries bytes and launches the two ends.
- The cost is three host-side dependencies (`waypipe`, an X server, FreeRDP)
  and one guest-side one (`waypipe`). A guest without waypipe falls back to X11,
  the one protocol that needs nothing installed in the guest.
- X11 forwarding gives the guest program the same reach into the host X server
  as `ssh -X`, including reading input to other X clients. That is consistent
  with §1.2, where vmlab is not a security boundary, but it is a reason to
  prefer waypipe on a Wayland host.
- RDP on a client edition of Windows takes over the account's console session.
  That is the protocol's behaviour, and vmlab states it rather than working
  around it.
- Every new transport connection costs one control round trip through the lab
  daemon before bytes flow. Waypipe and X11 open few connections, so this is
  latency at app start, not throughput.
