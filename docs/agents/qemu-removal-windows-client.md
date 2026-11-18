# QEMU removal, Part A3 — Windows client templates

VML-83 requires fresh base and `#standard` builds on vmlab-hv with
`VMLAB_SKIP_UPDATES=1`, followed by agent-ready boots of each new
`#standard` image on vmlab-hv and QEMU. The base is unsealed (ADR-0017).

## Helios preflight, 2026-11-18

The vmlab checkout at `aaed53eccda3a381c7acf1dcb97363e6855900c4`
contains both VML-99 fixes (merge commits `a6a90036` and
`da6c21a9`). The template checkout is at
`a4cf3fb558bf9379ae8d78bb104982b45c617dba` from vmlab-templates
`next`.

| Template | Planned version | vmlab-hv build | vmlab-hv boot | QEMU boot |
| --- | --- | --- | --- | --- |
| Windows 10 `#standard` | `19045.2006.10` | Blocked at preflight | Pending | Pending |
| Windows 11 `#standard` | `26100.1742.11` | Blocked at preflight | Pending | Pending |

On helios, as the `vmlab` account,
`du -sLB1 "$HOME/.local/share/vmlab/templates"` returned
`437709926400` bytes. The store already exceeds the 150 GB total
specified in the card handoff. The Windows 10 and 11 directories held
`19045.2006.9` and `26100.1742.10`, respectively. Card 8 added no
versions. The concurrent build check,
`pgrep -u vmlab -af '^vmlab template build( |$)'`, also found seven
Windows Server build commands at preflight.

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

The fresh-build matrix is pending an authorized storage limit or
destination and a free build slot.
