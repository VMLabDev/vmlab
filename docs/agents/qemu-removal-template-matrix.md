# Modern x86_64 template verification on helios

Card #16 (VML-39) requires a vmlab-hv build followed by boots of the
same sealed version on vmlab-hv and QEMU. The matrix collects the child
card evidence and the pinned-image controls from the q35 soak.
Both boot columns record agent readiness. Where a row gives a
published control, the boot results apply only to that version.

| Template | Version | Build on vmlab-hv | vmlab-hv boot | QEMU boot | Evidence |
| --- | --- | --- | --- | --- | --- |
| almalinux-10 | `10.2.3` | Unverified | Pass | Pass | Published control |
| alpine-3.23 | `3.23.10` | Unverified | Pass | Pass | Published control |
| arch | `20260901.2` | Unverified | Pass | Pass | Published control |
| debian-13 | `13.20261001.0` | Unverified | Pass | Pass | Published control |
| fedora-44 | `44.1.11` | Unverified | Pass | Pass | Published control |
| kali | `2026.1.3` control | Earlier build blocked by media HTTP 404 | Pass | Pass | Card #21 and published control |
| nixos-25.11 | `25.11.4` | Pass | Pass, 33 s | Pass, 29 s | Card #23. Both guest probes used `/bin/sh`. |
| parrot | `7.2.3` control | Earlier build blocked by media HTTP 404 | Pass | Pass | Card #22 and published control |
| rocky-9 | `9.8.3` | Unverified | Pass | Pass | Published control |
| steamos | `3.8.14.0` | Pass | Pass | Pass | Soak build log and paired boots. The recovery-media preparation VM used QEMU before the template build. |
| ubuntu-24.04 | `24.04.4.2` | Unverified | Pass | Pass | Published control |
| ubuntu-26.04 | `26.04.3` control | Earlier build blocked by source checksum | Pass | Pass | Card #20 and published control |
| windows-10 base | `19045.2006.10` | Pass | Unsealed base | Unsealed base | Card #8 |
| windows-10 #standard | `19045.2006.10` | Pass | Pass, 156 s | Pass, 139 s | Card #8 |
| windows-10 #win3d | — | Deferred | Pending | Pending | GPU work deferred to operator |
| windows-11 base | `26100.1742.11` | Pass | Unsealed base | Unsealed base | Card #8 |
| windows-11 #standard | `26100.1742.11` | Pass | Pass, 132 s | Pass, 241 s | Card #8 |
| windows-11 #win3d | — | Deferred | Pending | Pending | GPU work deferred to operator |
| windows-server-2019 | `17763.737.8` | Pass | Pass, 345 s | Pass, 326 s | Card #7 |
| windows-server-2019-core | `17763.737.2` | Pass | Pass, 235 s | Pass, 235 s | Card #7 |
| windows-server-2022 | `20348.169.8` | Pass | Pass, 76 s | Pass, 99 s | Card #7 |
| windows-server-2022-core | `20348.169.2` | Pass | Pass, 152 s | Pass, 148 s | Card #7 |
| windows-server-2025 | `26100.1742.9` | Pass | Pass, 141 s | Pass, 97 s | Card #7 |
| windows-server-2025-core | `26100.1742.1` | Pass | Pass, 59 s | Pass, 51 s | Card #7 |

The [Windows client report](qemu-removal-windows-client.md) and
[Windows Server report](../qemu-x86-removal-windows-server.md) give the
source commits, build timings, host setup and log paths. The earlier
Windows client Setup failure is tracked by cards #12 and #67. The
Server 2019 report records a vmlab-hv timeout of the older
`17763.737.7` control in `card-7/evidence/control-vmlab/boot.log` on
helios. That control failure has no linked bug card.

The [q35 soak report](q35-soak-vml-12.md) gives the boot evidence for
published controls and SteamOS. Its SteamOS build log is
`/home/vmlab/src/stc/soak-vml-12/steamos-build.log` on helios.
The VML-81 and VML-82 build evidence directory is no longer available.
The template fixes for Ubuntu 26.04, Kali and Parrot have merged.
Fresh builds and paired boots of those fixed definitions remain open.
The NixOS PATH fix was rebuilt as `25.11.5` and verified through
bare `sh` on vmlab-hv. The paired boot evidence above is for `25.11.4`.
