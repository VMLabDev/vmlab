# What still uses QEMU after the x86 removal

Card #6 (VML-45) removes QEMU as a hypervisor for x86 and x86_64 guests
(PRD §22.4, HV-D29, MEM-40). This document lists what still uses QEMU on
the branch `vml-45`, and what the removal leaves without a hypervisor until
vmlab-hv runs it. Proof #2 (vfio passthrough) is done. The operator decided on
2026-11-23 to merge without waiting for #28 (macOS on vmlab-hv) and #82
(legacy templates that need installer media). Card #28 has since admitted
macOS guests on vmlab-hv. Those templates do not run until their card is done.

## What a user must change

- Remove `hypervisor = "qemu"` from every x86 machine, or set it to
  `"vmlab"`. Validation refuses it and names the fix.
- Remove `hypervisor { vm = "qemu" }` and `{ container = "qemu" }` from the
  host config on an x86_64 host, or set them to `"vmlab"`. The config still
  loads, but validation refuses each x86 machine the default reaches.
- Remove `qemu_args` from every x86 VM, and declare the hardware with the vm
  block's own keys or a profile.
- Remove `accel = "tcg"` from every x86 VM, and give the user read/write
  access to `/dev/kvm`.
- A host `gpu` block's `qemu_binary` and `qemu_env` need no change. The
  config loads, and a VM on vmlab-hv that picks that GPU ignores both with a
  warning that names the field (`src/qemu/hostgpu.rs`, card #447). They can
  be removed where no aarch64 or riscv64 VM uses the GPU.
- An existing x86 machine needs no change: its QEMU identity moves to
  vmlab-hv at the next start (§22.3). An online QEMU snapshot must be
  converted to an offline one under the old release, or taken again.

## Other architectures

- aarch64 VMs run on vmlab-hv's emulated `virt` machine (§22.5, phase 3d),
  and an existing QEMU aarch64 identity moves there at the next start. QEMU
  runs an aarch64 VM only when a setting names it (troubleshooting) or when
  vmlab-hv refuses the VM (a `gpu {}`, `usb = true`, a TPM, `audio`,
  `qemu_args`, more than eight vCPUs), with the reason in `status`.
- riscv64 VMs run on QEMU's `virt` machine until phase 3b.
  `src/qemu/cmdline.rs` composes only that argv now, and refuses an x86
  guest, `agent_transport = "isa-serial"` and a floppy before an argv exists.
  Automatic selection falls back to QEMU for riscv64, with a reason in
  `status`.
- Containers on a host that is not x86_64 run on QEMU
  (`src/qemu/container.rs`). On an x86_64 host every container runs on
  vmlab-hv.
- The QEMU adapter (`src/labd/hypervisor/qemu.rs`), the QMP client
  (`src/qmp/`), the process supervision (`src/qemu/process.rs`) and the
  TCG fallback (`plan_accel`) serve those guests.
- The AAVMF and riscv64 firmware lookup in `src/qemu/firmware.rs` serves
  them, and the AAVMF lookup serves vmlab-hv's aarch64 machine too. The x86 OVMF and SeaBIOS lookups in the same file serve vmlab-hv.
- A host GPU's `qemu_binary`, its `qemu_env` and the Vulkan PCI filter apply
  to an aarch64 VM on QEMU. vmlab-hv reads the filter layer, and ignores
  `qemu_binary` and `qemu_env` with a warning each.

## Shared code that keeps a QEMU name

- `qemu-img` stays an image tool for every machine, selected with
  `VMLAB_IMAGE_TOOL=qemu-img` (`src/template/qimg`).
- `src/qemu/resolve.rs` is the hardware resolution for both hypervisors.
  `ResolvedVm` keeps the q35 and pc machine names, which vmlab-hv's
  `describe.rs` reads.
- `VmPaths` in `src/qemu/cmdline.rs` is the launch input of both adapters.
  Its SeaBIOS, splash, firmware-resolution and boot-hint fields are read
  by vmlab-hv only.
- `swtpm`, the host's `virtiofsd` for container volumes, and the TSC pin
  file (`src/qemu/tsc.rs`, now only the pin) serve vmlab-hv.
- The launch-log redaction of an Apple SMC key in `src/qemu/process.rs`
  stays. No QEMU argv carries the key any more.

## Guests that vmlab-hv does not run yet

These ran on QEMU before the removal. On this branch each is an error,
because QEMU runs no x86 guest and vmlab-hv refuses them:

- `accel = "tcg"` on an x86 guest, and every x86 guest on a host without
  usable KVM or on a host that is not x86_64: phase 3 has not started.
- `input = "usb"`, a `cpu_model` on q35, a floppy off the pc machine, an
  `isa-serial` agent off pc, a pinned `tsc` or a virtio-net NIC on pc, and a
  display or NIC model that vmlab-hv does not present. The legacy pc itself
  (phase 4a to 4d: i440fx, IDE, ISA serial, e1000, PCnet, floppy, UHCI,
  Cirrus) runs on vmlab-hv.
- Legacy templates that need installer media to rebuild: proof #82. See
  `qemu-removal-legacy-template-matrix.md` and
  `qemu-removal-template-matrix.md` for the per-template state.
- vfio passthrough runs on vmlab-hv. Its live check, proof #2, is done.

## Lab files and scripts in this repository

- `guest/vdisplay-windows/build-vm/vmlab.wcl` set `hypervisor = "qemu"` for
  its Windows 11 build VM. The branch removes the line, so the VM runs on
  vmlab-hv. The build has not been run on vmlab-hv.
- `guest/win3d/build/signing/load-test.sh` writes `qemu_args` (an
  `egl-headless` display) into an x86 Windows lab unless the display is
  `virtio-vga`. That lab now fails validation. It needs a vmlab-hv form of
  the render-node choice.
- `hv/scripts/hwref/capture.py` runs `/usr/bin/qemu-system-x86_64` directly,
  not through vmlab, to capture the reference hardware. The captures under
  `hv/docs/hwref/` stay the reference for vmlab-hv's device placement.
- `e2e/soak_q35.py` compares one template on both hypervisors. Its QEMU
  half no longer starts for an x86 template.

## Tests

- The lifecycle tests in `src/labd/lifecycle_tests.rs` drive x86 VMs on the
  fake adapter with `HypervisorKind::Qemu`, the default kind of the test
  constructor. That state cannot be reached through the lab builder now,
  which refuses it. The tests still cover the lifecycle.
- `src/qemu/hostgpu.rs` and `src/qemu/process.rs` tests name
  `qemu-system-x86_64` as a fixture binary. They test path handling, not an
  x86 launch.
- The `describe.rs` legacy pc tests (`*_has_the_pc_shape`) compared the
  description with QEMU's argv. They now check the description alone.
- The e2e scenarios keep their `h.hypervisor == "qemu"` branches (`core.py`,
  `unsupported.py`, `fastpath.py`). A QEMU run no longer reaches them:
  `--hypervisor qemu` runs `arch.emulated` alone. `e2e/Dockerfile` still
  installs `qemu-system-x86` and the old `virtiofsd` 1.10.0, which only the
  removed `*.old-virtiofsd` checks used.
