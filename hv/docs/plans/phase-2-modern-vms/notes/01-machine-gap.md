---
id: "01"
kind: research
---
# Phase 2 machine-model gap

## Scope and evidence

F1. Phase 1 supplies a direct-kernel micro-VM, not the captured q35 platform. D30 requires identical guest-visible hardware; D23 requires unchanged firmware. This report records gaps, not permission to simplify them. Sources: `.scratch/plans/vmlab-hypervisor/SPEC.md:116` and `:177`; decision rows in `.scratch/plans/vmlab-hypervisor/MAP.md:58` and `:65`.

Read-only research on 2026-10-30; only this note was written. Paths are relative to `/home/wil/dev/vmlab-hypervisor` unless absolute. Capture names below are L=`q35-seabios-linux-generic`, W=`q35-ovmf-win11`, S=`q35-ovmf-win11-secureboot` under `docs/hwref/`.
The oracle is QEMU 11.1.2. W and S run Linux probes, not Windows; S uses blank VARS with SecureBoot=0. Enforcing secure boot, Windows migration, reset-state registers, new S1/S2 runs and `just ci::check` were not checked. No source/interface changes or commit were made under the one-output-file brief. `docs/hwref/README.md:17`; `docs/hwref/q35-ovmf-win11-secureboot/README.md:32`.
No QEMU C source was read. Local QEMU reading was confined to `docs/specs/fw_cfg.rst`; firmware source reading used the separate SeaBIOS and edk2 projects. Firmware-source URLs below describe upstream master, not a verification of the exact captured distribution builds.

## Captured PCI functions

All BDFs are domain 0000, bus 00. L has 13 functions; W and S have 12. Counted lines in each `guest/lspci-nn.txt`. IDs and revisions: `docs/hwref/q35-seabios-linux-generic/guest/lspci-nn.txt:1`; `docs/hwref/q35-ovmf-win11/guest/lspci-nn.txt:1`; `docs/hwref/q35-ovmf-win11-secureboot/guest/lspci-nn.txt:1`.

| BDF | Function | Vendor:device | Revision | Present |
|---|---|---|---|---|
| 00:00.0 | Q35 host bridge | 8086:29c0 | 00 | L,W,S |
| 00:01.0 | transitional virtio-serial | 1af4:1003 | 00 | L,W,S |
| 00:02.0 | transitional virtio-blk | 1af4:1001 | 00 | L,W,S |
| 00:03.0 | transitional virtio-net | 1af4:1000 | 00 | L,W,S |
| 00:04.0 | standard VGA | 1234:1111 | 02 | L |
| 00:04.0 | virtio-vga | 1af4:1050 | 01 | W,S |
| 00:05.0 | qemu-xhci | 1b36:000d | 01 | L |
| 00:1d.0 | ICH9 UHCI #1 | 8086:2934 | 03 | L,W,S |
| 00:1d.1 | ICH9 UHCI #2 | 8086:2935 | 03 | L,W,S |
| 00:1d.2 | ICH9 UHCI #3 | 8086:2936 | 03 | L,W,S |
| 00:1d.7 | ICH9 EHCI | 8086:293a | 03 | L,W,S |
| 00:1f.0 | ICH9 LPC | 8086:2918 | 02 | L,W,S |
| 00:1f.2 | ICH9 SATA/AHCI | 8086:2922 | 02 | L,W,S |
| 00:1f.3 | ICH9 SMBus | 8086:2930 | 02 | L,W,S |

Subsystem IDs are 1af4:0003/0002/0001 for serial/block/net respectively, and 1af4:1100 for the other functions. Full BARs, capabilities, class/prog-if and config bytes are in `docs/hwref/q35-seabios-linux-generic/guest/lspci-nnnvvvxxxx.txt:1` and `docs/hwref/q35-ovmf-win11/guest/lspci-nnnvvvxxxx.txt:1`.
No PCIe root ports, subordinate buses, IDE function, HDA function, or PCI TPM function appear. TPM is ISA/MMIO. PCI topology is flat; root ports cannot be inferred from the word q35. `docs/hwref/q35-seabios-linux-generic/guest/lspci-tv.txt:1`; `docs/hwref/q35-ovmf-win11/guest/lspci-tv.txt:1`.
The PCI identities do not imply interchangeable transports: captured serial/block/net expose legacy I/O BAR0, MSI-X BAR1 and modern BAR4. W's virtio-vga has BAR0 framebuffer, BAR2 modern transport and BAR4 MSI-X. `docs/hwref/q35-ovmf-win11/guest/lspci-nnnvvvxxxx.txt:23`.
L includes xHCI in addition to ICH9 USB. W/S retain ICH9 USB without xHCI. USB tablet is 0627:0001; Linux's 1d6b root-hub IDs are driver-created, not PCI controller IDs. `docs/hwref/q35-ovmf-win11/guest/usb.txt:1`; `docs/hwref/q35-seabios-linux-generic/qemu-argv.txt:45`.

## ISA, timers, interrupts and ACPI namespace

| Piece | Observed interface | Evidence |
|---|---|---|
| RTC/CMOS | PNP0B00, 70-77, IRQ8; clock, alarms and NVRAM | `docs/hwref/q35-seabios-linux-generic/guest/pnp.txt:25`; `guest/dmesg.txt:352` in the same directory |
| PS/2 | PNP0303 keyboard, ports 60/64 IRQ1; PNP0F13 mouse IRQ12 | `docs/hwref/q35-seabios-linux-generic/guest/pnp.txt:1` |
| Parallel | PNP0400, 378-37f IRQ7 | `docs/hwref/q35-seabios-linux-generic/guest/pnp.txt:11` |
| Serial | PNP0501 COM1, 3f8-3ff IRQ4 | `docs/hwref/q35-seabios-linux-generic/guest/pnp.txt:18` |
| PIT/PIC/DMA | PIT 40-43 and timer1 50-53; PIC 20-21/a0-a1; ISA DMA 00-1f/80-8f/c0-df | `docs/hwref/q35-seabios-linux-generic/guest/ioports.txt:1` |
| APIC | LAPIC fee00000; IOAPIC fec00000, GSI0-23 | `docs/hwref/q35-ovmf-win11/guest/dmesg.txt:128` |
| HPET | fed00000, length 400; table block ID 8086a201 | `docs/hwref/q35-seabios-linux-generic/guest/acpi/HPET.dsl:23` |
| ACPI PM | SCI9; PM event 600, control 604, timer 608, GPE 620; SMI b2, enable/disable 02/03 | `docs/hwref/q35-seabios-linux-generic/guest/acpi/FACP.dsl:27` |
| fw_cfg | QEMU0002, ports 510-51b | `docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT.dsl:3191` |
| TPM, W/S | MSFT0101, fed40000 length 5000; physical-presence region fed45100 | `docs/hwref/q35-ovmf-win11-secureboot/guest/acpi/DSDT.dsl:3361` |
| Floppy | No FDC PNP node, PCI function or DSDT device observed in these q35 captures | `docs/hwref/q35-seabios-linux-generic/guest/pnp.txt:1`; `guest/acpi/DSDT.dsl:3211` in the same directory |

ACPI table inventory, measured by enumerating uppercase raw filenames in `guest/acpi/`: L has APIC, DSDT, FACP, FACS, HPET, MCFG, WAET. W adds TPM2 and BGRT. S additionally has WSMT. RSDP/RSDT/XSDT are evidenced in boot logs, not separate raw files: `docs/hwref/q35-seabios-linux-generic/guest/dmesg.txt:34`; `docs/hwref/q35-ovmf-win11/guest/dmesg.txt:87`; `docs/hwref/q35-ovmf-win11-secureboot/guest/dmesg.txt:89`.
L uses a revision-0 RSDP and RSDT; W/S use revision-2 RSDP and XSDT. QEMU tables use OEM `BOCHS `, table ID `BXPC    ` and usually compiler `BXPC`; firmware-generated BGRT/WSMT use `INTEL `/`EDK2    `. `docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT.dsl:11`; `docs/hwref/q35-ovmf-win11/guest/acpi/BGRT.dsl:13`; `docs/hwref/q35-ovmf-win11-secureboot/guest/acpi/WSMT.dsl:13`.
DSDT raw sizes are L 8,647 B, W 9,407 B, S 9,544 B, measured with `Path.stat().st_size`; these are generated guest observations, not QEMU implementation source.

The complete declared device/processor names, grouped by role, are:
- PCI root `PCI0`, DRAM/ECAM resource device `DRAC`; PIRQ link devices `LNKA` through `LNKH`, fixed GSI link devices `GSIA` through `GSIH`. `docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT.dsl:49`, `:119`, `:2323`.
- `HPET`, `PCI0.PRES` CPU-hotplug resources, `CPUS` processor container; processors `C000/C001` in L, plus `C002/C003` in W/S. `docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT.dsl:2822`, `:2868`, `:2903`, `:3025`; `docs/hwref/q35-ovmf-win11/guest/acpi/DSDT.dsl:3035`.
- `GPE0`, `PHPR` PCI-hotplug resources and `PCI0.FWCF`. Their advertised I/O is 620-62f, cc0-cd7 and 510-51b; CPU hotplug uses cd8-ce3. `docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT.dsl:2868`, `:3131`, `:3147`, `:3191`.
- PCI address nodes `S00/S08/S10/S18/S20` for slots 0-4; L also `S28` for slot5. `SE8/SE9/SEA/SEF` represent 1d.0/.1/.2/.7; `SF8/SFA/SFB` represent 1f.0/.2/.3. `docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT.dsl:3211`; `docs/hwref/q35-ovmf-win11/guest/acpi/DSDT.dsl:3269`.
- ISA children `KBD`, `MOU`, `LPT1`, `COM1`, `RTC` are declared under `SF8`; W/S additionally declare `TPM`. S adds `PCI0.SMI0` at b2-b3. `docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT.dsl:3275`; `docs/hwref/q35-ovmf-win11/guest/acpi/DSDT.dsl:3334`; `docs/hwref/q35-ovmf-win11-secureboot/guest/acpi/DSDT.dsl:134`.
The namespace also contains `_PRT`, `_OSC`, hotplug/eject/GPE methods and sleep packages. L's `_S3/_S4/_S5` types are 1/2/0, unlike phase 1's 3/4/5 constants. Merely renaming the OEM string cannot provide register/AML parity. `docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT.dsl:54`, `:3166`; `crates/vmlab-hv-vmm/src/pm.rs:24`.

## SMBIOS, memory and CPU identity

SMBIOS is 2.8. L has 9 structures/378 B: types 0,1,3,4,16,17,19,32,127. W/S have 10/426 B, with two type19 ranges. System/chassis/CPU version is `pc-q35-11.1`; system manufacturer `QEMU`, product `Standard PC (Q35 + ICH9, 2009)`, unspecified serial, UUID `Not Settable`. DIMM locator is `DIMM 0`, manufacturer QEMU. `docs/hwref/q35-seabios-linux-generic/guest/dmidecode.txt:3`; `docs/hwref/q35-ovmf-win11/guest/dmidecode.txt:3`.
L BIOS vendor/version/date is `SeaBIOS`/`Arch Linux 1.17.0-2-2`/`04/01/2014`; W/S is `Debian distribution of EDK II`/`2026.08+ds-2`/`09/26/2026`. Counts/widths differ with 2 versus 4 vCPUs and 2 versus 8 GiB. `docs/hwref/q35-seabios-linux-generic/guest/dmidecode.txt:7`; `docs/hwref/q35-ovmf-win11/guest/dmidecode.txt:114`.

All address ranges here are inclusive. L's complete OS e820 map is RAM 0-9fbff and 100000-7ffd7fff; reserved 9fc00-9ffff, f0000-fffff, 7ffd8000-7fffffff, b0000000-bfffffff, fed1c000-fed1ffff, feffc000-feffffff, fffc0000-ffffffff, fd00000000-ffffffffff. `docs/hwref/q35-seabios-linux-generic/guest/e820-memmap.txt:1`.
W's 8 GiB map has 2 GiB low RAM and 6 GiB at 100000000-27fffffff before firmware carves low RAM. ECAM is e0000000-efffffff versus L's b0000000-bfffffff. Common high reservations include feffc000-feffffff and fd00000000-ffffffffff. `docs/hwref/q35-ovmf-win11/guest/e820-memmap.txt:1`; `guest/acpi/MCFG.dsl:25` in the same directory.
W's low-memory carve-outs: NVS 800000-807fff, 80b000-80bfff, 814000-8fffff, 7fe54000-7fed3fff, 7ff60000-7fffffff; ACPI tables 7cb7a000-7cb7cfff and 7fe42000-7fe53fff; reserved 812000-812fff, 7eb3f000-7ebfffff, 7fbc2000-7fe41fff, 7fedc000-7ff5ffff. Remaining listed low ranges are RAM. ROM reservation ffc00000-ffffffff. `docs/hwref/q35-ovmf-win11/guest/e820-memmap.txt:1`.
S's low carve-outs: reserved 30000-4ffff, 9e000-9ffff, 812000-812fff, 7bc46000-7bec5fff, 7c000000-7fffffff; NVS 800000-805fff, 807000-807fff, 80b000-80bfff, 814000-1a0ffff, 7bed8000-7bf57fff, 7bf60000-7bffffff; ACPI tables 7882d000-7882ffff and 7bec6000-7bed7fff. Other listed low ranges are RAM. S retains W's ECAM/high RAM/common high reservations; it does not list W's separate ROM reservation. `docs/hwref/q35-ovmf-win11-secureboot/guest/e820-memmap.txt:1`.
The firmware input map is not that final OS map. Parsed the captured `fwcfg/blobs/etc/e820` using `struct.iter_unpack('<QQI', ...)`: L has three 20-byte entries, W/S four. All reserve feffc000+4000 and fd00000000+300000000, advertise RAM 0+80000000; W/S additionally advertise 100000000+180000000. Firmware makes the later subdivisions. `docs/hwref/q35-seabios-linux-generic/guest/fwcfg/sizes.txt:7`; `docs/hwref/q35-ovmf-win11/guest/fwcfg/sizes.txt:6`.
CPUID is host-dependent: captured host is Ryzen 9 9950X. L exposes `KVMKVMKVM`; W/S expose `Microsoft Hv` with `Hv#1`, feature leaves 40000002-40000005 and spinlock retry 1fff. Leaf1 signature is 00b40f40. The argv disables vmx/svm and enables the listed Hyper-V features for W/S; these are not implied by KVM_GET_SUPPORTED_CPUID. `docs/hwref/q35-ovmf-win11-secureboot/environment.txt:2`; `docs/hwref/q35-seabios-linux-generic/guest/cpuid.txt:2`; `docs/hwref/q35-ovmf-win11/guest/cpuid.txt:30`; `docs/hwref/q35-ovmf-win11/qemu-argv.txt:10`.

## Complete captured fw_cfg file inventory

Sizes are bytes, L/W/S respectively; `-` means absent. Actual inventory contradicts L's README claim about four `etc/boot-*` files: the only captured named kernel file is `etc/boot/kernel`. Numeric selectors are not inventoried by Linux's by_name list. `docs/hwref/q35-seabios-linux-generic/guest/fwcfg/sizes.txt:1`; `docs/hwref/q35-ovmf-win11/guest/fwcfg/sizes.txt:1`; `docs/hwref/q35-ovmf-win11-secureboot/guest/fwcfg/sizes.txt:1`.

| Name | L/W/S |
|---|---|
| bios-geometry | 0/0/0 |
| bootorder | 58/0/0 |
| etc/acpi/rsdp | 20/20/20 |
| etc/acpi/tables | 131072/131072/131072 |
| etc/boot-fail-wait | 4/4/4 |
| etc/boot/kernel | 11834368/-/- |
| etc/e820 | 60/80/80 |
| etc/smbios/smbios-anchor | 31/31/31 |
| etc/smbios/smbios-tables | 312/345/345 |
| etc/smi/features-ok | 1/1/1 |
| etc/smi/requested-features | 8/8/8 |
| etc/smi/supported-features | 8/8/8 |
| etc/system-states | 6/6/6 |
| etc/table-loader | 4096/4096/4096 |
| etc/tpm/config | -/6/6 |
| etc/tpm/log | 0/65536/65536 |
| genroms/kvmvapic.bin | 9216/9216/9216 |
| genroms/linuxboot_dma.bin | 1536/-/- |

## Phase 1 implementation and estimated missing work

F2. Existing building blocks are usable, but their current identities and placement differ from q35:
- KVM creates the in-kernel PIC/IOAPIC/LAPIC and PIT2; exposes GSI and MSI injection; passes the supported CPUID unchanged. `crates/vmlab-hv-vmm/src/kvm.rs:24`.
- RAM supports anonymous or shared-memfd backing; layout splits at c0000000, not W/S's 80000000. Direct bzImage/initrd loading builds e820/zero-page/RSDP and starts BSP in long mode. `crates/vmlab-hv-vmm/src/builder.rs:68`; `crates/vmlab-hv-vmm/src/layout.rs:58`; `crates/vmlab-hv-vmm/src/loader.rs:45`, `:164`.
- PCI has CF8/CFC and ECAM, BAR sizing/relocation and I/O/MMIO routing, but only bus0/function0 with 256-byte config; auto-allocation starts at slot1, leaving slot0 empty. `crates/vmlab-hv-vmm/src/pci.rs:130`, `:183`; buses reject overlap and return FF on unmapped reads: `crates/vmlab-hv-vmm/src/bus.rs:15`.
- ACPI installs RSDP/XSDT/FADT/MADT/MCFG/DSDT directly at 9e000, OEM VMLABH/HVMACHIN. DSDT has PCI0, PWRB, S0/S5. PM implements only six bytes at 600-605, power-button SCI and sleep signaling. `crates/vmlab-hv-vmm/src/acpi.rs:32`, `:49`, `:144`; `crates/vmlab-hv-vmm/src/pm.rs:78`.
- Real devices provide COM1, block, multiport console, net and external vhost-user-fs. Order is disks then console then NICs then shares, unlike the captures. Virtio PCI uses modern IDs 1040+type/revision2 and a single 32 KiB BAR4. `crates/vmlab-hv/src/devices.rs:48`; `crates/vmlab-hv-vmm/src/builder.rs:159`; `crates/vmlab-hv-devices/src/virtio/pci.rs:1`.
- Snapshots already save kernel irqchips/PIT/clock and vCPU state, but use a fixed MSR list. An HDA/audio implementation also exists, without attachment in the inspected builder and absent from these oracle captures. `crates/vmlab-hv-vmm/src/snapshot.rs:46`, `:370`; `crates/vmlab-hv-devices/src/hda/mod.rs:1`; `crates/vmlab-hv-vmm/src/builder.rs:121`.

Estimates below are additional handwritten Rust kLOC including unit/seam tests, not measured code sizes or schedules. They assume reuse of phase-1 disk/memory/queue infrastructure; GPU/USB ranges include integration of permitted upstream components. They exclude full implementation of unrelated chipset silicon. Evidence for absence is the complete builder wiring above and device exports at `crates/vmlab-hv-devices/src/lib.rs:12`.

| Piece | Missing behaviour or parity work | Estimated kLOC |
|---|---|---|
| P1 PCI placement/routing | explicit BDF, multifunction, extended config, INTx swizzle/PIRQ, firmware BAR allocation semantics | 2-4 |
| P2 Q35 MCH | 29c0 config, PCIEXBAR/ECAM, PAM ROM shadowing, RAM bounds, SMRAM/TSEG controls | 2-5 |
| P3 ICH9 LPC/PM | 2918 config/PIRQ, PMBASE, PM timer/GPE, SCI/SMI, ACPI enable/disable, reset cf9, S3/S4/S5 semantics | 3-6 |
| P4 RTC/CMOS | time/alarm/update IRQs, memory/CPU/boot CMOS bytes, rtc_base | 1-2 |
| P5 PS/2/i8042 | keyboard/mouse commands, IRQs, A20/reset and input delivery | 1.5-3 |
| P6 HPET | counter/comparators, IRQ routing, legacy replacement and snapshot time | 1.5-3 |
| P7 ISA DMA/LPT/misc | DMA register behaviour, parallel-port identity, port61/A20 ancillary platform behaviour | 1-3 |
| P8 SMBus | 2930 config, host-controller transactions and any populated slaves | 1-2 |
| P9 SATA/AHCI | 2922, command/FIS DMA, SATA ports, ATA/ATAPI for disks and installer CDs | 4-8 |
| P10 IDE if profile needs it | ATA taskfile/PIO/DMA/ATAPI; not evidenced as a PCI function in these captures | 3-6 |
| P11 fw_cfg | stream/select/directory, numeric keys, DMA including writable items | 1-2 |
| P12 ACPI/SMBIOS/e820 | q35 tables/AML namespace, loader script, firmware input memory map, SMBIOS identity | 3-6 |
| P13 SeaBIOS platform boot | ROM mapping/aliases/shadowing, reset entry and option-ROM handling | 1-2 |
| P14 pflash | CFI command/status/erase/program state, code/vars placement, persistence/protection | 1.5-3 |
| P15 SMM | KVM SMM capability/address spaces, SMI injection, SMRAM/TSEG visibility/locking, protected variable writes | 3-6 |
| P16 CPU/Hyper-V | topology/CPUID filtering, advertised MSRs/hypercalls/SynIC/timers/IPIs and restore state | 2-5 |
| P17 transitional virtio | legacy register/queue path plus QEMU config/capability/BAR shapes and IDs | 2-4 |
| P18 VGA | standard VGA register/memory/Bochs VBE compatibility and firmware display | 3-6 |
| P19 virtio-vga/GPU | VGA compatibility plus GPU/rutabaga integration, frame export and RFB | 5-12 |
| P20 ICH9 USB | three UHCI functions plus EHCI, companion routing and tablet | 5-10 |
| P21 xHCI/USB host | 000d controller identity plus USB device/backend/hotplug integration | 4-8 |
| P22 TPM | tpm-tis registers, swtpm transport, TPM2/physical-presence/log integration | 2-4 |
| P23 vfio | assignment, DMA/IOMMU maps, IRQs, lifecycle/restore constraints; absent from captures | 3-6 |
| P24 in-process virtio-fs | replace daemon backend, retain guest protocol and snapshot state | 4-8 |
| P25 machine lifecycle | firmware-aware reset, flash/device snapshot sections and expanded launch description | 2-4 |

P23/P24 are phase-2 contract work, not devices observed in these three machines. Existing PIC/PIT/serial need parity checks rather than assumed replacement. PCIe root-port and FDC implementation sizes are not estimated because neither is instantiated here; other profile captures would establish their scope. `.scratch/plans/vmlab-hypervisor/SPEC.md:177`; `docs/hwref/q35-seabios-linux-generic/guest/lspci-tv.txt:1`.

## Firmware platform contract

F3. Firmware installs tables and discovers memory/devices through platform interfaces; executing its binary alone is insufficient.

The x86 fw_cfg selector is a little-endian 16-bit write at 510; data is an advancing byte stream at 511, selection resets offset and exhausted reads return zero. Signature key0 returns QEMU; feature key1 advertises DMA. Directory key19 contains a big-endian count and 64-byte file records. DMA address is at 514, big-endian; descriptors carry control/length/address and support select/read/skip/write with completion/error status. `/home/wil/dev/qemu-vmlab/docs/specs/fw_cfg.rst:14`, `:95`, `:130`, `:150`, `:199`.
SeaBIOS consumes `etc/e820`, `etc/table-loader`, numeric CPU/NUMA/boot-menu/IRQ-override entries, and CMOS fallbacks. It loads `bootorder` and `etc/boot-fail-wait`, and SMBIOS anchor/tables. Bootorder is newline-separated firmware device paths, not PCI IDs alone; L includes `/rom@genroms/linuxboot_dma.bin` then `/pci@i0cf8/scsi@2/disk@0,0`. Firmware source: https://github.com/coreboot/seabios/blob/master/src/fw/paravirt.c#L241 , #L604 , #L729; https://github.com/coreboot/seabios/blob/master/src/boot.c#L254 ; https://github.com/coreboot/seabios/blob/master/src/fw/biostables.c#L607 ; captured `docs/hwref/q35-seabios-linux-generic/guest/fwcfg/blobs/bootorder:1`.
The `etc/table-loader` ABI uses little-endian 128-byte records: command1 ALLOCATE with 56-byte filename/alignment/HIGH-or-FSEG zone, command2 ADD_POINTER with source/destination/offset/width, command3 ADD_CHECKSUM, command4 WRITE_POINTER through fw_cfg DMA. Allocations precede references. The VMM supplies blobs plus relocation commands; firmware performs placement, pointer patching and checksums. https://github.com/coreboot/seabios/blob/master/src/fw/romfile_loader.h#L7 ; https://github.com/coreboot/seabios/blob/master/src/fw/romfile_loader.c#L51 .
SeaBIOS executes the script and finds the installed RSDP. OVMF reads the same script, allocates blobs as ACPI NVS, processes relocation/checksum/writeback commands and installs tables through EFI_ACPI_TABLE_PROTOCOL. Thus phase 1's preinstalled RSDP pointer is not this firmware interface. https://github.com/coreboot/seabios/blob/master/src/fw/paravirt.c#L304 ; https://github.com/tianocore/edk2/blob/master/OvmfPkg/Library/AcpiPlatformLib/QemuFwCfgAcpi.c#L368 , #L1051 , #L1121 , #L1198.
OVMF uses `etc/e820` plus CMOS memory sizing, including 34/35 for memory above 16 MiB and 5b-5d for above-4-GiB size. Its boot-order library translates `bootorder` to UEFI BootOrder/device paths. https://github.com/tianocore/edk2/blob/master/OvmfPkg/Library/PlatformInitLib/MemDetect.c#L405 , #L484 ; https://github.com/tianocore/edk2/blob/master/OvmfPkg/Library/QemuBootOrderLib/QemuBootOrderLib.c#L1546 .
For these 4 MiB OVMF builds, upstream layout defines VARS size 84000 at ffc00000 and CODE size 37c000 at ffc84000 through ffffffff; block size 1000. This is a layout derivation, not a newly measured QEMU memory-tree dump. Preserve existing variable-store bytes, firmware volumes and writable VARS backing. https://github.com/tianocore/edk2/blob/master/OvmfPkg/Include/Fdf/OvmfPkgDefines.fdf.inc#L11 , #L53 ; https://github.com/tianocore/edk2/blob/master/OvmfPkg/OvmfPkgX64.fdf#L39 .
`cfi.pflash01` must implement flash semantics, not just RAM containing firmware. OVMF issues program10, erase20/confirmd0, status70/clear50, ID90 and arrayff commands. CODE is read-only; VARS persists writes. https://github.com/tianocore/edk2/blob/master/OvmfPkg/QemuFlashFvbServicesRuntimeDxe/QemuFlash.c#L17 , #L188 , #L234 ; `docs/hwref/q35-ovmf-win11-secureboot/qemu-argv.txt:37`.
SMM_REQUIRE firmware refuses missing SMM, even in release builds; its variable stack requires pflash and the secure flash policy. Required platform behaviour includes KVM SMM execution, SMRAM/TSEG mapping, APMC SMI delivery, ICH9 SMI-enable/lock registers and flash access restricted appropriately across SMM/non-SMM. OVMF negotiates `etc/smi/supported-features`, writes `requested-features`, then reads `features-ok`; a read-only fw_cfg substitute is insufficient. https://github.com/tianocore/edk2/blob/master/OvmfPkg/README#L160 ; https://github.com/tianocore/edk2/blob/master/OvmfPkg/SmmControl2Dxe/SmmControl2Dxe.c#L190 ; https://github.com/tianocore/edk2/blob/master/OvmfPkg/SmmControl2Dxe/SmiFeatures.c#L93 .
Firmware boot must back the x86 reset fetch at fffffff0 and provide the low BIOS/option-ROM window with q35 shadow controls. SeaBIOS reserves the F-segment; L's ROM reservation is fffc0000-ffffffff. Phase 1 instead enters a loaded kernel and its real-mode helper explicitly avoids the unbacked reset address. ROM alias/reset register fidelity was not dynamically checked. `crates/vmlab-hv-vmm/src/loader.rs:234`; `docs/hwref/q35-seabios-linux-generic/guest/e820-memmap.txt:3`, `:9`; https://github.com/coreboot/seabios/blob/master/scripts/layoutrom.py#L64 .

## Template identity consequences

F4. Observable identity includes vendor/device/subsystem/revision/class IDs, BDF/topology, BAR/capability shapes, INTx routing, MAC and device serials, ACPI paths/HID/CID/UID/ADR/OEM strings, SMBIOS system/BIOS/UUID strings and CPU topology/features. The exact captured values above differ from phase 1; changing only the vendor string would leave those differences. Sources: PCI, DSDT, SMBIOS and CPUID inventories above; `crates/vmlab-hv-vmm/src/acpi.rs:32`; `crates/vmlab-hv-devices/src/virtio/pci.rs:1`.
Windows PCI hardware IDs include VEN/DEV/SUBSYS/REV and class variants. Device instances depend on bus-reported device/instance identity. Inference: changing transport IDs or PCI ancestry can create new device nodes and driver binding, with changed per-device settings; exact effects on these templates were not checked. https://learn.microsoft.com/en-us/windows-hardware/drivers/install/identifiers-for-pci-devices ; https://learn.microsoft.com/en-us/windows-hardware/drivers/install/device-instance-ids .
Changing the boot controller or its mode can cause INACCESSIBLE_BOOT_DEVICE. This includes a potential transitional-to-modern virtio change where the installed boot driver does not bind; that specific transition was not tested. Windows activation may fail after significant hardware changes; Microsoft documents motherboard replacement as an example, not a public per-field activation hash. No activation outcome is established by matching SMBIOS alone. https://learn.microsoft.com/en-us/troubleshoot/windows-client/performance/stop-error-7b-or-inaccessible-boot-device-troubleshooting ; https://support.microsoft.com/en-us/windows/activation/reactivating-windows-after-a-hardware-change .
Linux also observes PCI topology; systemd may derive NIC names from slot/path/MAC metadata. Inference: moving 00:03.0 or changing the MAC can break template networking policies despite a working driver. UEFI boot variables can encode device paths, so preserving VARS without preserving the device path is not sufficient. https://www.freedesktop.org/software/systemd/man/latest/systemd.net-naming-scheme.html ; https://github.com/tianocore/edk2/blob/master/OvmfPkg/Library/QemuBootOrderLib/QemuBootOrderLib.c#L1546 .

## Open questions this raises

- Q1. Which versioned q35 identity must each existing template retain? The captures resolve the unversioned alias to pc-q35-11.1; they do not establish the creation versions of older templates.
- Q2. What S1 differential gate compares firmware-installed tables, PCI config and OS enumeration while accounting for timestamps and firmware allocation addresses? Current captures are post-Linux-initialization observations.
- Q3. Which signed probe and real Windows templates establish enforcing secure boot, persisted VARS, TPM identity, boot-driver binding and activation after the switch? Those behaviours were not checked here.
- Q4. Which profile variants require AHCI-attached boot disks/CDs, IDE, root ports, USB-host or vfio beyond these captures? No root ports/FDC are instantiated in this evidence set.
- Q5. How will phase-2 launch descriptions preserve explicit BDFs, firmware/flash geometry, CPU policy and per-machine identity while retaining the phase-1 path? The inspected builder accepts only Boot::Kernel and chooses slots internally.
- Q6. What unmodified reset/firmware boots establish the exact CMOS bytes, ROM aliases, CFI geometry, SMI handshake and SMRAM visibility? Source requirements and OS maps do not prove reset-state parity.
