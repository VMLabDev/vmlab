---
id: "02"
kind: research
---

# vmlab's foreign-arch guests and the virt machines

Measured, not guessed, unless marked *estimate*. vmlab paths are relative
to `/home/wil/dev/vmlab`; HV-Dn are `../vmlab-hypervisor/MAP.md` decisions
(phase-3 Dn are this map's). Raw dumps: `target/p3-research/g2/` (see its
README). QEMU 11.1.2, host Ryzen 9 9950X, MTTCG, one VM at a time under
`systemd-run --user --scope -p MemoryMax=32G -p MemorySwapMax=0`.

## 1. How vmlab runs foreign-arch guests today

- Machine is fixed per arch, not a profile knob: non-x86 resolves to
  `virt`, riscv64 to `virt,acpi=off` (`src/qemu/resolve.rs:502-517`;
  containers identical at `resolve.rs:682-687`). x86 profiles' `machine`
  is ignored on foreign arch.
- Accelerator: KVM only when guest arch == host arch, else TCG with
  `-cpu max` (`src/qemu/cmdline.rs:120-126,467-470`). On x86_64 hosts every
  aarch64/riscv64 guest is TCG, always (HV-D3 makes this permanent).
- Firmware: a guest that names none gets UEFI on non-x86 — `virt` has no
  SeaBIOS (`resolve.rs:366-370`). vmlab ships its own firmware set pinned
  by sha256 under the guest assets (`src/qemu/firmware.rs:1-14`; observed
  at `~/.local/share/vmlab/guest/firmware/<arch>/`), falling back to host
  paths (`/usr/share/AAVMF`, `/usr/share/edk2/riscv64`, `firmware.rs:170-196`).
  AAVMF = 64 MiB pflash CODE+VARS; RISCV_VIRT = 32 MiB, already padded.
  riscv64 has no secure-boot variant (`firmware.rs:77-92`).
- Device placement on `virt` (`src/qemu/cmdline.rs`): disks and CD-ROMs are
  all `virtio-blk-pci` — `virt` has no IDE/AHCI, cloud-init finds the
  CIDATA seed by label on any block device (`:667-707`); display
  `virtio-vga` auto-downgrades to `virtio-gpu-pci` (`resolve.rs:458-479`);
  USB needs an explicit `qemu-xhci` plus `usb-tablet` (`:839-847`); the
  agent rides `virtio-serial-pci` port `vmlab.agent.0` (`:598-606`); serial
  is `-serial file:` — PL011 (`ttyAMA0`) on aarch64, 16550A (`ttyS0`) on
  riscv; TPM would be `tpm-tis-device` (MMIO).
- Container micro-VMs: direct kernel boot (`-kernel/-initrd/-append` with
  `vmlab.root=/dev/vda vmlab.scratch=/dev/vdb`), virtio-blk rootfs+scratch,
  two virtio-serial ports, virtio-net, no display/USB; console differs per
  arch (`src/qemu/container.rs:61-74`). They boot with no firmware at all.
- e2e: feature `arch.emulated` is scoped to phase 3
  (`e2e/features.py:211,290`); its scenario boots aarch64 under TCG and
  checks the argv, the bundled AAVMF reaching its UEFI shell on serial, and
  a screenshot PNG (`e2e/scenarios/admin.py:126-148`).
- Templates: `linux-modern` profile (virtio, OVMF), 2 CPUs, 2 GiB, 20 GiB
  disk, agent sealed. Local store: aarch64 (debian-13, alpine-3.23,
  fedora-44, ubuntu-24.04, home-assistant), riscv64 (debian-13, ubuntu-24.04,
  fedora-42). Examples: `examples/alpine-arm64`, `examples/riscv64-ubuntu`.

## 2. What the virt machines present (black box, vmlab's argv)

Guests booted from the local debian-13 templates with the argv above.

### aarch64 `virt`, AAVMF, `-cpu max`, TCG (ACPI boot)

- **Boots ACPI, not DT**: no `/proc/device-tree`; `/sys/firmware/fdt` is
  only the 552-byte EFI stub DT (chosen/UEFI mmap). The platform arrives
  as ACPI tables QEMU generates (OEM `BOCHS`/`BXPC`): APIC(MADT), BGRT,
  DBG2, DSDT (5.4 KiB), FACP(v6), GTDT, IORT, MCFG, PPTT, SPCR; plus
  SMBIOS 3.0 and EFI runtime services via fw_cfg. SPCR names the console:
  `pl011,mmio32,0x9000000,9600`.
- **GICv2, not GICv3**: `/proc/interrupts` shows `GIC-0` and
  `GICv2m-PCI-MSIX-*`; dumpdtb shows `arm,cortex-a15-gic` at 0x08000000
  (CPU 0x08010000) and a GICv2m MSI frame at 0x08020000, SPIs 80-143 (64
  MSIs) — that is what plain `-machine virt` means, and vmlab never passes
  `gic-version`. A `gic-version=3` probe yields `arm,gic-v3` + ITS at
  0x08080000 (`aarch64/virt-gicv3.dtb`) — present in QEMU, unused by vmlab.
- PSCI v1.1 from ACPI, kernel logged `SMC Calling Convention v1.0`, while
  the DT PSCI node says `method = "hvc"` — the conduit differs by boot path.
- CPU: implementer 0x00, part 0x051 (`max`), architecture 8, features
  through SVE2p1/MOPS/LSE128/F8; SVE vector length up to 256 bytes.
- PCI (identical BDFs on both arches — same argv order): 00:00.0
  `1b36:0008` ECAM host; 00:01.0 `1af4:1003` virtio console; 00:02.0 and
  00:03.0 `1af4:1001` virtio-blk; 00:04.0 `1af4:1050` rev1 virtio-gpu;
  00:05.0 `1b36:000d` rev1 qemu-xhci. All virtio devices use MSI-X through
  GICv2m. Conventional PCI endpoints behind a generic ECAM root.
- Memory map (dumpdtb + `/proc/iomem`): RAM 2 GiB @0x40000000; 32
  virtio-mmio slots @0x0a000000+ (advertised, unused); platform-bus
  @0x0c000000; fw-cfg @0x09020000; PL031 RTC @0x09010000; PL011
  @0x09000000; pflash 2×64 MiB @0x0/0x4000000; PCI MMIO32 window
  0x10000000-0x3efeffff and 64 KiB I/O @0x3eff0000; ECAM 256 MiB
  @0x4010000000; PCI MMIO64 @0x8000000000.
- Timers: architectural (GTDT; counter at 1000 MHz per sched_clock), irq
  27 `arch_timer`; PMU pmuv3 irq 23; ACPI GED irq 41 (powerdown path);
  UART irq 1 (SPI 33).
- RTC as the guest sees it: **`rtc-efi`** (EFI runtime GetTime) — the
  PL031 exists but no driver binds under ACPI.
- Boot: kernel uptime 21.7 s to poweroff; ~35 s wall including UEFI
  (2 vCPUs). D1 speed-bar reference point.

### riscv64 `virt,acpi=off`, RISCV_VIRT edk2, `-cpu max`, TCG (DT boot)

- **Boots DT**: `/proc/device-tree` present; the FDT the kernel consumed
  (8653 bytes) matches QEMU's dumpdtb modulo runtime fields. `secureboot:
  Secure boot disabled` from edk2.
- SBI v3.0, implementation OpenSBI 1.0.8 (inside EDK2's RISCV_VIRT
  build): TIME, IPI, RFENCE, **SRST**, DBCN, HSM. Poweroff used SRST at
  46.4 s uptime; the `sifive,test1` syscon @0x100000 (write 0x5555 poweroff /
  0x7777 reboot) sits beside it, and both DT nodes (`syscon-poweroff/reboot`)
  are generated.
- ISA (`-cpu max`, kernel-filtered): `rv64imafdcvh_…` with zba-zbs,
  zvk*-vector-crypto, zk*, ztso, sstc/svinval/svnapot/svpbmt/smaia/ssaia,
  mmu `sv57`, marchid 0x2a, timebase 10 MHz. The DTB `riscv,isa` string is
  longer still (Sv57, H, smstateen, …). Hypervisor extension `h` included.
- Interrupts: **PLIC** @0x0c000000, 95 sources, 4 contexts (M+S per hart);
  CLINT @0x02000000; **no AIA/APLIC/IMSIC by default** (opt-in `aia=`,
  vmlab never passes it) and no PCI MSI controller → virtio devices run
  INTx, shared: measured `virtio0` and `xhci` both on PLIC 33. Timer via
  SBI TIME (`riscv-timer`), PMU via Sscofpmf (INTC 13), IPIs via SBI.
- Memory map: RAM 2 GiB @0x80000000 (OpenSBI reserves 256 KiB+128 KiB at
  its base); RTC `google,goldfish-rtc` @0x101000 (guest `rtc0` — this one
  the guest does drive); ns16550a @0x10000000 (irq 10); fw-cfg
  @0x10100000; 8 virtio-mmio slots @0x10001000+ (unused); pflash 2×32 MiB
  @0x20000000; ECAM 256 MiB @0x30000000; PCI MMIO32 1 GiB @0x40000000;
  64 KiB I/O @0x3000000; PCI MMIO64 @0x400000000.
- Boot: kernel uptime ~46 s to poweroff; ~60 s wall (2 vCPUs).

Not checked: display/scanout behaviour under a live viewer (devices
present, `-display none` + no client), AIA machine variants, the alpine/
fedora/ubuntu templates, USB tablet HID paths, e2e `arch.emulated` end to
end, and riscv64 direct-kernel (-kernel) DT shape for containers.

## 3. Minimum device set for phase-3 parity

QEMU-identical per HV-D30; the D26 dump-diff tier is the existing
`scripts/hwref` harness (README: "Run the P2 differential"), which needs
`virt` configurations added for phase 3.

Shared: generic PCIe ECAM root (1B36:0008, bus 0-ff, one 64 KiB I/O
window, 32-bit and 64-bit MMIO windows, addresses per arch above) +
virtio-blk/-net/-serial/-gpu as conventional-PCI functions at vmlab's BDF
order; UART (PL011 / ns16550A); RTC; fw_cfg MMIO (+DMA) — AAVMF and
RISCV_VIRT edk2 read everything through it; pflash CODE+VARS (2×64 MiB /
 2×32 MiB, HV-D23 reuses the firmware unchanged); RNG seed and, on arm,
kaslr-seed via firmware; reset/power at the machine level.

aarch64 adds: GICv2 + GICv2m MSI frame (the QEMU default here — not
GICv3/ITS); architectural timers + PMU (pmuv3); PSCI 1.1 for
CPU hotplug-idle, system_off and system_reset, in *both* HVC and SMC
encodings (DT vs ACPI conduit); ACPI table generation with BOCHS/BXPC
OEM identities (MADT with GICD/GICC/v2m, GTDT, MCFG, SPCR, IORT, PPTT,
DBG2, DSDT with GED) + SMBIOS 3.0; EFI runtime services the guest
actually consumes for time (rtc-efi) and variables; virtio-mmio slot
advertising in the DT QEMU hands firmware.

riscv64 adds: PLIC (95 sources, M+S contexts) + CLINT; SBI environment
(free — OpenSBI ships inside the reused EDK2 firmware, M-mode) with at
least TIME/IPI/RFENCE/SRST/HSM/DBCN; syscon test device as SRST fallback;
goldfish RTC; DT generation (compatible `riscv-virtio,qemu`) including
for container direct-kernel boots where no firmware runs — the VMM itself
must emit the DT QEMU would.

Container micro-VMs need less: direct kernel boot, virtio-blk×2,
virtio-serial×2 ports, virtio-net, UART, PLIC/CLINT or GIC/timers/PSCI,
DT (riscv) — no PCI display, xHCI, RTC or pflash.

Constraints for the emulator: the ISA string is guest-visible
(`/proc/cpuinfo`, DTB) and `-cpu max` on riscv64 is enormous; lockstep
against Sail models (HV-D22) only covers what the models implement; and
MSI differs by arch (GICv2m MSIX vs shared PLIC INTx lines), which the
virtio devices and the D26 dumps both expose.

## Open questions this raises

1. GIC version: parity means GICv2+v2m (measured), but GICv3+ITS is the
   modern interface and AAVMF supports both. GICv2-only is smaller;
   deviating upward breaks D30. Owner call.
2. Which PSCI conduit(s) must the TCG core trap — HVC, SMC, or both?
   Evidence says both are reachable (DT `hvc`, ACPI-boot kernel using
   SMC); needs a decoding of QEMU's FADT flags to pin down.
3. How exact must the aarch64 ACPI tables be for D26: byte-identical
   BOCHS/BXPC tables, or the normalized diff the hwref harness already
   applies to x86 tables extended for MADT/GTDT/SPCR/IORT/PPTT?
4. Which riscv64 ISA string does phase 3 ship? `-cpu max` includes `h` and
   zvk*-style sets the Sail model likely does not implement; cutting them
   changes a D30-visible surface. Needs the Sail-coverage answer (notes 01).
5. EFI time and variable services on aarch64 are live guest state (rtc-efi
   reads, pflash VARS writes): how do they ride online snapshots (HV-D14,
   HV-D31) and what resets the RTC across restore?
6. The measured boot times (21.7 s / 46 s kernel-time, 2 vCPUs, this
   host) are the QEMU TCG side of the phase-3 D1 bar — should they be
   captured by the notes/05 benchmark harness rather than one-off?

DONE /home/wil/dev/vmlab-hypervisor/.scratch/plans/phase-3-tcg/notes/02-virt-machines.md
