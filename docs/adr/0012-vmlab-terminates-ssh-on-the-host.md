# ADR-0012: vmlab terminates SSH on the host

- **Status**: Accepted
- **Date**: 2026-08-05
- **Related**: [ADR-0002](0002-machine-is-the-only-route-to-a-machine.md),
  [ADR-0007](0007-the-wire-protocol-carries-a-typed-vocabulary.md),
  [ADR-0013](0013-the-host-opens-channels-the-guest-answers.md),
  [ADR-0011](0011-a-lab-name-is-its-host-global-runtime-identity.md)

## Context

PRD §19 publishes a dev machine as an SSH endpoint an editor attaches into. The
obvious construction is an `sshd` inside the guest, reached either over a §9.8
port forward or over a tunnel on the agent channel.

Both reach the same wall. A key-authenticated Windows OpenSSH session for a
domain account logs on via `KERB_S4U_LOGON` and holds **no network
credentials** — Microsoft's stated design, a compile-time constant, not
configurable. The session's identity is right and `\\dc\share` fails. For a lab
whose whole point is developing against a real domain, that makes "on the
domain" true for authorization and false for everything else. The rest of the
guest-sshd surface comes along with it: a home directory resolved from
`ProfileImagePath` that falls back for a never-logged-on domain user, a
`administrators_authorized_keys` redirect rather than a fallback, `cmd.exe` as
the default shell, and one host key shared by every clone of a template.

Changing the *transport* to the agent channel sidesteps none of it, because
sshd is still the thing authenticating.

`vmlab-agent` runs as LocalSystem on Windows and root on Linux. A LocalSystem
`LogonUser` plus `CreateProcessAsUserW` was measured against a live offline
domain to yield a **real initial TGT** and a successful read of a share ACL'd to
that account alone.

## Decision

**vmlab terminates SSH itself, on the host, and the guest runs no sshd.**

The SSH server lives in `labd`, beside the agent client, the cached logon and
the feature probe. It maps SSH channels onto agent channels: `session` to
terminal and exec, `subsystem sftp` to a host-side SFTP implementation over the
agent's file vocabulary, `direct-tcpip` to an agent tunnel stream. The endpoint
is a stdio `ProxyCommand` and nothing else — `vmlab ssh-proxy` connects one lab
command's returned unix socket to stdin/stdout and does nothing more, so nothing
listens on the host and no port is leased.

Authentication is `none`. There is no network path to the facade, so the trust
boundary is already "can you exec the proxy against this lab socket"; the SSH
username is a *selector* over the machine's declared logins, not a credential.
vmlab owns a per-machine host key and its own `known_hosts`.

Rejected: the proxy process terminating SSH and driving channels over the lab
protocol. That re-exports agent-proto through ADR-0007's typed vocabulary, and
every one of the several `ssh`/`scp` processes a client spawns per session pays
for it.

## Consequences

**Gained**

- An attached session holds real network credentials, because the agent mints
  the logon rather than sshd. The domain case works rather than half-working.
- No guest-side SSH surface at all: nothing to install, configure, secure or
  keep consistent across two guest families, and no host key to clone or roll
  back.
- No NIC, no lease, no forward. A machine on no segment at all is attachable,
  identically for VMs and container micro-VMs.
- One place implements SFTP, and its file vocabulary is reusable by the
  console's transfer and the workspace syncer.

**Given up**

- vmlab now owns an SSH server implementation and its compatibility surface.
- The facade's answerable request set is a published contract: an editor needing
  something outside it does not work, and `direct-tcpip` in particular is
  mandatory rather than a convenience.
- Two stacked flow-control layers. The facade must never grant SSH window it
  cannot back with agent credit, or `labd` buffers the difference without bound.

**Watch for**

- Anything that reintroduces a guest-side listener — it will be reaching for the
  invariant ADR-0013 forbids.
- Any second path to the endpoint. The stdio proxy being the only shape is what
  keeps "nothing listens" true.
