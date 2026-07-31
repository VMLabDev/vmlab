# ADR-0003: Decisions are computed as values before execution

- **Status**: Accepted
- **Date**: 2026-07-31
- **Related**: [ADR-0004](0004-lab-status-is-a-typed-projection.md)

## Context

Commit `aab9bce` extracted the wave ordering of `up` and `down` into a
`LabPlan` — a value computed in full before anything is started or stopped. The
commit records that doing so immediately surfaced a real defect: `down` was not
expanding dependents.

The rest of the lab runtime did not follow. `LabRuntime` is one type carrying
around eleven distinct responsibilities across ~2,300 lines, and it is the only
module in the lab daemon over a thousand lines with no tests at all. The reason
it has none is structural: every one of those responsibilities is reachable only
by constructing a whole runtime, which requires a parsed lab file, a real
template store, a real image cache and a real filesystem.

Four of them are pure decisions wrapped in an impure shell, and each is
currently inseparable from the shell:

- **Shared folders.** Which shares ride virtiofs and which fall back to SMB,
  which segments need a gateway rule, and which host port the bundled server
  takes — the last decided by walking a port range looking for a free one.
- **Guest mount steps.** The commands that mount those shares inside a guest,
  including hardcoded Windows filesystem-driver paths and registry keys sitting
  in the orchestrator rather than in the guest-facing module that already has a
  type for exactly this.
- **Pull bookkeeping.** The state machine a template or image download moves
  through — pending, active, progress, done, error, cancelled — and the progress
  arithmetic that feeds the console.
- **Port forwards.** Three near-identical routines that each resolve a machine,
  take its lease address, learn its hardware address and install a rule.

A related symptom: the same hand-rolled "wait for the exit monitor to settle"
loop is written out four times in the teardown paths, while the machine
interface has a method that does precisely that.

## Decision

**A decision is computed as a value before anything acts on it. Computing the
value and carrying it out are separate operations with separate tests.**

This generalises the `LabPlan` move. Each of the four decisions above becomes
its own module, with its own seam:

| Module | Computes |
| --- | --- |
| Share plan | which shares ride which transport, per segment, and the server's host port |
| Mount steps | the ordered guest commands that mount a share plan, per guest OS |
| Pull ledger | the lifecycle and progress of template and image downloads |
| Forward plan | the port-forward rules a lab's machines require |

Four seams rather than one: the concerns share a shape but not a subject, and
collapsing them into a single plan module would produce a type whose fields have
nothing to do with each other.

Rules that apply to all of them:

- The computing function takes configuration and observed state and returns a
  value. It performs no I/O — no filesystem, no sockets, no subprocesses.
- Anything the decision depends on that *is* I/O — whether a host port is free,
  whether a binary is on the path — is passed in as data or as an injected
  probe, not performed inside.
- The value is the test surface. Tests build inputs, call the function, and
  assert on the returned value. They do not reach past it into the executor.
- The executor takes the value and carries it out. It is thin enough that its
  own correctness is a matter of integration testing against a running lab.

Guest-OS knowledge does not live in the orchestrator. Mount-step generation is
the boundary case and it moves to the module that owns guest-side transport.

## Consequences

**Gained**

- Locality: the rule that a share is served by exactly one transport becomes a
  property of one module rather than a comment in a different one.
- The share plan, mount steps, pull ledger and forward plan all become testable
  with no lab, no registry and no guest.
- `LabRuntime` shrinks toward the thing it is named for — wave orchestration —
  at roughly a quarter of its current size.
- Precedent: `LabPlan` found a real defect the moment it became a value. Each of
  these four carries untested branch logic of comparable depth.

**Given up**

- Four more modules and four more seams to keep coherent. The trade is accepted
  because a single combined plan type would have no meaningful invariants.
- Some decisions become two-phase where they were one, which reads as more
  indirection at the call site.

**Watch for**

- A "plan" that is really an instruction list with one possible reading. If the
  computed value has no branches worth asserting on, the split has not paid for
  itself and the module should be folded back.
