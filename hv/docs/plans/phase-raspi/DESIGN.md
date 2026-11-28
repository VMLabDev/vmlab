---
slug: phase-raspi
kind: design
created: 2026-11-28
card: 1238
---

# Phase raspi of vmlab-hv: Raspberry Pi boards as machine models

This document is the design of the Raspberry Pi machines of vmlab-hv. Nothing
in the repo models a Raspberry Pi today. It is a design: it adds no feature
code. The codes of this document (`RQ`, `RS`, `RM`, `RB`, `RK`, `RF`, `RD`,
`RG`, `RO`, `RV`, `RU`, `RC`, `Q`) are stable. `Cn`, `Fn`, `Gn`, `On`, `Sn`,
`Vn` and `Xn` without the letter `R` are codes of
`../phase-3d-aarch64/DESIGN.md`. `HV-Dn` is a row of
`../vmlab-hypervisor/MAP.md`.

A number marked *measured* comes from the sandbox of the card or from helios
(QEMU 11.1.3) on 2026-11-28. "How to measure again" is at the end. A fact
marked *from memory* comes from the public documents of RS1 to RS3 as the
author remembers them, and the card that builds the device checks it against
the document before it writes code. A number marked *est.* is an estimate.

## The defaults of the operator (card #1238, thread 1, posts 100 and 101)

- **RQ1.** An unmodified Raspberry Pi OS 64-bit image and an unmodified
  Ubuntu for Pi image boot to agent ready. No GPIO, I2C or SPI function.
- **RQ2.** All boards in three SoC groups, delivered B2, then B1, then B3.
- **RQ3.** The display is a framebuffer through the mailbox: console and
  desktop, no 3D. Full KMS, HVS and V3D are a later phase.
- **RQ4.** Each board presents its real core and feature set (A53, A72, A76).
  The Pi 5 machine gets an Armv8.2 CPU beside the Armv8.0 CPU of `virt`. Q2
  of 3d stays for `virt`.

The card says "eleven boards". Its list has twelve names: six in B1, three in
B2 and three in B3. This design covers the twelve.

| Group | SoC | Core | Boards | Device tree that the firmware stage loads |
|---|---|---|---|---|
| B1 | BCM2837, BCM2837B0, RP3A0 | 4 x Cortex-A53, Armv8.0 | Pi 3B, 3B+, 3A+, CM3, CM3+, Zero 2 W | `bcm2710-rpi-3-b`, `-3-b-plus` (3B+ and 3A+), `-cm3` (CM3 and CM3+), `-zero-2-w` |
| B2 | BCM2711 | 4 x Cortex-A72, Armv8.0 | Pi 4B, Pi 400, CM4 | `bcm2711-rpi-4-b`, `-400`, `-cm4` |
| B3 | BCM2712 with RP1 behind PCIe | 4 x Cortex-A76, Armv8.2 | Pi 5, Pi 500, CM5 | `bcm2712-rpi-5-b`, `-500`, `-cm5-cm5io` |

## Sources: what a card may read

| Code | Text | Use |
|---|---|---|
| RS1 | The public Broadcom documents: "BCM2835 ARM Peripherals", "BCM2711 ARM Peripherals", and "BCM2836 ARM-local peripherals" (the QA7 document) | The interrupt controllers of B1, the system timer, the mailbox registers, the PL011 and the mini UART, the DMA engine, the EMMC, the GPIO register file, the BCM2711 map and its GIC interrupt numbers. |
| RS2 | The RP1 peripherals datasheet of Raspberry Pi Ltd | The map of RP1 behind its BAR, its interrupt numbers, its clocks, the GPIO bank. |
| RS3 | The device trees of the Raspberry Pi kernel, as the blobs in `boot/` of `github.com/raspberrypi/firmware`, read only. The public Raspberry Pi documentation of `config.txt`, of the boot sequence, of revision codes, and of the mailbox property interface (the wiki of that repository) | Addresses, interrupts, which driver binds to what, the keys of `config.txt`, the tags. The blobs are data that a tool reads. No blob and no overlay is committed. |
| RS4 | Standards and vendor documents: the SD Physical Layer Simplified Specification, the SD Host Controller Simplified Specification, the xHCI specification, USB 2.0, the GICv2 architecture specification, PSCI, the Devicetree specification, the Linux arm64 boot protocol, the LAN9514 and LAN7800 datasheets of Microchip, the Cadence GEM description in the Zynq-7000 reference manual, the PL011 manual | The device models, as S7 of 3d. |
| RS5 | Drivers under a BSD licence for the same hardware: FreeBSD and NetBSD (`bcm2835_sdhost`, `bcm2838_pci`, `if_genet`, `bcmgenet`, `dwc_otg`, `cgem`), and EDK2 `Platform/RaspberryPi` (BSD-2-Clause-Patent). The card that first reads a file checks its header | Register behaviour that RS1 to RS4 do not give: GENET, the BCM2711 PCIe host, SDHOST, DWC2. |
| RS6 | The behaviour of a running QEMU `raspi3b`, `raspi3ap` and `raspi4b`, through qtest, the monitor, the gdbstub and the guest's own log | The oracle (RO1 to RO3). |
| RS7 | QEMU device source under `hw/`, and Linux driver source, both GPL-2.0, read to settle behaviour and never copied or translated (the device rule of `hv/AGENTS.md`). Linux driver source needs Q2 | What is left: the BCM2712 PCIe host, the MIP, the RP1 clocks, `sdhci-brcmstb`. |

X1, X2 and X3 of 3d hold for the CPU core without change. No card of this
phase reads `target/arm`. Each device file names in its header the sources
that its author read, and each card report lists them.

No Broadcom firmware is run or shipped: not `bootcode.bin`, not `start*.elf`,
not `fixup*.dat`, not the EEPROM images, not `armstub8.bin`, not the VL805
firmware. The files of the guest's own boot partition are read as data (RF).

## 1. What was measured

### RM1. The device trees (measured)

`measure/dtnodes.py` reads a device tree blob with the Python standard
library. The blobs are those of `raspberrypi/firmware` at commit
`46c2fa49622d28b4d48f2c536f9693831b824f8f` (2026-11-25).

| Tree | Nodes | Enabled nodes with a `compatible` | Distinct first `compatible` |
|---|---|---|---|
| `bcm2710-rpi-3-b` | 168 | 50 | 40 |
| `bcm2711-rpi-4-b` | 325 | 61 | 44 |
| `bcm2712-rpi-5-b` | 320 | 107 | 64 |

- **Secondary cores.** B1 and B2 trees say `enable-method = "spin-table"`
  with `cpu-release-addr` `0xd8`, `0xe0`, `0xe8` and `0xf0`. The B3 tree says
  `enable-method = "psci"`, and its `/psci` node says `method = "smc"`.
- **Serial lines.** On B1 and B2 the alias `serial0` is the mini UART
  (`brcm,bcm2835-aux-uart` at bus address `0x7e215040`) and `serial1` is the
  PL011 with a `bluetooth` child. `stdout-path` is `serial0:115200n8`. On B3
  `stdout-path` is `serial10`, the PL011 at `0x10_7d00_1000`, and `serial0` is
  the PL011 of RP1 at offset `0x30000`, disabled in the tree.
- **Memory.** `/memory@0` has an empty `reg` in the B1 and B2 trees and a
  placeholder of `0x28000000` bytes in the B3 tree. The firmware writes it.
- **Storage.** `mmc0` is `brcm,bcm2835-sdhost` at `0x3f20_2000` on B1,
  `brcm,bcm2711-emmc2` at `0xfe34_0000` on B2, and `brcm,bcm2712-sdhci` at
  `0x10_00ff_f000` on B3. The second controller of each tree carries the
  `wifi@1` child.
- **Network.** `ethernet0` is `usb424,ec00` behind the hub `usb424,9514` on
  the 3B, `usb424,7800` behind two hubs `usb424,2514` on the 3B+,
  `brcm,bcm2711-genet-v5` at `0xfd58_0000` on B2, and `raspberrypi,rp1-gem`
  (`cdns,macb`) at offset `0x100000` of RP1 on B3. The Zero 2 W tree has no
  `ethernet0`.
- **Interrupts.** B1: `brcm,bcm2836-l1-intc` at `0x4000_0000` is the root, and
  `brcm,bcm2836-armctrl-ic` at `0x3f00_b200` hangs from its line 8. The timer
  node uses lines 0, 1, 3 and 2 of the root. B2: `arm,gic-400` with the
  distributor at `0xff84_1000` and the CPU interface at `0xff84_2000`. B3:
  `arm,gic-400` at `0x10_7fff_9000` and `0x10_7fff_a000`. B2 and B3 use the
  timer PPIs 13, 14, 11 and 10.
- **PCIe.** B2: `brcm,bcm2711-pcie` at `0xfd50_0000`, with one memory window
  at CPU address `0x6_0000_0000` (1 GiB, PCI address `0xc000_0000`). B3:
  three `brcm,bcm2712-pcie` hosts, of which only the one at `0x10_0012_0000`
  is enabled. Its memory window is at CPU address `0x1f_0000_0000`, PCI
  address 0. The node `rp1` under it maps `0x410000` bytes there. Two
  `brcm,bcm2712-mip` nodes at `0x10_0013_0000` and `0x10_0013_1000` turn MSI-X
  into SPIs (64 from SPI 128, and 8 from SPI 247).
- **The closure.** `dtnodes.py needs` follows clocks, resets, power domains,
  regulators, pin control, PHYs and DMA from the nodes of the console, the SD
  card, USB, the network and the framebuffer. On the 3B it reaches 17 nodes
  with a `compatible`, on the 4B 18. Beside the devices themselves these are
  `cprman`, `gpio`, `aux`, the DMA controller, `firmware`, `power`, the two
  `dsi` nodes (clock parents of `cprman`), fixed clocks and regulators. This
  is the list of what must answer for a driver to finish its probe (RD).
- `/soc/fb` (`brcm,bcm2708-fb`) is enabled in all three base trees.

### RM2. What QEMU 11.1.3 has (measured on helios)

- Machines: `raspi3ap`, `raspi3b` and `raspi4b` (and the 32-bit `raspi0`,
  `raspi1ap`, `raspi2b`). No Pi 5, no Pi 400, no compute module, no Zero 2 W.
- CPU models `cortex-a53`, `cortex-a72` and `cortex-a76` exist.
- `raspi3b` builds: the two interrupt controllers (`bcm2836-control`,
  `bcm2835-ic`), the system timer, the mailbox with the property channel and
  the framebuffer, the power manager, `cprman`, the GPIO, PL011, the aux
  block, SDHOST, a generic SDHCI, DWC2, the DMA engine, the RNG, thermal, OTP.
  9 regions are "unimplemented". `usb-net`, `usb-kbd`, `usb-tablet` and
  `usb-storage` plug into its DWC2.
- `raspi4b` builds the GIC, two SDHCI (the second at `0x7e34_0000`, the
  EMMC2), DWC2, the mailbox, the framebuffer and the BCM2711 GPIO. It has **no
  GENET, no PCIe host, so no VL805, no RNG200 and no thermal sensor**: 11
  regions are "unimplemented". Its RAM in the map is 2 GiB.

So QEMU is an oracle for all of B1, for the part of B2 that an SD boot with a
serial console and a framebuffer needs, and for nothing of B3 except the CPU.

### RM3. What vmlab-hv has (read in the tree at `6fa31920`)

- The emulator core `vmlab-hv-tcg-aarch64` with one CPU model, `vmlab-v8.0`
  (`sysreg.rs`): Armv8.0-A, EL0 and EL1, AArch64 only, no PMU. The exception
  banks have room for EL2 (`exception.rs`) and nothing runs there.
- The `virt` machine (`vmlab-hv-vmm/src/virt.rs`) with a GICv2 and GICv2m
  (`gic.rs`), PL011, PL031, the GED, a generic ECAM PCI host, the device tree
  builder (`virt/fdt.rs`), PSCI on `HVC` and `SMC` (`virt/psci.rs`), and
  snapshots (`virt/snapshot.rs`). `Machine` has `Microvm`, `Q35`, `Pc` and
  `Virt`.
- USB: an xHCI as a PCI function, UHCI, a keyboard and a tablet. No DWC2, no
  hub, no USB network device, no USB mass storage.
- No SD card and no SD host controller. `DiskBus` has `Virtio`, `Ahci`, `Ide`.
- Network models: virtio, e1000, pcnet.
- The Linux agent opens `/dev/ttyS0` when the guest has no virtio-serial
  controller (`guest/agent/src/unix/linux.rs`, `find_port`).
- The design of #1235 (F2, F4, F6) is on the branch `card-1235` (pull request
  #1100), not on `next`. This document names its parts by the codes of the
  card: F2 (the v8.1 to v8.5 sets) and F4 (the 16K granule).

## 2. The CPU

- **RK1. Three CPU models beside `vmlab-v8.0`**: `cortex-a53`, `cortex-a72`
  and `cortex-a76`, as rows of `CpuModel`. The ID registers, `MIDR_EL1` and
  the cache geometry of each are read from QEMU's model of the same name over
  the gdbstub (the reader of #1235, `measure/idregs.py` there), then reduced
  by RK2. They are not written from memory.
- **RK2. What differs from the real core** (Q1). The emulator has no EL2 and
  no EL3, no AArch32 at EL0, and no PMU. The ID registers say so
  (`ID_AA64PFR0_EL1` EL0 and EL1 AArch64 only, EL2 and EL3 absent, `PMUVer`
  0). The real firmware enters the kernel at EL2. vmlab-hv enters it at EL1,
  which the arm64 boot protocol allows. The guest sees it: KVM inside the
  guest is absent, and a 32-bit (armhf) user program does not run. Raspberry
  Pi OS 64-bit and Ubuntu arm64 have a 64-bit user land. The `/arm-pmu` node
  is removed from the tree that the kernel gets, as V5 of 3d does for `virt`.
- **RK3. The A53 and the A72 need no new instruction set.** Both are
  Armv8.0-A with CRC32, AES, PMULL, SHA1 and SHA2, which is the 3d set (G2).
- **RK4. The A76 needs F2 and F4 of #1235.** A Pi 5 reports these features to
  Linux (*from memory*, the `Features` line of `/proc/cpuinfo`): `atomics`
  (LSE), `fphp` and `asimdhp` (FP16), `asimdrdm` (RDM), `lrcpc`, `dcpop`,
  `asimddp` (DotProd), beside the v8.0 set. It also has PAN, UAO, RAS and
  SSBS. Raspberry Pi OS selects `kernel_2712.img` on a Pi 5, a kernel with
  16K pages, so F4 is needed for RQ1 on B3. Ubuntu for Pi uses 4K pages. The
  exact list is the diff of the ID registers of QEMU's `cortex-a76` against
  `vmlab-v8.0`, made in card RC14.
- **RK5. The counter frequency** (`CNTFRQ_EL0`) is 19.2 MHz on B1 and 54 MHz
  on B2 and B3 (*from memory*, checked against QEMU and the device tree of
  the image in RC1 and RC10). The machine sets it, as V4 of 3d.
- **RK6. Secondary cores.** B1 and B2: the machine puts a stub of its own at
  address 0, a few instructions that we write, where each secondary core
  waits and reads its release address (`0xe0`, `0xe8`, `0xf0`). The kernel
  writes the entry point there and the core jumps to it at EL1. The stub page
  is reserved in the tree (`/memreserve/`). B3: PSCI on `SMC`, which
  `virt/psci.rs` serves already.
- **RK7. The count of vCPUs** is 4 by default. Fewer is allowed (Q7): the
  firmware stage removes the `cpu@n` nodes that have no vCPU.

## 3. The boot path: vmlab-hv as the firmware stage (RF)

A real board runs the boot ROM, the EEPROM or `bootcode.bin`, then
`start.elf` on the VideoCore, which reads `config.txt`, loads the kernel, the
initramfs and the device tree, edits the tree, and starts the ARM cores.
vmlab-hv does the work of that last stage itself, in Rust, at machine build
and again at every reset of the machine, because a guest update writes a new
kernel to the boot partition.

- **RF1. Where.** A module `raspi` of `vmlab-hv-firmware`, pure over a
  `DiskImage` and the board. Its result is a value: the bytes to load with
  their addresses, the edited tree, the entry state of the cores, and a log
  of what it honoured and what it ignored (ADR-0003: a decision as a value).
- **RF2. The partition.** The first FAT partition of an MBR or a GPT table on
  the SD card image. A read-only FAT16 and FAT32 reader with long file names.
  The card RC3 chooses between the `fatfs` crate (MIT) and a small reader of
  our own, and says why.
- **RF3. `config.txt`.** A parser with the conditional filters (`[all]`,
  `[pi3]`, `[pi3+]`, `[pi4]`, `[pi400]`, `[cm4]`, `[pi5]`, `[pi500]`,
  `[cm5]`, `[pi02]`, `[board-type=]`, `[none]`, `[tryboot]`) and `include`.
  Keys honoured: `kernel`, `arm_64bit`, `os_prefix`, `overlay_prefix`,
  `device_tree`, `initramfs` with `followkernel`, `auto_initramfs`,
  `cmdline`, `enable_uart`, `gpu_mem` and its per-size forms, `total_mem`,
  `framebuffer_width`, `framebuffer_height`, `framebuffer_depth`,
  `disable_commandline_tags`. Every other key is named once in the log and
  ignored. A missing `config.txt` is legal: the defaults apply.
- **RF4. The kernel.** `kernel8.img` on B1 and B2. `kernel_2712.img`, then
  `kernel8.img`, on B3. A gzip file is unpacked. The arm64 image header gives
  the load offset and the size. A 32-bit kernel (`arm_64bit=0`, or no 64-bit
  kernel in the partition) is refused with an error that names the file.
- **RF5. The edits of the tree**, as the real firmware makes them (RS3):
  `/memory@0` from the RAM of the machine less `gpu_mem`, `/chosen/bootargs`
  (the `bootargs` of the tree, then what the firmware adds, then
  `cmdline.txt`), `linux,initrd-start` and `-end`, `kaslr-seed` and
  `rng-seed`, `/serial-number`, `/system` (`linux,revision`,
  `linux,serial`), `local-mac-address` of `ethernet0`, the reserved page of
  RK6, and on B2 the `dma-ranges` that depend on the RAM size.
- **RF6. Edits that only vmlab-hv makes** (Q3). The machine has no radio. The
  `wifi` and `bluetooth` nodes get `status = "disabled"`, the PL011 becomes
  `serial0` and the console, and the second UART is enabled for the agent:
  the mini UART on B1 and B2, the PL011 of RP1 on B3. This is what the
  overlay `disable-bt` and `enable_uart=1` do on a real board. A property
  `/chosen/vmlab,agent-uart` names the node of the agent's UART (RV3).
- **RF7. `dtparam` and `dtoverlay`** are not applied in the first cards. Each
  such line is named in the log. Card RC18 adds the overlay merge and the
  parameter mechanism, with a list of overlays that are never applied because
  they turn on hardware that RQ3 leaves out: `vc4-kms-v3d`, `vc4-fkms-v3d`,
  and what `camera_auto_detect` and `display_auto_detect` would load. Without
  `vc4-kms-v3d` the kernel keeps the firmware framebuffer of the base tree.
- **RF8. Direct boot for tests.** `Boot::Kernel` with a device tree file loads
  exactly the given bytes and edits nothing. The oracle runs use it (RO2).
- **RF9. Reset and power-off.** B1 and B2: a write of the full-reset command
  to the power manager (`PM_RSTC`) resets the machine in place, and the
  firmware stage runs again. The halt convention of the Pi kernel (the
  partition field of `PM_RSTS` set to 63 before the reset) ends the machine
  as a power-off. The watchdog timeout does the same reset. B3: PSCI
  `SYSTEM_RESET` and `SYSTEM_OFF`, and the same power manager.

## 4. The devices (RD)

Three tiers. **T1**: a working model with a snapshot and tests. **T2**: a
register file that keeps what is written and reads what lets the driver
finish its probe (a lock bit set, a busy bit clear, a level of 0). **T3**:
absent, which reads 0, ignores writes and logs each first access (G3f of 3d).
A node of the base tree that is in no table below is T3.

### RD1. The VideoCore mailbox, in all three groups (T1)

The mailbox registers (read, status, write, the interrupt) and the property
channel 8. vmlab-hv answers each tag as the firmware would:

- Identity: firmware revision, board model, board revision (composed from the
  public revision-code fields: memory size, manufacturer, processor, type,
  revision), MAC address, serial number, ARM memory, VideoCore memory, the
  command line, DMA channels.
- Clocks: state, rate, minimum and maximum rate, turbo. The values are those
  that the board has (*from memory*, checked against QEMU in RC1): they back
  `raspberrypi,firmware-clocks`.
- Power: the state of a device, and the domain state that
  `raspberrypi,bcm2835-power` sets. Each is stored and read back.
- The firmware GPIO expander (`raspberrypi,firmware-gpio`): state and
  configuration stored and read back. The SD supply regulators hang from it.
- Voltage, temperature, maximum temperature, throttled: constants.
- OTP: the customer rows read 0. A write is refused.
- The framebuffer tags (RD7). `NOTIFY_XHCI_RESET` answers success (RD4).
  `VCHIQ_INIT` answers failure, so no audio, camera or codec service starts.
  The reset notification is accepted. On B3 the real-time-clock tags answer
  from the clock of the machine (`raspberrypi,rpi-rtc`).
- An unknown tag gets the response code of a tag that was not processed and
  one log line. RC1 records every tag that the two images send in each group,
  and the list in the code is that record.

### RD2. Group B2: BCM2711 (first)

| Device | Address | Tier | Note |
|---|---|---|---|
| GIC-400 | `0xff84_1000`, `0xff84_2000` | T1, exists | `gic.rs` is a GICv2. No GICv2m frame. |
| Generic timer | PPI 13, 14, 11, 10 | T1, exists | |
| ARM-local block | `0xff80_0000` | T2 | The tree names it. The GIC is the interrupt root. |
| System timer | `0xfe00_3000` | T1, new | A free-running 1 MHz counter and four compares. |
| Mailbox | `0xfe00_b880` | T1, new | RD1. |
| Power manager and watchdog | `0xfe10_0000` | T1, new | RF9. |
| `cprman` | `0xfe10_1000` | T2 | Lock bits set, busy bits clear, dividers kept. |
| GPIO | `0xfe20_0000` | T2 | Function selects kept, levels 0 (RQ1). |
| PL011 | `0xfe20_1000` | T1, exists | The console after RF6. |
| Aux block and mini UART | `0xfe21_5000`, `0xfe21_5040` | T1, new | The agent's port. |
| EMMC2 (SDHCI) | `0xfe34_0000` | T1, new | With the SD card model. The card offers 3.3 V only, so the driver never switches the supply. |
| SDHCI of the radio | `0xfe30_0000` | T2 | No card present. |
| PCIe host | `0xfd50_0000` | T1, new | Not an ECAM: an index register selects the function, a data window reads its configuration space. Link-up status, the bridge reset, one outbound window, and its own MSI controller of 32 vectors. A front end of the existing `PciRoot`. |
| VL805 xHCI | bus 1 of the PCIe host | T1, exists | The existing xHCI function with the IDs of the VL805 (`1106:3483`). Pi 4B and Pi 400. |
| xHCI of the SoC | `0xfe9c_0000` (*from memory*) | T1, new front end | CM4, which has no VL805: the same xHCI core behind an MMIO front end. RC7 checks the address and which USB host the CM4 tree and the default `config.txt` of the images select. |
| DWC2 | `0xfe98_0000` | T3 | The USB-C port in device mode. Not used as a host on the 4B. |
| GENET v5 | `0xfd58_0000` | T1, new | With MDIO and a Gigabit PHY that reports link up. Not in QEMU (RM2). |
| RNG200 | `0xfe10_4000` | T1, new | Small. It gives the kernel entropy early. |
| AVS monitor and thermal | `0xfd5d_2000` | T2 | A constant temperature. |
| DMA engines | `0xfe00_7000`, `0xfe00_7b00` | T2 | The EMMC2 driver uses its own ADMA. |
| VCHIQ, HEVC decoder, DSI, HDMI, HVS, V3D, I2C, SPI, PWM, I2S | | T3 | RQ1 and RQ3. |

RAM: 1, 2, 4 or 8 GiB, the sizes that the boards have, because the revision
code holds the size (Q6). The map is RAM from 0 to `0xfc00_0000` and again
from `0x1_0000_0000`. The top `gpu_mem` megabytes of the first gigabyte are
backed by RAM and are not in `/memory`: the framebuffer lives there.

### RD3. Group B1: BCM2837 (second)

| Device | Address | Tier | Note |
|---|---|---|---|
| ARM-local controller (`bcm2836-l1-intc`) | `0x4000_0000` | T1, new | Per core: the routing of the four timer lines, the four mailboxes that carry the IPIs, the pending registers. The local timer. |
| Legacy controller (`bcm2836-armctrl-ic`) | `0x3f00_b200` | T1, new | Three pending registers, enable and disable banks. Its output is line 8 of the local controller. |
| Generic timer | lines 0, 1, 3, 2 of the local controller | T1, exists | |
| System timer, mailbox, power manager, PL011, aux and mini UART, `cprman`, GPIO | `0x3f00_3000` and on | as RD2 | The same models at the B1 base. The B1 forms of `cprman` and GPIO. |
| SDHOST | `0x3f20_2000` | T1, new | The SD card of B1. It shares the SD card model with the EMMC2. |
| DMA engine | `0x3f00_7000` | T1, new | The SDHOST driver moves its blocks through a DMA channel when the tree gives one, and the tree does (RM1). |
| Arasan SDHCI of the radio | `0x3f30_0000` | T2 | No card present. |
| DWC2 in host mode | `0x3f98_0000` | T1, new | Control, bulk and interrupt transfers. No isochronous transfer, no device mode. |
| USB hub | on the DWC2 | T1, new | LAN9514 (`0424:9514`) on the 3B, two LAN7515 hubs (`0424:2514`) on the 3B+. |
| USB network | behind the hub | T1, new | The LAN9514 function (`0424:ec00`, driver `smsc95xx`) on the 3B. The LAN7800 (`0424:7800`, driver `lan78xx`) on the 3B+. |
| USB keyboard and tablet | behind the hub | T1, exist | The models of `usb/keyboard.rs` and `usb/tablet.rs` on a second host controller. |
| RNG | `0x3f10_4000` | T1, new | Small. |
| Thermal | `0x3f21_2000` | T2 | A constant. |
| VCHIQ, DSI, HDMI, HVS, V3D, I2C, SPI, PWM, I2S | | T3 | |

RAM: 1 GiB, and 512 MiB on the 3A+ and the Zero 2 W. Peripherals from
`0x3f00_0000`, so RAM ends there.

The 3A+, the Zero 2 W, and the CM3 and CM3+ on their IO board have no wired
network. Their machine gets the LAN9514 network function as a USB adapter on
the DWC2 port, so that a lab network reaches them (Q4). The 3A+ and the Zero
2 W have no hub on the board: the machine plugs a generic USB 2.0 hub when it
needs more than one USB device.

### RD4. Group B3: BCM2712 and RP1 (third)

| Device | Address | Tier | Note |
|---|---|---|---|
| GIC-400 | `0x10_7fff_9000`, `0x10_7fff_a000` | T1, exists | |
| Generic timer, PSCI on `SMC` | | T1, exist | |
| PL011 (the debug UART) | `0x10_7d00_1000` | T1, exists | The console (`serial10`). |
| Mailbox | `0x10_7c01_3880` | T1 | RD1, with the RTC tags. |
| System timer | `0x10_7c00_3000` | T1 | As RD2. |
| SDHCI (`brcm,bcm2712-sdhci`) | `0x10_00ff_f000` | T1 | The SDHCI model of RD2 with the second register range of the brcmstb form (T2). |
| SDHCI of the radio | `0x10_0110_0000` | T2 | No card present. |
| Power manager and watchdog (`bcm2712-pm`) | `0x10_7d20_0000` | T1 | |
| RNG200 | `0x10_7d20_8000` | T1 | As RD2. |
| PCIe host (`brcm,bcm2712-pcie`) | `0x10_0012_0000` | T1, new | The family of RD2 with the BCM2712 registers and no MSI controller of its own. |
| MIP (`brcm,bcm2712-mip`) | `0x10_0013_0000`, `0x10_0013_1000` | T1, new | An `MsiSink` that raises an SPI of the GIC per vector. |
| Reset controllers (`brcmstb-reset`, `bcm7216-pcie-sata-rescal`) | `0x10_0150_4318`, `0x10_0011_9500` | T2 | The calibration reports done at once. |
| Level-2 interrupt controllers, the two `brcmstb-gpio`, the two pin controllers, the DVP clock, AVS and thermal | | T2 | |
| The three IOMMUs, the DMA engines, the HEVC decoder, the ISP back end, the SPI, the Bluetooth UART | | T3 | The firmware stage disables what would wait for hardware (RF6). |
| **RP1**, PCI function `1de4:0001` on bus 1 | BAR 1 at CPU address `0x1f_0000_0000` | T1, new | One PCI function with MSI-X. Its vectors are the interrupt numbers of the `rp1` node: 6 for the network, 31 and 36 for USB, 0 to 2 for GPIO. |
| RP1 clocks | offset `0x18000` | T2 | The PLLs report lock. |
| RP1 GPIO and pads | offset `0xd0000` | T2 | |
| RP1 network (Cadence GEM, `cdns,macb`) | offset `0x100000` | T1, new | With a Gigabit PHY that reports link up. |
| RP1 USB, two DWC3 hosts | offsets `0x200000`, `0x300000` | T1, new front end | The existing xHCI core at the start of each range, and the few DWC3 global registers that the host-mode probe reads and writes. Keyboard and tablet plug in here. |
| RP1 PL011 `uart0` | offset `0x30000` | T1, exists | The agent's port (RF6). |
| RP1 mailbox, shared memory, PIO, ADC, AXI DMA, I2C, SPI, PWM, I2S, the second SD hosts | | T3 | |

RAM: 2, 4, 8 or 16 GiB, from 0 without a hole, because the peripherals are
above `0x10_0000_0000`.

The second and third PCIe hosts of the SoC are disabled in the base tree. The
external connector of a Pi 5 (an NVMe drive) is not in this phase.

### RD5. The SD card (T1, new, all groups)

One model of an SD memory card in SD mode: the identification and transfer
states, CID, CSD (version 2 for a card above 2 GiB), SCR, the status
registers, single and multiple block read and write, erase, the switch
function command with high speed, and no 1.8 V signalling. It sits on a
`DiskImage`, so a qcow2 overlay and snapshots work as for any disk. Two host
controllers drive it: the SDHCI (B2, B3) and SDHOST (B1). `DiskBus` gains
`Sd`. The size of the image is rounded up to what a card can report.

### RD6. USB mass storage (not in this phase)

A USB disk needs a mass-storage model that vmlab-hv does not have. Boot from
USB is not needed for RQ1. It is listed under "Later".

### RD7. The framebuffer (T1, new, all groups)

- The tags: allocate and release the buffer, physical and virtual size, depth
  (16, 24 and 32 bits per pixel, and 8 with the palette tags), pixel order,
  alpha mode, pitch, virtual offset, overscan, blank, and the number of
  displays (one). The buffer is in the `gpu_mem` part of RAM. The address in
  the answer carries the bus alias that the board uses.
- The first size is `framebuffer_width` and `framebuffer_height` of
  `config.txt`, else the display size of the machine description, else
  1024 x 768.
- The frame source for the native frame protocol (`vmlab-hv-display`) reads
  the buffer from guest RAM at the frame rate and sends the tiles that
  changed since the last frame. A mode change through the tags becomes a
  resize of the frame protocol. `DisplayDevice` gains a variant for it.
- Input for the desktop is the USB keyboard and tablet of each group.
- **RU1 decides the desktop.** `bcm2708_fb` gives the kernel `/dev/fb0` and
  the text console. A Wayland compositor needs a DRM device. If the kernels
  of the two images have `simpledrm`, the firmware stage also puts a
  `simple-framebuffer` node under `/chosen`, as recent firmware does, and the
  desktop runs on it with software rendering. If they do not, the desktop of
  RQ3 runs under X11 on `fbdev` or needs the KMS phase. RC4 measures it.

## 5. What the VMM lacks (RG)

- **RG1. A third machine family.** `raspi.rs` beside `virt.rs`, with one
  table per board: the map, the interrupt wiring, the device set, the CPU
  model, the RAM sizes, the revision code, the name of the tree. `Machine`
  gains one variant that carries the board. The devices are shared across the
  groups wherever the tables above say "as RD2".
- **RG2. Interrupt controllers as device models in the process**, two more
  after the GICv2: the ARM-local controller, which drives the IRQ and FIQ
  line of each vCPU, and the legacy controller.
- **RG3. A PCI host that is not an ECAM**, as a front end of `PciRoot`, in
  two register forms (BCM2711, BCM2712), and the MIP as an `MsiSink`.
- **RG4. The xHCI core apart from its PCI function**, so that an MMIO front
  end (the SoC xHCI of the CM4, the DWC3 of RP1) can hold it.
- **RG5. USB devices apart from the xHCI**: the keyboard and the tablet on
  the DWC2, a hub, and the two network functions. The smallest change to the
  device trait of `usb/mod.rs` that works, named in the card report.
- **RG6. The firmware stage** (RF) and a boot form for it in `Boot`.
- **RG7. Snapshots** of every new device and of the machine (as
  `virt/snapshot.rs`), reset in place, and the debug port.

No card of this phase needs KVM. The machines run on the emulator on an
x86_64 host.

## 6. The proof plan (RO)

- **RO1. Register answers against QEMU, per device.** For each T1 and T2
  device that QEMU has (RM2), a test drives the same register sequence into
  QEMU through qtest and into our model, and compares the answers, as
  `virt_oracle.rs` does for `virt`. The qtest run needs QEMU, so it is a
  recipe outside `just ci::check`. It writes a vector file that is committed,
  and a unit test inside `just ci::check` replays the vectors without QEMU.
- **RO2. Boot lockstep against QEMU** (O4 of 3d), for B1 on `raspi3b` and
  for B2 on `raspi4b` up to the first device that QEMU lacks. Both sides get
  the same kernel and the device tree that QEMU hands its guest, dumped from
  QEMU's memory at the kernel entry and given to our machine through RF8.
  The compare starts at the kernel entry. QEMU enters at EL2 and we enter at
  EL1 (RK2), so RC1 first measures where the two runs meet: at the kernel's
  drop to EL1, or from the first instruction with QEMU held at EL1 by the
  options of its machine.
- **RO3. The log of the guest.** The same image boots on QEMU and on
  vmlab-hv with `ignore_loglevel`. The driver lines of the two kernel logs
  are compared after normalising times and addresses. A T1 device must have
  no probe error. The allow-list holds each expected difference with its
  reason (RK2, RF6, the T3 devices).
- **RO4. Real images, at seam S1.** Raspberry Pi OS Lite 64-bit, Raspberry Pi
  OS with desktop 64-bit, and Ubuntu Server for Pi, each pinned by URL and
  sha256 in a file of the tests and fetched to the cache of the test host.
  The test boots the unmodified image through the firmware stage to a login
  on the serial console, then to agent ready with the first-boot files of
  RV2. These tests are in `just ci::hv-gate`, not in `just ci::check`: an
  image is more than 1 GiB and a boot takes minutes on the emulator.
- **RO5. B3 has no machine oracle.** Its proof is: the CPU sets in lockstep
  against QEMU `-cpu cortex-a76` on `virt` (the method of #1235), unit tests
  of each device against the documents and the vectors of the shared models,
  RO3 with the allow-list but without the QEMU side, and RO4. If the operator
  has a Pi 5, its `dmesg`, its `/proc/cpuinfo` and its device tree from
  `/proc/device-tree` are the reference for RO3 (Q5).
- **RO6. The firmware stage** has unit tests on FAT images that the test
  builds: every filter, every honoured key, each kernel form, each tree edit
  compared field by field with the tree that QEMU or a real board hands over.
- **RO7. The manual boot checklist**, per board, run by a person before the
  group is called done:
  1. The serial console shows the kernel log from its first line.
  2. The root file system mounts from the SD card and first boot resizes it.
  3. A login prompt comes on the serial console and on the framebuffer.
  4. The network interface gets an address from the lab's DHCP and `apt
     update` runs.
  5. The keyboard types at the framebuffer console and the pointer moves on
     the desktop.
  6. `vcgencmd get_throttled` and `cat /proc/cpuinfo` answer, with the board
     name and revision of the machine.
  7. `reboot` comes back to the login, and `poweroff` ends the machine.
  8. A snapshot is taken while a file copy runs, restored, and the copy ends
     with the right checksum.
  9. The agent answers, and `vmlab exec` runs a command.
  10. All four cores are online (`nproc`).

## 7. The vmlab side (RV)

- **RV1. The declaration.** A machine names its board (`board = "pi4b"`, one
  of the twelve). Validation refuses a RAM size that the board does not have,
  a second disk, a NIC model, a `gpu {}`, `audio`, and firmware settings on
  such a machine. `describe.rs` composes the description. The hypervisor is
  vmlab-hv, with no QEMU fallback.
- **RV2. The image and the agent.** A template is made from a published image
  (`.img.xz`), which becomes the SD card. The agent must get into a guest
  that has no CD drive. The route that keeps the image as published is the
  one that Raspberry Pi Imager and Ubuntu use: files written to the FAT boot
  partition before the first boot (cloud-init `user-data` and `meta-data` on
  Ubuntu, and the first-run mechanism of Raspberry Pi OS). The build writes
  the agent binary and an install unit there. Q4 asks whether this counts as
  "unmodified". RU2 is which mechanism the current Raspberry Pi OS image has.
- **RV3. The agent's port.** The agent has no virtio-serial here. It reads
  `/sys/firmware/devicetree/base/chosen/vmlab,agent-uart`, finds the tty of
  that node, and serves on it with the framing of its serial fallback. On B1
  and B2 that tty is `/dev/ttyS0`, which the fallback opens today. The host
  side connects to the socket of that UART.
- **RV4. Profiles.** A guest profile for Raspberry Pi OS and one for Ubuntu
  for Pi, with `agent_transport` of the new kind.
- **RV5. The guest clock.** B1 and B2 have no clock chip. `clock = "host"`
  sets the time through the agent after the handshake (`boot_clock.rs`). B3
  reads the time from the RTC tags.

## 8. Unknowns, and the card that settles each (RU)

- **RU1.** Whether the desktop of each image comes up on the firmware
  framebuffer alone, and whether its kernel has `simpledrm`. RC4.
- **RU2.** The first-boot mechanism of the current Raspberry Pi OS image. RC8.
- **RU3.** The instruction count of each image to agent ready, and the boot
  time on the emulator. Not measured here: no image was fetched. RC1 counts
  it on QEMU `raspi4b` with the plugin of 3d, and sets the time limit of RO4.
- **RU4.** Whether a driver of a T3 or disabled device delays the boot by a
  timeout (VCHIQ, the radio SDHCI). RC2, from the log of RO3.
- **RU5.** The full list of mailbox tags that the images send. RC1.
- **RU6.** Which registers the RP1 clock driver and the BCM2712 PCIe driver
  wait on. RC15.
- **RU7.** Which keys the `config.txt` of each image has, and whether Ubuntu
  uses `os_prefix`. RC3 reads the two images and adds their files as test
  fixtures (text files only).

## 9. Cards (RC)

Every card ends with `just ci::check` at exit 0, lists the sources that it
read (RS1 to RS7), and keeps X1 to X3. "With" names the cards that can run at
the same time. The order is B2, then B1, then B3 (RQ2).

| Card | Title | What to do | Proof | After | With |
|---|---|---|---|---|---|
| RC1 | raspi B2: the Pi 4B machine to a kernel log on the serial console | RG1 for the 4B. The `cortex-a72` model (RK1, RK2). The map of RD2 with the GIC, the timer, PL011, aux and mini UART, system timer, power manager, RNG200, the T2 files. The mailbox with the tags of RD1 except the framebuffer. The stub of RK6. RF8. Measure RU3 and RU5. | RO1 for each device that QEMU has. RO2 from the kernel entry to the first access to a device that this card lacks. S1: the kernel of Raspberry Pi OS prints its log and stops at "no root device". | none | none |
| RC2 | raspi: the SD card and the SDHCI of the BCM2711 | RD5, the EMMC2 of RD2, `DiskBus::Sd`. RU4. | RO1 for the SDHCI and the card. S1: with RF8, Raspberry Pi OS Lite mounts its root and reaches the login on the serial console. A snapshot test with the card in use. | RC1 | RC3, RC4 |
| RC3 | raspi: the firmware stage | RF1 to RF6 and RF9, RG6. RU7. | RO6. S1: the unmodified Raspberry Pi OS Lite and Ubuntu Server images boot to the login with no kernel and no tree given from outside. A reboot inside the guest comes back. | RC1 | RC2, RC4 |
| RC4 | raspi: the framebuffer through the mailbox | RD7. RU1. | RO1 for the tags. S1: a screenshot of the text console matches the text. A mode change resizes the frame stream. The answer to RU1 in the report. | RC1 | RC2, RC3 |
| RC5 | raspi B2: the PCIe host and the VL805 | RG3 for the BCM2711, the VL805 IDs on the xHCI function, `NOTIFY_XHCI_RESET`, the keyboard and the tablet. | Unit tests of the host from RS5. S1: the kernel lists the xHCI, and keys typed through the control socket arrive at the console. RO3 without the QEMU side. | RC1 | RC2, RC3, RC4, RC6 |
| RC6 | raspi B2: the GENET network device | GENET v5, MDIO and the PHY of RD2, on the net backends that the other NICs use. | Unit tests from RS5. S1: the guest gets a DHCP lease and passes a 100 MiB transfer with a checksum in both directions. | RC1 | RC5 |
| RC7 | raspi B2: Pi 400 and CM4, snapshots and reset | The two board tables, the MMIO xHCI front end for the CM4 (RG4), RG7 for B2, the RAM sizes and revision codes. | The S1 snapshot tests on each board, online and offline. RO4 for the three boards. | RC2 to RC6 | RC8 |
| RC8 | vmlab: declare, describe and reach a Raspberry Pi machine | RV1 to RV5: the schema, validation, `describe.rs`, the template from an image, the first-boot files, the agent's port, the profiles, the manual and the PRD. RU2. | An e2e scenario: a lab with a Pi 4B reaches agent ready and `vmlab exec` runs. `just ci::check`. | RC3 (the port), RC7 (the scenario) | RC10 |
| RC9 | raspi B2: acceptance | What the boots still miss. RO3 and RO4 for the three images on the three boards, and RO7. | The checklist of RO7 per board in the report, and RO4 green in `ci::hv-gate`. | RC7, RC8 | RC10 |
| RC10 | raspi B1: the Pi 3B machine with its two interrupt controllers | RG2, the `cortex-a53` model, the map of RD3 with the shared devices at the B1 base. | RO1 for both controllers. RO2 on `raspi3b` from the kernel entry to the first missing device. | RC1 | RC5 to RC9 |
| RC11 | raspi B1: SDHOST and the DMA engine | The two models of RD3 on the SD card of RC2. | RO1 for both. RO2 to the mount of the root. S1: Raspberry Pi OS Lite reaches the login. | RC2, RC10 | RC12 |
| RC12 | raspi B1: the DWC2 host, the hub, and the USB network devices | RG5. DWC2 in host mode, the hub, the LAN9514 and LAN7800 functions, the keyboard and the tablet on the DWC2. | RO1 for the DWC2. S1: a DHCP lease and the transfer test of RC6. Keys arrive at the console. | RC10 | RC11 |
| RC13 | raspi B1: the six boards and acceptance | The board tables of the 3B+, 3A+, CM3, CM3+ and Zero 2 W, the adapter of Q4, RG7 for B1, the vmlab side for B1. | RO2 for Raspberry Pi OS Lite on `raspi3b` from the kernel entry to the login without a CPU difference. RO3, RO4 and RO7 per board. | RC8, RC11, RC12 | RC14 |
| RC14 | raspi B3: the Pi 5 machine to a login on the serial console | The `cortex-a76` model (RK4), the map of RD4 without PCIe: GIC, PSCI on `SMC`, the debug PL011, the mailbox with the RTC tags, the SDHCI in its brcmstb form, the power manager, the T2 files. | The ID registers against QEMU's `cortex-a76`, with the differences of RK2 in the allow-list. S1: Ubuntu Server for Pi (4K pages) and Raspberry Pi OS Lite (`kernel_2712.img`, 16K pages) reach the login. | RC2, RC3, and F2 and F4 of #1235 | RC10 to RC13 |
| RC15 | raspi B3: the PCIe host, the MIP and the RP1 function | RG3 for the BCM2712, the MIP, RP1 as a PCI function with MSI-X, its clocks and GPIO as T2. RU6. | Unit tests. S1: the `rp1` driver binds and the kernel log has no probe error for it. | RC5, RC14 | RC16 |
| RC16 | raspi B3: the network, USB and the agent UART of RP1 | The Cadence GEM with its PHY, the DWC3 front end on the xHCI core (RG4), the PL011 of RP1. | Unit tests from RS4. S1: a DHCP lease, the transfer test, keys at the console, the agent on the UART. | RC15, RC7 (RG4) | none |
| RC17 | raspi B3: Pi 500 and CM5, snapshots, the vmlab side, acceptance | The two board tables, RG7 for B3, the framebuffer on B3, the vmlab side for B3. | RO3, RO4, RO5 and RO7 per board. | RC4, RC8, RC16 | none |
| RC18 | raspi: `dtparam` and `dtoverlay` in the firmware stage | RF7: the overlay merge, the parameters of the Raspberry Pi trees, the list of overlays that are never applied. | RO6 with overlays from the images. A `dtoverlay=disable-bt` line gives the tree that RF6 gives. | RC3 | any |

RC1 is the card that proves the path: one board, a kernel log on the serial
console, the three oracle tools, and the two measurements that set the limits
of the later cards.

Later, with no card here: KMS with the HVS, the pixel valves, HDMI and V3D
(the later phase of RQ3). Audio. The radios. GPIO, I2C and SPI functions. The
camera and the codecs. USB mass storage and boot from USB. The external PCIe
connector of the Pi 5. EL2 for KVM inside the guest. AArch32 at EL0. A PMU.

## Questions for the operator

Each has a recommendation. The design follows the recommendation until the
operator answers.

- **Q1.** The kernel is entered at EL1, and the CPU has no EL2, no EL3, no
  AArch32 at EL0 and no PMU (RK2). So RQ4 holds for the instruction sets and
  the identity of the core, not for these four. Recommended: accept. EL2 is
  a phase of its own.
- **Q2.** May a card read Linux driver source (GPL-2.0) under the device rule
  of `hv/AGENTS.md`, as it may read QEMU device source, where RS1 to RS5 give
  nothing (RS7)? Recommended: yes. Without it the BCM2712 PCIe host, the MIP
  and the RP1 clocks have no readable description at all.
- **Q3.** The firmware stage disables the radio nodes and moves the console
  to the PL011 (RF6). The image is not changed, the tree that the kernel gets
  is. Recommended: accept.
- **Q4.** Two points on "unmodified". The build writes first-boot files into
  the boot partition to install the agent (RV2). The boards without a wired
  network get a USB network adapter (RD3). Recommended: accept both.
- **Q5.** Does the operator have a Pi 5, a Pi 4 and a Pi 3 from which to take
  `dmesg`, `/proc/cpuinfo` and `/proc/device-tree` once (RO5)? B3 has no
  other reference for its devices.
- **Q6.** RAM is one of the sizes that the board has, and another size is a
  validation error. Recommended: accept. The revision code cannot express
  another size.
- **Q7.** Fewer than 4 vCPUs is allowed (RK7). Recommended: accept, default 4.

## How to measure again

```sh
# RM1: the device trees. Fetch one blob of raspberrypi/firmware, boot/, into
# an empty directory. No blob is committed.
python3 -I measure/dtnodes.py counts bcm2711-rpi-4-b.dtb
python3 -I measure/dtnodes.py nodes  bcm2711-rpi-4-b.dtb
python3 -I measure/dtnodes.py needs  bcm2711-rpi-4-b.dtb \
    /emmc2bus/mmc@7e340000 /scb/pcie@7d500000 /scb/ethernet@7d580000 \
    /soc/serial@7e201000 /soc/serial@7e215040 /soc/fb
python3 -I measure/dtnodes.py needs  bcm2710-rpi-3-b.dtb \
    /soc/mmc@7e202000 /soc/usb@7e980000 /soc/serial@7e201000 \
    /soc/serial@7e215040 /soc/fb

# RM2: what QEMU has.
qemu-system-aarch64 -machine help | grep raspi
qemu-system-aarch64 -cpu help | grep -E 'cortex-a(53|72|76)'
(echo info qtree; echo info mtree -f; echo quit) | \
    qemu-system-aarch64 -machine raspi4b -display none -S -monitor stdio
```
