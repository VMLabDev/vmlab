# Release note: QEMU support for x86 guests is going away

This is the announcement for the release that ships the warning (PRD §22.2).
Put it at the top of that version's release notes. At least one minor release
ships between this one and the removal.

## QEMU support for x86 guests will be removed

A later vmlab release removes QEMU for x86 and x86_64 guests. They will run on
vmlab-hv only, and vmlab-hv needs KVM. Guests of other architectures stay on
QEMU. This release changes nothing about how a machine runs. It tells you
which machines the removal affects.

- `vmlab up`, `vmlab vm start`, `vmlab validate` and `vmlab status` print a
  warning for each x86 machine that runs on QEMU. The warning names the
  reason and the fix: an explicit `hypervisor = "qemu"`, a host config
  default, disks made under QEMU, `qemu_args`, or hardware vmlab-hv refuses.
- A host where `/dev/kvm` is not usable gets a warning that x86 guests will
  need KVM. The emulation (TCG) fallback goes away with QEMU.
- `vmlab status --json` lists the affected machines: each has a
  `qemu_removal` object with `reason`, `warning` and `online_snapshots`.
- Online snapshots taken under QEMU stay on disk, but after the removal
  restoring one by name is refused. There is no converter. To keep one,
  restore it, shut the machine down, and take an offline snapshot before you
  upgrade to the release that removes QEMU.

The manual's "QEMU or vmlab-hv" section (Host configuration) lists every
reason with its fix.
