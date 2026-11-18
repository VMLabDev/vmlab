# Modern q35 lifecycle soak on helios

Card #31 tested 22 pinned x86_64 templates on branch `vml-12`. Fifteen
templates passed every check on both hypervisors across the initial and
repeat runs. The product source was `a6a90036`. Later commits added the
soak runner and this report.

Each pair used separate linked clones of the same template, an SMB share,
and the agent baked into the image. Readiness time covers one observation
per hypervisor, from invoking `up` to observing `ready=yes`. Other work was
running on helios. Snapshot restores verified a reverted guest file, and
`down` verified that the lab daemon released its registration.

The three Server Core images were built from the template definitions at
`2a63cbd4` with the documented `VMLAB_SKIP_UPDATES=1` setting. SteamOS was
built from its recovery image. The Windows firmware controls used the
published v0.46.1-alpha guest bundle. Both hypervisors used matching
firmware within each pair.

Arch and Debian failed to mount SMB on both hypervisors with `Network is
unreachable`. The NixOS agent could not find `mount` on either hypervisor.
Published Server 2022 and 2025 missed agent-ready with both the host and
bundled firmware, which blocked their downstream checks. In the Server
2019 Core QEMU run, the restart completed and the following guest exec
probe timed out.

The Windows 11 QEMU online restore passed on repeat. Both offline restore
checks stopped at the marker read after a cold boot, before the runner
issued the restore command. See `rechecks/windows-11/` for the repeat.

## Findings

- **F1. Windows Server Core Setup advances much more slowly on vmlab-hv (card #70).**
  With the same install media and definitions, earlier vmlab-hv builds
  were at 45%, 2% and 7% for 2019, 2022 and 2025, respectively. The QEMU
  builds launched around 06:44 UTC had reached “Getting ready” on 2019 and
  2022 and 83% on 2025 by 06:53 UTC. The installer draft includes the start
  conditions and screenshot paths.
- **F2. Windows 10 and 11 readiness varied between runs.** The first
  vmlab-hv runs with bundled firmware exceeded 900 s. Fresh clones with
  the same firmware and shorter runtime paths reached agent-ready in
  478.4 s and 362.5 s, respectively, and passed every vmlab-hv check.
  The readiness timeout did not recur in the fresh clones, so no bug card
  was filed for it.
- **F3. A long XDG runtime path stops vmlab-hv startup (card #69).** Repeating
  Windows 10 and 11 with a longer `XDG_RUNTIME_DIR` caused vmlab-hv to fail
  binding `display.sock` with `path must be shorter than SUN_LEN`. The
  Windows 11 QEMU control reached its agent and captured a snapshot with
  the same path length.

The two bug drafts, command logs and screenshots are on helios in
`/home/vmlab/src/stc/soak-vml-12/`. See `windows-core-installer-defect.md`,
`windows-lifecycle-defects.md`, `install-comparison/` and
`boot-diagnostics/`. The log paths below are relative to that directory.

## almalinux-10@10.2.3

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 18.2 s | 15.1 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `aa1617c7`. Logs are `alma-confirm/almalinux-10@10.2.3-vmlab.log` and `alma-confirm/almalinux-10@10.2.3-qemu.log`.

## alpine-3.23@3.23.10

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 36.4 s | 34.4 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `29319aaa`. Logs are `matrix-1/alpine-3.23@3.23.10-vmlab.log` and `matrix-1/alpine-3.23@3.23.10-qemu.log`. The down command and the following status confirm daemon release. The report corrects the earlier runner assertion for vmlab, qemu.

## arch@20260901.2

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 19.5 s | 12.2 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Fail (exec guest exited 1) | Fail (exec guest exited 1) |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `29319aaa`. Logs are `arch/arch@20260901.2-vmlab.log` and `arch/arch@20260901.2-qemu.log`. The down command and the following status confirm daemon release. The report corrects the earlier runner assertion for vmlab, qemu.

## debian-13@13.20261001.0

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 10.8 s | 10.1 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Fail (exec guest exited 1) | Fail (exec guest exited 1) |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `29319aaa`. Logs are `matrix-1/debian-13@13.20261001.0-vmlab.log` and `matrix-1/debian-13@13.20261001.0-qemu.log`. The down command and the following status confirm daemon release. The report corrects the earlier runner assertion for vmlab, qemu.

## fedora-44@44.1.11

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 23.9 s | 10.1 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `1b33994c`. Logs are `fedora-confirm/fedora-44@44.1.11-vmlab.log` and `fedora-confirm/fedora-44@44.1.11-qemu.log`.

## kali@2026.1.3

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 24.6 s | 16.2 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `aa1617c7`. Logs are `late/kali@2026.1.3/kali@2026.1.3-vmlab.log` and `late/kali@2026.1.3/kali@2026.1.3-qemu.log`.

## nixos-25.11@25.11.3

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 12.2 s | 12.2 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Fail (exec guest exited 1) | Fail (exec guest exited 1) |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `aa1617c7`. Logs are `nixos-confirm/nixos-25.11@25.11.3-vmlab.log` and `nixos-confirm/nixos-25.11@25.11.3-qemu.log`.

## opensuse-leap@16.0.1

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 12.2 s | 10.1 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `1b33994c`. Logs are `remaining/opensuse-leap@16.0.1/opensuse-leap@16.0.1-vmlab.log` and `remaining/opensuse-leap@16.0.1/opensuse-leap@16.0.1-qemu.log`.

## opensuse-tumbleweed@20261003.1

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 12.1 s | 12.2 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `1b33994c`. Logs are `remaining/opensuse-tumbleweed@20261003.1/opensuse-tumbleweed@20261003.1-vmlab.log` and `remaining/opensuse-tumbleweed@20261003.1/opensuse-tumbleweed@20261003.1-qemu.log`.

## parrot@7.2.3

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 18.2 s | 14.2 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. Logs are `late/parrot@7.2.3/parrot@7.2.3-vmlab.log` and `late/parrot@7.2.3/parrot@7.2.3-qemu.log`.

## rocky-9@9.8.3

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 15.0 s | 10.1 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `aa1617c7`. Logs are `late/rocky-9@9.8.3/rocky-9@9.8.3-vmlab.log` and `late/rocky-9@9.8.3/rocky-9@9.8.3-qemu.log`.

## ubuntu-24.04@24.04.4.2

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 12.2 s | 17.1 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. Logs are `ubuntu2404-confirm/ubuntu-24.04@24.04.4.2-vmlab.log` and `ubuntu2404-confirm/ubuntu-24.04@24.04.4.2-qemu.log`.

## ubuntu-26.04@26.04.3

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 14.2 s | 10.1 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `aa1617c7`. Logs are `late/ubuntu-26.04@26.04.3/ubuntu-26.04@26.04.3-vmlab.log` and `late/ubuntu-26.04@26.04.3/ubuntu-26.04@26.04.3-qemu.log`.

## windows-10@19045.2006.9

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 478.4 s | 785.4 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. The vmlab-hv result uses the short-path
repeat in `redo/windows-10/`. The QEMU result is in
`fw-tests/windows-10@19045.2006.9/`, which also retains the first vmlab-hv
timeout. Both used the v0.46.1-alpha guest bundle firmware.

## windows-11@26100.1742.6

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 362.5 s | 299.2 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Fail (marker read timed out before restore) |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. The vmlab-hv result uses the short-path
repeat in `redo/windows-11/`. The QEMU result uses the repeat in
`rechecks/windows-11/`. Both used the v0.46.1-alpha guest bundle firmware.
The first attempt is in `fw-tests/windows-11@26100.1742.6/`, where QEMU
reached agent-ready in 659.7 s.

## windows-server-2019@17763.737.7

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 195.8 s | 136.8 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. Logs are `late/windows-server-2019@17763.737.7/windows-server-2019@17763.737.7-vmlab.log` and `late/windows-server-2019@17763.737.7/windows-server-2019@17763.737.7-qemu.log`.

## windows-server-2022@20348.169.7

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | Unavailable | Unavailable |
| up | Fail (up exited 1) | Fail (up exited 1) |
| agent-ready | Fail (up exited 1) | Fail (up exited 1) |
| exec | Blocked by an earlier step | Blocked by an earlier step |
| cp to guest | Blocked by an earlier step | Blocked by an earlier step |
| cp from guest | Blocked by an earlier step | Blocked by an earlier step |
| shared folder | Blocked by an earlier step | Blocked by an earlier step |
| screenshot | Blocked by an earlier step | Blocked by an earlier step |
| online snapshot | Blocked by an earlier step | Blocked by an earlier step |
| online restore | Blocked by an earlier step | Blocked by an earlier step |
| restart | Blocked by an earlier step | Blocked by an earlier step |
| stop | Blocked by an earlier step | Blocked by an earlier step |
| offline snapshot | Blocked by an earlier step | Blocked by an earlier step |
| offline restore | Blocked by an earlier step | Blocked by an earlier step |
| down | Blocked by an earlier step | Blocked by an earlier step |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. Logs are `fw-tests/windows-server-2022@20348.169.7/windows-server-2022@20348.169.7-vmlab.log` and `fw-tests/windows-server-2022@20348.169.7/windows-server-2022@20348.169.7-qemu.log`. This pair used the published v0.46.1-alpha guest bundle firmware. The initial distribution-firmware report is retained in late/ or last/.

## windows-server-2025@26100.1742.8

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | Unavailable | Unavailable |
| up | Fail (up exited 1) | Fail (up exited 1) |
| agent-ready | Fail (up exited 1) | Fail (up exited 1) |
| exec | Blocked by an earlier step | Blocked by an earlier step |
| cp to guest | Blocked by an earlier step | Blocked by an earlier step |
| cp from guest | Blocked by an earlier step | Blocked by an earlier step |
| shared folder | Blocked by an earlier step | Blocked by an earlier step |
| screenshot | Blocked by an earlier step | Blocked by an earlier step |
| online snapshot | Blocked by an earlier step | Blocked by an earlier step |
| online restore | Blocked by an earlier step | Blocked by an earlier step |
| restart | Blocked by an earlier step | Blocked by an earlier step |
| stop | Blocked by an earlier step | Blocked by an earlier step |
| offline snapshot | Blocked by an earlier step | Blocked by an earlier step |
| offline restore | Blocked by an earlier step | Blocked by an earlier step |
| down | Blocked by an earlier step | Blocked by an earlier step |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. Logs are `fw-tests/windows-server-2025@26100.1742.8/windows-server-2025@26100.1742.8-vmlab.log` and `fw-tests/windows-server-2025@26100.1742.8/windows-server-2025@26100.1742.8-qemu.log`. This pair used the published v0.46.1-alpha guest bundle firmware. The initial distribution-firmware report is retained in late/ or last/.

## windows-server-2019-core@17763.737.0

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 559.3 s | 369.0 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Fail (post-restart exec timed out after 40 s) |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. Logs are `fast-core/windows-server-2019-core/windows-server-2019-core@17763.737.0-vmlab.log` and `fast-core/windows-server-2019-core/windows-server-2019-core@17763.737.0-qemu.log`. The Core image was built locally on QEMU using the documented VMLAB_SKIP_UPDATES=1 option.

## windows-server-2022-core@20348.169.0

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 247.9 s | 372.9 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. Logs are `fast-core/windows-server-2022-core/windows-server-2022-core@20348.169.0-vmlab.log` and `fast-core/windows-server-2022-core/windows-server-2022-core@20348.169.0-qemu.log`. The Core image was built locally on QEMU using the documented VMLAB_SKIP_UPDATES=1 option.

## windows-server-2025-core@26100.1742.0

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 284.1 s | 216.3 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. Logs are `fast-core/windows-server-2025-core/windows-server-2025-core@26100.1742.0-vmlab.log` and `fast-core/windows-server-2025-core/windows-server-2025-core@26100.1742.0-qemu.log`. The Core image was built locally on QEMU using the documented VMLAB_SKIP_UPDATES=1 option.

## steamos@3.8.14.0

| Check | vmlab-hv | QEMU |
| --- | --- | --- |
| Time to agent-ready | 10.7 s | 14.2 s |
| up | Pass | Pass |
| agent-ready | Pass | Pass |
| exec | Pass | Pass |
| cp to guest | Pass | Pass |
| cp from guest | Pass | Pass |
| shared folder | Pass | Pass |
| screenshot | Pass | Pass |
| online snapshot | Pass | Pass |
| online restore | Pass | Pass |
| restart | Pass | Pass |
| stop | Pass | Pass |
| offline snapshot | Pass | Pass |
| offline restore | Pass | Pass |
| down | Pass | Pass |
| cleanup | Pass | Pass |

Runner checkout `80330b24`. Logs are `steamos-confirm/steamos@3.8.14.0-vmlab.log` and `steamos-confirm/steamos@3.8.14.0-qemu.log`.
