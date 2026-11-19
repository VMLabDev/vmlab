# Modern x86_64 template verification on helios

Card #16 (VML-39) requires a vmlab-hv build followed by boots of the
same sealed version on vmlab-hv and QEMU. The matrix collects the child
card evidence and the pinned-image controls from the q35 soak.
Both boot columns record agent readiness. Where a row gives a
published control, the boot results apply only to that version.

| Template | Version | Build on vmlab-hv | vmlab-hv boot | QEMU boot | Evidence |
| --- | --- | --- | --- | --- | --- |
| almalinux-10 | `10.2.4` | Pass, 88 s | Pass, 11 s | Pass, 7 s | Fresh run, 1,053,892,608 bytes. See follow-up draft for the pinned URL. |
| alpine-3.23 | `3.23.11` | Pass, 61 s | Pass, 22 s | Pass, 20 s | Fresh run, 304,553,984 bytes |
| arch | `20260901.3` | Pass, 106 s | Pass, 11 s | Pass, 10 s | Fresh run, 882,450,432 bytes |
| debian-13 | `13.20261001.1` | Pass, 112 s | Pass, 7 s | Pass, 7 s | Fresh run, 1,364,533,248 bytes |
| fedora-44 | `44.1.12` | Pass, 75 s | Pass, 10 s | Pass, 10 s | Fresh run, 805,969,920 bytes |
| kali | `2026.1.4` | Pass, 440 s | Pass, 17 s | Pass, 12 s | Fresh run, 16,251,953,152 bytes. Media fix from card #21. |
| nixos-25.11 | `25.11.4` | Pass | Pass, 33 s | Pass, 29 s | Card #23. Both guest probes used `/bin/sh`. |
| parrot | `7.4.0` | Pass, 435 s | Pass, 12 s | Pass, 11 s | Fresh run, 13,595,320,320 bytes. Media fix from card #22. |
| rocky-9 | `9.8.4` | Pass, 94 s | Pass, 8 s | Pass, 7 s | Fresh run, 1,490,165,760 bytes. See follow-up draft for the pinned URL. |
| steamos | `3.8.14.0` | Pass | Pass | Pass | Soak build log and paired boots. The recovery-media preparation VM used QEMU before the template build. |
| ubuntu-24.04 | `24.04.4.3` | Pass, 601 s | Pass, 8 s | Pass, 5 s | Fresh run, 4,791,934,976 bytes |
| ubuntu-26.04 | `26.04.4` | Pass, 106 s | Pass, 12 s | Pass, 7 s | Fresh run, 2,316,771,328 bytes. Checksum fix from card #20. |
| windows-10 base | `19045.2006.10` | Pass | Unsealed base | Unsealed base | Card #8 |
| windows-10 #standard | `19045.2006.10` | Pass | Pass, 156 s | Pass, 139 s | Card #8 |
| windows-10 #win3d | — | Deferred by the operator (no GPU builds) | — | — | — |
| windows-11 base | `26100.1742.11` | Pass | Unsealed base | Unsealed base | Card #8 |
| windows-11 #standard | `26100.1742.11` | Pass | Pass, 132 s | Pass, 241 s | Card #8 |
| windows-11 #win3d | — | Deferred by the operator (no GPU builds) | — | — | — |
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
helios. Card #76 tracks that control failure.

The [q35 soak report](q35-soak-vml-12.md) gives the boot evidence for
published controls and SteamOS. Its SteamOS build log is
`/home/vmlab/src/stc/soak-vml-12/steamos-build.log` on helios.
The VML-81 and VML-82 build evidence directory is no longer available.
The earlier Ubuntu 26.04 checksum failure and the Kali and Parrot
media failures are tracked by cards #20, #21 and #22.
The NixOS PATH fix was rebuilt as `25.11.5` and verified through
bare `sh` on vmlab-hv. The paired boot evidence above is for `25.11.4`.

## Fresh Linux runs on 2026-11-18

The fresh runs use vmlab and vmlab-hv built from vmlab `next` at
`815876b16384c1cdacf3d91a7ccd8aa19b07031c`, the definitions from
vmlab-templates `next` at `a4cf3fb558bf9379ae8d78bb104982b45c617dba`,
and QEMU 11.1.3. The Linux bootstrap agent was reused from the q35
soak (stamp `1b33994c`). The helios Rust installation lacked the
`x86_64-unknown-linux-musl` target needed to rebuild that agent.

The builds run serially in a 24 GiB systemd service with private XDG
directories and a separate template store. Each definition runs
`vmlab template build <template>` in its template directory. Kali and
Parrot first run the definition's `./fetch-deps.sh`. The host
configuration pins vmlab-hv and the run logs its VM process.
Each boot uses `vmlab up --wait` in a fresh lab pinned to the exact
sealed version, with automatic agent updates disabled. A passing boot
requires `ready=yes`, the requested hypervisor in `vmlab status -v`,
and an agent command that prints the guest OS and architecture. Build
timings cover the build command, excluding the separate media prefetch
and dependency preparation.

All ten fresh builds sealed successfully, and all twenty boot tests
passed. The versions occupy 42,857,545,728 bytes in the private
`data/vmlab/templates` store under the run directory, below the 150 GB
limit. The smallest recorded free space on either `/home` or
`/var/build` was 692,459,364,352 bytes, above the 300 GB floor. The
run destroyed the test labs and stopped the private supervisor.

Run files are under `/home/vmlab/src/stc/card-16` on helios.
Within `evidence/<template>/`, `build.log` records the build,
`samples.log` records the hypervisor process, and `vmlab.log` and
`qemu.log` record the two boot attempts.
Fedora's input came from the AARNet mirror and matched the SHA-256 in
the pinned definition. The download and hash check are in
`prefetch/fedora-44.mirror.log`.

## Follow-up cards

### Refresh the pinned AlmaLinux 10 cloud image URL

At vmlab-templates `a4cf3fb5`, enter `almalinux-10` with an empty
artefact cache and run `vmlab template build almalinux-10` with
`host.hypervisor.vm` set to `vmlab`. The pinned
`AlmaLinux-10-GenericCloud-10.2-20260526.0.x86_64.qcow2` URL returns
HTTP 404 and build `10.2.4` exits 1 before VM launch. See
`evidence/almalinux-10/first-build.log` on helios. The official 10.2
vault has an image with the pinned SHA-256, as recorded in
`prefetch/almalinux-10.mirror.log`.

### Refresh the pinned Rocky Linux 9 cloud image URL

At vmlab-templates `a4cf3fb5`, run `curl -IL` on the
`Rocky-9-GenericCloud-Base-9.8-20260525.0.x86_64.qcow2` URL in
`rocky-9/vmlab.wcl`. The unversioned `/pub/rocky/9/images/x86_64/`
path returns HTTP 404. See `prefetch/rocky-9-pinned-url.log` on
helios. The official 9.8 vault has an image with the pinned SHA-256,
as recorded in `prefetch/rocky-9.mirror.log`.
