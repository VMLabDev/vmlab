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
