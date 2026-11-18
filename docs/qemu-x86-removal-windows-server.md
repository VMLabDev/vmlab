# Windows Server template verification for x86 QEMU removal

Card VML-84 covers the Desktop Experience and Server Core templates for
Windows Server 2019, 2022, and 2025. Each template is built on vmlab-hv. The
same sealed version is then booted from a fresh clone on each hypervisor.

The helios run uses vmlab, vmlab-hv, and the Windows guest agent from vmlab
`next` at `aaed53eccda3a381c7acf1dcb97363e6855900c4`, QEMU 11.1.3, and
VMLab-Templates `next` at `a4cf3fb558bf9379ae8d78bb104982b45c617dba`.
Each build runs `./fetch-deps.sh` and then
`VMLAB_SKIP_UPDATES=1 vmlab template build <template> --version <version>`
in its template directory. The private host configuration selects vmlab-hv
for builds and uses the shared OVMF firmware. Builds run serially without a
GPU. The setup ISOs are checked against the SHA-256 digests pinned in the
template definitions.

Boot time is wall-clock time for `vmlab up --wait`. A passing boot also
requires the requested hypervisor in `vmlab status -v` and a successful
`vmlab exec server -- cmd.exe /c ver`. These are single runs on a shared
host.

| Windows Server template | Version | Build on vmlab-hv | Boot on vmlab-hv | Boot on QEMU |
| --- | --- | --- | --- | --- |
| 2019 Desktop Experience | `17763.737.8` | Pass, 2,879 s | Pass, 345 s | Pass, 326 s |
| 2019 Server Core | `17763.737.2` | Pass, 503 s | Pass, 235 s | Pass, 235 s |
| 2022 Desktop Experience | `20348.169.8` | Pass, 938 s | Pass, 76 s | Pass, 99 s |
| 2022 Server Core | `20348.169.2` | Pass, 612 s | Pass, 152 s | Pass, 148 s |
| 2025 Desktop Experience | `26100.1742.9` | Pass, 1,350 s | Pass, 141 s | Pass, 97 s |
| 2025 Server Core | `26100.1742.1` | Pass, 444 s | Pass, 59 s | Pass, 51 s |

All six versions in the table were added to the shared x86_64 template
store. They occupy 46.2 GB. The card workspace and these versions occupy
65.0 GB after the run, below the 150 GB budget. All 12 clones reached
agent-ready on the requested hypervisor and reported the expected Windows
build. The test labs were destroyed after their checks.

Logs and screenshots are under `/home/vmlab/src/stc/card-7/evidence` on
helios. Each template directory contains `build.log`, `samples.log`,
and the boot logs for the runs that reach that stage.

The Server 2025 Core build logged a missing module from
`Get-AppxProvisionedPackage -Online` in `generalize.ps1`. Sysprep succeeded
on its first pass and the image sealed. See
`windows-server-2025-core/build.log`.

A preflight control clone of the existing Server 2019 Desktop
Experience version `17763.737.7` timed out after 1,200 s on vmlab-hv with
`ready=no` and a **Getting ready** screen. The same version on QEMU reached
agent-ready in 361 s and reported build `10.0.17763.9245`. Both control
clones used the existing guest agent with automatic agent updates disabled.
See `control-vmlab/boot.log`, `control-vmlab/failure.png`, and
`control-qemu/boot.log` under `/home/vmlab/src/stc/card-7`.

Card 76 reran the `17763.737.7` control on helios with vmlab and vmlab-hv
built from vmlab `next` at `1c289d35d93cb1acc0b5b37b01fad1524771d509`.
The fresh clone used the shared OVMF firmware (Debian `2026.08+ds-2`) and
kept its baked agent (`f1e4515`) with automatic agent updates disabled.
`vmlab up --wait` finished in 209 s, `vmlab status -v` reported
`hypervisor=vmlab` and `ready=yes`, and
`vmlab exec server -- cmd.exe /c ver` reported `10.0.17763.9245`.
The clone was destroyed after the checks. See
`/home/vmlab/src/stc/card-76-work/17763.737.7-vmlab/boot.log`.

The timeout did not recur in this run. The agent briefly disconnected
during Windows setup, then returned, the specialize marker was cleared,
and the first-boot provision completed its reboot. The cause of the
original stall remains unknown. An
[earlier q35 soak](agents/q35-soak-vml-12.md#windows-server-2019177637377)
also reached agent-ready with this version on vmlab-hv.
