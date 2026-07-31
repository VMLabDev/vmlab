# ADR-0001: The Hypervisor seam substitutes the host, and Machine is the test surface

- **Status**: Accepted
- **Date**: 2026-07-31
- **Supersedes**: nothing
- **Related**: [ADR-0002](0002-machine-is-the-only-route-to-a-machine.md)

## Context

`vmlab` boots real QEMU. Everything that decides *when* a machine is running —
the start ladder, the power-state machine, the exit monitor, the readiness gate,
the stop ladder, teardown ordering, the container restart policy and its backoff
— sits above that, and none of it needs a hypervisor to be correct.

Commit `cc37a1d` introduced `trait Hypervisor` to make exactly that code
testable, along with a `FakeHypervisor` driven by a failure `Script` and a
`#[cfg(test)] set_hypervisor` injection point on the VM.

The seam was never consumed. `set_hypervisor` has one occurrence in `src/` — its
own definition. `FakeHypervisor` never leaves `hypervisor.rs`, where five tests
exercise the fake itself. The VM lifecycle module remains at ~1100 lines with no
tests, and the container lifecycle module — which carries the harder logic
(restart policy, rapid-failure accounting, stop-during-backoff cancellation) —
has no injection point at all.

Two properties of the seam explain why it stalled:

1. **It is placed at "which binary do I exec."** The three operations are start a
   software TPM, start a filesystem daemon, start the emulator.
2. **It returns concrete host types.** Handing back a process handle and a live
   QMP client means any adapter must produce real ones, so the fake spawns real
   `/bin/sh` processes and stands up a real mock QMP server rather than being an
   in-memory double.

Together those make the fake expensive enough that writing lifecycle tests
against it never happened.

## Decision

**The Hypervisor seam substitutes the host, and the `Machine` interface is the
surface lifecycle behaviour is tested through.**

Concretely:

- The process handle and the QMP client move **behind** the interface. The seam
  returns handle types owned by the seam, not by the QEMU module. An adapter is
  then free to be entirely in-memory.
- The seam is expressed in terms of *what running means* — the machine is up,
  the machine answers control, the machine exited with this reason — rather than
  which executable was launched.
- `ContainerInstance` gains the same injection point `VmInstance` has. The seam
  covers both machine kinds or it covers neither.
- Two adapters exist and both are exercised: the QEMU one in production, the
  fake one in tests. One adapter would mean a hypothetical seam; two make it
  real.
- Lifecycle tests assert through `Machine` — power state, readiness, callbacks
  fired, exit classification — not through the seam and not through the concrete
  machine types.

## Consequences

**Gained**

- Roughly 2,200 lines of lifecycle implementation across the two machine kinds
  become reachable by tests that need no KVM, no root, and no `/dev/kvm` on the
  build host.
- Exit classification, which is already tested in isolation, gets tested
  *together with* its caller — so "we clear the ready flag and tear down in the
  right order before firing the exit callback" becomes a provable claim rather
  than an assumed one.
- The container restart ladder — rapid-failure accounting, backoff, cancellation
  during backoff, the re-entrancy guard on restart — becomes testable, which is
  the single riskiest untested region in the lab daemon.
- CI stops depending on host QEMU layout for this class of behaviour.

**Given up**

- The seam's return types are no longer the QEMU module's types, so the QEMU
  adapter carries a small mapping layer it did not carry before.
- Behaviour that genuinely requires a hypervisor — QMP snapshot save and load,
  the vhost-user handshake, fast-path offload — stays out of reach and must
  still be verified against a running lab. The seam is not a claim that QEMU
  needs no integration testing.

**Watch for**

- Adapter drift: a fake that diverges from QEMU's real behaviour turns green
  tests into false confidence. The fake's failure `Script` should model failures
  that have actually been observed, not invented ones.
