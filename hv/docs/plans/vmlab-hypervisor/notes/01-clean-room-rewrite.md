---
id: "01"
decision: D1
---

# Rewrite as a new clean-room hypervisor rather than port QEMU

## Question
Strip QEMU and keep it in C (O1), port the stripped fork to Rust (O2), or write a new Rust hypervisor (O3)?

## Answer
O3: a new clean-room Rust hypervisor, MIT licensed, in its own repo `vmlab-hypervisor` at `/home/wil/dev/vmlab-hypervisor`.

## Why
A stripped QEMU still compiles about 700k lines of C (`docs/research/qemu-viability/01-minimal-build.md`). A Rust port of that is estimated at 180k-260k lines without TCG (`02-subsystem-porting.md`), stays GPLv2, and stays bound to QEMU's vmstate stream, QMP and qcow2-internal snapshots, so it pays a rewrite's cost without a rewrite's freedom. A clean-room VMM can reuse permissively licensed prior art (`03-rust-prior-art.md`) and own its control plane, snapshot format and frame path.

## Consequences
- No QEMU source may be copied or translated; QEMU is usable only as a black-box oracle and as a behavioural reference.
- Existing qcow2-internal snapshots will not carry over unless a converter is built.
- The six gaps no Rust project covers (legacy PC platform, TCG, qcow2 internal snapshots, QMP, QEMU-flavoured VNC, SeaBIOS boot) are this project's to fill or design away.
- The `qemu_args` escape hatch has no equivalent on the new backend.
