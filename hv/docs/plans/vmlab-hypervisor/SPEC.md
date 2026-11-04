---
slug: vmlab-hypervisor
kind: spec
created: 2026-10-30
---

# vmlab-hypervisor: implementation specification

Traces: every decision cited as `Dn` is a row in `MAP.md`; reasoning lives in
`notes/`. Grounding facts cite the vmlab repo (`/home/wil/dev/vmlab`).

## Problem

vmlab drives QEMU as a child process and pays for it in three places: QEMU's
control plane (QMP, its job model and event shapes) leaks above vmlab's
Hypervisor seam; snapshots are bound to QEMU's vmstate stream inside qcow2;
and the display path rides RFB plus out-of-tree egl-headless patches. A
stripped QEMU still compiles about 700k lines of C, and a Rust port of it would
stay GPLv2 and stay bound to the same formats (`docs/research/qemu-viability/`).
vmlab needs a hypervisor whose control plane, snapshot format and frame path it
owns, without losing any guest it supports today (D1).

## Solution

`vmlab-hv`, a clean-room MIT-licensed Rust hypervisor in its own repo
(`vmlab-hypervisor`, a cargo workspace), built on rust-vmm crates (D1, D7,
D18). vmlab spawns one `vmlab-hv` process per machine (D5) and selects it per
machine with `hypervisor = "vmlab"`, defaulting to QEMU from host config until
each phase passes its gate (D2, D11, D27, D29). The hypervisor speaks a typed
control protocol (D6), keeps qcow2 behind a disk-format seam (D8), stores its
own versioned device state in qcow2 internal snapshots (D14, D32), presents
QEMU-identical guest-visible hardware for templated VMs (D30), and is built in
four phases: container micro-VMs, modern x86_64 VMs, TCG guests, legacy PC
guests (D4). Hosts are x86_64 Linux only (D3).

This spec details phase 1 to build level and fixes the contracts later phases
must honour. vmlab-side changes (seam refactor, selector, PRD amendment) are a
separate vmlab PR (Q37) and are specified here only as the interface they meet.

## User stories

1. As a vmlab user, I set `hypervisor = "vmlab"` on a container and `vmlab up` boots it to agent-ready on `vmlab-hv` with no other change to my lab file.
2. As a vmlab user, my `vmlab-hv` container has the same networking, volumes (virtiofs), exec, shell, logs and stop behaviour as under QEMU.
3. As a vmlab user, I take, list, restore and delete online and offline snapshots of a `vmlab-hv` container, and a restore resumes where the snapshot left off.
4. As a vmlab user, when I switch a machine's hypervisor, my offline snapshots still restore and my online snapshots refuse with a message naming the hypervisor that took them (D31).
5. As a vmlab user, a machine that sets `qemu_args` with `hypervisor = "vmlab"` fails validation naming the argument (D16).
6. As a vmlab user, a machine whose profile or features `vmlab-hv` cannot yet run fails validation naming what is missing (D11).
7. As the owner, I can see per phase whether `vmlab-hv` passes the parity gate (e2e suite plus differential suite plus performance bars) before I flip a default (D13, D28, D29).
8. As a vmlab user (phase 2), my existing QEMU-built Linux and Windows templates boot unchanged on `vmlab-hv`, with display, screenshots, recording, input, TPM, secure boot, GPU, USB and vfio (D17, D21, D23, D24, D25, D30).
9. As a vmlab user (phase 3), aarch64 and riscv64 guests run on my x86_64 host under `vmlab-hv`'s own TCG (D22, D34).
10. As a vmlab user (phase 4), my DOS, 9x, XP and TempleOS templates boot unchanged on `vmlab-hv` (D23, D30).
11. As a developer, I can exercise `vmlab-hv` end to end without vmlab, through its control socket (S1).

## Implementation decisions

### Repository and crates (D18)

Cargo workspace at `/home/wil/dev/vmlab-hypervisor`, MIT licence, edition 2024,
stable toolchain. Crates:

- `vmlab-hv` (binary, D27): process entry, argument parsing, wiring.
- `vmlab-hv-proto`: the control protocol types and codec. vmlab depends on it as a git dependency.
- `vmlab-disk`: the disk-format seam and the qcow2 implementation, plus a small debugging CLI (D8, D15). vmlab depends on it as a git dependency.
- `vmlab-hv-vmm`: KVM VM, guest memory, vCPU threads, machine models, the event loop, snapshot orchestration.
- `vmlab-hv-devices`: device models (serial, virtio transport and devices, ACPI pieces), each with unit tests on its register or virtqueue interface.
- `vmlab-hv-tests` (or `tests/` at the workspace root): the S1 integration harness.

Dependency rule: nothing in the workspace depends on the `vmlab` crate (D18).
The S1 harness may take vmlab's wire crates `vmlab-agent-proto` and
`vmlab-cinit-proto` as git dev-dependencies to drive the guest; they are wire
contracts, not vmlab itself.

Licence policy (accepted unknown under D7, written before the first lift):
code lifted from crosvm keeps its BSD-3 header; Sail-derived generated code
keeps BSD-2 or BSD-3-Clause-Clear headers (D34); every such file is listed in
`THIRD_PARTY_NOTICES`. Crates licensed Apache-2.0 alone or Apache-2.0 AND another licence (`kvm-bindings`, `acpi_tables`,
`linux-loader`, `virtio-queue`, `vhost-user-backend`) are dependencies only,
never copied. No QEMU source is copied or translated; QEMU is used as a
black-box oracle and behavioural reference only (D1).

### Process and launch contract (D5, D19)

vmlab execs `vmlab-hv` once per machine. The launch carries a typed machine
description (a `vmlab-hv-proto` type, serialised as JSON) by path:

```
vmlab-hv --machine <machine.json> --control <control.sock> [--paused]
```

The machine description names: arch, accelerator, vCPU count, memory, boot
(direct kernel: kernel, initrd, cmdline; later firmware), disks (role, format,
path, read-only), NICs (MAC, MTU, attachment), serial log path, named
virtio-serial ports with their socket paths, vhost-user-fs shares (socket,
tag), and an optional RTC base. `--paused` is the `-S` equivalent: the machine
is built but vCPUs do not run until a `resume` command (vmlab needs this for
recording from the first frame).

The process binds every socket it serves itself (control, agent and ctl ports)
before reporting ready (D10). Exit status: 0 on guest-initiated or requested
clean shutdown, non-zero with a message on stderr otherwise; vmlab's
`classify_exit` reads the printed `std::process::ExitStatus` shape.

### Control protocol (D6, `vmlab-hv-proto`)

JSON-lines over the unix control socket, following vmlab's typed-vocabulary
style (ADR-0007): each request carries an id and a typed command; each reply
carries the id and a typed result or a typed error code; events are unsolicited
lines with no id.

- First exchange is a version handshake: the client sends its protocol version, the server answers with its own and its capabilities; a mismatch is an error before any other command (accepted unknown under D18).
- Commands, phase 1: `status`, `resume`, `pause`, `powerdown` (ACPI power button), `quit`, `snapshot_save {name}`, `snapshot_load {name}`, `snapshot_delete {name}`. Snapshot commands complete when done (no job polling, D6).
- Events, phase 1: `shutdown {reason: guest_shutdown | guest_reset | host_request | guest_panic}`, `reset`, `stopped`, `resumed`, `suspend`, `wakeup`.
- Capabilities advertised in the handshake so vmlab's capability traits (D19) know what to offer: phase 1 advertises `snapshots`, and not `display`, `input` or `hotplug`.
- Later phases add: `send_keys`, `input_event`, `frame_*` (D21), `device_add`/`device_del` for USB (D25), `wakeup`.

### Phase 1 machine (D4)

x86_64 under KVM only (containers are always native; TCG containers stay on
QEMU until phase 3). The container machine is a minimal x86_64 platform
sufficient for vmlab's own container kernel (`guest/dist/x86_64/vmlinuz`, the
pinned Alpine linux-virt build): it is not a templated guest, so D30's
QEMU-identical requirement binds VM machine models from phase 2 onward, not
this one.

- Boot: direct bzImage + initrd load with the cmdline vmlab passes today (`console=ttyS0 quiet loglevel=3 panic=-1 reboot=t vmlab.root=/dev/vda vmlab.scratch=/dev/vdb`).
- Platform: PCI root bus with virtio-pci (modern) transport, 16550 UART on COM1 written to the serial log file (container stdout/stderr is the serial log, PRD §18), ACPI tables with a power button and S5 so `powerdown` and guest poweroff work, and reset via the kernel's `reboot=t` triple fault reported as `guest_reset`.
- Devices, in vmlab's order: virtio-blk rootfs (raw, read-only, squashfs) as `/dev/vda`; virtio-blk scratch (qcow2 through `vmlab-disk`) as `/dev/vdb`; virtio-console multiport with ports `vmlab.ctl.0` and `vmlab.agent.0`; one virtio-net per NIC; vhost-user-fs per volume with guest RAM on a shared memfd.
- Network (D9): each NIC connects as a client to vmlab's switch socket and exchanges frames as a 4-byte big-endian length prefix plus one Ethernet frame, cap 64 KiB plus 14 bytes, as `src/net/framing.rs` defines.
- virtio-serial port semantics (D10): each port is a unix socket the hypervisor listens on, matching QEMU's `server=on,wait=off`; the guest sees the port open when a host client is connected; a client may disconnect and reconnect, and the agent's magic-resync framing tolerates replayed or dropped bytes after a restore.
- virtiofs (D33): external virtiofsd over vhost-user in phase 1. virtiofsd's session state rides our snapshots through vhost-user device-state transfer (`SET_DEVICE_STATE_FD`, `CHECK_DEVICE_STATE`; virtiofsd run with `--migration-mode find-paths --migration-verify-handles` when it supports them), stored as the virtio-fs device's section, so share-bearing machines snapshot online as they do under QEMU today (owner decision 2026-10-30, Q44).

### Snapshots (D14, D31, D32)

Snapshots are an operation of the disk-format seam. For qcow2 the hypervisor:

1. pauses vCPUs and quiesces devices,
2. writes a qcow2 internal snapshot on every writable disk,
3. writes RAM and device state into the vmstate area of the designated disk's snapshot (containers: the scratch disk, as vmlab does today),
4. resumes, unless the snapshot was taken paused.

Device state is the hypervisor's own format: a header with magic, the
device-state major and minor version, the `vmlab-hv` version and the machine
description digest, followed by one section per device keyed by a stable
device id, then RAM. Restore refuses a different major version with a message
naming both versions; before 1.0 compatibility is best effort (D32). Offline
(disk-only) snapshots are plain qcow2 internal snapshots and stay portable to
QEMU (D31). vmlab records which hypervisor took each snapshot; that is a vmlab
PR concern.

### Disk crate (`vmlab-disk`, D8, D15)

A `DiskFormat` trait owns open, read, write, flush, discard, size, backing
chain, and snapshot create/apply/delete/list including a vmstate area. The qcow2
implementation must read and write images `qemu-img` produces and produce
images `qemu-img check` accepts, including backing files with relative and
absolute paths, compressed clusters (read), and the internal snapshot table.
The crate also exposes the image operations vmlab uses through `qemu-img`
today: create, linked clone, rebase (safe and unsafe), info as JSON, resize,
convert (including from a named snapshot with a backing file), snapshot
create/apply/delete, and commit. A thin `vmlab-disk` CLI exposes them for
debugging. vmlab keeps calling `qemu-img` until this crate reaches parity on
vmlab's own freeze and template tests.

### vmlab-side interface (D11, D16, D19, D20; separate PR)

Specified so the hypervisor can be built against it; implemented in vmlab by a
separate PR:

- The Hypervisor seam gains capability traits (snapshots, input, frames, hot-plug) and a typed launch description; each adapter composes its own launch; the QEMU adapter keeps calling the pure argv builder and translates capabilities to QMP. An ADR revises ADR-0001's argv rule.
- A per-machine `hypervisor` field (`"qemu"` | `"vmlab"`) with a host-config default keyed by machine kind or profile, starting at QEMU.
- Validation refuses `qemu_args`, and any feature `vmlab-hv` does not advertise, on a `hypervisor = "vmlab"` machine.
- `required_binaries` and the orphan sweep learn `vmlab-hv`; snapshots record their hypervisor and refuse a cross-hypervisor online restore.
- PRD §1.2 is amended with an ADR when the selector lands.

### Later phases: contracts fixed now

- Phase 2, modern x86_64 VMs: q35 machine model with QEMU-identical guest-visible hardware (PCI IDs and slots, ACPI, SMBIOS, fw_cfg, cfi pflash layout) so existing templates and OVMF_VARS files keep working (D30, D23); SeaBIOS and OVMF binaries reused unchanged; SMM for secure boot; Hyper-V enlightenments vmlab passes today; AHCI/IDE and VGA as profiles need; tpm-tis over swtpm's socket (D17); native shared-memory frame protocol plus a small RFB server on `vnc.sock` (D21), including NVIDIA external-only dma-buf scanout; crosvm's virtio-gpu over rutabaga for virgl and Venus (D24); in-process virtio-fs replacing virtiofsd, with snapshot state in the device-state format (D33); xhci, usb-host and vfio at the end of the phase (D25).
- Phase 3, TCG: riscv64 then aarch64 interpreters generated by our own Rust emitter from sail-riscv and sail-arm, with a lockstep harness against Sail and QEMU and deterministic replay, then a Cranelift JIT behind the performance bar (D22, D34). No tooling extracts ASL from Arm's XML.
- Phase 4, legacy PC: i440fx/PIIX, IDE, floppy, cirrus, pcnet, e1000, isa-serial agent on COM1, all QEMU-identical (D30, D10).
- Any proposed break of D30 goes to the owner with its reason and the affected templates.

## Testing decisions

- **S1, `vmlab-hv` control socket (new, primary).** The harness spawns `vmlab-hv` with a machine description and drives it over the control protocol, observing the guest through the agent socket (agent `Hello` handshake, exec), the ctl socket (cinit `boot`, `Spec`, `started`/`idle`, `exited`) and the serial log. Phase 1 acceptance: boot vmlab's x86_64 container guest asset with a squashfs rootfs built from an Alpine minirootfs to cinit `idle` and agent ready; exec a command and read its output; snapshot save, mutate, load and observe the rollback; `powerdown` and guest poweroff both exit 0; `quit` exits; a missing kernel or socket path fails with a clear error. Needs `/dev/kvm`; tests skip with a named reason without it.
- **S2, `vmlab-disk` against `qemu-img` (new).** Round-trip tests: images written by the crate pass `qemu-img check` and `qemu-img compare` against the expected content; images and snapshots written by `qemu-img` read back identically through the crate, including backing chains and internal snapshots with vmstate. `qemu-img` is a test-only dependency.
- **Device unit tests.** Each device is tested at its guest-facing interface (registers, virtqueues) with in-memory guest memory, below S1, not instead of it.
- **S3, vmlab `Machine` surface and fake (existing, vmlab PR).** Covers the seam refactor, selector and validation refusals.
- **S4, vmlab e2e suite parameterised by hypervisor (existing plus a parameter, vmlab PR).** The parity gate per phase (D13), together with the differential suite (D26) and the performance bars (D28): boot to agent-ready no slower than QEMU, virtio-blk and virtio-net throughput at least 80% of QEMU's, TCG at least 50% of QEMU's TCG.
- **Merge bar in this repo:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (S2 and unit tests always; S1 when `/dev/kvm` is present).

## Out of scope

- ARM Linux hosts and x86 guest emulation (D3).
- A purpose-built disk format replacing qcow2, and merging this repo into vmlab (owner-deferred, D8, D18).
- Sandboxing; nothing may block per-process seccomp and Landlock later (D12).
- Converting QEMU-taken online snapshots (D31).
- vmlab's SMB server work and PRD §7.5's VioFS correction (vmlab tasks, `notes/14`).
- A passthrough for arbitrary hypervisor arguments (D16).

## Open questions

- Whether `vmlab-disk`'s qcow2 implementation extends imago (MIT; read-write and backing chains, no internal snapshot writes) or is written from scratch. D8 allowed either. The deciding factor is how cleanly imago takes snapshot-table and vmstate writes; the first disk milestone should answer it with a spike.
- Arm's position on MIT redistribution of sail-arm-derived code, before any public aarch64 release (owner task, `notes/32`).
- How vmlab learns a template's virtio-win driver version before Windows virtiofs is re-enabled (D33; a vmlab task).
