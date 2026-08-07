# ADR-0015: One guest-side Spawner seam, and identity is its parameter

- **Status**: Accepted
- **Date**: 2026-08-07
- **Supersedes**: nothing
- **Related**: [ADR-0001](0001-hypervisor-seam-substitutes-the-host.md),
  [ADR-0012](0012-vmlab-terminates-ssh-on-the-host.md),
  [ADR-0013](0013-the-host-opens-channels-the-guest-answers.md)

## Context

PRD §19.2 makes *who a process runs as* a per-channel decision: everything a
person invokes defaults to the machine's declared `login {}`, everything vmlab
does on its own behalf keeps the agent identity, and the workspace syncer is the
one exception that writes as the developer.

`vmlab-agent` created processes and handles in three unrelated places, each
doing its own creation on each of two guest targets:

- `linux.rs::open_terminal` — `openpty` + `fork`/`execve`, with a second fork
  and a `chroot` in container mode; `windows/conpty.rs` — `CreatePseudoConsole`
  + `CreateProcessW`.
- `exec.rs` — `std::process::Command` with piped stdio, which `linux.rs`
  overrode to reroute through the `--nsexec` trampoline.
- `files.rs::open_push` — `create_dir_all` + `File::create`, plus a
  `#[cfg]`-split `apply_mode`.

Six creation sites for one question. Adding a logon would mean answering it six
times, in six shapes, and — because every site also owned its own session
plumbing (register, pumps, reaper) — with no way to test any of it without a
live guest.

ADR-0001 diagnosed the same failure host-side and named the two properties that
made the Hypervisor seam unusable: it was placed at *which binary do I exec*,
and it returned concrete host types, so the fake had to spawn real processes.

## Decision

**All guest-side process and handle creation goes through one `Spawner` seam,
and the identity the work runs as is a parameter of every call.**

Concretely:

- The seam is expressed as *start a shell on a terminal*, *start a process with
  piped stdio*, *create a file for writing* — not as which OS call is made.
- It hands back its own `Spawned` (input, output, errors, resize, kill, wait)
  and `WriteFile` handles rather than a `Child`, an `OwnedFd` or a `HPCON`, so
  an adapter can be entirely in-memory.
- Every method takes an `Identity`. `Identity::Agent` — the PRD §19.2 floor, no
  logon — is the only value today; the declared logins join the enum, and every
  caller already passes one.
- Three adapters exist and all three are exercised: `LinuxSpawner`,
  `WindowsSpawner`, and the in-memory `FakeSpawner` in the session tests.
- With creation behind the seam, the terminal's session plumbing becomes
  portable the way exec's already was, so `Platform` drops `open_terminal` and
  `open_exec` and carries `spawner()` instead. `Platform` is left with what is
  genuinely OS-shaped: clipboard, event log, net/OS info, shutdown.
- Tests assert through the wire — `AgentMsg` frames off a capture port — not
  through the seam. The seam's call log is asserted only for the one claim
  nothing else can express: which identity a channel resolved to.

**What the seam deliberately does not cover.** A tunnel (§19.5) creates a
socket, not a process or a file. It dials from the agent's own network context
whatever identity a channel resolved to, and a container micro-VM shares the
guest's network stack, so there is nothing for an identity to change. It stays
portable in `tunnel.rs` with no platform hook and no spawner.

## Consequences

**Gained**

- §19.2's logon work is one change per surface instead of three: the `Identity`
  enum grows, each adapter grows one branch, and the mux dispatch resolves the
  login. No session plumbing moves.
- Terminal, exec and push session behaviour — flow control, the kill hook, exit
  reporting, teardown ordering — becomes testable with no guest, no child
  process and no filesystem. Nine such tests exist that could not before.
- The two guest targets stop carrying two copies of the same session plumbing.
  ConPTY and PTY differences are now confined to producing a `Spawned`.

**Given up**

- A layer of indirection on every session open, and `Spawned`'s boxed closures
  where the old code used the concrete handle directly.
- The adapters no longer share the caller's error strings, so a message like
  `terminal: no shell found in this guest` is now split across the adapter (the
  text) and the caller (the prefix).

**Watch for**

- `files::open_pull` and `tail` still open files directly. Reads were left out
  because the ticket scoped to writes, where ownership is what §19.2 argues
  about — but §19.2 lists `pull` as person-invoked, so the seam will need an
  open-for-read before that lands.
- Adapter drift, as ADR-0001 warns: `FakeSpawner`'s kill closes its output
  streams and reports 137 because that is what a SIGKILL'd shell does. If the
  real adapters stop behaving that way, the fake must follow.
