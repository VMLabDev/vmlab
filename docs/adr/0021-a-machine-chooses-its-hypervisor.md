# ADR-0021: A machine chooses its hypervisor, and vmlab-hv is the second

- **Status**: Accepted
- **Date**: 2026-10-30
- **Supersedes**: nothing
- **Amends**: PRD §1.2 (the "hypervisors other than QEMU/KVM" non-goal)
- **Related**: [ADR-0001](0001-hypervisor-seam-substitutes-the-host.md),
  [ADR-0005](0005-the-schema-is-reflected-not-restated.md),
  [ADR-0008](0008-hardware-resolution-happens-once.md),
  [ADR-0020](0020-a-hypervisor-reports-capabilities-and-composes-its-own-launch.md)

## Context

PRD §1.2 listed hypervisors other than QEMU/KVM as a non-goal. `vmlab-hv`
(github.com/VMLabDev/vmlab-hypervisor) is vmlab's own hypervisor: a clean-room,
MIT-licensed Rust VMM that vmlab runs one process per machine, in place of
QEMU. It grows in phases — container micro-VMs on x86_64 under KVM first, then
modern x86_64 VMs, then emulation, then legacy PCs — and QEMU stays the
default until each phase has passed its gate (the hypervisor project's
decisions D2, D29).

ADR-0020 made the seam ready for it: capabilities instead of QMP, and a typed
launch each adapter composes. What was left was choosing, per machine, which
adapter runs it, and what that choice means for validation, the host config
and snapshots.

## Decision

**Every machine runs on exactly one hypervisor, named by
`hypervisor = "qemu" | "vmlab"` on its block, else by the host config's
default for its kind, which starts at QEMU (D11, D27, D29).**

- The field sits on `vm` and `container` blocks only. It never resolves
  through template or profile: which program runs the hardware is not
  hardware (ADR-0008).
- The host default is keyed by machine kind — `host { hypervisor { container,
  vm } }` — because the cutover is per phase: a phase that passes its gate
  flips the default for its machines alone.
- What vmlab-hv cannot run yet is **one list**,
  `config::hypervisor::vmlab_refusals`, read by §5.1 validation and again by
  the lab daemon before it builds a machine, each refusal naming the machine
  and the reason. Phase 1 refuses a VM outright, and a container on a host
  that is not x86_64 or has no usable `/dev/kvm`. `qemu_args` is QEMU's argv
  and is refused on any other hypervisor (D16). A phase that lands shrinks the
  list.
- `labd::hypervisor::VmlabHv` is the second production adapter. It composes a
  `vmlab_hv_proto::MachineDescription` from the typed container launch,
  device for device what the QEMU argv gives the guest, writes it to the
  machine's runtime directory, runs `vmlab-hv --machine <file> --control
  <socket> --paused`, and drives the control socket through the protocol
  crate's client. Its capabilities are what the handshake advertised. The
  protocol crate is a git dependency pinned by revision.
- Snapshots record the hypervisor that took them. An online snapshot's RAM and
  device state are that hypervisor's own format, so restoring one on a machine
  that now runs on the other is refused, naming the one that took it; offline
  snapshots are plain qcow2 internal snapshots and restore under either (D31).
  A record without the field is QEMU's.

## Consequences

**Gained**

- One machine can run on vmlab-hv inside an otherwise-QEMU lab, which is how
  each phase is dogfooded before its default flips.
- The refusal list is the phase boundary in code: what vmlab-hv cannot do is
  said in one place, in words a user can act on, before anything starts.
- The fake-driven lifecycle tests run against the vmlab-hv adapter's own
  protocol handling, through a fake control server built on the protocol
  crate's `Server`.

**Given up**

- `qemu-img` stays a runtime dependency on vmlab-hv machines: the scratch disk
  and offline snapshots are still made with it until `vmlab-disk` reaches
  parity.
- An online snapshot does not survive a hypervisor switch. Nothing is lost —
  switching back makes it restorable — but there is no converter.
- vmlab-hv has no TCG, so a container that falls back to emulation under QEMU
  is refused on vmlab-hv instead.

**Watch for**

- The refusal list drifting from what the binary actually does. The daemon
  re-checks it, but a capability the hypervisor stops advertising is the
  handshake's word, not the list's.
- The protocol pin. The crate is a git revision; a protocol version bump on
  the hypervisor side fails the handshake with both versions named, which is
  the cue to move the pin.
