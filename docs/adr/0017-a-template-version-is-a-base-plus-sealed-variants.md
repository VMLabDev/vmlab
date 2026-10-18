# ADR-0017: A template version is a base plus sealed variants, stored as overlays

- **Status**: Accepted
- **Date**: 2026-10-18
- **Supersedes**: nothing
- **Related**: [ADR-0008](0008-hardware-resolution-happens-once.md),
  [ADR-0009](0009-build-hardware-resolves-block-over-source-template.md),
  [ADR-0010](0010-the-supervisor-owns-the-template-store.md)

## Context

A Windows template is mostly install and update time: hours of setup, then a
few minutes of sysprep. Several labs want the same installed Windows sealed in
different ways. One wants the plain generalize. Another wants a GPU driver
package installed first, with secure boot off. Before variants, each of those
was its own template with its own full build and its own full-size disk, even
though everything before the last step was identical.

A sysprepped disk is a dead end for further work: the generalize is what makes
every clone a new machine, and it must be the last thing that happens. So the
shared part has to be kept *before* the generalize, and the sealing steps have
to hang off it.

qcow2 offers two ways to keep several states of one disk:

- **Internal snapshots** inside one file. Rejected. A linked clone is created
  with `qemu-img create -b <file>`. It backs onto a file's *current* state and
  cannot name an internal snapshot. `qemu-img convert`, which the OCI
  push path and any export or copy would go through, drops internal snapshots.
  The word "snapshot" also already means a VM snapshot in vmlab (§7.3).
- **Overlays**: a separate qcow2 file whose backing file is the shared disk.
  Chosen. A clone backs onto an overlay exactly as it backs onto a template
  today, and the chain clone → variant → base is ordinary qcow2.

## Decision

**A template version is one base disk plus zero or more sealed variants. Each
variant is a qcow2 overlay on the base, at `variants/<name>/disk.qcow2`, with
the relative backing file `../../disk.qcow2`.** The version's single
`template.wcl` carries one `variant` block per variant (§6.6).

- **The base is unsealed** on a template with variants. A lab machine cannot
  clone it, because every clone would share one machine identity. `#base` names
  it only in build contexts: a variant build or a layered `from`. A template
  with no variants is base-only, and its base is the template exactly as before.
- **A reference selects a disk.** `#<name>` selects that variant. No `#`
  selects the default variant, which is the one marked `default = true` or a
  lone variant. Two or more variants without exactly one default is invalid
  metadata.
- **Metadata is effective.** A variant's metadata is the base's overridden
  field by field. Hardware resolution, first boot, the wscript surface check
  and agent staleness all read the effective metadata of whatever the reference
  resolved to, and one function computes it.
- **A variant names the base it was built on** (`base_sha256`). If that is no
  longer the base's `sha256`, the variant is stale and refused until it is
  rebuilt.
- **The backing path is relative**, so moving the store or importing an export
  keeps every variant attached to its base.

## Consequences

- The shared install is stored once and built once. A variant costs only its
  delta, and a registry deduplicates the base's chunks across variants (§6.4).
- A clone of a variant is a three-file chain. Removing a version removes the
  base and every variant together. The in-use check that guards removal looks at
  each disk, so a variant backing a clone cannot be removed, and neither can its
  base.
- The base is now a disk that exists in the store but that a lab may not use.
  That is enforced at resolution with `#base`, and by refusing to remove the
  last variant or the default one, either of which would let a bare reference
  fall through to the unsealed base.
- Rebuilding the base makes its variants stale rather than silently backing old
  deltas onto a new base, which would corrupt the guest. The cost is that a base
  rebuild means rebuilding every variant.
- `template.wcl` stays the one record of a version, so writing a variant is a
  rewrite of that file under the store lock. A variant counts as installed only
  when its block and its overlay both exist.
