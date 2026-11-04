# Phase 4 of vmlab-hv: legacy PC guests

## Problem

vmlab runs its legacy templates on QEMU's `pc` machine: i440fx with PIIX3, IDE,
an optional floppy, std VGA or Cirrus, e1000 or PCnet, a PIIX3 UHCI tablet, and
the legacy agent on COM1. The templates are Windows 7 and Vista, Windows 8 and
8.1, and the Server 2008, 2012 and 2016 families under `windows-legacy`, plus
TempleOS, FreeDOS, Windows 95 and Windows 98. vmlab-hv builds only `microvm`
and `q35`, so every one of these templates needs QEMU. QEMU cannot be removed
(HV-D29) until they run on vmlab-hv.

The gap is a pc machine model and its devices. The phase-2 seams carry it:
firmware loading, fw_cfg and the table-loader, KVM's irqchip and PIT, the
16550, VGA, AHCI's ATA and ATAPI logic, and the display, control and snapshot
protocols. Guest drivers expect the same hardware they were installed against
(HV-D30), so every device has to be QEMU-identical at the register level.

## Solution

vmlab-hv gains a `pc` machine: i440fx at 00:00.0, a PIIX3 ISA bridge with IDE
and UHCI at 00:01.0–2, and PIIX4 ACPI/PM at 00:01.3. It also gets pc-specific
ACPI, SMBIOS, CMOS and boot-order inputs, and the ISA platform the captures
show. The new devices are:
- IDE, with ATAPI and bus-master DMA.
- An 82078-style floppy controller on a working ISA DMA engine.
- Cirrus CL-GD5446.
- PCnet Am79C970A.
- e1000 82540EM.
- UHCI carrying the existing HID tablet.

The 16550 matches QEMU's immediate transmit and backpressure behavior (D5),
and A20 actually masks address bit 20 (D2).

Work lands in five sub-phases (D8):
1. **4a, machine core.** TempleOS and a network-less Windows 7 boot.
2. **4b, networking.** e1000 and PCnet.
3. **4c, floppy and USB.** The floppy controller with ISA DMA, and UHCI.
4. **4d, Cirrus.** Windows 98 SE's display.
5. **4e, vmlab side.** The pc machine description, the refusal list, input
   routing, the `legacy` e2e lab and the Windows 95 rebuilds. It may start in
   parallel with 4a.

Each sub-phase brings one more gate guest up. The gate guests are FreeDOS 1.3,
Windows 98 SE, Windows 7 7600.0.1 and TempleOS (D1).

## User stories

1. As a vmlab user, my Windows 7, Vista, 8, 8.1 and Server 2008–2016 templates
   boot unchanged on `hypervisor = "vmlab"`, with networking, the agent and
   snapshots working.
2. As a vmlab user, my TempleOS template boots, and the HolyC agent on COM1
   answers `exec`.
3. As a vmlab user, my FreeDOS and Windows 98 templates boot. The C legacy agent
   answers `exec` and `set_time` over COM1. Scripted keystrokes land in
   real-mode setup screens.
4. As a vmlab user, floppy attachments and floppy media built from a folder
   work on a pc machine.
5. As a vmlab user, my Windows 95 templates run on KVM after a rebuild, with no
   TCG anywhere (D7).
6. As the owner, I can remove QEMU after this phase, because no legacy template
   still needs it.

## Implementation decisions

### Machine description (`vmlab-hv-proto`, additive)

- `Machine` gains `Pc`.
- `DiskBus` gains `Ide`. A disk or CD on IDE carries a channel (primary or
  secondary) and a unit (master or slave). There are exactly four slots, and
  vmlab addresses them explicitly as it does for QEMU: disks from the front,
  then CD-ROMs.
- A `floppy` attachment carries a raw image. The drive type follows its size,
  1.44 or 2.88 MiB.
- `DisplayDevice` gains `CirrusVga`, so `StdVga` and `CirrusVga` are both
  available on pc.
- A NIC carries a `model`: `virtio`, `e1000` or `pcnet`. Absent means today's
  behaviour.
- Serial ports become selectable. COM1 can be the agent's socket (the
  `isa-serial` transport, HV-D10) and COM2 the serial log, as vmlab's QEMU
  argv does today. A machine with no legacy agent keeps the log on COM1.
- `cpu_model` is accepted for pc, because 9x needs `pentium3`. `rtc` gains
  `localtime`. `hyperv` stays valid on pc for `windows-legacy`.
- Validation refuses three things: IDE on q35, a fifth IDE device, and a floppy
  on anything other than pc.

### Machine model (`vmlab-hv-vmm`)

- **The `pc` builder sits beside `q35`.** The i440fx host bridge provides PAM
  shadowing and the SMRAM registers. The PIIX3 ISA bridge provides PIRQRC A–D
  at 0x60–0x63, with bit 7 as the disable bit. It shares INTx routing with ELCR
  level selection and PIC/IOAPIC delivery, and uses the pc slot swizzle.
- **Shared plumbing is reused, never copied.** PAM, INTx routing, timers and
  RTC reuse their q35 implementations through shared modules. q35's MCH and
  ICH9 identities never leak into pc.
- **A20 (D2).** The i8042's A20 output and fast-A20 port 0x92 both control
  address-bit-20 masking for CPU accesses. The mechanism (memslot aliasing of
  the 1 MiB wrap or equivalent) is the implementer's choice. A20 state rides
  snapshots and resets.
- **What pc omits (D3).** It has no VMware backdoor, no VMware mouse and no
  kvmvapic ROM. A clean-room TPR ROM is added only if a Windows HAL test proves
  it's needed (HV-D14).
- **Reset and snapshot.** Every new device implements power-on reset and joins
  the versioned device-state snapshot format. Reset happens in place, as
  phase 2 delivered.

### Firmware inputs (`vmlab-hv-firmware`)

- **pc ACPI.** The table set is RSDP revision 0, RSDT, FADT revision 1, a DSDT
  with the PIIX namespace (`LNKA`–`LNKD`, `LNKS`, the S08 ISA bridge with COM1,
  COM2, KBD, MOU, FDC0, LPT1 and RTC), FACS, an APIC table, HPET and WAET.
  There is no MCFG and no XSDT. Sleep types are pc's own: `_S3`=1, `_S4`=2,
  `_S5`=0. These inputs sit beside q35's rather than inside a generic builder.
- **pc SMBIOS.** The version strings are `pc-i440fx-11.1` and the product is
  `Standard PC (i440FX + PIIX, 1996)`.
- **PIIX4 PM.** It uses PMBASE 0x600, the SMBus at 0x700, SCI on IRQ 9, GPE0 at
  0xafe0, and APM command port 0xb2.
- **CMOS and boot order.** CMOS describes RAM, disk and floppy geometry, and
  the boot choices consistent with the attached devices. The boot order
  matches vmlab's: CDs at the lowest indexes, disks at 50 and up, the floppy
  never indexed.
- **The reference.** Each template's `docs/hwref/pc-*` capture is the
  reference these must match (P2).

### Devices (`vmlab-hv-devices`)

- **ATA/ATAPI layer.** Extract the command and media logic out of AHCI, with
  identify parameterised by transport. AHCI and IDE both sit on top of it. No
  SATA or NCQ bits appear in PIIX identify data, and no AHCI PRDT semantics
  appear in IDE.
- **PIIX IDE.** Two channels with four units, legacy ports and IRQs 14 and 15,
  and a bus-master block at BAR4. It covers task-file, control and data
  ports, master/slave selection, SRST, BSY/DRQ/ERR/IRQ sequencing, CHS and
  LBA PIO with multiple-sector commands, ATAPI PACKET with byte counts, PRD
  walking within 64 KiB boundaries, and media change.
- **Floppy and ISA DMA.** An 82078-style controller at 0x3f0–0x3f5 and 0x3f7
  (0x3f6 belongs to IDE), on IRQ 6 and DMA channel 2. It covers command,
  execution and result phases, seek/recalibrate/sense/read/write/format, motor
  and reset, disk-change and write-protect. The existing `IsaDevices` register
  model becomes a working 8237 pair: guest-memory transfers, masking, count
  and terminal count, autoinit, and page-register wrap.
- **Cirrus CL-GD5446.** It covers the extended sequencer, graphics and CRTC
  registers, unlock, bank switching, packed-pixel scanout, palette, hardware
  cursor, MMIO aliases, and the BitBLT engine: ROPs, fills, overlap direction,
  transparency, colour expansion and the CPU-to-screen FIFO.
  - **Scope.** At minimum, the BitBLT engine covers every blit the Windows 98
    SE capture records.
  - **What it reuses.** VGA register and planar logic, frame delivery and the
    display protocol.
- **PCnet Am79C970A.** It covers the PROM/MAC, RAP/RDP/BDP and reset, word and
  dword modes, CSR/BCR state, init blocks, software styles, descriptor
  ownership and chaining, filters, interrupts, safe DMA, and vmlab's
  stream-socket NIC attachment.
- **e1000 82540EM.** It covers the MMIO and I/O windows, EEPROM, PHY/MDIC, link,
  legacy and the extended TX descriptors drivers use, RX/TX rings, filters,
  checksum offload, ICR/IMS/IMC with interrupt delay timers, and safe DMA on
  the same attachment.
- **UHCI.** PIIX3 identity at 00:01.2, covering frame list, QH/TD processing,
  control and interrupt transfers, root-hub ports and the legacy handoff. It
  carries the existing HID tablet (D6).
- **16550 (D5, corrected by the owner after socket measurement).** Transmit
  immediately while the backend accepts bytes, with QEMU-identical FIFO,
  LSR/THRE interrupts, and receive backpressure. The programmed divisor controls
  receive timeouts. See `notes/03-serial-parity.md`. The agent socket's connection survives
  reset and snapshot load, and the HV-D10 magic resync still applies.

### vmlab side (4e)

- `labd/hypervisor/vmlab_hv/describe.rs` describes pc VMs:
  - IDE slots and bootindex.
  - The floppy.
  - The NIC model and display device.
  - COM1 agent plus COM2 log.
  - `cpu_model`, `rtc`, Hyper-V flags.
- `vmlab_refusals` drops the pc refusals as each sub-phase lands. vmlab's
  docs and the PRD §22 phase table follow.
- `input_transport = "vnc"` on a vmlab-hv machine routes to the native input
  path (D4).
- e2e gains a `legacy` lab booting the four gate guests (D1). `vm.hw.floppy`,
  `vm.media.floppy` and `share.smb1` move to phase 4's delivered tag.
- The Windows 95 templates are rebuilt on KVM with `pentium3` and FIX95CPU
  applied during the template build, and drop `accel = "tcg"` (D7).

### Clean room

- **Sources.** Public datasheets define each device: Intel 82441FX, 82371SB
  and PIIX4, Intel 82078, the Cirrus CL-GD5446 technical reference, AMD 19436,
  and the Intel 8254x SDM. The ATA/ATAPI and UHCI standards, and QEMU's
  published fw_cfg and std-VGA specs, are also allowed.
- **QEMU-specific behaviour.** The captures in `docs/hwref/pc-*` fix the
  subset QEMU implements. QEMU C source is never read.

## Testing decisions

- **P1, S1 in pc mode.** `vmlab-hv-tests` boots each gate template with a `pc`
  description, plus an agent-enabled `windows-legacy` lifecycle run on Server
  2008 R2 Core 7601.0.0 (owner clarification in `notes/04-smoke-template.md`). Each
  run uses a fresh overlay. The harness drives the legacy agent over COM1, or
  virtio-serial for `windows-legacy`. Per sub-phase:
  - **4a:** TempleOS to its HolyC agent, and Windows 7 to agent-ready with no
    NIC. Then the A20 wraparound probe through both i8042 and port 0x92,
    across reset and snapshot load (D2). Then Windows 7 HAL checks for the D3
    omissions.
  - **4b:** Windows 7 with e1000 doing DHCP and ping, and FreeDOS with PCnet.
  - **4c:** FreeDOS reading a floppy image, and the tablet on UHCI.
  - **4d:** Windows 98 SE to its desktop and agent under Cirrus. It also closes
    the accepted unknowns: Windows 98's PCnet data path and clean shutdown.
  - **Snapshots and resets:** every guest round-trips snapshot load and reset
    with the strict reboot check.
  - **Missing prerequisites:** a test skips with a reason when its template or
    `/dev/kvm` is missing.
- **P2, the hwref differential.** The static state each gate template sees on
  vmlab-hv is diffed against `docs/hwref/pc-<template>/`: PCI config bytes,
  ACPI tables, SMBIOS, fw_cfg file list and sizes, and e820. Every difference
  is either fixed or put on the allow-list with a reason.
- **P4, device tests at the guest interface.** These cover:
  - **Chipset:** PIIX3/PIIX4/i440fx config spaces and write masks, PIRQ
    routing and ELCR.
  - **IDE:** PIO, bus-master and ATAPI, including errors and media change.
  - **Floppy:** controller phases and ISA DMA terminal count, autoinit and
    page wrap.
  - **Cirrus:** BitBLT cases built from the Windows 98 SE capture's blit
    tables.
  - **PCnet:** init block, both software styles and descriptor rings.
  - **e1000:** rings, interrupt moderation and EEPROM.
  - **UHCI:** schedules.
  - **16550:** immediate TX, divisor-based RX timeouts, and FIFO-full backpressure.
- **P5, vmlab e2e on `--hypervisor vmlab`.** The `legacy` lab plus the
  phase-4-tagged features. Every applicable feature passes.
- **Merge bar.** `just ci::check` stays green. P1 and P2 skip cleanly without
  templates or KVM. Interpreter and lockstep checks are unaffected.

## Out of scope

- **Excluded hardware:** SB16, AdLib, the parallel port beyond its AML
  declaration, and the second PIT label.
- **x86 TCG of any kind (HV-D3).** Windows 95 moves to KVM instead (D7).
- **Windows XP.** There's no local template. It joins the gate only if one is
  built (D1).
- **Vendor GPU workarounds and Cirrus 3D.**
- **macOS.** It's its own phase.

## Open questions

Owner dispositions, 2026-10-31. Each is assigned to a sub-phase, not left open.

- A20 mechanism cost (4a): the 4a implementer chooses the mechanism, measures
  FreeDOS and Windows 98 SE boot and a real-mode-heavy loop with it, and records
  the choice and the numbers.
- Cirrus BitBLT coverage (4d): implement the full blitter the CL-GD5446
  technical reference documents (all 16 ROPs, pattern and colour-expansion
  modes, transparency, both directions, 8/16/24/32 bpp, system-to-screen), not
  only the captured subset. The Windows 98 SE capture is the test floor.
- Unstamped windows-legacy templates (4e): the 4e implementer finds what vmlab
  does at `up` with a template that has no `agent_version`, on QEMU first, and
  reports whether the `windows-legacy` smoke run needs a different readiness
  path.
- TempleOS `set_time` (4e): the 4e implementer tests it on QEMU against the
  existing template and records the result; if unsupported, vmlab says so for
  `clock = "host"` on TempleOS.
