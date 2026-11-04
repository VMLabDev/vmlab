---
slug: phase-2-modern-vms
kind: spec
created: 2026-10-30
---

# Phase 2 of vmlab-hv: modern x86_64 VMs — implementation specification

Traces: `Dn` is a row in this plan's `MAP.md`. `HV-Dn` is in
`../vmlab-hypervisor/MAP.md`. Research is in `notes/01-03`. Hardware references
are in `docs/hwref/`.

## Problem

`vmlab-hv` runs only container micro-VMs. vmlab's modern x86_64 VM profiles
(`windows-10`, `windows-11`, `windows-server`, `linux-modern`) and the templates
built for them under QEMU cannot run on it. Doing so means:

- a q35 platform that presents QEMU-identical guest-visible hardware (HV-D30)
- unmodified SeaBIOS and OVMF firmware (HV-D23)
- secure boot and TPM
- a display that vmlab can screenshot, OCR, record and drive
- GPU acceleration, USB, shares, snapshots and audio

## Solution

Six sub-phases, each ending at its own acceptance (D3):

| Sub-phase | Delivers |
|---|---|
| 2a | The q35 platform and SeaBIOS boot of a Linux template |
| 2b | The native frame protocol, an RFB server and input |
| 2c | OVMF, pflash, SMM, secure boot, TPM, Hyper-V and Windows templates |
| 2d | GPU (virgl, Venus) |
| 2e | In-process virtio-fs and audio wiring |
| 2f | USB host passthrough and vfio |

Hardware identity is `pc-q35-11.1` as captured (D4), with one approved
difference: USB (D6).

vmlab gains a native frame client for `vmlab-hv` machines (D8). Its e2e suite
is the gate for flipping the host default for VMs (D1, D13).

## User stories

1. As a vmlab user, my existing Debian or Ubuntu template boots on `vmlab-hv` with `hypervisor = "vmlab"` and reaches agent ready, with no change to the template (2a).
2. As a vmlab user, screenshots, OCR, image matching, `send_keys`, `type_text` and mouse actions work on a `vmlab-hv` VM (2b).
3. As a vmlab user, `vmlab record` captures from the first firmware frame, with cursor and click ripples, and survives `vm restart` and snapshot restore (2b).
4. As a vmlab user, `vmlab console` and `gui = true` open a VNC viewer on a `vmlab-hv` VM (2b).
5. As a vmlab user, my existing Windows 10, Windows 11 and Windows Server templates boot on `vmlab-hv` under OVMF, including secure boot with my existing OVMF_VARS and TPM state, and reach agent ready (2c).
6. As a vmlab user, `gpu = "virgl"` or `"vulkan"` works on a `vmlab-hv` VM, and screenshots work on its GL scanout (2d).
7. As a vmlab user, virtiofs shares and guest audio work on a `vmlab-hv` VM without an external `virtiofsd` (2e).
8. As a vmlab user, `vmlab usb attach` and GPU passthrough work on a `vmlab-hv` VM (2f).
9. As a vmlab user, online snapshots of a `vmlab-hv` VM work, except that they are refused while a paravirtual GPU, a vfio device or a USB device is attached, as under QEMU (D9).
10. As the owner, I can see the hwref differential and the e2e results per sub-phase before I flip the VM default (D12, D13).

## Implementation decisions

### Machine description (`vmlab-hv-proto`)

`Boot` gains a `firmware` variant:

- `kind`: `seabios` or `ovmf`.
- `code`: the firmware image path.
- `vars`: OVMF only. The writable VARS path, which vmlab owns and preserves.
- `secure_boot`: bool.
- `bootorder`: firmware device paths, so the platform can write the fw_cfg `bootorder` file.

A `machine` field selects `microvm` (phase 1's platform, the default) or `q35`.

Every device carries an explicit PCI address on q35, so placement is the
caller's choice and matches the captures. The device list grows to cover:

- disks by bus (`virtio`, `ahci`) and CD-ROMs on AHCI
- the display device (`std-vga`, `virtio-vga`)
- input (the xHCI tablet; an optional USB keyboard follows the `input = "usb"` profiles in a later phase)
- TPM
- Hyper-V enlightenments
- CPU policy: `host` with `vmx`/`svm` masking, and `nested`

The audio section already exists. The wire shape is pinned by tests. vmlab's
resolver produces all of this in a later vmlab PR.

### 2a — q35 platform and SeaBIOS

Each piece is built from public specs plus the observed captures (clean room,
`AGENTS.md`).

**PCI root.**
- Explicit bus 0 device and function placement, multifunction headers and 4 KiB extended config through ECAM.
- INTx routing through ICH9 PIRQ A–H with the swizzle the captured `_PRT` shows.
- Firmware-driven BAR allocation (firmware sizes and places BARs; the VMM stops pre-assigning on q35).

**Q35 MCH (00:0.0, 8086:29c0).**
- Config space as captured.
- PCIEXBAR controls the ECAM window: SeaBIOS uses 0xb0000000, OVMF 0xe0000000.
- PAM registers shadow the BIOS ROM.
- The RAM-bound registers are implemented.
- The SMRAM and TSEG registers exist from 2a but enforce only from 2c.

**ICH9 LPC (00:1f.0, 8086:2918).**
- PIRQ routing registers and PMBASE.
- The ACPI PM block: event, control, timer and GPE0, with the SCI on GSI 9.
- ACPI enable and disable through the APM ports 0xb2/0xb3.
- Reset through 0xcf9.
- S3/S4/S5 sleep types with the values the captured DSDT declares.

Phase 1's PM device is superseded on q35.

**Other platform devices.**
- **RTC/CMOS:** time, alarm, update interrupt, NVRAM, the memory-size bytes OVMF and SeaBIOS read, and `rtc_base`.
- **PS/2 i8042:** keyboard and mouse, IRQ 1 and 12, A20 and reset.
- **HPET:** at 0xfed00000.
- **Minor ISA:** DMA registers, port 0x61 and LPT1, at least to the level guests probe.
- **SMBus:** at 00:1f.3 with no slaves.
- **AHCI:** at 00:1f.2 (8086:2922), with SATA ports carrying ATA disks and ATAPI CD-ROMs. vmlab attaches CD-ROMs there on q35.

**fw_cfg** (I/O 0x510–0x51b, DMA at 0x514). Covers the signature, feature,
directory and numeric keys, and DMA read, write and skip. It serves the files
the captures list:
- `etc/acpi/rsdp` and `etc/acpi/tables`
- `etc/table-loader`
- `etc/e820`
- `etc/smbios/smbios-anchor` and `etc/smbios/smbios-tables`
- `bootorder` and `etc/boot-fail-wait`
- `etc/system-states`
- `etc/smi/*` (from 2c)
- `etc/tpm/*` (from 2c)
- `genroms/kvmvapic.bin` when it is available

**ACPI and SMBIOS.** The tables and AML namespace reproduce the captures
(APIC, DSDT, FACP, FACS, HPET, MCFG, WAET, and TPM2 from 2c), with OEM `BOCHS`
and table ID `BXPC`. They are delivered through the table-loader script
(ALLOCATE, ADD_POINTER, ADD_CHECKSUM, WRITE_POINTER), so the firmware installs
them. SMBIOS 2.8 carries the captured strings. On q35, phase 1's direct-install
builder stays only for the microvm machine.

**Firmware boot.**
- The reset vector at 0xfffffff0, with the BIOS image mapped below 4 GiB and its low alias at 0xe0000–0xfffff under PAM control.
- Option ROMs (the VGA BIOS for `std-vga`) through PCI ROM BARs.
- Firmware images come from the host: vmlab's bundled edk2, and SeaBIOS and SeaVGABIOS from the distro's `seabios` package.

**Transitional virtio (D5).** VM machines present virtio-blk, net, serial and
fs with the captured shape:
- legacy (0.9.5) I/O registers in BAR0
- MSI-X in BAR1
- the modern transport in BAR4
- IDs 1af4:1000–1003, revision 0, the captured subsystem IDs

Containers keep phase 1's modern-only transport. The choice is per machine.

**Std VGA (1234:1111 rev 2).** VGA registers, the legacy 0xa0000 window, Bochs
VBE (DISPI) and a linear framebuffer BAR. It is the frame source in 2a and 2b.

**xHCI with an HID tablet (D6).** The controller is qemu-xhci-compatible
(1b36:000d, subsystem 1af4:1100, 64-bit BAR0), with one absolute-pointer USB
HID tablet. There is no ICH9 UHCI/EHCI. It may lift crosvm's xHCI core under
HV-D7. Host passthrough builds on it in 2f.

**CPU.**
- CPUID is the host's, filtered as vmlab's QEMU path does: `vmx`/`svm` masked unless `nested`, topology matching vCPUs.
- Hyper-V CPUID leaves and KVM's Hyper-V capabilities for the enlightenment list vmlab passes, enabled in 2c.

**Approved differences (D6):** no 00:1d.x functions, plus an xHCI the captures
lack. These are listed in the differential tool's allow-list.

**2a acceptance:**
- SeaBIOS boots the local Debian 13 and Ubuntu 24.04 templates to agent ready.
- The hwref differential against `q35-seabios-linux-generic` shows only the allow-listed differences.
- Online snapshot round trip on the VM.
- `just ci::check`.

### 2b — frame protocol, RFB and input (D7, D8)

Each machine has a display socket `display.sock`.

**Messages.** Each message is a little-endian header, `{u32 len, u16 kind, u16
flags}`, followed by its payload. File descriptors travel with a message as
`SCM_RIGHTS` ancillary data.

**Version handshake.** The client sends `Hello {version, capabilities}`, where
the capabilities are `frames`, `cursor`, `input`, `dmabuf_ok` and `cpu_copy`.
The server answers `Hello {version, capabilities}`.

**Surfaces.**
- `SurfaceConfigure {surface_id, width, height, drm_fourcc, stride, modifier, kind: shm|dmabuf, buffers: N}` comes with N fds.
- For `shm` the fds are memfds; the server ring-buffers at least 3.
- A configure supersedes the previous one. That covers DesktopSize, reconnect and reset.

**Frames.**
- `FrameReady {surface_id, buffer, seq, timestamp_ns, damage: [rect]}` may carry an acquire `sync_file` fd for dma-buf.
- The client returns `Release {surface_id, buffer, seq}`.
- The server never reuses a buffer a client still holds. A slow client sees frames skipped (latest wins), and the guest is never stalled.
- A newly connected or reconfigured client's first frame is full.

**Cursor.** `CursorShape {w, h, hot_x, hot_y, premultiplied ARGB}` and
`CursorState {x, y, visible}`.

**CPU readback.** `CpuCopy {surface_id}` asks the server to read a dma-buf
scanout back into shm. That is the path for NVIDIA external-only buffers and
for consumers that want pixels.

**Input.**
- `Key {qcode, down}`, using the QEMU qcode names vmlab already uses.
- `PointerAbs {x, y, width, height}`, `Buttons {mask}`, `Scroll {dx, dy}`.
- `LedState` is sent server to client.
- Keys go to the PS/2 keyboard and the pointer to the xHCI tablet.

**Other details.**
- `RequestSize {width, height}` is reserved for the viewer, which arrives with virtio-gpu.
- Multiple clients are independent.
- The crate documents exact struct layouts and pins them with tests.

**Client crate.** A new crate, `vmlab-hv-display`, holds the protocol types, a
client library and a server core. vmlab and the tests depend on it.

**RFB server** on `vnc.sock`. RFB 3.8 with no authentication:
- encodings Raw, DesktopSize, RichCursor and AlphaCursor
- the LED-state pseudo-encoding
- key and pointer events mapped onto the same input path

It serves human viewers. The recorder moves to the native protocol (D8).

**vmlab side (vmlab PR).**
- `Frames` gains streaming.
- `Input` uses the native protocol.
- The recorder, screenshot and vision code consume the native client.
- `vnc.sock` stays for `vmlab console`.

**2b acceptance:**
- The client receives the SeaBIOS frame and then the Linux console, with damage rectangles, the cursor and a resize.
- Input typed through the protocol is read back through the agent.
- A third-party VNC client connects to `vnc.sock`.
- vmlab's vision and record e2e features pass on `vmlab-hv`.

### 2c — OVMF, secure boot, TPM, Windows

- **cfi.pflash01:** with OVMF's command set (program, erase, status, ID, array), CODE read-only and VARS persistent, at the 4 MiB layout.
- **SMM:** KVM SMM with SMRAM and TSEG mapping and locking, APMC SMI delivery, the ICH9 SMI registers, and the `etc/smi` feature negotiation.
- **TPM:** tpm-tis at 0xfed40000 over swtpm's socket (HV-D17), plus the `etc/tpm` files and the TPM2 table.
- **Display and Hyper-V:** the `virtio-vga` 2D shape as captured (VGA-compatible BAR0 framebuffer) as the frame source, and Hyper-V enlightenments.

**2c acceptance:**
- The local Windows 10 and Windows Server 2022 templates boot to agent ready, keeping their existing VARS.
- The secure-boot variant boots with an enforcing key store.
- The hwref differential against the two OVMF captures shows only the allow-listed differences.

### 2d–2f — contracts fixed now

- **2d GPU:** crosvm's virtio-gpu protocol lifted, depending on `rutabaga_gfx` (pinned as crosvm pins it, virglrenderer >= 1.3). The VMM gets a seam for the blob aperture. Scanouts are exported as dma-buf into the frame protocol, with NVIDIA external-only buffers handled through `CpuCopy`. Host render-node selection, the Vulkan environment and the filter layer, and keep-awake all follow vmlab (D10). Online snapshots are refused (D9).
- **2e:**
  - **virtio-fs:** in process, built on crosvm's BSD `fuse` crate, with its state in the snapshot format (HV-D33).
  - **Audio:** HDA at 00:1b.0 (D11), wired to the existing host-audio seam per `../hv-audio/SPEC.md`.
- **2f:**
  - **USB:** host passthrough on the xHCI, using crosvm's `usb_util` and host backend with FIDO pruned.
  - **vfio:** through rust-vmm `vfio-ioctls` and `vfio-bindings`, with KVM crate versions aligned.
  - **Snapshots:** refused while either is attached (D9).
- **Host facts:** `vmlab-hv` gets a probe that reports KVM, TSC and invariant-TSC facts (D10).

## Testing decisions

- **P1, S1 in VM mode.** The `vmlab-hv-tests` harness boots real templates through firmware with a `q35` machine description: Debian 13 and Ubuntu 24.04 in 2a, Windows 10 and Server 2022 in 2c. Templates come from `~/.local/share/vmlab/templates/x86_64/` or an env override. Each test uses a fresh qcow2 overlay on the template, so templates are never written. It drives the guest through the control socket and the agent, and skips with a reason when a template or `/dev/kvm` is missing.
- **P2, the hwref differential.** `scripts/hwref` gains a `vmlab-hv` mode and a normaliser that masks timestamps and firmware allocation addresses and applies the allow-list. A diff between the QEMU and `vmlab-hv` captures of the same configuration is the platform gate.
- **P3, the display protocol.** Tests in `vmlab-hv-display` cover the wire layouts and the client/server core. S1-level tests receive real frames from a booting guest and round-trip input through the agent.
- **P4, device tests at the guest interface.** These cover AHCI, fw_cfg and the table-loader, pflash, RTC, PS/2, HPET, LPC/PM, MCH, the legacy virtio transport, VGA and xHCI.
- **P5, vmlab e2e per hypervisor (S4).** Each sub-phase marks its features applicable on `vmlab` and must pass them.
- **Merge bar:** `just ci::check`, with P1/P2 skipping cleanly without KVM or templates (GitHub CI has neither templates nor firmware).

## Out of scope

- The viewer app (separate map, `../hv-viewer/INTAKE.md`).
- macOS (its own phase) and `windows-legacy`/i440fx (phase 4) (D2).
- ICH9 UHCI/EHCI (D6).
- Online snapshots with a paravirtual GPU or vfio device (D9).

## Open questions

- Which SeaBIOS and SeaVGABIOS builds to ship or require. The captures used Arch's `seabios 1.17.0-2-2`; vmlab bundles edk2 but not SeaBIOS today.
- Whether the AML is reproduced byte-for-byte or semantically. The differential suite decides: aim for semantic equality of the disassembled namespace and byte equality where cheap.
