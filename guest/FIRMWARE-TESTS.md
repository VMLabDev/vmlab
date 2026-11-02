# Firmware validation

## FW1: selective UEFI connection

Measurements on 2026-11-02 used the same stopped Ubuntu 24.04.4 clone as a
read-only qcow2 backing file, a fresh overlay and the same copied existing VARS
for each boot. Both builds are RELEASE with edk2 networking enabled. Each VM
has two vCPUs, 4 GiB RAM and one NIC. Timing starts when the host sends CPU
resume. UEFI handoff is the first `BdsDxe: starting Boot` serial message
(10 ms polling); agent-ready is daemon status (50 ms polling). Three samples
per cell are summarized by the median. This is a shared host, not an isolated
performance benchmark.

| Hypervisor | UEFI handoff before | UEFI handoff FW1 | Agent-ready before | Agent-ready FW1 |
| --- | ---: | ---: | ---: | ---: |
| QEMU | 1.599 s | 1.731 s | 7.246 s | 7.256 s |
| vmlab-hv | 0.773 s | 0.628 s | 5.824 s | 5.285 s |

Selective connection improves this vmlab-hv sample but regresses QEMU's firmware
handoff by 133 ms. There is no demonstrated QEMU end-to-end improvement from
FW1 alone. The timeout was already zero before the change; the patch prevents
host menu settings from adding a delay. It does not remove an existing delay
from the default configuration.

Focused checks: the two CODE builds complete, their generated blank VARS match
Debian byte for byte, the QEMU firmware/cmdline tests pass, and the vmlab-hv
description tests pass. All six candidate Ubuntu boots reach agent-ready.
Stock/custom OVMF without the feature manifest receives no connection hints.

Raw runs are under `.scratch/firmware-followups/`: `baseline-qemu-2` through
`baseline-qemu-4`, `baseline-hv-fixed3`, `baseline-hv-4`, `baseline-hv-5`,
`fw1-qemu-1` through `fw1-qemu-3`, and `fw1-hv-1` through `fw1-hv-3`.
Each successful run retains its result, event timestamps, guest proof and
screenshot. Early harness failures are not included. The early QEMU baseline
runs required explicit cleanup after force-stop; that harness defect was fixed
before the candidate runs. Source disk modification time and VARS hash remained
unchanged. Test VMs were limited to six host-wide and scoped to 24 GiB with
swap disabled.

## FW2: network-free OVMF and iPXE

All four RELEASE CODE variants build with unchanged flash sizes and a generated
blank VARS identical to Debian's. A clean iPXE rebuild produces identical hashes
for all four combined ROMs, both individual virtio ROMs and the corresponding
source archive. The build uses GCC 13.3.0 and binutils 2.42 in the build container.

QEMU boots both SeaBIOS and network-free OVMF through the built virtio iPXE ROM,
obtains a DHCP boot URL, fetches an HTTP script and executes its unique success
marker. Runs: `ipxe-bios-2` and `ipxe-uefi-minimal2`. EFI console and iPXE serial
output duplicate characters on COM1; the assertion recognizes that actual output.
The first UEFI assertion timed out despite the HTTP fetch and marker; it is not
counted as a passing test.

Ubuntu reaches agent-ready using network-free CODE on QEMU and vmlab-hv, and
using the native-network variant selected through `firmware_network_stack` on
QEMU (`fw2-qemu`, `fw2-hv`, `fw2-native`). QEMU and configuration focused tests
pass. `wcl check src/config/schema.wcl` and `wcl check docs/manual/main.wcl` pass.

## FW3: reproducible SeaBIOS

The existing Ubuntu image is UEFI-only: stock SeaBIOS stops at `Booting from
Hard Disk`. A disposable overlay installs `grub-pc-bin`, reuses its former ESP
as a BIOS boot partition, and masks that ESP's mount. The source disk is never
changed. All BIOS timing runs use fresh overlays of this same prepared disk.
The baseline uses the logo PR's host SeaBIOS and one-second splash; the candidate
uses the new bundled image, zero wait and the built iPXE ROM. OS configuration
is identical between the two groups. CPU-resume to daemon agent-ready has a
median of **6.836 s before, 5.137 s after** over three boots per group. Ranges
are 6.457–7.765 s before and 4.937–5.275 s after on this shared host.

Runs: `baseline-bios-1` through `baseline-bios-3` and `fw3-bios-1` through
`fw3-bios-3`. `fw3-bios-hv` also reaches agent-ready on vmlab-hv. The POST
capture `bios-post-capture/0099.png` shows the vmlab splash with a zero host
menu wait. No minimum splash duration is added.

Two clean builds produce identical BIOS images, expanded configs, source
archive and BUILDINFO. The first source-archive comparison failed because
Python bytecode embedded temporary build paths; disabling bytecode and
excluding its cache resolves the same comparison. Focused QEMU and vmlab-hv
tests pass. The manual validates. The legacy image builds with IDE/floppy
drivers, but a legacy guest boot was not exercised here.
