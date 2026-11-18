# QEMU removal, Part A3 — Windows client templates

VML-83 requires fresh base and `#standard` builds on vmlab-hv with
`VMLAB_SKIP_UPDATES=1`, followed by agent-ready boots of each new
`#standard` image on vmlab-hv and QEMU. The base is unsealed (ADR-0017).

## Helios results, 2026-11-18

The vmlab checkout at `aaed53eccda3a381c7acf1dcb97363e6855900c4`
contains both VML-99 fixes (merge commits `a6a90036` and
`da6c21a9`). The template checkout is at
`a4cf3fb558bf9379ae8d78bb104982b45c617dba` from vmlab-templates
`next`.

| Template | Version | vmlab-hv build | vmlab-hv boot | QEMU 11.1.3 boot | Store addition |
| --- | --- | --- | --- | --- | ---: |
| Windows 10 `#standard` | `19045.2006.10` | Pass, 1200 s | Pass, 156 s | Pass, 139 s | 11,723,640,832 bytes |
| Windows 11 `#standard` | `26100.1742.11` | Pass, 1441 s | Pass, 132 s | Pass, 241 s | 10,662,744,064 bytes |

The handoff limits this card's additions to the shared template store to
150 GB and requires at least 300 GB of free space during the builds.
Before the client builds, the Windows 10 and 11 directories held
`19045.2006.9` and `26100.1742.10`, respectively. Windows Server
builds were already running on helios, so the client matrix waited
for the build slot before each start. Other Windows Server builds
started during the client builds. Helios reported 695 GiB free
before Windows 11 and 660 GiB before Windows 10. The two fresh versions
added 22,386,384,896 bytes in total.

Each build used a copy of the template definition with the `win3d`
variant block removed. The builds used the pinned, SHA-256-verified
evaluation ISOs and recorded that they skipped Windows Update. Build
process samples identified vmlab-hv 29 times for Windows 11 and 24
times for Windows 10. The boot tests used fresh overlays, confirmed
`ready=yes`, `variant=standard` and the requested hypervisor in
`vmlab status -v`, then ran `cmd.exe /c ver` through the guest agent.
The Windows 10 boots reported `10.0.19045.2006`, and the Windows 11
boots reported `10.0.26100.1742`.

The logs, hypervisor process samples, setup screenshots, and store
inventory are in `/home/vmlab/src/stc/card-8/evidence/matrix` on helios.

## Prior helios results, 2026-11-07

These results come from the VML-83 worklog and its JUnit run
`01M6TSK566RR0JKHFMF495BZA7` (5 passed, 2 failed, 4 skipped).
The run used vmlab and vmlab-hv at `f4eda5a7`, QEMU 11.1.3, and
vmlab-templates at `c30415d`.

| Template | vmlab-hv build | Existing version used for control | vmlab-hv control | QEMU control |
| --- | --- | --- | --- | --- |
| Windows 10 `#standard` | Agent-ready timeout after 5400 s | `19045.2006.9` | 156 s, Windows 10.0.19045.6456 | 174 s, Windows 10.0.19045.6456 |
| Windows 11 `#standard` | Agent-ready timeout after 5400 s | `26100.1742.10` | 324 s, Windows 10.0.26100.9711 | 228 s, Windows 10.0.26100.9711 |

The Windows 11 diagnostic rerun showed the Windows Setup media-driver
prompt, and `diskpart` in WinPE stalled while enumerating disks.
VML-99 holds the diagnostic bundle (ART-4). The VML-83 evidence archive
(ART-5) contains both build logs, process samples, screenshots, and
the control boot results. The controls reached agent-ready and ran
`cmd.exe /c ver` in fresh overlays of the existing templates.
