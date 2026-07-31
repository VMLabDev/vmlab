# ADR-0008: Hardware resolution happens once, for both machine kinds

- **Status**: Accepted
- **Date**: 2026-07-31
- **Related**: [ADR-0002](0002-machine-is-the-only-route-to-a-machine.md)

## Context

Hardware resolution — the precedence chain by which a machine's CPU count,
memory, firmware, TPM, display device and bus choices are taken from the VM
declaration, then the template, then the profile — is implemented once,
correctly, in about a hundred lines, and directly tested for its precedence
behaviour.

It is then re-implemented three more times, and all three are wrong in different
ways:

- **Containers have no resolver.** They read their declared value or fall back to
  a module constant. There is no profile layer and no resolved-machine type, and
  the default differs from the VM default by an order of magnitude. The
  container argv builder consequently takes loose positional scalars where the
  VM path takes a resolved value.
- **The argv builder's own test fixture mirrors the resolver.** It constructs a
  resolved VM by hand from a profile, including a comment-flagged copy of the
  arch-aware display-device selection. Every argv test therefore exercises the
  mirror, not the real resolver: if the real selection changes, the tests still
  pass.
- **The designer implements a partial version.** Its help text promises the
  template-then-profile chain; it resolves the template only. Any VM whose
  memory comes from its profile — which is every VM using a shipped profile —
  shows the wrong inherited value in the form.

Separately, the argv builder documents itself as a pure function of resolved
hardware and runtime paths, and is not one: firmware lookup probes the host
filesystem from inside it. One of its tests asserts that the build host ships
edk2. The seam is nearly there already, since the firmware *variables* path is
injected alongside the other runtime paths — only the firmware image itself is
not.

## Decision

**Hardware resolution has one implementation, it covers both machine kinds, and
nothing mirrors it.**

Concretely:

- Containers get a resolved-machine value from the same resolver. The precedence
  chain is the same chain; fields that do not apply to a container are absent
  from its resolved shape, not silently defaulted elsewhere.
- The container argv builder takes a resolved value rather than positional
  scalars, matching the VM path.
- Argv tests call the real resolver. The mirrored fixture is deleted. A test
  that needs a specific resolved shape builds it by resolving real inputs.
- Firmware joins the injected runtime paths, next to the firmware variables
  path. The argv builder becomes pure as its documentation already claims, and
  its tests stop depending on the build host's firmware layout.
- The designer consumes the same resolution rather than approximating it. Per
  [ADR-0005](0005-the-schema-is-reflected-not-restated.md), inherited-value
  display is derived from one source, not restated per surface.

## Consequences

**Gained**

- Argv tests stop passing against a mirror, which is currently a silent hole
  under thirteen tests.
- The argv builder becomes genuinely pure, so firmware selection can be tested
  for candidate ordering and secure-boot variant choice — none of which is
  tested today.
- The designer shows the value the machine will actually boot with.
- Containers gain the profile layer, which is the mechanism by which micro-VM
  hardware defaults are meant to be adjustable at all.

**Given up**

- Firmware discovery moves up the call chain to whoever assembles runtime paths,
  which is a slightly wider change than it appears — that assembly happens in
  the machine lifecycle, not in the argv builder.
- Containers acquire a resolved shape they did not need, which is added
  machinery for a machine kind with a much smaller hardware surface.

**Watch for**

- One resolved type accreting fields that only apply to one kind. If the shape
  splits, split it — the decision is that resolution happens once, not that the
  result is one type.

## Note on scope

This is tightening, not deepening. The resolver and the argv builder are among
the healthiest modules in the codebase; the friction is in what surrounds them.
Sequenced accordingly — last, and safe to defer.
