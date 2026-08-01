# ADR-0009: Build hardware resolves block over source template, and the profile stays live

- **Status**: Accepted
- **Date**: 2026-08-01
- **Related**: [ADR-0008](0008-hardware-resolution-happens-once.md)

## Context

A template build is modelled as a one-VM `scratch` lab whose primary disk is
pre-seeded from the source, so the build reuses the whole lab runtime. The
synthetic lab is rendered as WCL text from the template definition, and the
renderer enumerates the fields it emits by hand.

That hand-written list has drifted behind the template block's schema twice. It
first swallowed declared NICs, which built with no network hardware at all. It
then swallowed the block's own firmware, TPM, secure boot, display, nested and
raw QEMU args — all of which the schema accepts on a template block and all of
which parse into the definition — so a template declaring OVMF and secure boot
installed its guest on SeaBIOS. Both were repaired field by field, the second in
#23, which also moved the firmware/secure-boot conflict check onto the build
path as a pre-flight.

What neither repair reached is the layer above the block. A layered build — one
whose source is an existing template — resolves its source through the store and
reads exactly one field off the recorded metadata, the first-boot script, then
discards the rest. The profile falls back to a literal `linux-generic`.
Rebuilding a Windows 11 template that recorded q35, OVMF, secure boot and a TPM
boots the installed disk on SeaBIOS with no TPM, which an OVMF-installed guest
does not survive.

The loss then compounds. Sealed metadata is built from the template definition
alone, so a layered build's *output* records only what its own block restated.
Layer onto a template recording `windows-11` without restating the hardware and
the new template records no profile at all; VMs cloning it resolve against the
"assume nothing" floor. Every further layer loses more.

Two things make this a decision rather than a repair. Recording *hardware* in
template metadata is what that metadata is for — it is the middle layer of the
§5.2 chain — so the question is which layers a build freezes into an image and
which it leaves to resolve later. And the build VM has no template layer by
construction (§6.5), so the source's hardware has to reach it some other way.

## Decision

**Effective build hardware is the template block over the source template's
recorded metadata, computed once, and it drives both the build VM and the
sealed image. The profile is never frozen.**

Concretely:

- One merge produces effective hardware from a template definition and the
  resolved source's metadata, if any. Each field is the block's value or else
  the source's. It is a pure function, and it is the whole of the logic.
- The rendered build lab carries those values as vm-block attributes. The build
  VM stays a `scratch` VM: its disk is pre-seeded rather than cloned, so it has
  no template layer to inherit through, and the source's hardware is rendered
  onto the block instead.
- Sealed metadata records the same effective values rather than only the ones
  the block restated.
- **The profile layer stays live.** No profile-derived value is frozen into
  either the rendered lab or the sealed image. A template whose effective
  profile is `windows-11` still picks up later edits to that profile when a VM
  clones it. A field neither the block nor the source declared stays absent.
- `linux-generic` remains the fallback only when neither layer names a profile —
  vmlab's default layer, below the profile, not a substitute for it.
- Everything downstream of the rendered lab is unchanged. The one resolver of
  ADR-0008 still applies VM block > template > profile over it. This merge is
  the narrower block-over-source step that happens *before* rendering, and it
  does not reach into profiles.

## Consequences

**Gained**

- Builds install their guest on the hardware the template asked for, which is
  the difference between a working OVMF image and one that never boots.
- Layered builds inherit hardware the way a clone does, so the precedence a lab
  author already understands is the precedence a rebuild gets.
- Metadata stops decaying across layers: a chain of five layered rebuilds ends
  with the profile the first one recorded.
- Templates stay responsive to profile edits, which is the mechanism by which
  hardware defaults are adjustable at all.

**Given up**

- Sealed metadata is no longer a complete description of the hardware a machine
  will boot with — it deliberately records two layers of three. Anything wanting
  the full picture must resolve, not read.
- Effective hardware is a second, narrower merge living beside the one resolver.
  It is not the §5.2 chain and must not grow into a copy of it.

**Watch for**

- The renderer drifting behind the schema again. Two field-sets have been lost
  and both were repaired by adding more hand-written lines. A third says the
  enumeration itself is the defect and the emitter should be exhaustive by
  construction.
- Firmware and secure boot separating. They only mean anything together —
  validation rejects secure boot on a machine resolving to SeaBIOS and names the
  layer each side came from — so any path that carries one carries both. The
  pre-flight added in #23 is where a conflicting pair is now caught, so the merge
  has to happen before it: inherited hardware arriving afterwards would reach the
  build unchecked.
- Pressure to freeze the profile "just for reproducibility". That is the option
  this record rejected; reproducibility of an image is the disk plus its
  recorded layers, not a snapshot of host-side defaults.

## Note on scope

This is a correctness fix that happened to force a decision, not a deepening.
The affected surface is the build lab renderer and the seal step; the resolver
it sits above is untouched. Found while triaging the layered-build report, which
described one of the three failures.
