# ADR-0020: A hypervisor reports capabilities and composes its own launch

- **Status**: Accepted
- **Date**: 2026-10-30
- **Supersedes**: nothing
- **Amends**: [ADR-0001](0001-hypervisor-seam-substitutes-the-host.md)
  (where argv composition sits)
- **Related**: [ADR-0003](0003-decisions-are-values-computed-before-execution.md),
  [ADR-0008](0008-hardware-resolution-happens-once.md)

## Context

ADR-0001 moved the process handle and the QMP client behind the Hypervisor
seam so an adapter could be entirely in-memory. Two things still crossed it.

**The QMP client, through an escape hatch.** `Control::qmp()` returned the
concrete `QmpClient` to whoever asked, and every operation beyond the
lifecycle asked: online snapshots on both machine kinds, USB `device_add` and
`device_del`, `screendump` and keyboard and pointer input in the display, the
install CD-ROM eject, and `poweroff`'s `quit`. Each of those callers spoke QMP
itself. An adapter that is not QEMU could only answer `None`, and every one of
those operations then failed as "not running" whatever the adapter could
actually do.

**The argv, built above the seam.** `LaunchSpec` carried a finished
command line: `VmInstance` and `ContainerInstance` called
`qemu::build_args` and `qemu::container::build_container_args` themselves and
handed the seam a binary and its arguments. The seam module said so outright —
composing a machine was `crate::qemu`'s job, running it the seam's. Any
adapter that does not run QEMU would have to reverse-engineer what to launch
from an argv written for QEMU.

A second hypervisor, `vmlab-hv`, is coming, with its own control protocol and
its own launch description. It cannot be plugged in while either of these
holds.

## Decision

**What a running machine can do beyond the lifecycle is a set of
capabilities its control channel reports, and the seam receives a typed
description of what to launch, not a command line.**

Concretely:

- `Control` reports five optional capabilities, each a trait object or `None`:
  **snapshots** (save, load, delete an online snapshot), **input** (key
  chords, absolute pointer, buttons), **frames** (a screendump to a path),
  **hot-plug** (attach a host USB device, detach by id) and **media** (eject
  the install CD-ROM). `Control` also gains `pause`, the bracket an online
  restore puts around its load. `qmp()` is gone.
- A capability is probed and reported, never inferred from which adapter or
  which machine kind is running — the same rule CONTEXT.md already applies to
  a Display. A caller asks for the one it needs; when it is absent the caller
  refuses exactly as it did before, `<name>: not running`.
- The QEMU adapter implements every capability by sending the QMP commands
  its callers used to send, in the same order. QMP errors stay inside the
  `anyhow::Error` a capability returns, so the display can still tell a GL
  scanout's `no surface` from any other failure. The one translation it owns
  is `device_del`'s `DeviceNotFound`, which the hot-plug contract defines as
  success.
- `LaunchSpec` carries a `Launch`: a VM's resolved hardware, runtime paths
  (`VmPaths`), accelerator and any host GPU's own emulator build, or a
  container's resolved hardware and `ContainerVmPaths`. The caller still
  resolves hardware and computes every path, descriptor and environment
  change. The QEMU adapter composes the binary and argv from it by calling
  the same pure builders, which keep their tests (ADR-0003, ADR-0008).
- The fake reports no capability unless a scenario grants one; each one
  granted is an in-memory double. The lifecycle tests hold both answers: a
  missing capability is refused with the old message, a reported one is used.

## Consequences

**Gained**

- Nothing above the seam names QMP. A second adapter implements the
  capabilities its hypervisor has and reports `None` for the rest, and every
  caller already handles both.
- The caller's side of snapshots, USB, the display and eject becomes testable
  against the fake: online restore's pause–load–resume ordering, the USB
  tracking around hot-plug, a screenshot reaching disk.
- An adapter is handed the machine it is launching, so it can build its own
  launch description from the resolved hardware rather than from an argv.

**Given up**

- A launch that cannot be composed now fails inside `start_emulator` rather
  than before it. Both sit inside the same start attempt and tear down the same
  way, so nothing outside the seam sees a difference; but the fake no longer
  runs the argv builder at all, so a lifecycle test cannot catch a builder
  error. The builders' own tests are where that coverage lives.
- Snapshot device names (`disk0`…, `scratch`) still cross the seam as
  strings. They are the names the launch gave each disk, and an adapter that
  names disks differently maps them itself.
- The display's `no surface` fallback still recognises a QMP error by type.
  It is QEMU's answer to a QEMU-only situation, and a second adapter that has
  no such case never produces it.

**Watch for**

- A capability that grows a method only QEMU can answer. Each trait is stated
  in terms of what the operation does to the machine; a QMP command name in a
  signature is the drift to refuse.
- Refusal drift. "not running" is the message every missing capability gives
  today because it is the message callers already gave. When a running machine
  can lack a capability in practice, a refusal that names the capability is
  the honest answer, and changing it is a user-visible change to make
  deliberately, not as a side effect.
